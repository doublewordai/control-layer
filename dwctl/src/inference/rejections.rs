//! Counting and tracing of the inference requests dwctl's own layers refuse.
//!
//! onwards counts every client error it decides on in
//! `onwards_rejections_total{model, status, code, traffic}` and sets the
//! error code, parameter, account and API key id on the current span (see
//! `onwards::rejections`). The layers in front of onwards refuse requests too
//! and record them the same way: the body limit, inference routing, protocol
//! translation, the prompt cache, continuation, request preparation, image
//! normalisation and the `/models` list. Error enrichment only rewrites
//! onwards' own refusals, which onwards has already counted.
//!
//! A [`Recorder`] is installed as a request extension around the whole
//! inference stack. A layer that refuses a request passes its response through
//! [`refuse`], which records the refusal and tags the response with its
//! [`RejectionCode`] and, when the key is known, the caller's
//! [`onwards::RequestIdentity`], so the request span reports that code and the
//! account rather than the bare status. A 4xx a layer only passes on or reshapes (onwards' own, or an
//! upstream's) is not recorded again.

use axum::{
    http::{HeaderMap, Request, header::AUTHORIZATION, request::Parts},
    response::Response,
};
use onwards::{rejections::RejectionContext, strict::schema_mismatch::SchemaMismatch, target::Targets};

use crate::inference::translation::anthropic;

/// Header the fusillade daemon stamps on every request it dispatches, and
/// only on those. onwards uses it to tell dispatched from realtime traffic.
pub const DISPATCHED_HEADER: &str = "x-fusillade-batch-created-at";

/// The onwards targets a rejection resolves its account and model against.
/// Installed on inference requests as an `axum::Extension`.
#[derive(Clone, Default)]
pub struct Recorder {
    targets: Targets,
}

impl Recorder {
    pub fn new(targets: Targets) -> Self {
        Self { targets }
    }

    /// See [`Targets::configured_alias`].
    pub fn configured_alias<'m>(&self, model: &'m str) -> Option<&'m str> {
        self.targets.configured_alias(model)
    }
}

/// The code a dwctl layer refused a response's request with, and the span it
/// was recorded on.
#[derive(Clone, Debug)]
pub struct RejectionCode {
    code: &'static str,
    recorded_on: Option<tracing::span::Id>,
}

impl RejectionCode {
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// Whether the refusal's attributes were already set on `span`.
    pub fn recorded_on(&self, span: &tracing::Span) -> bool {
        self.recorded_on.is_some() && self.recorded_on == span.id()
    }
}

/// The request a refusal is attributed to.
#[derive(Clone, Copy)]
pub(crate) struct Origin<'a> {
    recorder: Option<&'a Recorder>,
    headers: &'a HeaderMap,
    path: &'a str,
}

impl<'a> Origin<'a> {
    pub(crate) fn new(recorder: Option<&'a Recorder>, headers: &'a HeaderMap, path: &'a str) -> Self {
        Self { recorder, headers, path }
    }
}

impl<'a> From<&'a Parts> for Origin<'a> {
    fn from(parts: &'a Parts) -> Self {
        Self::new(parts.extensions.get(), &parts.headers, parts.uri.path())
    }
}

impl<'a, B> From<&'a Request<B>> for Origin<'a> {
    fn from(request: &'a Request<B>) -> Self {
        Self::new(request.extensions().get(), request.headers(), request.uri().path())
    }
}

/// Records `response` as refused for `code` and returns it, tagged. `model` is
/// the request's model, if the layer has read it; it is only used as a label
/// when it names a configured alias.
pub(crate) fn refuse<'a>(
    origin: impl Into<Origin<'a>>,
    response: Response,
    code: &'static str,
    param: Option<&str>,
    model: Option<&str>,
) -> Response {
    Scope::of(origin).refuse(response, code, param, model)
}

/// Records `response` as refused for a body that doesn't match its schema,
/// with where and how it stopped matching when that is known.
pub(crate) fn refuse_mismatch<'a>(
    origin: impl Into<Origin<'a>>,
    response: Response,
    mismatch: Option<&SchemaMismatch>,
    model: Option<&str>,
) -> Response {
    let param = mismatch.and_then(|mismatch| mismatch.path.as_deref());
    let reason = mismatch.map(|mismatch| mismatch.reason);
    Scope::of(origin).refuse_with_reason(response, "schema_mismatch", param, reason, model)
}

/// A request's caller, kept for a refusal decided after the request itself has
/// been handed on.
pub(crate) struct Scope {
    caller: Option<(Targets, RejectionContext)>,
}

impl Scope {
    pub(crate) fn of<'a>(origin: impl Into<Origin<'a>>) -> Self {
        let origin = origin.into();
        let caller = origin.recorder.map(|recorder| {
            let context = RejectionContext::for_api_key(
                &recorder.targets,
                api_key(origin.path, origin.headers),
                origin.headers.contains_key(DISPATCHED_HEADER),
            );
            (recorder.targets.clone(), context)
        });
        Self { caller }
    }

    pub(crate) fn refuse(self, response: Response, code: &'static str, param: Option<&str>, model: Option<&str>) -> Response {
        self.refuse_with_reason(response, code, param, None, model)
    }

    fn refuse_with_reason(
        self,
        mut response: Response,
        code: &'static str,
        param: Option<&str>,
        reason: Option<&'static str>,
        model: Option<&str>,
    ) -> Response {
        let mut recorded_on = None;
        if let Some((targets, mut context)) = self.caller {
            if let Some(model) = model {
                context.set_model_if_configured(&targets, model);
            }
            context.record_with_reason(response.status(), code, param, reason);
            recorded_on = tracing::Span::current().id();
            // Names the caller on the root request span, as onwards' own
            // refusals do.
            if let Some(identity) = context.identity() {
                response.extensions_mut().insert(identity);
            }
        }
        response.extensions_mut().insert(RejectionCode { code, recorded_on });
        response
    }
}

/// The API key the request presents: a bearer `Authorization`, or, on the
/// routes the Anthropic translators claim and only when `Authorization` is
/// absent, `x-api-key`.
fn api_key<'a>(path: &str, headers: &'a HeaderMap) -> Option<&'a str> {
    match headers.get(AUTHORIZATION) {
        Some(_) => bearer_token(headers),
        None if anthropic::accepts_x_api_key(path, headers) => headers.get("x-api-key")?.to_str().ok(),
        None => None,
    }
}

/// The token of a bearer `Authorization` header. The scheme is matched
/// case-insensitively, as `/models` authenticates it.
pub(crate) fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?.trim();
    let (scheme, token) = value.split_once(char::is_whitespace)?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(token.trim())
        .filter(|token| !token.is_empty())
}

/// The `model` of a JSON request body, if it has one.
pub(crate) fn model_of(body: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Model {
        model: Option<String>,
    }
    serde_json::from_slice::<Model>(body).ok()?.model
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::future::Future;

    use axum::Extension;
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

    /// The layer installing [`recorder`] on a test router.
    pub fn layer(alias: &str) -> Extension<Recorder> {
        Extension(recorder(alias))
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
    use axum::{
        Router,
        body::Body,
        http::{HeaderValue, StatusCode},
        response::IntoResponse,
        routing::post,
    };
    use tower::ServiceExt;

    async fn refused(request: Request<Body>) -> Response {
        let (parts, body) = request.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let response = StatusCode::BAD_REQUEST.into_response();
        refuse(&parts, response, "some_code", Some("some_param"), model_of(&bytes).as_deref())
    }

    fn app() -> Router {
        Router::new().route("/chat/completions", post(refused)).layer(layer("known-model"))
    }

    #[test]
    fn counts_configured_models_and_dispatched_traffic() {
        let (lines, (code, identities)) = rejections(async {
            let mut code = None;
            let mut identities = Vec::new();
            for (model, dispatched) in [("known-model:interactive", false), ("not-configured", true)] {
                let mut request = Request::post("/chat/completions").header("authorization", "Bearer sk-test");
                if dispatched {
                    request = request.header(DISPATCHED_HEADER, "2026-01-01T00:00:00Z");
                }
                let body = serde_json::json!({"model": model}).to_string();
                let response = app().oneshot(request.body(Body::from(body)).unwrap()).await.unwrap();
                code = response.extensions().get::<RejectionCode>().map(RejectionCode::code);
                identities.push(response.extensions().get::<onwards::RequestIdentity>().cloned());
            }
            (code, identities)
        });
        assert_eq!(code, Some("some_code"), "the response carries its code");
        let [configured, unconfigured] = identities.try_into().unwrap();
        let configured = configured.expect("the response names the caller");
        assert_eq!(configured.account_id.as_deref(), Some("acct-1"));
        assert_eq!(configured.model.as_deref(), Some("known-model"));
        assert_eq!(
            unconfigured.expect("the response names the caller").model,
            None,
            "unconfigured models are not labelled"
        );
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
            let app = Router::new().route("/chat/completions", post(refused));
            app.oneshot(Request::post("/chat/completions").body(Body::empty()).unwrap())
                .await
                .unwrap();
        });
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn names_no_caller_for_an_unknown_key() {
        let (_, identity) = rejections(async {
            let request = Request::post("/chat/completions")
                .header("authorization", "Bearer sk-unknown")
                .body(Body::from(r#"{"model": "known-model"}"#))
                .unwrap();
            let response = app().oneshot(request).await.unwrap();
            response.extensions().get::<onwards::RequestIdentity>().cloned()
        });
        assert_eq!(identity, None);
    }

    #[test]
    fn resolves_the_api_key_by_the_protocol_s_rules() {
        let headers = |pairs: &[(&'static str, &'static str)]| {
            let mut headers = HeaderMap::new();
            for (name, value) in pairs {
                headers.insert(*name, HeaderValue::from_static(value));
            }
            headers
        };
        let anthropic = headers(&[("x-api-key", "sk-x")]);
        assert_eq!(api_key("/ai/v1/messages", &anthropic), Some("sk-x"));
        assert_eq!(
            api_key("/ai/v1/chat/completions", &anthropic),
            None,
            "OpenAI routes ignore x-api-key"
        );
        let models = headers(&[("x-api-key", "sk-x"), ("anthropic-version", "2023-06-01")]);
        assert_eq!(api_key("/ai/v1/models", &models), Some("sk-x"));
        let both = headers(&[("authorization", "Bearer sk-a"), ("x-api-key", "sk-x")]);
        assert_eq!(api_key("/ai/v1/messages", &both), Some("sk-a"), "Authorization wins");
        let lowercase = headers(&[("authorization", "bearer sk-a")]);
        assert_eq!(
            api_key("/ai/v1/models", &lowercase),
            Some("sk-a"),
            "the scheme is case-insensitive, as /models accepts it"
        );
        let basic = headers(&[("authorization", "Basic abc"), ("x-api-key", "sk-x")]);
        assert_eq!(
            api_key("/ai/v1/messages", &basic),
            None,
            "a present Authorization is never replaced"
        );
    }

    #[test]
    fn reads_the_model_of_a_body() {
        assert_eq!(model_of(br#"{"model": "m", "x": 1}"#).as_deref(), Some("m"));
        assert_eq!(model_of(b"{not json"), None);
    }
}
