//! Anthropic models-list (`GET /v1/models`) edge translator.
//!
//! Both the OpenAI and Anthropic SDKs call `GET /v1/models` on the same path,
//! so - unlike [`super::AnthropicMessages`], whose `/messages` route is
//! unambiguous - this translator MUST gate on a header to claim the request: the
//! Anthropic client always sends `anthropic-version`, the OpenAI client never
//! does. A native OpenAI `/v1/models` call has no such header, so it falls
//! through `detect()` and passes through untouched.
//!
//! The request needs no body translation (it is a GET); we only promote
//! `x-api-key` to `Authorization: Bearer`. The path is already canonical
//! (`/v1/models` routes straight to onwards' models handler), so there is no
//! path normalisation. The response - onwards' OpenAI-shaped list - is reshaped
//! into the Anthropic models-list shape.

use axum::http::{HeaderMap, StatusCode, request::Parts};
use bytes::Bytes;
use serde_json::Value;

use super::super::{ProtocolTranslator, StreamReframer, TranslatedRequest, TranslationError};
use super::model::{ModelObject, ModelObjectType, ModelsListResponse};
use super::{normalize_auth, response};

/// Translator for the Anthropic models-list endpoint.
pub struct AnthropicModels;

impl ProtocolTranslator for AnthropicModels {
    fn name(&self) -> &'static str {
        "anthropic_models"
    }

    fn detect(&self, path: &str, headers: &HeaderMap) -> bool {
        // The path is shared with OpenAI's `/v1/models`; the `anthropic-version`
        // header (always sent by the Anthropic SDK, never by OpenAI's) is the
        // only reliable discriminator.
        path.ends_with("/models") && headers.contains_key("anthropic-version")
    }

    fn translate_request(&self, parts: &Parts, body: Bytes) -> Result<TranslatedRequest, TranslationError> {
        // A GET with no body to translate. The path already targets onwards'
        // models handler, so we leave it as-is and only normalise auth.
        let mut headers = parts.headers.clone();
        normalize_auth(&mut headers);
        Ok(TranslatedRequest {
            uri: parts.uri.clone(),
            headers,
            body,
        })
    }

    fn translate_response(&self, body: Bytes) -> Result<Bytes, TranslationError> {
        from_openai_models(body)
    }

    fn translate_error(&self, status: StatusCode, body: Bytes) -> (StatusCode, Bytes) {
        response::error_to_anthropic(status, body)
    }

    fn error_from_message(&self, status: StatusCode, message: &str) -> (StatusCode, Bytes) {
        response::anthropic_error(status, message.to_string())
    }

    fn stream_reframer(&self) -> Box<dyn StreamReframer> {
        // The models list is never streamed (the response is application/json, so
        // the middleware never reaches the SSE path); a no-op satisfies the trait.
        Box::new(NoopReframer)
    }
}

/// Reshape onwards' OpenAI models list (`{object:"list", data:[{id, created,
/// ...}]}`) into the Anthropic models-list shape.
fn from_openai_models(body: Bytes) -> Result<Bytes, TranslationError> {
    let resp: Value = serde_json::from_slice(&body).map_err(|e| TranslationError::Internal(format!("parse models response: {e}")))?;

    let data: Vec<ModelObject> = resp
        .get("data")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|m| {
                    let id = m.get("id").and_then(Value::as_str)?.to_string();
                    // OpenAI `created` is unix seconds; Anthropic `created_at` is
                    // RFC 3339. onwards always sets it, but default to the epoch
                    // rather than failing if it is ever absent.
                    let secs = m.get("created").and_then(Value::as_i64).unwrap_or(0);
                    let created_at = chrono::DateTime::from_timestamp(secs, 0).unwrap_or_default().to_rfc3339();
                    Some(ModelObject {
                        object_type: ModelObjectType::Model,
                        display_name: id.clone(),
                        id,
                        created_at,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let first_id = data.first().map(|m| m.id.clone());
    let last_id = data.last().map(|m| m.id.clone());

    let out = ModelsListResponse {
        data,
        has_more: false,
        first_id,
        last_id,
    };

    serde_json::to_vec(&out)
        .map(Bytes::from)
        .map_err(|e| TranslationError::Internal(e.to_string()))
}

/// A reframer that emits nothing. Unreachable for `/models` (never streamed);
/// exists only to satisfy [`ProtocolTranslator::stream_reframer`].
struct NoopReframer;

impl StreamReframer for NoopReframer {
    fn push(&mut self, _chunk: &Value) -> Vec<u8> {
        Vec::new()
    }

    fn error(&mut self, _message: &str) -> Vec<u8> {
        Vec::new()
    }

    fn finish(&mut self) -> Vec<u8> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use serde_json::json;

    #[test]
    fn detect_requires_models_path_and_anthropic_header() {
        let t = AnthropicModels;
        let mut h = HeaderMap::new();
        h.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        // Anthropic SDK: /v1/models + anthropic-version -> claimed.
        assert!(t.detect("/v1/models", &h));
        assert!(t.detect("/models", &h));
        // Native OpenAI /v1/models (no anthropic-version) -> NOT claimed.
        assert!(!t.detect("/v1/models", &HeaderMap::new()));
        // A single-model retrieve path is not the list endpoint.
        assert!(!t.detect("/v1/models/claude-x", &h));
        // Messages is not ours.
        assert!(!t.detect("/v1/messages", &h));
    }

    #[test]
    fn request_promotes_x_api_key_and_leaves_path_unchanged() {
        let req = axum::http::Request::builder()
            .uri("/ai/v1/models")
            .header("x-api-key", "sk-test")
            .body(())
            .unwrap();
        let (parts, ()) = req.into_parts();
        let out = AnthropicModels.translate_request(&parts, Bytes::new()).unwrap();
        assert_eq!(out.uri.path(), "/ai/v1/models");
        assert_eq!(out.headers.get(axum::http::header::AUTHORIZATION).unwrap(), "Bearer sk-test");
    }

    #[test]
    fn response_reshapes_openai_list_to_anthropic() {
        let openai = json!({
            "object": "list",
            "data": [
                { "id": "model-a", "created": 1_700_000_000, "object": "model", "owned_by": "None" },
                { "id": "model-b", "created": 1_700_000_500, "object": "model", "owned_by": "None" }
            ]
        });
        let bytes = from_openai_models(Bytes::from(serde_json::to_vec(&openai).unwrap())).unwrap();
        let out: Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(out["has_more"], false);
        assert_eq!(out["first_id"], "model-a");
        assert_eq!(out["last_id"], "model-b");
        let data = out["data"].as_array().unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["type"], "model");
        assert_eq!(data[0]["id"], "model-a");
        assert_eq!(data[0]["display_name"], "model-a");
        // unix 1_700_000_000 -> 2023-11-14T22:13:20+00:00
        assert_eq!(data[0]["created_at"], "2023-11-14T22:13:20+00:00");
        // The OpenAI-only `object`/`owned_by` fields are gone.
        assert!(data[0].get("object").is_none());
        assert!(data[0].get("owned_by").is_none());
    }

    #[test]
    fn response_handles_empty_list() {
        let openai = json!({ "object": "list", "data": [] });
        let bytes = from_openai_models(Bytes::from(serde_json::to_vec(&openai).unwrap())).unwrap();
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out["data"].as_array().unwrap().len(), 0);
        assert_eq!(out["has_more"], false);
        // No models -> pagination cursors omitted.
        assert!(out.get("first_id").is_none());
        assert!(out.get("last_id").is_none());
    }
}
