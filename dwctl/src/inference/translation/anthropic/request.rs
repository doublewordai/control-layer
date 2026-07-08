//! Anthropic Messages request -> OpenAI Chat Completions request (JSON).

use serde_json::{Value, json};

use super::model::{Content, ContentBlock, ImageSource, InputMessage, MessagesRequest, System, Tool, ToolResultContent};
use crate::inference::translation::TranslationError;

/// Translate an Anthropic Messages request into a Chat Completions request body.
pub fn to_chat_completions(req: MessagesRequest) -> Result<Value, TranslationError> {
    let mut messages: Vec<Value> = Vec::new();

    if let Some(system) = &req.system
        && let Some(msg) = system_to_message(system)
    {
        messages.push(msg);
    }

    for m in &req.messages {
        convert_message(m, &mut messages)?;
    }

    let mut out = serde_json::Map::new();
    out.insert("model".into(), json!(req.model));
    out.insert("max_tokens".into(), json!(req.max_tokens));
    out.insert("messages".into(), Value::Array(messages));

    if req.stream {
        out.insert("stream".into(), json!(true));
        // Ask for a usage row on the final chunk so the response carries usage.
        out.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    if let Some(t) = req.temperature {
        out.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        out.insert("top_p".into(), json!(p));
    }
    if let Some(k) = req.top_k {
        // OpenAI has no standard `top_k`; forwarded as an additive field that
        // top_k-aware backends (vLLM / sglang) honour and others ignore. Dropping
        // it would silently change sampling for clients that set it.
        out.insert("top_k".into(), json!(k));
    }
    if let Some(s) = &req.stop_sequences {
        out.insert("stop".into(), json!(s));
    }
    if let Some(tools) = &req.tools
        && !tools.is_empty()
    {
        out.insert("tools".into(), Value::Array(tools.iter().map(tool_to_openai).collect()));
    }
    if let Some(tc) = &req.tool_choice
        && let Some(v) = tool_choice_to_openai(tc)
    {
        out.insert("tool_choice".into(), v);
    }
    if let Some(effort) = thinking_to_reasoning_effort(req.thinking.as_ref()) {
        out.insert("reasoning_effort".into(), json!(effort));
    }
    // Only forward `service_tier` to engage dwctl's flex tier. Anthropic's
    // standard values (`auto`, `standard_only`) describe priority-vs-standard,
    // which dwctl routing ignores - and `standard_only` is not a valid OpenAI
    // value, so forwarding it could 400 a downstream provider. `flex` is a
    // dwctl-specific opt-in that the inference middleware routes to handle_flex.
    if req.service_tier.as_deref() == Some("flex") {
        out.insert("service_tier".into(), json!("flex"));
    }
    // Top-level automatic-caching marker: forward it verbatim so the cache layer can synthesize a
    // breakpoint on the last block (it strips the field before the upstream call). A `null` is "no
    // marker" — don't forward it. Explicit block-level `cache_control` on system/message content is
    // already preserved by the content-part converters.
    if let Some(cc) = &req.cache_control
        && !cc.is_null()
    {
        out.insert("cache_control".into(), cc.clone());
    }

    Ok(Value::Object(out))
}

/// Anthropic `thinking` config -> OpenAI `reasoning_effort` bucket. Only enabled
/// thinking maps; absent or disabled leaves the backend at its default.
fn thinking_to_reasoning_effort(thinking: Option<&Value>) -> Option<&'static str> {
    let t = thinking?;
    if t.get("type").and_then(Value::as_str) != Some("enabled") {
        return None;
    }
    let budget = t.get("budget_tokens").and_then(Value::as_u64).unwrap_or(0);
    Some(if budget <= 2048 {
        "low"
    } else if budget <= 8192 {
        "medium"
    } else {
        "high"
    })
}

/// Anthropic top-level `system` -> a leading OpenAI system message. Text blocks
/// keep `cache_control` by emitting array-form content parts.
fn system_to_message(system: &System) -> Option<Value> {
    match system {
        System::Text(t) if !t.is_empty() => Some(json!({ "role": "system", "content": t })),
        System::Text(_) => None,
        System::Blocks(blocks) => {
            let parts: Vec<Value> = blocks.iter().filter_map(text_block_to_part).collect();
            if parts.is_empty() {
                None
            } else {
                Some(json!({ "role": "system", "content": parts }))
            }
        }
    }
}

fn convert_message(m: &InputMessage, out: &mut Vec<Value>) -> Result<(), TranslationError> {
    match m.role.as_str() {
        "assistant" => out.push(convert_assistant(&m.content)),
        "user" => convert_user(&m.content, out),
        other => return Err(TranslationError::BadRequest(format!("unsupported message role: {other}"))),
    }
    Ok(())
}

/// Assistant turn: text -> `content`, `tool_use` blocks -> `tool_calls`.
fn convert_assistant(content: &Content) -> Value {
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    match content {
        Content::Text(t) => text.push_str(t),
        Content::Blocks(blocks) => {
            for b in blocks {
                match b {
                    ContentBlock::Text { text: t, .. } => text.push_str(t),
                    ContentBlock::ToolUse { id, name, input, .. } => {
                        tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                            }
                        }));
                    }
                    // Images / tool_results are not expected on an assistant turn.
                    _ => {}
                }
            }
        }
    }

    let mut msg = serde_json::Map::new();
    msg.insert("role".into(), json!("assistant"));
    // OpenAI permits null content when tool_calls are present.
    if text.is_empty() {
        msg.insert("content".into(), Value::Null);
    } else {
        msg.insert("content".into(), json!(text));
    }
    if !tool_calls.is_empty() {
        msg.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    Value::Object(msg)
}

/// User turn: `tool_result` blocks -> `tool` messages (emitted first, so they
/// follow the prior assistant `tool_calls`); text/image -> a `user` message.
///
/// Anthropic allows `tool_result` and text/image to coexist in one message;
/// OpenAI does not, so they are split into separate messages here. This is the
/// standard OpenAI conversation shape and has been validated against the target
/// backends.
fn convert_user(content: &Content, out: &mut Vec<Value>) {
    match content {
        Content::Text(t) => out.push(json!({ "role": "user", "content": t })),
        Content::Blocks(blocks) => {
            let mut tool_messages: Vec<Value> = Vec::new();
            let mut user_parts: Vec<Value> = Vec::new();

            for b in blocks {
                match b {
                    ContentBlock::Text { .. } => {
                        if let Some(part) = text_block_to_part(b) {
                            user_parts.push(part);
                        }
                    }
                    ContentBlock::Image { source, cache_control } => {
                        let mut part = json!({ "type": "image_url", "image_url": { "url": image_source_to_url(source) } });
                        if let Some(cc) = cache_control {
                            part["cache_control"] = cc.clone();
                        }
                        user_parts.push(part);
                    }
                    ContentBlock::ToolResult { tool_use_id, content, .. } => {
                        tool_messages.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": tool_result_to_text(content),
                        }));
                    }
                    _ => {}
                }
            }

            out.append(&mut tool_messages);
            if !user_parts.is_empty() {
                out.push(json!({ "role": "user", "content": user_parts }));
            }
        }
    }
}

/// A `text` content block -> an OpenAI `text` content part, preserving any
/// `cache_control` marker. Returns `None` for non-text blocks.
fn text_block_to_part(block: &ContentBlock) -> Option<Value> {
    if let ContentBlock::Text { text, cache_control } = block {
        let mut part = json!({ "type": "text", "text": text });
        if let Some(cc) = cache_control {
            part["cache_control"] = cc.clone();
        }
        Some(part)
    } else {
        None
    }
}

fn image_source_to_url(source: &ImageSource) -> String {
    match source {
        ImageSource::Url { url } => url.clone(),
        // Inline base64 becomes a data URI; the downstream image_normalizer
        // only acts on http(s) URLs and leaves data URIs alone.
        ImageSource::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
    }
}

fn tool_result_to_text(content: &Option<ToolResultContent>) -> String {
    match content {
        Some(ToolResultContent::Text(t)) => t.clone(),
        Some(ToolResultContent::Blocks(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        None => String::new(),
    }
}

fn tool_to_openai(t: &Tool) -> Value {
    let mut tool = json!({
        "type": "function",
        "function": {
            "name": t.name,
            "description": t.description,
            "parameters": t.input_schema.clone().unwrap_or_else(|| json!({ "type": "object" })),
        }
    });
    // Carry a tool-level cache breakpoint through for the downstream classifier.
    if let Some(cc) = &t.cache_control {
        tool["cache_control"] = cc.clone();
    }
    tool
}

/// Anthropic `tool_choice` -> OpenAI `tool_choice`.
fn tool_choice_to_openai(tc: &Value) -> Option<Value> {
    match tc.get("type").and_then(|v| v.as_str()) {
        Some("auto") => Some(json!("auto")),
        Some("any") => Some(json!("required")),
        Some("tool") => tc
            .get("name")
            .and_then(|n| n.as_str())
            .map(|name| json!({ "type": "function", "function": { "name": name } })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(v: Value) -> Value {
        to_chat_completions(serde_json::from_value::<MessagesRequest>(v).unwrap()).unwrap()
    }

    #[test]
    fn top_level_cache_control_forwarded_for_automatic_caching() {
        // The automatic-caching marker must survive translation onto the Chat Completions body so the
        // cache layer (which reads that body) can synthesize a breakpoint on the last block.
        let out = translate(json!({
            "model": "m",
            "max_tokens": 16,
            "cache_control": {"type": "ephemeral", "ttl": "1h"},
            "messages": [{"role": "user", "content": "hi"}]
        }));
        assert_eq!(out["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));
    }

    #[test]
    fn top_level_cache_control_absent_or_null_not_forwarded() {
        let absent = translate(json!({
            "model": "m", "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }));
        assert!(absent.get("cache_control").is_none(), "no marker → not emitted");

        let null = translate(json!({
            "model": "m", "max_tokens": 16,
            "cache_control": null,
            "messages": [{"role": "user", "content": "hi"}]
        }));
        assert!(null.get("cache_control").is_none(), "null marker → not emitted");
    }
}
