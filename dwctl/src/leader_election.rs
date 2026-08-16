//! Leader election using PostgreSQL advisory locks for multi-instance deployments.

use crate::config;
use crate::metrics::errors::component::LEADER_ELECTION;
use sqlx::PgPool;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument};

async fn release_leader_connection(leader_conn: &mut Option<sqlx::pool::PoolConnection<sqlx::Postgres>>, lock_id: i64) {
    let Some(mut conn) = leader_conn.take() else {
        return;
    };

    match sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock($1)")
        .bind(lock_id)
        .fetch_one(&mut *conn)
        .await
    {
        Ok(true) => debug!("Released leader advisory lock"),
        Ok(false) => {
            crate::background_error!(
                LEADER_ELECTION,
                "unlock_missing",
                Warning,
                "Leader connection no longer held the advisory lock"
            );
        }
        Err(e) => {
            crate::background_error!(LEADER_ELECTION, "unlock", Warning, "Failed to release leader advisory lock: {}", e);

            // Returning a connection with an uncertain session lock to the pool
            // could strand leadership. Closing it makes PostgreSQL release every
            // session-scoped advisory lock even when the explicit unlock failed.
            if let Err(close_error) = conn.close().await {
                crate::background_error!(
                    LEADER_ELECTION,
                    "unlock_close",
                    Warning,
                    "Failed to close leader connection after unlock error: {}",
                    close_error
                );
            }
            return;
        }
    }

    drop(conn);
}

/// Background task for leader election
/// Runs periodically to maintain leadership or attempt to acquire it
///
/// We use leadership election for figuring out who runs background tasks like sending probes to
/// the endpoints. At some point, we may want to expand this to other tasks as well.
///
/// PostgreSQL advisory locks are session-based, so we need to maintain a dedicated connection
/// for the entire duration we want to hold the lock.
#[instrument(skip(pool, config, lock_id, shutdown_token, on_gain_leadership, on_lose_leadership))]
pub async fn leader_election_task<F1, F2, Fut1, Fut2>(
    pool: PgPool,
    config: config::Config,
    is_leader: Arc<AtomicBool>,
    lock_id: i64,
    shutdown_token: CancellationToken,
    on_gain_leadership: F1,
    on_lose_leadership: F2,
) where
    F1: Fn(PgPool, config::Config) -> Fut1 + Send + 'static,
    F2: Fn(PgPool, config::Config) -> Fut2 + Send + 'static,
    Fut1: std::future::Future<Output = Result<(), anyhow::Error>> + Send + 'static,
    Fut2: std::future::Future<Output = Result<(), anyhow::Error>> + Send + 'static,
{
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    let mut leader_conn: Option<sqlx::pool::PoolConnection<sqlx::Postgres>> = None;

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = shutdown_token.cancelled() => {
                info!("Shutdown signal received, stopping leader election");
                // If we're currently the leader, call the lose leadership callback
                if is_leader.load(Ordering::Relaxed) {
                    is_leader.store(false, Ordering::Relaxed);
                    if let Err(e) = on_lose_leadership(pool.clone(), config.clone()).await {
                        crate::background_error!(LEADER_ELECTION, "lose_callback", Error, "Failed to execute on_lose_leadership callback during shutdown: {}", e);
                    }
                }
                release_leader_connection(&mut leader_conn, lock_id).await;
                break;
            }
        }

        let current_status = is_leader.load(Ordering::Relaxed);

        // If we're not leader, try to acquire the lock
        if !current_status {
            // Try to acquire a connection and the lock
            match pool.acquire().await {
                Ok(mut conn) => {
                    match sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
                        .bind(lock_id)
                        .fetch_one(&mut *conn)
                        .await
                    {
                        Ok(true) => {
                            // Successfully acquired lock!
                            info!("Gained leadership");
                            is_leader.store(true, Ordering::Relaxed);
                            leader_conn = Some(conn); // Keep connection alive

                            if let Err(e) = on_gain_leadership(pool.clone(), config.clone()).await {
                                crate::background_error!(
                                    LEADER_ELECTION,
                                    "gain_callback",
                                    Error,
                                    "Failed to execute on_gain_leadership callback: {}",
                                    e
                                );

                                is_leader.store(false, Ordering::Relaxed);
                                if let Err(e) = on_lose_leadership(pool.clone(), config.clone()).await {
                                    crate::background_error!(
                                        LEADER_ELECTION,
                                        "lose_callback",
                                        Error,
                                        "Failed to clean up after on_gain_leadership callback failure: {}",
                                        e
                                    );
                                }
                                release_leader_connection(&mut leader_conn, lock_id).await;
                            }
                        }
                        Ok(false) => {
                            // Someone else has the lock
                            debug!("Following - will retry");
                        }
                        Err(e) => {
                            crate::background_error!(LEADER_ELECTION, "lock_check", Warning, "Failed to check leader lock: {}", e);
                        }
                    }
                }
                Err(e) => {
                    crate::background_error!(
                        LEADER_ELECTION,
                        "db_acquire",
                        Warning,
                        "Failed to acquire connection for leader election: {}",
                        e
                    );
                }
            }
        } else {
            // We think we're leader - verify we still hold the lock
            // by checking if our connection is still valid
            if let Some(ref mut conn) = leader_conn {
                // Ping the connection to keep it alive
                match sqlx::query("SELECT 1").execute(&mut **conn).await {
                    Ok(_) => {
                        debug!(" Leadership renewed (connection alive)");
                    }
                    Err(e) => {
                        // Connection died, which will drop the advisory lock, we lost leadership
                        tracing::warn!("Lost leadership (connection died): {}", e);
                        info!("Lost leadership");
                        is_leader.store(false, Ordering::Relaxed);
                        leader_conn = None;

                        if let Err(e) = on_lose_leadership(pool.clone(), config.clone()).await {
                            crate::background_error!(
                                LEADER_ELECTION,
                                "lose_callback",
                                Error,
                                "Failed to execute on_lose_leadership callback: {}",
                                e
                            );
                        }
                    }
                }
            } else {
                // We think we're leader but have no connection, this can't happen
                crate::background_error!(
                    LEADER_ELECTION,
                    "invariant_violation",
                    Critical,
                    "Inconsistent state: is_leader=true but no connection"
                );
                is_leader.store(false, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    async fn wait_until(mut predicate: impl FnMut() -> bool) {
        for _ in 0..1_000 {
            if predicate() {
                return;
            }

            tokio::time::advance(Duration::from_millis(1)).await;
            tokio::task::yield_now().await;
        }

        panic!("condition was not met before the test deadline");
    }

    #[sqlx::test(migrations = false)]
    async fn failed_gain_cleans_up_unlocks_and_retries(pool: PgPool) {
        tokio::time::pause();

        let lock_id = 8_204_921_019_i64;
        let is_leader = Arc::new(AtomicBool::new(false));
        let gain_attempts = Arc::new(AtomicUsize::new(0));
        let loss_calls = Arc::new(AtomicUsize::new(0));
        let shutdown = CancellationToken::new();
        // Hold this session for the whole test so lock checks cannot reuse the
        // leader's pooled session and get PostgreSQL's re-entrant-lock result.
        let mut contender = pool.acquire().await.unwrap();

        let task = tokio::spawn(leader_election_task(
            pool.clone(),
            config::Config::default(),
            is_leader.clone(),
            lock_id,
            shutdown.clone(),
            {
                let gain_attempts = gain_attempts.clone();
                move |_, _| {
                    let attempt = gain_attempts.fetch_add(1, Ordering::Relaxed);
                    async move {
                        if attempt == 0 {
                            anyhow::bail!("intentional first-attempt failure");
                        }
                        Ok(())
                    }
                }
            },
            {
                let loss_calls = loss_calls.clone();
                move |_, _| {
                    loss_calls.fetch_add(1, Ordering::Relaxed);
                    async { Ok(()) }
                }
            },
        ));

        wait_until(|| loss_calls.load(Ordering::Relaxed) == 1).await;
        assert!(!is_leader.load(Ordering::Relaxed));

        let acquired = sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
            .bind(lock_id)
            .fetch_one(&mut *contender)
            .await
            .unwrap();
        assert!(acquired, "failed gain must release its advisory lock");
        assert!(
            sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock($1)")
                .bind(lock_id)
                .fetch_one(&mut *contender)
                .await
                .unwrap()
        );

        tokio::time::advance(Duration::from_secs(30)).await;
        wait_until(|| gain_attempts.load(Ordering::Relaxed) == 2).await;
        assert!(is_leader.load(Ordering::Relaxed));
        assert!(
            !sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
                .bind(lock_id)
                .fetch_one(&mut *contender)
                .await
                .unwrap(),
            "the successful retry must hold the advisory lock"
        );

        shutdown.cancel();
        task.await.unwrap();
        assert_eq!(loss_calls.load(Ordering::Relaxed), 2);
        assert!(!is_leader.load(Ordering::Relaxed));

        let acquired = sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
            .bind(lock_id)
            .fetch_one(&mut *contender)
            .await
            .unwrap();
        assert!(acquired, "shutdown must release its advisory lock");
        assert!(
            sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock($1)")
                .bind(lock_id)
                .fetch_one(&mut *contender)
                .await
                .unwrap()
        );
        drop(contender);
        tokio::time::resume();
    }
}
