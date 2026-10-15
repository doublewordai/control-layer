//! Counting and tracing of the requests onwards refuses itself.
//!
//! Every client error onwards decides on, as opposed to one reporting an
//! upstream's response:
//!
//! - increments `onwards_rejections_total{model, status, code, traffic}`.
//!   `model` is set only for configured aliases, so the label stays bounded
//!   whatever a client sends;
//! - sets `error.type` (the error code), `onwards.rejection.param`,
//!   `onwards.rejection.reason` (how a body failed the schema),
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
//!
//! The caller's identity ([`RequestIdentity`]) is also recorded on onwards'
//! request span for every authenticated request, not only refusals, and
//! attached to the response as an extension so an embedding gateway can copy
//! it onto its own root span.

use axum::http::{HeaderMap, StatusCode};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

use crate::{AppState, client::HttpClient, errors::OnwardsErrorResponse, serving, target::Targets};

/// Who made a request, from the key labels synced by the embedding gateway.
///
/// Attached to every response whose bearer token matched a known key (success,
/// gateway refusal or upstream error alike) as a response extension. It never
/// leaves the process. Absent when the request carried no known key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestIdentity {
    /// Owning account (`account` key label).
    pub account_id: Option<String>,
    /// Display name of the owning account (`account_display_name` key label,
    /// falling back to `account_name`).
    pub account_name: Option<String>,
    /// The API key used.
    pub api_key_id: Option<Uuid>,
    /// The key's purpose (`purpose` key label).
    pub key_purpose: Option<String>,
    /// The configured model alias, once the request resolved to one.
    pub model: Option<String>,
}

/// What is known about a request when the gateway refuses it.
#[derive(Debug, Clone)]
pub struct RejectionContext {
    identity: RequestIdentity,
    authenticated: bool,
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
        let label = |name: &str| labels.as_ref().and_then(|labels| labels.get(name).cloned());
        Self {
            authenticated: labels.is_some(),
            identity: RequestIdentity {
                model: None,
                account_id: label(serving::ACCOUNT_LABEL),
                account_name: label(serving::ACCOUNT_DISPLAY_NAME_LABEL)
                    .or_else(|| label(serving::ACCOUNT_NAME_LABEL)),
                api_key_id: label("api_key_id").and_then(|id| id.parse().ok()),
                key_purpose: label("purpose"),
            },
            traffic: if dispatched { "dispatched" } else { "realtime" },
        }
    }

    /// Records `model`, which must name a configured alias.
    pub(crate) fn set_model(&mut self, model: &str) {
        self.identity.model = Some(model.to_string());
    }

    /// Records the alias `model` selects, with any serving-class suffix
    /// removed, if it is configured.
    pub fn set_model_if_configured(&mut self, targets: &Targets, model: &str) {
        if let Some(alias) = targets.configured_alias(model) {
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
        self.record_with_reason(status, code, param, None);
    }

    /// Records a refusal, with a fixed `reason` saying how the parameter was
    /// wrong. Neither may hold request content: both are recorded for every
    /// caller.
    pub fn record_with_reason(
        &self,
        status: StatusCode,
        code: &str,
        param: Option<&str>,
        reason: Option<&'static str>,
    ) {
        let model = self.identity.model.as_deref().unwrap_or("");
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
        if let Some(reason) = reason {
            span.set_attribute("onwards.rejection.reason", reason);
        }
        self.record_identity();
    }

    /// The identity to attach to the response, if the request's key is known.
    pub fn identity(&self) -> Option<RequestIdentity> {
        self.authenticated.then(|| self.identity.clone())
    }

    /// Records the caller's identity on the current span. A no-op for
    /// requests without a known key.
    pub(crate) fn record_identity(&self) {
        if !self.authenticated {
            return;
        }
        let span = tracing::Span::current();
        let identity = &self.identity;
        if let Some(account) = &identity.account_id {
            span.set_attribute("onwards.account", account.clone());
        }
        if let Some(name) = &identity.account_name {
            span.set_attribute("onwards.account_name", name.clone());
        }
        if let Some(api_key_id) = identity.api_key_id {
            span.set_attribute("onwards.api_key_id", api_key_id.to_string());
        }
        if let Some(purpose) = &identity.key_purpose {
            span.set_attribute("onwards.key_purpose", purpose.clone());
        }
    }
}
