use crate::types::{FileId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// File status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    Processed,
    Error,
    Deleted,
    Expired,
}

impl FileStatus {
    pub fn to_db_string(self) -> &'static str {
        match self {
            FileStatus::Processed => "processed",
            FileStatus::Error => "error",
            FileStatus::Deleted => "deleted",
            FileStatus::Expired => "expired",
        }
    }

    pub fn from_db_string(s: &str) -> FileStatus {
        match s {
            "processed" => FileStatus::Processed,
            "error" => FileStatus::Error,
            "deleted" => FileStatus::Deleted,
            "expired" => FileStatus::Expired,
            _ => FileStatus::Processed, // Default fallback
        }
    }
}

/// Database request for creating a new file
#[derive(Debug, Clone)]
pub struct FileCreateDBRequest {
    pub id: FileId,
    pub filename: String,
    pub size_bytes: i64,
    pub uploaded_by: UserId,
    pub status: FileStatus,
    pub error_message: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Database request for updating file metadata
#[derive(Debug, Clone)]
pub struct FileUpdateDBRequest {
    pub filename: Option<String>,
    pub status: Option<FileStatus>,
    pub error_message: Option<Option<String>>,
    pub deleted_at: Option<DateTime<Utc>>,
}

/// Database response for a file
#[allow(dead_code)] // some fields are in the database but not currently used, will be useful internally later
#[derive(Debug, Clone)]
pub struct FileDBResponse {
    pub id: FileId,
    pub filename: String,
    pub size_bytes: i64,
    pub status: FileStatus,
    pub error_message: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub uploaded_by: UserId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
