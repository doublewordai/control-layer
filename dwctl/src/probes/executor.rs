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
                                .or_else(|| response_data.get("error"))
                                .and_then(|e| e.as_str())
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
