//! Probe execution engine for testing API endpoints.
//!
//! This module provides the `ProbeExecutor` which handles the actual HTTP requests
//! to monitored endpoints. It constructs appropriate payloads for different endpoint
//! types (chat completions vs embeddings) and measures response times.

use crate::db::models::deployments::ModelType;
use crate::db::models::probes::ProbeExecution;
use anyhow::Result;
use reqwest::Client;
use serde_json::json;
use std::time::Instant;
use uuid::Uuid;

/// Data needed to execute a probe, fetched from database
pub struct ProbeExecutionContext {
    pub probe_id: Uuid,
    pub model_name: String,
    pub model_type: ModelType,
    pub endpoint_url: String,
    pub api_key: Option<String>,
    pub http_method: String,
    pub request_path: Option<String>,
    pub request_body: Option<serde_json::Value>,
}

/// Executes health check requests against API endpoints.
///
/// The executor maintains an HTTP client and constructs type-appropriate
/// payloads for chat completion and embedding endpoints.
pub struct ProbeExecutor {
    client: Client,
}

impl ProbeExecutor {
    /// Create a new probe executor with a default HTTP client.
    pub fn new() -> Self {
        Self { client: Client::new() }
    }

    /// Get default URL and payload for a model type
    fn get_default_config(model_type: &ModelType, model_name: &str, endpoint_url: &str) -> (String, serde_json::Value) {
        match model_type {
            ModelType::Chat => (
                format!("{}/v1/chat/completions", endpoint_url.trim_end_matches('/')),
                json!({
                    "model": model_name,
                    "messages": [{"role": "user", "content": "Hello, this is a health check probe."}],
                    "max_tokens": 10
                }),
            ),
            ModelType::Embeddings => (
                format!("{}/v1/embeddings", endpoint_url.trim_end_matches('/')),
                json!({
                    "model": model_name,
                    "input": "Health check probe"
                }),
            ),
            ModelType::Reranker => (
                format!("{}/v1/rerank", endpoint_url.trim_end_matches('/')),
                json!({
                    "model": model_name,
                    "query": "Health check probe",
                    "documents": ["test document"]
                }),
            ),
        }
    }

    /// Execute a probe against its configured endpoint.
    ///
    /// Constructs an appropriate test payload based on the model type,
    /// sends the request, and measures the response time. Returns a
    /// `ProbeExecution` regardless of success or failure to ensure
    /// all execution attempts are captured.
    pub async fn execute(&self, context: ProbeExecutionContext) -> Result<ProbeExecution> {
        let start = Instant::now();

        // Get default config based on model type, then override with custom values if provided
        let (default_url, default_payload) = Self::get_default_config(&context.model_type, &context.model_name, &context.endpoint_url);

        let full_url = context
            .request_path
            .as_ref()
            .map(|path| format!("{}{}", context.endpoint_url.trim_end_matches('/'), path))
            .unwrap_or(default_url);

        let payload = context.request_body.clone().unwrap_or(default_payload);

        // Build and send request with the configured HTTP method
        let mut request = match context.http_method.to_uppercase().as_str() {
            "GET" => self.client.get(&full_url),
            "POST" => self.client.post(&full_url).json(&payload),
            "PUT" => self.client.put(&full_url).json(&payload),
            "PATCH" => self.client.patch(&full_url).json(&payload),
            "DELETE" => self.client.delete(&full_url),
            _ => self.client.post(&full_url).json(&payload), // Default to POST
        };

        if let Some(api_key) = &context.api_key {
            request = request.header("Authorization", format!("Bearer {}", api_key));
        }

        let response = request.send().await;
        let elapsed = start.elapsed().as_millis() as i32;

        // Process response
        match response {
            Ok(resp) => {
                let status_code = resp.status().as_u16() as i32;

                // Get response body as text first
                let body_text = match resp.text().await {
                    Ok(text) => text,
                    Err(e) => {
                        return Ok(ProbeExecution {
                            probe_id: context.probe_id,
                            success: false,
                            response_time_ms: elapsed,
                            status_code: Some(status_code),
                            error_message: Some(format!("HTTP {} - Failed to read response body: {}", status_code, e)),
                            response_data: None,
                            metadata: None,
                        });
                    }
                };

                // Try to parse as JSON
                match serde_json::from_str::<serde_json::Value>(&body_text) {
                    Ok(response_data) => {
                        // Check if the response contains an error, even if HTTP status is 200
                        // Some OpenAI-compatible APIs (vLLM) return HTTP 200 with error details in the body
                        let is_error_response = response_data.get("object").and_then(|o| o.as_str()) == Some("error")
                            || response_data
                                .get("code")
                                .and_then(|c| c.as_i64())
                                .map(|c| c >= 400)
                                .unwrap_or(false);

                        if (200..300).contains(&status_code) && !is_error_response {
                            Ok(ProbeExecution {
                                probe_id: context.probe_id,
                                success: true,
                                response_time_ms: elapsed,
                                status_code: Some(status_code),
                                error_message: None,
                                response_data: Some(response_data),
                                metadata: None,
                            })
                        } else {
                            let error_msg = response_data
                                .get("message")
                                .and_then(|e| e.as_str())
                                .or_else(|| {
                                    response_data.get("error").and_then(|e| match e {
                                        serde_json::Value::String(s) => Some(s.as_str()),
                                        serde_json::Value::Object(map) => map.get("message").and_then(|m| m.as_str()),
                                        _ => None,
                                    })
                                })
                                .unwrap_or("Unknown error");

                            Ok(ProbeExecution {
                                probe_id: context.probe_id,
                                success: false,
                                response_time_ms: elapsed,
                                status_code: Some(status_code),
                                error_message: Some(format!("HTTP {} - {}", status_code, error_msg)),
                                response_data: Some(response_data),
                                metadata: None,
                            })
                        }
                    }
                    Err(e) => Ok(ProbeExecution {
                        probe_id: context.probe_id,
                        success: false,
                        response_time_ms: elapsed,
                        status_code: Some(status_code),
                        error_message: Some(format!(
                            "HTTP {} - Failed to parse response as JSON: {}. Response body: {}",
                            status_code, e, body_text
                        )),
                        response_data: None,
                        metadata: None,
                    }),
                }
            }
            Err(e) => Ok(ProbeExecution {
                probe_id: context.probe_id,
                success: false,
                response_time_ms: elapsed,
                status_code: None,
                error_message: Some(e.to_string()),
                response_data: None,
                metadata: None,
            }),
        }
    }
}

impl Default for ProbeExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Build a chat-completion probe context pointing at `endpoint_url`.
    /// Mirrors the production default probe body (no `service_tier`), so the executor
    /// targets `{endpoint_url}/v1/chat/completions` with the default payload.
    fn ctx_for(endpoint_url: String) -> ProbeExecutionContext {
        ProbeExecutionContext {
            probe_id: Uuid::nil(),
            model_name: "test-model".to_string(),
            model_type: ModelType::Chat,
            endpoint_url,
            api_key: None,
            http_method: "POST".to_string(),
            request_path: None,
            request_body: None,
        }
    }

    /// OpenAI-hosted envelope shape (path K, verbatim upstream passthrough):
    /// `{"error": {"message": ...}}`. Before the fix this collapsed to
    /// "Unknown error"; the fix descends into the nested `error.message`.
    #[tokio::test]
    async fn object_form_error_envelope_yields_nested_message() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": {
                    "message": "model_not_found",
                    "type": "invalid_request_error",
                    "param": null,
                    "code": "model_not_found"
                }
            })))
            .mount(&mock)
            .await;

        let result = ProbeExecutor::new().execute(ctx_for(mock.uri())).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.status_code, Some(404));
        assert_eq!(
            result.error_message.as_deref(),
            Some("HTTP 404 - model_not_found"),
            "nested error.message should be surfaced verbatim in the operator summary"
        );
        // The full upstream body is still persisted unchanged.
        assert_eq!(
            result
                .response_data
                .as_ref()
                .unwrap()
                .get("error")
                .unwrap()
                .get("message")
                .unwrap()
                .as_str(),
            Some("model_not_found")
        );
    }

    /// Onwards-synthesized envelope (OnwardsErrorResponse path D and sanitized paths A/B):
    /// object form with a classified message.
    #[tokio::test]
    async fn onwards_synthesized_envelope_yields_classified_message() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(502).set_body_json(serde_json::json!({
                "error": {
                    "message": "The upstream provider rejected the request.",
                    "type": "invalid_request_error",
                    "param": null,
                    "code": "upstream_error"
                }
            })))
            .mount(&mock)
            .await;

        let result = ProbeExecutor::new().execute(ctx_for(mock.uri())).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.status_code, Some(502));
        assert_eq!(
            result.error_message.as_deref(),
            Some("HTTP 502 - The upstream provider rejected the request.")
        );
    }

    /// Legacy string-form `{"error": "..."}` (some providers and simple proxies) is still handled.
    #[tokio::test]
    async fn string_form_error_field_is_still_extracted() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": "invalid_api_key"
            })))
            .mount(&mock)
            .await;

        let result = ProbeExecutor::new().execute(ctx_for(mock.uri())).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.status_code, Some(401));
        assert_eq!(result.error_message.as_deref(), Some("HTTP 401 - invalid_api_key"));
    }

    /// A top-level `message` is still extracted first (it takes priority over `error`).
    #[tokio::test]
    async fn top_level_message_field_is_extracted() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "message": "rate_limit_exceeded"
            })))
            .mount(&mock)
            .await;

        let result = ProbeExecutor::new().execute(ctx_for(mock.uri())).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.status_code, Some(429));
        assert_eq!(result.error_message.as_deref(), Some("HTTP 429 - rate_limit_exceeded"));
    }

    /// When the nested `error` object has no `message` child, fall back to "Unknown error"
    /// rather than panicking or emitting a debug string.
    #[tokio::test]
    async fn object_form_error_without_message_falls_back_to_unknown() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "error": { "type": "internal", "code": 500 }
            })))
            .mount(&mock)
            .await;

        let result = ProbeExecutor::new().execute(ctx_for(mock.uri())).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.status_code, Some(500));
        assert_eq!(result.error_message.as_deref(), Some("HTTP 500 - Unknown error"));
    }

    /// Non-string, non-object `error` values (numbers, arrays, booleans, null) also fall back
    /// to "Unknown error" — the `_` match arm does not recover a message from them.
    #[tokio::test]
    async fn non_string_non_object_error_falls_back_to_unknown() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "error": 503
            })))
            .mount(&mock)
            .await;

        let result = ProbeExecutor::new().execute(ctx_for(mock.uri())).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.status_code, Some(500));
        assert_eq!(result.error_message.as_deref(), Some("HTTP 500 - Unknown error"));
    }

    /// vLLM-style HTTP-200 error body (`{"object":"error","message":...}`) is flagged as an
    /// error response, recorded as `success=false`, and its top-level `message` recovered.
    #[tokio::test]
    async fn vllm_200_with_error_body_is_flagged_and_message_extracted() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "error",
                "message": "model is overloaded"
            })))
            .mount(&mock)
            .await;

        let result = ProbeExecutor::new().execute(ctx_for(mock.uri())).await.unwrap();

        assert!(!result.success, "a 200 body marked object=error must be a failure");
        assert_eq!(result.status_code, Some(200));
        assert_eq!(result.error_message.as_deref(), Some("HTTP 200 - model is overloaded"));
    }

    /// A genuine 2xx success records no error message and persists the body.
    #[tokio::test]
    async fn success_response_records_no_error() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "choices": [{
                    "message": { "role": "assistant", "content": "hi" },
                    "finish_reason": "stop",
                    "index": 0
                }]
            })))
            .mount(&mock)
            .await;

        let result = ProbeExecutor::new().execute(ctx_for(mock.uri())).await.unwrap();

        assert!(result.success);
        assert_eq!(result.status_code, Some(200));
        assert!(result.error_message.is_none());
        assert!(result.response_data.is_some());
    }

    /// An OpenAI-style 4xx over the embeddings default probe path is also handled
    /// (validates the fix is not accidentally chat-specific).
    #[tokio::test]
    async fn embeddings_probe_object_form_error_yields_nested_message() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {
                    "message": "input is required",
                    "type": "invalid_request_error",
                    "param": "input",
                    "code": null
                }
            })))
            .mount(&mock)
            .await;

        let mut ctx = ctx_for(mock.uri());
        ctx.model_type = ModelType::Embeddings;

        let result = ProbeExecutor::new().execute(ctx).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.status_code, Some(400));
        assert_eq!(result.error_message.as_deref(), Some("HTTP 400 - input is required"));
    }
}
