//! Outstanding batch work per model, read from Fusillade's tables for batch
//! admission (`sla_capacity::reserve_capacity`).
//!
//! The connection must be a Fusillade transaction on the **primary**: the
//! admission race argument (see `reserve_capacity`) relies on the snapshot
//! including every batch committed before the caller's `since` instant, which a
//! lagging replica cannot guarantee.

use std::collections::HashMap;

use sqlx::{PgConnection, Row};

use crate::db::errors::{DbError, Result};

pub struct BatchAdmissionDemand<'c> {
    db: &'c mut PgConnection,
}

impl<'c> BatchAdmissionDemand<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    /// Count admitted, not-yet-finished batch requests per model and window.
    ///
    /// For each `(label, horizon_seconds)` window, a request counts towards the
    /// window when its batch is due before `now() + horizon_seconds`. Work due
    /// sooner consumes the capacity of every longer window too, so a 1h batch's
    /// rows count towards both the 1h and the 24h window. Overdue work (batch
    /// already past its deadline but still active) counts towards every window.
    ///
    /// What is counted, per active (non-terminal, non-cancelling, non-deleted)
    /// batch with a deadline:
    ///
    /// * populated batches (`requests_started_at` set): their
    ///   pending/claimed/processing request rows, whatever their service tier
    ///   (24h rows carry a NULL tier, 1h rows `flex`);
    /// * unpopulated batches (`requests_started_at` NULL — created, but the
    ///   background population job has not inserted their rows yet): their
    ///   template count from the input file. `requests_started_at` is stamped
    ///   in the same transaction as the row INSERT, so within this single
    ///   statement snapshot a batch is counted exactly one way.
    ///
    /// Not counted: background batches (no deadline) and batchless rows (async
    /// Responses API work), which are not admitted through this path.
    ///
    /// Cost: an index range scan of the active-batch partial index
    /// (`idx_batches_claimable_expiration_with_id`), then per model an index
    /// (mostly index-only) scan over that model's active request rows in
    /// `idx_requests_active_batched_demand`, plus a `(file_id, model)` index scan
    /// of the templates of unpopulated batches. It is proportional to the
    /// requested models' outstanding backlog, which admission itself bounds by
    /// `throughput × window`, and never touches terminal or archived rows.
    pub async fn outstanding_by_model_and_window(
        &mut self,
        models: &[String],
        windows: &[(String, i64)],
        statement_timeout_ms: u64,
    ) -> Result<HashMap<String, HashMap<String, i64>>> {
        if models.is_empty() || windows.is_empty() {
            return Ok(HashMap::new());
        }
        let labels: Vec<String> = windows.iter().map(|(label, _)| label.clone()).collect();
        let horizons: Vec<i64> = windows.iter().map(|(_, secs)| *secs).collect();

        // Bound the statement: admission fails open on error, so a slow count
        // must not hold a pooled connection (or a request) for longer than this.
        // The value is an integer from config, not user input.
        sqlx::query(&format!("SET LOCAL statement_timeout = {}", statement_timeout_ms.max(1)))
            .execute(&mut *self.db)
            .await?;
        // Bound array parameters must not push a long-lived pooled connection
        // onto a generic plan that cannot prove the partial-index predicates.
        sqlx::query("SET LOCAL plan_cache_mode = force_custom_plan")
            .execute(&mut *self.db)
            .await?;

        let rows = sqlx::query(
            r#"
            WITH windows AS (
                SELECT w.label, w.horizon_seconds
                FROM UNNEST($2::text[], $3::bigint[]) AS w(label, horizon_seconds)
            ),
            active_batches AS MATERIALIZED (
                -- Same eligibility as the claim path's active-batch filter;
                -- served by idx_batches_claimable_expiration_with_id.
                SELECT b.id, b.file_id, b.expires_at, b.requests_started_at IS NOT NULL AS populated
                FROM batches b
                WHERE b.cancelling_at IS NULL
                  AND b.deleted_at IS NULL
                  AND b.completed_at IS NULL
                  AND b.failed_at IS NULL
                  AND b.cancelled_at IS NULL
                  AND b.expires_at < NOW() + make_interval(secs => (SELECT MAX(horizon_seconds) FROM windows))
            ),
            demand AS (
                SELECT r.model, ab.expires_at, COUNT(*)::BIGINT AS n
                FROM active_batches ab
                JOIN requests r ON r.batch_id = ab.id
                WHERE ab.populated
                  AND r.model = ANY($1)
                  AND r.state IN ('pending', 'claimed', 'processing')
                  -- Spelled out so idx_requests_active_batched_demand's
                  -- predicate is provable.
                  AND r.batch_id IS NOT NULL
                  AND r.template_id IS NOT NULL
                  AND r.service_tier IS DISTINCT FROM 'background'
                GROUP BY r.model, ab.expires_at
                UNION ALL
                SELECT t.model, ab.expires_at, COUNT(*)::BIGINT AS n
                FROM active_batches ab
                JOIN request_templates_all t ON t.file_id = ab.file_id
                WHERE NOT ab.populated
                  -- An unpopulated batch past its deadline is stuck, not work
                  -- the daemon will run; don't let it hold capacity forever.
                  AND ab.expires_at > NOW()
                  AND t.model = ANY($1)
                GROUP BY t.model, ab.expires_at
            )
            SELECT d.model, w.label, SUM(d.n)::BIGINT AS outstanding
            FROM demand d
            JOIN windows w ON d.expires_at < NOW() + make_interval(secs => w.horizon_seconds)
            GROUP BY d.model, w.label
            "#,
        )
        .bind(models)
        .bind(&labels)
        .bind(&horizons)
        .fetch_all(&mut *self.db)
        .await?;

        let mut result: HashMap<String, HashMap<String, i64>> = HashMap::new();
        for row in rows {
            let model: String = row.try_get("model").map_err(DbError::from)?;
            let label: String = row.try_get("label").map_err(DbError::from)?;
            let outstanding: i64 = row.try_get("outstanding").map_err(DbError::from)?;
            *result.entry(model).or_default().entry(label).or_insert(0) += outstanding;
        }
        Ok(result)
    }
}
