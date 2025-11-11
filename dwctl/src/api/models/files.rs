use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Query parameters for listing files
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct ListFilesQuery {
    /// A cursor for use in pagination. after is an object ID that defines your place in the list.
    pub after: Option<String>,

    /// Maximum number of files to return (1-10000, default 10000)
    #[param(default = 10000, minimum = 1, maximum = 10000)]
    pub limit: Option<i64>,

    /// Sort order by created_at (asc or desc, default desc)
    #[serde(default = "default_order")]
    pub order: String,

    /// Only return files with the given purpose
    pub purpose: Option<String>,
}

fn default_order() -> String {
    "desc".to_string()
}

/// Query parameters for file content
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct FileContentQuery {
    /// Maximum number of items to return (default: unlimited)
    #[param(minimum = 1)]
    pub limit: Option<i64>,

    /// Number of items to skip (default 0)
    #[param(default = 0, minimum = 0)]
    pub offset: Option<i64>,
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
