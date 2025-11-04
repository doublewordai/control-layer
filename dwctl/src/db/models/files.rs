use crate::db::handlers::files::FilePurpose;
use crate::types::UserId;
use chrono::{DateTime, Utc};

/// Storage backend type for files
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, serde::Serialize, serde::Deserialize)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum StorageBackend {
    Postgres,
    Local,
}

/// File lifecycle status
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, serde::Serialize, serde::Deserialize)]
#[sqlx(type_name = "file_status", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    /// File is active and content is available
    Active,
    /// File was deleted by user - content removed, metadata retained
    Deleted,
    /// File passed expiration date - content removed, metadata retained
    Expired,
    /// File upload or processing failed
    Failed,
}

/// Database request for creating a new file
#[derive(Debug, Clone)]
pub struct FileCreateDBRequest {
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub storage_backend: StorageBackend,
    pub uploaded_by: UserId,
    pub purpose: FilePurpose,
    pub expires_at: Option<DateTime<Utc>>,
    /// Storage-specific key/identifier:
    /// - Postgres: OID as string (e.g., "16384")
    /// - Local: relative path (e.g., "2024/11/abc-123.jsonl")
    pub storage_key: String,
}

/// Database request for updating file metadata
#[derive(Debug, Clone)]
pub struct FileUpdateDBRequest {
    pub filename: Option<String>,
}
