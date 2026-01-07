//! Database pool metrics sampling.
//!
//! Provides a background task that periodically samples database pool state
//! and records metrics for observability.

use std::time::Duration;

use metrics::gauge;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

/// Configuration for pool metrics sampling
#[derive(Debug, Clone)]
pub struct PoolMetricsConfig {
    /// How often to sample pool metrics
    pub sample_interval: Duration,
}

impl Default for PoolMetricsConfig {
    fn default() -> Self {
        Self {
            sample_interval: Duration::from_secs(5),
        }
    }
}

/// A named pool for metrics labeling
pub struct LabeledPool {
    pub name: &'static str,
    pub pool: PgPool,
}

/// Start the pool metrics sampler background task.
///
/// This task periodically samples the pool state and records:
/// - `db_pool_connections_total` - Total connections in the pool
/// - `db_pool_connections_idle` - Idle connections available
/// - `db_pool_connections_in_use` - Connections currently in use
/// - `db_pool_connections_max` - Maximum configured connections
///
/// All metrics are labeled with `pool` to distinguish between different pools.
pub async fn run_pool_metrics_sampler(
    pools: Vec<LabeledPool>,
    config: PoolMetricsConfig,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    info!(
        "Starting pool metrics sampler for {} pools with {:?} interval",
        pools.len(),
        config.sample_interval
    );

    // Record max connections once at startup (it doesn't change)
    for labeled in &pools {
        let max = labeled.pool.options().get_max_connections();
        gauge!("dwctl_db_pool_connections_max", "pool" => labeled.name.to_string()).set(max as f64);
    }

    let mut interval = tokio::time::interval(config.sample_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("Pool metrics sampler shutting down");
                break;
            }
            _ = interval.tick() => {
                for labeled in &pools {
                    let size = labeled.pool.size();
                    let idle = labeled.pool.num_idle();
                    let in_use = size as usize - idle;

                    gauge!("dwctl_db_pool_connections_total", "pool" => labeled.name.to_string())
                        .set(size as f64);
                    gauge!("dwctl_db_pool_connections_idle", "pool" => labeled.name.to_string())
                        .set(idle as f64);
                    gauge!("dwctl_db_pool_connections_in_use", "pool" => labeled.name.to_string())
                        .set(in_use as f64);

                    debug!(
                        pool = labeled.name,
                        size,
                        idle,
                        in_use,
                        "Sampled pool metrics"
                    );
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[sqlx::test]
    async fn test_pool_metrics_sampler_runs_and_shuts_down(pool: PgPool) {
        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();

        let pools = vec![LabeledPool {
            name: "test",
            pool: pool.clone(),
        }];

        let config = PoolMetricsConfig {
            sample_interval: Duration::from_millis(10),
        };

        // Spawn the sampler
        let handle = tokio::spawn(async move { run_pool_metrics_sampler(pools, config, shutdown_clone).await });

        // Let it run for a few samples
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Verify task is still running
        assert!(!handle.is_finished(), "Sampler should still be running");

        // Signal shutdown
        shutdown.cancel();

        // Should complete gracefully
        let result = handle.await.unwrap();
        assert!(result.is_ok(), "Sampler should exit cleanly");
    }

    #[test]
    fn test_pool_metrics_config_default() {
        let config = PoolMetricsConfig::default();
        assert_eq!(config.sample_interval, Duration::from_secs(5));
    }
}
