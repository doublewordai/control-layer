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

use super::{DaemonStorage, Storage};
use crate::batch::{
    Batch, BatchErrorDetails, BatchErrorItem, BatchId, BatchInput, BatchOutputItem,
    BatchResponseDetails, BatchStatus, File, FileContentItem, FileId, FileMetadata, FileStreamItem,
    OutputFileType, RequestTemplate, RequestTemplateInput, TemplateId,
};
use crate::daemon::{
    AnyDaemonRecord, Daemon, DaemonConfig, DaemonData, DaemonRecord, DaemonState, DaemonStatus,
    Dead, Initializing, Running,
};
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
        // Interleave requests from different models using ROW_NUMBER partitioning
        // This ensures we claim requests round-robin across models rather than
        // draining one model before moving to the next (important for per-model concurrency)
        let rows = sqlx::query!(
            r#"
            WITH locked_requests AS (
                SELECT id, model, created_at
                FROM requests
                WHERE state = 'pending'
                    AND (not_before IS NULL OR not_before <= $2)
                FOR UPDATE SKIP LOCKED
            ),
            ranked AS (
                SELECT
                    id,
                    ROW_NUMBER() OVER (PARTITION BY model ORDER BY created_at) as model_rn,
                    created_at
                FROM locked_requests
            ),
            to_claim AS (
                SELECT id
                FROM ranked
                ORDER BY model_rn, created_at ASC
                LIMIT $3
            )
            UPDATE requests
            SET
                state = 'claimed',
                daemon_id = $1,
                claimed_at = $2
            FROM to_claim
            WHERE requests.id = to_claim.id
            RETURNING requests.id, requests.batch_id, requests.template_id, requests.endpoint,
                      requests.method, requests.path, requests.body, requests.model,
                      requests.api_key, requests.retry_attempt
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
                INSERT INTO request_templates (file_id, custom_id, endpoint, method, path, body, model, api_key)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                "#,
                file_id,
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

    #[tracing::instrument(skip(self, filter), fields(uploaded_by = ?filter.uploaded_by, status = ?filter.status, purpose = ?filter.purpose, after = ?filter.after, limit = ?filter.limit))]
    async fn list_files(&self, filter: crate::batch::FileFilter) -> Result<Vec<File>> {
        use sqlx::QueryBuilder;

        // Get cursor timestamp if needed
        let after_created_at = if let Some(after_id) = &filter.after {
            sqlx::query!(
                r#"SELECT created_at FROM files WHERE id = $1"#,
                **after_id as Uuid
            )
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| FusilladeError::Other(anyhow!("Failed to fetch after cursor: {}", e)))?
            .map(|row| row.created_at)
        } else {
            None
        };

        let mut query_builder = QueryBuilder::new(
            "SELECT id, name, description, size_bytes, status, error_message, purpose, expires_at, deleted_at, uploaded_by, created_at, updated_at FROM files"
        );

        // Build WHERE clause
        let mut has_where = false;

        if let Some(uploaded_by) = &filter.uploaded_by {
            query_builder.push(" WHERE uploaded_by = ");
            query_builder.push_bind(uploaded_by);
            has_where = true;
        }

        if let Some(status) = &filter.status {
            if has_where {
                query_builder.push(" AND status = ");
            } else {
                query_builder.push(" WHERE status = ");
                has_where = true;
            }
            query_builder.push_bind(status);
        }

        if let Some(purpose) = &filter.purpose {
            if has_where {
                query_builder.push(" AND purpose = ");
            } else {
                query_builder.push(" WHERE purpose = ");
                has_where = true;
            }
            query_builder.push_bind(purpose);
        }

        // Add cursor-based pagination
        if let (Some(after_id), Some(after_ts)) = (&filter.after, after_created_at) {
            let comparison = if filter.ascending { ">" } else { "<" };

            if has_where {
                query_builder.push(" AND ");
            } else {
                query_builder.push(" WHERE ");
            }

            query_builder.push("(created_at ");
            query_builder.push(comparison);
            query_builder.push(" ");
            query_builder.push_bind(after_ts);
            query_builder.push(" OR (created_at = ");
            query_builder.push_bind(after_ts);
            query_builder.push(" AND id ");
            query_builder.push(comparison);
            query_builder.push(" ");
            query_builder.push_bind(**after_id as Uuid);
            query_builder.push("))");
        }

        // Add ORDER BY
        let order_direction = if filter.ascending { "ASC" } else { "DESC" };
        query_builder.push(" ORDER BY created_at ");
        query_builder.push(order_direction);
        query_builder.push(", id ");
        query_builder.push(order_direction);

        // Add LIMIT
        if let Some(limit) = filter.limit {
            query_builder.push(" LIMIT ");
            query_builder.push_bind(limit as i64);
        }

        let rows = query_builder
            .build()
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
        offset: usize,
    ) -> Pin<Box<dyn Stream<Item = Result<FileContentItem>> + Send>> {
        let pool = self.pool.clone();
        let (tx, rx) = mpsc::channel(self.download_buffer_size);
        let offset = offset as i64;

        tokio::spawn(async move {
            // First, get the file to determine its purpose
            let file_result = sqlx::query!(
                r#"
                SELECT purpose
                FROM files
                WHERE id = $1
                "#,
                *file_id as Uuid,
            )
            .fetch_one(&pool)
            .await;

            let purpose = match file_result {
                Ok(row) => row.purpose,
                Err(e) => {
                    let _ = tx
                        .send(Err(FusilladeError::Other(anyhow!(
                            "Failed to fetch file: {}",
                            e
                        ))))
                        .await;
                    return;
                }
            };

            // Route to appropriate streaming logic based on purpose
            match purpose.as_deref() {
                Some("batch_output") => {
                    Self::stream_batch_output(pool, file_id, offset, tx).await;
                }
                Some("batch_error") => {
                    Self::stream_batch_error(pool, file_id, offset, tx).await;
                }
                _ => {
                    // Regular file or purpose='batch': stream request templates
                    Self::stream_request_templates(pool, file_id, offset, tx).await;
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

    async fn create_batch(&self, input: BatchInput) -> Result<Batch> {
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
            *input.file_id as Uuid,
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to fetch templates: {}", e)))?;

        if templates.is_empty() {
            return Err(FusilladeError::Other(anyhow!(
                "Cannot create batch from file with no templates"
            )));
        }

        // Calculate expires_at from completion_window
        let now = Utc::now();
        let expires_at = humantime::parse_duration(&input.completion_window)
            .ok()
            .and_then(|std_duration| chrono::Duration::from_std(std_duration).ok())
            .and_then(|duration| now.checked_add_signed(duration));

        // Create batch with new fields
        let row = sqlx::query!(
            r#"
            INSERT INTO batches (file_id, endpoint, completion_window, metadata, created_by, expires_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            RETURNING id, file_id, endpoint, completion_window, metadata, output_file_id, error_file_id, created_by, created_at, expires_at, cancelling_at, errors
            "#,
            *input.file_id as Uuid,
            input.endpoint,
            input.completion_window,
            input.metadata,
            input.created_by,
            expires_at,
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to create batch: {}", e)))?;

        let batch_id = row.id;

        // Create virtual output and error files
        let output_file_id = self
            .create_virtual_output_file(&mut tx, batch_id, &input.created_by)
            .await?;
        let error_file_id = self
            .create_virtual_error_file(&mut tx, batch_id, &input.created_by)
            .await?;

        // Update batch with file IDs
        sqlx::query!(
            r#"
            UPDATE batches
            SET output_file_id = $2, error_file_id = $3
            WHERE id = $1
            "#,
            batch_id,
            output_file_id,
            error_file_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            FusilladeError::Other(anyhow!("Failed to update batch with file IDs: {}", e))
        })?;

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

        Ok(Batch {
            id: BatchId(batch_id),
            file_id: FileId(row.file_id),
            created_at: row.created_at,
            metadata: row.metadata,
            completion_window: row.completion_window,
            endpoint: row.endpoint,
            output_file_id: Some(FileId(output_file_id)),
            error_file_id: Some(FileId(error_file_id)),
            created_by: row.created_by,
            expires_at: row.expires_at,
            cancelling_at: row.cancelling_at,
            errors: row.errors,
        })
    }

    async fn get_batch(&self, batch_id: BatchId) -> Result<Batch> {
        let row = sqlx::query!(
            r#"
            SELECT id, file_id, endpoint, completion_window, metadata, output_file_id, error_file_id, created_by, created_at, expires_at, cancelling_at, errors
            FROM batches
            WHERE id = $1
            "#,
            *batch_id as Uuid,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to fetch batch: {}", e)))?
        .ok_or_else(|| FusilladeError::Other(anyhow!("Batch not found")))?;

        Ok(Batch {
            id: BatchId(row.id),
            file_id: FileId(row.file_id),
            created_at: row.created_at,
            metadata: row.metadata,
            completion_window: row.completion_window,
            endpoint: row.endpoint,
            output_file_id: row.output_file_id.map(FileId),
            error_file_id: row.error_file_id.map(FileId),
            created_by: row.created_by,
            expires_at: row.expires_at,
            cancelling_at: row.cancelling_at,
            errors: row.errors,
        })
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

    async fn get_batch_by_output_file_id(
        &self,
        file_id: FileId,
        file_type: OutputFileType,
    ) -> Result<Option<Batch>> {
        match file_type {
            OutputFileType::Output => {
                let row = sqlx::query!(
                    r#"
                    SELECT id, file_id, endpoint, completion_window, metadata, output_file_id, error_file_id, created_by, created_at, expires_at, cancelling_at, errors
                    FROM batches
                    WHERE output_file_id = $1
                    "#,
                    *file_id as Uuid,
                )
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to get batch by output file: {}", e)))?;

                Ok(row.map(|row| Batch {
                    id: BatchId(row.id),
                    file_id: FileId(row.file_id),
                    created_at: row.created_at,
                    metadata: row.metadata,
                    completion_window: row.completion_window,
                    endpoint: row.endpoint,
                    output_file_id: row.output_file_id.map(FileId),
                    error_file_id: row.error_file_id.map(FileId),
                    created_by: row.created_by,
                    expires_at: row.expires_at,
                    cancelling_at: row.cancelling_at,
                    errors: row.errors,
                }))
            }
            OutputFileType::Error => {
                let row = sqlx::query!(
                    r#"
                    SELECT id, file_id, endpoint, completion_window, metadata, output_file_id, error_file_id, created_by, created_at, expires_at, cancelling_at, errors
                    FROM batches
                    WHERE error_file_id = $1
                    "#,
                    *file_id as Uuid,
                )
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to get batch by error file: {}", e)))?;

                Ok(row.map(|row| Batch {
                    id: BatchId(row.id),
                    file_id: FileId(row.file_id),
                    created_at: row.created_at,
                    metadata: row.metadata,
                    completion_window: row.completion_window,
                    endpoint: row.endpoint,
                    output_file_id: row.output_file_id.map(FileId),
                    error_file_id: row.error_file_id.map(FileId),
                    created_by: row.created_by,
                    expires_at: row.expires_at,
                    cancelling_at: row.cancelling_at,
                    errors: row.errors,
                }))
            }
        }
    }

    async fn list_batches(
        &self,
        created_by: Option<String>,
        after: Option<BatchId>,
        limit: i64,
    ) -> Result<Vec<Batch>> {
        // If after is provided, get the created_at timestamp of that batch for cursor-based pagination
        let (after_created_at, after_id) = if let Some(after_id) = after {
            let row = sqlx::query!(
                r#"
                SELECT created_at
                FROM batches
                WHERE id = $1
                "#,
                *after_id as Uuid,
            )
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| FusilladeError::Other(anyhow!("Failed to fetch after batch: {}", e)))?;

            (row.map(|r| r.created_at), Some(*after_id as Uuid))
        } else {
            (None, None)
        };

        // Use a single query with optional cursor filtering
        let rows = sqlx::query!(
            r#"
            SELECT id, file_id, endpoint, completion_window, metadata, output_file_id, error_file_id, created_by, created_at, expires_at, cancelling_at, errors
            FROM batches
            WHERE ($1::TEXT IS NULL OR created_by = $1)
              AND ($3::TIMESTAMPTZ IS NULL OR created_at < $3 OR (created_at = $3 AND id < $4))
            ORDER BY created_at DESC, id DESC
            LIMIT $2
            "#,
            created_by,
            limit,
            after_created_at,
            after_id,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to list batches: {}", e)))?;

        Ok(rows
            .into_iter()
            .map(|row| Batch {
                id: BatchId(row.id),
                file_id: FileId(row.file_id),
                created_at: row.created_at,
                metadata: row.metadata,
                completion_window: row.completion_window,
                endpoint: row.endpoint,
                output_file_id: row.output_file_id.map(FileId),
                error_file_id: row.error_file_id.map(FileId),
                created_by: row.created_by,
                expires_at: row.expires_at,
                cancelling_at: row.cancelling_at,
                errors: row.errors,
            })
            .collect())
    }

    async fn cancel_batch(&self, batch_id: BatchId) -> Result<()> {
        let now = Utc::now();

        // Set cancelling_at on the batch
        sqlx::query!(
            r#"
            UPDATE batches
            SET cancelling_at = $2
            WHERE id = $1 AND cancelling_at IS NULL
            "#,
            *batch_id as Uuid,
            now,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to set cancelling_at: {}", e)))?;

        // Cancel all pending/in-progress requests
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

// Helper methods for file streaming and virtual file creation
impl<H: HttpClient + 'static> PostgresRequestManager<H> {
    /// Stream request templates from a regular file
    async fn stream_request_templates(
        pool: sqlx::PgPool,
        file_id: FileId,
        offset: i64,
        tx: mpsc::Sender<Result<FileContentItem>>,
    ) {
        const BATCH_SIZE: i64 = 1000;
        let mut last_line_number: i32 = -1;
        let mut is_first_batch = true;

        loop {
            // Use OFFSET only on first batch, then use cursor pagination
            let (line_filter, offset_val) = if is_first_batch {
                (-1i32, offset)
            } else {
                (last_line_number, 0i64)
            };
            is_first_batch = false;

            let template_batch = sqlx::query!(
                r#"
                SELECT custom_id, endpoint, method, path, body, model, api_key, line_number
                FROM request_templates
                WHERE file_id = $1 AND ($2 = -1 OR line_number > $2)
                ORDER BY line_number ASC
                OFFSET $3
                LIMIT $4
                "#,
                *file_id as Uuid,
                line_filter,
                offset_val,
                BATCH_SIZE,
            )
            .fetch_all(&pool)
            .await;

            match template_batch {
                Ok(templates) => {
                    if templates.is_empty() {
                        break;
                    }

                    tracing::debug!(
                        "Fetched batch of {} templates, line_numbers {}-{}",
                        templates.len(),
                        templates.first().map(|r| r.line_number).unwrap_or(0),
                        templates.last().map(|r| r.line_number).unwrap_or(0)
                    );

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
                        if tx
                            .send(Ok(FileContentItem::Template(template)))
                            .await
                            .is_err()
                        {
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
    }

    /// Stream batch output (completed requests) for a virtual output file
    async fn stream_batch_output(
        pool: sqlx::PgPool,
        file_id: FileId,
        offset: i64,
        tx: mpsc::Sender<Result<FileContentItem>>,
    ) {
        // First, find the batch that owns this output file
        let batch_result = sqlx::query!(
            r#"
            SELECT id
            FROM batches
            WHERE output_file_id = $1
            "#,
            *file_id as Uuid,
        )
        .fetch_one(&pool)
        .await;

        let batch_id = match batch_result {
            Ok(row) => row.id,
            Err(e) => {
                let _ = tx
                    .send(Err(FusilladeError::Other(anyhow!(
                        "Failed to find batch for output file: {}",
                        e
                    ))))
                    .await;
                return;
            }
        };

        // Stream completed requests, ordered by completion time
        // This ensures new completions always append (no out-of-order issues)
        const BATCH_SIZE: i64 = 1000;
        let mut last_completed_at: Option<chrono::DateTime<chrono::Utc>> = None;
        let mut last_id: Uuid = Uuid::nil();
        let mut is_first_batch = true;

        loop {
            // Use OFFSET only on first batch, then use cursor pagination
            let (cursor_time, cursor_id, offset_val) = if is_first_batch {
                (None, Uuid::nil(), offset)
            } else {
                (last_completed_at, last_id, 0i64)
            };
            is_first_batch = false;

            let request_batch = sqlx::query!(
                r#"
                SELECT id, custom_id, response_status, response_body, completed_at
                FROM requests
                WHERE batch_id = $1
                  AND state = 'completed'
                  AND ($2::TIMESTAMPTZ IS NULL OR completed_at > $2 OR (completed_at = $2 AND id > $3))
                ORDER BY completed_at ASC, id ASC
                OFFSET $4
                LIMIT $5
                "#,
                batch_id,
                cursor_time,
                cursor_id,
                offset_val,
                BATCH_SIZE,
            )
            .fetch_all(&pool)
            .await;

            match request_batch {
                Ok(requests) => {
                    if requests.is_empty() {
                        break;
                    }

                    tracing::debug!("Fetched batch of {} completed requests", requests.len());

                    for row in requests {
                        last_completed_at = row.completed_at;
                        last_id = row.id;

                        let response_body: serde_json::Value = match row.response_body {
                            Some(body) => match serde_json::from_str(&body) {
                                Ok(json) => json,
                                Err(e) => {
                                    tracing::warn!("Failed to parse response body as JSON: {}", e);
                                    serde_json::Value::String(body)
                                }
                            },
                            None => serde_json::Value::Null,
                        };

                        let output_item = BatchOutputItem {
                            id: format!("batch_req_{}", row.id),
                            custom_id: row.custom_id,
                            response: BatchResponseDetails {
                                status_code: row.response_status.unwrap_or(200),
                                request_id: None, // Could be extracted from response if available
                                body: response_body,
                            },
                            error: None,
                        };

                        if tx
                            .send(Ok(FileContentItem::Output(output_item)))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                Err(e) => {
                    let _ = tx
                        .send(Err(FusilladeError::Other(anyhow!(
                            "Failed to fetch completed requests: {}",
                            e
                        ))))
                        .await;
                    return;
                }
            }
        }
    }

    /// Stream batch errors (failed requests) for a virtual error file
    async fn stream_batch_error(
        pool: sqlx::PgPool,
        file_id: FileId,
        offset: i64,
        tx: mpsc::Sender<Result<FileContentItem>>,
    ) {
        // First, find the batch that owns this error file
        let batch_result = sqlx::query!(
            r#"
            SELECT id
            FROM batches
            WHERE error_file_id = $1
            "#,
            *file_id as Uuid,
        )
        .fetch_one(&pool)
        .await;

        let batch_id = match batch_result {
            Ok(row) => row.id,
            Err(e) => {
                let _ = tx
                    .send(Err(FusilladeError::Other(anyhow!(
                        "Failed to find batch for error file: {}",
                        e
                    ))))
                    .await;
                return;
            }
        };

        // Stream failed requests, ordered by failure time
        // This ensures new failures always append (no out-of-order issues)
        const BATCH_SIZE: i64 = 1000;
        let mut last_failed_at: Option<chrono::DateTime<chrono::Utc>> = None;
        let mut last_id: Uuid = Uuid::nil();
        let mut is_first_batch = true;

        loop {
            // Use OFFSET only on first batch, then use cursor pagination
            let (cursor_time, cursor_id, offset_val) = if is_first_batch {
                (None, Uuid::nil(), offset)
            } else {
                (last_failed_at, last_id, 0i64)
            };
            is_first_batch = false;

            let request_batch = sqlx::query!(
                r#"
                SELECT id, custom_id, error, failed_at
                FROM requests
                WHERE batch_id = $1
                  AND state = 'failed'
                  AND ($2::TIMESTAMPTZ IS NULL OR failed_at > $2 OR (failed_at = $2 AND id > $3))
                ORDER BY failed_at ASC, id ASC
                OFFSET $4
                LIMIT $5
                "#,
                batch_id,
                cursor_time,
                cursor_id,
                offset_val,
                BATCH_SIZE,
            )
            .fetch_all(&pool)
            .await;

            match request_batch {
                Ok(requests) => {
                    if requests.is_empty() {
                        break;
                    }

                    tracing::debug!("Fetched batch of {} failed requests", requests.len());

                    for row in requests {
                        last_failed_at = row.failed_at;
                        last_id = row.id;

                        let error_item = BatchErrorItem {
                            id: format!("batch_req_{}", row.id),
                            custom_id: row.custom_id,
                            response: None,
                            error: BatchErrorDetails {
                                code: None, // Could parse from error field if structured
                                message: row.error.unwrap_or_else(|| "Unknown error".to_string()),
                            },
                        };

                        if tx
                            .send(Ok(FileContentItem::Error(error_item)))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                Err(e) => {
                    let _ = tx
                        .send(Err(FusilladeError::Other(anyhow!(
                            "Failed to fetch failed requests: {}",
                            e
                        ))))
                        .await;
                    return;
                }
            }
        }
    }

    /// Create a virtual output file for a batch (stores no data, streams from requests table)
    async fn create_virtual_output_file(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        batch_id: Uuid,
        created_by: &Option<String>,
    ) -> Result<Uuid> {
        let name = format!("batch-{}-output.jsonl", batch_id);
        let description = format!("Output file for batch {}", batch_id);

        let file_id = sqlx::query_scalar!(
            r#"
            INSERT INTO files (name, description, size_bytes, status, purpose, uploaded_by)
            VALUES ($1, $2, 0, 'processed', 'batch_output', $3)
            RETURNING id
            "#,
            name,
            description,
            created_by.as_deref(),
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to create output file: {}", e)))?;

        Ok(file_id)
    }

    /// Create a virtual error file for a batch (stores no data, streams from requests table)
    async fn create_virtual_error_file(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        batch_id: Uuid,
        created_by: &Option<String>,
    ) -> Result<Uuid> {
        let name = format!("batch-{}-error.jsonl", batch_id);
        let description = format!("Error file for batch {}", batch_id);

        let file_id = sqlx::query_scalar!(
            r#"
            INSERT INTO files (name, description, size_bytes, status, purpose, uploaded_by)
            VALUES ($1, $2, 0, 'processed', 'batch_error', $3)
            RETURNING id
            "#,
            name,
            description,
            created_by.as_deref(),
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to create error file: {}", e)))?;

        Ok(file_id)
    }
}

// Implement DaemonStorage trait
#[async_trait]
impl<H: HttpClient> DaemonStorage for PostgresRequestManager<H> {
    async fn persist_daemon<T: DaemonState + Clone>(&self, record: &DaemonRecord<T>) -> Result<()>
    where
        AnyDaemonRecord: From<DaemonRecord<T>>,
    {
        let any_daemon = AnyDaemonRecord::from(record.clone());

        match any_daemon {
            AnyDaemonRecord::Initializing(daemon) => {
                sqlx::query!(
                    r#"
                    INSERT INTO daemons (
                        id, status, hostname, pid, version, config_snapshot,
                        started_at, last_heartbeat, stopped_at,
                        requests_processed, requests_failed, requests_in_flight
                    ) VALUES ($1, 'initializing', $2, $3, $4, $5, $6, NULL, NULL, 0, 0, 0)
                    ON CONFLICT (id) DO UPDATE SET
                        status = 'initializing',
                        started_at = $6,
                        updated_at = NOW()
                    "#,
                    *daemon.data.id as Uuid,
                    daemon.data.hostname,
                    daemon.data.pid,
                    daemon.data.version,
                    daemon.data.config_snapshot,
                    daemon.state.started_at,
                )
                .execute(&self.pool)
                .await
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to persist daemon: {}", e)))?;
            }
            AnyDaemonRecord::Running(daemon) => {
                sqlx::query!(
                    r#"
                    INSERT INTO daemons (
                        id, status, hostname, pid, version, config_snapshot,
                        started_at, last_heartbeat, stopped_at,
                        requests_processed, requests_failed, requests_in_flight
                    ) VALUES ($1, 'running', $2, $3, $4, $5, $6, $7, NULL, $8, $9, $10)
                    ON CONFLICT (id) DO UPDATE SET
                        status = 'running',
                        last_heartbeat = $7,
                        requests_processed = $8,
                        requests_failed = $9,
                        requests_in_flight = $10,
                        updated_at = NOW()
                    "#,
                    *daemon.data.id as Uuid,
                    daemon.data.hostname,
                    daemon.data.pid,
                    daemon.data.version,
                    daemon.data.config_snapshot,
                    daemon.state.started_at,
                    daemon.state.last_heartbeat,
                    daemon.state.stats.requests_processed as i64,
                    daemon.state.stats.requests_failed as i64,
                    daemon.state.stats.requests_in_flight as i32,
                )
                .execute(&self.pool)
                .await
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to persist daemon: {}", e)))?;
            }
            AnyDaemonRecord::Dead(daemon) => {
                sqlx::query!(
                    r#"
                    INSERT INTO daemons (
                        id, status, hostname, pid, version, config_snapshot,
                        started_at, last_heartbeat, stopped_at,
                        requests_processed, requests_failed, requests_in_flight
                    ) VALUES ($1, 'dead', $2, $3, $4, $5, $6, NULL, $7, $8, $9, $10)
                    ON CONFLICT (id) DO UPDATE SET
                        status = 'dead',
                        stopped_at = $7,
                        requests_processed = $8,
                        requests_failed = $9,
                        requests_in_flight = $10,
                        updated_at = NOW()
                    "#,
                    *daemon.data.id as Uuid,
                    daemon.data.hostname,
                    daemon.data.pid,
                    daemon.data.version,
                    daemon.data.config_snapshot,
                    daemon.state.started_at,
                    daemon.state.stopped_at,
                    daemon.state.final_stats.requests_processed as i64,
                    daemon.state.final_stats.requests_failed as i64,
                    daemon.state.final_stats.requests_in_flight as i32,
                )
                .execute(&self.pool)
                .await
                .map_err(|e| FusilladeError::Other(anyhow!("Failed to persist daemon: {}", e)))?;
            }
        }

        Ok(())
    }

    async fn get_daemon(&self, daemon_id: DaemonId) -> Result<AnyDaemonRecord> {
        let row = sqlx::query!(
            r#"
            SELECT
                id, status, hostname, pid, version, config_snapshot,
                started_at, last_heartbeat, stopped_at,
                requests_processed, requests_failed, requests_in_flight
            FROM daemons
            WHERE id = $1
            "#,
            *daemon_id as Uuid,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => FusilladeError::Other(anyhow!("Daemon not found")),
            _ => FusilladeError::Other(anyhow!("Failed to fetch daemon: {}", e)),
        })?;

        let data = DaemonData {
            id: DaemonId(row.id),
            hostname: row.hostname,
            pid: row.pid,
            version: row.version,
            config_snapshot: row.config_snapshot,
        };

        let any_daemon = match row.status.as_str() {
            "initializing" => AnyDaemonRecord::Initializing(DaemonRecord {
                data,
                state: Initializing {
                    started_at: row.started_at,
                },
            }),
            "running" => AnyDaemonRecord::Running(DaemonRecord {
                data,
                state: Running {
                    started_at: row.started_at,
                    last_heartbeat: row.last_heartbeat.ok_or_else(|| {
                        FusilladeError::Other(anyhow!("Running daemon missing last_heartbeat"))
                    })?,
                    stats: crate::daemon::DaemonStats {
                        requests_processed: row.requests_processed as u64,
                        requests_failed: row.requests_failed as u64,
                        requests_in_flight: row.requests_in_flight as usize,
                    },
                },
            }),
            "dead" => AnyDaemonRecord::Dead(DaemonRecord {
                data,
                state: Dead {
                    started_at: row.started_at,
                    stopped_at: row.stopped_at.ok_or_else(|| {
                        FusilladeError::Other(anyhow!("Dead daemon missing stopped_at"))
                    })?,
                    final_stats: crate::daemon::DaemonStats {
                        requests_processed: row.requests_processed as u64,
                        requests_failed: row.requests_failed as u64,
                        requests_in_flight: row.requests_in_flight as usize,
                    },
                },
            }),
            _ => {
                return Err(FusilladeError::Other(anyhow!(
                    "Unknown daemon status: {}",
                    row.status
                )))
            }
        };

        Ok(any_daemon)
    }

    async fn list_daemons(
        &self,
        status_filter: Option<DaemonStatus>,
    ) -> Result<Vec<AnyDaemonRecord>> {
        let status_str = status_filter.as_ref().map(|s| s.as_str());

        let rows = sqlx::query!(
            r#"
            SELECT
                id, status, hostname, pid, version, config_snapshot,
                started_at, last_heartbeat, stopped_at,
                requests_processed, requests_failed, requests_in_flight
            FROM daemons
            WHERE ($1::text IS NULL OR status = $1)
            ORDER BY created_at DESC
            "#,
            status_str,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to list daemons: {}", e)))?;

        let mut daemons = Vec::new();

        for row in rows {
            let data = DaemonData {
                id: DaemonId(row.id),
                hostname: row.hostname,
                pid: row.pid,
                version: row.version,
                config_snapshot: row.config_snapshot,
            };

            let any_daemon = match row.status.as_str() {
                "initializing" => AnyDaemonRecord::Initializing(DaemonRecord {
                    data,
                    state: Initializing {
                        started_at: row.started_at,
                    },
                }),
                "running" => {
                    if let Some(last_heartbeat) = row.last_heartbeat {
                        AnyDaemonRecord::Running(DaemonRecord {
                            data,
                            state: Running {
                                started_at: row.started_at,
                                last_heartbeat,
                                stats: crate::daemon::DaemonStats {
                                    requests_processed: row.requests_processed as u64,
                                    requests_failed: row.requests_failed as u64,
                                    requests_in_flight: row.requests_in_flight as usize,
                                },
                            },
                        })
                    } else {
                        // Skip invalid running daemons
                        continue;
                    }
                }
                "dead" => {
                    if let Some(stopped_at) = row.stopped_at {
                        AnyDaemonRecord::Dead(DaemonRecord {
                            data,
                            state: Dead {
                                started_at: row.started_at,
                                stopped_at,
                                final_stats: crate::daemon::DaemonStats {
                                    requests_processed: row.requests_processed as u64,
                                    requests_failed: row.requests_failed as u64,
                                    requests_in_flight: row.requests_in_flight as usize,
                                },
                            },
                        })
                    } else {
                        // Skip invalid dead daemons
                        continue;
                    }
                }
                _ => {
                    // Skip unknown statuses
                    continue;
                }
            };

            daemons.push(any_daemon);
        }

        Ok(daemons)
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

    fn run(
        self: Arc<Self>,
        shutdown_token: tokio_util::sync::CancellationToken,
    ) -> Result<JoinHandle<Result<()>>> {
        tracing::info!("Starting PostgreSQL request manager daemon");

        let daemon = Arc::new(Daemon::new(
            self.clone(),
            self.http_client.clone(),
            self.config.clone(),
            shutdown_token,
        ));

        let handle = tokio::spawn(async move { daemon.run().await });

        tracing::info!("Daemon spawned successfully");

        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::{
        AnyDaemonRecord, DaemonData, DaemonRecord, DaemonStats, DaemonStatus, Dead, Initializing,
        Running,
    };
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
        let batch = manager
            .create_batch(crate::batch::BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
            })
            .await
            .expect("Failed to create batch");

        // Get batch status
        let status = manager
            .get_batch_status(batch.id)
            .await
            .expect("Failed to get batch status");

        assert_eq!(status.batch_id, batch.id);
        assert_eq!(status.file_id, file_id);
        assert_eq!(status.file_name, "batch-test");
        assert_eq!(status.total_requests, 3);
        assert_eq!(status.pending_requests, 3);
        assert_eq!(status.completed_requests, 0);
        assert_eq!(status.failed_requests, 0);

        // Get batch requests
        let requests = manager
            .get_batch_requests(batch.id)
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

        let batch = manager
            .create_batch(crate::batch::BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
            })
            .await
            .unwrap();

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
        let status = manager.get_batch_status(batch.id).await.unwrap();
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

        let batch = manager
            .create_batch(crate::batch::BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
            })
            .await
            .unwrap();

        // Verify all are pending
        let status_before = manager.get_batch_status(batch.id).await.unwrap();
        assert_eq!(status_before.pending_requests, 3);
        assert_eq!(status_before.canceled_requests, 0);

        // Cancel the batch
        manager.cancel_batch(batch.id).await.unwrap();

        // Verify all are canceled
        let status_after = manager.get_batch_status(batch.id).await.unwrap();
        assert_eq!(status_after.pending_requests, 0);
        assert_eq!(status_after.canceled_requests, 3);

        // Get the actual requests to verify their state
        let requests = manager.get_batch_requests(batch.id).await.unwrap();
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

        let batch = manager
            .create_batch(crate::batch::BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
            })
            .await
            .unwrap();

        // Get all request IDs
        let requests = manager.get_batch_requests(batch.id).await.unwrap();
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
        let status = manager.get_batch_status(batch.id).await.unwrap();
        assert_eq!(status.pending_requests, 2);
        assert_eq!(status.canceled_requests, 3);

        // Verify the requests
        let all_requests = manager.get_batch_requests(batch.id).await.unwrap();
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
        let batch1 = manager
            .create_batch(crate::batch::BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
            })
            .await
            .unwrap();
        let batch2 = manager
            .create_batch(crate::batch::BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
            })
            .await
            .unwrap();
        let batch3 = manager
            .create_batch(crate::batch::BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
            })
            .await
            .unwrap();

        // List batches for this file
        let batches = manager.list_file_batches(file_id).await.unwrap();

        assert_eq!(batches.len(), 3);

        // Verify all batch IDs are present
        let batch_ids: Vec<_> = batches.iter().map(|b| b.batch_id).collect();
        assert!(batch_ids.contains(&batch1.id));
        assert!(batch_ids.contains(&batch2.id));
        assert!(batch_ids.contains(&batch3.id));

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
        let batch = manager
            .create_batch(crate::batch::BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
            })
            .await
            .unwrap();

        // Verify the batch exists
        let status_before = manager.get_batch_status(batch.id).await;
        assert!(status_before.is_ok());

        // Delete the file
        manager.delete_file(file_id).await.unwrap();

        // Verify file is gone
        let file_result = manager.get_file(file_id).await;
        assert!(file_result.is_err());

        // Verify batch is gone (cascade delete)
        let status_after = manager.get_batch_status(batch.id).await;
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

        let batch = manager
            .create_batch(crate::batch::BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
            })
            .await
            .unwrap();

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
        let status = manager.get_batch_status(batch.id).await.unwrap();
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

        let batch = manager
            .create_batch(crate::batch::BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
            })
            .await
            .unwrap();

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
        let status_before = manager.get_batch_status(batch.id).await.unwrap();
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

        manager
            .create_batch(crate::batch::BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
            })
            .await
            .unwrap();

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

        manager
            .create_batch(crate::batch::BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
            })
            .await
            .unwrap();

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

    #[sqlx::test]
    async fn test_batch_output_and_error_streaming(pool: sqlx::PgPool) {
        use futures::StreamExt;

        let http_client = Arc::new(MockHttpClient::new());
        let manager = PostgresRequestManager::with_client(pool.clone(), http_client);

        // Create a file with 3 templates
        let file_id = manager
            .create_file(
                "streaming-test".to_string(),
                None,
                vec![
                    RequestTemplateInput {
                        custom_id: Some("req-1".to_string()),
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/chat/completions".to_string(),
                        body: r#"{"prompt":"first"}"#.to_string(),
                        model: "gpt-4".to_string(),
                        api_key: "key".to_string(),
                    },
                    RequestTemplateInput {
                        custom_id: Some("req-2".to_string()),
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/chat/completions".to_string(),
                        body: r#"{"prompt":"second"}"#.to_string(),
                        model: "gpt-4".to_string(),
                        api_key: "key".to_string(),
                    },
                    RequestTemplateInput {
                        custom_id: Some("req-3".to_string()),
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/chat/completions".to_string(),
                        body: r#"{"prompt":"third"}"#.to_string(),
                        model: "gpt-4".to_string(),
                        api_key: "key".to_string(),
                    },
                ],
            )
            .await
            .expect("Failed to create file");

        // Create a batch
        let batch = manager
            .create_batch(crate::batch::BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: Some("test-user".to_string()),
            })
            .await
            .expect("Failed to create batch");

        // Verify virtual output and error files were created
        assert!(batch.output_file_id.is_some());
        assert!(batch.error_file_id.is_some());
        let output_file_id = batch.output_file_id.unwrap();
        let error_file_id = batch.error_file_id.unwrap();

        // Get the virtual files and verify they exist
        let output_file = manager
            .get_file(output_file_id)
            .await
            .expect("Failed to get output file");
        let error_file = manager
            .get_file(error_file_id)
            .await
            .expect("Failed to get error file");

        assert_eq!(
            output_file.name,
            format!("batch-{}-output.jsonl", batch.id.0)
        );
        assert_eq!(error_file.name, format!("batch-{}-error.jsonl", batch.id.0));

        // Manually mark 2 requests as completed and 1 as failed
        // This simulates what the daemon would do after processing
        let requests = manager
            .get_batch_requests(batch.id)
            .await
            .expect("Failed to get requests");
        assert_eq!(requests.len(), 3);

        // Mark first request as completed
        sqlx::query!(
            r#"
            UPDATE requests
            SET state = 'completed',
                response_status = 200,
                response_body = $2,
                completed_at = NOW()
            WHERE id = $1
            "#,
            *requests[0].id() as Uuid,
            r#"{"id":"chatcmpl-123","choices":[{"message":{"content":"Response 1"}}]}"#,
        )
        .execute(&pool)
        .await
        .expect("Failed to mark request as completed");

        // Mark second request as completed
        sqlx::query!(
            r#"
            UPDATE requests
            SET state = 'completed',
                response_status = 200,
                response_body = $2,
                completed_at = NOW()
            WHERE id = $1
            "#,
            *requests[1].id() as Uuid,
            r#"{"id":"chatcmpl-456","choices":[{"message":{"content":"Response 2"}}]}"#,
        )
        .execute(&pool)
        .await
        .expect("Failed to mark request as completed");

        // Mark third request as failed
        sqlx::query!(
            r#"
            UPDATE requests
            SET state = 'failed',
                error = $2,
                failed_at = NOW()
            WHERE id = $1
            "#,
            *requests[2].id() as Uuid,
            "Rate limit exceeded",
        )
        .execute(&pool)
        .await
        .expect("Failed to mark request as failed");

        // Stream the output file - should contain 2 completed requests
        let output_stream = manager.get_file_content_stream(output_file_id, 0);
        let output_items: Vec<_> = output_stream.collect().await;

        assert_eq!(output_items.len(), 2, "Should have 2 output items");

        // Collect and verify custom_ids (order doesn't matter)
        let mut found_custom_ids = Vec::new();
        for item_result in output_items.iter() {
            let item = item_result.as_ref().expect("Output item should be Ok");

            match item {
                FileContentItem::Output(output) => {
                    found_custom_ids.push(output.custom_id.clone());

                    // Verify response structure
                    assert_eq!(output.response.status_code, 200);
                    assert!(output.response.body.is_object());
                    assert!(output.error.is_none());

                    // Verify ID format
                    assert!(output.id.starts_with("batch_req_"));
                }
                _ => panic!("Expected FileContentItem::Output, got different type"),
            }
        }

        // Verify we got both custom IDs (order doesn't matter)
        found_custom_ids.sort();
        assert_eq!(
            found_custom_ids,
            vec![Some("req-1".to_string()), Some("req-2".to_string())]
        );

        // Stream the error file - should contain 1 failed request
        let error_stream = manager.get_file_content_stream(error_file_id, 0);
        let error_items: Vec<_> = error_stream.collect().await;

        assert_eq!(error_items.len(), 1, "Should have 1 error item");

        // Verify the error item
        let error_result = &error_items[0];
        let error_item = error_result.as_ref().expect("Error item should be Ok");

        match error_item {
            FileContentItem::Error(error) => {
                assert_eq!(error.custom_id, Some("req-3".to_string()));
                assert_eq!(error.error.message, "Rate limit exceeded");
                assert!(error.response.is_none());
                assert!(error.id.starts_with("batch_req_"));
            }
            _ => panic!("Expected FileContentItem::Error, got different type"),
        }

        // Verify that streaming a regular input file still works
        let input_stream = manager.get_file_content_stream(file_id, 0);
        let input_items: Vec<_> = input_stream.collect().await;

        assert_eq!(input_items.len(), 3, "Input file should have 3 templates");

        for item_result in input_items {
            let item = item_result.expect("Input item should be Ok");
            match item {
                FileContentItem::Template(_) => {
                    // Expected - input files contain templates
                }
                _ => panic!("Expected FileContentItem::Template for input file"),
            }
        }
    }

    // ========================================================================
    // Daemon storage tests
    // ========================================================================

    #[sqlx::test]
    async fn test_daemon_persist_and_get(pool: sqlx::PgPool) {
        let http_client = Arc::new(MockHttpClient::new());
        let manager = PostgresRequestManager::with_client(pool.clone(), http_client);

        let daemon_id = DaemonId(Uuid::new_v4());
        let daemon_data = DaemonData {
            id: daemon_id,
            hostname: "test-host".to_string(),
            pid: 12345,
            version: "1.0.0".to_string(),
            config_snapshot: serde_json::json!({"test": "config"}),
        };

        // Test Initializing state
        let initializing = DaemonRecord {
            data: daemon_data.clone(),
            state: Initializing {
                started_at: Utc::now(),
            },
        };

        manager.persist_daemon(&initializing).await.unwrap();

        let retrieved = manager.get_daemon(daemon_id).await.unwrap();
        match retrieved {
            AnyDaemonRecord::Initializing(d) => {
                assert_eq!(d.data.id, daemon_id);
                assert_eq!(d.data.hostname, "test-host");
            }
            _ => panic!("Expected Initializing state"),
        }

        // Test Running state
        let running = DaemonRecord {
            data: daemon_data.clone(),
            state: Running {
                started_at: Utc::now(),
                last_heartbeat: Utc::now(),
                stats: DaemonStats {
                    requests_processed: 10,
                    requests_failed: 2,
                    requests_in_flight: 3,
                },
            },
        };

        manager.persist_daemon(&running).await.unwrap();

        let retrieved = manager.get_daemon(daemon_id).await.unwrap();
        match retrieved {
            AnyDaemonRecord::Running(d) => {
                assert_eq!(d.data.id, daemon_id);
                assert_eq!(d.state.stats.requests_processed, 10);
                assert_eq!(d.state.stats.requests_failed, 2);
                assert_eq!(d.state.stats.requests_in_flight, 3);
            }
            _ => panic!("Expected Running state"),
        }

        // Test Dead state
        let dead = DaemonRecord {
            data: daemon_data,
            state: Dead {
                started_at: Utc::now() - chrono::Duration::hours(1),
                stopped_at: Utc::now(),
                final_stats: DaemonStats {
                    requests_processed: 100,
                    requests_failed: 5,
                    requests_in_flight: 0,
                },
            },
        };

        manager.persist_daemon(&dead).await.unwrap();

        let retrieved = manager.get_daemon(daemon_id).await.unwrap();
        match retrieved {
            AnyDaemonRecord::Dead(d) => {
                assert_eq!(d.data.id, daemon_id);
                assert_eq!(d.state.final_stats.requests_processed, 100);
                assert_eq!(d.state.final_stats.requests_failed, 5);
            }
            _ => panic!("Expected Dead state"),
        }
    }

    #[sqlx::test]
    async fn test_daemon_list_all(pool: sqlx::PgPool) {
        let http_client = Arc::new(MockHttpClient::new());
        let manager = PostgresRequestManager::with_client(pool.clone(), http_client);

        // Create multiple daemons in different states
        let daemon1 = DaemonRecord {
            data: DaemonData {
                id: DaemonId(Uuid::new_v4()),
                hostname: "host1".to_string(),
                pid: 1001,
                version: "1.0.0".to_string(),
                config_snapshot: serde_json::json!({}),
            },
            state: Running {
                started_at: Utc::now(),
                last_heartbeat: Utc::now(),
                stats: DaemonStats::default(),
            },
        };

        let daemon2 = DaemonRecord {
            data: DaemonData {
                id: DaemonId(Uuid::new_v4()),
                hostname: "host2".to_string(),
                pid: 1002,
                version: "1.0.0".to_string(),
                config_snapshot: serde_json::json!({}),
            },
            state: Running {
                started_at: Utc::now(),
                last_heartbeat: Utc::now(),
                stats: DaemonStats::default(),
            },
        };

        let daemon3 = DaemonRecord {
            data: DaemonData {
                id: DaemonId(Uuid::new_v4()),
                hostname: "host3".to_string(),
                pid: 1003,
                version: "1.0.0".to_string(),
                config_snapshot: serde_json::json!({}),
            },
            state: Dead {
                started_at: Utc::now() - chrono::Duration::hours(1),
                stopped_at: Utc::now(),
                final_stats: DaemonStats::default(),
            },
        };

        manager.persist_daemon(&daemon1).await.unwrap();
        manager.persist_daemon(&daemon2).await.unwrap();
        manager.persist_daemon(&daemon3).await.unwrap();

        // List all daemons
        let all = manager.list_daemons(None).await.unwrap();
        assert_eq!(all.len(), 3);

        // List only running daemons
        let running = manager
            .list_daemons(Some(DaemonStatus::Running))
            .await
            .unwrap();
        assert_eq!(running.len(), 2);

        // List only dead daemons
        let dead = manager
            .list_daemons(Some(DaemonStatus::Dead))
            .await
            .unwrap();
        assert_eq!(dead.len(), 1);
    }

    #[sqlx::test]
    async fn test_daemon_heartbeat_updates(pool: sqlx::PgPool) {
        let http_client = Arc::new(MockHttpClient::new());
        let manager = PostgresRequestManager::with_client(pool.clone(), http_client);

        let daemon_id = DaemonId(Uuid::new_v4());
        let daemon_data = DaemonData {
            id: daemon_id,
            hostname: "test-host".to_string(),
            pid: 12345,
            version: "1.0.0".to_string(),
            config_snapshot: serde_json::json!({}),
        };

        // Start with Running state
        let running = DaemonRecord {
            data: daemon_data,
            state: Running {
                started_at: Utc::now(),
                last_heartbeat: Utc::now(),
                stats: DaemonStats {
                    requests_processed: 0,
                    requests_failed: 0,
                    requests_in_flight: 0,
                },
            },
        };

        manager.persist_daemon(&running).await.unwrap();

        // Simulate heartbeat updates
        for i in 1..=3 {
            let updated = DaemonRecord {
                data: running.data.clone(),
                state: Running {
                    started_at: running.state.started_at,
                    last_heartbeat: Utc::now(),
                    stats: DaemonStats {
                        requests_processed: i * 10,
                        requests_failed: i,
                        requests_in_flight: i as usize,
                    },
                },
            };
            manager.persist_daemon(&updated).await.unwrap();
        }

        // Verify final state
        let retrieved = manager.get_daemon(daemon_id).await.unwrap();
        match retrieved {
            AnyDaemonRecord::Running(d) => {
                assert_eq!(d.state.stats.requests_processed, 30);
                assert_eq!(d.state.stats.requests_failed, 3);
                assert_eq!(d.state.stats.requests_in_flight, 3);
            }
            _ => panic!("Expected Running state"),
        }
    }

    #[sqlx::test]
    async fn test_claim_requests_interleaves_by_model(pool: sqlx::PgPool) {
        let http_client = Arc::new(MockHttpClient::new());
        let manager = PostgresRequestManager::with_client(pool.clone(), http_client);

        // Create a file with 9 templates: 3 for each of 3 different models
        // We create them in order: all model-a, then all model-b, then all model-c
        let file_id = manager
            .create_file(
                "interleave-test".to_string(),
                None,
                vec![
                    // model-a requests
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/test".to_string(),
                        body: r#"{"model":"a","n":1}"#.to_string(),
                        model: "model-a".to_string(),
                        api_key: "key".to_string(),
                    },
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/test".to_string(),
                        body: r#"{"model":"a","n":2}"#.to_string(),
                        model: "model-a".to_string(),
                        api_key: "key".to_string(),
                    },
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/test".to_string(),
                        body: r#"{"model":"a","n":3}"#.to_string(),
                        model: "model-a".to_string(),
                        api_key: "key".to_string(),
                    },
                    // model-b requests
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/test".to_string(),
                        body: r#"{"model":"b","n":1}"#.to_string(),
                        model: "model-b".to_string(),
                        api_key: "key".to_string(),
                    },
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/test".to_string(),
                        body: r#"{"model":"b","n":2}"#.to_string(),
                        model: "model-b".to_string(),
                        api_key: "key".to_string(),
                    },
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/test".to_string(),
                        body: r#"{"model":"b","n":3}"#.to_string(),
                        model: "model-b".to_string(),
                        api_key: "key".to_string(),
                    },
                    // model-c requests
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/test".to_string(),
                        body: r#"{"model":"c","n":1}"#.to_string(),
                        model: "model-c".to_string(),
                        api_key: "key".to_string(),
                    },
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/test".to_string(),
                        body: r#"{"model":"c","n":2}"#.to_string(),
                        model: "model-c".to_string(),
                        api_key: "key".to_string(),
                    },
                    RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/test".to_string(),
                        body: r#"{"model":"c","n":3}"#.to_string(),
                        model: "model-c".to_string(),
                        api_key: "key".to_string(),
                    },
                ],
            )
            .await
            .unwrap();

        let _batch = manager
            .create_batch(crate::batch::BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
            })
            .await
            .unwrap();

        let daemon_id = DaemonId::from(Uuid::new_v4());

        // Claim 6 requests - should get 2 from each model in round-robin order
        let claimed = manager
            .claim_requests(6, daemon_id)
            .await
            .expect("Failed to claim requests");

        assert_eq!(claimed.len(), 6);

        // Extract the models in order
        let models: Vec<&str> = claimed.iter().map(|r| r.data.model.as_str()).collect();

        // With interleaving, we should see a round-robin pattern:
        // First 3 requests should be from 3 different models
        // Next 3 requests should also be from the same 3 different models
        // The order of models isn't guaranteed, but the pattern should repeat
        let first_three: Vec<&str> = models[0..3].to_vec();
        let second_three: Vec<&str> = models[3..6].to_vec();

        // Verify all unique (first batch has one from each model)
        let mut first_sorted = first_three.clone();
        first_sorted.sort();
        first_sorted.dedup();
        assert_eq!(
            first_sorted.len(),
            3,
            "First 3 requests should be from 3 different models"
        );
        assert_eq!(
            first_sorted,
            vec!["model-a", "model-b", "model-c"],
            "First 3 requests should cover all 3 models"
        );

        // Verify the pattern repeats (same order)
        assert_eq!(
            first_three, second_three,
            "The round-robin pattern should repeat: got {:?} then {:?}",
            first_three, second_three
        );

        // Verify each model's requests are in chronological order (n=1 before n=2)
        for model in &["model-a", "model-b", "model-c"] {
            let model_requests: Vec<&str> = claimed
                .iter()
                .filter(|r| r.data.model == *model)
                .map(|r| r.data.body.as_str())
                .collect();

            // Should have 2 requests per model
            assert_eq!(model_requests.len(), 2, "Should have 2 requests for {}", model);

            // First should be n=1, second should be n=2
            assert!(
                model_requests[0].contains(r#""n":1"#),
                "First request for {} should be n=1",
                model
            );
            assert!(
                model_requests[1].contains(r#""n":2"#),
                "Second request for {} should be n=2",
                model
            );
        }
    }
}
