//! Anthropic Messages request -> OpenAI Chat Completions request (JSON).

use serde_json::{Value, json};

use super::model::{Content, ContentBlock, ImageSource, InputMessage, MessagesRequest, System, Tool, ToolResultContent};
use crate::inference::translation::TranslationError;

/// Translate an Anthropic Messages request into a Chat Completions request body. `cache_enabled`
/// gates emission of the top-level automatic-caching marker (see below).
pub fn to_chat_completions(req: MessagesRequest, cache_enabled: bool) -> Result<Value, TranslationError> {
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
    // Top-level automatic-caching marker: an internal signal the cache middleware reads (to
    // synthesize a breakpoint on the last block) AND strips before the upstream call. Emit it ONLY
    // when caching is enabled — that middleware is wired on the same `cache.enabled` flag, so with
    // caching off it wouldn't be there to strip the field, and emitting an unknown top-level field
    // would leak to OpenAI-compatible upstreams (which may reject it) for no benefit. A `null` is
    // "no marker". Explicit block-level `cache_control` is already preserved by the content-part
    // converters (and behaves the same as today when caching is off).
    if cache_enabled
        && let Some(cc) = &req.cache_control
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
///
/// A `cache_control` marker on an assistant text block is PRESERVED: the SDK advances a breakpoint
/// onto recent turns, so dropping it here (as we used to) meant the growing conversation never
/// cached. We emit the marked message as a single-text-part array carrying the marker (the same
/// shape a native OpenAI-ingress request may send; the cache layer reads it, then strips it before
/// the upstream call). A single text part is deliberate: with the marker stripped it hashes
/// identically to the plain-string form, so the prefix stays stable as the marker moves off this
/// message on the next turn — the read chain keeps matching.
fn convert_assistant(content: &Content) -> Value {
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    // The last `cache_control` seen on a text block (the breakpoint to preserve).
    let mut marker: Option<Value> = None;

    match content {
        Content::Text(t) => text.push_str(t),
        Content::Blocks(blocks) => {
            for b in blocks {
                match b {
                    ContentBlock::Text { text: t, cache_control } => {
                        text.push_str(t);
                        // An explicit `null` is "no marker" (matching parse/strip) — don't let it
                        // flip the message to array form.
                        if let Some(cc) = cache_control
                            && !cc.is_null()
                        {
                            marker = Some(cc.clone());
                        }
                    }
                    ContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        cache_control,
                    } => {
                        let mut call = json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                            }
                        });
                        // Preserve a marker on the tool call itself — the OpenAI-native mirror of
                        // Anthropic's `tool_use.cache_control`. The cache reads it as a breakpoint and
                        // strips it before upstream. Dropping it (as we used to) lost the SDK's
                        // advancing breakpoint whenever it landed on a tool_use block, breaking the
                        // read chain and leaving the growing conversation uncached. A `null` is "no
                        // marker".
                        if let Some(cc) = cache_control
                            && !cc.is_null()
                        {
                            call["cache_control"] = cc.clone();
                        }
                        tool_calls.push(call);
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
    } else if let Some(cc) = marker {
        msg.insert("content".into(), json!([{ "type": "text", "text": text, "cache_control": cc }]));
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
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        cache_control,
                        ..
                    } => {
                        // In an agent loop the newest block before a model call is usually the
                        // tool_result, so this is where the SDK's advancing breakpoint most often
                        // lands. Preserve a marker as a single-text-part array (same rationale as
                        // convert_assistant: stripped, it hashes like the plain string, so the read
                        // chain stays stable as the marker advances). Unmarked → plain string, as before.
                        let result_text = tool_result_to_text(content);
                        // An explicit `null` is "no marker" (matching parse/strip) → plain string, as before.
                        let content_val = match cache_control {
                            Some(cc) if !cc.is_null() => json!([{ "type": "text", "text": result_text, "cache_control": cc }]),
                            _ => json!(result_text),
                        };
                        tool_messages.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": content_val,
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

    // Translate with caching enabled (the prod configuration); pass `false` explicitly where the
    // cache-disabled behaviour is under test.
    fn translate(v: Value) -> Value {
        to_chat_completions(serde_json::from_value::<MessagesRequest>(v).unwrap(), true).unwrap()
    }

    #[test]
    fn top_level_cache_control_forwarded_for_automatic_caching() {
        // With caching enabled, the automatic-caching marker must survive translation onto the Chat
        // Completions body so the cache layer (which reads it) can synthesize a last-block breakpoint.
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

    #[test]
    fn top_level_cache_control_not_emitted_when_caching_disabled() {
        // With caching OFF the middleware isn't in the stack to strip it, so the field must NOT be
        // emitted — otherwise it leaks an unknown top-level field to the upstream.
        let req = serde_json::from_value::<MessagesRequest>(json!({
            "model": "m", "max_tokens": 16,
            "cache_control": {"type": "ephemeral"},
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap();
        let out = to_chat_completions(req, false).unwrap();
        assert!(out.get("cache_control").is_none(), "caching disabled → top-level marker dropped");
    }

    #[test]
    fn assistant_cache_control_preserved_as_array_content() {
        // An advancing marker on an assistant text block must survive translation (it used to be
        // flattened away), carried on a single text part so the cache layer can read it.
        let out = translate(json!({
            "model": "m", "max_tokens": 16,
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": [
                    { "type": "text", "text": "let me think", "cache_control": { "type": "ephemeral" } }
                ]}
            ]
        }));
        let asst = &out["messages"][1];
        assert_eq!(asst["role"], "assistant");
        assert_eq!(asst["content"][0]["type"], "text");
        assert_eq!(asst["content"][0]["text"], "let me think");
        assert_eq!(asst["content"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn assistant_without_marker_stays_string() {
        // No marker → byte-identical to today (plain string content), so unmarked turns are untouched.
        let out = translate(json!({
            "model": "m", "max_tokens": 16,
            "messages": [ { "role": "assistant", "content": [ { "type": "text", "text": "plain" } ] } ]
        }));
        assert_eq!(out["messages"][0]["content"], "plain");
    }

    #[test]
    fn tool_result_cache_control_preserved_as_array_content() {
        // The agent-loop case: the marker rides the tool_result (the newest block before a model
        // call). It must survive onto the translated tool message.
        let out = translate(json!({
            "model": "m", "max_tokens": 16,
            "messages": [
                { "role": "assistant", "content": [ { "type": "tool_use", "id": "tu_1", "name": "wx", "input": {} } ] },
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "tu_1", "content": "sunny",
                      "cache_control": { "type": "ephemeral", "ttl": "1h" } }
                ]}
            ]
        }));
        let tool = &out["messages"][1];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "tu_1");
        assert_eq!(tool["content"][0]["text"], "sunny");
        assert_eq!(tool["content"][0]["cache_control"]["ttl"], "1h");
    }

    #[test]
    fn explicit_null_cache_control_is_not_a_marker() {
        // `cache_control: null` is "no marker" everywhere else (parse/strip) — it must NOT flip the
        // message to array form on assistant or tool_result.
        let out = translate(json!({
            "model": "m", "max_tokens": 16,
            "messages": [
                { "role": "assistant", "content": [ { "type": "text", "text": "hi", "cache_control": null } ] },
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "tu_1", "content": "sunny", "cache_control": null }
                ]}
            ]
        }));
        assert_eq!(out["messages"][0]["content"], "hi", "null marker → assistant stays a string");
        assert_eq!(out["messages"][1]["content"], "sunny", "null marker → tool_result stays a string");
    }

    #[test]
    fn tool_result_without_marker_stays_string() {
        let out = translate(json!({
            "model": "m", "max_tokens": 16,
            "messages": [
                { "role": "assistant", "content": [ { "type": "tool_use", "id": "tu_1", "name": "wx", "input": {} } ] },
                { "role": "user", "content": [ { "type": "tool_result", "tool_use_id": "tu_1", "content": "sunny" } ] }
            ]
        }));
        assert_eq!(out["messages"][1]["content"], "sunny", "unmarked tool_result stays a plain string");
    }

    #[test]
    fn assistant_tool_use_cache_control_preserved_on_tool_call() {
        // The SDK's advancing breakpoint often lands on a tool_use block — it must survive onto the
        // OpenAI tool_call (1b: mirror of Anthropic's tool_use.cache_control), not be dropped.
        let out = translate(json!({
            "model": "m", "max_tokens": 16,
            "messages": [{ "role": "assistant", "content": [
                { "type": "text", "text": "calling" },
                { "type": "tool_use", "id": "tu_1", "name": "lookup", "input": {"q": "x"},
                  "cache_control": { "type": "ephemeral", "ttl": "1h" } }
            ]}]
        }));
        let call = &out["messages"][0]["tool_calls"][0];
        assert_eq!(call["id"], "tu_1");
        assert_eq!(call["function"]["name"], "lookup");
        assert_eq!(call["cache_control"]["ttl"], "1h", "marker preserved on the tool_call");
    }

    #[test]
    fn assistant_tool_use_without_marker_has_no_cache_control() {
        let out = translate(json!({
            "model": "m", "max_tokens": 16,
            "messages": [{ "role": "assistant", "content": [
                { "type": "tool_use", "id": "tu_1", "name": "lookup", "input": {} }
            ]}]
        }));
        assert!(
            out["messages"][0]["tool_calls"][0].get("cache_control").is_none(),
            "no marker → clean tool_call"
        );
    }
}
