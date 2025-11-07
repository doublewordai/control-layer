//! PostgreSQL implementation of Storage and DaemonExecutor.
//!
//! This implementation combines PostgreSQL storage with the daemon to provide
//! a production-ready batching system with persistent storage and real-time updates.

use crate::request::AnyRequest;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use chrono::Utc;
use futures::stream::Stream;
use sqlx::postgres::{PgListener, PgPool};
use sqlx::Row;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use super::Storage;
use crate::batch::{
    BatchId, BatchStatus, File, FileId, FileMetadata, FileStreamItem, RequestTemplate,
    RequestTemplateInput, TemplateId,
};
use crate::daemon::{Daemon, DaemonConfig};
use crate::error::{FusilladeError, Result};
use crate::http::HttpClient;
use crate::request::{
    Canceled, Claimed, Completed, DaemonId, Failed, Pending, Processing, Request, RequestData,
    RequestId, RequestState,
};

use super::DaemonExecutor;

/// PostgreSQL implementation of the Storage and DaemonExecutor traits.
///
/// This manager uses PostgreSQL for persistent storage and runs a daemon for processing requests.
/// It leverages Postgres LISTEN/NOTIFY for real-time status updates.
///
/// # Example
/// ```ignore
/// use fusillade::PostgresRequestManager;
/// use sqlx::PgPool;
///
/// let pool = PgPool::connect("postgresql://localhost/fusillade").await?;
/// let manager = Arc::new(PostgresRequestManager::new(pool));
///
/// // Start processing
/// let handle = manager.clone().run()?;
///
/// // Create files and batches
/// let file_id = manager.create_file(name, description, templates).await?;
/// let batch_id = manager.create_batch(file_id).await?;
/// ```
pub struct PostgresRequestManager<H: HttpClient> {
    pool: PgPool,
    http_client: Arc<H>,
    config: DaemonConfig,
    download_buffer_size: usize,
}

impl PostgresRequestManager<crate::http::ReqwestHttpClient> {
    /// Create a new PostgreSQL request manager with default settings.
    ///
    /// Uses the default Reqwest HTTP client and default daemon configuration.
    /// Customize with `.with_config()` if needed.
    ///
    /// # Example
    /// ```ignore
    /// let manager = PostgresRequestManager::new(pool)
    ///     .with_config(my_config);
    /// ```
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            http_client: Arc::new(crate::http::ReqwestHttpClient::default()),
            config: DaemonConfig::default(),
            download_buffer_size: 100,
        }
    }
}

impl<H: HttpClient + 'static> PostgresRequestManager<H> {
    /// Create a PostgreSQL request manager with a custom HTTP client.
    ///
    /// Uses the default daemon configuration. Customize with `.with_config()` if needed.
    ///
    /// # Example
    /// ```ignore
    /// let manager = PostgresRequestManager::with_client(pool, Arc::new(my_client))
    ///     .with_config(my_config);
    /// ```
    pub fn with_client(pool: PgPool, http_client: Arc<H>) -> Self {
        Self {
            pool,
            http_client,
            config: DaemonConfig::default(),
            download_buffer_size: 100,
        }
    }

    /// Set a custom daemon configuration.
    ///
    /// This is a builder method that can be chained after `new()` or `with_client()`.
    pub fn with_config(mut self, config: DaemonConfig) -> Self {
        self.config = config;
        self
    }

    /// Set the download buffer size for file content streams.
    ///
    /// This is a builder method that can be chained after `new()` or `with_client()`.
    /// Default is 100.
    pub fn with_download_buffer_size(mut self, buffer_size: usize) -> Self {
        self.download_buffer_size = buffer_size;
        self
    }

    /// Get the connection pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Create a listener for real-time request updates.
    ///
    /// This returns a PgListener that can be used to receive notifications
    /// when requests are updated.
    pub async fn create_listener(&self) -> Result<PgListener> {
        PgListener::connect_with(&self.pool)
            .await
            .map_err(|e| FusilladeError::Other(anyhow!("Failed to create listener: {}", e)))
    }
}

// Additional methods for PostgresRequestManager (not part of Storage trait)
impl<H: HttpClient + 'static> PostgresRequestManager<H> {
    /// Unclaim stale requests that have been stuck in "claimed" or "processing" states
    /// for longer than the configured timeouts. This handles daemon crashes.
    ///
    /// Returns the number of requests that were unclaimed.
    async fn unclaim_stale_requests(&self) -> Result<usize> {
        let claim_timeout_ms = self.config.claim_timeout_ms as i64;
        let processing_timeout_ms = self.config.processing_timeout_ms as i64;

        // Unclaim requests that are stuck in claimed or processing states
        let result = sqlx::query!(
            r#"
            UPDATE requests
            SET
                state = 'pending',
                daemon_id = NULL,
                claimed_at = NULL,
                started_at = NULL
            WHERE
                (state = 'claimed' AND claimed_at < NOW() - ($1 || ' milliseconds')::INTERVAL)
                OR
                (state = 'processing' AND started_at < NOW() - ($2 || ' milliseconds')::INTERVAL)
            RETURNING id
            "#,
            claim_timeout_ms.to_string(),
            processing_timeout_ms.to_string(),
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to unclaim stale requests: {}", e)))?;

        let count = result.len();

        if count > 0 {
            let request_ids: Vec<_> = result.iter().map(|r| r.id).collect();
            tracing::warn!(
                count = count,
                request_ids = ?request_ids,
                claim_timeout_ms,
                processing_timeout_ms,
                "Unclaimed stale requests (likely due to daemon crash)"
            );
        }

        Ok(count)
    }

    /// Check if a file should be expired and mark it as such.
    /// Returns true if the file was marked as expired.
    async fn check_and_mark_expired(&self, file: &mut File) -> Result<bool> {
        // Only check files that are currently in 'processed' status
        if file.status != crate::batch::FileStatus::Processed {
            return Ok(false);
        }

        // Check if file has an expiration date and it has passed
        if let Some(expires_at) = file.expires_at {
            if Utc::now() > expires_at {
                // Mark as expired in the database
                sqlx::query!(
                    r#"
                    UPDATE files
                    SET status = 'expired'
                    WHERE id = $1 AND status = 'processed'
                    "#,
                    *file.id as Uuid,
                )
                .execute(&self.pool)
                .await
                .map_err(|e| {
                    FusilladeError::Other(anyhow!("Failed to mark file as expired: {}", e))
                })?;

                // Update the in-memory file object
                file.status = crate::batch::FileStatus::Expired;
                return Ok(true);
            }
        }

        Ok(false)
    }
}

// Implement Storage trait directly (no delegation)
#[async_trait]
impl<H: HttpClient + 'static> Storage for PostgresRequestManager<H> {
    async fn claim_requests(
        &self,
        limit: usize,
        daemon_id: DaemonId,
    ) -> Result<Vec<Request<Claimed>>> {
        // First, unclaim any stale requests (self-healing for daemon crashes)
        let unclaimed_count = self.unclaim_stale_requests().await?;
        if unclaimed_count > 0 {
            tracing::info!(
                unclaimed_count,
                "Unclaimed stale requests before claiming new ones"
            );
        }

        let now = Utc::now();

        // Atomically claim pending executions using SELECT FOR UPDATE
        let rows = sqlx::query!(
            r#"
            UPDATE requests
            SET
                state = 'claimed',
                daemon_id = $1,
                claimed_at = $2
            WHERE id IN (
                SELECT id
                FROM requests
                WHERE state = 'pending'
                    AND (not_before IS NULL OR not_before <= $2)
                ORDER BY created_at ASC
                LIMIT $3
                FOR UPDATE SKIP LOCKED
            )
            RETURNING id, batch_id, template_id, endpoint, method, path, body, model, api_key, retry_attempt
            "#,
            *daemon_id as Uuid,
            now,
            limit as i64,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to claim requests: {}", e)))?;

        Ok(rows
            .into_iter()
            .map(|row| Request {
                state: Claimed {
                    daemon_id,
                    claimed_at: now,
                    retry_attempt: row.retry_attempt as u32,
                },
                data: RequestData {
                    id: RequestId(row.id),
                    batch_id: BatchId(row.batch_id),
                    template_id: TemplateId(row.template_id),
                    endpoint: row.endpoint,
                    method: row.method,
                    path: row.path,
                    body: row.body,
                    model: row.model,
                    api_key: row.api_key,
                },
            })
            .collect())
    }

    async fn persist<T: RequestState + Clone>(&self, request: &Request<T>) -> Result<()>
    where
        AnyRequest: From<Request<T>>,
    {
        let any_request = AnyRequest::from(request.clone());

        match any_request {
            AnyRequest::Pending(req) => {
                let rows_affected = sqlx::query!(
                    r#"
                    UPDATE requests SET
                        state = 'pending',
                        retry_attempt = $2,
                        not_before = $3,
                        daemon_id = NULL,
                        claimed_at = NULL,
                        started_at = NULL
                    WHERE id = $1
                    "#,
                    *req.data.id as Uuid,
                    req.state.retry_attempt as i32,
                    req.state.not_before,
                )
                .execute(&self.pool)
                .await
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to update request: {}", e)))?
                .rows_affected();

                if rows_affected == 0 {
                    return Err(FusilladeError::RequestNotFound(req.data.id));
                }
            }
            AnyRequest::Claimed(req) => {
                let rows_affected = sqlx::query!(
                    r#"
                    UPDATE requests SET
                        state = 'claimed',
                        retry_attempt = $2,
                        daemon_id = $3,
                        claimed_at = $4,
                        started_at = NULL,
                        not_before = NULL
                    WHERE id = $1
                    "#,
                    *req.data.id as Uuid,
                    req.state.retry_attempt as i32,
                    *req.state.daemon_id as Uuid,
                    req.state.claimed_at,
                )
                .execute(&self.pool)
                .await
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to update request: {}", e)))?
                .rows_affected();

                if rows_affected == 0 {
                    return Err(FusilladeError::RequestNotFound(req.data.id));
                }
            }
            AnyRequest::Processing(req) => {
                let rows_affected = sqlx::query!(
                    r#"
                    UPDATE requests SET
                        state = 'processing',
                        retry_attempt = $2,
                        daemon_id = $3,
                        claimed_at = $4,
                        started_at = $5
                    WHERE id = $1
                    "#,
                    *req.data.id as Uuid,
                    req.state.retry_attempt as i32,
                    *req.state.daemon_id as Uuid,
                    req.state.claimed_at,
                    req.state.started_at,
                )
                .execute(&self.pool)
                .await
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to update request: {}", e)))?
                .rows_affected();

                if rows_affected == 0 {
                    return Err(FusilladeError::RequestNotFound(req.data.id));
                }
            }
            AnyRequest::Completed(req) => {
                let rows_affected = sqlx::query!(
                    r#"
                    UPDATE requests SET
                        state = 'completed',
                        response_status = $2,
                        response_body = $3,
                        claimed_at = $4,
                        started_at = $5,
                        completed_at = $6
                    WHERE id = $1
                    "#,
                    *req.data.id as Uuid,
                    req.state.response_status as i16,
                    req.state.response_body,
                    req.state.claimed_at,
                    req.state.started_at,
                    req.state.completed_at,
                )
                .execute(&self.pool)
                .await
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to update request: {}", e)))?
                .rows_affected();

                if rows_affected == 0 {
                    return Err(FusilladeError::RequestNotFound(req.data.id));
                }
            }
            AnyRequest::Failed(req) => {
                let rows_affected = sqlx::query!(
                    r#"
                    UPDATE requests SET
                        state = 'failed',
                        retry_attempt = $2,
                        error = $3,
                        failed_at = $4
                    WHERE id = $1
                    "#,
                    *req.data.id as Uuid,
                    req.state.retry_attempt as i32,
                    req.state.error,
                    req.state.failed_at,
                )
                .execute(&self.pool)
                .await
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to update request: {}", e)))?
                .rows_affected();

                if rows_affected == 0 {
                    return Err(FusilladeError::RequestNotFound(req.data.id));
                }
            }
            AnyRequest::Canceled(req) => {
                let rows_affected = sqlx::query!(
                    r#"
                    UPDATE requests SET
                        state = 'canceled',
                        canceled_at = $2
                    WHERE id = $1
                    "#,
                    *req.data.id as Uuid,
                    req.state.canceled_at,
                )
                .execute(&self.pool)
                .await
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to update request: {}", e)))?
                .rows_affected();

                if rows_affected == 0 {
                    return Err(FusilladeError::RequestNotFound(req.data.id));
                }
            }
        }

        Ok(())
    }

    async fn get_requests(&self, ids: Vec<RequestId>) -> Result<Vec<Result<AnyRequest>>> {
        let uuid_ids: Vec<Uuid> = ids.iter().map(|id| **id).collect();

        let rows = sqlx::query!(
            r#"
            SELECT
                id, batch_id, template_id, state, endpoint, method, path, body, model, api_key,
                retry_attempt, not_before, daemon_id, claimed_at, started_at,
                response_status, response_body, completed_at, error, failed_at, canceled_at
            FROM requests
            WHERE id = ANY($1)
            "#,
            &uuid_ids,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to fetch requests: {}", e)))?;

        // Build a map of id -> request for efficient lookup
        let mut request_map: std::collections::HashMap<RequestId, Result<AnyRequest>> =
            std::collections::HashMap::new();

        for row in rows {
            let request_id = RequestId(row.id);
            let data = RequestData {
                id: request_id,
                batch_id: BatchId(row.batch_id),
                template_id: TemplateId(row.template_id),
                endpoint: row.endpoint,
                method: row.method,
                path: row.path,
                body: row.body,
                model: row.model,
                api_key: row.api_key,
            };

            let state = &row.state;

            let any_request = match state.as_str() {
                "pending" => Ok(AnyRequest::Pending(Request {
                    state: Pending {
                        retry_attempt: row.retry_attempt as u32,
                        not_before: row.not_before,
                    },
                    data,
                })),
                "claimed" => Ok(AnyRequest::Claimed(Request {
                    state: Claimed {
                        daemon_id: DaemonId(row.daemon_id.ok_or_else(|| {
                            FusilladeError::Other(anyhow!("Missing daemon_id for claimed request"))
                        })?),
                        claimed_at: row.claimed_at.ok_or_else(|| {
                            FusilladeError::Other(anyhow!("Missing claimed_at for claimed request"))
                        })?,
                        retry_attempt: row.retry_attempt as u32,
                    },
                    data,
                })),
                "processing" => {
                    // TODO: fix this - creating dummy channels is ugly but works for now
                    // Create a "read-only" Processing state for status display.
                    // The channel fields are marked #[serde(skip)] and won't be serialized anyway.
                    let (_tx, rx) = tokio::sync::mpsc::channel(1);
                    // Create a dummy abort handle (from a noop task)
                    let abort_handle = tokio::spawn(async {}).abort_handle();
                    Ok(AnyRequest::Processing(Request {
                        state: Processing {
                            daemon_id: DaemonId(row.daemon_id.ok_or_else(|| {
                                FusilladeError::Other(anyhow!(
                                    "Missing daemon_id for processing request"
                                ))
                            })?),
                            claimed_at: row.claimed_at.ok_or_else(|| {
                                FusilladeError::Other(anyhow!(
                                    "Missing claimed_at for processing request"
                                ))
                            })?,
                            started_at: row.started_at.ok_or_else(|| {
                                FusilladeError::Other(anyhow!(
                                    "Missing started_at for processing request"
                                ))
                            })?,
                            retry_attempt: row.retry_attempt as u32,
                            result_rx: Arc::new(Mutex::new(rx)),
                            abort_handle,
                        },
                        data,
                    }))
                }
                "completed" => Ok(AnyRequest::Completed(Request {
                    state: Completed {
                        response_status: row.response_status.ok_or_else(|| {
                            FusilladeError::Other(anyhow!(
                                "Missing response_status for completed request"
                            ))
                        })? as u16,
                        response_body: row.response_body.ok_or_else(|| {
                            FusilladeError::Other(anyhow!(
                                "Missing response_body for completed request"
                            ))
                        })?,
                        claimed_at: row.claimed_at.ok_or_else(|| {
                            FusilladeError::Other(anyhow!(
                                "Missing claimed_at for completed request"
                            ))
                        })?,
                        started_at: row.started_at.ok_or_else(|| {
                            FusilladeError::Other(anyhow!(
                                "Missing started_at for completed request"
                            ))
                        })?,
                        completed_at: row.completed_at.ok_or_else(|| {
                            FusilladeError::Other(anyhow!(
                                "Missing completed_at for completed request"
                            ))
                        })?,
                    },
                    data,
                })),
                "failed" => Ok(AnyRequest::Failed(Request {
                    state: Failed {
                        error: row.error.ok_or_else(|| {
                            FusilladeError::Other(anyhow!("Missing error for failed request"))
                        })?,
                        failed_at: row.failed_at.ok_or_else(|| {
                            FusilladeError::Other(anyhow!("Missing failed_at for failed request"))
                        })?,
                        retry_attempt: row.retry_attempt as u32,
                    },
                    data,
                })),
                "canceled" => Ok(AnyRequest::Canceled(Request {
                    state: Canceled {
                        canceled_at: row.canceled_at.ok_or_else(|| {
                            FusilladeError::Other(anyhow!(
                                "Missing canceled_at for canceled request"
                            ))
                        })?,
                    },
                    data,
                })),
                _ => Err(FusilladeError::Other(anyhow!("Unknown state: {}", state))),
            };

            request_map.insert(request_id, any_request);
        }

        // Return results in the same order as the input ids
        Ok(ids
            .into_iter()
            .map(|id| {
                request_map
                    .remove(&id)
                    .unwrap_or_else(|| Err(FusilladeError::RequestNotFound(id)))
            })
            .collect())
    }

    // ===================================================================
    // File and Batch Management
    // ===================================================================

    #[tracing::instrument(skip(self, templates), fields(name = %name, template_count = templates.len()))]
    async fn create_file(
        &self,
        name: String,
        description: Option<String>,
        templates: Vec<RequestTemplateInput>,
    ) -> Result<FileId> {
        let mut tx =
            self.pool.begin().await.map_err(|e| {
                FusilladeError::Other(anyhow!("Failed to begin transaction: {}", e))
            })?;

        // Insert file
        let file_id = sqlx::query_scalar!(
            r#"
            INSERT INTO files (name, description)
            VALUES ($1, $2)
            RETURNING id
            "#,
            name,
            description,
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to create file: {}", e)))?;

        // Insert templates
        for template in templates {
            sqlx::query!(
                r#"
                INSERT INTO request_templates (file_id, endpoint, method, path, body, model, api_key)
                VALUES ($1, $2, $3, $4, $5, $6, $7)
                "#,
                file_id,
                template.endpoint,
                template.method,
                template.path,
                template.body,
                template.model,
                template.api_key,
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| FusilladeError::Other(anyhow!("Failed to create template: {}", e)))?;
        }

        tx.commit()
            .await
            .map_err(|e| FusilladeError::Other(anyhow!("Failed to commit transaction: {}", e)))?;

        Ok(FileId(file_id))
    }

    #[tracing::instrument(skip(self, stream))]
    async fn create_file_stream<S: Stream<Item = FileStreamItem> + Send + Unpin>(
        &self,
        mut stream: S,
    ) -> Result<FileId> {
        use futures::StreamExt;

        // Start a transaction for atomic file + templates creation
        let mut tx =
            self.pool.begin().await.map_err(|e| {
                FusilladeError::Other(anyhow!("Failed to begin transaction: {}", e))
            })?;

        // Accumulate metadata as we encounter it
        let mut metadata = FileMetadata::default();
        let mut file_id: Option<Uuid> = None;
        let mut template_count = 0;

        while let Some(item) = stream.next().await {
            match item {
                FileStreamItem::Metadata(meta) => {
                    // Accumulate metadata (later values override earlier ones)
                    if meta.filename.is_some() {
                        metadata.filename = meta.filename;
                    }
                    if meta.purpose.is_some() {
                        metadata.purpose = meta.purpose;
                    }
                    if meta.expires_after_anchor.is_some() {
                        metadata.expires_after_anchor = meta.expires_after_anchor;
                    }
                    if meta.expires_after_seconds.is_some() {
                        metadata.expires_after_seconds = meta.expires_after_seconds;
                    }
                    if meta.size_bytes.is_some() {
                        metadata.size_bytes = meta.size_bytes;
                    }
                    if meta.uploaded_by.is_some() {
                        metadata.uploaded_by = meta.uploaded_by;
                    }
                }
                FileStreamItem::Error(error_message) => {
                    // Rollback transaction and return validation error
                    tx.rollback().await.ok(); // Ignore rollback errors
                    return Err(FusilladeError::ValidationError(error_message));
                }
                FileStreamItem::Template(template) => {
                    // Create file stub on first template with minimal metadata
                    if file_id.is_none() {
                        let name = metadata
                            .filename
                            .clone()
                            .unwrap_or_else(|| format!("file_{}", uuid::Uuid::new_v4()));

                        let created_file_id = sqlx::query_scalar!(
                            r#"
                            INSERT INTO files (name)
                            VALUES ($1)
                            RETURNING id
                            "#,
                            name,
                        )
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(|e| {
                            // Check for unique constraint violation (PostgreSQL error code 23505)
                            if let sqlx::Error::Database(db_err) = &e {
                                if db_err.code().as_deref() == Some("23505") {
                                    return FusilladeError::ValidationError(format!(
                                        "A file with the name '{}' already exists",
                                        name
                                    ));
                                }
                            }
                            FusilladeError::Other(anyhow!("Failed to create file: {}", e))
                        })?;

                        file_id = Some(created_file_id);
                        tracing::debug!(
                            "Created file stub {} for streaming upload",
                            created_file_id
                        );
                    }

                    // Insert the template immediately with line_number for ordering
                    let fid = file_id.unwrap();
                    sqlx::query!(
                        r#"
                        INSERT INTO request_templates (file_id, custom_id, endpoint, method, path, body, model, api_key, line_number)
                        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                        "#,
                        fid,
                        template.custom_id,
                        template.endpoint,
                        template.method,
                        template.path,
                        template.body,
                        template.model,
                        template.api_key,
                        template_count as i32,
                    )
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| FusilladeError::Other(anyhow!("Failed to create template: {}", e)))?;

                    template_count += 1;
                }
            }
        }

        // If no templates were received, still create an empty file with whatever metadata we have
        let fid = if let Some(id) = file_id {
            id
        } else {
            let name = metadata
                .filename
                .unwrap_or_else(|| format!("file_{}", uuid::Uuid::new_v4()));

            sqlx::query_scalar!(
                r#"
                INSERT INTO files (name)
                VALUES ($1)
                RETURNING id
                "#,
                name.clone(),
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| {
                // Check for unique constraint violation (PostgreSQL error code 23505)
                if let sqlx::Error::Database(db_err) = &e {
                    if db_err.code().as_deref() == Some("23505") {
                        return FusilladeError::ValidationError(format!(
                            "A file with the name '{}' already exists",
                            name
                        ));
                    }
                }
                FusilladeError::Other(anyhow!("Failed to create file: {}", e))
            })?
        };

        // Now update the file with all the final metadata
        let size_bytes = metadata.size_bytes.unwrap_or(0);
        let status = crate::batch::FileStatus::Processed.to_string();
        let purpose = metadata.purpose.clone();

        // Calculate expires_at from expires_after if provided
        let expires_at = if let (Some(anchor), Some(seconds)) = (
            &metadata.expires_after_anchor,
            metadata.expires_after_seconds,
        ) {
            // Calculate from creation time
            if anchor == "created_at" {
                Some(Utc::now() + chrono::Duration::seconds(seconds))
            } else {
                None
            }
        } else {
            None
        };

        let uploaded_by = metadata.uploaded_by.clone();

        sqlx::query!(
            r#"
            UPDATE files
            SET size_bytes = $2, status = $3, purpose = $4, expires_at = $5, uploaded_by = $6
            WHERE id = $1
            "#,
            fid,
            size_bytes,
            status,
            purpose,
            expires_at,
            uploaded_by,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to update file metadata: {}", e)))?;

        // Commit the transaction
        tx.commit()
            .await
            .map_err(|e| FusilladeError::Other(anyhow!("Failed to commit transaction: {}", e)))?;

        tracing::info!(
            "File {} created with {} templates via streaming upload",
            fid,
            template_count
        );

        Ok(FileId(fid))
    }

    #[tracing::instrument(skip(self), fields(file_id = %file_id))]
    async fn get_file(&self, file_id: FileId) -> Result<File> {
        let row = sqlx::query!(
            r#"
            SELECT id, name, description, size_bytes, status, error_message, purpose, expires_at, deleted_at, uploaded_by, created_at, updated_at
            FROM files
            WHERE id = $1
            "#,
            *file_id as Uuid,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to fetch file: {}", e)))?
        .ok_or_else(|| FusilladeError::Other(anyhow!("File not found")))?;

        let status = row
            .status
            .parse::<crate::batch::FileStatus>()
            .map_err(|e| {
                FusilladeError::Other(anyhow!("Invalid file status '{}': {}", row.status, e))
            })?;
        let purpose = row
            .purpose
            .map(|s| s.parse::<crate::batch::Purpose>())
            .transpose()
            .map_err(|e| FusilladeError::Other(anyhow!("Invalid purpose: {}", e)))?;

        let mut file = File {
            id: FileId(row.id),
            name: row.name,
            description: row.description,
            size_bytes: row.size_bytes,
            status,
            error_message: row.error_message,
            purpose,
            expires_at: row.expires_at,
            deleted_at: row.deleted_at,
            uploaded_by: row.uploaded_by,
            created_at: row.created_at,
            updated_at: row.updated_at,
        };

        // Check and mark as expired if needed (passive expiration)
        self.check_and_mark_expired(&mut file).await?;

        Ok(file)
    }

    #[tracing::instrument(skip(self, filter), fields(uploaded_by = ?filter.uploaded_by, status = ?filter.status, purpose = ?filter.purpose))]
    async fn list_files(&self, filter: crate::batch::FileFilter) -> Result<Vec<File>> {
        // Build WHERE clause based on filter
        let mut where_clauses = Vec::new();
        let mut params: Vec<Option<&str>> = Vec::new();

        if filter.uploaded_by.is_some() {
            where_clauses.push(format!("uploaded_by = ${}", params.len() + 1));
            params.push(filter.uploaded_by.as_deref());
        }

        if filter.status.is_some() {
            where_clauses.push(format!("status = ${}", params.len() + 1));
            params.push(filter.status.as_deref());
        }

        if filter.purpose.is_some() {
            where_clauses.push(format!("purpose = ${}", params.len() + 1));
            params.push(filter.purpose.as_deref());
        }

        let where_clause = if where_clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", where_clauses.join(" AND "))
        };

        let query = format!(
            r#"
            SELECT id, name, description, size_bytes, status, error_message, purpose, expires_at, deleted_at, uploaded_by, created_at, updated_at
            FROM files
            {}
            ORDER BY created_at DESC
            "#,
            where_clause
        );

        let mut query_builder = sqlx::query(&query);

        if let Some(uploaded_by) = filter.uploaded_by {
            query_builder = query_builder.bind(uploaded_by);
        }
        if let Some(status) = filter.status {
            query_builder = query_builder.bind(status);
        }
        if let Some(purpose) = filter.purpose {
            query_builder = query_builder.bind(purpose);
        }

        let rows = query_builder
            .fetch_all(&self.pool)
            .await
            .map_err(|e| FusilladeError::Other(anyhow!("Failed to list files: {}", e)))?;

        let mut files = Vec::new();

        for row in rows {
            let id: Uuid = row
                .try_get("id")
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to read id: {}", e)))?;
            let name: String = row
                .try_get("name")
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to read name: {}", e)))?;
            let description: Option<String> = row
                .try_get("description")
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to read description: {}", e)))?;
            let size_bytes: i64 = row
                .try_get("size_bytes")
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to read size_bytes: {}", e)))?;
            let status_str: String = row
                .try_get("status")
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to read status: {}", e)))?;
            let status = status_str
                .parse::<crate::batch::FileStatus>()
                .map_err(|e| {
                    FusilladeError::Other(anyhow!("Invalid file status '{}': {}", status_str, e))
                })?;
            let error_message: Option<String> = row.try_get("error_message").map_err(|e| {
                FusilladeError::Other(anyhow!("Failed to read error_message: {}", e))
            })?;
            let purpose_str: Option<String> = row
                .try_get("purpose")
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to read purpose: {}", e)))?;
            let purpose = purpose_str
                .map(|s| s.parse::<crate::batch::Purpose>())
                .transpose()
                .map_err(|e| FusilladeError::Other(anyhow!("Invalid purpose: {}", e)))?;
            let expires_at: Option<chrono::DateTime<Utc>> = row
                .try_get("expires_at")
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to read expires_at: {}", e)))?;
            let deleted_at: Option<chrono::DateTime<Utc>> = row
                .try_get("deleted_at")
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to read deleted_at: {}", e)))?;
            let uploaded_by: Option<String> = row
                .try_get("uploaded_by")
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to read uploaded_by: {}", e)))?;
            let created_at: chrono::DateTime<Utc> = row
                .try_get("created_at")
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to read created_at: {}", e)))?;
            let updated_at: chrono::DateTime<Utc> = row
                .try_get("updated_at")
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to read updated_at: {}", e)))?;

            let mut file = File {
                id: FileId(id),
                name,
                description,
                size_bytes,
                status,
                error_message,
                purpose,
                expires_at,
                deleted_at,
                uploaded_by,
                created_at,
                updated_at,
            };

            // Check and mark as expired if needed (passive expiration)
            self.check_and_mark_expired(&mut file).await?;

            files.push(file);
        }

        Ok(files)
    }

    async fn get_file_templates(&self, file_id: FileId) -> Result<Vec<RequestTemplate>> {
        let rows = sqlx::query!(
            r#"
            SELECT id, file_id, custom_id, endpoint, method, path, body, model, api_key, created_at, updated_at
            FROM request_templates
            WHERE file_id = $1
            ORDER BY line_number ASC
            "#,
            *file_id as Uuid,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to fetch templates: {}", e)))?;

        Ok(rows
            .into_iter()
            .map(|row| RequestTemplate {
                id: TemplateId(row.id),
                file_id: FileId(row.file_id),
                custom_id: row.custom_id,
                endpoint: row.endpoint,
                method: row.method,
                path: row.path,
                body: row.body,
                model: row.model,
                api_key: row.api_key,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
            .collect())
    }

    #[tracing::instrument(skip(self), fields(file_id = %file_id))]
    fn get_file_content_stream(
        &self,
        file_id: FileId,
    ) -> Pin<Box<dyn Stream<Item = Result<RequestTemplateInput>> + Send>> {
        let pool = self.pool.clone();
        let (tx, rx) = mpsc::channel(self.download_buffer_size);

        tokio::spawn(async move {
            // Use keyset pagination to fetch in batches with back-pressure
            const BATCH_SIZE: i64 = 1000;
            let mut last_line_number: i32 = -1;

            loop {
                // Fetch next batch using keyset pagination
                let template_batch = sqlx::query!(
                    r#"
                    SELECT custom_id, endpoint, method, path, body, model, api_key, line_number
                    FROM request_templates
                    WHERE file_id = $1 AND line_number > $2
                    ORDER BY line_number ASC
                    LIMIT $3
                    "#,
                    *file_id as Uuid,
                    last_line_number,
                    BATCH_SIZE,
                )
                .fetch_all(&pool)
                .await;

                match template_batch {
                    Ok(templates) => {
                        if templates.is_empty() {
                            // No more rows, we're done
                            break;
                        }

                        tracing::debug!(
                            "Fetched batch of {} templates, line_numbers {}-{}",
                            templates.len(),
                            templates.first().map(|r| r.line_number).unwrap_or(0),
                            templates.last().map(|r| r.line_number).unwrap_or(0)
                        );

                        // Send each template in the batch
                        for row in templates {
                            last_line_number = row.line_number;

                            let template = RequestTemplateInput {
                                custom_id: row.custom_id,
                                endpoint: row.endpoint,
                                method: row.method,
                                path: row.path,
                                body: row.body,
                                model: row.model,
                                api_key: row.api_key,
                            };
                            if tx.send(Ok(template)).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx
                            .send(Err(FusilladeError::Other(anyhow!(
                                "Failed to fetch template batch: {}",
                                e
                            ))))
                            .await;
                        return;
                    }
                }
            }
        });

        Box::pin(ReceiverStream::new(rx))
    }

    #[tracing::instrument(skip(self), fields(file_id = %file_id))]
    async fn delete_file(&self, file_id: FileId) -> Result<()> {
        let rows_affected = sqlx::query!(
            r#"
            DELETE FROM files
            WHERE id = $1
            "#,
            *file_id as Uuid,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to delete file: {}", e)))?
        .rows_affected();

        if rows_affected == 0 {
            return Err(FusilladeError::Other(anyhow!("File not found")));
        }

        Ok(())
    }

    async fn create_batch(&self, file_id: FileId) -> Result<BatchId> {
        let mut tx =
            self.pool.begin().await.map_err(|e| {
                FusilladeError::Other(anyhow!("Failed to begin transaction: {}", e))
            })?;

        // Get templates
        let templates = sqlx::query!(
            r#"
            SELECT id, custom_id, endpoint, method, path, body, model, api_key
            FROM request_templates
            WHERE file_id = $1
            "#,
            *file_id as Uuid,
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to fetch templates: {}", e)))?;

        if templates.is_empty() {
            return Err(FusilladeError::Other(anyhow!(
                "Cannot create batch from file with no templates"
            )));
        }

        // Create batch
        let batch_id = sqlx::query_scalar!(
            r#"
            INSERT INTO batches (file_id)
            VALUES ($1)
            RETURNING id
            "#,
            *file_id as Uuid,
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to create batch: {}", e)))?;

        // Create executions from templates
        for template in templates {
            sqlx::query!(
                r#"
                INSERT INTO requests (
                    batch_id, template_id, state,
                    custom_id, endpoint, method, path, body, model, api_key,
                    retry_attempt
                )
                VALUES ($1, $2, 'pending', $3, $4, $5, $6, $7, $8, $9, 0)
                "#,
                batch_id,
                template.id,
                template.custom_id,
                template.endpoint,
                template.method,
                template.path,
                template.body,
                template.model,
                template.api_key,
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| FusilladeError::Other(anyhow!("Failed to create execution: {}", e)))?;
        }

        tx.commit()
            .await
            .map_err(|e| FusilladeError::Other(anyhow!("Failed to commit transaction: {}", e)))?;

        Ok(BatchId(batch_id))
    }

    async fn get_batch_status(&self, batch_id: BatchId) -> Result<BatchStatus> {
        let row = sqlx::query!(
            r#"
            SELECT * FROM batch_status
            WHERE batch_id = $1
            "#,
            *batch_id as Uuid,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to fetch batch status: {}", e)))?
        .ok_or_else(|| FusilladeError::Other(anyhow!("Batch not found")))?;

        Ok(BatchStatus {
            batch_id: BatchId(row.batch_id.ok_or_else(|| {
                FusilladeError::Other(anyhow!("Batch status view missing batch_id"))
            })?),
            file_id: FileId(row.file_id.ok_or_else(|| {
                FusilladeError::Other(anyhow!("Batch status view missing file_id"))
            })?),
            file_name: row.file_name.ok_or_else(|| {
                FusilladeError::Other(anyhow!("Batch status view missing file_name"))
            })?,
            total_requests: row.total_requests.unwrap_or(0),
            pending_requests: row.pending_requests.unwrap_or(0),
            in_progress_requests: row.in_progress_requests.unwrap_or(0),
            completed_requests: row.completed_requests.unwrap_or(0),
            failed_requests: row.failed_requests.unwrap_or(0),
            canceled_requests: row.canceled_requests.unwrap_or(0),
            started_at: row.started_at,
            last_updated_at: row.last_updated_at,
            created_at: row.created_at.ok_or_else(|| {
                FusilladeError::Other(anyhow!("Batch status view missing created_at"))
            })?,
        })
    }

    async fn list_file_batches(&self, file_id: FileId) -> Result<Vec<BatchStatus>> {
        let rows = sqlx::query!(
            r#"
            SELECT * FROM batch_status
            WHERE file_id = $1
            ORDER BY created_at DESC
            "#,
            *file_id as Uuid,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to list batches: {}", e)))?;

        Ok(rows
            .into_iter()
            .filter_map(|row| {
                Some(BatchStatus {
                    batch_id: BatchId(row.batch_id?),
                    file_id: FileId(row.file_id?),
                    file_name: row.file_name?,
                    total_requests: row.total_requests.unwrap_or(0),
                    pending_requests: row.pending_requests.unwrap_or(0),
                    in_progress_requests: row.in_progress_requests.unwrap_or(0),
                    completed_requests: row.completed_requests.unwrap_or(0),
                    failed_requests: row.failed_requests.unwrap_or(0),
                    canceled_requests: row.canceled_requests.unwrap_or(0),
                    started_at: row.started_at,
                    last_updated_at: row.last_updated_at,
                    created_at: row.created_at?,
                })
            })
            .collect())
    }

    async fn cancel_batch(&self, batch_id: BatchId) -> Result<()> {
        let now = Utc::now();

        sqlx::query!(
            r#"
            UPDATE requests
            SET state = 'canceled', canceled_at = $2
            WHERE batch_id = $1
                AND state IN ('pending', 'claimed', 'processing')
            "#,
            *batch_id as Uuid,
            now,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to cancel batch: {}", e)))?;

        Ok(())
    }

    async fn get_batch_requests(&self, batch_id: BatchId) -> Result<Vec<AnyRequest>> {
        let rows = sqlx::query!(
            r#"
            SELECT
                id, batch_id, template_id, state, endpoint, method, path, body, model, api_key,
                retry_attempt, not_before, daemon_id, claimed_at, started_at,
                response_status, response_body, completed_at, error, failed_at, canceled_at
            FROM requests
            WHERE batch_id = $1
            ORDER BY created_at ASC
            "#,
            *batch_id as Uuid,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to fetch batch executions: {}", e)))?;

        let mut results = Vec::new();

        for row in rows {
            let data = RequestData {
                id: RequestId(row.id),
                batch_id: BatchId(row.batch_id),
                template_id: TemplateId(row.template_id),
                endpoint: row.endpoint,
                method: row.method,
                path: row.path,
                body: row.body,
                model: row.model,
                api_key: row.api_key,
            };

            let state = &row.state;

            let any_request = match state.as_str() {
                "pending" => AnyRequest::Pending(Request {
                    state: Pending {
                        retry_attempt: row.retry_attempt as u32,
                        not_before: row.not_before,
                    },
                    data,
                }),
                "claimed" => AnyRequest::Claimed(Request {
                    state: Claimed {
                        daemon_id: DaemonId(row.daemon_id.ok_or_else(|| {
                            FusilladeError::Other(anyhow!(
                                "Missing daemon_id for claimed execution"
                            ))
                        })?),
                        claimed_at: row.claimed_at.ok_or_else(|| {
                            FusilladeError::Other(anyhow!(
                                "Missing claimed_at for claimed execution"
                            ))
                        })?,
                        retry_attempt: row.retry_attempt as u32,
                    },
                    data,
                }),
                "processing" => {
                    let (_tx, rx) = tokio::sync::mpsc::channel(1);
                    let abort_handle = tokio::spawn(async {}).abort_handle();
                    AnyRequest::Processing(Request {
                        state: Processing {
                            daemon_id: DaemonId(row.daemon_id.ok_or_else(|| {
                                FusilladeError::Other(anyhow!(
                                    "Missing daemon_id for processing execution"
                                ))
                            })?),
                            claimed_at: row.claimed_at.ok_or_else(|| {
                                FusilladeError::Other(anyhow!(
                                    "Missing claimed_at for processing execution"
                                ))
                            })?,
                            started_at: row.started_at.ok_or_else(|| {
                                FusilladeError::Other(anyhow!(
                                    "Missing started_at for processing execution"
                                ))
                            })?,
                            retry_attempt: row.retry_attempt as u32,
                            result_rx: Arc::new(Mutex::new(rx)),
                            abort_handle,
                        },
                        data,
                    })
                }
                "completed" => AnyRequest::Completed(Request {
                    state: Completed {
                        response_status: row.response_status.ok_or_else(|| {
                            FusilladeError::Other(anyhow!(
                                "Missing response_status for completed execution"
                            ))
                        })? as u16,
                        response_body: row.response_body.ok_or_else(|| {
                            FusilladeError::Other(anyhow!(
                                "Missing response_body for completed execution"
                            ))
                        })?,
                        claimed_at: row.claimed_at.ok_or_else(|| {
                            FusilladeError::Other(anyhow!(
                                "Missing claimed_at for completed execution"
                            ))
                        })?,
                        started_at: row.started_at.ok_or_else(|| {
                            FusilladeError::Other(anyhow!(
                                "Missing started_at for completed execution"
                            ))
                        })?,
                        completed_at: row.completed_at.ok_or_else(|| {
                            FusilladeError::Other(anyhow!(
                                "Missing completed_at for completed execution"
                            ))
                        })?,
                    },
                    data,
                }),
                "failed" => AnyRequest::Failed(Request {
                    state: Failed {
                        error: row.error.ok_or_else(|| {
                            FusilladeError::Other(anyhow!("Missing error for failed execution"))
                        })?,
                        failed_at: row.failed_at.ok_or_else(|| {
                            FusilladeError::Other(anyhow!("Missing failed_at for failed execution"))
                        })?,
                        retry_attempt: row.retry_attempt as u32,
                    },
                    data,
                }),
                "canceled" => AnyRequest::Canceled(Request {
                    state: Canceled {
                        canceled_at: row.canceled_at.ok_or_else(|| {
                            FusilladeError::Other(anyhow!(
                                "Missing canceled_at for canceled execution"
                            ))
                        })?,
                    },
                    data,
                }),
                _ => {
                    return Err(FusilladeError::Other(anyhow!("Unknown state: {}", state)));
                }
            };

            results.push(any_request);
        }

        Ok(results)
    }

    fn get_request_updates(
        &self,
        id_filter: Option<Vec<RequestId>>,
    ) -> std::pin::Pin<Box<dyn Stream<Item = Result<Result<AnyRequest>>> + Send>> {
        let pool = self.pool.clone();
        let (tx, rx) = mpsc::channel(100);

        tokio::spawn(async move {
            // Create a listener for Postgres NOTIFY events
            let mut listener = match PgListener::connect_with(&pool)
                .await
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to create listener: {}", e)))
            {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to create listener");
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            };

            // Listen on the request_updates channel
            if let Err(e) = listener.listen("request_updates").await {
                tracing::error!(error = %e, "Failed to listen on request_updates channel");
                let _ = tx
                    .send(Err(FusilladeError::Other(anyhow::anyhow!(
                        "Failed to listen: {}",
                        e
                    ))))
                    .await;
                return;
            }

            tracing::info!("Listening for request updates");

            loop {
                match listener.recv().await {
                    Ok(notification) => {
                        // Parse the JSON payload
                        let payload = notification.payload();

                        // The payload contains: { "id": "...", "state": "...", "updated_at": "..." }
                        // We need to parse the ID and fetch the full request from storage
                        let parsed: serde_json::Result<serde_json::Value> =
                            serde_json::from_str(payload);

                        match parsed {
                            Ok(json) => {
                                if let Some(id_str) = json.get("id").and_then(|v| v.as_str()) {
                                    // Parse the UUID
                                    if let Ok(uuid) = uuid::Uuid::parse_str(id_str) {
                                        let request_id = RequestId(uuid);

                                        // Apply filter if specified
                                        if let Some(ref filter) = id_filter {
                                            if !filter.contains(&request_id) {
                                                // Skip this update - not in filter
                                                continue;
                                            }
                                        }

                                        // Fetch the full request from storage by querying directly
                                        let fetch_result: Result<Vec<Result<AnyRequest>>> =
                                            async {
                                                let uuid_ids = [*request_id];
                                                let rows = sqlx::query!(
                                                    r#"
                                                SELECT
                                                    id, batch_id, template_id, state, endpoint, method, path, body, model, api_key,
                                                    retry_attempt, not_before, daemon_id, claimed_at, started_at,
                                                    response_status, response_body, completed_at, error, failed_at, canceled_at
                                                FROM requests
                                                WHERE id = ANY($1)
                                                "#,
                                                    &uuid_ids[..],
                                                )
                                                .fetch_all(&pool)
                                                .await
                                                .map_err(|e| FusilladeError::Other(anyhow!("Failed to fetch requests: {}", e)))?;

                                                let mut results = Vec::new();
                                                for row in rows {
                                                    let data = RequestData {
                                                        id: RequestId(row.id),
                                                        batch_id: BatchId(row.batch_id),
                                                        template_id: TemplateId(row.template_id),
                                                        endpoint: row.endpoint,
                                                        method: row.method,
                                                        path: row.path,
                                                        body: row.body,
                                                        model: row.model,
                                                        api_key: row.api_key,
                                                    };

                                                    let state = &row.state;
                                                    let any_request =
                                                        match state.as_str() {
                                                            "pending" => Ok(AnyRequest::Pending(Request {
                                                                state: Pending {
                                                                    retry_attempt: row.retry_attempt as u32,
                                                                    not_before: row.not_before,
                                                                },
                                                                data,
                                                            })),
                                                            "claimed" => Ok(AnyRequest::Claimed(Request {
                                                                state: Claimed {
                                                                    daemon_id: DaemonId(row.daemon_id.ok_or_else(|| {
                                                                        FusilladeError::Other(anyhow!("Missing daemon_id"))
                                                                    })?),
                                                                    claimed_at: row.claimed_at.ok_or_else(|| {
                                                                        FusilladeError::Other(anyhow!("Missing claimed_at"))
                                                                    })?,
                                                                    retry_attempt: row.retry_attempt as u32,
                                                                },
                                                                data,
                                                            })),
                                                            "processing" => {
                                                                let (_tx, rx) = mpsc::channel(1);
                                                                let abort_handle = tokio::spawn(async {}).abort_handle();
                                                                Ok(AnyRequest::Processing(Request {
                                                                    state: Processing {
                                                                        daemon_id: DaemonId(row.daemon_id.ok_or_else(|| {
                                                                            FusilladeError::Other(anyhow!("Missing daemon_id"))
                                                                        })?),
                                                                        claimed_at: row.claimed_at.ok_or_else(|| {
                                                                            FusilladeError::Other(anyhow!("Missing claimed_at"))
                                                                        })?,
                                                                        started_at: row.started_at.ok_or_else(|| {
                                                                            FusilladeError::Other(anyhow!("Missing started_at"))
                                                                        })?,
                                                                        retry_attempt: row.retry_attempt as u32,
                                                                        result_rx: Arc::new(Mutex::new(rx)),
                                                                        abort_handle,
                                                                    },
                                                                    data,
                                                                }))
                                                            }
                                                            "completed" => Ok(AnyRequest::Completed(Request {
                                                                state: Completed {
                                                                    response_status: row.response_status.ok_or_else(|| {
                                                                        FusilladeError::Other(anyhow!("Missing response_status"))
                                                                    })?
                                                                        as u16,
                                                                    response_body: row.response_body.ok_or_else(|| {
                                                                        FusilladeError::Other(anyhow!("Missing response_body"))
                                                                    })?,
                                                                    claimed_at: row.claimed_at.ok_or_else(|| {
                                                                        FusilladeError::Other(anyhow!("Missing claimed_at"))
                                                                    })?,
                                                                    started_at: row.started_at.ok_or_else(|| {
                                                                        FusilladeError::Other(anyhow!("Missing started_at"))
                                                                    })?,
                                                                    completed_at: row.completed_at.ok_or_else(|| {
                                                                        FusilladeError::Other(anyhow!("Missing completed_at"))
                                                                    })?,
                                                                },
                                                                data,
                                                            })),
                                                            "failed" => Ok(AnyRequest::Failed(Request {
                                                                state: Failed {
                                                                    error: row
                                                                        .error
                                                                        .ok_or_else(|| FusilladeError::Other(anyhow!("Missing error")))?,
                                                                    failed_at: row.failed_at.ok_or_else(|| {
                                                                        FusilladeError::Other(anyhow!("Missing failed_at"))
                                                                    })?,
                                                                    retry_attempt: row.retry_attempt as u32,
                                                                },
                                                                data,
                                                            })),
                                                            "canceled" => Ok(AnyRequest::Canceled(Request {
                                                                state: Canceled {
                                                                    canceled_at: row.canceled_at.ok_or_else(|| {
                                                                        FusilladeError::Other(anyhow!("Missing canceled_at"))
                                                                    })?,
                                                                },
                                                                data,
                                                            })),
                                                            _ => Err(FusilladeError::Other(anyhow!("Unknown state: {}", state))),
                                                        };
                                                    results.push(any_request);
                                                }
                                                Ok(results)
                                            }
                                            .await;

                                        match fetch_result {
                                            Ok(results) => {
                                                if let Some(result) = results.into_iter().next() {
                                                    if tx.send(Ok(result)).await.is_err() {
                                                        return;
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    error = %e,
                                                    request_id = %request_id,
                                                    "Failed to fetch request after notification"
                                                );
                                                if tx.send(Err(e)).await.is_err() {
                                                    return;
                                                }
                                            }
                                        }
                                    } else {
                                        tracing::warn!(
                                            id_str = id_str,
                                            "Failed to parse UUID from notification"
                                        );
                                    }
                                } else {
                                    tracing::warn!(
                                        payload = payload,
                                        "Notification payload missing 'id' field"
                                    );
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    payload = payload,
                                    "Failed to parse notification payload"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Error receiving notification");
                        if tx
                            .send(Err(FusilladeError::Other(anyhow::anyhow!(
                                "Notification error: {}",
                                e
                            ))))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        // Don't return - keep trying to receive notifications
                    }
                }
            }
        });

        Box::pin(ReceiverStream::new(rx))
    }
}

// Implement DaemonExecutor trait
#[async_trait]
impl<H: HttpClient + 'static> DaemonExecutor<H> for PostgresRequestManager<H> {
    fn http_client(&self) -> &Arc<H> {
        &self.http_client
    }

    fn config(&self) -> &DaemonConfig {
        &self.config
    }

    fn run(self: Arc<Self>) -> Result<JoinHandle<Result<()>>> {
        tracing::info!("Starting PostgreSQL request manager daemon");

        let daemon = Arc::new(Daemon::new(
            self.clone(),
            self.http_client.clone(),
            self.config.clone(),
        ));

        let handle = tokio::spawn(async move { daemon.run().await });

        tracing::info!("Daemon spawned successfully");

        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::MockHttpClient;

    #[sqlx::test]
    async fn test_create_and_get_file(pool: sqlx::PgPool) {
        let http_client = Arc::new(MockHttpClient::new());
        let manager = PostgresRequestManager::with_client(pool.clone(), http_client);

        // Create a file with templates
        let file_id = manager
            .create_file(
                "test-file".to_string(),
                Some("A test file".to_string()),
                vec![
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/completions".to_string(),
                        body: r#"{"model":"gpt-4"}"#.to_string(),
                        model: "gpt-4".to_string(),
                        api_key: "key1".to_string(),
                    },
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/completions".to_string(),
                        body: r#"{"model":"gpt-3.5"}"#.to_string(),
                        model: "gpt-3.5".to_string(),
                        api_key: "key2".to_string(),
                    },
                ],
            )
            .await
            .expect("Failed to create file");

        // Get the file back
        let file = manager.get_file(file_id).await.expect("Failed to get file");

        assert_eq!(file.id, file_id);
        assert_eq!(file.name, "test-file");
        assert_eq!(file.description, Some("A test file".to_string()));

        // Get templates for the file
        let templates = manager
            .get_file_templates(file_id)
            .await
            .expect("Failed to get templates");

        assert_eq!(templates.len(), 2);
        assert_eq!(templates[0].model, "gpt-4");
        assert_eq!(templates[1].model, "gpt-3.5");
    }

    #[sqlx::test]
    async fn test_create_batch_and_get_status(pool: sqlx::PgPool) {
        let http_client = Arc::new(MockHttpClient::new());
        let manager = PostgresRequestManager::with_client(pool.clone(), http_client);

        // Create a file with 3 templates
        let file_id = manager
            .create_file(
                "batch-test".to_string(),
                None,
                vec![
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/test".to_string(),
                        body: r#"{"prompt":"1"}"#.to_string(),
                        model: "gpt-4".to_string(),
                        api_key: "key".to_string(),
                    },
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/test".to_string(),
                        body: r#"{"prompt":"2"}"#.to_string(),
                        model: "gpt-4".to_string(),
                        api_key: "key".to_string(),
                    },
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/test".to_string(),
                        body: r#"{"prompt":"3"}"#.to_string(),
                        model: "gpt-4".to_string(),
                        api_key: "key".to_string(),
                    },
                ],
            )
            .await
            .expect("Failed to create file");

        // Create a batch
        let batch_id = manager
            .create_batch(file_id)
            .await
            .expect("Failed to create batch");

        // Get batch status
        let status = manager
            .get_batch_status(batch_id)
            .await
            .expect("Failed to get batch status");

        assert_eq!(status.batch_id, batch_id);
        assert_eq!(status.file_id, file_id);
        assert_eq!(status.file_name, "batch-test");
        assert_eq!(status.total_requests, 3);
        assert_eq!(status.pending_requests, 3);
        assert_eq!(status.completed_requests, 0);
        assert_eq!(status.failed_requests, 0);

        // Get batch requests
        let requests = manager
            .get_batch_requests(batch_id)
            .await
            .expect("Failed to get batch requests");

        assert_eq!(requests.len(), 3);
        for request in requests {
            assert!(request.is_pending());
        }
    }

    #[sqlx::test]
    async fn test_claim_requests(pool: sqlx::PgPool) {
        let http_client = Arc::new(MockHttpClient::new());
        let manager = PostgresRequestManager::with_client(pool.clone(), http_client);

        // Create a file with 5 templates
        let file_id = manager
            .create_file(
                "claim-test".to_string(),
                None,
                (0..5)
                    .map(|i| RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/test".to_string(),
                        body: format!(r#"{{"n":{}}}"#, i),
                        model: "test".to_string(),
                        api_key: "key".to_string(),
                    })
                    .collect(),
            )
            .await
            .unwrap();

        let batch_id = manager.create_batch(file_id).await.unwrap();

        let daemon_id = DaemonId::from(Uuid::new_v4());

        // Claim 3 requests
        let claimed = manager
            .claim_requests(3, daemon_id)
            .await
            .expect("Failed to claim requests");

        assert_eq!(claimed.len(), 3);
        for request in &claimed {
            assert_eq!(request.state.daemon_id, daemon_id);
            assert_eq!(request.state.retry_attempt, 0);
        }

        // Try to claim again - should get the remaining 2
        let claimed2 = manager
            .claim_requests(10, daemon_id)
            .await
            .expect("Failed to claim requests");

        assert_eq!(claimed2.len(), 2);

        // Verify batch status shows claimed requests
        let status = manager.get_batch_status(batch_id).await.unwrap();
        assert_eq!(status.total_requests, 5);
        assert_eq!(status.pending_requests, 0);
        assert_eq!(status.in_progress_requests, 5); // All claimed
    }

    #[sqlx::test]
    async fn test_cancel_batch(pool: sqlx::PgPool) {
        let http_client = Arc::new(MockHttpClient::new());
        let manager = PostgresRequestManager::with_client(pool.clone(), http_client);

        // Create a file with 3 templates
        let file_id = manager
            .create_file(
                "cancel-test".to_string(),
                None,
                (0..3)
                    .map(|i| RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/test".to_string(),
                        body: format!(r#"{{"n":{}}}"#, i),
                        model: "test".to_string(),
                        api_key: "key".to_string(),
                    })
                    .collect(),
            )
            .await
            .unwrap();

        let batch_id = manager.create_batch(file_id).await.unwrap();

        // Verify all are pending
        let status_before = manager.get_batch_status(batch_id).await.unwrap();
        assert_eq!(status_before.pending_requests, 3);
        assert_eq!(status_before.canceled_requests, 0);

        // Cancel the batch
        manager.cancel_batch(batch_id).await.unwrap();

        // Verify all are canceled
        let status_after = manager.get_batch_status(batch_id).await.unwrap();
        assert_eq!(status_after.pending_requests, 0);
        assert_eq!(status_after.canceled_requests, 3);

        // Get the actual requests to verify their state
        let requests = manager.get_batch_requests(batch_id).await.unwrap();
        assert_eq!(requests.len(), 3);
        for request in requests {
            assert!(matches!(request, AnyRequest::Canceled(_)));
        }
    }

    #[sqlx::test]
    async fn test_cancel_individual_requests(pool: sqlx::PgPool) {
        let http_client = Arc::new(MockHttpClient::new());
        let manager = PostgresRequestManager::with_client(pool.clone(), http_client);

        // Create a file with 5 templates
        let file_id = manager
            .create_file(
                "individual-cancel-test".to_string(),
                None,
                (0..5)
                    .map(|i| RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/test".to_string(),
                        body: format!(r#"{{"n":{}}}"#, i),
                        model: "test".to_string(),
                        api_key: "key".to_string(),
                    })
                    .collect(),
            )
            .await
            .unwrap();

        let batch_id = manager.create_batch(file_id).await.unwrap();

        // Get all request IDs
        let requests = manager.get_batch_requests(batch_id).await.unwrap();
        let request_ids: Vec<_> = requests.iter().map(|r| r.id()).collect();

        // Cancel the first 3 requests
        let results = manager
            .cancel_requests(request_ids[0..3].to_vec())
            .await
            .unwrap();

        // All 3 cancellations should succeed
        for result in results {
            assert!(result.is_ok());
        }

        // Verify batch status
        let status = manager.get_batch_status(batch_id).await.unwrap();
        assert_eq!(status.pending_requests, 2);
        assert_eq!(status.canceled_requests, 3);

        // Verify the requests
        let all_requests = manager.get_batch_requests(batch_id).await.unwrap();
        let canceled_count = all_requests
            .iter()
            .filter(|r| matches!(r, AnyRequest::Canceled(_)))
            .count();
        assert_eq!(canceled_count, 3);
    }

    #[sqlx::test]
    async fn test_list_files(pool: sqlx::PgPool) {
        let http_client = Arc::new(MockHttpClient::new());
        let manager = PostgresRequestManager::with_client(pool.clone(), http_client);

        // Create 3 files
        let file1_id = manager
            .create_file("file1".to_string(), Some("First".to_string()), vec![])
            .await
            .unwrap();

        let file2_id = manager
            .create_file("file2".to_string(), Some("Second".to_string()), vec![])
            .await
            .unwrap();

        let file3_id = manager
            .create_file("file3".to_string(), None, vec![])
            .await
            .unwrap();

        // List all files
        let files = manager
            .list_files(crate::batch::FileFilter::default())
            .await
            .unwrap();

        // Should have at least our 3 files (may have more from other tests)
        assert!(files.len() >= 3);

        // Verify our files are present
        let file_ids: Vec<_> = files.iter().map(|f| f.id).collect();
        assert!(file_ids.contains(&file1_id));
        assert!(file_ids.contains(&file2_id));
        assert!(file_ids.contains(&file3_id));

        // Verify names and descriptions
        let file1 = files.iter().find(|f| f.id == file1_id).unwrap();
        assert_eq!(file1.name, "file1");
        assert_eq!(file1.description, Some("First".to_string()));

        let file3 = files.iter().find(|f| f.id == file3_id).unwrap();
        assert_eq!(file3.name, "file3");
        assert_eq!(file3.description, None);
    }

    #[sqlx::test]
    async fn test_list_file_batches(pool: sqlx::PgPool) {
        let http_client = Arc::new(MockHttpClient::new());
        let manager = PostgresRequestManager::with_client(pool.clone(), http_client);

        // Create a file with templates
        let file_id = manager
            .create_file(
                "batch-list-test".to_string(),
                None,
                vec![RequestTemplateInput {
                    custom_id: None,
                    endpoint: "https://api.example.com".to_string(),
                    method: "POST".to_string(),
                    path: "/test".to_string(),
                    body: "{}".to_string(),
                    model: "test".to_string(),
                    api_key: "key".to_string(),
                }],
            )
            .await
            .unwrap();

        // Create 3 batches
        let batch1_id = manager.create_batch(file_id).await.unwrap();
        let batch2_id = manager.create_batch(file_id).await.unwrap();
        let batch3_id = manager.create_batch(file_id).await.unwrap();

        // List batches for this file
        let batches = manager.list_file_batches(file_id).await.unwrap();

        assert_eq!(batches.len(), 3);

        // Verify all batch IDs are present
        let batch_ids: Vec<_> = batches.iter().map(|b| b.batch_id).collect();
        assert!(batch_ids.contains(&batch1_id));
        assert!(batch_ids.contains(&batch2_id));
        assert!(batch_ids.contains(&batch3_id));

        // Verify each batch has 1 pending request
        for batch in batches {
            assert_eq!(batch.total_requests, 1);
            assert_eq!(batch.pending_requests, 1);
        }
    }

    #[sqlx::test]
    async fn test_delete_file_cascade(pool: sqlx::PgPool) {
        let http_client = Arc::new(MockHttpClient::new());
        let manager = PostgresRequestManager::with_client(pool.clone(), http_client);

        // Create a file with templates
        let file_id = manager
            .create_file(
                "delete-test".to_string(),
                None,
                vec![
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/test".to_string(),
                        body: r#"{"n":1}"#.to_string(),
                        model: "test".to_string(),
                        api_key: "key".to_string(),
                    },
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/test".to_string(),
                        body: r#"{"n":2}"#.to_string(),
                        model: "test".to_string(),
                        api_key: "key".to_string(),
                    },
                ],
            )
            .await
            .unwrap();

        // Create a batch
        let batch_id = manager.create_batch(file_id).await.unwrap();

        // Verify the batch exists
        let status_before = manager.get_batch_status(batch_id).await;
        assert!(status_before.is_ok());

        // Delete the file
        manager.delete_file(file_id).await.unwrap();

        // Verify file is gone
        let file_result = manager.get_file(file_id).await;
        assert!(file_result.is_err());

        // Verify batch is gone (cascade delete)
        let status_after = manager.get_batch_status(batch_id).await;
        assert!(status_after.is_err());
    }

    #[sqlx::test]
    async fn test_unclaim_stale_claimed_requests(pool: sqlx::PgPool) {
        let http_client = Arc::new(MockHttpClient::new());

        // Create manager with 1-second claim timeout for testing
        let config = crate::daemon::DaemonConfig {
            claim_timeout_ms: 1000,       // 1 second
            processing_timeout_ms: 60000, // 1 minute
            ..Default::default()
        };
        let manager = Arc::new(
            PostgresRequestManager::with_client(pool.clone(), http_client).with_config(config),
        );

        // Create a file and batch
        let file_id = manager
            .create_file(
                "stale-test".to_string(),
                None,
                vec![RequestTemplateInput {
                    custom_id: None,
                    endpoint: "https://api.example.com".to_string(),
                    method: "POST".to_string(),
                    path: "/test".to_string(),
                    body: "{}".to_string(),
                    model: "test".to_string(),
                    api_key: "key".to_string(),
                }],
            )
            .await
            .unwrap();

        let batch_id = manager.create_batch(file_id).await.unwrap();

        // Claim the request with daemon1
        let daemon1_id = DaemonId::from(Uuid::new_v4());
        let claimed = manager.claim_requests(1, daemon1_id).await.unwrap();
        assert_eq!(claimed.len(), 1);
        let request_id = claimed[0].data.id;

        // Manually set claimed_at to 3 seconds ago (past the 1s timeout)
        sqlx::query!(
            "UPDATE requests SET claimed_at = NOW() - INTERVAL '3 seconds' WHERE id = $1",
            *request_id as Uuid
        )
        .execute(&pool)
        .await
        .unwrap();

        // Now daemon2 tries to claim - should unclaim the stale request and re-claim it
        let daemon2_id = DaemonId::from(Uuid::new_v4());
        let reclaimed = manager.claim_requests(1, daemon2_id).await.unwrap();

        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].data.id, request_id);
        assert_eq!(reclaimed[0].state.daemon_id, daemon2_id);

        // Verify the request is now claimed by daemon2
        let status = manager.get_batch_status(batch_id).await.unwrap();
        assert_eq!(status.in_progress_requests, 1);
    }

    #[sqlx::test]
    async fn test_unclaim_stale_processing_requests(pool: sqlx::PgPool) {
        let http_client = Arc::new(MockHttpClient::new());

        // Create manager with 1-second processing timeout for testing
        let config = crate::daemon::DaemonConfig {
            claim_timeout_ms: 60000,     // 1 minute
            processing_timeout_ms: 1000, // 1 second
            ..Default::default()
        };
        let manager = Arc::new(
            PostgresRequestManager::with_client(pool.clone(), http_client).with_config(config),
        );

        // Create a file and batch
        let file_id = manager
            .create_file(
                "stale-processing-test".to_string(),
                None,
                vec![RequestTemplateInput {
                    custom_id: None,
                    endpoint: "https://api.example.com".to_string(),
                    method: "POST".to_string(),
                    path: "/test".to_string(),
                    body: "{}".to_string(),
                    model: "test".to_string(),
                    api_key: "key".to_string(),
                }],
            )
            .await
            .unwrap();

        let batch_id = manager.create_batch(file_id).await.unwrap();

        // Claim and manually set to processing state
        let daemon1_id = DaemonId::from(Uuid::new_v4());
        let claimed = manager.claim_requests(1, daemon1_id).await.unwrap();
        assert_eq!(claimed.len(), 1);
        let request_id = claimed[0].data.id;

        // Manually set to processing state with started_at 3 seconds ago
        sqlx::query!(
            r#"
            UPDATE requests
            SET
                state = 'processing',
                started_at = NOW() - INTERVAL '3 seconds'
            WHERE id = $1
            "#,
            *request_id as Uuid
        )
        .execute(&pool)
        .await
        .unwrap();

        // Verify it's in processing state
        let status_before = manager.get_batch_status(batch_id).await.unwrap();
        assert_eq!(status_before.in_progress_requests, 1);

        // Now daemon2 tries to claim - should unclaim the stale processing request
        let daemon2_id = DaemonId::from(Uuid::new_v4());
        let reclaimed = manager.claim_requests(1, daemon2_id).await.unwrap();

        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].data.id, request_id);
        assert_eq!(reclaimed[0].state.daemon_id, daemon2_id);
    }

    #[sqlx::test]
    async fn test_dont_unclaim_recent_requests(pool: sqlx::PgPool) {
        let http_client = Arc::new(MockHttpClient::new());

        // Create manager with long timeouts
        let config = crate::daemon::DaemonConfig {
            claim_timeout_ms: 60000,       // 1 minute
            processing_timeout_ms: 600000, // 10 minutes
            ..Default::default()
        };
        let manager = Arc::new(
            PostgresRequestManager::with_client(pool.clone(), http_client).with_config(config),
        );

        // Create a file with 2 templates
        let file_id = manager
            .create_file(
                "recent-test".to_string(),
                None,
                vec![
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/test".to_string(),
                        body: r#"{"n":1}"#.to_string(),
                        model: "test".to_string(),
                        api_key: "key".to_string(),
                    },
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/test".to_string(),
                        body: r#"{"n":2}"#.to_string(),
                        model: "test".to_string(),
                        api_key: "key".to_string(),
                    },
                ],
            )
            .await
            .unwrap();

        manager.create_batch(file_id).await.unwrap();

        // Daemon1 claims first request
        let daemon1_id = DaemonId::from(Uuid::new_v4());
        let claimed1 = manager.claim_requests(1, daemon1_id).await.unwrap();
        assert_eq!(claimed1.len(), 1);

        // Daemon2 immediately tries to claim - should get the second request, not steal the first
        let daemon2_id = DaemonId::from(Uuid::new_v4());
        let claimed2 = manager.claim_requests(1, daemon2_id).await.unwrap();
        assert_eq!(claimed2.len(), 1);

        // Verify they got different requests
        assert_ne!(claimed1[0].data.id, claimed2[0].data.id);

        // Verify first request still belongs to daemon1
        let results = manager
            .get_requests(vec![claimed1[0].data.id])
            .await
            .unwrap();
        if let Ok(crate::AnyRequest::Claimed(req)) = &results[0] {
            assert_eq!(req.state.daemon_id, daemon1_id);
        } else {
            panic!("Request should still be claimed by daemon1");
        }
    }

    #[sqlx::test]
    async fn test_preserve_retry_attempt_on_unclaim(pool: sqlx::PgPool) {
        let http_client = Arc::new(MockHttpClient::new());

        // Create manager with 1-second claim timeout
        let config = crate::daemon::DaemonConfig {
            claim_timeout_ms: 1000,
            processing_timeout_ms: 60000,
            ..Default::default()
        };
        let manager = Arc::new(
            PostgresRequestManager::with_client(pool.clone(), http_client).with_config(config),
        );

        // Create a file and batch
        let file_id = manager
            .create_file(
                "retry-test".to_string(),
                None,
                vec![RequestTemplateInput {
                    custom_id: None,
                    endpoint: "https://api.example.com".to_string(),
                    method: "POST".to_string(),
                    path: "/test".to_string(),
                    body: "{}".to_string(),
                    model: "test".to_string(),
                    api_key: "key".to_string(),
                }],
            )
            .await
            .unwrap();

        manager.create_batch(file_id).await.unwrap();

        // Manually set a request to claimed with retry_attempt=2
        sqlx::query!(
            r#"
            UPDATE requests
            SET
                retry_attempt = 2,
                state = 'claimed',
                daemon_id = $1,
                claimed_at = NOW() - INTERVAL '3 seconds'
            WHERE id IN (SELECT id FROM requests WHERE state = 'pending' LIMIT 1)
            RETURNING id
            "#,
            Uuid::new_v4()
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        // Claim should unclaim the stale request and reclaim it
        let daemon_id = DaemonId::from(Uuid::new_v4());
        let claimed = manager.claim_requests(1, daemon_id).await.unwrap();

        assert_eq!(claimed.len(), 1);
        // Verify retry_attempt is preserved
        assert_eq!(claimed[0].state.retry_attempt, 2);
    }
}
