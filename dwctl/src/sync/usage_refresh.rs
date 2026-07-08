//! Usage-aggregate refresh daemon.
//!
//! Incrementally folds new `http_analytics` rows into the `user_model_usage_daily`
//! rollup (see [`refresh_user_model_usage_daily`]). Unlike the onwards config sync, the
//! signal never leaves the pod: the analytics batcher and this consumer are co-located,
//! so a `tokio::sync::Notify` nudge after each batch flush is all that's needed — no
//! Postgres `LISTEN/NOTIFY` round-trip. Because rows only accrue when there is traffic,
//! and traffic is exactly what nudges us, the rollup stays current under load and does
//! nothing while idle (when there is also nothing to catch up on).
//!
//! Every pod runs this daemon; the refresh takes an advisory lock, so concurrent runs
//! across pods collapse to one and the rest no-op. A periodic fallback tick backstops a
//! missed nudge or drains the cursor after a restart during a lull.

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument};

use crate::config::UsageRefreshConfig;
use crate::db::handlers::analytics::refresh_user_model_usage_daily;
use crate::metrics::errors::component::USAGE_REFRESH;

/// Run the usage-refresh daemon until `shutdown` is cancelled.
///
/// Wakes on a batcher nudge (`notify`) or the fallback tick, runs one incremental
/// refresh, then waits `min_interval` before it can run again (bounding the refresh rate
/// and coalescing bursts of nudges).
#[instrument(skip_all)]
pub async fn run_usage_refresh_daemon(pool: PgPool, config: UsageRefreshConfig, notify: Arc<Notify>, shutdown: CancellationToken) {
    let min_interval = Duration::from_millis(config.min_interval_milliseconds);

    // Fallback tick is optional (0 disables it). Skip missed ticks so a stall doesn't
    // produce a burst of catch-up refreshes.
    let mut fallback = (config.fallback_interval_milliseconds > 0).then(|| {
        let mut t = tokio::time::interval(Duration::from_millis(config.fallback_interval_milliseconds));
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        t
    });

    info!(
        fallback_interval_ms = config.fallback_interval_milliseconds,
        min_interval_ms = config.min_interval_milliseconds,
        "Usage-refresh daemon started"
    );

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("Usage-refresh daemon shutting down");
                break;
            }
            _ = notify.notified() => {}
            _ = async { match fallback.as_mut() { Some(t) => { t.tick().await; }, None => std::future::pending().await } } => {}
        }

        if let Err(e) = refresh_user_model_usage_daily(&pool).await {
            crate::background_error!(USAGE_REFRESH, "refresh", Error, error = %e, "Failed to refresh user_model_usage_daily");
        }

        // Rate-limit: coalesce any nudges that arrive while we wait.
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("Usage-refresh daemon shutting down");
                break;
            }
            _ = tokio::time::sleep(min_interval) => {}
        }
    }
}
