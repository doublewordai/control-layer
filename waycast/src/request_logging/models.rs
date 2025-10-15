use async_openai::types::{
    CreateBase64EmbeddingResponse, CreateChatCompletionRequest, CreateChatCompletionResponse, CreateChatCompletionStreamResponse,
    CreateCompletionRequest, CreateCompletionResponse, CreateEmbeddingRequest, CreateEmbeddingResponse,
};
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
pub enum AiResponse {
    ChatCompletions(CreateChatCompletionResponse),
    ChatCompletionsStream(Vec<ChatCompletionChunk>),
    Completions(CreateCompletionResponse),
    Embeddings(CreateEmbeddingResponse),
    Base64Embeddings(CreateBase64EmbeddingResponse),
    Other(Value),
}
