//! Surface-aware extraction of a [`RequestView`] from a parsed request body.
//!
//!
//! The extractor never fails and never panics: it walks the wire JSON with
//! type-checked accessors and ignores any shape it does not recognise. That is
//! deliberate — an unknown field or a forward-compatible content block must not
//! turn into a rejection. Only the fields the rules consume are read.
//!
//! `prompt_text_bytes` is an UPPER BOUND on the prompt token count, never a
//! lower one: byte-level BPE emits at most one token per byte, so the UTF-8 byte
//! length of the text the engine tokenizes is >= its token count. Token-id
//! arrays are counted as one byte per id. Anything the extractor does not
//! recognise is left out, which can only make the bound LOWER, so unknown
//! shapes must be handled by the caller's fail-open rules rather than assumed
//! counted. Known-but-non-text inputs (images, audio, files) are recorded via
//! the modality flags and not added to the byte count.

use serde_json::Value;

use super::Surface;

/// The fields rules need, extracted once from the wire-shaped body.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestView {
    pub surface: Option<Surface>,
    pub model: Option<String>,
    /// Raw `service_tier` value, if present.
    pub service_tier: Option<serde_json::Value>,
    /// Requested completion budget: `max_completion_tokens` / `max_tokens`
    /// (chat, completions, messages) or `max_output_tokens` (responses). Kept
    /// raw so non-integer values can be reported.
    pub max_output_tokens: Option<serde_json::Value>,
    /// Name of the field `max_output_tokens` came from (for `param`).
    pub max_output_tokens_param: Option<&'static str>,
    pub has_image_input: bool,
    pub has_audio_input: bool,
    /// Files and documents (chat `file`, Responses `input_file`, Anthropic
    /// `document`). Tracked apart from images: no capability gates them yet.
    pub has_file_input: bool,
    /// UTF-8 bytes of all prompt text the engine will tokenize (messages,
    /// system, instructions, tool definitions, embeddings input strings).
    pub prompt_text_bytes: usize,
}

/// Build the fields the rules need from a parsed request body. `surface` is
/// always recorded; every other field is `None`/zero when absent or malformed.
pub fn extract(surface: Surface, body: &Value) -> RequestView {
    let mut view = RequestView {
        surface: Some(surface),
        // `model` is the bare alias string; non-string values are ignored.
        model: body.get("model").and_then(Value::as_str).map(str::to_owned),
        // Raw, so a rule can report a non-string tier rather than silently drop it.
        service_tier: body.get("service_tier").cloned(),
        ..RequestView::default()
    };

    match surface {
        Surface::ChatCompletions => extract_chat(&mut view, body),
        Surface::Completions => extract_completions(&mut view, body),
        Surface::Responses => extract_responses(&mut view, body),
        Surface::Messages => extract_messages(&mut view, body),
        Surface::Embeddings => extract_embeddings(&mut view, body),
    }

    view
}

// ---------------------------------------------------------------------------
// Shared helpers.
// ---------------------------------------------------------------------------

/// Add the UTF-8 byte length of `text` to the prompt bound.
fn add_text(view: &mut RequestView, text: &str) {
    view.prompt_text_bytes = view.prompt_text_bytes.saturating_add(text.len());
}

/// Add the compact JSON serialization of `value` to the prompt bound. Used for
/// tool definitions and tool-call arguments the engine tokenizes as JSON.
fn add_json(view: &mut RequestView, value: &Value) {
    if let Ok(serialized) = serde_json::to_string(value) {
        add_text(view, &serialized);
    }
}

/// Bytes of the largest independent sequence in a Completions `prompt` or an
/// Embeddings `input`. The engine processes each element of a list of strings
/// (or of token-id lists) as its own sequence against the context window, so
/// the bound is the largest one, not the sum. A flat list of token ids is a
/// single sequence; each id counts as one byte (it is exactly one token).
fn largest_sequence_bytes(value: &Value) -> usize {
    match value {
        Value::String(text) => text.len(),
        Value::Array(items) if items.iter().all(Value::is_number) => items.len(),
        Value::Array(items) => items.iter().map(largest_sequence_bytes).max().unwrap_or(0),
        // null / bool / object are not prompt text in any surface we know.
        _ => 0,
    }
}

/// Record `max_output_tokens` and the field it came from. A non-null
/// `preferred` wins over `fallback` (chat: `max_completion_tokens` over
/// `max_tokens`); a null `preferred` is treated as absent.
fn set_max_output_tokens(view: &mut RequestView, body: &Value, preferred: &'static str, fallback: Option<&'static str>) {
    if let Some(value) = body.get(preferred).filter(|value| !value.is_null()) {
        view.max_output_tokens = Some(value.clone());
        view.max_output_tokens_param = Some(preferred);
    } else if let Some(fallback) = fallback
        && let Some(value) = body.get(fallback)
    {
        view.max_output_tokens = Some(value.clone());
        view.max_output_tokens_param = Some(fallback);
    }
}

/// Add every serialized tool definition to the bound.
fn count_tools(view: &mut RequestView, tools: Option<&Value>) {
    let Some(Value::Array(tools)) = tools else {
        return;
    };
    for tool in tools {
        add_json(view, tool);
    }
}

/// Count an OpenAI `function` object (assistant tool call): the function name
/// and its argument string are both tokenized.
fn count_function_call(view: &mut RequestView, function: Option<&Value>) {
    let Some(function) = function else {
        return;
    };
    if let Some(name) = function.get("name").and_then(Value::as_str) {
        add_text(view, name);
    }
    match function.get("arguments") {
        Some(Value::String(arguments)) => add_text(view, arguments),
        Some(other) => add_json(view, other),
        None => {}
    }
}

// ---------------------------------------------------------------------------
// Chat Completions.
// ---------------------------------------------------------------------------

fn extract_chat(view: &mut RequestView, body: &Value) {
    // `max_completion_tokens` is the current field and takes precedence when
    // both are present.
    set_max_output_tokens(view, body, "max_completion_tokens", Some("max_tokens"));

    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for message in messages {
            if let Some(content) = message.get("content") {
                walk_chat_content(view, content);
            }
            if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
                for call in tool_calls {
                    count_function_call(view, call.get("function"));
                }
            }
        }
    }

    count_tools(view, body.get("tools"));
}

/// Chat `content`: a string or an array of typed parts. Text is counted; the
/// multimodal parts set a flag and are deliberately not counted as bytes.
fn walk_chat_content(view: &mut RequestView, content: &Value) {
    match content {
        Value::String(text) => add_text(view, text),
        Value::Array(parts) => {
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            add_text(view, text);
                        }
                    }
                    Some("image_url") => view.has_image_input = true,
                    Some("input_audio") => view.has_audio_input = true,
                    Some("file") => view.has_file_input = true,
                    // Unknown part type: ignored (never a reason to reject).
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Completions.
// ---------------------------------------------------------------------------

fn extract_completions(view: &mut RequestView, body: &Value) {
    set_max_output_tokens(view, body, "max_tokens", None);
    if let Some(prompt) = body.get("prompt") {
        view.prompt_text_bytes = largest_sequence_bytes(prompt);
    }
}

// ---------------------------------------------------------------------------
// Responses.
// ---------------------------------------------------------------------------

fn extract_responses(view: &mut RequestView, body: &Value) {
    set_max_output_tokens(view, body, "max_output_tokens", None);

    if let Some(instructions) = body.get("instructions").and_then(Value::as_str) {
        add_text(view, instructions);
    }

    match body.get("input") {
        Some(Value::String(text)) => add_text(view, text),
        Some(Value::Array(items)) => {
            for item in items {
                walk_response_item(view, item);
            }
        }
        _ => {}
    }

    // Only function tools reach the engine; the translator drops hosted tools
    // (`code_interpreter`, `web_search`, ...), so they are not prompt text.
    if let Some(Value::Array(tools)) = body.get("tools") {
        for tool in tools
            .iter()
            .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("function"))
        {
            add_json(view, tool);
        }
    }
}

/// Responses input items. A missing `type` defaults to `message`, matching the
/// schema dwctl's own translator accepts.
fn walk_response_item(view: &mut RequestView, item: &Value) {
    match item.get("type").and_then(Value::as_str).unwrap_or("message") {
        "message" => {
            if let Some(content) = item.get("content") {
                walk_response_content(view, content);
            }
        }
        "function_call" => {
            if let Some(name) = item.get("name").and_then(Value::as_str) {
                add_text(view, name);
            }
            match item.get("arguments") {
                Some(Value::String(arguments)) => add_text(view, arguments),
                Some(other) => add_json(view, other),
                None => {}
            }
        }
        "function_call_output" => match item.get("output") {
            Some(Value::String(output)) => add_text(view, output),
            Some(other) => add_json(view, other),
            None => {}
        },
        // Plaintext reasoning is replayed to the engine as context, so count it
        // (encrypted reasoning is opaque and is left to fail-open).
        "reasoning" => {
            if let Some(parts) = item.get("content").and_then(Value::as_array) {
                for part in parts {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        add_text(view, text);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Responses message content: a string or an array of typed parts.
fn walk_response_content(view: &mut RequestView, content: &Value) {
    match content {
        Value::String(text) => add_text(view, text),
        Value::Array(parts) => {
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("input_text") | Some("output_text") => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            add_text(view, text);
                        }
                    }
                    Some("refusal") => {
                        if let Some(text) = part.get("refusal").and_then(Value::as_str) {
                            add_text(view, text);
                        }
                    }
                    // The Responses translator only forwards images given by URL.
                    Some("input_image") if part.get("image_url").is_some_and(|url| !url.is_null()) => view.has_image_input = true,
                    Some("input_file") => view.has_file_input = true,
                    Some("input_audio") => view.has_audio_input = true,
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Anthropic Messages.
// ---------------------------------------------------------------------------

fn extract_messages(view: &mut RequestView, body: &Value) {
    set_max_output_tokens(view, body, "max_tokens", None);

    if let Some(system) = body.get("system") {
        walk_anthropic_content(view, system);
    }

    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for message in messages {
            if let Some(content) = message.get("content") {
                walk_anthropic_content(view, content);
            }
        }
    }

    count_tools(view, body.get("tools"));
}

/// Anthropic content: a string or an array of typed content blocks.
fn walk_anthropic_content(view: &mut RequestView, content: &Value) {
    match content {
        Value::String(text) => add_text(view, text),
        Value::Array(blocks) => {
            for block in blocks {
                walk_anthropic_block(view, block);
            }
        }
        _ => {}
    }
}

fn walk_anthropic_block(view: &mut RequestView, block: &Value) {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                add_text(view, text);
            }
        }
        Some("image") => view.has_image_input = true,
        Some("document") => view.has_file_input = true,
        Some("tool_use") => {
            if let Some(name) = block.get("name").and_then(Value::as_str) {
                add_text(view, name);
            }
            if let Some(input) = block.get("input") {
                add_json(view, input);
            }
        }
        Some("tool_result") => {
            if let Some(content) = block.get("content") {
                walk_tool_result_content(view, content);
            }
        }
        // `thinking` is intentionally dropped by the translator; unknown blocks
        // are ignored.
        _ => {}
    }
}

/// `tool_result` content: a string or an array of blocks. Each block's `text`
/// is counted and image/document blocks set the modality flag.
fn walk_tool_result_content(view: &mut RequestView, content: &Value) {
    match content {
        Value::String(text) => add_text(view, text),
        Value::Array(blocks) => {
            for block in blocks {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    add_text(view, text);
                }
                match block.get("type").and_then(Value::as_str) {
                    Some("image") => view.has_image_input = true,
                    Some("document") => view.has_file_input = true,
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Embeddings.
// ---------------------------------------------------------------------------

fn extract_embeddings(view: &mut RequestView, body: &Value) {
    if let Some(input) = body.get("input") {
        view.prompt_text_bytes = largest_sequence_bytes(input);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chat_string_content_and_metadata() {
        let body = json!({
            "model": "gpt-4o",
            "service_tier": "flex",
            "max_completion_tokens": 128,
            "messages": [
                { "role": "system", "content": "be terse" },
                { "role": "user", "content": "hello there" }
            ]
        });

        let view = extract(Surface::ChatCompletions, &body);

        assert_eq!(view.surface, Some(Surface::ChatCompletions));
        assert_eq!(view.model.as_deref(), Some("gpt-4o"));
        assert_eq!(view.service_tier, Some(json!("flex")));
        assert_eq!(view.max_output_tokens, Some(json!(128)));
        assert_eq!(view.max_output_tokens_param, Some("max_completion_tokens"));
        assert_eq!(view.prompt_text_bytes, "be terse".len() + "hello there".len());
        assert!(!view.has_image_input && !view.has_audio_input);
    }

    #[test]
    fn chat_parts_count_text_and_flag_multimodal() {
        let body = json!({
            "model": "m",
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": "look" },
                    { "type": "image_url", "image_url": { "url": "https://x/y.png" } },
                    { "type": "input_audio", "input_audio": { "data": "AAAA", "format": "wav" } },
                    { "type": "file", "file": { "filename": "doc.pdf" } }
                ]
            }]
        });

        let view = extract(Surface::ChatCompletions, &body);

        assert_eq!(view.prompt_text_bytes, "look".len());
        assert!(view.has_image_input);
        assert!(view.has_audio_input);
        assert!(view.has_file_input);
    }

    #[test]
    fn chat_tool_calls_and_tools_are_counted() {
        let tool = json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Look up weather",
                "parameters": { "type": "object" }
            }
        });
        let body = json!({
            "model": "m",
            "messages": [{
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": { "name": "get", "arguments": "{\"city\":\"SF\"}" }
                }]
            }],
            "tools": [tool.clone()]
        });

        let view = extract(Surface::ChatCompletions, &body);
        let expected = "get".len() + "{\"city\":\"SF\"}".len() + serde_json::to_string(&tool).unwrap().len();

        assert_eq!(view.prompt_text_bytes, expected);
    }

    #[test]
    fn chat_max_completion_tokens_wins_over_max_tokens() {
        let both = extract(Surface::ChatCompletions, &json!({ "max_completion_tokens": 10, "max_tokens": 20 }));
        assert_eq!(both.max_output_tokens, Some(json!(10)));
        assert_eq!(both.max_output_tokens_param, Some("max_completion_tokens"));

        let fallback = extract(Surface::ChatCompletions, &json!({ "max_tokens": 20 }));
        assert_eq!(fallback.max_output_tokens, Some(json!(20)));
        assert_eq!(fallback.max_output_tokens_param, Some("max_tokens"));
    }

    #[test]
    fn chat_unknown_parts_and_fields_are_ignored() {
        let body = json!({
            "model": "m",
            "future_top_level": { "nested": [1, 2, 3] },
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "future_block", "text": "not counted" },
                    { "type": "text", "text": "kept" }
                ],
                "future_field": true
            }]
        });

        let view = extract(Surface::ChatCompletions, &body);
        assert_eq!(view.prompt_text_bytes, "kept".len());
    }

    #[test]
    fn chat_non_string_model_and_tier_are_preserved_raw() {
        let body = json!({ "model": 7, "service_tier": { "tier": "x" } });
        let view = extract(Surface::ChatCompletions, &body);

        assert_eq!(view.model, None);
        assert_eq!(view.service_tier, Some(json!({ "tier": "x" })));
    }

    #[test]
    fn completions_prompt_shapes() {
        let single = extract(Surface::Completions, &json!({ "prompt": "hello" }));
        assert_eq!(single.prompt_text_bytes, "hello".len());
        assert_eq!(single.max_output_tokens_param, None);

        // Each string is its own sequence: the bound is the largest, not the sum.
        let strings = extract(Surface::Completions, &json!({ "prompt": ["ab", "cde"] }));
        assert_eq!(strings.prompt_text_bytes, "cde".len());

        let tokens = extract(Surface::Completions, &json!({ "prompt": [1, 2, 3], "max_tokens": 5 }));
        assert_eq!(tokens.prompt_text_bytes, 3);
        assert_eq!(tokens.max_output_tokens, Some(json!(5)));
        assert_eq!(tokens.max_output_tokens_param, Some("max_tokens"));
    }

    #[test]
    fn completions_mixed_and_nested_token_arrays() {
        let view = extract(Surface::Completions, &json!({ "prompt": ["abc", [1, 2], [3]] }));
        assert_eq!(view.prompt_text_bytes, "abc".len());
        let view = extract(Surface::Completions, &json!({ "prompt": ["a", [1, 2, 3, 4]] }));
        assert_eq!(view.prompt_text_bytes, 4);
    }

    #[test]
    fn responses_instructions_input_and_parts() {
        let simple = extract(
            Surface::Responses,
            &json!({
                "model": "m",
                "instructions": "be brief",
                "input": "hi",
                "max_output_tokens": 64
            }),
        );
        assert_eq!(simple.prompt_text_bytes, "be brief".len() + "hi".len());
        assert_eq!(simple.max_output_tokens, Some(json!(64)));
        assert_eq!(simple.max_output_tokens_param, Some("max_output_tokens"));

        let parts = extract(
            Surface::Responses,
            &json!({
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [
                        { "type": "input_text", "text": "look" },
                        { "type": "input_image", "image_url": "https://x/y.png" },
                        { "type": "input_file", "file_id": "file-1" },
                        { "type": "input_audio", "input_audio": {} }
                    ]
                }]
            }),
        );
        assert_eq!(parts.prompt_text_bytes, "look".len());
        assert!(parts.has_image_input);
        assert!(parts.has_audio_input);
        assert!(parts.has_file_input);
    }

    #[test]
    fn responses_typeless_message_defaults_to_message() {
        let view = extract(Surface::Responses, &json!({ "input": [{ "role": "user", "content": "hi" }] }));
        assert_eq!(view.prompt_text_bytes, "hi".len());
    }

    #[test]
    fn responses_function_call_and_output_are_counted() {
        let view = extract(
            Surface::Responses,
            &json!({
                "input": [
                    { "type": "function_call", "call_id": "c", "name": "go", "arguments": "{\"a\":1}" },
                    { "type": "function_call_output", "call_id": "c", "output": "result" }
                ]
            }),
        );
        assert_eq!(view.prompt_text_bytes, "go".len() + "{\"a\":1}".len() + "result".len());
    }

    #[test]
    fn responses_tools_are_counted() {
        let tool = json!({
            "type": "function",
            "name": "f",
            "description": "d",
            "parameters": { "type": "object" }
        });
        let view = extract(Surface::Responses, &json!({ "tools": [tool.clone()] }));
        assert_eq!(view.prompt_text_bytes, serde_json::to_string(&tool).unwrap().len());
    }

    #[test]
    fn messages_system_and_content_blocks() {
        let view = extract(
            Surface::Messages,
            &json!({
                "model": "claude",
                "system": [{ "type": "text", "text": "sys" }],
                "messages": [
                    { "role": "user", "content": "hi" },
                    { "role": "assistant", "content": [
                        { "type": "text", "text": "ok" },
                        { "type": "tool_use", "id": "t", "name": "f", "input": { "x": 1 } },
                        { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" } },
                        { "type": "document", "source": { "type": "base64", "media_type": "application/pdf", "data": "AAAA" } }
                    ]}
                ]
            }),
        );

        let expected = "sys".len() + "hi".len() + "ok".len() + "f".len() + serde_json::to_string(&json!({ "x": 1 })).unwrap().len();
        assert_eq!(view.prompt_text_bytes, expected);
        assert!(view.has_image_input);
        assert!(!view.has_audio_input);
        assert!(view.has_file_input);
    }

    #[test]
    fn messages_tool_result_content_shapes() {
        let string_result = extract(
            Surface::Messages,
            &json!({
                "messages": [{
                    "role": "user",
                    "content": [{ "type": "tool_result", "tool_use_id": "t", "content": "done" }]
                }]
            }),
        );
        assert_eq!(string_result.prompt_text_bytes, "done".len());

        let block_result = extract(
            Surface::Messages,
            &json!({
                "messages": [{
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "t",
                        "content": [
                            { "type": "text", "text": "part one" },
                            { "type": "image", "source": {} }
                        ]
                    }]
                }]
            }),
        );
        assert_eq!(block_result.prompt_text_bytes, "part one".len());
        assert!(block_result.has_image_input);
    }

    #[test]
    fn messages_tools_and_max_tokens() {
        let tool = json!({ "name": "f", "description": "d", "input_schema": { "type": "object" } });
        let view = extract(Surface::Messages, &json!({ "max_tokens": 32, "tools": [tool.clone()] }));
        assert_eq!(view.max_output_tokens, Some(json!(32)));
        assert_eq!(view.max_output_tokens_param, Some("max_tokens"));
        assert_eq!(view.prompt_text_bytes, serde_json::to_string(&tool).unwrap().len());
    }

    #[test]
    fn embeddings_input_shapes() {
        assert_eq!(
            extract(Surface::Embeddings, &json!({ "input": "hello" })).prompt_text_bytes,
            "hello".len()
        );
        assert_eq!(
            extract(Surface::Embeddings, &json!({ "input": ["a", "bc"] })).prompt_text_bytes,
            "bc".len()
        );
        assert_eq!(extract(Surface::Embeddings, &json!({ "input": [1, 2, 3] })).prompt_text_bytes, 3);
        assert_eq!(
            extract(Surface::Embeddings, &json!({ "input": [[1, 2], [3]] })).prompt_text_bytes,
            2
        );
    }

    #[test]
    fn garbage_input_never_panics() {
        for garbage in [
            json!(null),
            json!(42),
            json!("just a string"),
            json!([1, 2, 3]),
            json!({}),
            json!({ "messages": "not an array" }),
            json!({ "input": { "unexpected": null } }),
            json!({ "prompt": [null, true, { "x": 1 }] }),
            json!({ "messages": [{ "content": 7 }] }),
            json!({ "system": 7, "tools": "nope" }),
        ] {
            for surface in [
                Surface::ChatCompletions,
                Surface::Completions,
                Surface::Responses,
                Surface::Messages,
                Surface::Embeddings,
            ] {
                let view = extract(surface, &garbage);
                assert_eq!(view.surface, Some(surface));
            }
        }
    }

    #[test]
    fn null_max_completion_tokens_falls_back_to_max_tokens() {
        let view = extract(
            Surface::ChatCompletions,
            &json!({ "max_completion_tokens": null, "max_tokens": 100_000 }),
        );
        assert_eq!(view.max_output_tokens, Some(json!(100_000)));
        assert_eq!(view.max_output_tokens_param, Some("max_tokens"));
    }

    #[test]
    fn responses_input_image_without_url_is_not_image_input() {
        let body = |part: Value| json!({ "input": [{ "role": "user", "content": [part] }] });
        let by_file = extract(Surface::Responses, &body(json!({ "type": "input_image", "file_id": "file-1" })));
        assert!(!by_file.has_image_input);
        let by_url = extract(
            Surface::Responses,
            &body(json!({ "type": "input_image", "image_url": "https://x/y.png" })),
        );
        assert!(by_url.has_image_input);
    }

    #[test]
    fn responses_hosted_tools_are_not_counted() {
        let function = json!({ "type": "function", "name": "f", "parameters": {} });
        let view = extract(
            Surface::Responses,
            &json!({ "tools": [function.clone(), { "type": "code_interpreter", "container": { "type": "auto" } }] }),
        );
        assert_eq!(view.prompt_text_bytes, serde_json::to_string(&function).unwrap().len());
    }
}
