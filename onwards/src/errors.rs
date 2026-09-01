//! Error handling and response structures
//!
//! This module provides standardized error responses that are compatible with
//! OpenAI's API format, ensuring consistent error handling across the proxy.

use axum::{
    Json,
    response::{IntoResponse, Response},
};
use bon::Builder;
use hyper::StatusCode;
use serde::{Deserialize, Serialize};

use crate::reasoning::ReasoningError;

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
        }
    }

    pub fn concurrency_limited() -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: "Too many concurrent requests. Please wait for some requests to complete before sending more.".to_string(),
                r#type: "rate_limit_error".to_string(),
                param: None,
                code: "concurrency_limit_exceeded".to_string(),
            }),
            status: StatusCode::TOO_MANY_REQUESTS,
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
        }
    }
}

/// OpenAI-compatible error envelope: `{"error": {...}}`
#[derive(Debug, Clone, Serialize)]
struct ErrorEnvelope<'a> {
    error: &'a ErrorResponseBody,
}

impl IntoResponse for OnwardsErrorResponse {
    fn into_response(self) -> Response {
        match self.body {
            Some(ref body) => (self.status, Json(ErrorEnvelope { error: body })).into_response(),
            None => self.status.into_response(), // No body, just status
        }
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
