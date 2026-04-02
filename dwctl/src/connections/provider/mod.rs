//! Source provider trait and implementations.
//!
//! A [`SourceProvider`] knows how to list and stream files from an external
//! data source. Implementations are provider-specific (S3, BigQuery, etc.)
//! but the trait surface is generic so the sync pipeline is provider-agnostic.

pub mod s3;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;

/// A file-like item discovered in an external source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalFile {
    /// Provider-domain identifier (e.g. S3 object key relative to prefix).
    pub key: String,
    /// File size in bytes (best-effort, depends on provider).
    pub size_bytes: Option<i64>,
    /// Last modification time (best-effort).
    pub last_modified: Option<DateTime<Utc>>,
    /// Human-friendly display name (often the file name portion of key).
    pub display_name: Option<String>,
}

/// Result of a connection test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionTestResult {
    pub ok: bool,
    pub message: Option<String>,
    /// Provider-specific scope info (e.g. bucket name, prefix).
    pub scope: Option<serde_json::Value>,
}

/// Errors from provider operations.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("authentication failed: {0}")]
    AuthenticationFailed(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("access denied: {0}")]
    AccessDenied(String),

    #[error("provider error: {0}")]
    Internal(String),

    #[error("invalid config: {0}")]
    InvalidConfig(String),
}

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send>>;

/// Paginated file listing result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileListPage {
    pub files: Vec<ExternalFile>,
    /// Opaque cursor for the next page. `None` means no more pages.
    pub next_cursor: Option<String>,
    /// Whether there are more results beyond this page.
    pub has_more: bool,
}

/// Options for listing files.
#[derive(Debug, Clone, Default)]
pub struct ListFilesOptions {
    /// Maximum number of files to return per page.
    pub limit: Option<usize>,
    /// Opaque cursor from a previous `FileListPage.next_cursor`.
    pub cursor: Option<String>,
    /// Filter files whose key contains this substring (case-insensitive).
    pub search: Option<String>,
}

/// Trait for external source providers. Each provider type (S3, BigQuery, etc.)
/// implements this trait. The sync pipeline only interacts through this interface.
#[async_trait]
pub trait SourceProvider: Send + Sync {
    /// The provider type string (e.g. "s3", "bigquery").
    fn provider_type(&self) -> &str;

    /// List all files visible in the configured scope (unpaginated, for sync).
    async fn list_files(&self) -> Result<Vec<ExternalFile>, ProviderError>;

    /// List files with pagination and optional search filter (for browsing).
    async fn list_files_paged(&self, options: ListFilesOptions) -> Result<FileListPage, ProviderError>;

    /// Open a byte stream for a single file.
    async fn stream_file(&self, file_key: &str) -> Result<ByteStream, ProviderError>;

    /// Test that the connection credentials and scope are valid.
    async fn test_connection(&self) -> Result<ConnectionTestResult, ProviderError>;
}

/// Construct a provider from decrypted config JSON.
pub fn create_provider(
    provider_type: &str,
    config: serde_json::Value,
) -> Result<Box<dyn SourceProvider>, ProviderError> {
    match provider_type {
        "s3" => {
            let s3_config: s3::S3Config =
                serde_json::from_value(config).map_err(|e| ProviderError::InvalidConfig(e.to_string()))?;
            Ok(Box::new(s3::S3Provider::new(s3_config)))
        }
        other => Err(ProviderError::InvalidConfig(format!("unsupported provider: {other}"))),
    }
}
