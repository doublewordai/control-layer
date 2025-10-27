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

    /// Execute a probe against its configured endpoint.
    ///
    /// Constructs an appropriate test payload based on the model type,
    /// sends the request, and measures the response time. Returns a
    /// `ProbeExecution` regardless of success or failure to ensure
    /// all execution attempts are captured.
    pub async fn execute(&self, context: ProbeExecutionContext) -> Result<ProbeExecution> {
        let start = Instant::now();

        // Construct full URL and payload based on model type
        // Note: Don't add /v1/ here - the endpoint_url from onwards already includes it
        let (full_url, payload) = match context.model_type {
            ModelType::Chat => {
                let url = format!("{}/chat/completions", context.endpoint_url.trim_end_matches('/'));
                let payload = json!({
                    "model": context.model_name,
                    "messages": [
                        {
                            "role": "user",
                            "content": "Hello, this is a health check probe."
                        }
                    ],
                    "max_tokens": 10
                });
                (url, payload)
            }
            ModelType::Embeddings => {
                let url = format!("{}/embeddings", context.endpoint_url.trim_end_matches('/'));
                let payload = json!({
                    "model": context.model_name,
                    "input": "Health check probe"
                });
                (url, payload)
            }
        };

        // Build and send request
        let mut request = self.client.post(&full_url).json(&payload);

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
                        if (200..300).contains(&status_code) {
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
                            Ok(ProbeExecution {
                                probe_id: context.probe_id,
                                success: false,
                                response_time_ms: elapsed,
                                status_code: Some(status_code),
                                error_message: Some(format!(
                                    "HTTP {} - {}",
                                    status_code,
                                    response_data.get("error").unwrap_or(&response_data)
                                )),
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
