//! PostgreSQL storage implementation for the batcher system.
//!
//! This module provides a production-ready storage backend using PostgreSQL with:
//! - Atomic claim operations using SELECT FOR UPDATE
//! - LISTEN/NOTIFY for real-time status updates
//! - Normalized schema with proper indexes
//! - Connection pooling via sqlx
//! - Compile-time checked queries using sqlx macros

use anyhow::anyhow;
use chrono::Utc;
use sqlx::postgres::{PgListener, PgPool};
use uuid::Uuid;

use crate::error::{BatcherError, Result};
use crate::request::{
    AnyRequest, Canceled, Claimed, Completed, DaemonId, Failed, Pending, Request, RequestData,
    RequestId, RequestState,
};
use crate::storage::Storage;

/// PostgreSQL storage backend.
///
/// This implementation uses a connection pool and provides atomic operations
/// for request lifecycle management with compile-time SQL verification.
#[derive(Clone)]
pub struct PostgresStorage {
    pool: PgPool,
}

impl PostgresStorage {
    /// Create a new PostgresStorage instance with the given connection pool.
    ///
    /// # Example
    /// ```ignore
    /// let pool = PgPool::connect("postgresql://localhost/batcher").await?;
    /// let storage = PostgresStorage::new(pool);
    /// ```
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Get the connection pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Create a listener for real-time request updates.
    ///
    /// This returns a PgListener that can be used to receive notifications
    /// when requests are updated.
    ///
    /// # Example
    /// ```ignore
    /// let mut listener = storage.create_listener().await?;
    /// listener.listen("request_updates").await?;
    ///
    /// while let Some(notification) = listener.recv().await {
    ///     println!("Update: {}", notification.payload());
    /// }
    /// ```
    pub async fn create_listener(&self) -> Result<PgListener> {
        PgListener::connect_with(&self.pool)
            .await
            .map_err(|e| BatcherError::Other(anyhow!("Failed to create listener: {}", e)))
    }
}

impl Storage for PostgresStorage {
    async fn submit(&self, request: Request<Pending>) -> Result<()> {
        sqlx::query!(
            r#"
            INSERT INTO requests (
                id, state, endpoint, method, path, body, model, api_key,
                retry_attempt, not_before
            ) VALUES ($1, 'pending', $2, $3, $4, $5, $6, $7, $8, $9)
            "#,
            *request.data.id as Uuid,
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

        // Atomically claim pending requests using SELECT FOR UPDATE
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
            RETURNING id, endpoint, method, path, body, model, api_key, retry_attempt
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

    async fn view_pending_requests(
        &self,
        limit: usize,
        _daemon_id: Option<DaemonId>,
    ) -> Result<Vec<Request<Pending>>> {
        let now = Utc::now();

        let rows = sqlx::query!(
            r#"
            SELECT id, endpoint, method, path, body, model, api_key, retry_attempt, not_before
            FROM requests
            WHERE state = 'pending'
                AND (not_before IS NULL OR not_before <= $1)
            ORDER BY created_at ASC
            LIMIT $2
            "#,
            now,
            limit as i64,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| BatcherError::Other(anyhow!("Failed to fetch pending requests: {}", e)))?;

        Ok(rows
            .into_iter()
            .map(|row| Request {
                state: Pending {
                    retry_attempt: row.retry_attempt as u32,
                    not_before: row.not_before,
                },
                data: RequestData {
                    id: RequestId(row.id),
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

    async fn get_requests(&self, ids: Vec<RequestId>) -> Result<Vec<Result<AnyRequest>>> {
        let uuid_ids: Vec<Uuid> = ids.iter().map(|id| **id).collect();

        let rows = sqlx::query!(
            r#"
            SELECT
                id, state::text AS state, endpoint, method, path, body, model, api_key,
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
                endpoint: row.endpoint,
                method: row.method,
                path: row.path,
                body: row.body,
                model: row.model,
                api_key: row.api_key,
            };

            let state = row.state.as_ref().ok_or_else(|| {
                BatcherError::Other(anyhow!("Missing state for request"))
            })?;

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
                    // Cannot reconstruct Processing state from database
                    Err(BatcherError::Other(anyhow!(
                        "Cannot reconstruct Processing state from database"
                    )))
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
                        error: row
                            .error
                            .ok_or_else(|| {
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
                            BatcherError::Other(anyhow!(
                                "Missing canceled_at for canceled request"
                            ))
                        })?,
                    },
                    data,
                })),
                _ => Err(BatcherError::Other(anyhow!(
                    "Unknown state: {}",
                    state
                ))),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to create a test pool (requires DATABASE_URL env var)
    async fn create_test_pool() -> PgPool {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
        PgPool::connect(&database_url)
            .await
            .expect("Failed to connect to test database")
    }

    #[tokio::test]
    #[ignore] // Run with: cargo test --features postgres -- --ignored
    async fn test_submit_and_get() {
        let pool = create_test_pool().await;
        let storage = PostgresStorage::new(pool);

        let request_id = RequestId(Uuid::new_v4());
        let request = Request {
            state: Pending {
                retry_attempt: 0,
                not_before: None,
            },
            data: RequestData {
                id: request_id,
                endpoint: "https://api.example.com".to_string(),
                method: "POST".to_string(),
                path: "/v1/test".to_string(),
                body: r#"{"key": "value"}"#.to_string(),
                model: "test-model".to_string(),
                api_key: "test-key".to_string(),
            },
        };

        storage.submit(request.clone()).await.unwrap();

        let results = storage.get_requests(vec![request_id]).await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_ok());

        let retrieved = results[0].as_ref().unwrap();
        assert!(retrieved.is_pending());
    }

    #[tokio::test]
    #[ignore]
    async fn test_claim_requests() {
        let pool = create_test_pool().await;
        let storage = PostgresStorage::new(pool);

        let daemon_id = DaemonId(Uuid::new_v4());

        // Submit 5 pending requests
        for i in 0..5 {
            let request = Request {
                state: Pending {
                    retry_attempt: 0,
                    not_before: None,
                },
                data: RequestData {
                    id: RequestId(Uuid::new_v4()),
                    endpoint: "https://api.example.com".to_string(),
                    method: "POST".to_string(),
                    path: "/v1/test".to_string(),
                    body: format!(r#"{{"test": {}}}"#, i),
                    model: "test-model".to_string(),
                    api_key: "test-key".to_string(),
                },
            };
            storage.submit(request).await.unwrap();
        }

        // Claim 3 requests
        let claimed = storage.claim_requests(3, daemon_id).await.unwrap();
        assert_eq!(claimed.len(), 3);

        for req in claimed {
            assert_eq!(req.state.daemon_id, daemon_id);
        }
    }
}
