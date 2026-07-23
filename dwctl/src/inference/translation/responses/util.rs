//! Small helpers backing the Responses translator.
//!
//! `ensure_field` / `scrub_request_id_fields_from_extra` are copied from onwards'
//! `strict::schemas::utils`; `merge_reasoning_text` / `chat_usage_to_response_usage`
//! from onwards' `strict` module (both `pub(crate)` there, so not importable).
//! They are duplicated here as part of moving Responses ownership into dwctl; the
//! onwards copies retire with the rest of its Responses code (COR-536).

// `ensure_field` / `scrub_request_id_fields_from_extra` back the copied response
// normaliser, which dwctl doesn't call yet; allow dead code until it's wired.
#![allow(dead_code)]

use serde_json::{Map, Value};

use onwards::strict::schemas::chat_completions::Usage;

use super::types::{InputTokensDetails, OutputTokensDetails, ResponseUsage};

/// Insert a schema-valid placeholder only when the provider omitted a field.
/// Used by the response normaliser, not by the serde types themselves (which
/// stay strict).
pub(crate) fn ensure_field(object: &mut Map<String, Value>, key: &str, default: impl FnOnce() -> Value) {
    if !object.contains_key(key) {
        object.insert(key.to_string(), default());
    }
}

/// Merge the provider-specific reasoning fields into one string, de-duplicating
/// identical content.
pub(crate) fn merge_reasoning_text(
    reasoning: Option<&String>,
    reasoning_content: Option<&String>,
    reasoning_details: Option<&Vec<Value>>,
) -> String {
    let mut parts: Vec<&str> = Vec::new();

    if let Some(rc) = reasoning_content
        && !rc.is_empty()
    {
        parts.push(rc);
    }
    if let Some(r) = reasoning
        && !r.is_empty()
        && !parts.contains(&r.as_str())
    {
        parts.push(r);
    }
    if let Some(details) = reasoning_details {
        for detail in details {
            if let Some(text) = detail.get("text").and_then(|v| v.as_str())
                && !text.is_empty()
                && !parts.contains(&text)
            {
                parts.push(text);
            }
        }
    }

    parts.join("\n")
}

/// Convert Chat Completions `Usage` to Responses `ResponseUsage`, extracting
/// `cached_tokens` / `reasoning_tokens` from the raw detail JSON, clamped to
/// `u32::MAX`.
pub(crate) fn chat_usage_to_response_usage(u: &Usage) -> ResponseUsage {
    fn extract(details: Option<&Value>, key: &str) -> u32 {
        details
            .and_then(|v| v.get(key))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32
    }

    let cached_tokens = extract(u.prompt_tokens_details.as_ref(), "cached_tokens");
    let reasoning_tokens = extract(u.completion_tokens_details.as_ref(), "reasoning_tokens");

    ResponseUsage {
        input_tokens: u.prompt_tokens,
        output_tokens: u.completion_tokens,
        total_tokens: u.total_tokens,
        input_tokens_details: InputTokensDetails { cached_tokens },
        output_tokens_details: OutputTokensDetails { reasoning_tokens },
    }
}
