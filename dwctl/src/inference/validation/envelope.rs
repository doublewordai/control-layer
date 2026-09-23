//! Rejection bodies in the surface's own error shape.
//!
//! Every ingress-validation rejection is rendered here so the client sees the
//! same envelope it would have seen from the engine. OpenAI-compatible surfaces
//! (`ChatCompletions`, `Completions`, `Responses`, `Embeddings`) share the
//! `{"error": {..}}` body; `Messages` uses Anthropic's `{"type":"error",..}`
//! body via the translator's existing status -> error-type mapping.
//!
//! All responses carry `content-type: application/json` and a
//! `x-dw-rejected-by: ingress-validation` header so a rejected request is
//! distinguishable from an engine-originated error.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use serde_json::json;

use super::{Surface, Violation};
use crate::inference::translation::anthropic::response::anthropic_error;

/// Header marking a response as produced by ingress validation.
const REJECTED_BY_HEADER: &str = "x-dw-rejected-by";
const REJECTED_BY_VALUE: &str = "ingress-validation";
const CONTENT_TYPE_JSON: &str = "application/json";

/// Render a proven [`Violation`] in the surface's error shape.
pub fn rejection_response(surface: Surface, violation: &Violation) -> Response {
    match surface {
        Surface::Messages => anthropic_rejection(violation.status, &violation.message),
        _ => openai_rejection(violation.status, &violation.message, violation.param, violation.code),
    }
}

/// A request whose body could not be parsed as JSON. `surface` is known when the
/// caller could route the path before parsing, and `None` falls back to the
/// OpenAI shape. `detail` is the parser message, appended verbatim.
pub fn malformed_body_response(surface: Option<Surface>, detail: &str) -> Response {
    let message = format!("Request body is not valid JSON: {detail}");
    match surface {
        Some(Surface::Messages) => anthropic_rejection(StatusCode::BAD_REQUEST, &message),
        _ => openai_rejection(StatusCode::BAD_REQUEST, &message, None, "invalid_json"),
    }
}

/// OpenAI-compatible error body, the shape `onwards` and dwctl use everywhere
/// except Anthropic Messages.
fn openai_rejection(status: StatusCode, message: &str, param: Option<&str>, code: &str) -> Response {
    let body = json!({
        "error": {
            "message": message,
            "type": "invalid_request_error",
            "param": param,
            "code": code,
        }
    });
    json_response(status, &body)
}

/// Anthropic Messages error body. Delegates the status -> error-type mapping to
/// the translator so a validation rejection and an engine error agree.
fn anthropic_rejection(status: StatusCode, message: &str) -> Response {
    let (status, bytes) = anthropic_error(status, message.to_string());
    response(status, Body::from(bytes))
}

fn json_response(status: StatusCode, body: &serde_json::Value) -> Response {
    // Serializing a `serde_json::Value` cannot fail; an empty body is a safe
    // fallback that still carries the status and headers.
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    response(status, Body::from(bytes))
}

/// Attach the status, content type and rejection marker to a body.
fn response(status: StatusCode, body: Body) -> Response {
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE_JSON));
    headers.insert(REJECTED_BY_HEADER, HeaderValue::from_static(REJECTED_BY_VALUE));
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::validation::RuleId;

    fn violation(status: StatusCode, code: &'static str, param: Option<&'static str>) -> Violation {
        Violation {
            rule: RuleId::ContextLengthExceeded,
            status,
            code,
            param,
            message: "too long".to_string(),
        }
    }

    async fn read(resp: Response) -> (StatusCode, serde_json::Value) {
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read rejection body");
        (status, serde_json::from_slice(&bytes).expect("valid JSON body"))
    }

    fn assert_markers(resp: &Response) {
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(resp.headers()[REJECTED_BY_HEADER], "ingress-validation");
    }

    #[tokio::test]
    async fn openai_rejection_has_exact_shape_and_markers() {
        let resp = rejection_response(
            Surface::ChatCompletions,
            &violation(StatusCode::BAD_REQUEST, "context_length_exceeded", Some("messages")),
        );
        assert_markers(&resp);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            json!({
                "error": {
                    "message": "too long",
                    "type": "invalid_request_error",
                    "param": "messages",
                    "code": "context_length_exceeded"
                }
            })
        );
    }

    #[tokio::test]
    async fn openai_rejection_without_param_serializes_null() {
        let resp = rejection_response(Surface::Embeddings, &violation(StatusCode::BAD_REQUEST, "invalid_max_tokens", None));
        let (_, body) = read(resp).await;
        assert_eq!(body["error"]["param"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn anthropic_rejection_has_exact_shape_and_markers() {
        let resp = rejection_response(
            Surface::Messages,
            &violation(StatusCode::TOO_MANY_REQUESTS, "rate_limited", Some("model")),
        );
        assert_markers(&resp);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        // Anthropic has no param/code; the status maps to the error type.
        assert_eq!(
            body,
            json!({
                "type": "error",
                "error": {
                    "type": "rate_limit_error",
                    "message": "too long"
                }
            })
        );
    }

    #[tokio::test]
    async fn anthropic_rejection_maps_status_to_error_type() {
        let resp = rejection_response(
            Surface::Messages,
            &violation(StatusCode::PAYLOAD_TOO_LARGE, "context_length_exceeded", None),
        );
        let (_, body) = read(resp).await;
        assert_eq!(body["error"]["type"], "request_too_large");
    }

    #[tokio::test]
    async fn malformed_without_surface_uses_openai_shape() {
        let resp = malformed_body_response(None, "expected value at line 1 column 1");
        assert_markers(&resp);

        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            json!({
                "error": {
                    "message": "Request body is not valid JSON: expected value at line 1 column 1",
                    "type": "invalid_request_error",
                    "param": null,
                    "code": "invalid_json"
                }
            })
        );
    }

    #[tokio::test]
    async fn malformed_with_non_anthropic_surface_uses_openai_shape() {
        let resp = malformed_body_response(Some(Surface::Responses), "trailing characters");
        let (_, body) = read(resp).await;
        assert_eq!(body["error"]["code"], "invalid_json");
        assert_eq!(body["error"]["message"], "Request body is not valid JSON: trailing characters");
    }

    #[tokio::test]
    async fn malformed_with_messages_surface_uses_anthropic_shape() {
        let resp = malformed_body_response(Some(Surface::Messages), "unexpected EOF");
        assert_markers(&resp);

        let (status, body) = read(resp).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "message": "Request body is not valid JSON: unexpected EOF"
                }
            })
        );
    }
}
