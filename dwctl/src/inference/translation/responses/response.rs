//! Chat Completions -> OpenAI Responses response conversion.
//!
//! Ported from onwards' `OpenResponsesAdapter::to_responses_response` and
//! `message_to_items` (`onwards/src/strict/adapter.rs`), plus the two usage /
//! reasoning helpers that live `pub(crate)` in onwards' `strict` module
//! (`merge_reasoning_text`, `chat_usage_to_response_usage`) and so can't be
//! called across the crate boundary. Pure and synchronous: persistence (storing
//! the produced object) runs in the async `post_response` stage, not here.

use std::sync::atomic::{AtomicU64, Ordering};

use onwards::strict::schemas::chat_completions::{
    ChatCompletionResponse, ChatMessage, Choice, ContentPart as ChatContentPart, MessageContent,
};

use super::types::{
    ContentPart, FunctionCallItem, Item, ItemStatus, MessageContent as ResponseMessageContent, MessageItem, ReasoningContent,
    ReasoningItem, ResponseStatus, ResponsesRequest, ResponsesResponse, SummaryContent, TextConfig, TextFormat, TruncationStrategy,
};
use super::util::{chat_usage_to_response_usage, merge_reasoning_text};

/// Convert a Chat Completions response to a Responses response, echoing request
/// parameters as the Open Responses spec requires. `request` is the original
/// inbound Responses request (the response carries many request-only fields).
pub fn to_responses_response(chat_response: &ChatCompletionResponse, request: &ResponsesRequest) -> ResponsesResponse {
    let output = chat_response
        .choices
        .iter()
        .flat_map(|choice| message_to_items(&choice.message, choice.finish_reason.as_deref()))
        .collect();

    let status = determine_response_status(&chat_response.choices);

    let completed_at = if status == ResponseStatus::Completed {
        Some(chat_response.created)
    } else {
        None
    };

    let tool_choice = request
        .tool_choice
        .as_ref()
        .and_then(|tc| serde_json::to_value(tc).ok())
        .unwrap_or(serde_json::Value::String("auto".to_string()));

    ResponsesResponse {
        id: format!("resp_{}", &chat_response.id),
        object: "response".to_string(),
        created_at: chat_response.created,
        completed_at,
        status,
        incomplete_details: None,
        model: request.model.clone(),
        previous_response_id: request.previous_response_id.clone(),
        instructions: request.instructions.clone(),
        output,
        error: None,
        tools: request.tools.clone().unwrap_or_default(),
        tool_choice,
        truncation: request.truncation.clone().unwrap_or(TruncationStrategy::Disabled),
        parallel_tool_calls: request.parallel_tool_calls.unwrap_or(true),
        text: request.text.clone().unwrap_or(TextConfig {
            format: Some(TextFormat::Text),
        }),
        top_p: request.top_p.unwrap_or(1.0),
        presence_penalty: 0.0,
        frequency_penalty: 0.0,
        top_logprobs: 0,
        temperature: request.temperature.unwrap_or(1.0),
        reasoning: serde_json::to_value(&request.reasoning).unwrap_or(serde_json::Value::Null),
        usage: chat_response.usage.as_ref().map(chat_usage_to_response_usage),
        max_output_tokens: request.max_output_tokens,
        max_tool_calls: None,
        store: request.store.unwrap_or(false),
        background: false,
        service_tier: chat_response.service_tier.clone().unwrap_or_else(|| "default".to_string()),
        metadata: request.metadata.clone(),
        safety_identifier: None,
        prompt_cache_key: None,
    }
}

/// Convert a Chat Completions message to Responses output items: an optional
/// leading reasoning item, the text message, then any function-call items.
pub fn message_to_items(message: &ChatMessage, finish_reason: Option<&str>) -> Vec<Item> {
    let mut items = Vec::new();
    let status = match finish_reason {
        Some("length") => Some(ItemStatus::Incomplete),
        _ => Some(ItemStatus::Completed),
    };

    let reasoning_text = merge_reasoning_text(
        message.reasoning.as_ref(),
        message.reasoning_content.as_ref(),
        message.reasoning_details.as_ref(),
    );

    if !reasoning_text.is_empty() {
        items.push(Item::Reasoning(ReasoningItem {
            id: Some(generate_item_id()),
            content: Some(vec![ReasoningContent::Text {
                text: reasoning_text.clone(),
            }]),
            encrypted_content: None,
            summary: Some(vec![SummaryContent::Text { text: reasoning_text }]),
            status,
        }));
    }

    if let Some(ref content) = message.content {
        let content_text = match content {
            MessageContent::Text(t) => t.clone(),
            MessageContent::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ChatContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        };

        if !content_text.is_empty() {
            items.push(Item::Message(MessageItem {
                id: Some(generate_item_id()),
                role: message.role.clone(),
                content: ResponseMessageContent::Parts(vec![ContentPart::OutputText {
                    text: content_text,
                    annotations: vec![],
                    logprobs: vec![],
                }]),
                status,
            }));
        }
    }

    if let Some(ref tool_calls) = message.tool_calls {
        for call in tool_calls {
            items.push(Item::FunctionCall(FunctionCallItem {
                id: Some(generate_item_id()),
                call_id: call.id.clone(),
                name: call.function.name.clone(),
                arguments: call.function.arguments.clone(),
                status,
            }));
        }
    }

    items
}

/// Determine the response status from Chat Completions choices.
fn determine_response_status(choices: &[Choice]) -> ResponseStatus {
    if choices.is_empty() {
        return ResponseStatus::Failed;
    }

    match choices[0].finish_reason.as_deref() {
        Some("stop") => ResponseStatus::Completed,
        Some("length") => ResponseStatus::Incomplete,
        Some("tool_calls") => ResponseStatus::RequiresAction,
        Some("content_filter") => ResponseStatus::Failed,
        _ => ResponseStatus::Completed,
    }
}

static ITEM_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a process-unique item id. (Item ids are non-deterministic and are
/// excluded from parity diffing against the legacy path.)
fn generate_item_id() -> String {
    let count = ITEM_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("item_{count:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use onwards::strict::schemas::chat_completions::{FunctionCall, ToolCall, Usage};

    fn assistant(content: Option<&str>) -> ChatMessage {
        ChatMessage {
            role: "assistant".to_string(),
            content: content.map(|c| MessageContent::Text(c.to_string())),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning: None,
            reasoning_content: None,
            reasoning_details: None,
            extra: None,
        }
    }

    #[test]
    fn text_message_becomes_single_message_item() {
        let items = message_to_items(&assistant(Some("Hello!")), Some("stop"));
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], Item::Message(_)));
        if let Item::Message(ref msg) = items[0] {
            assert_eq!(msg.status, Some(ItemStatus::Completed));
        }
    }

    #[test]
    fn tool_calls_become_function_call_items() {
        let mut message = assistant(None);
        message.tool_calls = Some(vec![ToolCall {
            id: "call_123".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "get_weather".to_string(),
                arguments: r#"{"location": "Paris"}"#.to_string(),
            },
        }]);

        let items = message_to_items(&message, Some("tool_calls"));
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], Item::FunctionCall(_)));
    }

    #[test]
    fn reasoning_field_becomes_leading_reasoning_item() {
        let mut message = assistant(Some("The answer is 42."));
        message.reasoning = Some("Let me think step by step...".to_string());

        let items = message_to_items(&message, Some("stop"));
        assert_eq!(items.len(), 2);
        assert!(matches!(items[0], Item::Reasoning(_)));
        assert!(matches!(items[1], Item::Message(_)));
        if let Item::Reasoning(ref r) = items[0] {
            let summary = r.summary.as_ref().unwrap();
            let SummaryContent::Text { text } = &summary[0];
            assert_eq!(text, "Let me think step by step...");
        }
    }

    #[test]
    fn reasoning_details_are_joined() {
        let mut message = assistant(Some("Done"));
        message.reasoning_details = Some(vec![
            serde_json::json!({"type": "text", "text": "step 1"}),
            serde_json::json!({"type": "text", "text": "step 2"}),
        ]);

        let items = message_to_items(&message, Some("stop"));
        if let Item::Reasoning(ref r) = items[0] {
            let summary = r.summary.as_ref().unwrap();
            let SummaryContent::Text { text } = &summary[0];
            assert_eq!(text, "step 1\nstep 2");
        } else {
            panic!("first item should be reasoning");
        }
    }

    #[test]
    fn status_maps_from_finish_reason() {
        let stop = vec![Choice {
            index: 0,
            message: assistant(Some("Done")),
            finish_reason: Some("stop".to_string()),
            logprobs: None,
        }];
        assert_eq!(determine_response_status(&stop), ResponseStatus::Completed);

        let mut tool_msg = assistant(None);
        tool_msg.tool_calls = Some(vec![]);
        let tool_calls = vec![Choice {
            index: 0,
            message: tool_msg,
            finish_reason: Some("tool_calls".to_string()),
            logprobs: None,
        }];
        assert_eq!(determine_response_status(&tool_calls), ResponseStatus::RequiresAction);
    }

    fn chat_response_with_usage(prompt: u32, completion: u32) -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: "chatcmpl-test".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "gpt-4o".to_string(),
            choices: vec![Choice {
                index: 0,
                message: assistant(Some("done")),
                finish_reason: Some("stop".to_string()),
                logprobs: None,
            }],
            usage: Some(Usage {
                prompt_tokens: prompt,
                completion_tokens: completion,
                total_tokens: prompt + completion,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
            system_fingerprint: None,
            service_tier: None,
        }
    }

    fn minimal_request() -> ResponsesRequest {
        serde_json::from_value(serde_json::json!({ "model": "gpt-4o", "input": "Hello" })).unwrap()
    }

    #[test]
    fn response_echoes_request_and_maps_usage() {
        let mut chat = chat_response_with_usage(100, 50);
        chat.usage = Some(Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            prompt_tokens_details: Some(serde_json::json!({ "cached_tokens": 42 })),
            completion_tokens_details: Some(serde_json::json!({ "reasoning_tokens": 30 })),
        });

        let response = to_responses_response(&chat, &minimal_request());

        assert_eq!(response.object, "response");
        assert_eq!(response.model, "gpt-4o");
        assert_eq!(response.status, ResponseStatus::Completed);
        assert!(response.id.starts_with("resp_"));

        let usage = response.usage.expect("usage present");
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.total_tokens, 150);
        assert_eq!(usage.input_tokens_details.cached_tokens, 42);
        assert_eq!(usage.output_tokens_details.reasoning_tokens, 30);
    }

    #[test]
    fn generated_item_ids_are_unique() {
        let ids: Vec<String> = (0..100).map(|_| generate_item_id()).collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(ids.len(), unique.len());
    }
}
