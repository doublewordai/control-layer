//! Underway task retention.
//!
//! Every Underway task carries a `ttl` (the crate's default is 14 days) but the crate only
//! deletes expired rows when its deletion routine is running, and that routine is one
//! unbounded `DELETE`. This daemon is the bounded replacement: it deletes expired tasks
//! oldest-first in small batches with a pause between them, so the task table stops
//! growing without ever holding a long transaction or a large lock.
//!
//! A row is expired when it is not `in_progress`, is older than the configured floor, and
//! its own `created_at + ttl` has passed. The floor (`min_age_days`, default 14 = the crate
//! default) bounds the index range scan; without it the sweep would have to read every row
//! to evaluate the per-row `ttl`. Tasks that set a longer `ttl` are still kept for as long
//! as they ask.
//!
//! Every pod runs the daemon; the sweep takes a transaction-scoped advisory lock, so
//! concurrent sweeps collapse to one and the rest wait for the next tick. `SKIP LOCKED`
//! keeps the sweep off rows a worker is claiming at that moment.

use std::time::Duration;

use sqlx::PgConnection;
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument};

use crate::config::TaskRetentionConfig;
use crate::metrics::errors::component::TASK_RETENTION;

/// Advisory lock key shared by every sweep. Distinct from leader election's key.
const SWEEP_LOCK_KEY: i64 = 0x7a5b_5265_7465_6e74; // "zRetent"

/// Delete one batch of expired tasks. Returns the number of rows deleted.
///
/// Runs in its own transaction: the batch is selected `FOR UPDATE SKIP LOCKED` so it never
/// contends with a worker claiming a task, and `task_attempt` rows follow through the
/// dependency's `ON DELETE CASCADE`.
#[instrument(skip(conn), fields(batch_size, min_age_days = min_age.as_secs() / 86_400))]
pub async fn purge_expired_batch(conn: &mut PgConnection, batch_size: i64, min_age: Duration) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        r#"
        WITH victims AS (
            SELECT task_queue_name, id
            FROM underway.task
            WHERE state <> 'in_progress'
              AND created_at < now() - make_interval(secs => $2)
              AND created_at + ttl < now()
            ORDER BY created_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        DELETE FROM underway.task t
        USING victims v
        WHERE t.task_queue_name = v.task_queue_name
          AND t.id = v.id
        "#,
    )
    .bind(batch_size)
    .bind(min_age.as_secs_f64())
    .execute(conn)
    .await?;
    Ok(result.rows_affected())
}

/// One sweep: repeat batches until a batch comes back short, pausing between them.
///
/// Returns the total rows deleted, or `None` when another sweep held the lock.
async fn sweep(pool: &sqlx::PgPool, config: &TaskRetentionConfig, shutdown: &CancellationToken) -> Result<Option<u64>, sqlx::Error> {
    let batch_size = config.batch_size.max(1) as i64;
    let min_age = Duration::from_secs(config.min_age_days * 86_400);
    let pause = Duration::from_millis(config.batch_pause_milliseconds);

    let mut total = 0u64;
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        let mut tx = pool.begin().await?;
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
            .bind(SWEEP_LOCK_KEY)
            .fetch_one(&mut *tx)
            .await?;
        if !locked {
            tx.rollback().await?;
            return Ok(None);
        }
        let deleted = purge_expired_batch(&mut tx, batch_size, min_age).await?;
        tx.commit().await?;
        total += deleted;
        if deleted < batch_size as u64 {
            break;
        }
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(pause) => {}
        }
    }
    Ok(Some(total))
}

/// Run the retention daemon until `shutdown` is cancelled.
#[instrument(skip_all)]
pub async fn run_task_retention_daemon(
    pools: impl sqlx_pool_router::PoolProvider,
    config: TaskRetentionConfig,
    shutdown: CancellationToken,
) {
    let pools = sqlx_pool_router::DynPools::new(pools);
    let interval = Duration::from_secs(config.interval_seconds.max(1));

    info!(
        interval_s = interval.as_secs(),
        batch_size = config.batch_size,
        batch_pause_ms = config.batch_pause_milliseconds,
        min_age_days = config.min_age_days,
        "Task-retention daemon started"
    );

    loop {
        match sweep(&pools.write(), &config, &shutdown).await {
            Ok(Some(deleted)) if deleted > 0 => info!(deleted, "Task-retention sweep deleted expired tasks"),
            Ok(Some(_)) => {}
            Ok(None) => tracing::debug!("Task-retention sweep skipped: another instance holds the lock"),
            Err(e) => {
                crate::background_error!(TASK_RETENTION, "sweep_failed", Error, error = %e, "Task-retention sweep failed");
            }
        }

        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("Task-retention daemon shutting down");
                break;
            }
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;

    async fn setup(pool: &PgPool) {
        underway::run_migrations(pool).await.unwrap();
        crate::migrations::apply_underway(pool).await.unwrap();
        sqlx::raw_sql("INSERT INTO underway.task_queue(name) VALUES ('q') ON CONFLICT DO NOTHING")
            .execute(pool)
            .await
            .unwrap();
    }

    async fn insert(pool: &PgPool, state: &str, age_days: i64, ttl_days: i64) -> uuid::Uuid {
        let id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO underway.task(id, task_queue_name, input, state, created_at, ttl)
             VALUES ($1, 'q', '{}', $2::underway.task_state, now() - make_interval(days => $3), make_interval(days => $4))",
        )
        .bind(id)
        .bind(state)
        .bind(age_days as i32)
        .bind(ttl_days as i32)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    async fn remaining(pool: &PgPool) -> Vec<uuid::Uuid> {
        sqlx::query_scalar("SELECT id FROM underway.task ORDER BY created_at")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn purges_only_expired_finished_tasks(pool: PgPool) {
        setup(&pool).await;
        let old_done = insert(&pool, "succeeded", 30, 14).await;
        let old_failed = insert(&pool, "failed", 30, 14).await;
        let old_pending = insert(&pool, "pending", 30, 14).await;
        let old_running = insert(&pool, "in_progress", 30, 14).await;
        let long_ttl = insert(&pool, "succeeded", 30, 60).await;
        let fresh = insert(&pool, "succeeded", 1, 14).await;
        // Past the crate's default ttl but under the configured floor: kept.
        let under_floor = insert(&pool, "succeeded", 20, 14).await;

        let mut conn = pool.acquire().await.unwrap();
        let deleted = purge_expired_batch(&mut conn, 100, Duration::from_secs(21 * 86_400)).await.unwrap();
        assert_eq!(deleted, 3, "old succeeded, failed and pending rows expire");

        let left = remaining(&pool).await;
        for id in [old_done, old_failed, old_pending] {
            assert!(!left.contains(&id));
        }
        for id in [old_running, long_ttl, fresh, under_floor] {
            assert!(left.contains(&id));
        }
    }

    #[sqlx::test]
    async fn purge_is_bounded_by_batch_size_and_oldest_first(pool: PgPool) {
        setup(&pool).await;
        let oldest = insert(&pool, "succeeded", 40, 14).await;
        let middle = insert(&pool, "succeeded", 30, 14).await;
        let newest = insert(&pool, "succeeded", 20, 14).await;

        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(purge_expired_batch(&mut conn, 2, Duration::ZERO).await.unwrap(), 2);
        assert_eq!(remaining(&pool).await, vec![newest]);
        assert_eq!(purge_expired_batch(&mut conn, 2, Duration::ZERO).await.unwrap(), 1);
        assert!(remaining(&pool).await.is_empty());
        let _ = (oldest, middle);
    }

    #[sqlx::test]
    async fn purge_cascades_attempt_history(pool: PgPool) {
        setup(&pool).await;
        let id = insert(&pool, "succeeded", 30, 14).await;
        sqlx::query("INSERT INTO underway.task_attempt(task_id, task_queue_name, attempt_number, state) VALUES ($1, 'q', 1, 'succeeded')")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();

        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(purge_expired_batch(&mut conn, 10, Duration::ZERO).await.unwrap(), 1);
        let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM underway.task_attempt")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(attempts, 0);
    }

    #[sqlx::test]
    async fn purge_uses_the_retention_index(pool: PgPool) {
        setup(&pool).await;
        sqlx::raw_sql(
            "INSERT INTO underway.task(id, task_queue_name, input, state, created_at)
             SELECT gen_random_uuid(), 'q', '{}', 'succeeded', now() - interval '30 days' + (n * interval '1 second')
             FROM generate_series(1, 5000) n;
             ANALYZE underway.task;",
        )
        .execute(&pool)
        .await
        .unwrap();
        let plan: serde_json::Value = sqlx::query_scalar(
            "EXPLAIN (FORMAT JSON)
             WITH victims AS (
                 SELECT task_queue_name, id FROM underway.task
                 WHERE state <> 'in_progress' AND created_at < now() - interval '14 days' AND created_at + ttl < now()
                 ORDER BY created_at LIMIT 1000 FOR UPDATE SKIP LOCKED
             )
             DELETE FROM underway.task t USING victims v WHERE t.task_queue_name = v.task_queue_name AND t.id = v.id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let rendered = plan.to_string();
        // The victim selection is what the index exists for: it must walk
        // idx_task_created_at and never read the whole task history. The join
        // that applies the delete is left to the planner; on a table this small
        // it legitimately prefers a hash join over a thousand primary-key probes.
        fn find_cte<'a>(node: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
            if node["Subplan Name"] == format!("CTE {name}") {
                return Some(node);
            }
            node["Plans"].as_array()?.iter().find_map(|child| find_cte(child, name))
        }
        fn seq_scans_task(node: &serde_json::Value) -> bool {
            (node["Node Type"] == "Seq Scan" && node["Relation Name"] == "task")
                || node["Plans"].as_array().is_some_and(|plans| plans.iter().any(seq_scans_task))
        }
        let victims = find_cte(&plan[0]["Plan"], "victims").unwrap_or_else(|| panic!("{rendered}"));
        assert!(victims.to_string().contains("idx_task_created_at"), "{rendered}");
        assert!(!seq_scans_task(victims), "{rendered}");
    }

    #[sqlx::test]
    async fn sweep_drains_in_batches_and_reports_the_total(pool: PgPool) {
        setup(&pool).await;
        for _ in 0..7 {
            insert(&pool, "succeeded", 30, 14).await;
        }
        insert(&pool, "in_progress", 30, 14).await;
        let config = TaskRetentionConfig {
            enabled: true,
            interval_seconds: 60,
            batch_size: 3,
            batch_pause_milliseconds: 0,
            min_age_days: 14,
        };
        let deleted = sweep(&pool, &config, &CancellationToken::new()).await.unwrap();
        assert_eq!(deleted, Some(7));
        assert_eq!(remaining(&pool).await.len(), 1);
    }

    #[sqlx::test]
    async fn sweep_yields_when_another_sweep_holds_the_lock(pool: PgPool) {
        setup(&pool).await;
        insert(&pool, "succeeded", 30, 14).await;
        let mut holder = pool.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(SWEEP_LOCK_KEY)
            .execute(&mut *holder)
            .await
            .unwrap();
        let config = TaskRetentionConfig::default();
        assert_eq!(sweep(&pool, &config, &CancellationToken::new()).await.unwrap(), None);
        assert_eq!(remaining(&pool).await.len(), 1);
        holder.rollback().await.unwrap();
    }
}
