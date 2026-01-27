//! API request/response models for batch processing.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::{IntoParams, ToSchema};

/// Batch-level errors
#[derive(Debug, Serialize, Deserialize, ToSchema, Clone)]
#[schema(example = json!({
    "data": [{
        "code": "invalid_request",
        "line": 5,
        "message": "Invalid JSON on line 5"
    }]
}))]
pub struct BatchErrors {
    /// Array of error details
    pub data: Vec<BatchError>,
}

/// Individual error in a batch
#[derive(Debug, Serialize, Deserialize, ToSchema, Clone)]
#[schema(example = json!({
    "code": "invalid_request",
    "line": 5,
    "message": "Invalid JSON on line 5"
}))]
pub struct BatchError {
    /// An error code identifying the error type
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "invalid_request")]
    pub code: Option<String>,

    /// The line number of the input file where the error occurred, if applicable
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 5)]
    pub line: Option<i32>,

    /// A human-readable message providing more details about the error
    #[schema(example = "Invalid JSON on line 5")]
    pub message: String,

    /// The name of the parameter that caused the error, if applicable
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
}

/// Request body for creating a batch
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "input_file_id": "file-abc123",
    "endpoint": "/v1/chat/completions",
    "completion_window": "24h"
}))]
pub struct CreateBatchRequest {
    /// The ID of an uploaded file that contains requests for the new batch
    #[schema(example = "file-abc123")]
    pub input_file_id: String,

    /// The endpoint to be used for all requests in the batch
    /// Currently /v1/chat/completions, /v1/embeddings, /v1/completions, and /v1/moderations are supported
    #[schema(example = "/v1/chat/completions")]
    pub endpoint: String,

    /// The time frame within which the batch should be processed ("1h" or "24h")
    #[schema(example = "24h")]
    pub completion_window: String,

    /// Optional metadata (up to 16 key-value pairs)
    #[serde(default)]
    pub metadata: Option<HashMap<String, String>>,
}

/// Request body for retrying specific requests
#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "request_ids": ["req-abc123", "req-def456"]
}))]
pub struct RetryRequestsRequest {
    /// Array of request IDs to retry
    pub request_ids: Vec<String>,
}

/// Batch object response (OpenAI-compatible)
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "id": "batch-abc123",
    "object": "batch",
    "endpoint": "/v1/chat/completions",
    "input_file_id": "file-abc123",
    "completion_window": "24h",
    "status": "completed",
    "output_file_id": "file-xyz789",
    "created_at": 1703187200,
    "completed_at": 1703190800,
    "request_counts": {
        "total": 100,
        "completed": 98,
        "failed": 2
    }
}))]
pub struct BatchResponse {
    #[schema(example = "batch-abc123")]
    pub id: String,

    #[serde(rename = "object")]
    pub object_type: BatchObjectType,

    #[schema(example = "/v1/chat/completions")]
    pub endpoint: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<BatchErrors>,

    #[schema(example = "file-abc123")]
    pub input_file_id: String,

    #[schema(example = "24h")]
    pub completion_window: String,

    #[schema(example = "completed")]
    pub status: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "file-xyz789")]
    pub output_file_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_file_id: Option<String>,

    #[schema(example = 1703187200)]
    pub created_at: i64, // Unix timestamp

    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_progress_at: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub finalizing_at: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 1703190800)]
    pub completed_at: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_at: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub expired_at: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancelling_at: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancelled_at: Option<i64>,

    pub request_counts: RequestCounts,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,

    /// Aggregated analytics metrics (only included when requested via `include=analytics`)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analytics: Option<BatchAnalytics>,
}

/// Aggregated analytics metrics for batch requests
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "total_requests": 100,
    "total_prompt_tokens": 50000,
    "total_completion_tokens": 25000,
    "total_tokens": 75000,
    "avg_duration_ms": 1250.5,
    "avg_ttfb_ms": 150.2,
    "total_cost": "0.75"
}))]
pub struct BatchAnalytics {
    /// Total number of requests with analytics data
    #[schema(example = 100)]
    pub total_requests: i64,

    /// Total prompt tokens across all requests
    #[schema(example = 50000)]
    pub total_prompt_tokens: i64,

    /// Total completion tokens across all requests
    #[schema(example = 25000)]
    pub total_completion_tokens: i64,

    /// Total tokens (prompt + completion) across all requests
    #[schema(example = 75000)]
    pub total_tokens: i64,

    /// Average request duration in milliseconds
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 1250.5)]
    pub avg_duration_ms: Option<f64>,

    /// Average time to first byte in milliseconds
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 150.2)]
    pub avg_ttfb_ms: Option<f64>,

    /// Total cost in credits (if pricing is available)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "0.75")]
    pub total_cost: Option<String>,
}

/// Object type - always "batch"
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum BatchObjectType {
    Batch,
}

/// Request counts for a batch
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "total": 100,
    "completed": 98,
    "failed": 2
}))]
pub struct RequestCounts {
    #[schema(example = 100)]
    pub total: i64,
    #[schema(example = 98)]
    pub completed: i64,
    #[schema(example = 2)]
    pub failed: i64,
}

/// Response for batch list
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "object": "list",
    "data": [{
        "id": "batch-abc123",
        "object": "batch",
        "endpoint": "/v1/chat/completions",
        "input_file_id": "file-abc123",
        "completion_window": "24h",
        "status": "completed",
        "created_at": 1703187200,
        "request_counts": {"total": 100, "completed": 98, "failed": 2}
    }],
    "first_id": "batch-abc123",
    "last_id": "batch-abc123",
    "has_more": false
}))]
pub struct BatchListResponse {
    #[serde(rename = "object")]
    pub object_type: ListObjectType,

    pub data: Vec<BatchResponse>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "batch-abc123")]
    pub first_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "batch-abc123")]
    pub last_id: Option<String>,

    #[schema(example = false)]
    pub has_more: bool,
}

/// Object type for lists - always "list"
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ListObjectType {
    List,
}

use super::pagination::CursorPagination;
use crate::api::models::Pagination;

/// Query parameters for listing batches
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct ListBatchesQuery {
    /// Pagination parameters
    #[serde(flatten)]
    #[param(inline)]
    pub pagination: CursorPagination,

    /// Search query to filter batches by endpoint or input filename (case-insensitive substring match)
    pub search: Option<String>,

    /// Comma-separated list of related resources to include. Supported: "analytics"
    #[param(example = "analytics")]
    pub include: Option<String>,
}

/// Query parameters for batch results
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct BatchResultsQuery {
    /// Pagination parameters (limit and skip)
    #[serde(flatten)]
    #[param(inline)]
    pub pagination: Pagination,

    /// Search query to filter by custom_id (case-insensitive substring match)
    pub search: Option<String>,

    /// Filter by request status (completed, failed, pending, in_progress)
    pub status: Option<String>,
}
