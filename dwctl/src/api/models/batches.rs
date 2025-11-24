//! API request/response models for batch processing.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::{IntoParams, ToSchema};

/// Batch-level errors
#[derive(Debug, Serialize, Deserialize, ToSchema, Clone)]
pub struct BatchErrors {
    /// Array of error details
    pub data: Vec<BatchError>,
}

/// Individual error in a batch
#[derive(Debug, Serialize, Deserialize, ToSchema, Clone)]
pub struct BatchError {
    /// An error code identifying the error type
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,

    /// The line number of the input file where the error occurred, if applicable
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i32>,

    /// A human-readable message providing more details about the error
    pub message: String,

    /// The name of the parameter that caused the error, if applicable
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
}

/// Request body for creating a batch
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreateBatchRequest {
    /// The ID of an uploaded file that contains requests for the new batch
    pub input_file_id: String,

    /// The endpoint to be used for all requests in the batch
    /// Currently /v1/chat/completions, /v1/embeddings, /v1/completions, and /v1/moderations are supported
    pub endpoint: String,

    /// The time frame within which the batch should be processed (currently only "24h" is supported)
    pub completion_window: String,

    /// Optional metadata (up to 16 key-value pairs)
    #[serde(default)]
    pub metadata: Option<HashMap<String, String>>,
}

/// Batch object response (OpenAI-compatible)
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct BatchResponse {
    pub id: String,

    #[serde(rename = "object")]
    pub object_type: BatchObjectType,

    pub endpoint: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<BatchErrors>,

    pub input_file_id: String,

    pub completion_window: String,

    pub status: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_file_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_file_id: Option<String>,

    pub created_at: i64, // Unix timestamp

    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_progress_at: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub finalizing_at: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
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
}

/// Object type - always "batch"
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum BatchObjectType {
    Batch,
}

/// Request counts for a batch
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct RequestCounts {
    pub total: i64,
    pub completed: i64,
    pub failed: i64,
}

/// Response for batch list
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct BatchListResponse {
    #[serde(rename = "object")]
    pub object_type: ListObjectType,

    pub data: Vec<BatchResponse>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_id: Option<String>,

    pub has_more: bool,
}

/// Object type for lists - always "list"
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ListObjectType {
    List,
}

use super::pagination::CursorPagination;

/// Query parameters for listing batches
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct ListBatchesQuery {
    /// Pagination parameters
    #[serde(flatten)]
    #[param(inline)]
    pub pagination: CursorPagination,
}
