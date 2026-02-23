//! Request logging data models.

use std::collections::HashMap;

use async_openai::types::chat::{CreateChatCompletionRequest, CreateChatCompletionResponse, CreateChatCompletionStreamResponse};
use async_openai::types::completions::{CreateCompletionRequest, CreateCompletionResponse};
use async_openai::types::embeddings::{CreateBase64EmbeddingResponse, CreateEmbeddingRequest, CreateEmbeddingResponse};
use async_openai::types::responses::{Response, ResponseStreamEvent};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Errors that can occur during SSE parsing
#[derive(Debug, Clone, PartialEq, Error)]
pub enum SseParseError {
    /// Input does not contain valid SSE format or contains no data
    #[error("Input does not contain valid SSE format or contains no data")]
    InvalidFormat,
}

/// AI request types covering common OpenAI-compatible endpoints
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
pub enum AiRequest {
    ChatCompletions(CreateChatCompletionRequest),
    Completions(CreateCompletionRequest),
    Embeddings(CreateEmbeddingRequest),
    Other(Value),
}

/// Minimal parsed form of a /v1/responses request – only the fields needed for analytics.
#[derive(Debug, Clone)]
pub struct ResponsesRequest {
    pub model: Option<String>,
    pub stream: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedAIRequest {
    pub headers: HashMap<String, String>,
    pub request: AiRequest,
    /// Populated when the request was routed to /v1/responses.
    /// Skipped during serde because `ResponsesRequest` is a local computation artifact.
    #[serde(skip)]
    pub responses_request: Option<ResponsesRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChatCompletionChunk {
    Normal(CreateChatCompletionStreamResponse),
    #[serde(rename = "[DONE]")]
    Done,
}

/// AI response types with special handling for streaming
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
pub enum AiResponse {
    ChatCompletions(CreateChatCompletionResponse),
    ChatCompletionsStream(Vec<ChatCompletionChunk>),
    Completions(CreateCompletionResponse),
    Embeddings(CreateEmbeddingResponse),
    Base64Embeddings(CreateBase64EmbeddingResponse),
    /// Non-streaming /v1/responses response object.
    Responses(Response),
    /// Streaming /v1/responses – SSE events collected until stream end.
    ResponsesStream(Vec<ResponseStreamEvent>),
    Other(Value),
}

// There is currently no need for capturing response headers
// struct ParsedAIResponse {
//     headers: HashMap<String, String>,
//     response: AiResponse,
// }
