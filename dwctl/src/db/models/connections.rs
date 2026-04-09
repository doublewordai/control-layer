//! Database models for connections (external data source/destination integrations).

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// A connection record from the `connections` table.
#[derive(Debug, Clone)]
pub struct Connection {
    pub id: Uuid,
    pub user_id: Uuid,
    pub api_key_id: Option<Uuid>,
    pub kind: String,
    pub provider: String,
    pub name: String,
    pub config_encrypted: Vec<u8>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

/// A sync_operations record.
#[derive(Debug, Clone)]
pub struct SyncOperation {
    pub id: Uuid,
    pub connection_id: Uuid,
    pub status: String,
    pub strategy: String,
    pub strategy_config: Option<serde_json::Value>,
    pub files_found: i32,
    pub files_skipped: i32,
    pub files_ingested: i32,
    pub files_failed: i32,
    pub batches_created: i32,
    pub error_summary: Option<serde_json::Value>,
    pub triggered_by: Uuid,
    pub sync_config: serde_json::Value,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// A sync_entries record — tracks one external file through the sync pipeline.
#[derive(Debug, Clone)]
pub struct SyncEntry {
    pub id: Uuid,
    pub sync_id: Uuid,
    pub connection_id: Uuid,
    pub external_key: String,
    pub external_last_modified: Option<DateTime<Utc>>,
    pub external_size_bytes: Option<i64>,
    pub status: String,
    pub file_id: Option<Uuid>,
    pub batch_id: Option<Uuid>,
    pub template_count: Option<i32>,
    pub error: Option<String>,
    /// Number of lines that couldn't be parsed as JSON (tier 1 — garbled lines).
    pub skipped_lines: i32,
    /// Per-line validation errors for parseable-but-invalid lines (tier 2).
    /// Schema: [{"line": 3, "error": "missing model field"}, ...]
    pub validation_errors: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
