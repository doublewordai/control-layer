use crate::db::handlers::files::FilePurpose;
use crate::types::FileId;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Query parameters for listing files
#[derive(Debug, Deserialize, IntoParams)]
pub struct ListFilesQuery {
    /// A cursor for pagination - returns files after this ID
    #[param(value_type = String, format = "uuid")]
    pub after: Option<FileId>,

    /// Maximum number of files to return (1-10000, default 10000)
    #[param(default = 10000, minimum = 1, maximum = 10000)]
    pub limit: Option<i64>,

    /// Sort order by created_at (asc or desc, default desc)
    #[serde(default = "default_order")]
    pub order: String,

    /// Filter by purpose
    pub purpose: Option<FilePurpose>,
}

fn default_order() -> String {
    "desc".to_string()
}

/// File object response (OpenAI-compatible)
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct FileResponse {
    pub id: String,
    pub object: String, // Always "file"
    pub bytes: i64,
    pub created_at: i64, // Unix timestamp
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    pub purpose: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>, // Unix timestamp when file expires
}

impl FileResponse {
    /// Convert from the shared File object
    pub fn from_file(file: &crate::db::handlers::files::File) -> Self {
        Self {
            id: file.id.to_string(),
            object: "file".to_string(),
            bytes: file.size_bytes,
            created_at: file.created_at.timestamp(),
            filename: Some(file.filename.clone()),
            purpose: file.purpose.to_string(),
            expires_at: file.expires_at.map(|dt| dt.timestamp()),
        }
    }
}

/// Response for file deletion
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct FileDeleteResponse {
    pub id: String,
    pub object: String,
    pub deleted: bool,
}

/// Response for file list
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct FileListResponse {
    pub object: String, // Always "list"
    pub data: Vec<FileResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_id: Option<String>,
    pub has_more: bool,
}
