//! OpenAPI documentation for the AI API (OpenAI-compatible endpoints).
//!
//! This module defines the OpenAPI spec for `/ai/v1/*` endpoints, including:
//! - Proxied inference endpoints (chat/completions, embeddings, models)
//! - Batch processing endpoints (files, batches)

use utoipa::{
    Modify, OpenApi,
    openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme},
};

use super::extra_types;
use crate::api;

/// Security scheme for the AI API (Bearer token only).
struct AiSecurityAddon;

impl Modify for AiSecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.security_schemes.insert(
                "BearerAuth".to_string(),
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("API Key")
                        .description(Some(
                            "API key authentication. Include your key in the `Authorization` header:\n\n\
                            ```\nAuthorization: Bearer YOUR_API_KEY\n```\n\n\
                            API keys can be created and managed in the dashboard.",
                        ))
                        .build(),
                ),
            );
        }
    }
}

// ============================================================================
// Stub handlers for proxied endpoints (documentation only)
// ============================================================================
//
// These functions exist solely to generate OpenAPI documentation for endpoints
// that are actually handled by the `onwards` routing layer.

/// Create a chat completion.
#[utoipa::path(
    post,
    path = "/chat/completions",
    tag = "chat",
    summary = "Create chat completion",
    description = "Creates a model response for the given chat conversation.

The conversation is provided as an array of messages, where each message has a `role` (system, user, assistant, or tool) and `content`.

Set `stream: true` to receive partial responses as server-sent events.",
    request_body = extra_types::ChatCompletionRequest,
    responses(
        (status = 200, description = "Chat completion generated successfully. When streaming, returns a series of SSE events.", body = extra_types::ChatCompletionResponse),
        (status = 400, description = "Invalid request — check that your messages array is properly formatted and all required fields are present.", body = extra_types::OpenAIErrorResponse),
        (status = 401, description = "Invalid or missing API key. Ensure your `Authorization` header is set to `Bearer YOUR_API_KEY`.", body = extra_types::OpenAIErrorResponse),
        (status = 402, description = "Insufficient credits. Top up your account to continue making requests.", body = extra_types::OpenAIErrorResponse),
        (status = 403, description = "Your API key does not have access to the requested model.", body = extra_types::OpenAIErrorResponse),
        (status = 404, description = "The specified model does not exist. Use `GET /models` to list available models.", body = extra_types::OpenAIErrorResponse),
        (status = 429, description = "Rate limit exceeded. Back off and retry after a short delay.", body = extra_types::OpenAIErrorResponse),
        (status = 500, description = "An unexpected error occurred. Retry the request or contact support if the issue persists.", body = extra_types::OpenAIErrorResponse),
    ),
    security(("BearerAuth" = []))
)]
#[allow(unused)]
fn chat_completions() {}

/// Create embeddings.
#[utoipa::path(
    post,
    path = "/embeddings",
    tag = "embeddings",
    summary = "Create embeddings",
    description = "Creates embedding vectors representing the input text.

Input can be a single string or an array of strings. Each input produces one embedding vector in the response.",
    request_body = extra_types::EmbeddingRequest,
    responses(
        (status = 200, description = "Embeddings generated successfully. Each input string has a corresponding entry in the `data` array.", body = extra_types::EmbeddingResponse),
        (status = 400, description = "Invalid request — check that your input is a string or array of strings.", body = extra_types::OpenAIErrorResponse),
        (status = 401, description = "Invalid or missing API key. Ensure your `Authorization` header is set to `Bearer YOUR_API_KEY`.", body = extra_types::OpenAIErrorResponse),
        (status = 402, description = "Insufficient credits. Top up your account to continue making requests.", body = extra_types::OpenAIErrorResponse),
        (status = 403, description = "Your API key does not have access to the requested model.", body = extra_types::OpenAIErrorResponse),
        (status = 404, description = "The specified model does not exist. Use `GET /models` to list available models.", body = extra_types::OpenAIErrorResponse),
        (status = 429, description = "Rate limit exceeded. Back off and retry after a short delay.", body = extra_types::OpenAIErrorResponse),
        (status = 500, description = "An unexpected error occurred. Retry the request or contact support if the issue persists.", body = extra_types::OpenAIErrorResponse),
    ),
    security(("BearerAuth" = []))
)]
#[allow(unused)]
fn embeddings() {}

/// List available models.
#[utoipa::path(
    get,
    path = "/models",
    tag = "models",
    summary = "List models",
    description = "Lists the models available to your API key.

The response includes model IDs that can be used in chat completion and embedding requests.",
    responses(
        (status = 200, description = "List of models your API key can access.", body = extra_types::ModelsListResponse),
        (status = 401, description = "Invalid or missing API key. Ensure your `Authorization` header is set to `Bearer YOUR_API_KEY`.", body = extra_types::OpenAIErrorResponse),
    ),
    security(("BearerAuth" = []))
)]
#[allow(unused)]
fn list_models() {}

/// Get a specific model.
#[utoipa::path(
    get,
    path = "/models/{model}",
    tag = "models",
    summary = "Retrieve model",
    description = "Retrieves information about a specific model.",
    params(
        ("model" = String, Path, description = "The model ID (e.g., `gpt-4`, `text-embedding-ada-002`)")
    ),
    responses(
        (status = 200, description = "Model details including ID, owner, and creation timestamp.", body = extra_types::ModelObject),
        (status = 401, description = "Invalid or missing API key. Ensure your `Authorization` header is set to `Bearer YOUR_API_KEY`.", body = extra_types::OpenAIErrorResponse),
        (status = 404, description = "The specified model does not exist or you don't have access to it.", body = extra_types::OpenAIErrorResponse),
    ),
    security(("BearerAuth" = []))
)]
#[allow(unused)]
fn get_model() {}

/// Create a response.
#[utoipa::path(
    post,
    path = "/responses",
    tag = "responses-api",
    summary = "Create response",
    description = "Creates a model response for the given input.

The Responses API is OpenAI's unified API that supersedes Chat Completions for advanced use cases. It provides enhanced capabilities including:

- **Reasoning models** with controllable effort levels via `reasoning_effort`
- **Multimodal support** via `modalities` parameter
- **Stateful conversations** via `previous_response_id` for maintaining context across turns
- **Flexible input** - accepts either a string or array of messages
- **Structured instructions** - separate `instructions` and `input` fields for cleaner semantics

Set `stream: true` to receive partial responses as server-sent events.",
    request_body = extra_types::ResponseRequest,
    responses(
        (status = 200, description = "Response generated successfully. When streaming, returns a series of SSE events.", body = extra_types::ResponseObject),
        (status = 400, description = "Invalid request — check that your input is properly formatted and all required fields are present.", body = extra_types::OpenAIErrorResponse),
        (status = 401, description = "Invalid or missing API key. Ensure your `Authorization` header is set to `Bearer YOUR_API_KEY`.", body = extra_types::OpenAIErrorResponse),
        (status = 402, description = "Insufficient credits. Top up your account to continue making requests.", body = extra_types::OpenAIErrorResponse),
        (status = 403, description = "Your API key does not have access to the requested model.", body = extra_types::OpenAIErrorResponse),
        (status = 404, description = "The specified model does not exist. Use `GET /models` to list available models.", body = extra_types::OpenAIErrorResponse),
        (status = 429, description = "Rate limit exceeded. Back off and retry after a short delay.", body = extra_types::OpenAIErrorResponse),
        (status = 500, description = "An unexpected error occurred. Retry the request or contact support if the issue persists.", body = extra_types::OpenAIErrorResponse),
    ),
    security(("BearerAuth" = []))
)]
#[allow(unused)]
fn create_response() {}

// ============================================================================
// OpenAPI Document
// ============================================================================

#[derive(OpenApi)]
#[openapi(
    servers(
        (url = "/ai/v1", description = "AI API server (OpenAI-compatible)")
    ),
    modifiers(&AiSecurityAddon),
    paths(
        // Proxied inference endpoints (documentation stubs)
        chat_completions,
        embeddings,
        list_models,
        get_model,
        create_response,
        // Batch API endpoints (actual handlers)
        api::handlers::files::upload_file,
        api::handlers::files::list_files,
        api::handlers::files::get_file,
        api::handlers::files::get_file_content,
        api::handlers::files::get_file_cost_estimate,
        api::handlers::files::delete_file,
        api::handlers::batches::create_batch,
        api::handlers::batches::get_batch,
        api::handlers::batches::get_batch_analytics,
        api::handlers::batches::get_batch_results,
        api::handlers::batches::cancel_batch,
        api::handlers::batches::delete_batch,
        api::handlers::batches::retry_failed_batch_requests,
        api::handlers::batches::retry_specific_requests,
        api::handlers::batches::list_batches,
    ),
    components(
        schemas(
            // OpenAI-compatible types
            extra_types::ChatCompletionRequest,
            extra_types::ChatCompletionResponse,
            extra_types::ChatMessage,
            extra_types::ChatChoice,
            extra_types::ToolCall,
            extra_types::FunctionCall,
            extra_types::Tool,
            extra_types::FunctionDefinition,
            extra_types::Usage,
            extra_types::EmbeddingRequest,
            extra_types::EmbeddingResponse,
            extra_types::EmbeddingInput,
            extra_types::EmbeddingData,
            extra_types::EmbeddingUsage,
            extra_types::ModelsListResponse,
            extra_types::ModelObject,
            extra_types::OpenAIErrorResponse,
            extra_types::OpenAIError,
            // Responses API types
            extra_types::ResponseRequest,
            extra_types::ResponseObject,
            extra_types::ResponseInput,
            extra_types::ResponseItem,
            // File/Batch types
            api::models::files::ListFilesQuery,
            api::models::files::FileResponse,
            api::models::files::FileDeleteResponse,
            api::models::files::FileListResponse,
            api::models::files::FileCostEstimate,
            api::models::files::ModelCostBreakdown,
            api::models::files::ObjectType,
            api::models::files::Purpose,
            api::models::files::ListObject,
            api::models::batches::CreateBatchRequest,
            api::models::batches::RetryRequestsRequest,
            api::models::batches::BatchResponse,
            api::models::batches::BatchAnalytics,
            api::models::batches::BatchObjectType,
            api::models::batches::RequestCounts,
            api::models::batches::BatchListResponse,
            api::models::batches::ListObjectType,
            api::models::batches::ListBatchesQuery,
            api::models::batches::BatchErrors,
            api::models::batches::BatchError,
        )
    ),
    tags(
        (name = "files", description = "Upload and manage JSONL files for batch processing.

Each line in the file should be a JSON object with:
- `custom_id` — your identifier for tracking the request
- `method` — HTTP method (POST)
- `url` — endpoint path (e.g., `/v1/chat/completions`)
- `body` — the request payload

[Learn more about the JSONL file format →](https://docs.doubleword.ai/batches/jsonl-files)"),
        (name = "batches", description = "Process large volumes of requests asynchronously.

Batch processing is ideal when you:
- Have many requests that don't need immediate responses
- Want to process data in bulk (e.g., embeddings for a document corpus)
- Are running offline evaluations or data pipelines

Choose your completion window: 1 hour or 24 hours. You can track progress, cancel in-flight batches, and retry failed requests.

[Getting started with the Batch API →](https://docs.doubleword.ai/batches/getting-started-with-batched-api)"),
        (name = "chat", description = "Create model responses for chat conversations.

Supports:
- **Multi-turn dialogue** with conversation history
- **System prompts** to control model behavior
- **Tool calling** for function execution and structured outputs
- **Streaming** for real-time token delivery
- **Sampling parameters** like temperature, top_p, and frequency penalties"),
        (name = "embeddings", description = "Generate vector representations of text.

Use embeddings for:
- **Semantic search** — find content by meaning, not just keywords
- **Clustering** — group similar documents together
- **Classification** — categorize text based on similarity to examples
- **Recommendations** — suggest related content"),
        (name = "models", description = "List and retrieve information about available models.

Use these endpoints to discover which models you have access to and their capabilities."),
        (name = "responses-api", description = "Create model responses with enhanced capabilities.

The Responses API is OpenAI's unified API that supersedes Chat Completions for advanced use cases:

- **Reasoning models** — Control computational effort with `reasoning_effort` parameter
- **Multimodal support** — Generate text, audio, or other modalities via `modalities`
- **Stateful conversations** — Maintain context across turns with `previous_response_id`
- **Flexible input** — Use simple strings or full message arrays
- **Structured instructions** — Separate system instructions from user input

[Learn more about the Responses API →](https://platform.openai.com/docs/api-reference/responses)"),
    ),
    info(
        title = "AI API",
        version = "1.0.0",
        description = "OpenAI-compatible API for chat completions, embeddings, and batch processing.

## Authentication

All endpoints require an API key passed in the `Authorization` header:

```
Authorization: Bearer YOUR_API_KEY
```

## Errors

Errors follow the OpenAI format with `error.message`, `error.type`, and `error.code` fields:

```json
{
  \"error\": {
    \"message\": \"Invalid API key\",
    \"type\": \"authentication_error\",
    \"code\": \"invalid_api_key\"
  }
}
```

## Streaming

Chat completions support streaming responses via `\"stream\": true`. Responses are sent as server-sent events (SSE) with `data:` prefixed JSON chunks.",
    ),
)]
pub struct AiApiDoc;
