//! Error handling and response structures
//!
//! This module provides standardized error responses that are compatible with
//! OpenAI's API format, ensuring consistent error handling across the proxy.

use axum::{
    Json,
    response::{IntoResponse, Response},
};
use bon::Builder;
use hyper::{
    StatusCode,
    header::{HeaderValue, RETRY_AFTER},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::reasoning::ReasoningError;

/// Error code for a request refused because the caller's own concurrency
/// limit is reached.
const CONCURRENCY_LIMIT_CODE: &str = "concurrency_limit_exceeded";
const INFLIGHT_LIMIT_CODE: &str = "inflight_limit_exceeded";
/// Error code for a batch request refused because the model's global batch
/// in-flight cap is reached. A 529, never a 429: the batch dispatcher cuts its
/// adaptive concurrency on overload, so it must back off rather than treat the
/// refusal as a per-key rate limit.
const BATCH_CAPACITY_CODE: &str = "batch_capacity_exceeded";
/// Error code for a request refused because the model's providers are full.
const OVERLOADED_CODE: &str = "overloaded";
/// Error code for a request refused because the model has no provider
/// serving it right now.
const NO_CAPACITY_CODE: &str = "no_capacity";

/// Seconds a caller is told to wait before retrying a refusal with `code`.
fn retry_after_secs(code: &str) -> Option<&'static str> {
    match code {
        // Room frees as in-flight requests finish.
        CONCURRENCY_LIMIT_CODE | INFLIGHT_LIMIT_CODE | BATCH_CAPACITY_CODE | OVERLOADED_CODE => {
            Some("1")
        }
        // A provider has to be placed and start before anything is served.
        NO_CAPACITY_CODE => Some("30"),
        _ => None,
    }
}

/// 529: the platform refused the request for capacity. Distinct from 503,
/// which generic infrastructure sends when a backend is down, and from 429,
/// which is the caller's own limit.
fn overload_status() -> StatusCode {
    StatusCode::from_u16(529).expect("529 is a valid status code")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponseBody {
    pub message: String,
    pub r#type: String,
    pub param: Option<String>,
    pub code: String,
}

#[derive(Debug, Clone, Builder)]
pub struct OnwardsErrorResponse {
    pub body: Option<ErrorResponseBody>,
    pub status: StatusCode,
    /// What the request resolved to before it failed, when it got that far.
    /// Attached to the response as an extension so analytics records the
    /// class for failed requests too.
    pub serving_outcome: Option<crate::serving::ServingClassOutcome>,
    pub(crate) authenticated_api_key_id: Option<Uuid>,
}

impl OnwardsErrorResponse {
    pub fn reasoning(error: &ReasoningError) -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: error.message().to_owned(),
                r#type: "invalid_request_error".to_string(),
                param: error.param().map(str::to_string),
                code: error.code().to_string(),
            }),
            status: StatusCode::from_u16(error.status_code())
                .expect("reasoning errors use valid HTTP status codes"),
            serving_outcome: None,
            authenticated_api_key_id: None,
        }
    }

    pub fn model_not_found(model: &str) -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: format!(
                    "The model `{model}` does not exist or you do not have access to it."
                ),
                r#type: "invalid_request_error".to_string(),
                param: None,
                code: "model_not_found".to_string(),
            }),
            status: StatusCode::NOT_FOUND,
            serving_outcome: None,
            authenticated_api_key_id: None,
        }
    }

    pub fn rate_limited() -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: "You are sending requests too quickly. Please slow down.".to_string(),
                r#type: "rate_limit_error".to_string(),
                param: None,
                code: "rate_limit".to_string(),
            }),
            status: StatusCode::TOO_MANY_REQUESTS,
            serving_outcome: None,
            authenticated_api_key_id: None,
        }
    }

    /// An upstream rate limit, distinct from limits enforced by this proxy.
    pub fn upstream_rate_limited(message: Option<&str>) -> Self {
        Self {
            body: Some(ErrorResponseBody {
                message: message
                    .unwrap_or("The upstream service is rate limited. Please try again later.")
                    .to_string(),
                r#type: "rate_limit_error".to_string(),
                param: None,
                code: "upstream_rate_limit".to_string(),
            }),
            status: StatusCode::TOO_MANY_REQUESTS,
            serving_outcome: None,
            authenticated_api_key_id: None,
        }
    }

    pub fn concurrency_limited() -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: "Too many concurrent requests. Please wait for some requests to complete before sending more.".to_string(),
                r#type: "rate_limit_error".to_string(),
                param: None,
                code: CONCURRENCY_LIMIT_CODE.to_string(),
            }),
            status: StatusCode::TOO_MANY_REQUESTS,
            serving_outcome: None,
            authenticated_api_key_id: None,
        }
    }

    pub fn inflight_limited(model: &str, limit: u32) -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: format!(
                    "Too many realtime requests in flight for model '{model}'. Your account's limit is {limit}; retry once one completes."
                ),
                r#type: "rate_limit_error".to_string(),
                param: None,
                code: INFLIGHT_LIMIT_CODE.to_string(),
            }),
            status: StatusCode::TOO_MANY_REQUESTS,
            serving_outcome: None,
            authenticated_api_key_id: None,
        }
    }

    /// A batch request refused because the model's global batch in-flight cap is
    /// reached. Deliberately a 529 (`overloaded_error`), not a 429: the cap is a
    /// ceiling on batch traffic so it cannot crowd out realtime, and the batch
    /// dispatcher cuts its adaptive concurrency on 529.
    pub fn batch_capacity_exceeded(model: &str, limit: u32) -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: format!(
                    "The batch in-flight cap of {limit} for model '{model}' is reached; retry once a batch request completes."
                ),
                r#type: "overloaded_error".to_string(),
                param: None,
                code: BATCH_CAPACITY_CODE.to_string(),
            }),
            status: overload_status(),
            serving_outcome: None,
            authenticated_api_key_id: None,
        }
    }

    /// The model's providers are full right now.
    pub fn overloaded() -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: "The model is at capacity. Please retry shortly.".to_string(),
                r#type: "overloaded_error".to_string(),
                param: None,
                code: OVERLOADED_CODE.to_string(),
            }),
            status: overload_status(),
            serving_outcome: None,
            authenticated_api_key_id: None,
        }
    }

    /// The model has no provider serving it right now.
    pub fn no_capacity() -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: "The model has no capacity available right now. Please retry later."
                    .to_string(),
                r#type: "overloaded_error".to_string(),
                param: None,
                code: NO_CAPACITY_CODE.to_string(),
            }),
            status: overload_status(),
            serving_outcome: None,
            authenticated_api_key_id: None,
        }
    }

    pub fn internal() -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: "An internal error occurred. Please try again later.".to_string(),
                r#type: "internal_error".to_string(),
                param: None,
                code: "internal_error".to_string(),
            }),
            status: StatusCode::INTERNAL_SERVER_ERROR,
            serving_outcome: None,
            authenticated_api_key_id: None,
        }
    }

    pub fn bad_gateway() -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: "An internal error occurred. Please try again later.".to_string(),
                r#type: "internal_error".to_string(),
                param: None,
                code: "internal_error".to_string(),
            }),
            status: StatusCode::BAD_GATEWAY,
            serving_outcome: None,
            authenticated_api_key_id: None,
        }
    }

    pub fn service_unavailable() -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: "An internal error occurred. Please try again later.".to_string(),
                r#type: "internal_error".to_string(),
                param: None,
                code: "service_unavailable".to_string(),
            }),
            status: StatusCode::SERVICE_UNAVAILABLE,
            serving_outcome: None,
            authenticated_api_key_id: None,
        }
    }

    pub fn gateway_timeout() -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: "The upstream service took too long to respond. Please try again."
                    .to_string(),
                r#type: "internal_error".to_string(),
                param: None,
                code: "gateway_timeout".to_string(),
            }),
            status: StatusCode::GATEWAY_TIMEOUT,
            serving_outcome: None,
            authenticated_api_key_id: None,
        }
    }

    pub fn payload_too_large(limit: usize) -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: format!(
                    "Request body too large: the maximum allowed size is {limit} bytes."
                ),
                r#type: "invalid_request_error".to_string(),
                param: None,
                code: "payload_too_large".to_string(),
            }),
            status: StatusCode::PAYLOAD_TOO_LARGE,
            serving_outcome: None,
            authenticated_api_key_id: None,
        }
    }

    pub fn unprocessable_request(message: &str, param: Option<&str>) -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: message.to_owned(),
                r#type: "invalid_request_error".to_string(),
                param: param.map(|s| s.to_string()),
                code: "unprocessable_request".to_string(),
            }),
            status: StatusCode::UNPROCESSABLE_ENTITY,
            serving_outcome: None,
            authenticated_api_key_id: None,
        }
    }

    pub fn bad_request(message: &str, param: Option<&str>) -> Self {
        Self::invalid_request(message, param, "bad_request")
    }

    pub fn invalid_request(message: &str, param: Option<&str>, code: &str) -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: message.to_owned(),
                r#type: "invalid_request_error".to_string(),
                param: param.map(|s| s.to_string()),
                code: code.to_string(),
            }),
            status: StatusCode::BAD_REQUEST,
            serving_outcome: None,
            authenticated_api_key_id: None,
        }
    }

    pub fn forbidden() -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: "Forbidden".to_string(),
                r#type: "invalid_request_error".to_string(),
                param: None,
                code: "forbidden".to_string(),
            }),
            status: StatusCode::FORBIDDEN,
            serving_outcome: None,
            authenticated_api_key_id: None,
        }
    }

    pub fn unauthorized() -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: "Please supply an authentication token to access this resource"
                    .to_string(),
                r#type: "invalid_request_error".to_string(),
                param: None,
                code: "unauthenticated".to_string(),
            }),
            status: StatusCode::UNAUTHORIZED,
            serving_outcome: None,
            authenticated_api_key_id: None,
        }
    }

    pub(crate) fn with_authenticated_api_key_id(mut self, api_key_id: Option<Uuid>) -> Self {
        self.authenticated_api_key_id = api_key_id;
        self
    }
}

/// OpenAI-compatible error envelope: `{"error": {...}}`
#[derive(Debug, Clone, Serialize)]
struct ErrorEnvelope<'a> {
    error: &'a ErrorResponseBody,
}

impl IntoResponse for OnwardsErrorResponse {
    fn into_response(self) -> Response {
        let mut response = match self.body {
            Some(ref body) => (self.status, Json(ErrorEnvelope { error: body })).into_response(),
            None => self.status.into_response(), // No body, just status
        };
        // SDKs that honour Retry-After wait this long before retrying.
        if let Some(secs) = self
            .body
            .as_ref()
            .and_then(|body| retry_after_secs(&body.code))
        {
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from_static(secs));
        }
        if let Some(outcome) = self.serving_outcome {
            response.extensions_mut().insert(outcome);
        }
        if let Some(api_key_id) = self.authenticated_api_key_id {
            response
                .extensions_mut()
                .insert(crate::AuthenticatedApiKeyId(api_key_id));
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn test_error_response_has_openai_envelope() {
        let error = OnwardsErrorResponse::rate_limited();
        let response = error.into_response();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

        // Must be wrapped in {"error": {...}} envelope
        assert!(body.get("error").is_some(), "Missing error envelope");
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert_eq!(body["error"]["code"], "rate_limit");
        assert_eq!(
            body["error"]["message"],
            "You are sending requests too quickly. Please slow down."
        );

        // Must NOT have fields at the top level
        assert!(body.get("type").is_none());
        assert!(body.get("code").is_none());
        assert!(body.get("message").is_none());
    }

    #[tokio::test]
    async fn test_error_response_no_body() {
        let error = OnwardsErrorResponse::builder()
            .status(StatusCode::NO_CONTENT)
            .build();
        let response = error.into_response();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(body_bytes.is_empty());
    }

    #[tokio::test]
    async fn test_forbidden_response_uses_forbidden_message_and_code() {
        let error = OnwardsErrorResponse::forbidden();
        let response = error.into_response();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

        assert_eq!(body["error"]["message"], "Forbidden");
        assert_eq!(body["error"]["code"], "forbidden");
    }
}
