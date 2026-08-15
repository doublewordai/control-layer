//! PostgreSQL implementation of [`ResponseStepStore`].
//!
//! Mirrors the structural patterns of [`PostgresRequestManager`]: a thin
//! wrapper over a [`PoolProvider`] using runtime-checked `sqlx::query()`.
//! Point reads resolve the live table first and then exact active retained
//! routes; lifecycle mutations remain live-only.

use anyhow::anyhow;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::Row;
use uuid::Uuid;

use crate::error::{FusilladeError, Result};
use crate::request::RequestId;
use crate::response_step::{
    CreateStepInput, ResponseStep, ResponseStepStore, StepId, StepKind, StepState,
};

pub use sqlx_pool_router::PoolProvider;

/// Content-free conflict returned when a response-step mutation targets a
/// graph that has already moved out of the live tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetainedResponseStepConflict;

impl std::fmt::Display for RetainedResponseStepConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Response step is already retained")
    }
}

impl std::error::Error for RetainedResponseStepConflict {}

/// Content-free outcome for a step whose response graph was erased or whose
/// retained partition is no longer readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseStepNotFound;

impl std::fmt::Display for ResponseStepNotFound {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Response step is no longer available")
    }
}

impl std::error::Error for ResponseStepNotFound {}

/// PostgreSQL implementation of [`ResponseStepStore`].
///
/// Holds a [`PoolProvider`] for write/read pool selection, mirroring
/// [`crate::PostgresRequestManager`]. Construct directly or share the
/// same `PoolProvider` instance with a request manager so that both
/// stores see consistent reads.
///
/// All read methods on this impl route through the **write/primary**
/// pool. The orchestration loop reads its own freshly-written rows on
/// every iteration (e.g., `list_chain` after `create_step` to confirm the
/// frontier under crash recovery), and read-replica lag would surface as
/// `None` or stale rows. The dashboard, if it ever queries the
/// `response_steps` table, should grow a separate replica-routed read
/// path rather than re-using these methods.
pub struct PostgresResponseStepManager<P: PoolProvider> {
    pools: P,
    db_retry_config: crate::DbRetryConfig,
}

impl<P: PoolProvider> PostgresResponseStepManager<P> {
    pub fn new(pools: P) -> Self {
        Self {
            pools,
            db_retry_config: crate::DbRetryConfig::default(),
        }
    }

    /// Set the retry cadence for transient pool acquisition failures.
    pub fn with_db_retry_config(mut self, config: crate::DbRetryConfig) -> Self {
        self.db_retry_config = config;
        self
    }

    pub fn db_retry_config(&self) -> &crate::DbRetryConfig {
        &self.db_retry_config
    }

    async fn begin_write_transaction(&self) -> Result<sqlx::Transaction<'static, sqlx::Postgres>> {
        crate::db::begin_transaction(self.pools.write(), &self.db_retry_config)
            .await
            .map_err(|_| FusilladeError::Other(anyhow!("Failed to begin response-step mutation")))
    }

    async fn begin_primary_read(&self) -> Result<sqlx::Transaction<'static, sqlx::Postgres>> {
        let mut tx = crate::db::begin_transaction(self.pools.write(), &self.db_retry_config)
            .await
            .map_err(|_| FusilladeError::Other(anyhow!("Failed to begin response-step read")))?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(|_| {
                FusilladeError::Other(anyhow!("Failed to configure response-step read"))
            })?;
        Ok(tx)
    }

    async fn write_conflict_in_transaction(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        step_ids: &[Uuid],
        request_id: Option<RequestId>,
    ) -> Result<Option<FusilladeError>> {
        let mut object_ids = step_ids.to_vec();
        object_ids.extend(request_id.map(|id| id.0));
        crate::postgres::retained_response::classify_response_write(tx, &object_ids)
            .await
            .map(|disposition| {
                disposition.map(|disposition| match disposition {
                    crate::postgres::retained_response::ResponseWriteDisposition::AlreadyRetained => {
                        FusilladeError::Other(anyhow::Error::new(RetainedResponseStepConflict))
                    }
                    crate::postgres::retained_response::ResponseWriteDisposition::NotFound => {
                        FusilladeError::Other(anyhow::Error::new(ResponseStepNotFound))
                    }
                })
            })
    }
}

fn step_from_row(row: &sqlx::postgres::PgRow) -> Result<ResponseStep> {
    let kind_str: String = row.get("step_kind");
    let kind = StepKind::parse(&kind_str).ok_or_else(|| {
        FusilladeError::Other(anyhow!("Unknown step_kind in response_steps: {}", kind_str))
    })?;

    let state_str: String = row.get("state");
    let state = StepState::parse(&state_str).ok_or_else(|| {
        FusilladeError::Other(anyhow!("Unknown state in response_steps: {}", state_str))
    })?;

    Ok(ResponseStep {
        id: StepId(row.get("id")),
        request_id: row.get::<Option<Uuid>, _>("request_id").map(RequestId),
        prev_step_id: row.get::<Option<Uuid>, _>("prev_step_id").map(StepId),
        parent_step_id: row.get::<Option<Uuid>, _>("parent_step_id").map(StepId),
        step_kind: kind,
        step_sequence: row.get("step_sequence"),
        request_payload: row.get("request_payload"),
        response_payload: row.get::<Option<serde_json::Value>, _>("response_payload"),
        state,
        started_at: row.get::<Option<DateTime<Utc>>, _>("started_at"),
        completed_at: row.get::<Option<DateTime<Utc>>, _>("completed_at"),
        failed_at: row.get::<Option<DateTime<Utc>>, _>("failed_at"),
        canceled_at: row.get::<Option<DateTime<Utc>>, _>("canceled_at"),
        retry_attempt: row.get("retry_attempt"),
        error: row.get::<Option<serde_json::Value>, _>("error"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

const STEP_COLUMNS: &str = "id, request_id, prev_step_id, parent_step_id, step_kind, step_sequence, \
    request_payload, response_payload, state, started_at, completed_at, failed_at, \
    canceled_at, retry_attempt, error, created_at, updated_at";

/// Look up the current state of a step. Used by the lifecycle update
/// methods to disambiguate "row not found" from "row in unexpected state"
/// after a 0-rows-affected update.
async fn fetch_state_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: StepId,
) -> Result<Option<String>> {
    sqlx::query("SELECT state FROM response_steps WHERE id = $1")
        .bind(id.0)
        .fetch_optional(&mut **tx)
        .await
        .map(|opt| opt.map(|row| row.get::<String, _>("state")))
        .map_err(|_| FusilladeError::Other(anyhow!("Failed to fetch response-step state")))
}

#[async_trait]
impl<P: PoolProvider> ResponseStepStore for PostgresResponseStepManager<P> {
    async fn create_step(&self, input: CreateStepInput) -> Result<StepId> {
        let id = input.id.unwrap_or_else(Uuid::new_v4);
        let mut linked_step_ids = vec![id];
        linked_step_ids.extend(input.prev_step_id.map(|step| step.0));
        linked_step_ids.extend(input.parent_step_id.map(|step| step.0));

        let mut tx = self.begin_write_transaction().await?;
        let mut lifecycle_ids = linked_step_ids.clone();
        lifecycle_ids.extend(input.request_id.map(|request_id| request_id.0));
        crate::postgres::retained_response::lock_response_write_graphs(&mut tx, &lifecycle_ids)
            .await?;

        if let Some(conflict) =
            Self::write_conflict_in_transaction(&mut tx, &linked_step_ids, input.request_id).await?
        {
            tx.rollback().await.map_err(|_| {
                FusilladeError::Other(anyhow!("Failed to roll back response-step mutation"))
            })?;
            return Err(conflict);
        }

        let insert = sqlx::query(
            "INSERT INTO response_steps \
             (id, request_id, prev_step_id, parent_step_id, step_kind, step_sequence, request_payload) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id)
        .bind(input.request_id.map(|r| r.0))
        .bind(input.prev_step_id.map(|s| s.0))
        .bind(input.parent_step_id.map(|s| s.0))
        .bind(input.step_kind.as_str())
        .bind(input.step_sequence)
        .bind(&input.request_payload)
        .execute(&mut *tx)
        .await;
        if let Err(error) = insert {
            let _ = error;
            return Err(FusilladeError::Other(anyhow!(
                "Failed to insert response step"
            )));
        }

        // A same-ID unique check may wait for movement's DELETE and then
        // succeed after movement commits. This second statement shares the
        // creator's write transaction, so an active route rolls the INSERT
        // back before it can become visible.
        if let Some(conflict) =
            Self::write_conflict_in_transaction(&mut tx, &linked_step_ids, input.request_id).await?
        {
            tx.rollback().await.map_err(|_| {
                FusilladeError::Other(anyhow!("Failed to roll back response-step mutation"))
            })?;
            return Err(conflict);
        }

        tx.commit().await.map_err(|_| {
            FusilladeError::Other(anyhow!("Failed to finish response-step mutation"))
        })?;

        Ok(StepId(id))
    }

    async fn get_step(&self, id: StepId) -> Result<Option<ResponseStep>> {
        let mut tx = self.begin_primary_read().await?;
        let query = format!("SELECT {} FROM response_steps WHERE id = $1", STEP_COLUMNS);
        let row = sqlx::query(&query)
            .bind(id.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| FusilladeError::Other(anyhow!("Failed to fetch response step")))?;

        let step = match row.as_ref().map(step_from_row).transpose()? {
            Some(step) => Some(step),
            None => crate::postgres::retained_response::get_step(&mut tx, id).await?,
        };
        tx.commit()
            .await
            .map_err(|_| FusilladeError::Other(anyhow!("Failed to finish response-step read")))?;
        Ok(step)
    }

    async fn get_step_by_request(&self, request_id: RequestId) -> Result<Option<ResponseStep>> {
        let mut tx = self.begin_primary_read().await?;
        // Uses response_steps_request_id_unique partial index for O(log n) lookup.
        let query = format!(
            "SELECT {} FROM response_steps WHERE request_id = $1",
            STEP_COLUMNS
        );
        let row = sqlx::query(&query)
            .bind(request_id.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| {
                FusilladeError::Other(anyhow!("Failed to fetch response step by request"))
            })?;

        let step = match row.as_ref().map(step_from_row).transpose()? {
            Some(step) => Some(step),
            None => {
                crate::postgres::retained_response::get_step_by_request(&mut tx, request_id).await?
            }
        };
        tx.commit()
            .await
            .map_err(|_| FusilladeError::Other(anyhow!("Failed to finish response-step read")))?;
        Ok(step)
    }

    async fn list_chain(&self, head_step_id: StepId) -> Result<Vec<ResponseStep>> {
        // Returns the head + every descendant. The two arms each hit a
        // different index, so they're written as a UNION ALL rather than
        // an OR predicate (which can degenerate into a bitmap-or that
        // ignores one of the indexes under planner pressure):
        //   * head:        primary key lookup on `id`
        //   * descendants: partial index `response_steps_chain_walk`
        //                  on (parent_step_id, step_sequence) WHERE
        //                  parent_step_id IS NOT NULL
        // The two sets are disjoint (the head's parent_step_id is NULL
        // by invariant, and descendants have a distinct id), so UNION
        // ALL — cheaper than UNION's dedup — is correct.
        let mut tx = self.begin_primary_read().await?;
        let query = format!(
            "SELECT {cols} FROM response_steps WHERE id = $1 \
             UNION ALL \
             SELECT {cols} FROM response_steps WHERE parent_step_id = $1 \
             ORDER BY step_sequence ASC",
            cols = STEP_COLUMNS
        );
        let rows = sqlx::query(&query)
            .bind(head_step_id.0)
            .fetch_all(&mut *tx)
            .await
            .map_err(|_| FusilladeError::Other(anyhow!("Failed to list response steps")))?;

        let steps = if rows.is_empty() {
            crate::postgres::retained_response::list_chain(&mut tx, head_step_id).await?
        } else {
            rows.iter().map(step_from_row).collect::<Result<Vec<_>>>()?
        };
        tx.commit()
            .await
            .map_err(|_| FusilladeError::Other(anyhow!("Failed to finish response-step read")))?;
        Ok(steps)
    }

    async fn mark_step_processing(&self, id: StepId) -> Result<()> {
        let mut tx = self.begin_write_transaction().await?;
        crate::postgres::retained_response::lock_response_write_graphs(&mut tx, &[id.0]).await?;
        let result = sqlx::query(
            "UPDATE response_steps \
             SET state = 'processing', started_at = NOW(), updated_at = NOW() \
             WHERE id = $1 AND state = 'pending'",
        )
        .bind(id.0)
        .execute(&mut *tx)
        .await
        .map_err(|_| {
            FusilladeError::Other(anyhow!("Failed to mark response step as processing"))
        })?;

        if result.rows_affected() == 0 {
            // Idempotent: the row may already be processing or terminal under
            // crash recovery; surface only if the row is genuinely missing.
            if fetch_state_in_transaction(&mut tx, id).await?.is_none() {
                if let Some(conflict) =
                    Self::write_conflict_in_transaction(&mut tx, &[id.0], None).await?
                {
                    return Err(conflict);
                }
                return Err(FusilladeError::Other(anyhow::Error::new(
                    ResponseStepNotFound,
                )));
            }
        }

        tx.commit().await.map_err(|_| {
            FusilladeError::Other(anyhow!("Failed to finish response-step mutation"))
        })?;
        Ok(())
    }

    async fn complete_step(&self, id: StepId, response: serde_json::Value) -> Result<()> {
        let mut tx = self.begin_write_transaction().await?;
        crate::postgres::retained_response::lock_response_write_graphs(&mut tx, &[id.0]).await?;
        let result = sqlx::query(
            "UPDATE response_steps \
             SET state = 'completed', \
                 response_payload = $2, \
                 completed_at = NOW(), \
                 updated_at = NOW() \
             WHERE id = $1 AND state IN ('pending', 'processing')",
        )
        .bind(id.0)
        .bind(&response)
        .execute(&mut *tx)
        .await
        .map_err(|_| FusilladeError::Other(anyhow!("Failed to complete response step")))?;

        if result.rows_affected() == 0 {
            return Err(match fetch_state_in_transaction(&mut tx, id).await? {
                Some(_) => {
                    FusilladeError::Other(anyhow!("Response step is not in completable state"))
                }
                None => Self::write_conflict_in_transaction(&mut tx, &[id.0], None)
                    .await?
                    .unwrap_or_else(|| {
                        FusilladeError::Other(anyhow::Error::new(ResponseStepNotFound))
                    }),
            });
        }

        tx.commit().await.map_err(|_| {
            FusilladeError::Other(anyhow!("Failed to finish response-step mutation"))
        })?;
        Ok(())
    }

    async fn fail_step(&self, id: StepId, error: serde_json::Value) -> Result<()> {
        let mut tx = self.begin_write_transaction().await?;
        crate::postgres::retained_response::lock_response_write_graphs(&mut tx, &[id.0]).await?;
        let result = sqlx::query(
            "UPDATE response_steps \
             SET state = 'failed', \
                 error = $2, \
                 failed_at = NOW(), \
                 updated_at = NOW() \
             WHERE id = $1 AND state IN ('pending', 'processing')",
        )
        .bind(id.0)
        .bind(&error)
        .execute(&mut *tx)
        .await
        .map_err(|_| FusilladeError::Other(anyhow!("Failed to fail response step")))?;

        if result.rows_affected() == 0 {
            return Err(match fetch_state_in_transaction(&mut tx, id).await? {
                Some(_) => FusilladeError::Other(anyhow!("Response step is not in failable state")),
                None => Self::write_conflict_in_transaction(&mut tx, &[id.0], None)
                    .await?
                    .unwrap_or_else(|| {
                        FusilladeError::Other(anyhow::Error::new(ResponseStepNotFound))
                    }),
            });
        }

        tx.commit().await.map_err(|_| {
            FusilladeError::Other(anyhow!("Failed to finish response-step mutation"))
        })?;
        Ok(())
    }

    async fn cancel_step(&self, id: StepId) -> Result<()> {
        let mut tx = self.begin_write_transaction().await?;
        crate::postgres::retained_response::lock_response_write_graphs(&mut tx, &[id.0]).await?;
        let result = sqlx::query(
            "UPDATE response_steps \
             SET state = 'canceled', \
                 canceled_at = NOW(), \
                 updated_at = NOW() \
             WHERE id = $1 AND state IN ('pending', 'processing')",
        )
        .bind(id.0)
        .execute(&mut *tx)
        .await
        .map_err(|_| FusilladeError::Other(anyhow!("Failed to cancel response step")))?;

        if result.rows_affected() == 0 {
            return Err(match fetch_state_in_transaction(&mut tx, id).await? {
                Some(_) => {
                    FusilladeError::Other(anyhow!("Response step is not in cancelable state"))
                }
                None => Self::write_conflict_in_transaction(&mut tx, &[id.0], None)
                    .await?
                    .unwrap_or_else(|| {
                        FusilladeError::Other(anyhow::Error::new(ResponseStepNotFound))
                    }),
            });
        }

        tx.commit().await.map_err(|_| {
            FusilladeError::Other(anyhow!("Failed to finish response-step mutation"))
        })?;
        Ok(())
    }

    async fn requeue_step_for_retry(&self, id: StepId) -> Result<()> {
        let mut tx = self.begin_write_transaction().await?;
        crate::postgres::retained_response::lock_response_write_graphs(&mut tx, &[id.0]).await?;
        let result = sqlx::query(
            "UPDATE response_steps \
             SET state = 'pending', \
                 retry_attempt = retry_attempt + 1, \
                 started_at = NULL, \
                 updated_at = NOW() \
             WHERE id = $1 AND state = 'processing'",
        )
        .bind(id.0)
        .execute(&mut *tx)
        .await
        .map_err(|_| FusilladeError::Other(anyhow!("Failed to requeue response step")))?;

        if result.rows_affected() == 0 {
            return Err(match fetch_state_in_transaction(&mut tx, id).await? {
                Some(_) => {
                    FusilladeError::Other(anyhow!("Response step is not in retryable state"))
                }
                None => Self::write_conflict_in_transaction(&mut tx, &[id.0], None)
                    .await?
                    .unwrap_or_else(|| {
                        FusilladeError::Other(anyhow::Error::new(ResponseStepNotFound))
                    }),
            });
        }

        tx.commit().await.map_err(|_| {
            FusilladeError::Other(anyhow!("Failed to finish response-step mutation"))
        })?;
        Ok(())
    }
}
