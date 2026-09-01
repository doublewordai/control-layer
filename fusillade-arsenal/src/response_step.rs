//! PostgreSQL implementation of [`ResponseStepStore`].
//!
//! Mirrors the structural patterns of [`PostgresRequestManager`]: a thin
//! wrapper over a [`PoolProvider`] using runtime-checked `sqlx::query()`
//! against the `response_steps` table.

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

    fn write_executor(&self) -> crate::db::RetryingPgPool {
        crate::db::RetryingPgPool::new(self.pools.write(), &self.db_retry_config)
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
async fn fetch_state(executor: crate::db::RetryingPgPool, id: StepId) -> Result<Option<String>> {
    sqlx::query("SELECT state FROM response_steps WHERE id = $1")
        .bind(id.0)
        .fetch_optional(executor)
        .await
        .map(|opt| opt.map(|row| row.get::<String, _>("state")))
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to fetch step state: {}", e)))
}

#[async_trait]
impl<P: PoolProvider> ResponseStepStore for PostgresResponseStepManager<P> {
    async fn create_step(&self, input: CreateStepInput) -> Result<StepId> {
        let id = input.id.unwrap_or_else(Uuid::new_v4);

        sqlx::query(
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
        .execute(self.write_executor())
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to insert response_step: {}", e)))?;

        Ok(StepId(id))
    }

    async fn get_step(&self, id: StepId) -> Result<Option<ResponseStep>> {
        // Reads go through the primary pool; see the type-level doc for why.
        let query = format!("SELECT {} FROM response_steps WHERE id = $1", STEP_COLUMNS);
        let row = sqlx::query(&query)
            .bind(id.0)
            .fetch_optional(self.write_executor())
            .await
            .map_err(|e| FusilladeError::Other(anyhow!("Failed to fetch response_step: {}", e)))?;

        row.as_ref().map(step_from_row).transpose()
    }

    async fn get_step_by_request(&self, request_id: RequestId) -> Result<Option<ResponseStep>> {
        // Uses response_steps_request_id_unique partial index for O(log n) lookup.
        let query = format!(
            "SELECT {} FROM response_steps WHERE request_id = $1",
            STEP_COLUMNS
        );
        let row = sqlx::query(&query)
            .bind(request_id.0)
            .fetch_optional(self.write_executor())
            .await
            .map_err(|e| {
                FusilladeError::Other(anyhow!(
                    "Failed to fetch response_step by request_id: {}",
                    e
                ))
            })?;

        row.as_ref().map(step_from_row).transpose()
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
        let query = format!(
            "SELECT {cols} FROM response_steps WHERE id = $1 \
             UNION ALL \
             SELECT {cols} FROM response_steps WHERE parent_step_id = $1 \
             ORDER BY step_sequence ASC",
            cols = STEP_COLUMNS
        );
        let rows = sqlx::query(&query)
            .bind(head_step_id.0)
            .fetch_all(self.write_executor())
            .await
            .map_err(|e| FusilladeError::Other(anyhow!("Failed to list response_steps: {}", e)))?;

        rows.iter().map(step_from_row).collect()
    }

    async fn mark_step_processing(&self, id: StepId) -> Result<()> {
        let result = sqlx::query(
            "UPDATE response_steps \
             SET state = 'processing', started_at = NOW(), updated_at = NOW() \
             WHERE id = $1 AND state = 'pending'",
        )
        .bind(id.0)
        .execute(self.write_executor())
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to mark step as processing: {}", e)))?;

        if result.rows_affected() == 0 {
            // Idempotent: the row may already be processing or terminal under
            // crash recovery; surface only if the row is genuinely missing.
            if fetch_state(self.write_executor(), id).await?.is_none() {
                return Err(FusilladeError::Other(anyhow!(
                    "response_step not found: {}",
                    id
                )));
            }
        }

        Ok(())
    }

    async fn complete_step(&self, id: StepId, response: serde_json::Value) -> Result<()> {
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
        .execute(self.write_executor())
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to complete response_step: {}", e)))?;

        if result.rows_affected() == 0 {
            return Err(match fetch_state(self.write_executor(), id).await? {
                Some(state) => FusilladeError::Other(anyhow!(
                    "response_step {} not in completable state (current: {})",
                    id,
                    state
                )),
                None => FusilladeError::Other(anyhow!("response_step not found: {}", id)),
            });
        }

        Ok(())
    }

    async fn fail_step(&self, id: StepId, error: serde_json::Value) -> Result<()> {
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
        .execute(self.write_executor())
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to fail response_step: {}", e)))?;

        if result.rows_affected() == 0 {
            return Err(match fetch_state(self.write_executor(), id).await? {
                Some(state) => FusilladeError::Other(anyhow!(
                    "response_step {} not in failable state (current: {})",
                    id,
                    state
                )),
                None => FusilladeError::Other(anyhow!("response_step not found: {}", id)),
            });
        }

        Ok(())
    }

    async fn cancel_step(&self, id: StepId) -> Result<()> {
        let result = sqlx::query(
            "UPDATE response_steps \
             SET state = 'canceled', \
                 canceled_at = NOW(), \
                 updated_at = NOW() \
             WHERE id = $1 AND state IN ('pending', 'processing')",
        )
        .bind(id.0)
        .execute(self.write_executor())
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to cancel response_step: {}", e)))?;

        if result.rows_affected() == 0 {
            return Err(match fetch_state(self.write_executor(), id).await? {
                Some(state) => FusilladeError::Other(anyhow!(
                    "response_step {} not in cancelable state (current: {})",
                    id,
                    state
                )),
                None => FusilladeError::Other(anyhow!("response_step not found: {}", id)),
            });
        }

        Ok(())
    }

    async fn requeue_step_for_retry(&self, id: StepId) -> Result<()> {
        let result = sqlx::query(
            "UPDATE response_steps \
             SET state = 'pending', \
                 retry_attempt = retry_attempt + 1, \
                 started_at = NULL, \
                 updated_at = NOW() \
             WHERE id = $1 AND state = 'processing'",
        )
        .bind(id.0)
        .execute(self.write_executor())
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to requeue response_step: {}", e)))?;

        if result.rows_affected() == 0 {
            return Err(match fetch_state(self.write_executor(), id).await? {
                Some(state) => FusilladeError::Other(anyhow!(
                    "response_step {} not in retryable state (current: {})",
                    id,
                    state
                )),
                None => FusilladeError::Other(anyhow!("response_step not found: {}", id)),
            });
        }

        Ok(())
    }
}
