//! Bounded statistics maintenance for a shrinking active request table.

use std::time::Duration;

use anyhow::anyhow;
use chrono::{DateTime, Utc};
use metrics::counter;
use sqlx::Row;
use tracing::warn;

use super::{PoolProvider, PostgresRequestManager};
use crate::error::{FusilladeError, Result};

/// A failed check stops this archive pass; the daemon's existing retry loop
/// retries later. Foreground reads/writes never invoke maintenance.
pub(super) async fn before_archive<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
) -> Result<()> {
    // Another worker normally finishes inside the analyze statement budget.
    // Wait without holding a connection instead of reporting healthy overlap
    // as an archive failure. This also bounds pool acquisition and cancellation.
    let result = tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            if refresh_request_statistics(manager).await? {
                return Ok::<(), FusilladeError>(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await;
    match result {
        Ok(Ok(())) => Ok(()),
        Err(_) => {
            counter!("fusillade_request_statistics_maintenance_total", "outcome" => "deferred")
                .increment(1);
            Err(FusilladeError::Other(anyhow!(
                "Request statistics maintenance is pending; archive pass deferred"
            )))
        }
        Ok(Err(error)) => {
            counter!("fusillade_request_statistics_maintenance_total", "outcome" => "error")
                .increment(1);
            warn!("Request statistics maintenance failed; archive pass deferred");
            Err(error)
        }
    }
}

fn database_error(error: sqlx::Error) -> FusilladeError {
    FusilladeError::Other(anyhow!("Request statistics maintenance failed: {error}"))
}

async fn refresh_request_statistics<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
) -> Result<bool> {
    let mut tx = manager.begin_write().await.map_err(database_error)?;
    sqlx::query("SET LOCAL lock_timeout = '250ms'")
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    sqlx::query("SET LOCAL statement_timeout = '2s'")
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    // A plain conditional UPDATE can lock a concurrently changed row even
    // after rechecking its predicate and deciding not to update it. That losing
    // claimant could then block the owner's maintenance transaction. Skip
    // busy candidates instead of queueing claimants behind their owner.
    let claimed: Option<DateTime<Utc>> = sqlx::query_scalar(
        "WITH candidate AS MATERIALIZED ( \
             SELECT singleton FROM request_statistics_maintenance \
             WHERE singleton AND attempted_at < clock_timestamp() - interval '1 minute' \
             FOR UPDATE SKIP LOCKED \
         ) \
         UPDATE request_statistics_maintenance maintenance \
         SET attempted_at = clock_timestamp(), completed_at = NULL \
         FROM candidate WHERE maintenance.singleton = candidate.singleton \
         RETURNING maintenance.attempted_at",
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(database_error)?;
    let Some(attempt) = claimed else {
        let complete: bool = sqlx::query_scalar(
            "SELECT completed_at IS NOT NULL FROM request_statistics_maintenance WHERE singleton",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(database_error)?;
        tx.commit().await.map_err(database_error)?;
        return Ok(complete);
    };
    // Commit the cooldown before doing work: errors or cancellation must not
    // cause every pod to retry an expensive operation immediately.
    tx.commit().await.map_err(database_error)?;

    let mut tx = manager.begin_write().await.map_err(database_error)?;
    sqlx::query("SET LOCAL lock_timeout = '250ms'")
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    sqlx::query("SET LOCAL statement_timeout = '20s'")
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    // A delayed connection acquisition can outlive the cooldown. Recheck the
    // attempt under a row lock so an expired worker cannot overlap its successor.
    // Allow the bounded lock wait here: a claimant rechecking a just-committed
    // row can briefly hold its lock even when it ultimately claims nothing.
    let still_owner: Option<bool> = sqlx::query_scalar(
        "SELECT singleton FROM request_statistics_maintenance WHERE singleton AND attempted_at = $1 FOR UPDATE",
    ).bind(attempt).fetch_optional(&mut *tx).await.map_err(database_error)?;
    if still_owner.is_none() {
        tx.rollback().await.map_err(database_error)?;
        return Ok(false);
    }
    let stats = sqlx::query(
        "SELECT n_mod_since_analyze >= 1000 OR \
            (n_mod_since_analyze > 0 AND \
             COALESCE(GREATEST(last_analyze, last_autoanalyze), '-infinity') < now() - interval '5 minutes') AS due \
         FROM pg_stat_user_tables WHERE relid = 'requests'::regclass"
    ).fetch_one(&mut *tx).await.map_err(database_error)?;
    if stats.get::<bool, _>("due") {
        // PostgreSQL can merely warn and skip ANALYZE for a non-owner. Fail
        // visibly rather than marking that skipped operation as successful.
        let owns_table: bool = sqlx::query_scalar(
            "SELECT pg_has_role(relowner, 'USAGE') FROM pg_class WHERE oid = 'requests'::regclass",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(database_error)?;
        if !owns_table {
            return Err(FusilladeError::Other(anyhow!(
                "Request statistics maintenance requires the requests table owner role"
            )));
        }
        // Never queue a conflicting lock behind autovacuum (which can cause
        // PostgreSQL to cancel it). NOWAIT also closes the race between checking
        // progress views and a vacuum starting, without extra monitoring grants.
        sqlx::query("LOCK TABLE requests IN SHARE UPDATE EXCLUSIVE MODE NOWAIT")
            .execute(&mut *tx)
            .await
            .map_err(database_error)?;
        sqlx::query("ANALYZE requests")
            .execute(&mut *tx)
            .await
            .map_err(database_error)?;
    }
    sqlx::query("UPDATE request_statistics_maintenance SET completed_at = clock_timestamp() WHERE singleton AND attempted_at = $1")
        .bind(attempt).execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)?;
    if stats.get::<bool, _>("due") {
        counter!("fusillade_request_statistics_maintenance_total", "outcome" => "analyzed")
            .increment(1);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PostgresStorageConfig, TestDbPools};

    #[sqlx::test]
    async fn maintenance_claim_skips_another_workers_row_lock(pool: sqlx::PgPool) {
        let manager = PostgresRequestManager::new(
            TestDbPools::new(pool.clone()).await.unwrap(),
            PostgresStorageConfig::default(),
        );
        let mut holder = pool.begin().await.unwrap();
        sqlx::query("SELECT singleton FROM request_statistics_maintenance FOR UPDATE")
            .execute(&mut *holder)
            .await
            .unwrap();
        let ready =
            tokio::time::timeout(Duration::from_secs(1), refresh_request_statistics(&manager))
                .await
                .expect("a competing claim must not wait for the owner")
                .expect("a competing claim is normal coordination, not an error");
        assert!(!ready);
        holder.rollback().await.unwrap();
        assert!(refresh_request_statistics(&manager).await.unwrap());
    }

    #[sqlx::test]
    async fn maintenance_uses_transaction_schema_without_leaking_it(pool: sqlx::PgPool) {
        sqlx::raw_sql("CREATE SCHEMA maintenance_test; CREATE TABLE maintenance_test.requests (LIKE public.requests INCLUDING ALL); CREATE TABLE maintenance_test.request_statistics_maintenance (LIKE public.request_statistics_maintenance INCLUDING ALL); INSERT INTO maintenance_test.request_statistics_maintenance (singleton) VALUES (true);")
            .execute(&pool).await.unwrap();
        let manager = PostgresRequestManager::new(
            TestDbPools::new(pool.clone()).await.unwrap(),
            PostgresStorageConfig::default(),
        )
        .with_query_schema("maintenance_test");
        assert!(refresh_request_statistics(&manager).await.unwrap());
        let (public_untouched, scoped_complete): (bool, bool) = sqlx::query_as(
            "SELECT (SELECT attempted_at = '-infinity' FROM public.request_statistics_maintenance), (SELECT completed_at IS NOT NULL FROM maintenance_test.request_statistics_maintenance)",
        ).fetch_one(&pool).await.unwrap();
        assert!(public_untouched && scoped_complete);
        let schema: String = sqlx::query_scalar("SELECT current_schema()")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            schema, "public",
            "transaction schema must not leak into subsequent pool users"
        );
    }

    #[sqlx::test]
    async fn maintenance_retries_blocked_analyze_after_mass_deletion(pool: sqlx::PgPool) {
        let manager = PostgresRequestManager::new(
            TestDbPools::new(pool.clone()).await.unwrap(),
            PostgresStorageConfig::default(),
        );
        let mut writer = pool.acquire().await.unwrap();
        sqlx::query("ALTER TABLE requests SET (autovacuum_enabled = false)")
            .execute(&mut *writer)
            .await
            .unwrap();
        sqlx::query("INSERT INTO requests (model, created_by) SELECT 'test', 'owner' FROM generate_series(1, 20000)")
            .execute(&mut *writer)
            .await
            .unwrap();
        sqlx::query("ANALYZE requests")
            .execute(&mut *writer)
            .await
            .unwrap();
        sqlx::query("DELETE FROM requests WHERE id NOT IN (SELECT id FROM requests LIMIT 100)")
            .execute(&mut *writer)
            .await
            .unwrap();
        sqlx::query("SELECT pg_stat_force_next_flush()")
            .execute(&mut *writer)
            .await
            .unwrap();
        drop(writer);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let changed: i64 = sqlx::query_scalar("SELECT n_mod_since_analyze FROM pg_stat_user_tables WHERE relid = 'requests'::regclass")
                    .fetch_one(&pool).await.unwrap();
                if changed >= 1000 { break; }
                tokio::task::yield_now().await;
            }
        }).await.expect("committed deletion statistics must become visible");
        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE requests IN SHARE UPDATE EXCLUSIVE MODE")
            .execute(&mut *blocker)
            .await
            .unwrap();
        assert!(refresh_request_statistics(&manager).await.is_err());
        blocker.rollback().await.unwrap();
        assert!(
            !refresh_request_statistics(&manager).await.unwrap(),
            "failed attempts retain their cooldown"
        );
        sqlx::query(
            "UPDATE request_statistics_maintenance SET attempted_at = now() - interval '2 minutes'",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(refresh_request_statistics(&manager).await.unwrap());
        let estimated: f32 =
            sqlx::query_scalar("SELECT reltuples FROM pg_class WHERE oid = 'requests'::regclass")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            estimated, 100.0,
            "the retry must refresh the post-movement estimate"
        );
    }

    #[sqlx::test]
    async fn maintenance_is_coordinated_and_recovers_after_an_abandoned_attempt(
        pool: sqlx::PgPool,
    ) {
        let manager = PostgresRequestManager::new(
            TestDbPools::new(pool.clone()).await.unwrap(),
            PostgresStorageConfig::default(),
        );
        assert!(refresh_request_statistics(&manager).await.unwrap());
        let first: chrono::DateTime<chrono::Utc> =
            sqlx::query_scalar("SELECT attempted_at FROM request_statistics_maintenance")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(refresh_request_statistics(&manager).await.unwrap());
        let second: chrono::DateTime<chrono::Utc> =
            sqlx::query_scalar("SELECT attempted_at FROM request_statistics_maintenance")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(first, second, "successful passes must respect the cooldown");
        sqlx::query("UPDATE request_statistics_maintenance SET completed_at = NULL")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            !refresh_request_statistics(&manager).await.unwrap(),
            "another worker's active or failed attempt pauses movement"
        );
        sqlx::query(
            "UPDATE request_statistics_maintenance SET attempted_at = now() - interval '2 minutes'",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            refresh_request_statistics(&manager).await.unwrap(),
            "expired attempts must be retried without an operator"
        );
    }
}
