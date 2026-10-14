//! Counting and tracing of the requests onwards refuses itself.
//!
//! Every client error onwards decides on, as opposed to one reporting an
//! upstream's response:
//!
//! - increments `onwards_rejections_total{model, status, code, traffic}`.
//!   `model` is set only for configured aliases, so the label stays bounded
//!   whatever a client sends;
//! - sets `error.type` (the error code), `onwards.rejection.param`,
//!   `onwards.account` and `onwards.api_key_id` (when the key has them) on the
//!   current span, and the response status on onwards' request span. A
//!   strict-mode refusal made before that span exists needs an embedding
//!   gateway's request span; the standalone server has none.
//!
//! An embedding gateway counts the requests its own layers refuse through the
//! same [`RejectionContext`], so one series and one set of span attributes
//! covers every client error the gateway decides on.
//!
//! Request values and bodies are never recorded.

use axum::http::{HeaderMap, StatusCode};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

use crate::{AppState, client::HttpClient, errors::OnwardsErrorResponse, serving, target::Targets};

/// What is known about a request when the gateway refuses it.
#[derive(Debug, Clone)]
pub struct RejectionContext {
    model: Option<String>,
    account: Option<String>,
    api_key_id: Option<Uuid>,
    traffic: &'static str,
}

impl RejectionContext {
    /// The caller and traffic kind, from the request's bearer token and
    /// headers. The model is added once it resolves to a configured alias.
    pub(crate) fn from_request<T: HttpClient>(state: &AppState<T>, headers: &HeaderMap) -> Self {
        let dispatched = state
            .first_token_timeout_exempt_header
            .as_deref()
            .is_some_and(|header| headers.contains_key(header));
        Self::new(&state.targets, headers, dispatched)
    }

    /// The caller, from the request's bearer token and `targets`' key labels.
    /// `dispatched` marks traffic sent by a batch dispatcher rather than a
    /// client waiting for the answer.
    pub(crate) fn new(targets: &Targets, headers: &HeaderMap, dispatched: bool) -> Self {
        let api_key = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        Self::for_api_key(targets, api_key, dispatched)
    }

    /// As [`new`](Self::new), for a gateway that has already resolved the
    /// request's API key under its own protocol's rules.
    pub fn for_api_key(targets: &Targets, api_key: Option<&str>, dispatched: bool) -> Self {
        let labels = api_key.and_then(|key| targets.key_labels.get(key));
        Self {
            model: None,
            account: labels
                .as_ref()
                .and_then(|labels| labels.get(serving::ACCOUNT_LABEL).cloned()),
            api_key_id: labels
                .as_ref()
                .and_then(|labels| labels.get("api_key_id"))
                .and_then(|id| id.parse().ok()),
            traffic: if dispatched { "dispatched" } else { "realtime" },
        }
    }

    /// Records `model`, which must name a configured alias.
    pub(crate) fn set_model(&mut self, model: &str) {
        self.model = Some(model.to_string());
    }

    /// Records the alias `model` selects, with any serving-class suffix
    /// removed, if it is configured.
    pub fn set_model_if_configured(&mut self, targets: &Targets, model: &str) {
        if let Ok((alias, _)) = serving::split_class_suffix(model)
            && targets.targets.contains_key(alias)
        {
            self.set_model(alias);
        }
    }

    /// Records `error` if onwards decided on it itself.
    pub(crate) fn record_error(&self, error: &OnwardsErrorResponse) {
        if !error.is_gateway_rejection() {
            return;
        }
        let (code, param) = error
            .body
            .as_ref()
            .map(|body| (body.code.as_str(), body.param.as_deref()))
            .unwrap_or(("", None));
        self.record(error.status, code, param);
    }

    /// Records a refusal with this status, error code and parameter.
    pub fn record(&self, status: StatusCode, code: &str, param: Option<&str>) {
        let model = self.model.as_deref().unwrap_or("");
        metrics::counter!(
            "onwards_rejections_total",
            "model" => model.to_string(),
            "status" => status.as_str().to_string(),
            "code" => code.to_string(),
            "traffic" => self.traffic,
        )
        .increment(1);

        let span = tracing::Span::current();
        // A no-op on spans that don't declare the field, such as an embedding
        // gateway's, which records the response status itself.
        span.record("http.response.status_code", status.as_u16());
        span.set_attribute("error.type", code.to_string());
        if let Some(param) = param {
            span.set_attribute("onwards.rejection.param", param.to_string());
        }
        if let Some(account) = &self.account {
            span.set_attribute("onwards.account", account.clone());
        }
        if let Some(api_key_id) = self.api_key_id {
            span.set_attribute("onwards.api_key_id", api_key_id.to_string());
        }
    }
}
