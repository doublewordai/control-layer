//! OpenAI Chat Completions response -> Anthropic Messages response (blocking).

use axum::http::StatusCode;
use bytes::Bytes;
use serde_json::{Value, json};

use super::model::{
    ErrorDetail, ErrorEnvelopeType, ErrorResponse, ErrorType, MessageType, MessagesResponse, ResponseContentBlock, ResponseRole,
    StopReason, Usage,
};
use crate::inference::translation::TranslationError;

/// Translate a successful Chat Completions response body into an Anthropic
/// Messages response body.
pub fn from_chat_completions(body: Bytes) -> Result<Bytes, TranslationError> {
    let resp: Value =
        serde_json::from_slice(&body).map_err(|e| TranslationError::Internal(format!("parse chat completions response: {e}")))?;

    let id = resp.get("id").and_then(Value::as_str).unwrap_or("msg_unknown").to_string();
    let model = resp.get("model").and_then(Value::as_str).unwrap_or_default().to_string();
    let choice = resp.get("choices").and_then(Value::as_array).and_then(|a| a.first());
    let message = choice.and_then(|c| c.get("message"));
    let finish_reason = choice.and_then(|c| c.get("finish_reason")).and_then(Value::as_str);

    let mut content: Vec<ResponseContentBlock> = Vec::new();

    // Reasoning -> a leading `thinking` block. OpenAI-spec backends emit
    // `reasoning_content` (a plain string); prefer structured `thinking_blocks`
    // if a backend ever provides signed ones.
    if let Some(blocks) = message.and_then(|m| m.get("thinking_blocks")).and_then(Value::as_array) {
        for b in blocks {
            if let Some(t) = b.get("thinking").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                content.push(ResponseContentBlock::Thinking {
                    thinking: t.to_string(),
                    signature: b.get("signature").and_then(Value::as_str).map(str::to_owned),
                });
            }
        }
    } else if let Some(reasoning) = message.and_then(reasoning_text) {
        content.push(ResponseContentBlock::Thinking {
            thinking: reasoning,
            signature: None,
        });
    }

    if let Some(text) = message.and_then(|m| m.get("content")).and_then(Value::as_str)
        && !text.is_empty()
    {
        content.push(ResponseContentBlock::Text { text: text.to_string() });
    }

    if let Some(tool_calls) = message.and_then(|m| m.get("tool_calls")).and_then(Value::as_array) {
        for tc in tool_calls {
            let id = tc.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
            let func = tc.get("function");
            let name = func
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let args = func.and_then(|f| f.get("arguments")).and_then(Value::as_str).unwrap_or("{}");
            let input: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
            content.push(ResponseContentBlock::ToolUse { id, name, input });
        }
    }

    let (input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens) =
        resp.get("usage").map(anthropic_usage).unwrap_or((0, 0, None, None));

    // vLLM/sglang expose the matched stop sequence (a string) at
    // `choices[].stop_reason`. When present, Anthropic reports
    // `stop_reason: "stop_sequence"` plus the matched string; otherwise we map
    // the standard `finish_reason`. (Absent or a non-string token id falls back.)
    let matched_stop = choice
        .and_then(|c| c.get("stop_reason"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let (stop_reason, stop_sequence) = match matched_stop {
        Some(s) => (StopReason::StopSequence, Some(s.to_string())),
        None => (map_stop_reason(finish_reason), None),
    };

    let out = MessagesResponse {
        id,
        message_type: MessageType::Message,
        role: ResponseRole::Assistant,
        model,
        content,
        stop_reason: Some(stop_reason),
        stop_sequence,
        usage: Usage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
        },
    };

    serde_json::to_vec(&out)
        .map(Bytes::from)
        .map_err(|e| TranslationError::Internal(e.to_string()))
}

/// Reshape an upstream error body into the Anthropic error envelope, preserving
/// its message where one can be found.
pub fn error_to_anthropic(status: StatusCode, body: Bytes) -> (StatusCode, Bytes) {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let message = parsed
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .or_else(|| parsed.get("message").and_then(Value::as_str))
        .unwrap_or("request failed")
        .to_string();
    anthropic_error(status, message)
}

/// Build a fresh Anthropic error envelope from a status and message.
pub fn anthropic_error(status: StatusCode, message: String) -> (StatusCode, Bytes) {
    let out = ErrorResponse {
        envelope_type: ErrorEnvelopeType::Error,
        error: ErrorDetail {
            error_type: anthropic_error_type(status),
            message,
        },
    };
    // Serialising a typed struct cannot fail; default to empty on the off chance.
    let bytes = serde_json::to_vec(&out).map(Bytes::from).unwrap_or_default();
    (status, bytes)
}

/// Map an OpenAI `usage` object to Anthropic counts. Per Anthropic's usage
/// shape, `input_tokens` EXCLUDES cached prompt tokens, which are surfaced
/// separately as `cache_read_input_tokens`. Returns
/// `(input, output, cache_read, cache_creation)`; the cache values are `None`
/// when zero/absent so they serialise out. Shared by the blocking and streaming
/// paths.
pub(super) fn anthropic_usage(usage: &Value) -> (u64, u64, Option<u64>, Option<u64>) {
    let prompt = usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
    let output = usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0);
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let creation = usage.get("cache_creation_input_tokens").and_then(Value::as_u64).unwrap_or(0);
    (
        prompt.saturating_sub(cached),
        output,
        (cached > 0).then_some(cached),
        (creation > 0).then_some(creation),
    )
}

/// First non-empty reasoning string from the de-facto fields backends use
/// (`reasoning_content` is sglang/vLLM/DeepSeek; `reasoning` is OpenRouter).
fn reasoning_text(message: &Value) -> Option<String> {
    for key in ["reasoning_content", "reasoning"] {
        if let Some(s) = message.get(key).and_then(Value::as_str)
            && !s.is_empty()
        {
            return Some(s.to_string());
        }
    }
    None
}

/// OpenAI `finish_reason` -> Anthropic `stop_reason`.
fn map_stop_reason(finish: Option<&str>) -> StopReason {
    match finish {
        Some("length") => StopReason::MaxTokens,
        Some("tool_calls") => StopReason::ToolUse,
        // A matched stop sequence is handled by the caller; here "stop",
        // "content_filter", and unknown finish_reasons map to end_turn.
        _ => StopReason::EndTurn,
    }
}

/// HTTP status -> Anthropic error `type`.
fn anthropic_error_type(status: StatusCode) -> ErrorType {
    match status.as_u16() {
        400 => ErrorType::InvalidRequestError,
        401 => ErrorType::AuthenticationError,
        403 => ErrorType::PermissionError,
        404 => ErrorType::NotFoundError,
        413 => ErrorType::RequestTooLarge,
        429 => ErrorType::RateLimitError,
        529 => ErrorType::OverloadedError,
        _ => ErrorType::ApiError,
    }
}
