//! Database repository for connections and sync operations.

use crate::db::errors::{DbError, Result};
use crate::db::models::connections::{Connection, SyncEntry, SyncOperation};
use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection};
use tracing::instrument;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Row structs (map exactly to table columns)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
struct ConnectionRow {
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

impl From<ConnectionRow> for Connection {
    fn from(r: ConnectionRow) -> Self {
        Self {
            id: r.id,
            user_id: r.user_id,
            api_key_id: r.api_key_id,
            kind: r.kind,
            provider: r.provider,
            name: r.name,
            config_encrypted: r.config_encrypted,
            created_at: r.created_at,
            updated_at: r.updated_at,
            deleted_at: r.deleted_at,
        }
    }
}

#[derive(Debug, Clone, FromRow)]
struct SyncOperationRow {
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

impl From<SyncOperationRow> for SyncOperation {
    fn from(r: SyncOperationRow) -> Self {
        Self {
            id: r.id,
            connection_id: r.connection_id,
            status: r.status,
            strategy: r.strategy,
            strategy_config: r.strategy_config,
            files_found: r.files_found,
            files_skipped: r.files_skipped,
            files_ingested: r.files_ingested,
            files_failed: r.files_failed,
            batches_created: r.batches_created,
            error_summary: r.error_summary,
            triggered_by: r.triggered_by,
            sync_config: r.sync_config,
            started_at: r.started_at,
            completed_at: r.completed_at,
            created_at: r.created_at,
        }
    }
}

#[derive(Debug, Clone, FromRow)]
struct SyncEntryRow {
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
    pub skipped_lines: i32,
    pub validation_errors: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<SyncEntryRow> for SyncEntry {
    fn from(r: SyncEntryRow) -> Self {
        Self {
            id: r.id,
            sync_id: r.sync_id,
            connection_id: r.connection_id,
            external_key: r.external_key,
            external_last_modified: r.external_last_modified,
            external_size_bytes: r.external_size_bytes,
            status: r.status,
            file_id: r.file_id,
            batch_id: r.batch_id,
            template_count: r.template_count,
            error: r.error,
            skipped_lines: r.skipped_lines,
            validation_errors: r.validation_errors,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

// ---------------------------------------------------------------------------
// Connections repository
// ---------------------------------------------------------------------------

pub struct Connections<'c> {
    db: &'c mut PgConnection,
}

impl<'c> Connections<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    #[instrument(skip(self, config_encrypted), fields(name = %name, provider = %provider), err)]
    pub async fn create(
        &mut self,
        user_id: Uuid,
        api_key_id: Option<Uuid>,
        kind: &str,
        provider: &str,
        name: &str,
        config_encrypted: &[u8],
    ) -> Result<Connection> {
        let row = sqlx::query_as!(
            ConnectionRow,
            r#"
            INSERT INTO connections (user_id, api_key_id, kind, provider, name, config_encrypted)
            VALUES ($1, $2, $3, $4, $5, $6)
            RETURNING *
            "#,
            user_id,
            api_key_id,
            kind,
            provider,
            name,
            config_encrypted,
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(Connection::from(row))
    }

    #[instrument(skip(self), fields(id = %id), err)]
    pub async fn get_by_id(&mut self, id: Uuid) -> Result<Option<Connection>> {
        let row = sqlx::query_as!(ConnectionRow, "SELECT * FROM connections WHERE id = $1 AND deleted_at IS NULL", id,)
            .fetch_optional(&mut *self.db)
            .await?;

        Ok(row.map(Connection::from))
    }

    /// Bulk fetch connection names and owners by IDs. Returns a map of id → (name, user_id).
    #[instrument(skip(self, ids), fields(count = ids.len()), err)]
    pub async fn get_names_by_ids(&mut self, ids: &[Uuid]) -> Result<std::collections::HashMap<Uuid, (String, Uuid)>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows = sqlx::query!(
            "SELECT id, name, user_id FROM connections WHERE id = ANY($1) AND deleted_at IS NULL",
            ids,
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().map(|r| (r.id, (r.name, r.user_id))).collect())
    }

    #[instrument(skip(self), fields(user_id = %user_id), err)]
    pub async fn list_by_user(&mut self, user_id: Uuid, kind: Option<&str>) -> Result<Vec<Connection>> {
        let rows = sqlx::query_as!(
            ConnectionRow,
            r#"
            SELECT * FROM connections
            WHERE user_id = $1
              AND deleted_at IS NULL
              AND ($2::text IS NULL OR kind = $2)
            ORDER BY created_at DESC
            "#,
            user_id,
            kind,
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().map(Connection::from).collect())
    }

    #[instrument(skip(self), fields(id = %id), err)]
    pub async fn soft_delete(&mut self, id: Uuid) -> Result<bool> {
        // Soft-delete sync tracking data — the files and batches in fusillade
        // remain untouched so users can still access them.
        // We mark entries/operations as deleted so dedup won't consider them,
        // but keep the records for audit trail.
        sqlx::query!(
            "UPDATE sync_entries SET status = 'deleted' WHERE connection_id = $1 AND status != 'deleted'",
            id,
        )
        .execute(&mut *self.db)
        .await?;

        sqlx::query!(
            "UPDATE sync_operations SET status = 'deleted' WHERE connection_id = $1 AND status != 'deleted'",
            id,
        )
        .execute(&mut *self.db)
        .await?;

        let result = sqlx::query!("UPDATE connections SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL", id,)
            .execute(&mut *self.db)
            .await?;

        Ok(result.rows_affected() > 0)
    }

    #[instrument(skip(self, config_encrypted), fields(id = %id), err)]
    pub async fn update(&mut self, id: Uuid, name: Option<&str>, config_encrypted: Option<&[u8]>) -> Result<Connection> {
        let row = sqlx::query_as!(
            ConnectionRow,
            r#"
            UPDATE connections SET
                name = COALESCE($2, name),
                config_encrypted = COALESCE($3, config_encrypted)
            WHERE id = $1 AND deleted_at IS NULL
            RETURNING *
            "#,
            id,
            name,
            config_encrypted,
        )
        .fetch_optional(&mut *self.db)
        .await?
        .ok_or(DbError::NotFound)?;

        Ok(Connection::from(row))
    }
}

// ---------------------------------------------------------------------------
// Sync operations repository
// ---------------------------------------------------------------------------

pub struct SyncOperations<'c> {
    db: &'c mut PgConnection,
}

impl<'c> SyncOperations<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    #[instrument(skip(self, sync_config, strategy_config), err)]
    pub async fn create(
        &mut self,
        connection_id: Uuid,
        triggered_by: Uuid,
        strategy: &str,
        strategy_config: Option<&serde_json::Value>,
        sync_config: &serde_json::Value,
    ) -> Result<SyncOperation> {
        let row = sqlx::query_as!(
            SyncOperationRow,
            r#"
            INSERT INTO sync_operations (connection_id, triggered_by, strategy, strategy_config, sync_config)
            VALUES ($1, $2, $3, $4, $5)
            RETURNING *
            "#,
            connection_id,
            triggered_by,
            strategy,
            strategy_config,
            sync_config,
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(SyncOperation::from(row))
    }

    #[instrument(skip(self), fields(id = %id), err)]
    pub async fn get_by_id(&mut self, id: Uuid) -> Result<Option<SyncOperation>> {
        let row = sqlx::query_as!(
            SyncOperationRow,
            "SELECT * FROM sync_operations WHERE id = $1 AND status != 'deleted'",
            id,
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(row.map(SyncOperation::from))
    }

    #[instrument(skip(self), fields(connection_id = %connection_id), err)]
    pub async fn list_by_connection(&mut self, connection_id: Uuid) -> Result<Vec<SyncOperation>> {
        let rows = sqlx::query_as!(
            SyncOperationRow,
            "SELECT * FROM sync_operations WHERE connection_id = $1 AND status != 'deleted' ORDER BY created_at DESC",
            connection_id,
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().map(SyncOperation::from).collect())
    }

    #[instrument(skip(self), fields(id = %id, status = %status), err)]
    pub async fn update_status(&mut self, id: Uuid, status: &str) -> Result<()> {
        let started = if status == "listing" { Some(Utc::now()) } else { None };
        let completed = if matches!(status, "completed" | "failed" | "cancelled") {
            Some(Utc::now())
        } else {
            None
        };

        sqlx::query!(
            r#"
            UPDATE sync_operations SET
                status = $2,
                started_at = COALESCE($3, started_at),
                completed_at = COALESCE($4, completed_at)
            WHERE id = $1 AND status != 'deleted'
            "#,
            id,
            status,
            started,
            completed,
        )
        .execute(&mut *self.db)
        .await?;

        Ok(())
    }

    #[instrument(skip(self), fields(id = %id), err)]
    pub async fn update_counters(
        &mut self,
        id: Uuid,
        files_found: Option<i32>,
        files_skipped: Option<i32>,
        files_ingested: Option<i32>,
        files_failed: Option<i32>,
        batches_created: Option<i32>,
    ) -> Result<()> {
        sqlx::query!(
            r#"
            UPDATE sync_operations SET
                files_found = COALESCE($2, files_found),
                files_skipped = COALESCE($3, files_skipped),
                files_ingested = COALESCE($4, files_ingested),
                files_failed = COALESCE($5, files_failed),
                batches_created = COALESCE($6, batches_created)
            WHERE id = $1
              AND status != 'deleted'
            "#,
            id,
            files_found,
            files_skipped,
            files_ingested,
            files_failed,
            batches_created,
        )
        .execute(&mut *self.db)
        .await?;

        Ok(())
    }

    /// Atomically increment a single counter by 1.
    #[instrument(skip(self), fields(id = %id, field = %field), err)]
    pub async fn increment_counter(&mut self, id: Uuid, field: &str) -> Result<()> {
        // Use dynamic SQL safely — field is validated against an allowlist.
        let query = match field {
            "files_found" => "UPDATE sync_operations SET files_found = files_found + 1 WHERE id = $1 AND status != 'deleted'",
            "files_skipped" => "UPDATE sync_operations SET files_skipped = files_skipped + 1 WHERE id = $1 AND status != 'deleted'",
            "files_ingested" => "UPDATE sync_operations SET files_ingested = files_ingested + 1 WHERE id = $1 AND status != 'deleted'",
            "files_failed" => "UPDATE sync_operations SET files_failed = files_failed + 1 WHERE id = $1 AND status != 'deleted'",
            "batches_created" => "UPDATE sync_operations SET batches_created = batches_created + 1 WHERE id = $1 AND status != 'deleted'",
            _ => return Err(DbError::Other(anyhow::anyhow!("unknown counter field: {}", field))),
        };

        sqlx::query(query).bind(id).execute(&mut *self.db).await?;
        Ok(())
    }

    /// Check if all entries for a sync are in a terminal state, and if so mark
    /// the sync_operation as completed (or failed if all entries failed).
    ///
    /// Returns true if the sync was marked terminal.
    #[instrument(skip(self), fields(id = %sync_id), err)]
    pub async fn try_complete(&mut self, sync_id: Uuid) -> Result<bool> {
        // Count entries by terminal vs non-terminal status in a single query
        let row = sqlx::query!(
            r#"
            SELECT
                COUNT(*) AS "total!",
                COUNT(*) FILTER (WHERE status IN ('activated', 'failed', 'skipped')) AS "terminal!",
                COUNT(*) FILTER (WHERE status = 'failed') AS "failed!"
            FROM sync_entries
            WHERE sync_id = $1 AND status != 'deleted'
            "#,
            sync_id,
        )
        .fetch_one(&mut *self.db)
        .await?;

        if row.total == 0 || row.terminal < row.total {
            return Ok(false);
        }

        // All entries are terminal — determine final sync status
        let final_status = if row.failed == row.total { "failed" } else { "completed" };

        sqlx::query!(
            "UPDATE sync_operations SET status = $2, completed_at = now() WHERE id = $1 AND completed_at IS NULL AND status != 'deleted'",
            sync_id,
            final_status,
        )
        .execute(&mut *self.db)
        .await?;

        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Sync entries repository
// ---------------------------------------------------------------------------

pub struct SyncEntries<'c> {
    db: &'c mut PgConnection,
}

impl<'c> SyncEntries<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    /// Bulk-insert sync entries for discovered files.
    #[allow(clippy::type_complexity)]
    #[instrument(skip(self, entries), fields(sync_id = %sync_id, count = entries.len()), err)]
    pub async fn bulk_create(
        &mut self,
        sync_id: Uuid,
        connection_id: Uuid,
        entries: &[(String, Option<DateTime<Utc>>, Option<i64>)], // (key, last_modified, size)
    ) -> Result<Vec<SyncEntry>> {
        if entries.is_empty() {
            return Ok(vec![]);
        }

        let keys: Vec<&str> = entries.iter().map(|(k, _, _)| k.as_str()).collect();
        let last_mods: Vec<Option<DateTime<Utc>>> = entries.iter().map(|(_, lm, _)| *lm).collect();
        let sizes: Vec<Option<i64>> = entries.iter().map(|(_, _, s)| *s).collect();

        let rows = sqlx::query_as!(
            SyncEntryRow,
            r#"
            INSERT INTO sync_entries (sync_id, connection_id, external_key, external_last_modified, external_size_bytes)
            SELECT $1, $2, t.key, t.last_modified, t.size_bytes
            FROM unnest($3::text[], $4::timestamptz[], $5::bigint[]) AS t(key, last_modified, size_bytes)
            RETURNING *
            "#,
            sync_id,
            connection_id,
            &keys as &[&str],
            &last_mods as &[Option<DateTime<Utc>>],
            &sizes as &[Option<i64>],
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().map(SyncEntry::from).collect())
    }

    /// Find previously-synced entries for dedup.
    #[instrument(skip(self), fields(connection_id = %connection_id), err)]
    pub async fn find_existing(
        &mut self,
        connection_id: Uuid,
        keys_and_dates: &[(String, Option<DateTime<Utc>>)],
    ) -> Result<Vec<(String, Option<DateTime<Utc>>)>> {
        if keys_and_dates.is_empty() {
            return Ok(vec![]);
        }

        let keys: Vec<&str> = keys_and_dates.iter().map(|(k, _)| k.as_str()).collect();
        let dates: Vec<Option<DateTime<Utc>>> = keys_and_dates.iter().map(|(_, d)| *d).collect();

        let rows = sqlx::query!(
            r#"
            SELECT se.external_key, se.external_last_modified
            FROM sync_entries se
            INNER JOIN unnest($2::text[], $3::timestamptz[]) AS input(key, last_modified)
              ON se.external_key = input.key
              AND se.external_last_modified IS NOT DISTINCT FROM input.last_modified
            WHERE se.connection_id = $1
              AND se.status IN ('activated', 'failed')
            "#,
            connection_id,
            &keys as &[&str],
            &dates as &[Option<DateTime<Utc>>],
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().map(|r| (r.external_key, r.external_last_modified)).collect())
    }

    /// Get all terminal sync entries for a connection (for UI status display).
    /// Returns (key, last_modified, status) where status is 'activated' or 'failed'.
    /// The frontend uses this to show Synced/Failed/Modified states in the file browser.
    #[instrument(skip(self), fields(connection_id = %connection_id), err)]
    #[allow(clippy::type_complexity)]
    pub async fn list_synced_keys(&mut self, connection_id: Uuid) -> Result<Vec<(String, Option<DateTime<Utc>>, String)>> {
        let rows = sqlx::query!(
            r#"
            SELECT DISTINCT ON (external_key)
                   external_key,
                   external_last_modified AS "last_modified",
                   status AS "status!"
            FROM sync_entries
            WHERE connection_id = $1
              AND status IN ('activated', 'failed')
            ORDER BY external_key, external_last_modified DESC NULLS LAST, updated_at DESC
            "#,
            connection_id,
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().map(|r| (r.external_key, r.last_modified, r.status)).collect())
    }

    /// Returns true if the row was updated, false if it was already deleted.
    #[instrument(skip(self), fields(id = %id, status = %status), err)]
    pub async fn update_status(&mut self, id: Uuid, status: &str, error: Option<&str>) -> Result<bool> {
        let result = sqlx::query!(
            "UPDATE sync_entries SET status = $2, error = $3 WHERE id = $1 AND status != 'deleted'",
            id,
            status,
            error,
        )
        .execute(&mut *self.db)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Returns true if the row was updated, false if it was already deleted.
    #[instrument(skip(self, validation_errors), fields(id = %id), err)]
    pub async fn set_ingested(
        &mut self,
        id: Uuid,
        file_id: Uuid,
        template_count: i32,
        skipped_lines: i32,
        validation_errors: Option<&serde_json::Value>,
    ) -> Result<bool> {
        let result = sqlx::query!(
            r#"
            UPDATE sync_entries
            SET status = 'ingested', file_id = $2, template_count = $3,
                skipped_lines = $4, validation_errors = $5
            WHERE id = $1 AND status != 'deleted'
            "#,
            id,
            file_id,
            template_count,
            skipped_lines,
            validation_errors,
        )
        .execute(&mut *self.db)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    #[instrument(skip(self), fields(id = %id), err)]
    pub async fn set_activated(&mut self, id: Uuid, batch_id: Uuid) -> Result<()> {
        sqlx::query!(
            "UPDATE sync_entries SET status = 'activated', batch_id = $2 WHERE id = $1 AND status != 'deleted'",
            id,
            batch_id,
        )
        .execute(&mut *self.db)
        .await?;

        Ok(())
    }

    #[instrument(skip(self), fields(sync_id = %sync_id), err)]
    pub async fn list_by_sync(&mut self, sync_id: Uuid) -> Result<Vec<SyncEntry>> {
        let rows = sqlx::query_as!(
            SyncEntryRow,
            "SELECT * FROM sync_entries WHERE sync_id = $1 ORDER BY external_key",
            sync_id,
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().map(SyncEntry::from).collect())
    }

    /// Get entry by ID.
    #[instrument(skip(self), fields(id = %id), err)]
    pub async fn get_by_id(&mut self, id: Uuid) -> Result<Option<SyncEntry>> {
        let row = sqlx::query_as!(SyncEntryRow, "SELECT * FROM sync_entries WHERE id = $1", id,)
            .fetch_optional(&mut *self.db)
            .await?;

        Ok(row.map(SyncEntry::from))
    }
}
