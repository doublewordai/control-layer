//! Leader election using PostgreSQL advisory locks for multi-instance deployments.

use crate::config;
use crate::metrics::errors::component::LEADER_ELECTION;
use sqlx::PgPool;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio_stream::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument};

/// How often a follower retries the advisory lock and a leader re-checks its
/// connection.
const ELECTION_INTERVAL: Duration = Duration::from_secs(30);

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
    let ticks = tokio_stream::wrappers::IntervalStream::new(tokio::time::interval(ELECTION_INTERVAL)).map(|_| ());
    run_leader_election(
        pool,
        config,
        is_leader,
        lock_id,
        shutdown_token,
        ticks,
        on_gain_leadership,
        on_lose_leadership,
    )
    .await;
}

/// The election loop behind [`leader_election_task`], with the election
/// cadence supplied as a stream so tests can drive each attempt explicitly
/// instead of waiting on (or faking) the clock. The loop runs one election
/// round per item; it shuts down when the token is cancelled or the stream
/// ends.
#[allow(clippy::too_many_arguments)]
async fn run_leader_election<F1, F2, Fut1, Fut2>(
    pool: PgPool,
    config: config::Config,
    is_leader: Arc<AtomicBool>,
    lock_id: i64,
    shutdown_token: CancellationToken,
    mut ticks: impl Stream<Item = ()> + Unpin,
    on_gain_leadership: F1,
    on_lose_leadership: F2,
) where
    F1: Fn(PgPool, config::Config) -> Fut1 + Send + 'static,
    F2: Fn(PgPool, config::Config) -> Fut2 + Send + 'static,
    Fut1: std::future::Future<Output = Result<(), anyhow::Error>> + Send + 'static,
    Fut2: std::future::Future<Output = Result<(), anyhow::Error>> + Send + 'static,
{
    let mut leader_conn: Option<sqlx::pool::PoolConnection<sqlx::Postgres>> = None;

    loop {
        let tick = tokio::select! {
            tick = ticks.next() => tick,
            _ = shutdown_token.cancelled() => None,
        };
        if tick.is_none() {
            info!("Shutdown signal received, stopping leader election");
            // If we're currently the leader, call the lose leadership callback
            if is_leader.load(Ordering::Relaxed) {
                is_leader.store(false, Ordering::Relaxed);
                if let Err(e) = on_lose_leadership(pool.clone(), config.clone()).await {
                    crate::background_error!(
                        LEADER_ELECTION,
                        "lose_callback",
                        Error,
                        "Failed to execute on_lose_leadership callback during shutdown: {}",
                        e
                    );
                }
            }
            release_leader_connection(&mut leader_conn, lock_id).await;
            break;
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
                        // The ping failing does not prove the session is dead: a
                        // statement timeout leaves it alive and still holding the
                        // advisory lock. Never return such a connection to the pool.
                        release_leader_connection(&mut leader_conn, lock_id).await;

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
    use tokio_stream::wrappers::ReceiverStream;

    /// Real-time budget for each wait below. The tokio clock is never paused:
    /// the task talks to a real database, and under a paused clock tokio's
    /// auto-advance could run sqlx's timers past round-trips still in flight.
    const WAIT_BUDGET: Duration = Duration::from_secs(30);

    async fn wait_until(what: &str, mut predicate: impl FnMut() -> bool) {
        tokio::time::timeout(WAIT_BUDGET, async {
            while !predicate() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what} did not happen within {WAIT_BUDGET:?}"));
    }

    #[sqlx::test(migrations = false)]
    async fn failed_gain_cleans_up_unlocks_and_retries(pool: PgPool) {
        let lock_id = 8_204_921_019_i64;
        let is_leader = Arc::new(AtomicBool::new(false));
        let gain_attempts = Arc::new(AtomicUsize::new(0));
        let loss_calls = Arc::new(AtomicUsize::new(0));
        let shutdown = CancellationToken::new();
        // Hold this session for the whole test so lock checks cannot reuse the
        // leader's pooled session and get PostgreSQL's re-entrant-lock result.
        let mut contender = pool.acquire().await.unwrap();
        // Every election round is triggered from here, so the task cannot
        // retry until this test says so.
        let (ticks, tick_stream) = tokio::sync::mpsc::channel(1);

        let task = tokio::spawn(run_leader_election(
            pool.clone(),
            config::Config::default(),
            is_leader.clone(),
            lock_id,
            shutdown.clone(),
            ReceiverStream::new(tick_stream),
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

        ticks.send(()).await.unwrap();
        wait_until("the failed gain's lose callback", || loss_calls.load(Ordering::Relaxed) == 1).await;
        assert!(!is_leader.load(Ordering::Relaxed));

        // The lose callback returns before the cleanup's unlock round-trip
        // completes. A blocking advisory lock is the observation point: it
        // queues behind the leader's session and PostgreSQL grants it as part
        // of that session's unlock, so it resolves exactly when the release
        // lands and can never race a retry (none is ticked until below).
        tokio::time::timeout(
            WAIT_BUDGET,
            sqlx::query("SELECT pg_advisory_lock($1)").bind(lock_id).execute(&mut *contender),
        )
        .await
        .expect("failed gain must release its advisory lock")
        .unwrap();
        assert!(
            sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock($1)")
                .bind(lock_id)
                .fetch_one(&mut *contender)
                .await
                .unwrap()
        );

        ticks.send(()).await.unwrap();
        wait_until("the retried gain", || gain_attempts.load(Ordering::Relaxed) == 2).await;
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
        drop(ticks);
        drop(contender);
    }
}
