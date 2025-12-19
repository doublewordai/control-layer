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

/// File object response (OpenAI-compatible)
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct FileResponse {
    pub id: String,
    #[serde(rename = "object")]
    pub object_type: ObjectType,
    pub bytes: i64,
    pub created_at: i64, // Unix timestamp
    pub filename: String,
    pub purpose: Purpose,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>, // Unix timestamp
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
pub struct FileDeleteResponse {
    pub id: String,
    #[serde(rename = "object")]
    pub object_type: ObjectType,
    pub deleted: bool,
}

/// Response for file list
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct FileListResponse {
    #[serde(rename = "object")]
    pub object_type: ListObject,
    pub data: Vec<FileResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_id: Option<String>,
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
pub struct ModelCostBreakdown {
    pub model: String,
    pub request_count: i64,
    pub estimated_input_tokens: i64,
    pub estimated_output_tokens: i64,
    /// Cost as string to preserve decimal precision
    pub estimated_cost: String,
}

/// Response for file cost estimation
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct FileCostEstimate {
    pub file_id: String,
    pub total_requests: i64,
    pub total_estimated_input_tokens: i64,
    pub total_estimated_output_tokens: i64,
    /// Total cost as string to preserve decimal precision
    pub total_estimated_cost: String,
    /// Per-model breakdown
    pub models: Vec<ModelCostBreakdown>,
}
