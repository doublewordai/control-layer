//! API request/response models for connections and sync operations.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateConnectionRequest {
    pub kind: Option<String>,
    pub provider: String,
    pub name: String,
    /// Provider-specific config (e.g. bucket, prefix, credentials).
    /// Stored encrypted — never returned via API.
    pub config: serde_json::Value,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ConnectionResponse {
    pub id: String,
    pub kind: String,
    pub provider: String,
    pub name: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ConnectionListResponse {
    pub data: Vec<ConnectionResponse>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateConnectionRequest {
    pub name: Option<String>,
    /// If provided, replaces the entire config (re-encrypted).
    pub config: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ConnectionTestResponse {
    pub ok: bool,
    pub provider: String,
    pub message: Option<String>,
    pub scope: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Sync operations
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub struct TriggerSyncRequest {
    /// Sync strategy: "snapshot" (default) or "select".
    #[serde(default = "default_strategy")]
    pub strategy: String,
    /// For "select" strategy: explicit list of file keys to ingest.
    pub file_keys: Option<Vec<String>>,
    /// Override default endpoint for batch creation.
    pub endpoint: Option<String>,
    /// Override default completion window.
    pub completion_window: Option<String>,
    /// When true, skip dedup and re-ingest files even if already synced.
    #[serde(default)]
    pub force: bool,
}

fn default_strategy() -> String {
    "snapshot".to_string()
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SyncOperationResponse {
    pub id: String,
    pub connection_id: String,
    pub status: String,
    pub strategy: String,
    pub files_found: i32,
    pub files_skipped: i32,
    pub files_ingested: i32,
    pub files_failed: i32,
    pub batches_created: i32,
    pub error_summary: Option<serde_json::Value>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub created_at: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SyncOperationListResponse {
    pub data: Vec<SyncOperationResponse>,
}

// ---------------------------------------------------------------------------
// Sync entries
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct SyncEntryResponse {
    pub id: String,
    pub external_key: String,
    pub external_size_bytes: Option<i64>,
    pub status: String,
    pub file_id: Option<String>,
    pub batch_id: Option<String>,
    pub template_count: Option<i32>,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SyncEntryListResponse {
    pub data: Vec<SyncEntryResponse>,
}

// ---------------------------------------------------------------------------
// Query params
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct SyncedKeyResponse {
    pub key: String,
    pub last_modified: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ListConnectionsQuery {
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListExternalFilesQuery {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
    pub search: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ExternalFileResponse {
    pub key: String,
    pub size_bytes: Option<i64>,
    pub last_modified: Option<i64>,
    pub display_name: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ExternalFileListResponse {
    pub data: Vec<ExternalFileResponse>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

// ---------------------------------------------------------------------------
// Conversions from DB models
// ---------------------------------------------------------------------------

impl From<crate::db::models::connections::Connection> for ConnectionResponse {
    fn from(c: crate::db::models::connections::Connection) -> Self {
        Self {
            id: c.id.to_string(),
            kind: c.kind,
            provider: c.provider,
            name: c.name,
            created_at: c.created_at.timestamp(),
            updated_at: c.updated_at.timestamp(),
        }
    }
}

impl From<crate::db::models::connections::SyncOperation> for SyncOperationResponse {
    fn from(s: crate::db::models::connections::SyncOperation) -> Self {
        Self {
            id: s.id.to_string(),
            connection_id: s.connection_id.to_string(),
            status: s.status,
            strategy: s.strategy,
            files_found: s.files_found,
            files_skipped: s.files_skipped,
            files_ingested: s.files_ingested,
            files_failed: s.files_failed,
            batches_created: s.batches_created,
            error_summary: s.error_summary,
            started_at: s.started_at.map(|dt| dt.timestamp()),
            completed_at: s.completed_at.map(|dt| dt.timestamp()),
            created_at: s.created_at.timestamp(),
        }
    }
}

impl From<crate::db::models::connections::SyncEntry> for SyncEntryResponse {
    fn from(e: crate::db::models::connections::SyncEntry) -> Self {
        Self {
            id: e.id.to_string(),
            external_key: e.external_key,
            external_size_bytes: e.external_size_bytes,
            status: e.status,
            file_id: e.file_id.map(|id| id.to_string()),
            batch_id: e.batch_id.map(|id| id.to_string()),
            template_count: e.template_count,
            error: e.error,
            created_at: e.created_at.timestamp(),
            updated_at: e.updated_at.timestamp(),
        }
    }
}
