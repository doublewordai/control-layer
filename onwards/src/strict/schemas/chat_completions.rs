//! Chat Completions API schemas
//!
//! These schemas match the OpenAI Chat Completions API specification.
//! See: https://platform.openai.com/docs/api-reference/chat

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::utils::{ensure_field, scrub_request_id_fields_from_extra};

pub(crate) fn generated_chat_completion_id() -> String {
    format!("chatcmpl-{}", Uuid::new_v4())
}

fn normalize_chat_message_value(value: &mut Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };

    ensure_field(object, "role", || Value::String("assistant".to_string()));
    ensure_field(object, "content", || Value::Null);
}

fn normalize_chat_choice_value(value: &mut Value, fallback_index: usize) {
    let Some(object) = value.as_object_mut() else {
        return;
    };

    ensure_field(object, "index", || Value::from(fallback_index));
    ensure_field(object, "finish_reason", || Value::Null);
    ensure_field(object, "logprobs", || Value::Null);

    if let Some(message) = object.get_mut("message") {
        normalize_chat_message_value(message);
    }
}

fn normalize_chat_chunk_choice_value(value: &mut Value, fallback_index: usize) {
    let Some(object) = value.as_object_mut() else {
        return;
    };

    ensure_field(object, "index", || Value::from(fallback_index));
    ensure_field(object, "finish_reason", || Value::Null);
    ensure_field(object, "logprobs", || Value::Null);

    if !object.contains_key("delta") {
        object.insert("delta".to_string(), serde_json::json!({}));
    }
}

/// Backfill omitted non-critical chat completion response fields during strict
/// sanitization. This is intentionally kept out of serde defaults so we only
/// relax third-party provider payloads, not every deserialize path.
pub(crate) fn normalize_chat_completion_response_value(value: &mut Value, fallback_model: &str) {
    let Some(object) = value.as_object_mut() else {
        return;
    };

    // Only coerce payloads that already look like chat completion responses.
    if !object.contains_key("choices") {
        return;
    }

    ensure_field(object, "id", || {
        Value::String(generated_chat_completion_id())
    });
    ensure_field(object, "object", || {
        Value::String("chat.completion".to_string())
    });
    ensure_field(object, "created", || Value::from(0));
    ensure_field(object, "model", || {
        Value::String(fallback_model.to_string())
    });
    ensure_field(object, "usage", || Value::Null);
    ensure_field(object, "system_fingerprint", || Value::Null);
    ensure_field(object, "service_tier", || Value::Null);

    if let Some(choices) = object.get_mut("choices").and_then(Value::as_array_mut) {
        for (index, choice) in choices.iter_mut().enumerate() {
            normalize_chat_choice_value(choice, index);
        }
    }
}

/// Backfill omitted non-critical chat completion chunk fields during strict
/// sanitization. This keeps partial-but-usable streamed chunks from failing.
pub(crate) fn normalize_chat_completion_chunk_value(
    value: &mut Value,
    fallback_model: &str,
    fallback_id: &str,
) {
    let Some(object) = value.as_object_mut() else {
        return;
    };

    // Only coerce payloads that already look like streamed chat chunks.
    if !object.contains_key("choices") {
        return;
    }

    ensure_field(object, "id", || Value::String(fallback_id.to_string()));
    ensure_field(object, "object", || {
        Value::String("chat.completion.chunk".to_string())
    });
    ensure_field(object, "created", || Value::from(0));
    ensure_field(object, "model", || {
        Value::String(fallback_model.to_string())
    });
    ensure_field(object, "usage", || Value::Null);
    ensure_field(object, "system_fingerprint", || Value::Null);
    ensure_field(object, "service_tier", || Value::Null);

    if let Some(choices) = object.get_mut("choices").and_then(Value::as_array_mut) {
        for (index, choice) in choices.iter_mut().enumerate() {
            normalize_chat_chunk_choice_value(choice, index);
        }
    }
}

/// Request body for POST /v1/chat/completions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    /// The model to use for completion
    pub model: String,

    /// The messages to generate a completion for
    pub messages: Vec<ChatMessage>,

    /// Sampling temperature (0-2)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    /// Nucleus sampling parameter
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,

    /// Number of completions to generate
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,

    /// Whether to stream the response
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,

    /// Stream options (e.g., include_usage)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,

    /// Stop sequences
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopSequence>,

    /// Maximum tokens to generate
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,

    /// Maximum completion tokens (newer parameter)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,

    /// Canonical OpenAI reasoning effort.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<serde_json::Value>,

    /// Presence penalty (-2.0 to 2.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,

    /// Frequency penalty (-2.0 to 2.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,

    /// Logit bias for tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logit_bias: Option<serde_json::Value>,

    /// Whether to return log probabilities
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<bool>,

    /// Number of most likely tokens to return
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<u32>,

    /// User identifier for abuse tracking
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,

    /// Random seed for deterministic sampling
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,

    /// Tools available to the model
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,

    /// How to choose which tool to use
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,

    /// Parallel tool calls setting
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,

    /// Response format specification
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,

    /// Service tier (auto, default, etc.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,

    /// Additional fields not explicitly modeled
    #[serde(flatten)]
    pub extra: Option<serde_json::Value>,
}

impl ChatCompletionRequest {
    /// Remove caller-controlled request identifier fields before proxying.
    pub(crate) fn scrub_request_id_fields(&mut self) {
        scrub_request_id_fields_from_extra(&mut self.extra);
    }
}

/// A message in the conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// The role of the message author
    pub role: String,

    /// The message content (can be string or array of content parts)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,

    /// Name of the author (for function/tool messages)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Tool calls made by the assistant
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,

    /// Tool call ID (for tool messages)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,

    /// Reasoning text (OpenRouter format)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,

    /// Reasoning content text (vLLM / DeepSeek format)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,

    /// Reasoning details array (OpenRouter format)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_details: Option<Vec<serde_json::Value>>,

    /// Additional fields
    #[serde(flatten)]
    pub extra: Option<serde_json::Value>,
}

/// Message content - either a string or array of content parts
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

/// A content part in a multimodal message
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrl },
}

/// Image URL specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Stream options
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_usage: Option<bool>,
}

/// Stop sequence - single string or array
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StopSequence {
    Single(String),
    Multiple(Vec<String>),
}

/// Tool definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionDefinition,
}

/// Function definition for a tool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// Tool choice specification
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolChoice {
    Mode(String), // "none", "auto", "required"
    Specific {
        #[serde(rename = "type")]
        tool_type: String,
        function: ToolChoiceFunction,
    },
}

/// Specific function choice
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolChoiceFunction {
    pub name: String,
}

/// Tool call in an assistant message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

/// Function call details
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// Response format specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseFormat {
    #[serde(rename = "type")]
    pub format_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json_schema: Option<serde_json::Value>,
}

/// Response from POST /v1/chat/completions (non-streaming)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<Choice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
}

/// A completion choice
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub index: u32,
    pub message: ChatMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<serde_json::Value>,
}

/// Token usage information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<serde_json::Value>,
}

/// Streaming chunk response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
}

/// A streaming choice
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: ChunkDelta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<serde_json::Value>,
}

/// Delta content in a streaming chunk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ChunkToolCall>>,
    /// Reasoning text (OpenRouter format)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Reasoning content text (vLLM / DeepSeek format)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Reasoning details array (OpenRouter format)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_details: Option<Vec<serde_json::Value>>,
}

/// Tool call in a streaming chunk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkToolCall {
    pub index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    pub call_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<ChunkFunctionCall>,
}

/// Function call in a streaming chunk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkFunctionCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_simple_request() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [
                {"role": "user", "content": "Hello"}
            ]
        }"#;

        let request: ChatCompletionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.model, "gpt-4");
        assert_eq!(request.messages.len(), 1);
    }

    #[test]
    fn test_deserialize_multimodal_content() {
        let json = r#"{
            "model": "gpt-4-vision",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "What's in this image?"},
                        {"type": "image_url", "image_url": {"url": "https://example.com/image.jpg"}}
                    ]
                }
            ]
        }"#;

        let request: ChatCompletionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.messages.len(), 1);
        if let Some(MessageContent::Parts(parts)) = &request.messages[0].content {
            assert_eq!(parts.len(), 2);
        } else {
            panic!("Expected parts content");
        }
    }

    #[test]
    fn test_deserialize_with_tools() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "What's the weather?"}],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "description": "Get the weather",
                        "parameters": {"type": "object", "properties": {}}
                    }
                }
            ]
        }"#;

        let request: ChatCompletionRequest = serde_json::from_str(json).unwrap();
        assert!(request.tools.is_some());
        assert_eq!(request.tools.unwrap().len(), 1);
    }

    #[test]
    fn test_serialize_response() {
        let response = ChatCompletionResponse {
            id: "chatcmpl-123".to_string(),
            object: "chat.completion".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
            choices: vec![Choice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: Some(MessageContent::Text("Hello!".to_string())),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning: None,
                    reasoning_content: None,
                    reasoning_details: None,
                    extra: None,
                },
                finish_reason: Some("stop".to_string()),
                logprobs: None,
            }],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
            system_fingerprint: None,
            service_tier: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("chatcmpl-123"));
    }

    #[test]
    fn test_nonstreaming_response_with_reasoning_roundtrips() {
        let json = r#"{
            "id": "chatcmpl-abc",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "deepseek-r1",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "The answer is 4.",
                    "reasoning": "Let me think step by step...",
                    "reasoning_details": [{"type": "text", "text": "step 1"}]
                },
                "finish_reason": "stop"
            }]
        }"#;

        let response: ChatCompletionResponse = serde_json::from_str(json).unwrap();
        let msg = &response.choices[0].message;
        assert_eq!(
            msg.reasoning.as_deref(),
            Some("Let me think step by step...")
        );
        assert!(msg.reasoning_details.is_some());
        assert_eq!(msg.reasoning_details.as_ref().unwrap().len(), 1);

        // Round-trip: serialize and deserialize again
        let serialized = serde_json::to_string(&response).unwrap();
        let round_tripped: ChatCompletionResponse = serde_json::from_str(&serialized).unwrap();
        assert_eq!(round_tripped.choices[0].message.reasoning, msg.reasoning);
    }

    #[test]
    fn test_streaming_chunk_with_reasoning_roundtrips() {
        let json = r#"{
            "id": "chatcmpl-abc",
            "object": "chat.completion.chunk",
            "created": 1700000000,
            "model": "deepseek-r1",
            "choices": [{
                "index": 0,
                "delta": {
                    "reasoning": "thinking...",
                    "reasoning_details": [{"type": "text", "text": "step"}]
                },
                "finish_reason": null
            }]
        }"#;

        let chunk: ChatCompletionChunk = serde_json::from_str(json).unwrap();
        let delta = &chunk.choices[0].delta;
        assert_eq!(delta.reasoning.as_deref(), Some("thinking..."));
        assert!(delta.reasoning_details.is_some());

        let serialized = serde_json::to_string(&chunk).unwrap();
        let round_tripped: ChatCompletionChunk = serde_json::from_str(&serialized).unwrap();
        assert_eq!(round_tripped.choices[0].delta.reasoning, delta.reasoning);
    }

    #[test]
    fn test_streaming_chunk_with_reasoning_content_vllm_roundtrips() {
        let json = r#"{
            "id": "chatcmpl-abc",
            "object": "chat.completion.chunk",
            "created": 1700000000,
            "model": "deepseek-r1",
            "choices": [{
                "index": 0,
                "delta": {
                    "reasoning_content": "let me reason about this..."
                },
                "finish_reason": null
            }]
        }"#;

        let chunk: ChatCompletionChunk = serde_json::from_str(json).unwrap();
        let delta = &chunk.choices[0].delta;
        assert_eq!(
            delta.reasoning_content.as_deref(),
            Some("let me reason about this...")
        );
        assert!(delta.reasoning.is_none());

        let serialized = serde_json::to_string(&chunk).unwrap();
        assert!(serialized.contains("reasoning_content"));
        assert!(!serialized.contains("\"reasoning\""));
    }

    #[test]
    fn test_chunk_without_reasoning_no_regression() {
        let json = r#"{
            "id": "chatcmpl-abc",
            "object": "chat.completion.chunk",
            "created": 1700000000,
            "model": "gpt-4",
            "choices": [{
                "index": 0,
                "delta": {
                    "content": "Hello!"
                },
                "finish_reason": null
            }]
        }"#;

        let chunk: ChatCompletionChunk = serde_json::from_str(json).unwrap();
        let delta = &chunk.choices[0].delta;
        assert_eq!(delta.content.as_deref(), Some("Hello!"));
        assert!(delta.reasoning.is_none());
        assert!(delta.reasoning_content.is_none());
        assert!(delta.reasoning_details.is_none());

        // Reasoning fields should not appear in serialized output
        let serialized = serde_json::to_string(&chunk).unwrap();
        assert!(!serialized.contains("reasoning"));
    }
}
