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
#[schema(example = json!({
    "role": "user",
    "content": "What is a doubleword?"
}))]
pub struct ChatMessage {
    /// The role of the message author (system, user, assistant, tool, function).
    #[schema(example = "user")]
    pub role: String,

    /// The content of the message.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "What is a doubleword?")]
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
#[schema(example = json!({
    "id": "call_abc123",
    "type": "function",
    "function": {
        "name": "get_weather",
        "arguments": "{\"location\": \"San Francisco\"}"
    }
}))]
pub struct ToolCall {
    /// The ID of the tool call.
    #[schema(example = "call_abc123")]
    pub id: String,

    /// The type of tool (currently only "function").
    #[serde(rename = "type")]
    #[schema(example = "function")]
    pub call_type: String,

    /// The function that was called.
    pub function: FunctionCall,
}

/// A function call within a tool call.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "name": "get_weather",
    "arguments": "{\"location\": \"San Francisco\"}"
}))]
pub struct FunctionCall {
    /// The name of the function to call.
    #[schema(example = "get_weather")]
    pub name: String,

    /// The arguments to pass to the function, as a JSON string.
    #[schema(example = "{\"location\": \"San Francisco\"}")]
    pub arguments: String,
}

/// Request body for chat completions.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "model": "Qwen/Qwen3-30B-A3B-FP8",
    "messages": [
        {"role": "system", "content": "You are a helpful assistant."},
        {"role": "user", "content": "What is a doubleword?"}
    ],
    "temperature": 0.7,
    "max_tokens": 256
}))]
pub struct ChatCompletionRequest {
    /// ID of the model to use.
    #[schema(example = "Qwen/Qwen3-30B-A3B-FP8")]
    pub model: String,

    /// A list of messages comprising the conversation so far.
    pub messages: Vec<ChatMessage>,

    /// What sampling temperature to use, between 0 and 2.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 0.7)]
    pub temperature: Option<f32>,

    /// An alternative to sampling with temperature, called nucleus sampling.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 1.0)]
    pub top_p: Option<f32>,

    /// How many chat completion choices to generate for each input message.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 1)]
    pub n: Option<i32>,

    /// If set, partial message deltas will be sent as server-sent events.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = false)]
    pub stream: Option<bool>,

    /// Up to 4 sequences where the API will stop generating further tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,

    /// The maximum number of tokens to generate in the chat completion.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 256)]
    pub max_tokens: Option<i32>,

    /// Number between -2.0 and 2.0. Positive values penalize new tokens based on
    /// whether they appear in the text so far.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 0.0)]
    pub presence_penalty: Option<f32>,

    /// Number between -2.0 and 2.0. Positive values penalize new tokens based on
    /// their existing frequency in the text so far.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 0.0)]
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
#[schema(example = json!({
    "type": "function",
    "function": {
        "name": "get_weather",
        "description": "Get the current weather in a location",
        "parameters": {
            "type": "object",
            "properties": {
                "location": {"type": "string", "description": "City name"}
            },
            "required": ["location"]
        }
    }
}))]
pub struct Tool {
    /// The type of tool (currently only "function").
    #[serde(rename = "type")]
    #[schema(example = "function")]
    pub tool_type: String,

    /// The function definition.
    pub function: FunctionDefinition,
}

/// Definition of a function that can be called by the model.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "name": "get_weather",
    "description": "Get the current weather in a location",
    "parameters": {
        "type": "object",
        "properties": {
            "location": {"type": "string", "description": "City name"}
        },
        "required": ["location"]
    }
}))]
pub struct FunctionDefinition {
    /// The name of the function.
    #[schema(example = "get_weather")]
    pub name: String,

    /// A description of what the function does.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "Get the current weather in a location")]
    pub description: Option<String>,

    /// The parameters the function accepts, as a JSON Schema object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

/// Response from chat completions.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "id": "chatcmpl-abc123",
    "object": "chat.completion",
    "created": 1703187200,
    "model": "Qwen/Qwen3-30B-A3B-FP8",
    "choices": [{
        "index": 0,
        "message": {
            "role": "assistant",
            "content": "A doubleword is a data unit that is twice the size of a standard word in computer architecture, typically 32 or 64 bits depending on the system."
        },
        "finish_reason": "stop"
    }],
    "usage": {
        "prompt_tokens": 24,
        "completion_tokens": 36,
        "total_tokens": 60
    }
}))]
pub struct ChatCompletionResponse {
    /// A unique identifier for the chat completion.
    #[schema(example = "chatcmpl-abc123")]
    pub id: String,

    /// The object type, always "chat.completion".
    #[schema(example = "chat.completion")]
    pub object: String,

    /// The Unix timestamp of when the chat completion was created.
    #[schema(example = 1703187200)]
    pub created: i64,

    /// The model used for the chat completion.
    #[schema(example = "Qwen/Qwen3-30B-A3B-FP8")]
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
#[schema(example = json!({
    "index": 0,
    "message": {
        "role": "assistant",
        "content": "A doubleword is a data unit that is twice the size of a standard word in computer architecture."
    },
    "finish_reason": "stop"
}))]
pub struct ChatChoice {
    /// The index of the choice in the list of choices.
    #[schema(example = 0)]
    pub index: i32,

    /// The chat completion message generated by the model.
    pub message: ChatMessage,

    /// The reason the model stopped generating tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "stop")]
    pub finish_reason: Option<String>,
}

/// Token usage statistics.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "prompt_tokens": 24,
    "completion_tokens": 36,
    "total_tokens": 60
}))]
pub struct Usage {
    /// Number of tokens in the prompt.
    #[schema(example = 24)]
    pub prompt_tokens: i32,

    /// Number of tokens in the generated completion.
    #[schema(example = 36)]
    pub completion_tokens: i32,

    /// Total number of tokens used in the request.
    #[schema(example = 60)]
    pub total_tokens: i32,
}

// ============================================================================
// Embeddings
// ============================================================================

/// Request body for embeddings.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "model": "Qwen/Qwen3-30B-A3B-FP8",
    "input": "What is a doubleword?"
}))]
pub struct EmbeddingRequest {
    /// ID of the model to use.
    #[schema(example = "Qwen/Qwen3-30B-A3B-FP8")]
    pub model: String,

    /// Input text to embed. Can be a string or array of strings.
    pub input: EmbeddingInput,

    /// The format to return the embeddings in ("float" or "base64").
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "float")]
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
#[schema(example = "What is a doubleword?")]
pub enum EmbeddingInput {
    /// A single string to embed.
    Single(String),
    /// An array of strings to embed.
    Multiple(Vec<String>),
}

/// Response from embeddings.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "object": "list",
    "data": [{
        "object": "embedding",
        "index": 0,
        "embedding": [0.0023, -0.0134, 0.0256]
    }],
    "model": "Qwen/Qwen3-30B-A3B-FP8",
    "usage": {
        "prompt_tokens": 6,
        "total_tokens": 6
    }
}))]
pub struct EmbeddingResponse {
    /// The object type, always "list".
    #[schema(example = "list")]
    pub object: String,

    /// The list of embeddings generated.
    pub data: Vec<EmbeddingData>,

    /// The model used for generating embeddings.
    #[schema(example = "Qwen/Qwen3-30B-A3B-FP8")]
    pub model: String,

    /// Usage statistics for the request.
    pub usage: EmbeddingUsage,
}

/// A single embedding result.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "object": "embedding",
    "index": 0,
    "embedding": [0.0023, -0.0134, 0.0256]
}))]
pub struct EmbeddingData {
    /// The object type, always "embedding".
    #[schema(example = "embedding")]
    pub object: String,

    /// The index of this embedding in the list.
    #[schema(example = 0)]
    pub index: i32,

    /// The embedding vector.
    pub embedding: Vec<f32>,
}

/// Usage statistics for embedding requests.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "prompt_tokens": 6,
    "total_tokens": 6
}))]
pub struct EmbeddingUsage {
    /// Number of tokens in the input.
    #[schema(example = 6)]
    pub prompt_tokens: i32,

    /// Total number of tokens used.
    #[schema(example = 6)]
    pub total_tokens: i32,
}

// ============================================================================
// Models List
// ============================================================================

/// Response for listing available models.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "object": "list",
    "data": [{
        "id": "Qwen/Qwen3-30B-A3B-FP8",
        "object": "model",
        "created": 1703187200,
        "owned_by": "qwen"
    }]
}))]
pub struct ModelsListResponse {
    /// The object type, always "list".
    #[schema(example = "list")]
    pub object: String,

    /// The list of available models.
    pub data: Vec<ModelObject>,
}

/// A model object.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "id": "Qwen/Qwen3-30B-A3B-FP8",
    "object": "model",
    "created": 1703187200,
    "owned_by": "qwen"
}))]
pub struct ModelObject {
    /// The model identifier.
    #[schema(example = "Qwen/Qwen3-30B-A3B-FP8")]
    pub id: String,

    /// The object type, always "model".
    #[schema(example = "model")]
    pub object: String,

    /// The Unix timestamp of when the model was created.
    #[schema(example = 1703187200)]
    pub created: i64,

    /// The organization that owns the model.
    #[schema(example = "qwen")]
    pub owned_by: String,
}

// ============================================================================
// Error Response
// ============================================================================

/// OpenAI-compatible error response.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "error": {
        "message": "Invalid API key provided",
        "type": "authentication_error",
        "code": "invalid_api_key"
    }
}))]
pub struct OpenAIErrorResponse {
    /// The error details.
    pub error: OpenAIError,
}

/// OpenAI-compatible error details.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "message": "Invalid API key provided",
    "type": "authentication_error",
    "code": "invalid_api_key"
}))]
pub struct OpenAIError {
    /// The error message.
    #[schema(example = "Invalid API key provided")]
    pub message: String,

    /// The type of error.
    #[serde(rename = "type")]
    #[schema(example = "authentication_error")]
    pub error_type: String,

    /// The parameter that caused the error, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,

    /// The error code.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "invalid_api_key")]
    pub code: Option<String>,
}

// ============================================================================
// Responses API
// ============================================================================

/// Input for response requests - can be a single string or array of messages.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
#[schema(example = "What is a doubleword?")]
pub enum ResponseInput {
    /// A single string input.
    Single(String),
    /// An array of messages (chat-style conversation).
    Messages(Vec<ChatMessage>),
}

/// A single item in the response output array.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "type": "message",
    "role": "assistant",
    "content": "A doubleword is a data unit that is twice the size of a standard word in computer architecture."
}))]
pub struct ResponseItem {
    /// The type of item (e.g., "message", "function_call").
    #[serde(rename = "type")]
    #[schema(example = "message")]
    pub item_type: String,

    /// The role of the message (for message-type items).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "assistant")]
    pub role: Option<String>,

    /// The content of the message (for message-type items).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "A doubleword is a data unit that is twice the size of a standard word.")]
    pub content: Option<String>,

    /// Tool calls made by the model (for message-type items with tool calls).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// Request body for creating a response.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "model": "gpt-4o",
    "input": "What is a doubleword?",
    "temperature": 0.7,
    "max_output_tokens": 256
}))]
pub struct ResponseRequest {
    /// ID of the model to use.
    #[schema(example = "gpt-4o")]
    pub model: String,

    /// The input to generate a response for. Can be a string or array of messages.
    pub input: ResponseInput,

    /// System instructions for the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "You are a helpful assistant.")]
    pub instructions: Option<String>,

    /// What sampling temperature to use, between 0 and 2.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 0.7)]
    pub temperature: Option<f32>,

    /// An alternative to sampling with temperature, called nucleus sampling.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 1.0)]
    pub top_p: Option<f32>,

    /// The maximum number of tokens to generate in the response.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 256)]
    pub max_output_tokens: Option<i32>,

    /// Output types that you would like the model to generate (e.g., ["text"], ["text", "audio"]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modalities: Option<Vec<String>>,

    /// Constrains effort on reasoning. Supported values: "none", "minimal", "low", "medium", "high", "xhigh".
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "medium")]
    pub reasoning_effort: Option<String>,

    /// A list of tools the model may call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,

    /// Controls which (if any) tool is called by the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,

    /// Whether to enable parallel function calling during tool use.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = true)]
    pub parallel_tool_calls: Option<bool>,

    /// If set, partial message deltas will be sent as server-sent events.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = false)]
    pub stream: Option<bool>,

    /// Options for streaming response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<serde_json::Value>,

    /// The ID of a previous response to continue from (for stateful conversations).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,

    /// Developer-defined tags and values for organizing responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,

    /// Include encrypted reasoning content for rehydration on subsequent requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<String>,

    /// Text output configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<serde_json::Value>,

    /// Reasoning configuration for controlling reasoning behavior.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<serde_json::Value>,

    /// How to handle context window overflow ("auto" or "disabled").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<String>,

    /// Whether to store this response for future reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = false)]
    pub store: Option<bool>,

    /// A unique identifier representing your end-user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,

    /// Up to 4 sequences where the API will stop generating further tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,

    /// Number between -2.0 and 2.0. Positive values penalize new tokens based on
    /// whether they appear in the text so far.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 0.0)]
    pub presence_penalty: Option<f32>,

    /// Number between -2.0 and 2.0. Positive values penalize new tokens based on
    /// their existing frequency in the text so far.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 0.0)]
    pub frequency_penalty: Option<f32>,
}

/// Response from creating a response.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "id": "resp-abc123",
    "object": "response",
    "created_at": 1703187200,
    "completed_at": 1703187205,
    "model": "gpt-4o",
    "status": "completed",
    "output": [{
        "type": "message",
        "role": "assistant",
        "content": "A doubleword is a data unit that is twice the size of a standard word in computer architecture."
    }],
    "usage": {
        "prompt_tokens": 10,
        "completion_tokens": 25,
        "total_tokens": 35
    },
    "temperature": 0.7,
    "top_p": 1.0
}))]
pub struct ResponseObject {
    /// A unique identifier for the response.
    #[schema(example = "resp-abc123")]
    pub id: String,

    /// The object type, always "response".
    #[schema(example = "response")]
    pub object: String,

    /// The Unix timestamp of when the response was created.
    #[schema(example = 1703187200)]
    pub created_at: i64,

    /// The Unix timestamp of when the response was completed.
    #[schema(example = 1703187205)]
    pub completed_at: i64,

    /// The model used for generating the response.
    #[schema(example = "gpt-4o")]
    pub model: String,

    /// The status of the response. Can be "completed", "incomplete", "cancelled", or "failed".
    #[schema(example = "completed")]
    pub status: String,

    /// The output items generated by the model.
    pub output: Vec<ResponseItem>,

    /// Usage statistics for the response request.
    pub usage: Usage,

    /// The temperature used for sampling (echoed from request).
    #[schema(example = 0.7)]
    pub temperature: f32,

    /// The nucleus sampling parameter used (echoed from request).
    #[schema(example = 1.0)]
    pub top_p: f32,

    /// Developer-defined tags and values.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}
