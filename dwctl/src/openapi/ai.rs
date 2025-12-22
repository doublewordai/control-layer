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
                        .description(Some("Enter your API key"))
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

/// Create a chat completion (proxied to upstream provider).
#[utoipa::path(
    post,
    path = "/chat/completions",
    tag = "chat",
    summary = "Create chat completion",
    description = "Creates a model response for the given chat conversation. This endpoint is proxied to the configured upstream inference provider.",
    request_body = extra_types::ChatCompletionRequest,
    responses(
        (status = 200, description = "Chat completion response", body = extra_types::ChatCompletionResponse),
        (status = 400, description = "Bad request", body = extra_types::OpenAIErrorResponse),
        (status = 401, description = "Unauthorized - invalid or missing API key", body = extra_types::OpenAIErrorResponse),
        (status = 402, description = "Payment required - insufficient credits", body = extra_types::OpenAIErrorResponse),
        (status = 403, description = "Forbidden - no access to requested model", body = extra_types::OpenAIErrorResponse),
        (status = 404, description = "Model not found", body = extra_types::OpenAIErrorResponse),
        (status = 429, description = "Rate limit exceeded", body = extra_types::OpenAIErrorResponse),
        (status = 500, description = "Internal server error", body = extra_types::OpenAIErrorResponse),
    ),
    security(("BearerAuth" = []))
)]
#[allow(unused)]
fn chat_completions() {}

/// Create embeddings (proxied to upstream provider).
#[utoipa::path(
    post,
    path = "/embeddings",
    tag = "embeddings",
    summary = "Create embeddings",
    description = "Creates an embedding vector representing the input text. This endpoint is proxied to the configured upstream inference provider.",
    request_body = extra_types::EmbeddingRequest,
    responses(
        (status = 200, description = "Embedding response", body = extra_types::EmbeddingResponse),
        (status = 400, description = "Bad request", body = extra_types::OpenAIErrorResponse),
        (status = 401, description = "Unauthorized - invalid or missing API key", body = extra_types::OpenAIErrorResponse),
        (status = 402, description = "Payment required - insufficient credits", body = extra_types::OpenAIErrorResponse),
        (status = 403, description = "Forbidden - no access to requested model", body = extra_types::OpenAIErrorResponse),
        (status = 404, description = "Model not found", body = extra_types::OpenAIErrorResponse),
        (status = 429, description = "Rate limit exceeded", body = extra_types::OpenAIErrorResponse),
        (status = 500, description = "Internal server error", body = extra_types::OpenAIErrorResponse),
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
    description = "Lists the currently available models, and provides basic information about each one.",
    responses(
        (status = 200, description = "List of available models", body = extra_types::ModelsListResponse),
        (status = 401, description = "Unauthorized - invalid or missing API key", body = extra_types::OpenAIErrorResponse),
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
    description = "Retrieves a model instance, providing basic information about the model.",
    params(
        ("model" = String, Path, description = "The ID of the model to retrieve")
    ),
    responses(
        (status = 200, description = "Model information", body = extra_types::ModelObject),
        (status = 401, description = "Unauthorized - invalid or missing API key", body = extra_types::OpenAIErrorResponse),
        (status = 404, description = "Model not found", body = extra_types::OpenAIErrorResponse),
    ),
    security(("BearerAuth" = []))
)]
#[allow(unused)]
fn get_model() {}

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
        (name = "chat", description = "Chat completions API"),
        (name = "embeddings", description = "Embeddings API"),
        (name = "models", description = "Models API"),
        (name = "files", description = "File management for batch processing"),
        (name = "batches", description = "Batch processing API"),
    ),
    info(
        title = "AI API",
        version = "1.0.0",
        description = "OpenAI-compatible API for chat completions, embeddings, and batch processing",
    ),
)]
pub struct AiApiDoc;
