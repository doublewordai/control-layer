//! OpenAI Responses -> Chat Completions request conversion.
//!
//! Ported near-verbatim from onwards' `OpenResponsesAdapter::to_chat_request`
//! (`onwards/src/strict/adapter.rs`), with the stateful half removed: the
//! `previous_response_id` store read is NOT here. Hydration (reading the prior
//! turn and inlining its items into `input`) runs as the async `pre_request`
//! stage BEFORE this pure converter, so by the time we run, `request.input`
//! already carries any prior context and this is a plain, synchronous transform.

use super::types::{
    ContentPart, Include, Input, Item, MessageContent as ResponseMessageContent, NamespaceTool, ReasoningContent, ResponsesRequest,
    StopSequence as ResponsesStopSequence, TextConfig, Tool as ResponseTool, ToolChoice as ResponseToolChoice,
};
use onwards::strict::schemas::chat_completions::{
    ChatCompletionRequest, ChatMessage, ContentPart as ChatContentPart, FunctionCall, FunctionDefinition, ImageUrl, MessageContent,
    ResponseFormat, StopSequence as ChatStopSequence, StreamOptions, Tool as ChatTool, ToolCall, ToolChoice as ChatToolChoice,
    ToolChoiceFunction,
};
use tracing::{debug, warn};

/// Convert a (fully hydrated) Responses request into a Chat Completions request.
///
/// Any `previous_response_id` context has already been inlined into
/// `request.input` by the hydration stage.
pub fn to_chat_request(request: &ResponsesRequest) -> ChatCompletionRequest {
    let mut messages: Vec<ChatMessage> = Vec::new();

    // System message from instructions leads the conversation.
    if let Some(ref instructions) = request.instructions {
        messages.push(ChatMessage {
            role: "system".to_string(),
            content: Some(MessageContent::Text(instructions.clone())),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning: None,
            reasoning_content: None,
            reasoning_details: None,
            extra: None,
        });
    }

    messages.extend(input_to_messages(&request.input));

    let (tools, dropped_tools) = match request.tools.as_ref() {
        Some(tools) => {
            let (flattened, dropped) = flatten_tool_namespaces(tools);
            (Some(convert_tools(&flattened)), dropped)
        }
        None => (None, std::collections::BTreeSet::new()),
    };
    let tool_choice = request
        .tool_choice
        .as_ref()
        .map(|choice| reconcile_tool_choice(choice, &dropped_tools));

    let include_logprobs = request.includes(Include::MessageOutputTextLogprobs);

    ChatCompletionRequest {
        model: request.model.clone(),
        messages,
        temperature: request.temperature,
        top_p: request.top_p,
        n: None,
        stream: request.stream,
        stream_options: if request.stream == Some(true) {
            Some(StreamOptions { include_usage: Some(true) })
        } else {
            None
        },
        stop: request.stop.clone().map(|s| match s {
            ResponsesStopSequence::Single(s) => ChatStopSequence::Single(s),
            ResponsesStopSequence::Multiple(v) => ChatStopSequence::Multiple(v),
        }),
        max_tokens: request.max_output_tokens,
        max_completion_tokens: None,
        reasoning_effort: request.reasoning.as_ref().and_then(|reasoning| reasoning.effort.clone()),
        presence_penalty: None,
        frequency_penalty: None,
        logit_bias: None,
        logprobs: include_logprobs.then_some(true),
        top_logprobs: include_logprobs.then_some(request.top_logprobs).flatten(),
        user: request.user.clone(),
        seed: None,
        tools,
        tool_choice,
        parallel_tool_calls: request.parallel_tool_calls,
        response_format: convert_text_format_to_response_format(request.text.as_ref()),
        service_tier: None,
        extra: nvext_passthrough(request),
    }
}

/// The request extensions (`nvext`) carried onto the Chat Completions body.
/// They hold the fusillade daemon's scheduling priority and scheduling
/// tolerations for a batch-dispatched `/v1/responses` request; dropping them
/// here would let the backend schedule a batch request as if it carried
/// neither. A realtime
/// client's own priority and tolerations are stripped by the inference
/// middleware, which runs before translation, so whatever survives to here is
/// either trusted or harmless.
fn nvext_passthrough(request: &ResponsesRequest) -> Option<serde_json::Value> {
    let nvext = request.extra.as_ref()?.get("nvext").filter(|v| v.is_object())?;
    Some(serde_json::json!({ "nvext": nvext }))
}

/// Convert Responses API input to Chat Completions messages.
fn input_to_messages(input: &Input) -> Vec<ChatMessage> {
    match input {
        Input::Text(text) => vec![ChatMessage {
            role: "user".to_string(),
            content: Some(MessageContent::Text(text.clone())),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning: None,
            reasoning_content: None,
            reasoning_details: None,
            extra: None,
        }],
        Input::Items(items) => items_to_messages(items),
    }
}

/// Convert Responses API items to Chat Completions messages.
///
/// Also used by the hydration stage to fold a prior response's `output` items
/// into the current request, which is why it lives on the request side.
pub fn items_to_messages(items: &[Item]) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    let mut pending_reasoning: Option<String> = None;

    for item in items {
        match item {
            Item::Message(msg) => {
                if msg.role != "assistant" {
                    flush_pending_reasoning(&mut messages, &mut pending_reasoning);
                }
                messages.push(ChatMessage {
                    role: msg.role.clone(),
                    content: Some(convert_message_content(&msg.content)),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning: None,
                    reasoning_content: if msg.role == "assistant" { pending_reasoning.take() } else { None },
                    reasoning_details: None,
                    extra: None,
                });
            }
            Item::FunctionCall(call) => {
                let tool_call = ToolCall {
                    id: call.call_id.clone(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    },
                };

                // Append to a trailing assistant message if there is one, else
                // start a new assistant message carrying the tool call.
                if let Some(last) = messages.last_mut()
                    && last.role == "assistant"
                {
                    if last.reasoning_content.is_none() {
                        last.reasoning_content = pending_reasoning.take();
                    }
                    if let Some(ref mut calls) = last.tool_calls {
                        calls.push(tool_call);
                    } else {
                        last.tool_calls = Some(vec![tool_call]);
                    }
                    continue;
                }

                messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: None,
                    name: None,
                    tool_calls: Some(vec![tool_call]),
                    tool_call_id: None,
                    reasoning: None,
                    reasoning_content: pending_reasoning.take(),
                    reasoning_details: None,
                    extra: None,
                });
            }
            Item::FunctionCallOutput(output) => {
                flush_pending_reasoning(&mut messages, &mut pending_reasoning);
                messages.push(ChatMessage {
                    role: "tool".to_string(),
                    content: Some(MessageContent::Text(output.output.clone())),
                    name: None,
                    tool_calls: None,
                    tool_call_id: Some(output.call_id.clone()),
                    reasoning: None,
                    reasoning_content: None,
                    reasoning_details: None,
                    extra: None,
                });
            }
            Item::Reasoning(reasoning) => {
                let plaintext_content = reasoning
                    .content
                    .as_ref()
                    .map(|content| {
                        content
                            .iter()
                            .map(|part| match part {
                                ReasoningContent::Text { text } => text.as_str(),
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                if !plaintext_content.is_empty() {
                    match pending_reasoning.as_mut() {
                        Some(pending) => {
                            pending.push('\n');
                            pending.push_str(&plaintext_content);
                        }
                        None => pending_reasoning = Some(plaintext_content),
                    }
                }
            }
            Item::Unknown(_) => {
                warn!("Unknown item type encountered during conversion");
            }
        }
    }

    flush_pending_reasoning(&mut messages, &mut pending_reasoning);
    messages
}

fn flush_pending_reasoning(messages: &mut Vec<ChatMessage>, pending_reasoning: &mut Option<String>) {
    let Some(reasoning_content) = pending_reasoning.take() else {
        return;
    };
    messages.push(ChatMessage {
        role: "assistant".to_string(),
        content: None,
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning: None,
        reasoning_content: Some(reasoning_content),
        reasoning_details: None,
        extra: None,
    });
}

/// Convert Responses message content to Chat Completions message content.
fn convert_message_content(content: &ResponseMessageContent) -> MessageContent {
    match content {
        ResponseMessageContent::Text(text) => MessageContent::Text(text.clone()),
        ResponseMessageContent::Parts(parts) => {
            let chat_parts: Vec<ChatContentPart> = parts
                .iter()
                .filter_map(|part| match part {
                    ContentPart::InputText { text } => Some(ChatContentPart::Text { text: text.clone() }),
                    ContentPart::OutputText { text, .. } => Some(ChatContentPart::Text { text: text.clone() }),
                    ContentPart::InputImage { image_url, detail } => image_url.as_ref().map(|url| ChatContentPart::ImageUrl {
                        image_url: ImageUrl {
                            url: url.clone(),
                            detail: detail.clone(),
                        },
                    }),
                    ContentPart::InputFile { .. } => {
                        warn!("File input cannot be converted to Chat Completions format");
                        None
                    }
                    ContentPart::Refusal { refusal } => Some(ChatContentPart::Text { text: refusal.clone() }),
                })
                .collect();

            if chat_parts.is_empty() {
                MessageContent::Text(String::new())
            } else {
                MessageContent::Parts(chat_parts)
            }
        }
    }
}

/// Convert Responses tools to Chat Completions tools.
///
/// Namespace groups are flattened first (see [`flatten_tool_namespaces`]), so
/// this only ever sees top-level tools.
fn convert_tools(tools: &[ResponseTool]) -> Vec<ChatTool> {
    tools
        .iter()
        .filter_map(|tool| match tool {
            ResponseTool::Function {
                name,
                description,
                parameters,
                strict,
            } => {
                // OpenAI requires `additionalProperties: false` for strict tools,
                // and `strict` defaults to true, so fill it in when the caller
                // omitted it. Applied regardless of the `strict` value: this is a
                // verbatim port of the onwards adapter's behaviour, and narrowing
                // it to `strict == true` would change the schema we send for
                // explicitly non-strict tools. Deliberately left as-is so this
                // move is behaviour-preserving; revisit separately.
                let mut params = parameters.clone();
                if let Some(obj) = params.as_object_mut()
                    && !obj.contains_key("additionalProperties")
                {
                    obj.insert("additionalProperties".to_string(), serde_json::Value::Bool(false));
                }

                Some(ChatTool {
                    tool_type: "function".to_string(),
                    function: FunctionDefinition {
                        name: name.clone(),
                        description: Some(description.clone()),
                        parameters: Some(params),
                        strict: Some(*strict),
                    },
                })
            }
            // Non-function tool types don't map to Chat Completions.
            _ => {
                debug!("Skipping non-function tool type in conversion");
                None
            }
        })
        .collect()
}

/// Flatten `namespace` tool groups into individual function tools.
///
/// Codex groups its function tools under a `namespace` wrapper
/// (`{"type": "namespace", "name": "functions", "tools": [...]}`). Chat
/// Completions has no namespaced-tool concept, so the group is flattened into
/// the functions it contains. Names are left exactly as Codex declared them:
/// the model sees the flat function name, calls it back flat, and Codex maps a
/// missing namespace to its default `functions` namespace — so no name
/// mangling or round-trip translation is needed.
///
/// `custom` members (freeform tools such as `apply_patch`, whose input is a
/// grammar rather than a JSON Schema) are dropped: there is no Chat Completions
/// equivalent, and silently rewriting them as functions would send the model a
/// schema the tool does not accept.
///
/// Non-namespace tools pass through untouched.
fn flatten_tool_namespaces(tools: &[ResponseTool]) -> (Vec<ResponseTool>, std::collections::BTreeSet<String>) {
    let mut flattened = Vec::with_capacity(tools.len());
    let mut dropped = std::collections::BTreeSet::new();
    for tool in tools {
        match tool {
            ResponseTool::Namespace { tools: members, .. } => {
                for member in members {
                    match member {
                        NamespaceTool::Function {
                            name,
                            description,
                            parameters,
                            strict,
                            ..
                        } => flattened.push(ResponseTool::Function {
                            name: name.clone(),
                            description: description.clone(),
                            parameters: parameters.clone(),
                            strict: strict.unwrap_or(true),
                        }),
                        NamespaceTool::Custom { name, .. } => {
                            debug!(tool = %name, "Skipping freeform tool in namespace group");
                            dropped.insert(name.clone());
                        }
                    }
                }
            }
            other => flattened.push(other.clone()),
        }
    }
    (flattened, dropped)
}

/// Translate a tool choice, downgrading one that pinned a tool we could not
/// forward. A choice naming a tool the request no longer offers is rejected by
/// providers, so it falls back to `auto` and the rest of the request still runs.
fn reconcile_tool_choice(choice: &ResponseToolChoice, dropped: &std::collections::BTreeSet<String>) -> ChatToolChoice {
    match choice {
        ResponseToolChoice::Specific { name: Some(target), .. } if dropped.contains(target) => {
            warn!(tool = %target, "tool_choice named a dropped freeform tool; falling back to auto");
            ChatToolChoice::Mode("auto".to_string())
        }
        other => convert_tool_choice(other),
    }
}

/// Convert Responses tool choice to Chat Completions tool choice.
fn convert_tool_choice(choice: &ResponseToolChoice) -> ChatToolChoice {
    match choice {
        ResponseToolChoice::Mode(mode) => ChatToolChoice::Mode(mode.clone()),
        ResponseToolChoice::Specific { tool_type, name } => {
            if let Some(n) = name {
                ChatToolChoice::Specific {
                    tool_type: tool_type.clone(),
                    function: ToolChoiceFunction { name: n.clone() },
                }
            } else {
                ChatToolChoice::Mode("auto".to_string())
            }
        }
    }
}

/// Map a Responses `text.format` into a Chat Completions `response_format`.
fn convert_text_format_to_response_format(text: Option<&TextConfig>) -> Option<ResponseFormat> {
    let format = text?.format.as_ref()?;
    let mut value = serde_json::to_value(format).ok()?;
    let format_type = value.get("type")?.as_str()?.to_string();

    match format_type.as_str() {
        "json_object" => Some(ResponseFormat {
            format_type,
            json_schema: None,
        }),
        "json_schema" => {
            value.as_object_mut()?.shift_remove("type");
            Some(ResponseFormat {
                format_type,
                json_schema: Some(value),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_text_becomes_single_user_message() {
        let input = Input::Text("Hello".to_string());
        let messages = input_to_messages(&input);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
        assert!(matches!(messages[0].content, Some(MessageContent::Text(ref t)) if t == "Hello"));
    }

    #[test]
    fn items_become_messages_with_tool_call_and_output() {
        use super::super::types::{FunctionCallItem, FunctionCallOutputItem, MessageItem};

        let items = vec![
            Item::Message(MessageItem {
                id: Some("msg_1".to_string()),
                role: "user".to_string(),
                content: ResponseMessageContent::Text("What's the weather?".to_string()),
                status: None,
            }),
            Item::FunctionCall(FunctionCallItem {
                id: Some("fc_1".to_string()),
                call_id: "call_123".to_string(),
                name: "get_weather".to_string(),
                arguments: r#"{"location": "Paris"}"#.to_string(),
                status: None,
            }),
            Item::FunctionCallOutput(FunctionCallOutputItem {
                id: Some("fco_1".to_string()),
                call_id: "call_123".to_string(),
                output: r#"{"temp": 72}"#.to_string(),
            }),
        ];

        let messages = items_to_messages(&items);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
        assert!(messages[1].tool_calls.is_some());
        assert_eq!(messages[2].role, "tool");
        assert_eq!(messages[2].tool_call_id, Some("call_123".to_string()));
    }

    #[test]
    fn namespace_tool_group_is_flattened_to_functions() {
        // Codex sends its function tools wrapped in a `namespace` group; the
        // edge translator must accept the wrapper and flatten it, or the
        // request is rejected before any model sees it.
        let request: ResponsesRequest = serde_json::from_value(serde_json::json!({
            "model": "kimi-k2.5",
            "input": "fix the build",
            "tools": [{
                "type": "namespace",
                "name": "functions",
                "description": "",
                "tools": [
                    {
                        "type": "function",
                        "name": "exec_command",
                        "description": "Runs a command.",
                        "strict": false,
                        "parameters": {"type": "object", "properties": {}}
                    },
                    {
                        "type": "function",
                        "name": "apply_patch",
                        "description": "Edits files.",
                        "strict": false,
                        "parameters": {"type": "object", "properties": {}}
                    }
                ]
            }]
        }))
        .unwrap();

        let chat = to_chat_request(&request);
        let tools = chat.tools.expect("tools should survive conversion");

        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].function.name, "exec_command");
        assert_eq!(tools[1].function.name, "apply_patch");
    }

    #[test]
    fn freeform_tools_inside_a_namespace_are_dropped() {
        // `custom` members carry a grammar, not a JSON Schema. There is no Chat
        // Completions equivalent, so they are dropped rather than mistranslated.
        let request: ResponsesRequest = serde_json::from_value(serde_json::json!({
            "model": "kimi-k2.5",
            "input": "fix the build",
            "tools": [{
                "type": "namespace",
                "name": "functions",
                "tools": [
                    {
                        "type": "function",
                        "name": "exec_command",
                        "description": "Runs a command.",
                        "parameters": {"type": "object", "properties": {}}
                    },
                    {
                        "type": "custom",
                        "name": "apply_patch",
                        "description": "Edits files.",
                        "format": {"type": "grammar", "syntax": "lark", "definition": "start: /.+/"}
                    }
                ]
            }]
        }))
        .unwrap();

        let chat = to_chat_request(&request);
        let tools = chat.tools.expect("tools should survive conversion");

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "exec_command");
    }

    #[test]
    fn namespace_group_and_plain_tools_coexist() {
        // A request can mix grouped and top-level tools; order is preserved.
        let request: ResponsesRequest = serde_json::from_value(serde_json::json!({
            "model": "kimi-k2.5",
            "input": "hi",
            "tools": [
                {
                    "type": "namespace",
                    "name": "functions",
                    "tools": [{
                        "type": "function",
                        "name": "exec_command",
                        "description": "Runs a command.",
                        "parameters": {"type": "object", "properties": {}}
                    }]
                },
                {
                    "type": "function",
                    "name": "get_weather",
                    "description": "Weather.",
                    "parameters": {"type": "object", "properties": {}}
                }
            ]
        }))
        .unwrap();

        let chat = to_chat_request(&request);
        let tools = chat.tools.expect("tools should survive conversion");

        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].function.name, "exec_command");
        assert_eq!(tools[1].function.name, "get_weather");
    }

    #[test]
    fn dropped_freeform_tool_downgrades_a_pinned_tool_choice() {
        // `tool_choice` naming a tool that was dropped would name a tool the
        // request no longer offers; it falls back to `auto` instead.
        let request: ResponsesRequest = serde_json::from_value(serde_json::json!({
            "model": "kimi-k2.5",
            "input": "fix the build",
            "tools": [{
                "type": "namespace",
                "name": "functions",
                "tools": [
                    {
                        "type": "function",
                        "name": "exec_command",
                        "description": "Runs a command.",
                        "parameters": {"type": "object", "properties": {}}
                    },
                    {
                        "type": "custom",
                        "name": "apply_patch",
                        "format": {"type": "grammar", "syntax": "lark", "definition": "start: /.+/"}
                    }
                ]
            }],
            "tool_choice": {"type": "function", "name": "apply_patch"}
        }))
        .unwrap();

        let chat = to_chat_request(&request);
        let choice = chat.tool_choice.expect("tool_choice should survive");

        assert!(matches!(choice, ChatToolChoice::Mode(ref mode) if mode == "auto"));
    }

    #[test]
    fn a_tool_choice_naming_a_surviving_tool_is_preserved() {
        let request: ResponsesRequest = serde_json::from_value(serde_json::json!({
            "model": "kimi-k2.5",
            "input": "fix the build",
            "tools": [{
                "type": "namespace",
                "name": "functions",
                "tools": [{
                    "type": "function",
                    "name": "exec_command",
                    "description": "Runs a command.",
                    "parameters": {"type": "object", "properties": {}}
                }]
            }],
            "tool_choice": {"type": "function", "name": "exec_command"}
        }))
        .unwrap();

        let chat = to_chat_request(&request);
        let choice = chat.tool_choice.expect("tool_choice should survive");

        match choice {
            ChatToolChoice::Specific { function, .. } => assert_eq!(function.name, "exec_command"),
            other => panic!("expected a specific tool choice, got {other:?}"),
        }
    }

    #[test]
    fn a_namespace_echoes_back_exactly_as_sent() {
        // The Responses object echoes the request's tools, so parsing and
        // re-serializing must not invent fields the caller never sent.
        let sent = serde_json::json!({
            "model": "kimi-k2.5",
            "input": "fix the build",
            "tools": [{
                "type": "namespace",
                "name": "functions",
                "tools": [
                    {
                        "type": "function",
                        "name": "exec_command",
                        "description": "Runs a command.",
                        "parameters": {"type": "object", "properties": {}}
                    },
                    {
                        "type": "custom",
                        "name": "apply_patch",
                        "format": {"type": "grammar", "syntax": "lark", "definition": "start: /.+/"}
                    }
                ]
            }]
        });

        let request: ResponsesRequest = serde_json::from_value(sent.clone()).unwrap();
        let echoed = serde_json::to_value(request.tools.unwrap()).unwrap();

        assert_eq!(echoed, sent["tools"]);
    }

    #[test]
    fn simple_request_folds_instructions_and_input() {
        let request: ResponsesRequest = serde_json::from_value(serde_json::json!({
            "model": "gpt-4o",
            "input": "Hello",
            "instructions": "Be helpful",
            "temperature": 0.7,
            "max_output_tokens": 100
        }))
        .unwrap();

        let chat = to_chat_request(&request);

        assert_eq!(chat.model, "gpt-4o");
        assert_eq!(chat.messages.len(), 2); // system + user
        assert_eq!(chat.messages[0].role, "system");
        assert_eq!(chat.messages[1].role, "user");
        assert_eq!(chat.temperature, Some(0.7));
        assert_eq!(chat.max_tokens, Some(100));
    }

    #[test]
    fn reasoning_effort_is_forwarded() {
        let request: ResponsesRequest = serde_json::from_value(serde_json::json!({
            "model": "kimi-k2.5",
            "input": "Hello",
            "reasoning": {"effort": "none"}
        }))
        .unwrap();

        let chat = to_chat_request(&request);
        assert_eq!(chat.reasoning_effort.as_ref(), Some(&serde_json::json!("none")));
    }

    #[test]
    fn logprobs_include_requests_chat_logprobs() {
        let request: ResponsesRequest = serde_json::from_value(serde_json::json!({
            "model": "gpt-4o",
            "input": "Hello",
            "include": ["message.output_text.logprobs"],
            "top_logprobs": 3
        }))
        .unwrap();

        let chat = to_chat_request(&request);
        assert_eq!(chat.logprobs, Some(true));
        assert_eq!(chat.top_logprobs, Some(3));
    }

    #[test]
    fn plaintext_reasoning_item_is_replayed_on_assistant_message() {
        let request: ResponsesRequest = serde_json::from_value(serde_json::json!({
            "model": "gpt-4o",
            "input": [
                {
                    "type": "reasoning",
                    "content": [{"type": "reasoning_text", "text": "private chain"}]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "answer"}]
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "continue"}]
                }
            ]
        }))
        .unwrap();

        let chat = to_chat_request(&request);
        assert_eq!(chat.messages[0].role, "assistant");
        assert_eq!(chat.messages[0].reasoning_content.as_deref(), Some("private chain"));
        assert_eq!(chat.messages[1].role, "user");
    }

    #[test]
    fn json_schema_text_format_keeps_key_order() {
        let schema = r#"{"type":"object","properties":{"zeta":{"type":"string"},"alpha":{"type":"string"}},"required":["zeta","alpha"]}"#;
        let request: ResponsesRequest = serde_json::from_str(&format!(
            r#"{{"model":"m","input":"hi","text":{{"format":{{"type":"json_schema","name":"n","strict":true,"schema":{schema}}}}}}}"#
        ))
        .unwrap();
        let chat = serde_json::to_string(&to_chat_request(&request)).unwrap();
        assert!(chat.contains(&format!(r#""schema":{schema}"#)), "{chat}");
    }

    #[test]
    fn json_schema_text_format_becomes_response_format() {
        let request: ResponsesRequest = serde_json::from_value(serde_json::json!({
            "model": "gpt-4o",
            "input": "Return product data",
            "text": { "format": {
                "type": "json_schema",
                "name": "product_info",
                "strict": true,
                "schema": {
                    "type": "object",
                    "required": ["title"],
                    "properties": { "title": { "type": "string" } },
                    "additionalProperties": false
                }
            } }
        }))
        .unwrap();

        let chat = to_chat_request(&request);
        let rf = chat.response_format.expect("json_schema text format should be forwarded");

        assert_eq!(rf.format_type, "json_schema");
        assert_eq!(
            rf.json_schema,
            Some(serde_json::json!({
                "name": "product_info",
                "strict": true,
                "schema": {
                    "type": "object",
                    "required": ["title"],
                    "properties": { "title": { "type": "string" } },
                    "additionalProperties": false
                }
            }))
        );
    }

    #[test]
    fn json_object_text_format_becomes_response_format() {
        let request: ResponsesRequest = serde_json::from_value(serde_json::json!({
            "model": "gpt-4o",
            "input": "Return JSON",
            "text": { "format": { "type": "json_object" } }
        }))
        .unwrap();

        let chat = to_chat_request(&request);
        let rf = chat.response_format.expect("json_object text format should be forwarded");

        assert_eq!(rf.format_type, "json_object");
        assert_eq!(rf.json_schema, None);
    }

    #[test]
    fn stream_options_track_stream_flag() {
        let streaming: ResponsesRequest = serde_json::from_value(serde_json::json!({
            "model": "gpt-4o", "input": "Hello", "stream": true
        }))
        .unwrap();
        assert_eq!(
            to_chat_request(&streaming)
                .stream_options
                .expect("set when streaming")
                .include_usage,
            Some(true)
        );

        let blocking: ResponsesRequest = serde_json::from_value(serde_json::json!({
            "model": "gpt-4o", "input": "Hello"
        }))
        .unwrap();
        assert!(to_chat_request(&blocking).stream_options.is_none());
    }

    #[test]
    fn nvext_survives_conversion() {
        let request: ResponsesRequest = serde_json::from_value(serde_json::json!({
            "model": "m",
            "input": "hi",
            "stream": true,
            "nvext": {
                "agent_hints": {"priority": -1_700_000_000},
                "routing_constraints": {"tolerations": []}
            }
        }))
        .unwrap();
        let chat = serde_json::to_value(to_chat_request(&request)).unwrap();
        assert_eq!(
            chat["nvext"]["routing_constraints"]["tolerations"],
            serde_json::json!([]),
            "a batch request must keep the daemon's tolerations"
        );
        assert_eq!(chat["nvext"]["agent_hints"]["priority"], -1_700_000_000);

        let request: ResponsesRequest = serde_json::from_value(serde_json::json!({"model": "m", "input": "hi"})).unwrap();
        let chat = serde_json::to_value(to_chat_request(&request)).unwrap();
        assert!(chat.get("nvext").is_none(), "nothing to carry, nothing added");
    }
}
