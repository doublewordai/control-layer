//! Counting and logging of the requests onwards refuses itself.
//!
//! Every client error onwards decides on, as opposed to one reporting an
//! upstream's response, increments
//! `onwards_rejections_total{model, status, code, traffic}` and is logged once
//! at `info` with its code, parameter, model, account and API key ID. Request
//! values and bodies are never logged. `model` is set only for configured
//! aliases, so the label stays bounded whatever a client sends.

use axum::http::{HeaderMap, StatusCode};
use tracing::info;
use uuid::Uuid;

use crate::{AppState, client::HttpClient, errors::OnwardsErrorResponse, serving};

/// What is known about a request when onwards refuses it.
#[derive(Debug, Clone)]
pub(crate) struct RejectionContext {
    model: Option<String>,
    account: Option<String>,
    api_key_id: Option<Uuid>,
    traffic: &'static str,
}

impl RejectionContext {
    /// The caller and traffic kind, from the request's bearer token and
    /// headers. The model is added once it resolves to a configured alias.
    pub(crate) fn from_request<T: HttpClient>(state: &AppState<T>, headers: &HeaderMap) -> Self {
        let labels = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .and_then(|token| state.targets.key_labels.get(token));
        let dispatched = state
            .first_token_timeout_exempt_header
            .as_deref()
            .is_some_and(|header| headers.contains_key(header));
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

    /// Records `model` if it names a configured alias.
    pub(crate) fn set_model_if_configured<T: HttpClient>(
        &mut self,
        state: &AppState<T>,
        model: &str,
    ) {
        if state.targets.targets.contains_key(model) {
            self.set_model(model);
        }
    }

    /// Counts and logs `error` if onwards decided on it itself.
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

    /// Counts and logs a refusal with this status, error code and parameter.
    pub(crate) fn record(&self, status: StatusCode, code: &str, param: Option<&str>) {
        let model = self.model.as_deref().unwrap_or("");
        metrics::counter!(
            "onwards_rejections_total",
            "model" => model.to_string(),
            "status" => status.as_str().to_string(),
            "code" => code.to_string(),
            "traffic" => self.traffic,
        )
        .increment(1);
        info!(
            status = status.as_u16(),
            code,
            param = param.unwrap_or(""),
            model,
            account = self.account.as_deref().unwrap_or(""),
            api_key_id = ?self.api_key_id,
            traffic = self.traffic,
            "Request rejected by the gateway"
        );
    }
}
