//! Prompt-cache entry retention.
//!
//! Lookups only match live entries (`expires_at > now()`), and a write for an expired
//! prefix upserts the same row, so an entry that has expired serves no request. Nothing
//! deleted them, though, so `prompt_cache_entries` kept every prefix ever written.
//! This daemon deletes entries that expired more than a grace period ago, oldest first, in
//! bounded batches.
//!
//! The grace exists for usage recompute: it re-derives a past request's cache split by
//! asking whether an entry covering each prefix was live at the request's timestamp (see
//! [`crate::recompute::report::CACHE_GRACE_DAYS`]). The configured grace may be longer than
//! that, never shorter.
//!
//! Every pod runs the daemon. A sweep holds a session-level advisory lock on a dedicated
//! connection for its whole duration, so concurrent sweeps collapse to one while each
//! delete batch still commits on its own. `SKIP LOCKED` keeps the sweep off rows a request
//! is refreshing or re-writing at that moment. A request that reaches a row after the sweep
//! locked it waits for the batch to commit; a write then inserts the entry afresh.

use std::time::Duration;

use sqlx::{Connection, PgConnection, PgPool};
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument};

use crate::config::PromptCacheRetentionConfig;
use crate::metrics::errors::component::PROMPT_CACHE_RETENTION;

/// Advisory lock key shared by every sweep ("DWPCRETE").
const SWEEP_LOCK_KEY: i64 = 0x4457_5043_5245_5445;

/// Delete one batch of entries that expired more than `grace` ago, in its own transaction.
/// Returns the number of rows deleted.
#[instrument(skip(pool), fields(batch_size, grace_days = grace.as_secs() / 86_400))]
pub async fn purge_expired_batch(pool: &PgPool, batch_size: i64, grace: Duration) -> Result<u64, sqlx::Error> {
    let mut connection = pool.acquire().await?;
    purge_expired_batch_on_connection(&mut connection, batch_size, grace).await
}

async fn purge_expired_batch_on_connection(connection: &mut PgConnection, batch_size: i64, grace: Duration) -> Result<u64, sqlx::Error> {
    let mut tx = connection.begin().await?;
    // Bound lock waits and total batch execution independently. SET LOCAL keeps these
    // limits out of subsequent users of the pooled connection.
    sqlx::query("SET LOCAL lock_timeout = '2s'").execute(&mut *tx).await?;
    sqlx::query("SET LOCAL statement_timeout = '30s'").execute(&mut *tx).await?;
    let result = sqlx::query(
        r#"
        WITH victims AS (
            SELECT id
            FROM prompt_cache_entries
            WHERE expires_at < now() - make_interval(secs => $2)
            ORDER BY expires_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        DELETE FROM prompt_cache_entries e
        USING victims v
        WHERE e.id = v.id
        "#,
    )
    .bind(batch_size)
    .bind(grace.as_secs_f64())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected())
}

/// One sweep: repeat batches until a batch comes back short, pausing between them.
///
/// Returns the total rows deleted, or `None` when another sweep held the lock. The lock is
/// session-level on a detached connection that is closed on every exit path, so it can
/// neither leak back into the pool nor outlive the sweep.
async fn sweep(pool: &PgPool, config: &PromptCacheRetentionConfig, shutdown: &CancellationToken) -> Result<Option<u64>, sqlx::Error> {
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
    // Closing the session releases the advisory lock whatever happened above.
    let _ = guard.close().await;
    result.map(Some)
}

async fn sweep_batches(
    connection: &mut PgConnection,
    config: &PromptCacheRetentionConfig,
    shutdown: &CancellationToken,
) -> Result<u64, sqlx::Error> {
    let batch_size = i64::from(config.batch_size.max(1));
    let grace = config.grace();
    let pause = Duration::from_millis(config.batch_pause_milliseconds);

    let mut total = 0u64;
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        let deleted = purge_expired_batch_on_connection(connection, batch_size, grace).await?;
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
    Ok(total)
}

/// Run the retention daemon until `shutdown` is cancelled.
/// The provider must use direct connections: the sweep lock is session-scoped.
#[instrument(skip_all)]
pub async fn run_prompt_cache_retention_daemon(
    pools: impl sqlx_pool_router::PoolProvider,
    config: PromptCacheRetentionConfig,
    shutdown: CancellationToken,
) {
    let pools = sqlx_pool_router::DynPools::new(pools);
    let interval = Duration::from_secs(config.interval_seconds.max(1));

    info!(
        interval_s = interval.as_secs(),
        batch_size = config.batch_size,
        batch_pause_ms = config.batch_pause_milliseconds,
        grace_days = config.grace_days,
        "Prompt-cache retention daemon started"
    );

    loop {
        match sweep(&pools.write(), &config, &shutdown).await {
            Ok(Some(deleted)) if deleted > 0 => info!(deleted, "Prompt-cache retention sweep deleted expired entries"),
            Ok(Some(_)) => {}
            Ok(None) => tracing::debug!("Prompt-cache retention sweep skipped: another instance holds the lock"),
            Err(e) => {
                crate::background_error!(
                    PROMPT_CACHE_RETENTION,
                    "sweep_failed",
                    Error,
                    error = %e,
                    "Prompt-cache retention sweep failed"
                );
            }
        }

        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("Prompt-cache retention daemon shutting down");
                break;
            }
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    /// Insert an entry whose expiry is `expired_seconds_ago` in the past (negative = live).
    async fn insert(pool: &PgPool, hash: &str, expired_seconds_ago: i64) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO prompt_cache_entries
               (principal_id, virtual_model, tokenizer_version, prefix_hash, cumulative_token_count, ttl_tier, expires_at)
             VALUES (gen_random_uuid(), 'model', 'tok', $1::bytea, 10, '1h', now() - make_interval(secs => $2))
             RETURNING id",
        )
        .bind(hash.as_bytes())
        .bind(expired_seconds_ago as f64)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn remaining(pool: &PgPool) -> Vec<i64> {
        sqlx::query_scalar("SELECT id FROM prompt_cache_entries ORDER BY expires_at")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[dwctl_test_macros::test]
    async fn purges_only_entries_expired_beyond_the_grace(pool: PgPool) {
        let long_expired = insert(&pool, "long-expired", 30 * DAY).await;
        let within_grace = insert(&pool, "within-grace", 3 * DAY).await;
        let live = insert(&pool, "live", -3600).await;

        let deleted = purge_expired_batch(&pool, 100, Duration::from_secs(7 * DAY as u64)).await.unwrap();
        assert_eq!(deleted, 1);
        let left = remaining(&pool).await;
        assert!(!left.contains(&long_expired));
        assert!(left.contains(&within_grace) && left.contains(&live));
    }

    #[dwctl_test_macros::test]
    async fn purge_is_bounded_by_batch_size_and_oldest_first(pool: PgPool) {
        let oldest = insert(&pool, "a", 40 * DAY).await;
        let middle = insert(&pool, "b", 30 * DAY).await;
        let newest = insert(&pool, "c", 20 * DAY).await;

        assert_eq!(purge_expired_batch(&pool, 2, Duration::ZERO).await.unwrap(), 2);
        assert_eq!(remaining(&pool).await, vec![newest]);
        assert_eq!(purge_expired_batch(&pool, 2, Duration::ZERO).await.unwrap(), 1);
        assert!(remaining(&pool).await.is_empty());
        let _ = (oldest, middle);
    }

    #[dwctl_test_macros::test]
    async fn purge_skips_an_entry_a_request_holds(pool: PgPool) {
        let held = insert(&pool, "held", 30 * DAY).await;
        let free = insert(&pool, "free", 30 * DAY).await;
        let mut request = pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM prompt_cache_entries WHERE id = $1 FOR UPDATE")
            .bind(held)
            .execute(&mut *request)
            .await
            .unwrap();

        assert_eq!(purge_expired_batch(&pool, 100, Duration::ZERO).await.unwrap(), 1);
        request.rollback().await.unwrap();
        assert_eq!(remaining(&pool).await, vec![held]);
        let _ = free;
    }

    #[dwctl_test_macros::test]
    async fn purge_walks_the_expiry_index(pool: PgPool) {
        sqlx::raw_sql(
            "INSERT INTO prompt_cache_entries
               (principal_id, virtual_model, tokenizer_version, prefix_hash, cumulative_token_count, ttl_tier, expires_at)
             SELECT gen_random_uuid(), 'model', 'tok', int4send(n), 10, '1h', now() + (n * interval '1 second')
             FROM generate_series(1, 20000) n;
             INSERT INTO prompt_cache_entries
               (principal_id, virtual_model, tokenizer_version, prefix_hash, cumulative_token_count, ttl_tier, expires_at)
             SELECT gen_random_uuid(), 'model', 'tok', int4send(-n), 10, '1h', now() - interval '30 days' + (n * interval '1 second')
             FROM generate_series(1, 10) n;
             ANALYZE prompt_cache_entries;",
        )
        .execute(&pool)
        .await
        .unwrap();
        let plan: serde_json::Value = sqlx::query_scalar(
            "EXPLAIN (FORMAT JSON)
             SELECT id FROM prompt_cache_entries
             WHERE expires_at < now() - make_interval(secs => 604800)
             ORDER BY expires_at LIMIT 1000 FOR UPDATE SKIP LOCKED",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        // Live entries must not be scanned to find the expired ones.
        assert!(plan.to_string().contains("idx_prompt_cache_entries_expires_at"), "{plan}");
        assert_eq!(
            purge_expired_batch(&pool, 1000, Duration::from_secs(7 * DAY as u64)).await.unwrap(),
            10
        );
    }

    #[dwctl_test_macros::test]
    async fn entries_autovacuum_on_a_fixed_row_count(pool: PgPool) {
        let options: Vec<String> =
            sqlx::query_scalar("SELECT unnest(reloptions) FROM pg_class WHERE oid = 'prompt_cache_entries'::regclass")
                .fetch_all(&pool)
                .await
                .unwrap();
        for expected in [
            "autovacuum_vacuum_scale_factor=0.0",
            "autovacuum_vacuum_threshold=1000000",
            "autovacuum_vacuum_insert_scale_factor=0.0",
            "autovacuum_vacuum_insert_threshold=1000000",
        ] {
            assert!(options.iter().any(|option| option == expected), "missing {expected}: {options:?}");
        }
    }

    #[dwctl_test_macros::test]
    async fn sweep_drains_in_batches_and_reports_the_total(pool: PgPool) {
        for n in 0..7 {
            insert(&pool, &format!("old-{n}"), 30 * DAY).await;
        }
        insert(&pool, "live", -3600).await;
        let config = PromptCacheRetentionConfig {
            batch_size: 3,
            batch_pause_milliseconds: 0,
            ..PromptCacheRetentionConfig::default()
        };
        let deleted = sweep(&pool, &config, &CancellationToken::new()).await.unwrap();
        assert_eq!(deleted, Some(7));
        assert_eq!(remaining(&pool).await.len(), 1);
    }

    #[dwctl_test_macros::test]
    async fn sweep_yields_when_another_sweep_holds_the_lock(pool: PgPool) {
        insert(&pool, "old", 30 * DAY).await;
        let mut holder = pool.acquire().await.unwrap().detach();
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(SWEEP_LOCK_KEY)
            .execute(&mut holder)
            .await
            .unwrap();
        let config = PromptCacheRetentionConfig::default();
        assert_eq!(sweep(&pool, &config, &CancellationToken::new()).await.unwrap(), None);
        assert_eq!(remaining(&pool).await.len(), 1);
        holder.close().await.unwrap();
        assert_eq!(sweep_once_the_lock_is_free(&pool, &config).await, Some(1));
        assert_eq!(sweep_once_the_lock_is_free(&pool, &config).await, Some(0));
    }

    async fn sweep_once_the_lock_is_free(pool: &PgPool, config: &PromptCacheRetentionConfig) -> Option<u64> {
        for _ in 0..200 {
            if let Some(deleted) = sweep(pool, config, &CancellationToken::new()).await.unwrap() {
                return Some(deleted);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        None
    }
}
