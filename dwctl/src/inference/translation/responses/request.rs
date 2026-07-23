//! OpenAI Responses -> Chat Completions request conversion.
//!
//! Ported near-verbatim from onwards' `OpenResponsesAdapter::to_chat_request`
//! (`onwards/src/strict/adapter.rs`), with the stateful half removed: the
//! `previous_response_id` store read is NOT here. Hydration (reading the prior
//! turn and inlining its items into `input`) runs as the async `pre_request`
//! stage BEFORE this pure converter, so by the time we run, `request.input`
//! already carries any prior context and this is a plain, synchronous transform.

use super::types::{
    ContentPart, Input, Item, MessageContent as ResponseMessageContent, ResponsesRequest, StopSequence as ResponsesStopSequence,
    TextConfig, Tool as ResponseTool, ToolChoice as ResponseToolChoice,
};
use onwards::strict::schemas::chat_completions::{
    ChatCompletionRequest, ChatMessage, ContentPart as ChatContentPart, FunctionCall, FunctionDefinition, ImageUrl, MessageContent,
    ResponseFormat, StopSequence as ChatStopSequence, StreamOptions, Tool as ChatTool, ToolCall, ToolChoice as ChatToolChoice,
    ToolChoiceFunction,
};
use tracing::{debug, warn};

/// Convert a (fully hydrated) Responses request into a Chat Completions request.
///
/// Pure and infallible: any `previous_response_id` context has already been
/// inlined into `request.input` by the hydration stage, so this only walks the
/// self-contained request. Message order is `[instructions, input...]`, matching
/// the onwards adapter.
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

    let tools = request.tools.as_ref().map(|t| convert_tools(t));
    let tool_choice = request.tool_choice.as_ref().map(convert_tool_choice);

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
        logprobs: None,
        top_logprobs: None,
        user: request.user.clone(),
        seed: None,
        tools,
        tool_choice,
        parallel_tool_calls: request.parallel_tool_calls,
        response_format: convert_text_format_to_response_format(request.text.as_ref()),
        service_tier: None,
        extra: None,
    }
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

    for item in items {
        match item {
            Item::Message(msg) => {
                messages.push(ChatMessage {
                    role: msg.role.clone(),
                    content: Some(convert_message_content(&msg.content)),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning: None,
                    reasoning_content: None,
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
                    reasoning_content: None,
                    reasoning_details: None,
                    extra: None,
                });
            }
            Item::FunctionCallOutput(output) => {
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
            Item::Reasoning(_) => {
                // Reasoning items are model-internal and can't be fed back.
                debug!("Skipping reasoning item in conversion to messages");
            }
            Item::Unknown(_) => {
                warn!("Unknown item type encountered during conversion");
            }
        }
    }

    messages
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
            value.as_object_mut()?.remove("type");
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
}
