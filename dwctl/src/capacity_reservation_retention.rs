//! Batch capacity reservation retention.
//!
//! `POST /batches` inserts one `batch_capacity_reservations` row per model to hold
//! admission capacity while the batch is created, and releases it (`released_at = now()`)
//! when the submission finishes. Admission only reads rows that are unexpired and either
//! unreleased or released since its pending-request snapshot, a few moments earlier. Every
//! other row is history nothing reads, but nothing deleted it either, so the ledger kept
//! every reservation ever made.
//!
//! This daemon deletes rows whose `coalesce(released_at, expires_at)` is more than the
//! configured retention in the past: released rows once the retention has passed since
//! release, and rows that were never released once it has passed since they expired. The
//! retention is never shorter than `batches.reservation_ttl_secs` (enforced when the
//! configuration loads), so a row is gone only long after any admission check could count
//! it.
//!
//! Each batch finds its victims with a sequential scan and deletes them by `ctid`, so the
//! delete is a TID scan and never re-finds each row through the primary key; the delete
//! rechecks the age predicate, so a row whose version moved in between is left alone. A
//! sweep deletes at most `max_rows_per_sweep` rows, so a large existing ledger drains over
//! several sweeps.
//!
//! Every pod runs the daemon. A sweep holds a session-level advisory lock on a dedicated
//! connection for its whole duration, so concurrent sweeps collapse to one while each
//! delete batch still commits on its own.

use std::time::Duration;

use metrics::counter;
use sqlx::{Connection, PgConnection, PgPool};
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument};

use crate::config::CapacityReservationRetentionConfig;
use crate::metrics::errors::component::CAPACITY_RESERVATION_RETENTION;

/// Advisory lock key shared by every sweep ("DWCRRETN").
const SWEEP_LOCK_KEY: i64 = 0x4457_4352_5245_544e;

/// Rows deleted by the daemon, across all sweeps on this instance.
const DELETED_ROWS_METRIC: &str = "dwctl_capacity_reservation_retention_deleted_rows_total";

/// Delete up to `limit` reservations released or expired more than `retention` ago, in one
/// transaction. Returns the number of rows deleted.
#[instrument(skip(pool), fields(limit, retention_s = retention.as_secs()))]
pub async fn purge_batch(pool: &PgPool, limit: i64, retention: Duration) -> Result<u64, sqlx::Error> {
    let mut connection = pool.acquire().await?;
    purge_batch_on_connection(&mut connection, limit, retention).await
}

async fn purge_batch_on_connection(connection: &mut PgConnection, limit: i64, retention: Duration) -> Result<u64, sqlx::Error> {
    let mut tx = connection.begin().await?;
    // Bound lock waits and total batch execution independently. SET LOCAL keeps these
    // limits out of subsequent users of the pooled connection.
    sqlx::query("SET LOCAL lock_timeout = '2s'").execute(&mut *tx).await?;
    sqlx::query("SET LOCAL statement_timeout = '30s'").execute(&mut *tx).await?;
    let result = sqlx::query(
        r#"
        DELETE FROM batch_capacity_reservations
        WHERE ctid = ANY(ARRAY(
                SELECT ctid
                FROM batch_capacity_reservations
                WHERE coalesce(released_at, expires_at) < now() - make_interval(secs => $2)
                LIMIT $1
            ))
          AND coalesce(released_at, expires_at) < now() - make_interval(secs => $2)
        "#,
    )
    .bind(limit)
    .bind(retention.as_secs_f64())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected())
}

/// The outcome of one sweep that held the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SweepOutcome {
    deleted: u64,
    /// The sweep stopped at `max_rows_per_sweep` with rows possibly left to delete.
    capped: bool,
}

/// One sweep: repeat batches until a batch comes back short or the per-sweep cap is
/// reached, pausing between batches.
///
/// Returns `None` when another sweep held the lock. The lock is session-level on a
/// detached connection that is unlocked and closed on every exit path, so it can neither
/// leak back into the pool nor outlive the sweep.
async fn sweep(
    pool: &PgPool,
    config: &CapacityReservationRetentionConfig,
    shutdown: &CancellationToken,
) -> Result<Option<SweepOutcome>, sqlx::Error> {
    let mut guard = pool.acquire().await?.detach();
    let locked: bool = match sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(SWEEP_LOCK_KEY)
        .fetch_one(&mut guard)
        .await
    {
        Ok(locked) => locked,
        Err(e) => {
            let _ = guard.close().await;
            return Err(e);
        }
    };
    if !locked {
        let _ = guard.close().await;
        return Ok(None);
    }
    let result = sweep_batches(&mut guard, config, shutdown).await;
    // Unlock explicitly so the lock is free when this returns. Closing the session would
    // also release it, but only once the backend has exited, after `close` has returned;
    // the close remains the fallback when the connection is already broken.
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(SWEEP_LOCK_KEY)
        .execute(&mut guard)
        .await;
    let _ = guard.close().await;
    result.map(Some)
}

async fn sweep_batches(
    connection: &mut PgConnection,
    config: &CapacityReservationRetentionConfig,
    shutdown: &CancellationToken,
) -> Result<SweepOutcome, sqlx::Error> {
    let batch_size = u64::from(config.batch_size.max(1));
    let max_rows = config.max_rows_per_sweep.max(1);
    let retention = config.retention();
    let pause = Duration::from_millis(config.batch_pause_milliseconds);

    let mut deleted = 0u64;
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        let limit = batch_size.min(max_rows - deleted);
        let batch = purge_batch_on_connection(connection, limit as i64, retention).await?;
        counter!(DELETED_ROWS_METRIC).increment(batch);
        deleted += batch;
        if batch < limit {
            return Ok(SweepOutcome { deleted, capped: false });
        }
        if deleted >= max_rows {
            return Ok(SweepOutcome { deleted, capped: true });
        }
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(pause) => {}
        }
    }
    Ok(SweepOutcome { deleted, capped: false })
}

/// Run the retention daemon until `shutdown` is cancelled.
/// The provider must use direct connections: the sweep lock is session-scoped.
#[instrument(skip_all)]
pub async fn run_capacity_reservation_retention_daemon(
    pools: impl sqlx_pool_router::PoolProvider,
    config: CapacityReservationRetentionConfig,
    shutdown: CancellationToken,
) {
    let pools = sqlx_pool_router::DynPools::new(pools);
    let interval = Duration::from_secs(config.interval_seconds.max(1));

    info!(
        interval_s = interval.as_secs(),
        retention_s = config.retention_seconds,
        batch_size = config.batch_size,
        batch_pause_ms = config.batch_pause_milliseconds,
        max_rows_per_sweep = config.max_rows_per_sweep,
        "Capacity-reservation retention daemon started"
    );

    loop {
        match sweep(&pools.write(), &config, &shutdown).await {
            Ok(Some(SweepOutcome { deleted, capped: true })) => info!(
                deleted,
                "Capacity-reservation retention sweep reached max_rows_per_sweep; the rest is left to later sweeps"
            ),
            Ok(Some(SweepOutcome { deleted, .. })) if deleted > 0 => {
                info!(deleted, "Capacity-reservation retention sweep deleted old reservations")
            }
            Ok(Some(_)) => {}
            Ok(None) => tracing::debug!("Capacity-reservation retention sweep skipped: another instance holds the lock"),
            Err(e) => {
                crate::background_error!(
                    CAPACITY_RESERVATION_RETENTION,
                    "sweep_failed",
                    Error,
                    error = %e,
                    "Capacity-reservation retention sweep failed"
                );
            }
        }

        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("Capacity-reservation retention daemon shutting down");
                break;
            }
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::users::Role;
    use crate::test::utils::{create_test_endpoint, create_test_model, create_test_user};
    use uuid::Uuid;

    const HOUR: Duration = Duration::from_secs(3600);

    async fn setup_model(pool: &PgPool) -> Uuid {
        let user = create_test_user(pool, Role::StandardUser).await;
        let endpoint_id = create_test_endpoint(pool, &format!("test-{}", Uuid::new_v4()), user.id).await;
        create_test_model(pool, "model-a", &format!("alias-{}", Uuid::new_v4()), endpoint_id, user.id).await
    }

    /// Insert a reservation created `created_ago` seconds ago that expires `expires_ago`
    /// seconds ago (negative = in the future) and, when set, was released `released_ago`
    /// seconds ago.
    async fn insert(pool: &PgPool, model_id: Uuid, created_ago: i64, expires_ago: i64, released_ago: Option<i64>) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO batch_capacity_reservations
               (model_id, completion_window, reserved_requests, created_at, expires_at, released_at)
             VALUES ($1, '24h', 1,
                     now() - make_interval(secs => $2),
                     now() - make_interval(secs => $3),
                     now() - make_interval(secs => $4))
             RETURNING id",
        )
        .bind(model_id)
        .bind(created_ago as f64)
        .bind(expires_ago as f64)
        .bind(released_ago.map(|s| s as f64))
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn count(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM batch_capacity_reservations")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn remaining(pool: &PgPool) -> Vec<Uuid> {
        sqlx::query_scalar("SELECT id FROM batch_capacity_reservations")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    /// Insert `n` reservations released two hours ago.
    async fn insert_old_released(pool: &PgPool, model_id: Uuid, n: i64) {
        sqlx::query(
            "INSERT INTO batch_capacity_reservations
               (model_id, completion_window, reserved_requests, created_at, expires_at, released_at)
             SELECT $1, '24h', 1, now() - interval '3 hours', now() - interval '3 hours' + interval '10 minutes',
                    now() - interval '2 hours'
             FROM generate_series(1, $2)",
        )
        .bind(model_id)
        .bind(n)
        .execute(pool)
        .await
        .unwrap();
    }

    fn config(batch_size: u32, max_rows_per_sweep: u64) -> CapacityReservationRetentionConfig {
        CapacityReservationRetentionConfig {
            batch_size,
            max_rows_per_sweep,
            batch_pause_milliseconds: 0,
            ..CapacityReservationRetentionConfig::default()
        }
    }

    #[dwctl_test_macros::test]
    async fn purges_only_reservations_past_the_retention(pool: PgPool) {
        let model = setup_model(&pool).await;
        // Released two hours ago: past the one-hour retention.
        let old_released = insert(&pool, model, 3 * 3600, 3 * 3600 - 600, Some(2 * 3600)).await;
        // Never released, expired two hours ago.
        let old_expired = insert(&pool, model, 3 * 3600, 2 * 3600, None).await;
        // Created long ago but released late, within the retention: kept by `released_at`.
        let late_release = insert(&pool, model, 3 * 3600, 2 * 3600, Some(30 * 60)).await;
        // Live: unreleased and not yet expired.
        let live = insert(&pool, model, 5, -595, None).await;
        // Released moments ago.
        let recently_released = insert(&pool, model, 20, -580, Some(10)).await;
        // Never released, expired within the retention.
        let recently_expired = insert(&pool, model, 30 * 60, 20 * 60, None).await;

        assert_eq!(purge_batch(&pool, 100, HOUR).await.unwrap(), 2);
        let left = remaining(&pool).await;
        for id in [old_released, old_expired] {
            assert!(!left.contains(&id), "{id} should be deleted");
        }
        for id in [late_release, live, recently_released, recently_expired] {
            assert!(left.contains(&id), "{id} should be kept");
        }
        assert_eq!(purge_batch(&pool, 100, HOUR).await.unwrap(), 0);
    }

    #[dwctl_test_macros::test]
    async fn purge_is_bounded_by_its_limit(pool: PgPool) {
        let model = setup_model(&pool).await;
        insert_old_released(&pool, model, 5).await;

        assert_eq!(purge_batch(&pool, 2, HOUR).await.unwrap(), 2);
        assert_eq!(count(&pool).await, 3);
        assert_eq!(purge_batch(&pool, 10, HOUR).await.unwrap(), 3);
        assert_eq!(count(&pool).await, 0);
    }

    #[dwctl_test_macros::test]
    async fn purge_times_out_on_table_locks_without_deleting(pool: PgPool) {
        let model = setup_model(&pool).await;
        insert_old_released(&pool, model, 1).await;
        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE batch_capacity_reservations IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *blocker)
            .await
            .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(10), purge_batch(&pool, 100, HOUR))
            .await
            .expect("batch must not wait indefinitely")
            .unwrap_err();
        assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("55P03"));
        blocker.rollback().await.unwrap();
        assert_eq!(count(&pool).await, 1);
        assert_eq!(purge_batch(&pool, 100, HOUR).await.unwrap(), 1);
    }

    #[dwctl_test_macros::test]
    async fn purge_deletes_by_tid_without_primary_key_lookups(pool: PgPool) {
        let model = setup_model(&pool).await;
        insert_old_released(&pool, model, 2000).await;
        sqlx::query("ANALYZE batch_capacity_reservations").execute(&pool).await.unwrap();
        let plan: serde_json::Value = sqlx::query_scalar(
            "EXPLAIN (FORMAT JSON)
             DELETE FROM batch_capacity_reservations
             WHERE ctid = ANY(ARRAY(
                     SELECT ctid FROM batch_capacity_reservations
                     WHERE coalesce(released_at, expires_at) < now() - make_interval(secs => 3600)
                     LIMIT 1000
                 ))
               AND coalesce(released_at, expires_at) < now() - make_interval(secs => 3600)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let rendered = plan.to_string();
        // The delete must target the rows found by the subquery directly; an index scan
        // here means each victim is re-found through an index, one random read per row.
        fn node_types(node: &serde_json::Value, out: &mut Vec<String>) {
            if let Some(node_type) = node["Node Type"].as_str() {
                out.push(node_type.to_string());
            }
            for child in node["Plans"].as_array().into_iter().flatten() {
                node_types(child, out);
            }
        }
        let delete = &plan[0]["Plan"];
        assert_eq!(delete["Node Type"], "ModifyTable", "{rendered}");
        let mut types = Vec::new();
        node_types(delete, &mut types);
        assert!(types.iter().any(|t| t == "Tid Scan"), "{rendered}");
        assert!(!types.iter().any(|t| t.contains("Index")), "{rendered}");
    }

    #[dwctl_test_macros::test]
    async fn sweep_drains_in_batches_and_reports_the_total(pool: PgPool) {
        let model = setup_model(&pool).await;
        insert_old_released(&pool, model, 7).await;
        let live = insert(&pool, model, 5, -595, None).await;

        let outcome = sweep(&pool, &config(3, 100), &CancellationToken::new()).await.unwrap();
        assert_eq!(outcome, Some(SweepOutcome { deleted: 7, capped: false }));
        assert_eq!(remaining(&pool).await, vec![live]);
    }

    #[dwctl_test_macros::test]
    async fn sweep_stops_at_the_per_sweep_cap(pool: PgPool) {
        let model = setup_model(&pool).await;
        insert_old_released(&pool, model, 10).await;

        // A cap that is not a multiple of the batch size: the last batch is shortened.
        let config = config(3, 7);
        let first = sweep(&pool, &config, &CancellationToken::new()).await.unwrap();
        assert_eq!(first, Some(SweepOutcome { deleted: 7, capped: true }));
        assert_eq!(count(&pool).await, 3);

        let second = sweep(&pool, &config, &CancellationToken::new()).await.unwrap();
        assert_eq!(second, Some(SweepOutcome { deleted: 3, capped: false }));
        assert_eq!(count(&pool).await, 0);
    }

    #[dwctl_test_macros::test]
    async fn sweep_cap_equal_to_the_backlog_reports_capped(pool: PgPool) {
        let model = setup_model(&pool).await;
        insert_old_released(&pool, model, 6).await;

        // The sweep cannot tell an exactly drained backlog from a capped one; the next sweep
        // finds nothing.
        let config = config(3, 6);
        let first = sweep(&pool, &config, &CancellationToken::new()).await.unwrap();
        assert_eq!(first, Some(SweepOutcome { deleted: 6, capped: true }));
        let second = sweep(&pool, &config, &CancellationToken::new()).await.unwrap();
        assert_eq!(second, Some(SweepOutcome { deleted: 0, capped: false }));
    }

    #[dwctl_test_macros::test]
    async fn sweep_yields_when_another_sweep_holds_the_lock(pool: PgPool) {
        let model = setup_model(&pool).await;
        insert_old_released(&pool, model, 1).await;
        let mut holder = pool.acquire().await.unwrap().detach();
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(SWEEP_LOCK_KEY)
            .execute(&mut holder)
            .await
            .unwrap();
        let config = config(10, 100);
        assert_eq!(sweep(&pool, &config, &CancellationToken::new()).await.unwrap(), None);
        assert_eq!(count(&pool).await, 1);
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(SWEEP_LOCK_KEY)
            .execute(&mut holder)
            .await
            .unwrap();
        holder.close().await.unwrap();
        // With the lock released the sweep proceeds, and it releases its own lock when
        // done: a second sweep is not refused.
        assert_eq!(
            sweep(&pool, &config, &CancellationToken::new()).await.unwrap(),
            Some(SweepOutcome { deleted: 1, capped: false })
        );
        assert_eq!(
            sweep(&pool, &config, &CancellationToken::new()).await.unwrap(),
            Some(SweepOutcome { deleted: 0, capped: false })
        );
    }

    #[dwctl_test_macros::test]
    async fn sweep_stops_on_shutdown(pool: PgPool) {
        let model = setup_model(&pool).await;
        insert_old_released(&pool, model, 5).await;
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let outcome = sweep(&pool, &config(1, 100), &shutdown).await.unwrap();
        assert_eq!(outcome, Some(SweepOutcome { deleted: 0, capped: false }));
        assert_eq!(count(&pool).await, 5);
    }
}
