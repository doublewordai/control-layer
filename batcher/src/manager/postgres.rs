//! PostgreSQL implementation of Storage and DaemonExecutor.
//!
//! This implementation combines PostgreSQL storage with the daemon to provide
//! a production-ready batching system with persistent storage and real-time updates.

use crate::request::AnyRequest;
use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use chrono::Utc;
use futures::stream::Stream;
use sqlx::postgres::{PgListener, PgPool};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::Storage;
use crate::batch::{
    BatchId, BatchStatus, File, FileId, RequestTemplate, RequestTemplateInput, TemplateId,
};
use crate::daemon::{Daemon, DaemonConfig};
use crate::error::{BatcherError, Result};
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
/// use batcher::{PostgresRequestManager, ReqwestHttpClient, DaemonConfig};
/// use sqlx::PgPool;
///
/// let pool = PgPool::connect("postgresql://localhost/batcher").await?;
/// let http_client = Arc::new(ReqwestHttpClient::new());
/// let config = DaemonConfig::default();
/// let manager = PostgresRequestManager::new(pool, http_client, config).await?;
///
/// // Start processing
/// let handle = manager.run()?;
///
/// // Submit requests
/// manager.submit_requests(vec![request]).await?;
/// ```
pub struct PostgresRequestManager<H: HttpClient> {
    pool: PgPool,
    http_client: Arc<H>,
    config: DaemonConfig,
}

impl<H: HttpClient + 'static> PostgresRequestManager<H> {
    /// Create a new PostgreSQL request manager.
    ///
    /// # Arguments
    /// * `pool` - PostgreSQL connection pool
    /// * `http_client` - HTTP client for making requests
    /// * `config` - Daemon configuration (batch size, concurrency limits, etc.)
    pub fn new(pool: PgPool, http_client: Arc<H>, config: DaemonConfig) -> Self {
        Self {
            pool,
            http_client,
            config,
        }
    }

    /// Create with default daemon configuration.
    pub fn with_defaults(pool: PgPool, http_client: Arc<H>) -> Self {
        Self::new(pool, http_client, DaemonConfig::default())
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
            .map_err(|e| BatcherError::Other(anyhow!("Failed to create listener: {}", e)))
    }
}

// Implement Storage trait directly (no delegation)
#[async_trait]
impl<H: HttpClient + 'static> Storage for PostgresRequestManager<H> {
    async fn submit(&self, request: Request<Pending>) -> Result<()> {
        sqlx::query!(
            r#"
            INSERT INTO requests (
                id, batch_id, template_id, state, endpoint, method, path, body, model, api_key,
                retry_attempt, not_before
            ) VALUES ($1, $2, $3, 'pending', $4, $5, $6, $7, $8, $9, $10, $11)
            "#,
            *request.data.id as Uuid,
            *request.data.batch_id as Uuid,
            *request.data.template_id as Uuid,
            request.data.endpoint,
            request.data.method,
            request.data.path,
            request.data.body,
            request.data.model,
            request.data.api_key,
            request.state.retry_attempt as i32,
            request.state.not_before,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| BatcherError::Other(anyhow!("Failed to submit request: {}", e)))?;

        Ok(())
    }

    async fn claim_requests(
        &self,
        limit: usize,
        daemon_id: DaemonId,
    ) -> Result<Vec<Request<Claimed>>> {
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
        .map_err(|e| BatcherError::Other(anyhow!("Failed to claim requests: {}", e)))?;

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
                .map_err(|e| BatcherError::Other(anyhow!("Failed to update request: {}", e)))?
                .rows_affected();

                if rows_affected == 0 {
                    return Err(BatcherError::RequestNotFound(req.data.id));
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
                .map_err(|e| BatcherError::Other(anyhow!("Failed to update request: {}", e)))?
                .rows_affected();

                if rows_affected == 0 {
                    return Err(BatcherError::RequestNotFound(req.data.id));
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
                .map_err(|e| BatcherError::Other(anyhow!("Failed to update request: {}", e)))?
                .rows_affected();

                if rows_affected == 0 {
                    return Err(BatcherError::RequestNotFound(req.data.id));
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
                .map_err(|e| BatcherError::Other(anyhow!("Failed to update request: {}", e)))?
                .rows_affected();

                if rows_affected == 0 {
                    return Err(BatcherError::RequestNotFound(req.data.id));
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
                .map_err(|e| BatcherError::Other(anyhow!("Failed to update request: {}", e)))?
                .rows_affected();

                if rows_affected == 0 {
                    return Err(BatcherError::RequestNotFound(req.data.id));
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
                .map_err(|e| BatcherError::Other(anyhow!("Failed to update request: {}", e)))?
                .rows_affected();

                if rows_affected == 0 {
                    return Err(BatcherError::RequestNotFound(req.data.id));
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
        .map_err(|e| BatcherError::Other(anyhow!("Failed to fetch requests: {}", e)))?;

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
                            BatcherError::Other(anyhow!("Missing daemon_id for claimed request"))
                        })?),
                        claimed_at: row.claimed_at.ok_or_else(|| {
                            BatcherError::Other(anyhow!("Missing claimed_at for claimed request"))
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
                                BatcherError::Other(anyhow!(
                                    "Missing daemon_id for processing request"
                                ))
                            })?),
                            claimed_at: row.claimed_at.ok_or_else(|| {
                                BatcherError::Other(anyhow!(
                                    "Missing claimed_at for processing request"
                                ))
                            })?,
                            started_at: row.started_at.ok_or_else(|| {
                                BatcherError::Other(anyhow!(
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
                            BatcherError::Other(anyhow!(
                                "Missing response_status for completed request"
                            ))
                        })? as u16,
                        response_body: row.response_body.ok_or_else(|| {
                            BatcherError::Other(anyhow!(
                                "Missing response_body for completed request"
                            ))
                        })?,
                        claimed_at: row.claimed_at.ok_or_else(|| {
                            BatcherError::Other(anyhow!("Missing claimed_at for completed request"))
                        })?,
                        started_at: row.started_at.ok_or_else(|| {
                            BatcherError::Other(anyhow!("Missing started_at for completed request"))
                        })?,
                        completed_at: row.completed_at.ok_or_else(|| {
                            BatcherError::Other(anyhow!(
                                "Missing completed_at for completed request"
                            ))
                        })?,
                    },
                    data,
                })),
                "failed" => Ok(AnyRequest::Failed(Request {
                    state: Failed {
                        error: row.error.ok_or_else(|| {
                            BatcherError::Other(anyhow!("Missing error for failed request"))
                        })?,
                        failed_at: row.failed_at.ok_or_else(|| {
                            BatcherError::Other(anyhow!("Missing failed_at for failed request"))
                        })?,
                        retry_attempt: row.retry_attempt as u32,
                    },
                    data,
                })),
                "canceled" => Ok(AnyRequest::Canceled(Request {
                    state: Canceled {
                        canceled_at: row.canceled_at.ok_or_else(|| {
                            BatcherError::Other(anyhow!("Missing canceled_at for canceled request"))
                        })?,
                    },
                    data,
                })),
                _ => Err(BatcherError::Other(anyhow!("Unknown state: {}", state))),
            };

            request_map.insert(request_id, any_request);
        }

        // Return results in the same order as the input ids
        Ok(ids
            .into_iter()
            .map(|id| {
                request_map
                    .remove(&id)
                    .unwrap_or_else(|| Err(BatcherError::RequestNotFound(id)))
            })
            .collect())
    }

    // ===================================================================
    // File and Batch Management
    // ===================================================================

    async fn create_file(
        &self,
        name: String,
        description: Option<String>,
        templates: Vec<RequestTemplateInput>,
    ) -> Result<FileId> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| BatcherError::Other(anyhow!("Failed to begin transaction: {}", e)))?;

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
        .map_err(|e| BatcherError::Other(anyhow!("Failed to create file: {}", e)))?;

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
            .map_err(|e| BatcherError::Other(anyhow!("Failed to create template: {}", e)))?;
        }

        tx.commit()
            .await
            .map_err(|e| BatcherError::Other(anyhow!("Failed to commit transaction: {}", e)))?;

        Ok(FileId(file_id))
    }

    async fn get_file(&self, file_id: FileId) -> Result<File> {
        let row = sqlx::query!(
            r#"
            SELECT id, name, description, created_at, updated_at
            FROM files
            WHERE id = $1
            "#,
            *file_id as Uuid,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| BatcherError::Other(anyhow!("Failed to fetch file: {}", e)))?
        .ok_or_else(|| BatcherError::Other(anyhow!("File not found")))?;

        Ok(File {
            id: FileId(row.id),
            name: row.name,
            description: row.description,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }

    async fn list_files(&self) -> Result<Vec<File>> {
        let rows = sqlx::query!(
            r#"
            SELECT id, name, description, created_at, updated_at
            FROM files
            ORDER BY created_at DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| BatcherError::Other(anyhow!("Failed to list files: {}", e)))?;

        Ok(rows
            .into_iter()
            .map(|row| File {
                id: FileId(row.id),
                name: row.name,
                description: row.description,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
            .collect())
    }

    async fn get_file_templates(&self, file_id: FileId) -> Result<Vec<RequestTemplate>> {
        let rows = sqlx::query!(
            r#"
            SELECT id, file_id, endpoint, method, path, body, model, api_key, created_at, updated_at
            FROM request_templates
            WHERE file_id = $1
            ORDER BY created_at ASC
            "#,
            *file_id as Uuid,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| BatcherError::Other(anyhow!("Failed to fetch templates: {}", e)))?;

        Ok(rows
            .into_iter()
            .map(|row| RequestTemplate {
                id: TemplateId(row.id),
                file_id: FileId(row.file_id),
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
        .map_err(|e| BatcherError::Other(anyhow!("Failed to delete file: {}", e)))?
        .rows_affected();

        if rows_affected == 0 {
            return Err(BatcherError::Other(anyhow!("File not found")));
        }

        Ok(())
    }

    async fn create_batch(&self, file_id: FileId) -> Result<BatchId> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| BatcherError::Other(anyhow!("Failed to begin transaction: {}", e)))?;

        // Get templates
        let templates = sqlx::query!(
            r#"
            SELECT id, endpoint, method, path, body, model, api_key
            FROM request_templates
            WHERE file_id = $1
            "#,
            *file_id as Uuid,
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| BatcherError::Other(anyhow!("Failed to fetch templates: {}", e)))?;

        if templates.is_empty() {
            return Err(BatcherError::Other(anyhow!(
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
        .map_err(|e| BatcherError::Other(anyhow!("Failed to create batch: {}", e)))?;

        // Create executions from templates
        for template in templates {
            sqlx::query!(
                r#"
                INSERT INTO requests (
                    batch_id, template_id, state,
                    endpoint, method, path, body, model, api_key,
                    retry_attempt
                )
                VALUES ($1, $2, 'pending', $3, $4, $5, $6, $7, $8, 0)
                "#,
                batch_id,
                template.id,
                template.endpoint,
                template.method,
                template.path,
                template.body,
                template.model,
                template.api_key,
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| BatcherError::Other(anyhow!("Failed to create execution: {}", e)))?;
        }

        tx.commit()
            .await
            .map_err(|e| BatcherError::Other(anyhow!("Failed to commit transaction: {}", e)))?;

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
        .map_err(|e| BatcherError::Other(anyhow!("Failed to fetch batch status: {}", e)))?
        .ok_or_else(|| BatcherError::Other(anyhow!("Batch not found")))?;

        Ok(BatchStatus {
            batch_id: BatchId(row.batch_id.ok_or_else(|| {
                BatcherError::Other(anyhow!("Batch status view missing batch_id"))
            })?),
            file_id: FileId(row.file_id.ok_or_else(|| {
                BatcherError::Other(anyhow!("Batch status view missing file_id"))
            })?),
            file_name: row.file_name.ok_or_else(|| {
                BatcherError::Other(anyhow!("Batch status view missing file_name"))
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
                BatcherError::Other(anyhow!("Batch status view missing created_at"))
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
        .map_err(|e| BatcherError::Other(anyhow!("Failed to list batches: {}", e)))?;

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
        .map_err(|e| BatcherError::Other(anyhow!("Failed to cancel batch: {}", e)))?;

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
        .map_err(|e| BatcherError::Other(anyhow!("Failed to fetch batch executions: {}", e)))?;

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
                            BatcherError::Other(anyhow!("Missing daemon_id for claimed execution"))
                        })?),
                        claimed_at: row.claimed_at.ok_or_else(|| {
                            BatcherError::Other(anyhow!("Missing claimed_at for claimed execution"))
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
                                BatcherError::Other(anyhow!(
                                    "Missing daemon_id for processing execution"
                                ))
                            })?),
                            claimed_at: row.claimed_at.ok_or_else(|| {
                                BatcherError::Other(anyhow!(
                                    "Missing claimed_at for processing execution"
                                ))
                            })?,
                            started_at: row.started_at.ok_or_else(|| {
                                BatcherError::Other(anyhow!(
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
                            BatcherError::Other(anyhow!(
                                "Missing response_status for completed execution"
                            ))
                        })? as u16,
                        response_body: row.response_body.ok_or_else(|| {
                            BatcherError::Other(anyhow!(
                                "Missing response_body for completed execution"
                            ))
                        })?,
                        claimed_at: row.claimed_at.ok_or_else(|| {
                            BatcherError::Other(anyhow!(
                                "Missing claimed_at for completed execution"
                            ))
                        })?,
                        started_at: row.started_at.ok_or_else(|| {
                            BatcherError::Other(anyhow!(
                                "Missing started_at for completed execution"
                            ))
                        })?,
                        completed_at: row.completed_at.ok_or_else(|| {
                            BatcherError::Other(anyhow!(
                                "Missing completed_at for completed execution"
                            ))
                        })?,
                    },
                    data,
                }),
                "failed" => AnyRequest::Failed(Request {
                    state: Failed {
                        error: row.error.ok_or_else(|| {
                            BatcherError::Other(anyhow!("Missing error for failed execution"))
                        })?,
                        failed_at: row.failed_at.ok_or_else(|| {
                            BatcherError::Other(anyhow!("Missing failed_at for failed execution"))
                        })?,
                        retry_attempt: row.retry_attempt as u32,
                    },
                    data,
                }),
                "canceled" => AnyRequest::Canceled(Request {
                    state: Canceled {
                        canceled_at: row.canceled_at.ok_or_else(|| {
                            BatcherError::Other(anyhow!(
                                "Missing canceled_at for canceled execution"
                            ))
                        })?,
                    },
                    data,
                }),
                _ => {
                    return Err(BatcherError::Other(anyhow!("Unknown state: {}", state)));
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

        Box::pin(async_stream::stream! {
            // Create a listener for Postgres NOTIFY events
            let mut listener = match PgListener::connect_with(&pool)
                .await
                .map_err(|e| BatcherError::Other(anyhow!("Failed to create listener: {}", e))) {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to create listener");
                    yield Err(e);
                    return;
                }
            };

            // Listen on the request_updates channel
            if let Err(e) = listener.listen("request_updates").await {
                tracing::error!(error = %e, "Failed to listen on request_updates channel");
                yield Err(BatcherError::Other(anyhow::anyhow!("Failed to listen: {}", e)));
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
                        let parsed: serde_json::Result<serde_json::Value> = serde_json::from_str(payload);

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
                                        let fetch_result: Result<Vec<Result<AnyRequest>>> = async {
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
                                            .map_err(|e| BatcherError::Other(anyhow!("Failed to fetch requests: {}", e)))?;

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
                                                    "pending" => Ok(AnyRequest::Pending(Request {
                                                        state: Pending { retry_attempt: row.retry_attempt as u32, not_before: row.not_before },
                                                        data,
                                                    })),
                                                    "claimed" => Ok(AnyRequest::Claimed(Request {
                                                        state: Claimed {
                                                            daemon_id: DaemonId(row.daemon_id.ok_or_else(|| BatcherError::Other(anyhow!("Missing daemon_id")))?),
                                                            claimed_at: row.claimed_at.ok_or_else(|| BatcherError::Other(anyhow!("Missing claimed_at")))?,
                                                            retry_attempt: row.retry_attempt as u32,
                                                        },
                                                        data,
                                                    })),
                                                    "processing" => {
                                                        let (_tx, rx) = tokio::sync::mpsc::channel(1);
                                                        let abort_handle = tokio::spawn(async {}).abort_handle();
                                                        Ok(AnyRequest::Processing(Request {
                                                            state: Processing {
                                                                daemon_id: DaemonId(row.daemon_id.ok_or_else(|| BatcherError::Other(anyhow!("Missing daemon_id")))?),
                                                                claimed_at: row.claimed_at.ok_or_else(|| BatcherError::Other(anyhow!("Missing claimed_at")))?,
                                                                started_at: row.started_at.ok_or_else(|| BatcherError::Other(anyhow!("Missing started_at")))?,
                                                                retry_attempt: row.retry_attempt as u32,
                                                                result_rx: Arc::new(Mutex::new(rx)),
                                                                abort_handle,
                                                            },
                                                            data,
                                                        }))
                                                    }
                                                    "completed" => Ok(AnyRequest::Completed(Request {
                                                        state: Completed {
                                                            response_status: row.response_status.ok_or_else(|| BatcherError::Other(anyhow!("Missing response_status")))? as u16,
                                                            response_body: row.response_body.ok_or_else(|| BatcherError::Other(anyhow!("Missing response_body")))?,
                                                            claimed_at: row.claimed_at.ok_or_else(|| BatcherError::Other(anyhow!("Missing claimed_at")))?,
                                                            started_at: row.started_at.ok_or_else(|| BatcherError::Other(anyhow!("Missing started_at")))?,
                                                            completed_at: row.completed_at.ok_or_else(|| BatcherError::Other(anyhow!("Missing completed_at")))?,
                                                        },
                                                        data,
                                                    })),
                                                    "failed" => Ok(AnyRequest::Failed(Request {
                                                        state: Failed {
                                                            error: row.error.ok_or_else(|| BatcherError::Other(anyhow!("Missing error")))?,
                                                            failed_at: row.failed_at.ok_or_else(|| BatcherError::Other(anyhow!("Missing failed_at")))?,
                                                            retry_attempt: row.retry_attempt as u32,
                                                        },
                                                        data,
                                                    })),
                                                    "canceled" => Ok(AnyRequest::Canceled(Request {
                                                        state: Canceled {
                                                            canceled_at: row.canceled_at.ok_or_else(|| BatcherError::Other(anyhow!("Missing canceled_at")))?,
                                                        },
                                                        data,
                                                    })),
                                                    _ => Err(BatcherError::Other(anyhow!("Unknown state: {}", state))),
                                                };
                                                results.push(any_request);
                                            }
                                            Ok(results)
                                        }.await;

                                        match fetch_result {
                                            Ok(results) => {
                                                if let Some(result) = results.into_iter().next() {
                                                    yield Ok(result);
                                                }
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    error = %e,
                                                    request_id = %request_id,
                                                    "Failed to fetch request after notification"
                                                );
                                                yield Err(e);
                                            }
                                        }
                                    } else {
                                        tracing::warn!(id_str = id_str, "Failed to parse UUID from notification");
                                    }
                                } else {
                                    tracing::warn!(payload = payload, "Notification payload missing 'id' field");
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
                        yield Err(BatcherError::Other(anyhow::anyhow!("Notification error: {}", e)));
                        // Don't return - keep trying to receive notifications
                    }
                }
            }
        })
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
