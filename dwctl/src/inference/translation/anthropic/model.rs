//! Serde model for the Anthropic Messages API.
//!
//! Inbound request types are `Deserialize`; outbound response/error types are
//! `Serialize`. Unknown request fields are ignored so forward-compatible clients
//! do not break. The translated Chat Completions request is built as JSON in
//! [`super::request`] (it targets onwards' own schema), but the Anthropic
//! response we own is modelled as typed structs here.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An inbound Anthropic `/v1/messages` request.
#[derive(Debug, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    /// Required by Anthropic; absence is a 400.
    pub max_tokens: u32,
    #[serde(default)]
    pub messages: Vec<InputMessage>,
    #[serde(default)]
    pub system: Option<System>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub top_k: Option<u32>,
    #[serde(default)]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    pub tools: Option<Vec<Tool>>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    /// Extended-thinking config, e.g. `{"type":"enabled","budget_tokens":N}`.
    /// Mapped to OpenAI `reasoning_effort` on the request.
    #[serde(default)]
    pub thinking: Option<Value>,
}

/// Top-level `system`: either a string or an array of content blocks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum System {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// A single conversation message.
#[derive(Debug, Deserialize)]
pub struct InputMessage {
    pub role: String,
    pub content: Content,
}

/// Message content: either a string or an array of typed content blocks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// A typed Anthropic content block.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(default)]
        cache_control: Option<Value>,
    },
    Image {
        source: ImageSource,
        #[serde(default)]
        cache_control: Option<Value>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(default)]
        cache_control: Option<Value>,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: Option<ToolResultContent>,
        #[serde(default)]
        is_error: Option<bool>,
        #[serde(default)]
        cache_control: Option<Value>,
    },
    /// Forward-compat catch-all. Inbound `thinking` blocks land here and are
    /// intentionally dropped: OpenAI-spec backends do not consume reasoning as
    /// input, so there is nothing downstream to forward them to.
    #[serde(other)]
    Other,
}

/// An image source: base64 inline data or a URL.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

/// `tool_result` content: a string or an array of blocks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<Value>),
}

/// An Anthropic tool definition.
#[derive(Debug, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: Option<Value>,
    #[serde(default)]
    pub cache_control: Option<Value>,
}

// ---------------------------------------------------------------------------
// Response types (Anthropic Messages response we serialise back to the client).
// ---------------------------------------------------------------------------

/// An outbound Anthropic Messages response.
#[derive(Debug, Serialize)]
pub struct MessagesResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub message_type: MessageType,
    pub role: ResponseRole,
    pub model: String,
    pub content: Vec<ResponseContentBlock>,
    pub stop_reason: Option<StopReason>,
    pub stop_sequence: Option<String>,
    pub usage: Usage,
}

/// Always `"message"`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    Message,
}

/// Always `"assistant"` on a response.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseRole {
    Assistant,
}

/// A content block in an outbound response.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseContentBlock {
    /// Model reasoning, surfaced from the backend's `reasoning_content` (or
    /// `thinking_blocks` if a backend ever provides signed ones). Emitted before
    /// the answer, per the Anthropic Messages shape.
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
}

/// Anthropic stop reasons. `StopSequence` is only emitted when the backend tells
/// us a stop sequence matched (vLLM/sglang put the matched string in
/// `choices[].stop_reason`); standard OpenAI does not, so we fall back to
/// `EndTurn` there.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    StopSequence,
}

/// Token usage on a response. Per Anthropic's usage shape, `input_tokens`
/// excludes cached prompt tokens, which are reported separately. The cache
/// fields are omitted entirely when zero/absent.
#[derive(Debug, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
}

/// The Anthropic error envelope (`{"type":"error","error":{...}}`).
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    #[serde(rename = "type")]
    pub envelope_type: ErrorEnvelopeType,
    pub error: ErrorDetail,
}

/// Always `"error"`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorEnvelopeType {
    Error,
}

/// The body of an error envelope.
#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    #[serde(rename = "type")]
    pub error_type: ErrorType,
    pub message: String,
}

/// Anthropic error type tags.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorType {
    InvalidRequestError,
    AuthenticationError,
    PermissionError,
    NotFoundError,
    RequestTooLarge,
    RateLimitError,
    OverloadedError,
    ApiError,
}
