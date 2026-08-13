//! Request logging data models.

use std::collections::HashMap;

use async_openai::types::chat::{CreateChatCompletionResponse, CreateChatCompletionStreamResponse};
use async_openai::types::completions::{CreateCompletionRequest, CreateCompletionResponse};
use async_openai::types::embeddings::{CreateBase64EmbeddingResponse, CreateEmbeddingRequest, CreateEmbeddingResponse};
// Chat REQUESTS use onwards' strict schema, not async-openai's. onwards owns the
// chat request shape (dwctl already reuses it in translation) and models
// `reasoning_effort` as a permissive `serde_json::Value`, so canonical values
// async-openai has not caught up with - notably `max`, which
// `crate::reasoning::ReasoningEffort` defines and the platform routes on - still
// deserialize instead of collapsing the request to `AiRequest::Other`.
use onwards::strict::schemas::chat_completions::ChatCompletionRequest;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::inference::translation::anthropic::model::MessagesResponse;
use crate::inference::translation::responses::types::{ResponsesResponse, ResponsesStreamingEvent};

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
    ChatCompletions(ChatCompletionRequest),
    // NOT onwards' schemas, deliberately: `AiRequest` is untagged, so classification
    // depends on each variant's REQUIRED fields to disambiguate. onwards'
    // `CompletionRequest` requires only `model`, so it would swallow embeddings (and
    // anything else) before the right variant was tried. async-openai's stricter
    // shapes are what make the untagged match work. Chat is safe on onwards' schema
    // because `messages` is required there and it is tried first.
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

/// SSE chunk emitted by an upstream provider when it fails mid-stream.
///
/// OpenAI-compatible inference engines (Dynamo, vLLM, etc.) signal errors that occur
/// after the 200 OK headers have been sent by emitting a `data:` frame whose payload is
/// `{"error": {...}}` instead of the usual `chat.completion.chunk` shape. Capturing this
/// variant lets the analytics layer reclassify the request as failed, even though the
/// HTTP status was 200.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamErrorChunk {
    pub error: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChatCompletionChunk {
    Normal(CreateChatCompletionStreamResponse),
    Error(StreamErrorChunk),
    #[serde(rename = "[DONE]")]
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CompletionChunk {
    Normal(CreateCompletionResponse), //async-openai reuses this type for streaming
    Error(StreamErrorChunk),
    #[serde(rename = "[DONE]")]
    Done,
}

/// AI response types with special handling for streaming.
///
/// One dwctl-owned enum whose variants wrap each endpoint's canonical response
/// type from wherever that endpoint's types live:
/// - chat completions / completions / embeddings are the native OpenAI-shape
///   passthrough endpoints, so they use async-openai's tolerant response types
///   (outlet captures the raw upstream provider body, which async-openai parses),
/// - responses and anthropic bodies are produced by dwctl's own edge translators
///   (translation sits inside outlet), so they parse back with dwctl's own types:
///   [`ResponsesResponse`] / [`ResponsesStreamingEvent`] and [`MessagesResponse`].
///
/// This single type feeds both request logging (stored as JSONB) and billing
/// ([`TokenMetrics`](crate::request_logging::serializers) via `From<&AiResponse>`),
/// so the response is parsed once, into one shape, for both.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
pub enum AiResponse {
    ChatCompletions(CreateChatCompletionResponse),
    ChatCompletionsStream(Vec<ChatCompletionChunk>),
    Completions(CreateCompletionResponse),
    CompletionsStream(Vec<CompletionChunk>),
    Embeddings(CreateEmbeddingResponse),
    Base64Embeddings(CreateBase64EmbeddingResponse),
    /// Non-streaming /v1/responses response object (dwctl-owned schema).
    Responses(ResponsesResponse),
    /// Streaming /v1/responses – SSE events collected until stream end.
    ResponsesStream(Vec<ResponsesStreamingEvent>),
    /// Non-streaming /v1/messages (Anthropic) response object.
    Anthropic(MessagesResponse),
    /// Streaming /v1/messages – SSE event frames collected as raw JSON. Anthropic's
    /// stream events are emitted ad-hoc (never typed structs anywhere in dwctl), so
    /// keeping them as `Value` avoids inventing a parallel typed event hierarchy
    /// just for billing; usage is read by scanning the frames in `TokenMetrics`.
    /// Listed last (before `Other`) so its `Vec<Value>` catch-all cannot shadow the
    /// other, typed stream variants during untagged deserialization.
    AnthropicStream(Vec<Value>),
    Other(Value),
}

// There is currently no need for capturing response headers
// struct ParsedAIResponse {
//     headers: HashMap<String, String>,
//     response: AiResponse,
// }
