//! Counting and tracing of the inference requests dwctl's own layers refuse.
//!
//! onwards counts every client error it decides on in
//! `onwards_rejections_total{model, status, code, traffic}` and sets the
//! error code, parameter, account and API key id on the current span (see
//! `onwards::rejections`). The layers in front of onwards (body limit,
//! inference routing, protocol translation, image normalisation) refuse
//! requests too and record them the same way, so one series covers every
//! client error the gateway itself returns.
//!
//! [`attach_recorder`] wraps the whole inference stack and puts a [`Recorder`]
//! on each request; a layer that refuses the request calls [`record`] with its
//! extensions. A 4xx a layer only passes on or reshapes (onwards' own, or an
//! upstream's) is not recorded again.

use axum::{
    body::Body,
    extract::State,
    http::{Extensions, HeaderMap, Request, StatusCode},
    middleware::Next,
    response::Response,
};
use onwards::{rejections::RejectionContext, target::Targets};

/// Header the fusillade daemon stamps on every request it dispatches, and
/// only on those. onwards uses it to tell dispatched from realtime traffic.
pub const DISPATCHED_HEADER: &str = "x-fusillade-batch-created-at";

/// The onwards targets a rejection resolves its account and model against.
#[derive(Clone, Default)]
pub struct Recorder {
    targets: Targets,
}

impl Recorder {
    pub fn new(targets: Targets) -> Self {
        Self { targets }
    }
}

/// Makes `recorder` available to every layer inside this one.
pub async fn attach_recorder(State(recorder): State<Recorder>, mut request: Request<Body>, next: Next) -> Response {
    request.extensions_mut().insert(recorder);
    next.run(request).await
}

/// Records a refusal of the request with these `extensions` and `headers`.
/// `model` is the request's model, if the layer has read it; it is only used
/// as a label when it names a configured alias.
pub(crate) fn record(
    extensions: &Extensions,
    headers: &HeaderMap,
    status: StatusCode,
    code: &str,
    param: Option<&str>,
    model: Option<&str>,
) {
    if let Some(scope) = Scope::of(extensions, headers) {
        scope.record(status, code, param, model);
    }
}

/// A request's caller, kept for a refusal decided after the request itself has
/// been handed on.
pub(crate) struct Scope {
    targets: Targets,
    context: RejectionContext,
}

impl Scope {
    /// `None` when no [`Recorder`] is attached.
    pub(crate) fn of(extensions: &Extensions, headers: &HeaderMap) -> Option<Self> {
        let recorder = extensions.get::<Recorder>()?;
        Some(Self {
            context: RejectionContext::new(&recorder.targets, headers, headers.contains_key(DISPATCHED_HEADER)),
            targets: recorder.targets.clone(),
        })
    }

    pub(crate) fn record(mut self, status: StatusCode, code: &str, param: Option<&str>, model: Option<&str>) {
        if let Some(model) = model {
            self.context.set_model_if_configured(&self.targets, model);
        }
        self.context.record(status, code, param);
    }
}

/// The `model` of a JSON request body, if it has one.
pub(crate) fn model_of(body: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Model {
        model: Option<String>,
    }
    serde_json::from_slice::<Model>(body).ok()?.model
}

/// Code for a request body that couldn't be translated: `invalid_json` when it
/// isn't JSON at all, `schema_mismatch` otherwise. onwards' strict mode counts
/// its own body errors under the same codes.
pub(crate) fn body_error_code(body: &[u8]) -> &'static str {
    if serde_json::from_slice::<serde::de::IgnoredAny>(body).is_ok() {
        "schema_mismatch"
    } else {
        "invalid_json"
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::future::Future;

    use metrics_exporter_prometheus::PrometheusBuilder;
    use onwards::target::Targets;

    use super::Recorder;

    /// A recorder whose only configured alias is `alias`, and whose key
    /// `sk-test` belongs to account `acct-1`.
    pub fn recorder(alias: &str) -> Recorder {
        let targets = Targets::from_config(
            serde_json::from_value(serde_json::json!({
                "targets": {alias: {"url": "https://upstream.example.com/v1/"}},
                "auth": {"global_keys": [], "key_definitions": {
                    "k": {"key": "sk-test", "labels": {"account": "acct-1"}}
                }}
            }))
            .unwrap(),
        )
        .unwrap();
        Recorder::new(targets)
    }

    /// Runs `future` on a fresh current-thread runtime and returns the
    /// `onwards_rejections_total` lines it recorded, with its output.
    pub fn rejections<F: Future>(future: F) -> (Vec<String>, F::Output) {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let output = metrics::with_local_recorder(&recorder, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(future)
        });
        let lines = handle
            .render()
            .lines()
            .filter(|line| line.starts_with("onwards_rejections_total{"))
            .map(str::to_string)
            .collect();
        (lines, output)
    }
}

#[cfg(test)]
mod tests {
    use super::{test_support::*, *};
    use axum::{Router, middleware, routing::post};
    use tower::ServiceExt;

    async fn refuse(request: Request<Body>) -> StatusCode {
        let (parts, body) = request.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        record(
            &parts.extensions,
            &parts.headers,
            StatusCode::BAD_REQUEST,
            "some_code",
            Some("some_param"),
            model_of(&bytes).as_deref(),
        );
        StatusCode::BAD_REQUEST
    }

    fn app() -> Router {
        Router::new()
            .route("/chat/completions", post(refuse))
            .layer(middleware::from_fn_with_state(recorder("known-model"), attach_recorder))
    }

    #[test]
    fn counts_configured_models_and_dispatched_traffic() {
        let (lines, _) = rejections(async {
            for (model, dispatched) in [("known-model:interactive", false), ("not-configured", true)] {
                let mut request = Request::post("/chat/completions").header("authorization", "Bearer sk-test");
                if dispatched {
                    request = request.header(DISPATCHED_HEADER, "2026-01-01T00:00:00Z");
                }
                let body = serde_json::json!({"model": model}).to_string();
                app().oneshot(request.body(Body::from(body)).unwrap()).await.unwrap();
            }
        });
        let realtime = lines.iter().find(|line| line.contains("traffic=\"realtime\"")).unwrap();
        assert!(realtime.contains("model=\"known-model\""), "{realtime}");
        assert!(
            realtime.contains("code=\"some_code\"") && realtime.contains("status=\"400\""),
            "{realtime}"
        );
        let dispatched = lines.iter().find(|line| line.contains("traffic=\"dispatched\"")).unwrap();
        assert!(
            dispatched.contains("model=\"\""),
            "unconfigured models stay unlabelled: {dispatched}"
        );
    }

    #[test]
    fn records_nothing_without_a_recorder() {
        let (lines, _) = rejections(async {
            let app = Router::new().route("/chat/completions", post(refuse));
            app.oneshot(Request::post("/chat/completions").body(Body::empty()).unwrap())
                .await
                .unwrap();
        });
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn classifies_untranslatable_bodies() {
        assert_eq!(body_error_code(b"{not json"), "invalid_json");
        assert_eq!(body_error_code(br#"{"input": 1}"#), "schema_mismatch");
        assert_eq!(model_of(br#"{"model": "m", "x": 1}"#).as_deref(), Some("m"));
        assert_eq!(model_of(b"{not json"), None);
    }
}
