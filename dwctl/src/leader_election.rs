use crate::config;
use sqlx::PgPool;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{debug, info, instrument};

/// Background task for leader election
/// Runs periodically to maintain leadership or attempt to acquire it
///
/// We use leadership election for figuring out who runs background tasks like sending probes to
/// the endpoints. At some point, we may want to expand this to other tasks as well.
///
/// PostgreSQL advisory locks are session-based, so we need to maintain a dedicated connection
/// for the entire duration we want to hold the lock.
#[instrument(skip(pool, config, lock_id, on_gain_leadership, on_lose_leadership))]
pub async fn leader_election_task<F1, F2, Fut1, Fut2>(
    pool: PgPool,
    config: config::Config,
    is_leader: Arc<AtomicBool>,
    lock_id: i64,
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
        interval.tick().await;

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
                                tracing::error!("Failed to execute on_gain_leadership callback: {}", e);
                            }
                        }
                        Ok(false) => {
                            // Someone else has the lock
                            debug!("Following - will retry");
                        }
                        Err(e) => {
                            tracing::error!("Failed to check leader lock: {}", e);
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to acquire connection for leader election: {}", e);
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
                            tracing::error!("Failed to execute on_lose_leadership callback: {}", e);
                        }
                    }
                }
            } else {
                // We think we're leader but have no connection, this can't happen
                tracing::error!("Inconsistent state: is_leader=true but no connection");
                is_leader.store(false, Ordering::Relaxed);
            }
        }
    }
}
