//! API request/response models for file management.

use crate::api::models::Pagination;

use super::pagination::CursorPagination;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Query parameters for listing files (OpenAI-compatible cursor-based pagination)
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct ListFilesQuery {
    /// Pagination parameters
    #[serde(flatten)]
    #[param(inline)]
    pub pagination: CursorPagination,

    /// Sort order by created_at (asc or desc, default desc)
    #[serde(default = "default_order")]
    pub order: String,

    /// Only return files with the given purpose
    pub purpose: Option<String>,

    /// Search query to filter files by filename (case-insensitive substring match)
    pub search: Option<String>,

    /// Comma-separated list of related resources to include. Supported: "cost_estimate"
    #[param(example = "cost_estimate")]
    pub include: Option<String>,

    /// Completion window (SLA) for cost estimates (e.g., "24h", "1h"). Only used when include=cost_estimate.
    #[param(example = "24h")]
    pub completion_window: Option<String>,
}

fn default_order() -> String {
    "desc".to_string()
}

/// Query parameters for file content
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct FileContentQuery {
    /// Pagination parameters
    #[serde(flatten)]
    #[param(inline)]
    pub pagination: Pagination,

    /// Search query to filter by custom_id (case-insensitive substring match)
    pub search: Option<String>,
}

/// Query parameters for file cost estimate
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct FileCostEstimateQuery {
    /// Completion window (SLA) for batch processing (e.g., "24h", "1h", "48h")
    /// If not provided, defaults to "24h"
    pub completion_window: Option<String>,
}

/// File object response (OpenAI-compatible)
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "id": "file-abc123",
    "object": "file",
    "bytes": 12045,
    "created_at": 1703187200,
    "filename": "batch_requests.jsonl",
    "purpose": "batch"
}))]
pub struct FileResponse {
    #[schema(example = "file-abc123")]
    pub id: String,
    #[serde(rename = "object")]
    pub object_type: ObjectType,
    #[schema(example = 12045)]
    pub bytes: i64,
    #[schema(example = 1703187200)]
    pub created_at: i64, // Unix timestamp
    #[schema(example = "batch_requests.jsonl")]
    pub filename: String,
    pub purpose: Purpose,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>, // Unix timestamp
    /// Cost estimate for this file (only included when requested via include=cost_estimate)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_estimate: Option<FileCostEstimateSummary>,
}

/// Summary cost estimate for a file (embedded in file response)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "total_requests": 100,
    "total_estimated_input_tokens": 50000,
    "total_estimated_output_tokens": 25000,
    "total_estimated_cost": "0.75"
}))]
pub struct FileCostEstimateSummary {
    #[schema(example = 100)]
    pub total_requests: i64,
    #[schema(example = 50000)]
    pub total_estimated_input_tokens: i64,
    #[schema(example = 25000)]
    pub total_estimated_output_tokens: i64,
    /// Total cost as string to preserve decimal precision
    #[schema(example = "0.75")]
    pub total_estimated_cost: String,
}

/// Object type - always "file"
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ObjectType {
    File,
}

/// Purpose for file
#[derive(Debug, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    Batch,
    BatchOutput,
    BatchError,
}

/// Response for file deletion
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "id": "file-abc123",
    "object": "file",
    "deleted": true
}))]
pub struct FileDeleteResponse {
    #[schema(example = "file-abc123")]
    pub id: String,
    #[serde(rename = "object")]
    pub object_type: ObjectType,
    #[schema(example = true)]
    pub deleted: bool,
}

/// Response for file list
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "object": "list",
    "data": [{
        "id": "file-abc123",
        "object": "file",
        "bytes": 12045,
        "created_at": 1703187200,
        "filename": "batch_requests.jsonl",
        "purpose": "batch"
    }],
    "first_id": "file-abc123",
    "last_id": "file-abc123",
    "has_more": false
}))]
pub struct FileListResponse {
    #[serde(rename = "object")]
    pub object_type: ListObject,
    pub data: Vec<FileResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "file-abc123")]
    pub first_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "file-abc123")]
    pub last_id: Option<String>,
    #[schema(example = false)]
    pub has_more: bool,
}

/// Object type for lists - always "list"
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ListObject {
    List,
}

/// Per-model cost breakdown
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "model": "Qwen/Qwen3-30B-A3B-FP8",
    "request_count": 100,
    "estimated_input_tokens": 50000,
    "estimated_output_tokens": 25000,
    "estimated_cost": "0.75"
}))]
pub struct ModelCostBreakdown {
    #[schema(example = "Qwen/Qwen3-30B-A3B-FP8")]
    pub model: String,
    #[schema(example = 100)]
    pub request_count: i64,
    #[schema(example = 50000)]
    pub estimated_input_tokens: i64,
    #[schema(example = 25000)]
    pub estimated_output_tokens: i64,
    /// Cost as string to preserve decimal precision
    #[schema(example = "0.75")]
    pub estimated_cost: String,
}

/// Response for file cost estimation
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "file_id": "file-abc123",
    "total_requests": 100,
    "total_estimated_input_tokens": 50000,
    "total_estimated_output_tokens": 25000,
    "total_estimated_cost": "0.75",
    "models": [{
        "model": "Qwen/Qwen3-30B-A3B-FP8",
        "request_count": 100,
        "estimated_input_tokens": 50000,
        "estimated_output_tokens": 25000,
        "estimated_cost": "0.75"
    }]
}))]
pub struct FileCostEstimate {
    #[schema(example = "file-abc123")]
    pub file_id: String,
    #[schema(example = 100)]
    pub total_requests: i64,
    #[schema(example = 50000)]
    pub total_estimated_input_tokens: i64,
    #[schema(example = 25000)]
    pub total_estimated_output_tokens: i64,
    /// Total cost as string to preserve decimal precision
    #[schema(example = "0.75")]
    pub total_estimated_cost: String,
    /// Per-model breakdown
    pub models: Vec<ModelCostBreakdown>,
}
