//! Response sanitization module for OpenAI-compatible API responses
//!
//! This module provides functionality to sanitize chat completion responses from various
//! LLM providers, ensuring they conform to the strict OpenAI API schema. It handles both
//! streaming (Server-Sent Events) and non-streaming (JSON) responses.
//!
//! ## Features
//!
//! - **Field filtering**: Removes provider-specific fields (e.g., `provider`, `cost`, `logprobs`)
//! - **Model rewriting**: Replaces the upstream model name with the client's requested model
//! - **Lenient parsing**: Handles missing fields with sensible defaults
//! - **Streaming support**: Processes SSE chunks in real-time without buffering
//!
//! ## Example
//!
//! ```rust
//! use onwards::response_sanitizer::ResponseSanitizer;
//!
//! let sanitizer = ResponseSanitizer {
//!     original_model: Some("gpt-4".to_string()),
//! };
//!
//! // Sanitize a response (removes extra fields, rewrites model to "gpt-4")
//! // let sanitized = sanitizer.sanitize(path, headers, body)?;
//! ```

use axum::body::Bytes;
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Lenient wrapper for OpenAI chat completion responses.
///
/// Deserializes with defaults for missing fields and ignores extra provider-specific fields.
/// Uses `#[serde(flatten)]` to capture unknown fields and `skip_serializing` to drop them.
#[derive(Debug, Deserialize, Serialize)]
struct LenientChatCompletion {
    #[serde(default = "default_id")]
    id: String,
    #[serde(default = "default_object")]
    object: String,
    #[serde(default = "default_created")]
    created: u32,
    model: String,
    choices: Vec<LenientChoice>,
    #[serde(default)]
    usage: Option<LenientUsage>,
    #[serde(default)]
    system_fingerprint: Option<String>,
    #[serde(default)]
    service_tier: Option<String>,
    /// Catch-all for unknown fields (ignored during serialization)
    #[serde(flatten, skip_serializing)]
    _extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LenientChoice {
    #[serde(default)]
    index: u64,
    message: serde_json::Value,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    logprobs: Option<serde_json::Value>,
    /// vLLM/sglang report the matched stop sequence (a string, or a token id)
    /// here. Preserved through sanitization so downstream readers (e.g. the
    /// Anthropic `stop_sequence` translation) can see it; omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stop_reason: Option<serde_json::Value>,
    /// Catch-all for unknown fields (ignored during serialization)
    #[serde(flatten, skip_serializing)]
    _extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LenientUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
    /// Catch-all for unknown fields (ignored during serialization)
    #[serde(flatten, skip_serializing)]
    _extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    audio_tokens: u64,
    /// Catch-all for unknown fields (ignored during serialization)
    #[serde(flatten, skip_serializing)]
    _extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: u64,
    #[serde(default)]
    audio_tokens: u64,
    #[serde(default)]
    accepted_prediction_tokens: u64,
    #[serde(default)]
    rejected_prediction_tokens: u64,
    /// Catch-all for unknown fields (ignored during serialization)
    #[serde(flatten, skip_serializing)]
    _extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LenientStreamChunk {
    #[serde(default = "default_id")]
    id: String,
    #[serde(default = "default_stream_object")]
    object: String,
    #[serde(default = "default_created")]
    created: u32,
    model: String,
    choices: Vec<LenientStreamChoice>,
    #[serde(default)]
    usage: Option<LenientUsage>,
    #[serde(default)]
    system_fingerprint: Option<String>,
    #[serde(default)]
    service_tier: Option<String>,
    /// Catch-all for unknown fields (ignored during serialization)
    #[serde(flatten, skip_serializing)]
    _extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LenientStreamChoice {
    #[serde(default)]
    index: u64,
    #[serde(default)]
    delta: Option<serde_json::Value>,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    logprobs: Option<serde_json::Value>,
    /// Catch-all for unknown fields (ignored during serialization)
    #[serde(flatten, skip_serializing)]
    _extra: HashMap<String, serde_json::Value>,
}

fn default_id() -> String {
    "chatcmpl-unknown".to_string()
}

fn default_object() -> String {
    "chat.completion".to_string()
}

fn default_stream_object() -> String {
    "chat.completion.chunk".to_string()
}

fn default_created() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32
}

/// Response sanitizer for OpenAI chat completion responses
///
/// Enforces strict OpenAI API schema compliance by:
/// - Removing provider-specific fields
/// - Rewriting the model field to match the client's original request
/// - Supporting both streaming (SSE) and non-streaming responses
pub struct ResponseSanitizer {
    /// The model name originally requested by the client
    pub original_model: Option<String>,
}

impl ResponseSanitizer {
    /// Sanitizes an OpenAI API response
    ///
    /// # Arguments
    ///
    /// * `path` - The request path (e.g., "/v1/chat/completions")
    /// * `headers` - HTTP response headers from the upstream provider
    /// * `body` - Raw response body bytes
    ///
    /// # Returns
    ///
    /// * `Ok(Some(Bytes))` - Sanitized response body
    /// * `Ok(None)` - Path not applicable for sanitization
    /// * `Err(String)` - Sanitization failed with error message
    pub fn sanitize(
        &self,
        path: &str,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<Option<Bytes>, String> {
        // Only apply to chat completions endpoint (with or without /v1 prefix)
        if !path.contains("/chat/completions") {
            return Ok(None);
        }

        // Detect streaming via Content-Type header
        let is_streaming = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("text/event-stream"))
            .unwrap_or(false);

        if is_streaming {
            self.sanitize_streaming(body)
        } else {
            self.sanitize_non_streaming(body)
        }
    }

    /// Sanitizes a non-streaming JSON response
    fn sanitize_non_streaming(&self, body: &[u8]) -> Result<Option<Bytes>, String> {
        // Deserialize using lenient types that ignore unknown fields and provide defaults
        let mut completion: LenientChatCompletion = serde_json::from_slice(body)
            .map_err(|e| format!("Failed to parse response as JSON: {}", e))?;

        // Rewrite model field if original model provided
        if let Some(ref original) = self.original_model {
            completion.model = original.clone();
        }

        // Serialize back to clean JSON (unknown fields are automatically dropped)
        let sanitized_bytes = serde_json::to_vec(&completion)
            .map_err(|e| format!("Failed to serialize sanitized response: {}", e))?;

        Ok(Some(Bytes::from(sanitized_bytes)))
    }

    /// Sanitizes a streaming Server-Sent Events (SSE) response
    ///
    /// SSE format: `data: {...}\n\ndata: {...}\n\ndata: [DONE]\n\n`
    ///
    /// Embedded provider error envelopes — either bare `{"error":{...}}`
    /// chunks or normal chunks that also carry an `error` field (OpenRouter
    /// shape) — are surfaced as stand-alone `data: {"error":{...}}` events.
    /// Otherwise the lenient deserializer would absorb the `error` field into
    /// its unknown-fields map and drop it on re-serialize, leaving the caller
    /// with a content-less stream and no signal. Non-strict mode forwards the
    /// envelope verbatim and does not mask the status code — masking is strict
    /// mode's job.
    pub fn sanitize_streaming(&self, body: &[u8]) -> Result<Option<Bytes>, String> {
        let body_str = std::str::from_utf8(body)
            .map_err(|e| format!("Invalid UTF-8 in streaming response: {}", e))?;

        let mut sanitized_lines = Vec::new();

        for line in body_str.lines() {
            if let Some(data_part) = line.strip_prefix("data: ") {
                // Skip "data: " prefix

                if data_part.trim() == "[DONE]" {
                    // Preserve [DONE] marker as-is
                    sanitized_lines.push(line.to_string());
                } else {
                    // Parse once: an embedded error envelope and a normal
                    // chunk are both valid JSON, so reuse this value for the
                    // error check and the lenient coercion below.
                    let value: serde_json::Value = serde_json::from_str(data_part)
                        .map_err(|e| format!("Failed to parse stream chunk: {}", e))?;

                    if let Some(error_event) = extract_embedded_error_envelope(&value) {
                        // Surface embedded provider errors as a stand-alone
                        // event so a downstream reassembler can detect them and
                        // reclassify the HTTP status. Non-strict mode forwards
                        // the envelope verbatim and preserves the upstream code.
                        // ZDR: log only the length, never the SSE chunk content.
                        tracing::warn!(
                            data_len = data_part.len(),
                            "Provider returned error envelope in SSE stream (non-strict: forwarding verbatim)"
                        );
                        sanitized_lines.push(error_event);
                    } else {
                        // Deserialize using lenient types that ignore unknown fields and provide defaults
                        let mut chunk: LenientStreamChunk = serde_json::from_value(value)
                            .map_err(|e| format!("Failed to parse stream chunk: {}", e))?;

                        // Rewrite model field if original model provided
                        if let Some(ref original) = self.original_model {
                            chunk.model = original.clone();
                        }

                        // Serialize the sanitized chunk (unknown fields are automatically dropped)
                        let sanitized_json = serde_json::to_string(&chunk)
                            .map_err(|e| format!("Failed to serialize stream chunk: {}", e))?;

                        sanitized_lines.push(format!("data: {}", sanitized_json));
                    }
                }
            } else if line.is_empty() {
                // Preserve empty lines (SSE delimiter)
                sanitized_lines.push(String::new());
            }
            // Ignore other lines (comments, etc.)
        }

        // Join with \n and ensure the output preserves trailing newlines from input
        // SSE format requires messages to end with \n\n
        let mut sanitized_body = sanitized_lines.join("\n");

        // .lines() strips trailing newlines, so we need to restore them
        // Count trailing newlines in both input and current output
        let input_trailing = body_str.chars().rev().take_while(|&c| c == '\n').count();
        let output_trailing = sanitized_body
            .chars()
            .rev()
            .take_while(|&c| c == '\n')
            .count();

        // Add the difference to match input's trailing newlines
        for _ in output_trailing..input_trailing {
            sanitized_body.push('\n');
        }

        Ok(Some(Bytes::from(sanitized_body)))
    }
}

/// If `value` carries a provider `error` object — either as the entire
/// payload or alongside completion fields (OpenRouter shape) — return a
/// stand-alone `data: {"error":{...}}` line. All surrounding chunk fields
/// (`id`, `object`, `choices`, …) are discarded; only the `error` object
/// itself is emitted.
///
/// The chunk wrapper is stripped so the emitted SSE data line begins with
/// `{"error"`, the prefix downstream reassemblers match on to reclassify the
/// HTTP status from the embedded `code`.
///
/// WARNING: the `error` object is forwarded **verbatim** — the provider's
/// `message` and any extra fields (`metadata`, `raw`, …) reach the client
/// unchanged, and the status `code` is not masked. Non-strict mode performs
/// no sanitization here; that is strict mode's job. Do not enable
/// `sanitize_response` on targets proxying untrusted third parties if you need
/// to hide upstream internals from clients.
///
/// `error: null` and empty `error: {}` payloads (which some providers emit on
/// healthy chunks) are ignored so they don't become spurious error events.
fn extract_embedded_error_envelope(value: &serde_json::Value) -> Option<String> {
    let error_obj = value
        .get("error")
        .filter(|v| v.is_object() && (v.get("message").is_some() || v.get("code").is_some()))?;
    let envelope = serde_json::json!({ "error": error_obj });
    Some(format!("data: {envelope}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    #[test]
    fn test_non_streaming_sanitization() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("gpt-4".to_string()),
        };

        let response_json = r#"{
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "created": 1677652288,
            "model": "gpt-4-turbo",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 9,
                "completion_tokens": 2,
                "total_tokens": 11
            }
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/chat/completions", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(sanitized["model"], "gpt-4");
    }

    #[test]
    fn test_matched_stop_reason_is_preserved() {
        // vLLM/sglang put the matched stop sequence at choices[].stop_reason;
        // sanitization must keep it so downstream translation can read it.
        let sanitizer = ResponseSanitizer {
            original_model: Some("m".to_string()),
        };
        let response_json = r#"{
            "id": "c", "object": "chat.completion", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "one two" },
                "finish_reason": "stop",
                "stop_reason": "three"
            }]
        }"#;
        let result = sanitizer
            .sanitize("/v1/chat/completions", &HeaderMap::new(), response_json.as_bytes())
            .unwrap()
            .unwrap();
        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(sanitized["choices"][0]["stop_reason"], "three");

        // A response without it does not gain a spurious null field.
        let plain = r#"{
            "id": "c", "object": "chat.completion", "created": 1, "model": "m",
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": "hi" }, "finish_reason": "stop" }]
        }"#;
        let result = sanitizer
            .sanitize("/v1/chat/completions", &HeaderMap::new(), plain.as_bytes())
            .unwrap()
            .unwrap();
        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert!(sanitized["choices"][0].get("stop_reason").is_none());
    }

    #[test]
    fn test_streaming_sanitization() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("gpt-4".to_string()),
        };

        let sse_response = "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1677652288,\"model\":\"gpt-4-turbo\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n";

        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );

        let result = sanitizer
            .sanitize("/v1/chat/completions", &headers, sse_response.as_bytes())
            .unwrap()
            .unwrap();

        let result_str = std::str::from_utf8(&result).unwrap();
        assert!(result_str.contains("\"model\":\"gpt-4\""));
        assert!(result_str.contains("data: [DONE]"));

        // Verify SSE format: should end with \n\n for proper message termination
        assert!(
            result_str.ends_with("\n\n"),
            "SSE response should end with \\n\\n, got: {:?}",
            &result_str[result_str.len().saturating_sub(10)..]
        );

        // Verify messages are separated by \n\n
        assert!(
            result_str.contains("}\n\ndata:"),
            "SSE messages should be separated by \\n\\n"
        );

        // Verify exactly 2 trailing newlines (not 3 or more)
        let trailing_count = result_str.chars().rev().take_while(|&c| c == '\n').count();
        assert_eq!(
            trailing_count, 2,
            "Should have exactly 2 trailing newlines, got {}",
            trailing_count
        );
    }

    #[test]
    fn test_streaming_multiple_chunks() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("gpt-4".to_string()),
        };

        // Multiple chunks with proper SSE format
        let sse_response = "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1677652288,\"model\":\"gpt-4-turbo\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1677652289,\"model\":\"gpt-4-turbo\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" World\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n";

        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );

        let result = sanitizer
            .sanitize("/v1/chat/completions", &headers, sse_response.as_bytes())
            .unwrap()
            .unwrap();

        let result_str = std::str::from_utf8(&result).unwrap();

        // All chunks should have model rewritten
        let chunk_count = result_str.matches("\"model\":\"gpt-4\"").count();
        assert_eq!(chunk_count, 2, "Both chunks should have model rewritten");

        // Should end with \n\n
        assert!(result_str.ends_with("\n\n"));

        // Should have three messages (2 chunks + [DONE])
        let message_count = result_str.matches("data:").count();
        assert_eq!(message_count, 3);
    }

    #[test]
    fn test_path_filtering() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        let headers = HeaderMap::new();
        let result = sanitizer.sanitize("/v1/models", &headers, b"test").unwrap();

        assert!(result.is_none());
    }

    #[test]
    fn test_no_model_rewrite() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };

        let response_json = r#"{
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "created": 1677652288,
            "model": "gpt-4-turbo",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 9,
                "completion_tokens": 2,
                "total_tokens": 11
            }
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize("/v1/chat/completions", &headers, response_json.as_bytes())
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(sanitized["model"], "gpt-4-turbo");
    }

    #[test]
    fn test_openrouter_response_sanitization() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("fred".to_string()),
        };

        // Actual OpenRouter response with extra fields
        let openrouter_response = r#"{
            "id":"gen-1768414327-n9nnQrvnhHU8HF0oRVKq",
            "provider":"DeepInfra",
            "model":"google/gemma-3-12b-it",
            "object":"chat.completion",
            "created":1768414327,
            "choices":[{
                "logprobs":null,
                "finish_reason":"stop",
                "native_finish_reason":"stop",
                "index":0,
                "message":{
                    "role":"assistant",
                    "content":"Hello! I'm doing well, thank you for asking! As an AI, I don't experience feelings like humans do, but I'm operating smoothly and ready to assist you. 😊 \n\nHow are *you* doing today?",
                    "refusal":null,
                    "reasoning":null
                }
            }],
            "usage":{
                "prompt_tokens":15,
                "completion_tokens":51,
                "total_tokens":66,
                "cost":0.00000723,
                "is_byok":false,
                "prompt_tokens_details":{
                    "cached_tokens":0,
                    "audio_tokens":0,
                    "video_tokens":0
                },
                "cost_details":{
                    "upstream_inference_cost":null,
                    "upstream_inference_prompt_cost":6e-7,
                    "upstream_inference_completions_cost":0.00000663
                },
                "completion_tokens_details":{
                    "reasoning_tokens":0,
                    "image_tokens":0
                }
            }
        }"#;

        let headers = HeaderMap::new();
        let result = sanitizer
            .sanitize(
                "/v1/chat/completions",
                &headers,
                openrouter_response.as_bytes(),
            )
            .unwrap()
            .unwrap();

        let sanitized: serde_json::Value = serde_json::from_slice(&result).unwrap();

        // Verify model rewrite
        assert_eq!(sanitized["model"], "fred");

        // Verify OpenAI fields are preserved
        assert_eq!(sanitized["id"], "gen-1768414327-n9nnQrvnhHU8HF0oRVKq");
        assert_eq!(sanitized["object"], "chat.completion");
        assert_eq!(sanitized["created"], 1768414327);

        // Verify choices structure
        let choices = sanitized["choices"].as_array().unwrap();
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0]["index"], 0);
        assert_eq!(choices[0]["finish_reason"], "stop");
        assert_eq!(choices[0]["message"]["role"], "assistant");
        assert!(
            choices[0]["message"]["content"]
                .as_str()
                .unwrap()
                .starts_with("Hello!")
        );

        // Verify provider-specific fields are removed
        assert!(sanitized.get("provider").is_none());
        assert!(choices[0].get("native_finish_reason").is_none());

        // Verify logprobs is preserved (it's a valid OpenAI field, even when null)
        assert!(choices[0].get("logprobs").is_some());
        assert_eq!(choices[0]["logprobs"], serde_json::Value::Null);

        // Verify usage is preserved (but sanitized)
        assert!(sanitized.get("usage").is_some());
        let usage = &sanitized["usage"];
        assert_eq!(usage["prompt_tokens"], 15);
        assert_eq!(usage["completion_tokens"], 51);
        assert_eq!(usage["total_tokens"], 66);

        // Verify OpenRouter-specific usage fields are removed
        assert!(usage.get("cost").is_none());
        assert!(usage.get("is_byok").is_none());
        assert!(usage.get("cost_details").is_none());

        // Verify OpenAI token detail fields are preserved
        assert!(usage.get("prompt_tokens_details").is_some());
        assert!(usage.get("completion_tokens_details").is_some());

        // Verify the nested details contain valid OpenAI fields (invalid ones like video_tokens, image_tokens are removed)
        let prompt_details = &usage["prompt_tokens_details"];
        assert_eq!(prompt_details["cached_tokens"], 0);
        assert_eq!(prompt_details["audio_tokens"], 0);
        assert!(prompt_details.get("video_tokens").is_none()); // OpenRouter-specific, should be removed

        let completion_details = &usage["completion_tokens_details"];
        assert_eq!(completion_details["reasoning_tokens"], 0);
        assert!(completion_details.get("image_tokens").is_none()); // OpenRouter-specific, should be removed
    }

    /// Non-strict mode must surface embedded provider errors (OpenRouter
    /// shape: normal chunk fields alongside an `error` object) as a
    /// stand-alone event so a downstream reassembler can detect them. The
    /// error is forwarded verbatim — non-strict does not mask the code or
    /// rewrite the message.
    #[test]
    fn test_streaming_sanitizer_surfaces_embedded_error_verbatim() {
        let sanitizer = ResponseSanitizer {
            original_model: Some("gpt-4".to_string()),
        };
        let body = b"data: {\"id\":\"gen-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"openai/gpt-oss-20b\",\"choices\":[],\"error\":{\"message\":\"Provider rate limited\",\"code\":429}}\n\n";
        let result = sanitizer.sanitize_streaming(body).unwrap().unwrap();
        let out = std::str::from_utf8(&result).unwrap();

        // Stand-alone error event surfaces with the `{"error"` prefix.
        assert!(
            out.contains("data: {\"error\""),
            "expected stand-alone error event, got: {out}"
        );
        // Code preserved and NOT masked — non-strict keeps the upstream status.
        assert!(
            out.contains("\"code\":429"),
            "expected code 429 preserved, got: {out}"
        );
        // Verbatim: the provider's message passes through in non-strict mode.
        assert!(
            out.contains("Provider rate limited"),
            "expected verbatim provider message, got: {out}"
        );
    }

    /// An empty `error: {}` object carries no signal and must not be turned
    /// into a spurious error event that would derail a valid stream.
    #[test]
    fn test_streaming_sanitizer_ignores_empty_error_object() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };
        let body =
            b"data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"m\",\"choices\":[],\"error\":{}}\n\n";
        let result = sanitizer.sanitize_streaming(body).unwrap().unwrap();
        let out = std::str::from_utf8(&result).unwrap();
        assert!(
            !out.contains("data: {\"error\""),
            "empty error object must not surface as an error event, got: {out}"
        );
    }

    /// Behavioral delta from strict mode: non-strict does NOT mask
    /// account-class codes. An embedded `402` must surface as `402`, whereas
    /// strict mode's `try_format_sse_error` would mask it to `502` for an
    /// untrusted provider.
    #[test]
    fn test_streaming_sanitizer_does_not_mask_account_class_code() {
        let sanitizer = ResponseSanitizer {
            original_model: None,
        };
        let body = b"data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"m\",\"choices\":[],\"error\":{\"message\":\"insufficient credits\",\"code\":402}}\n\n";
        let result = sanitizer.sanitize_streaming(body).unwrap().unwrap();
        let out = std::str::from_utf8(&result).unwrap();

        assert!(
            out.contains("data: {\"error\""),
            "expected stand-alone error event, got: {out}"
        );
        // 402 preserved, NOT masked to 502 (non-strict forwards verbatim).
        assert!(
            out.contains("\"code\":402"),
            "non-strict must preserve the original 402, got: {out}"
        );
        assert!(
            !out.contains("\"code\":502"),
            "non-strict must not mask the code to 502, got: {out}"
        );
        // Verbatim message preserved too.
        assert!(out.contains("insufficient credits"));
    }
}
