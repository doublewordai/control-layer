//! Anthropic Messages (`/v1/messages`) edge translator.

pub mod model;
pub mod request;
pub mod response;
pub mod streaming;

use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header, request::Parts};
use bytes::Bytes;

use super::{ProtocolTranslator, StreamReframer, TranslatedRequest, TranslationError};
use model::MessagesRequest;
use streaming::AnthropicStreamReframer;

/// Translator for the Anthropic Messages API.
pub struct AnthropicMessages;

impl ProtocolTranslator for AnthropicMessages {
    fn name(&self) -> &'static str {
        "anthropic_messages"
    }

    fn detect(&self, path: &str, _headers: &HeaderMap) -> bool {
        // The route is unambiguous; `/v1/messages` is owned solely by Anthropic.
        path.ends_with("/messages")
    }

    fn translate_request(&self, parts: &Parts, body: Bytes) -> Result<TranslatedRequest, TranslationError> {
        let req: MessagesRequest =
            serde_json::from_slice(&body).map_err(|e| TranslationError::BadRequest(format!("invalid Anthropic Messages request: {e}")))?;

        let chat = request::to_chat_completions(req)?;
        let new_body = serde_json::to_vec(&chat).map_err(|e| TranslationError::Internal(e.to_string()))?;

        // Normalise the path so downstream code (the non-strict upstream
        // forwarder, sanitizer, image_normalizer) treats this as chat
        // completions. The route already matched; this is not a re-route.
        let uri = normalize_path(&parts.uri)?;

        let mut headers = parts.headers.clone();
        normalize_auth(&mut headers);
        // Body size changed; drop the stale length so it is recomputed downstream.
        headers.remove(header::CONTENT_LENGTH);

        Ok(TranslatedRequest {
            uri,
            headers,
            body: Bytes::from(new_body),
        })
    }

    fn translate_response(&self, body: Bytes) -> Result<Bytes, TranslationError> {
        response::from_chat_completions(body)
    }

    fn translate_error(&self, status: StatusCode, body: Bytes) -> (StatusCode, Bytes) {
        response::error_to_anthropic(status, body)
    }

    fn error_from_message(&self, status: StatusCode, message: &str) -> (StatusCode, Bytes) {
        response::anthropic_error(status, message.to_string())
    }

    fn stream_reframer(&self) -> Box<dyn StreamReframer> {
        Box::new(AnthropicStreamReframer::new())
    }
}

/// Rewrite a `.../messages` path to `.../chat/completions`, preserving any
/// query. The route already matched; this only normalises the path for the
/// code that reads it downstream (not a re-route).
fn normalize_path(uri: &Uri) -> Result<Uri, TranslationError> {
    let path = uri.path();
    let base = path
        .strip_suffix("/messages")
        .ok_or_else(|| TranslationError::Internal(format!("path does not end with /messages: {path}")))?;
    let new_path = format!("{base}/chat/completions");
    let target = match uri.query() {
        Some(q) => format!("{new_path}?{q}"),
        None => new_path,
    };
    target
        .parse::<Uri>()
        .map_err(|e| TranslationError::Internal(format!("failed to build normalised URI: {e}")))
}

/// Accept `x-api-key` by promoting it to `Authorization: Bearer`. A real
/// `Authorization` header already present wins (we leave it untouched).
fn normalize_auth(headers: &mut HeaderMap) {
    if headers.contains_key(header::AUTHORIZATION) {
        return;
    }
    if let Some(key) = headers.get("x-api-key").and_then(|v| v.to_str().ok()).map(str::to_owned)
        && let Ok(value) = HeaderValue::from_str(&format!("Bearer {key}"))
    {
        headers.insert(header::AUTHORIZATION, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn translate(body: Value) -> Value {
        let req: MessagesRequest = serde_json::from_value(body).expect("valid request");
        request::to_chat_completions(req).expect("translates")
    }

    #[test]
    fn system_and_text_message() {
        let out = translate(json!({
            "model": "claude-x",
            "max_tokens": 100,
            "system": "be brief",
            "messages": [ { "role": "user", "content": "hello" } ]
        }));
        assert_eq!(out["model"], "claude-x");
        assert_eq!(out["max_tokens"], 100);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "be brief");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "hello");
    }

    #[test]
    fn tool_use_and_result_round_trip_shapes() {
        let out = translate(json!({
            "model": "claude-x",
            "max_tokens": 100,
            "messages": [
                { "role": "assistant", "content": [
                    { "type": "text", "text": "let me check" },
                    { "type": "tool_use", "id": "tu_1", "name": "get_weather", "input": { "city": "SF" } }
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "tu_1", "content": "sunny" }
                ]}
            ]
        }));
        let msgs = out["messages"].as_array().unwrap();
        // assistant message carries tool_calls
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"], "let me check");
        assert_eq!(msgs[0]["tool_calls"][0]["id"], "tu_1");
        assert_eq!(msgs[0]["tool_calls"][0]["function"]["name"], "get_weather");
        // tool_result becomes a tool-role message keyed by tool_call_id
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["tool_call_id"], "tu_1");
        assert_eq!(msgs[1]["content"], "sunny");
    }

    #[test]
    fn cache_control_marker_is_carried_through() {
        let out = translate(json!({
            "model": "claude-x",
            "max_tokens": 100,
            "system": [ { "type": "text", "text": "big prefix", "cache_control": { "type": "ephemeral" } } ],
            "messages": [ { "role": "user", "content": "hi" } ]
        }));
        let sys = &out["messages"][0];
        assert_eq!(sys["role"], "system");
        assert_eq!(sys["content"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn response_maps_finish_reason_and_usage() {
        let chat = json!({
            "id": "chatcmpl-1",
            "model": "claude-x",
            "choices": [ { "message": { "role": "assistant", "content": "hello there" }, "finish_reason": "stop" } ],
            "usage": { "prompt_tokens": 5, "completion_tokens": 3 }
        });
        let bytes = response::from_chat_completions(Bytes::from(serde_json::to_vec(&chat).unwrap())).unwrap();
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out["type"], "message");
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][0]["text"], "hello there");
        assert_eq!(out["stop_reason"], "end_turn");
        assert_eq!(out["usage"]["input_tokens"], 5);
        assert_eq!(out["usage"]["output_tokens"], 3);
    }

    #[test]
    fn error_envelope_shape() {
        let (status, bytes) = response::error_to_anthropic(
            StatusCode::TOO_MANY_REQUESTS,
            Bytes::from_static(b"{\"error\":{\"message\":\"slow down\"}}"),
        );
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out["type"], "error");
        assert_eq!(out["error"]["type"], "rate_limit_error");
        assert_eq!(out["error"]["message"], "slow down");
    }

    #[test]
    fn x_api_key_promoted_to_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("sk-test"));
        normalize_auth(&mut headers);
        assert_eq!(headers.get(header::AUTHORIZATION).unwrap(), "Bearer sk-test");
    }

    // --- Request / response / streaming / error coverage across both modes. ---

    #[test]
    fn multi_turn_with_parallel_tools_and_results() {
        // Assistant fires two tool calls in one turn; the next user turn returns
        // two results. Results must become tool-role messages emitted BEFORE any
        // trailing user content, keyed by the matching ids.
        let out = translate(json!({
            "model": "claude-x",
            "max_tokens": 100,
            "messages": [
                { "role": "user", "content": "weather in SF and NYC?" },
                { "role": "assistant", "content": [
                    { "type": "text", "text": "checking" },
                    { "type": "tool_use", "id": "tu_1", "name": "wx", "input": { "city": "SF" } },
                    { "type": "tool_use", "id": "tu_2", "name": "wx", "input": { "city": "NYC" } }
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "tu_1", "content": "sunny" },
                    { "type": "tool_result", "tool_use_id": "tu_2", "content": "rain" },
                    { "type": "text", "text": "thanks" }
                ]}
            ]
        }));
        let m = out["messages"].as_array().unwrap();
        assert_eq!(m[0]["role"], "user");
        assert_eq!(m[1]["role"], "assistant");
        assert_eq!(m[1]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(m[1]["tool_calls"][1]["id"], "tu_2");
        // both tool results come before the trailing user text
        assert_eq!(m[2]["role"], "tool");
        assert_eq!(m[2]["tool_call_id"], "tu_1");
        assert_eq!(m[3]["role"], "tool");
        assert_eq!(m[3]["tool_call_id"], "tu_2");
        assert_eq!(m[4]["role"], "user");
        assert_eq!(m[4]["content"][0]["text"], "thanks");
    }

    #[test]
    fn tool_result_array_content_is_flattened() {
        let out = translate(json!({
            "model": "claude-x",
            "max_tokens": 100,
            "messages": [ { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "tu_1", "is_error": true,
                  "content": [ { "type": "text", "text": "boom" } ] }
            ]}]
        }));
        assert_eq!(out["messages"][0]["role"], "tool");
        assert_eq!(out["messages"][0]["content"], "boom");
    }

    #[test]
    fn image_blocks_become_image_url() {
        let out = translate(json!({
            "model": "claude-x",
            "max_tokens": 100,
            "messages": [ { "role": "user", "content": [
                { "type": "text", "text": "what is this?" },
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" } },
                { "type": "image", "source": { "type": "url", "url": "https://x/y.png" } }
            ]}]
        }));
        let parts = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
        assert_eq!(parts[2]["image_url"]["url"], "https://x/y.png");
    }

    #[test]
    fn sampling_params_pass_through() {
        let out = translate(json!({
            "model": "claude-x", "max_tokens": 64,
            "temperature": 0.5, "top_p": 0.9, "stop_sequences": ["STOP"],
            "messages": [ { "role": "user", "content": "hi" } ]
        }));
        // temperature/top_p are f32 (matching onwards' schema), so compare with
        // tolerance rather than exact (0.9_f32 != 0.9_f64).
        assert!((out["temperature"].as_f64().unwrap() - 0.5).abs() < 1e-6);
        assert!((out["top_p"].as_f64().unwrap() - 0.9).abs() < 1e-6);
        assert_eq!(out["stop"][0], "STOP");
        assert_eq!(out["max_tokens"], 64);
    }

    #[test]
    fn tool_choice_variants_map() {
        let mk = |tc: Value| {
            translate(json!({
                "model": "m", "max_tokens": 1, "messages": [],
                "tools": [ { "name": "f", "input_schema": { "type": "object" } } ],
                "tool_choice": tc
            }))["tool_choice"]
                .clone()
        };
        assert_eq!(mk(json!({ "type": "auto" })), json!("auto"));
        assert_eq!(mk(json!({ "type": "any" })), json!("required"));
        assert_eq!(
            mk(json!({ "type": "tool", "name": "f" })),
            json!({ "type": "function", "function": { "name": "f" } })
        );
    }

    #[test]
    fn response_multiple_tool_calls_become_tool_use_blocks() {
        let chat = json!({
            "id": "c1", "model": "m",
            "choices": [ { "message": { "role": "assistant", "content": Value::Null, "tool_calls": [
                { "id": "tc1", "function": { "name": "a", "arguments": "{\"x\":1}" } },
                { "id": "tc2", "function": { "name": "b", "arguments": "{}" } }
            ]}, "finish_reason": "tool_calls" } ],
            "usage": { "prompt_tokens": 1, "completion_tokens": 2 }
        });
        let bytes = response::from_chat_completions(Bytes::from(serde_json::to_vec(&chat).unwrap())).unwrap();
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        // null content -> no text block, two tool_use blocks
        assert_eq!(out["content"].as_array().unwrap().len(), 2);
        assert_eq!(out["content"][0]["type"], "tool_use");
        assert_eq!(out["content"][0]["id"], "tc1");
        assert_eq!(out["content"][0]["input"]["x"], 1);
        assert_eq!(out["content"][1]["name"], "b");
        assert_eq!(out["stop_reason"], "tool_use");
    }

    #[test]
    fn response_finish_reason_table() {
        let reason = |fr: &str| {
            let chat = json!({ "id": "c", "model": "m", "choices": [ { "message": { "content": "x" }, "finish_reason": fr } ] });
            let bytes = response::from_chat_completions(Bytes::from(serde_json::to_vec(&chat).unwrap())).unwrap();
            serde_json::from_slice::<Value>(&bytes).unwrap()["stop_reason"].clone()
        };
        assert_eq!(reason("stop"), "end_turn");
        assert_eq!(reason("length"), "max_tokens");
        assert_eq!(reason("tool_calls"), "tool_use");
        assert_eq!(reason("content_filter"), "end_turn");
    }

    #[test]
    fn response_matched_stop_sequence() {
        // vLLM/sglang expose the matched stop at choices[].stop_reason.
        let chat = json!({
            "id": "c", "model": "m",
            "choices": [ { "message": { "role": "assistant", "content": "one two" }, "finish_reason": "stop", "stop_reason": "three" } ]
        });
        let bytes = response::from_chat_completions(Bytes::from(serde_json::to_vec(&chat).unwrap())).unwrap();
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out["stop_reason"], "stop_sequence");
        assert_eq!(out["stop_sequence"], "three");
    }

    #[test]
    fn response_usage_excludes_cached_tokens() {
        let chat = json!({
            "id": "c", "model": "m",
            "choices": [ { "message": { "content": "hi" }, "finish_reason": "stop" } ],
            "usage": { "prompt_tokens": 100, "completion_tokens": 5, "prompt_tokens_details": { "cached_tokens": 30 } }
        });
        let bytes = response::from_chat_completions(Bytes::from(serde_json::to_vec(&chat).unwrap())).unwrap();
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out["usage"]["input_tokens"], 70); // 100 - 30 cached
        assert_eq!(out["usage"]["output_tokens"], 5);
        assert_eq!(out["usage"]["cache_read_input_tokens"], 30);
    }

    #[test]
    fn response_usage_omits_cache_fields_when_uncached() {
        let chat = json!({
            "id": "c", "model": "m",
            "choices": [ { "message": { "content": "hi" }, "finish_reason": "stop" } ],
            "usage": { "prompt_tokens": 10, "completion_tokens": 2 }
        });
        let bytes = response::from_chat_completions(Bytes::from(serde_json::to_vec(&chat).unwrap())).unwrap();
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out["usage"]["input_tokens"], 10);
        assert!(out["usage"].get("cache_read_input_tokens").is_none());
        assert!(out["usage"].get("cache_creation_input_tokens").is_none());
    }

    #[test]
    fn response_no_matched_stop_falls_back_to_end_turn() {
        // No choices[].stop_reason -> standard mapping, null stop_sequence (no regression).
        let chat = json!({ "id": "c", "model": "m", "choices": [ { "message": { "content": "hi" }, "finish_reason": "stop" } ] });
        let bytes = response::from_chat_completions(Bytes::from(serde_json::to_vec(&chat).unwrap())).unwrap();
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out["stop_reason"], "end_turn");
        assert!(out["stop_sequence"].is_null());
    }

    #[test]
    fn error_status_table_maps_to_anthropic_types() {
        let t = |code: u16| {
            let (_s, b) = response::error_to_anthropic(StatusCode::from_u16(code).unwrap(), Bytes::from_static(b"{}"));
            serde_json::from_slice::<Value>(&b).unwrap()["error"]["type"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(t(400), "invalid_request_error");
        assert_eq!(t(401), "authentication_error");
        assert_eq!(t(403), "permission_error");
        assert_eq!(t(404), "not_found_error");
        assert_eq!(t(413), "request_too_large");
        assert_eq!(t(429), "rate_limit_error");
        assert_eq!(t(500), "api_error");
        assert_eq!(t(529), "overloaded_error");
    }

    #[test]
    fn cache_control_on_user_part_and_tool_is_carried() {
        let out = translate(json!({
            "model": "m", "max_tokens": 1,
            "messages": [ { "role": "user", "content": [
                { "type": "text", "text": "big", "cache_control": { "type": "ephemeral" } }
            ]}],
            "tools": [ { "name": "f", "input_schema": { "type": "object" }, "cache_control": { "type": "ephemeral", "ttl": "1h" } } ]
        }));
        assert_eq!(out["messages"][0]["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(out["tools"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(out["tools"][0]["function"]["name"], "f");
    }

    #[test]
    fn unknown_fields_and_block_types_are_ignored() {
        // Unknown top-level fields and unknown content block types (document,
        // future) must not error. (`thinking` is a handled field now - see
        // thinking_param_maps_to_reasoning_effort.)
        let out = translate(json!({
            "model": "m", "max_tokens": 1,
            "metadata": { "user_id": "u1" },
            "future_top_level": true,
            "messages": [ { "role": "user", "content": [
                { "type": "text", "text": "hi" },
                { "type": "document", "source": { "type": "base64", "media_type": "application/pdf", "data": "AAAA" } },
                { "type": "future_block", "whatever": 1 }
            ]}]
        }));
        // Only the known text part survives; unknown blocks are dropped.
        let parts = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["text"], "hi");
    }

    #[test]
    fn response_reasoning_content_becomes_leading_thinking_block() {
        let chat = json!({
            "id": "c", "model": "m",
            "choices": [ { "message": { "role": "assistant", "content": "answer", "reasoning_content": "thinking..." }, "finish_reason": "stop" } ],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
        });
        let bytes = response::from_chat_completions(Bytes::from(serde_json::to_vec(&chat).unwrap())).unwrap();
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        // Thinking comes first, then the answer text.
        assert_eq!(out["content"][0]["type"], "thinking");
        assert_eq!(out["content"][0]["thinking"], "thinking...");
        assert!(out["content"][0].get("signature").is_none());
        assert_eq!(out["content"][1]["type"], "text");
        assert_eq!(out["content"][1]["text"], "answer");
    }

    #[test]
    fn response_prefers_thinking_blocks_with_signature() {
        let chat = json!({
            "id": "c", "model": "m",
            "choices": [ { "message": {
                "role": "assistant", "content": "ans",
                "reasoning_content": "ignored when blocks present",
                "thinking_blocks": [ { "type": "thinking", "thinking": "reasoned", "signature": "sig123" } ]
            }, "finish_reason": "stop" } ]
        });
        let bytes = response::from_chat_completions(Bytes::from(serde_json::to_vec(&chat).unwrap())).unwrap();
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out["content"][0]["type"], "thinking");
        assert_eq!(out["content"][0]["thinking"], "reasoned");
        assert_eq!(out["content"][0]["signature"], "sig123");
    }

    #[test]
    fn thinking_param_maps_to_reasoning_effort() {
        let out = translate(json!({
            "model": "m", "max_tokens": 1, "messages": [ { "role": "user", "content": "hi" } ],
            "thinking": { "type": "enabled", "budget_tokens": 4096 }
        }));
        assert_eq!(out["reasoning_effort"], "medium");
        // disabled / absent thinking does not set reasoning_effort
        let out2 = translate(json!({ "model": "m", "max_tokens": 1, "messages": [] }));
        assert!(out2.get("reasoning_effort").is_none());
    }

    #[test]
    fn authorization_bearer_preserved_and_wins_over_x_api_key() {
        // Bearer alone is preserved.
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer real"));
        normalize_auth(&mut h);
        assert_eq!(h.get(header::AUTHORIZATION).unwrap(), "Bearer real");

        // When both are present, the existing Authorization wins (x-api-key ignored).
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer real"));
        h.insert("x-api-key", HeaderValue::from_static("sk-ignored"));
        normalize_auth(&mut h);
        assert_eq!(h.get(header::AUTHORIZATION).unwrap(), "Bearer real");
    }

    #[test]
    fn detect_matches_messages_ignoring_headers() {
        use crate::inference::translation::ProtocolTranslator;
        let t = AnthropicMessages;
        let mut h = HeaderMap::new();
        h.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        h.insert("anthropic-beta", HeaderValue::from_static("prompt-caching-2024-07-31"));
        // Matched regardless of version/beta headers (accepted, never rejected).
        assert!(t.detect("/v1/messages", &h));
        assert!(t.detect("/messages", &HeaderMap::new()));
        // Native chat completions is never claimed.
        assert!(!t.detect("/v1/chat/completions", &h));
    }
}
