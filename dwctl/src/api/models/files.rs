use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Query parameters for listing files
#[derive(Debug, Deserialize, IntoParams)]
pub struct ListFilesQuery {
    /// Maximum number of files to return (1-10000, default 20)
    #[param(default = 20, minimum = 1, maximum = 10000)]
    pub limit: Option<i64>,

    /// Number of files to skip (for pagination, default 0)
    #[param(default = 0, minimum = 0)]
    #[allow(dead_code)] // TODO: Implement offset-based pagination or remove this field
    pub skip: Option<i64>,

    /// Sort order by created_at (asc or desc, default desc)
    #[serde(default = "default_order")]
    pub order: String,
}

fn default_order() -> String {
    "desc".to_string()
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
}

/// Object type - always "file"
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ObjectType {
    File,
}

/// Purpose - always "batch" for our use case
#[derive(Debug, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Purpose {
    Batch,
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
