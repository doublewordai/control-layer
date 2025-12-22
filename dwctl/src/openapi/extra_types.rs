//! OpenAI-compatible API request/response models.
//!
//! These models document the OpenAI-compatible proxy endpoints. The actual request handling
//! is done by the `onwards` routing layer, but these types provide OpenAPI documentation.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// ============================================================================
// Chat Completions
// ============================================================================

/// A message in a chat conversation.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChatMessage {
    /// The role of the message author (system, user, assistant, tool, function).
    pub role: String,

    /// The content of the message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,

    /// The name of the author (for function/tool messages).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Tool calls made by the assistant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,

    /// The ID of the tool call this message is responding to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// A tool call made by the model.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ToolCall {
    /// The ID of the tool call.
    pub id: String,

    /// The type of tool (currently only "function").
    #[serde(rename = "type")]
    pub call_type: String,

    /// The function that was called.
    pub function: FunctionCall,
}

/// A function call within a tool call.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FunctionCall {
    /// The name of the function to call.
    pub name: String,

    /// The arguments to pass to the function, as a JSON string.
    pub arguments: String,
}

/// Request body for chat completions.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChatCompletionRequest {
    /// ID of the model to use.
    pub model: String,

    /// A list of messages comprising the conversation so far.
    pub messages: Vec<ChatMessage>,

    /// What sampling temperature to use, between 0 and 2.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    /// An alternative to sampling with temperature, called nucleus sampling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,

    /// How many chat completion choices to generate for each input message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<i32>,

    /// If set, partial message deltas will be sent as server-sent events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,

    /// Up to 4 sequences where the API will stop generating further tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,

    /// The maximum number of tokens to generate in the chat completion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i32>,

    /// Number between -2.0 and 2.0. Positive values penalize new tokens based on
    /// whether they appear in the text so far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,

    /// Number between -2.0 and 2.0. Positive values penalize new tokens based on
    /// their existing frequency in the text so far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,

    /// A unique identifier representing your end-user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,

    /// A list of tools the model may call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,

    /// Controls which (if any) tool is called by the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
}

/// A tool that the model may call.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Tool {
    /// The type of tool (currently only "function").
    #[serde(rename = "type")]
    pub tool_type: String,

    /// The function definition.
    pub function: FunctionDefinition,
}

/// Definition of a function that can be called by the model.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FunctionDefinition {
    /// The name of the function.
    pub name: String,

    /// A description of what the function does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// The parameters the function accepts, as a JSON Schema object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

/// Response from chat completions.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChatCompletionResponse {
    /// A unique identifier for the chat completion.
    pub id: String,

    /// The object type, always "chat.completion".
    pub object: String,

    /// The Unix timestamp of when the chat completion was created.
    pub created: i64,

    /// The model used for the chat completion.
    pub model: String,

    /// A list of chat completion choices.
    pub choices: Vec<ChatChoice>,

    /// Usage statistics for the completion request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,

    /// The system fingerprint of the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<String>,
}

/// A chat completion choice.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChatChoice {
    /// The index of the choice in the list of choices.
    pub index: i32,

    /// The chat completion message generated by the model.
    pub message: ChatMessage,

    /// The reason the model stopped generating tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// Token usage statistics.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Usage {
    /// Number of tokens in the prompt.
    pub prompt_tokens: i32,

    /// Number of tokens in the generated completion.
    pub completion_tokens: i32,

    /// Total number of tokens used in the request.
    pub total_tokens: i32,
}

// ============================================================================
// Embeddings
// ============================================================================

/// Request body for embeddings.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EmbeddingRequest {
    /// ID of the model to use.
    pub model: String,

    /// Input text to embed. Can be a string or array of strings.
    pub input: EmbeddingInput,

    /// The format to return the embeddings in ("float" or "base64").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding_format: Option<String>,

    /// The number of dimensions the resulting output embeddings should have.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<i32>,

    /// A unique identifier representing your end-user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

/// Input for embedding requests - can be a single string or array of strings.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum EmbeddingInput {
    /// A single string to embed.
    Single(String),
    /// An array of strings to embed.
    Multiple(Vec<String>),
}

/// Response from embeddings.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EmbeddingResponse {
    /// The object type, always "list".
    pub object: String,

    /// The list of embeddings generated.
    pub data: Vec<EmbeddingData>,

    /// The model used for generating embeddings.
    pub model: String,

    /// Usage statistics for the request.
    pub usage: EmbeddingUsage,
}

/// A single embedding result.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EmbeddingData {
    /// The object type, always "embedding".
    pub object: String,

    /// The index of this embedding in the list.
    pub index: i32,

    /// The embedding vector.
    pub embedding: Vec<f32>,
}

/// Usage statistics for embedding requests.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EmbeddingUsage {
    /// Number of tokens in the input.
    pub prompt_tokens: i32,

    /// Total number of tokens used.
    pub total_tokens: i32,
}

// ============================================================================
// Models List
// ============================================================================

/// Response for listing available models.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ModelsListResponse {
    /// The object type, always "list".
    pub object: String,

    /// The list of available models.
    pub data: Vec<ModelObject>,
}

/// A model object.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ModelObject {
    /// The model identifier.
    pub id: String,

    /// The object type, always "model".
    pub object: String,

    /// The Unix timestamp of when the model was created.
    pub created: i64,

    /// The organization that owns the model.
    pub owned_by: String,
}

// ============================================================================
// Error Response
// ============================================================================

/// OpenAI-compatible error response.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OpenAIErrorResponse {
    /// The error details.
    pub error: OpenAIError,
}

/// OpenAI-compatible error details.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OpenAIError {
    /// The error message.
    pub message: String,

    /// The type of error.
    #[serde(rename = "type")]
    pub error_type: String,

    /// The parameter that caused the error, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,

    /// The error code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}
