//! Background scheduler daemon for executing probes at their configured intervals.
//!
//! This module provides the `ProbeScheduler` which runs as a background daemon
//! on the leader replica. It periodically polls the database for active probes
//! and manages background tasks that execute each probe at its configured interval.

use crate::probes::db::ProbeManager;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Background scheduler daemon for managing probe execution.
///
/// This runs independently of API operations and only needs to run on the leader replica.
/// It reads probe state from the database and manages background tasks accordingly.
#[derive(Clone)]
pub struct ProbeScheduler {
    pool: PgPool,
    config: crate::config::Config,
    schedulers: Arc<RwLock<HashMap<Uuid, JoinHandle<()>>>>,
}

impl ProbeScheduler {
    /// Create a new ProbeScheduler instance
    pub fn new(pool: PgPool, config: crate::config::Config) -> Self {
        Self {
            pool,
            config,
            schedulers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Initialize schedulers for all active probes in the database.
    ///
    /// This should be called when the replica becomes the leader to start monitoring.
    pub async fn initialize(&self) -> Result<(), anyhow::Error> {
        let probes = ProbeManager::list_active_probes(&self.pool).await?;

        tracing::info!("Initializing schedulers for {} active probes", probes.len());

        for probe in probes {
            self.start_scheduler(probe.id).await?;
        }

        Ok(())
    }

    /// Start a scheduler for a specific probe
    async fn start_scheduler(&self, probe_id: Uuid) -> Result<(), anyhow::Error> {
        // Check if scheduler already exists
        {
            let schedulers = self.schedulers.read().await;
            if schedulers.contains_key(&probe_id) {
                return Ok(());
            }
        }

        let pool = self.pool.clone();
        let config = self.config.clone();

        // Spawn the scheduler task
        let handle = tokio::spawn(async move {
            // Check when the probe last executed to avoid immediate execution on restart
            let _should_delay = match ProbeManager::get_recent_results(&pool, probe_id, 1).await {
                Ok(results) => {
                    if let Some(last_result) = results.first() {
                        let probe = match ProbeManager::get_probe(&pool, probe_id).await {
                            Ok(p) => p,
                            Err(e) => {
                                tracing::error!("Error fetching probe {}: {}", probe_id, e);
                                return;
                            }
                        };

                        let now = chrono::Utc::now();
                        let time_since_last = now - last_result.executed_at;
                        let interval = chrono::Duration::seconds(probe.interval_seconds as i64);

                        if time_since_last < interval {
                            // Calculate how long to wait until next scheduled execution
                            let wait_duration = interval - time_since_last;
                            let wait_secs = wait_duration.num_seconds().max(0) as u64;

                            tracing::info!(
                                "Probe {} last executed {}s ago, waiting {}s until next execution",
                                probe.name,
                                time_since_last.num_seconds(),
                                wait_secs
                            );

                            tokio::time::sleep(tokio::time::Duration::from_secs(wait_secs)).await;
                            false // Don't delay again
                        } else {
                            tracing::info!(
                                "Probe {} last executed {}s ago (>{}s interval), executing immediately",
                                probe.name,
                                time_since_last.num_seconds(),
                                probe.interval_seconds
                            );
                            true // Execute immediately
                        }
                    } else {
                        true // No previous results, execute immediately
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Error checking last execution for probe {}: {}, will execute immediately",
                        probe_id,
                        e
                    );
                    true
                }
            };

            loop {
                // Get the probe to check if it's still active and get the interval
                let probe = match ProbeManager::get_probe(&pool, probe_id).await {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!("Error fetching probe {}: {}", probe_id, e);
                        break;
                    }
                };

                // If probe is not active, stop the scheduler
                if !probe.active {
                    tracing::info!("Probe {} is not active, stopping scheduler", probe.name);
                    break;
                }

                // Execute the probe
                match ProbeManager::execute_probe(&pool, probe_id, &config).await {
                    Ok(result) => {
                        if result.success {
                            tracing::debug!(
                                "Probe {} executed successfully in {}ms",
                                probe.name,
                                result.response_time_ms.unwrap_or(0)
                            );
                        } else {
                            tracing::warn!("Probe {} execution failed: {:?}", probe.name, result.error_message);
                        }
                    }
                    Err(e) => {
                        tracing::error!("Error executing probe {}: {}", probe.name, e);
                    }
                }

                // Sleep for the configured interval
                tokio::time::sleep(tokio::time::Duration::from_secs(probe.interval_seconds as u64)).await;
            }

            tracing::info!("Scheduler for probe {} has stopped", probe_id);
        });

        // Store the handle
        let mut schedulers = self.schedulers.write().await;
        schedulers.insert(probe_id, handle);

        tracing::info!("Started scheduler for probe {}", probe_id);

        Ok(())
    }

    /// Stop a scheduler for a specific probe
    async fn stop_scheduler(&self, probe_id: Uuid) -> Result<(), anyhow::Error> {
        let mut schedulers = self.schedulers.write().await;

        if let Some(handle) = schedulers.remove(&probe_id) {
            handle.abort();
            tracing::info!("Stopped scheduler for probe {}", probe_id);
        }

        Ok(())
    }

    /// Stop all running schedulers (called when losing leadership)
    pub async fn stop_all(&self) -> Result<(), anyhow::Error> {
        let mut schedulers = self.schedulers.write().await;
        let count = schedulers.len();

        for (probe_id, handle) in schedulers.drain() {
            handle.abort();
            tracing::debug!("Stopped scheduler for probe {}", probe_id);
        }

        if count > 0 {
            tracing::info!("Stopped {} probe schedulers", count);
        }

        Ok(())
    }

    /// Synchronize schedulers with database state
    ///
    /// This should be called periodically to ensure the scheduler state matches the database:
    /// - Start schedulers for newly activated probes
    /// - Stop schedulers for deactivated/deleted probes
    pub async fn sync_with_database(&self) -> Result<(), anyhow::Error> {
        let active_probes = ProbeManager::list_active_probes(&self.pool).await?;
        let active_probe_ids: std::collections::HashSet<Uuid> = active_probes.iter().map(|p| p.id).collect();

        let schedulers = self.schedulers.read().await;
        let running_probe_ids: std::collections::HashSet<Uuid> = schedulers.keys().copied().collect();
        drop(schedulers); // Release read lock

        // Start schedulers for probes that are active but not running
        for probe_id in active_probe_ids.difference(&running_probe_ids) {
            tracing::info!("Starting scheduler for newly activated probe {}", probe_id);
            if let Err(e) = self.start_scheduler(*probe_id).await {
                tracing::error!("Failed to start scheduler for probe {}: {}", probe_id, e);
            }
        }

        // Stop schedulers for probes that are running but not active
        for probe_id in running_probe_ids.difference(&active_probe_ids) {
            tracing::info!("Stopping scheduler for deactivated probe {}", probe_id);
            if let Err(e) = self.stop_scheduler(*probe_id).await {
                tracing::error!("Failed to stop scheduler for probe {}: {}", probe_id, e);
            }
        }

        Ok(())
    }

    /// Handle a probe change notification
    async fn handle_probe_change(&self, probe_id: Uuid, active: bool) -> Result<(), anyhow::Error> {
        if active {
            // Probe is now active - start its scheduler if not already running
            if !self.is_scheduler_running(probe_id).await {
                tracing::info!("Probe {} activated, starting scheduler", probe_id);
                self.start_scheduler(probe_id).await?;
            }
        } else {
            // Probe is now inactive - stop its scheduler if running
            if self.is_scheduler_running(probe_id).await {
                tracing::info!("Probe {} deactivated, stopping scheduler", probe_id);
                self.stop_scheduler(probe_id).await?;
            }
        }
        Ok(())
    }

    /// Check if a scheduler is running for a probe
    async fn is_scheduler_running(&self, probe_id: Uuid) -> bool {
        let schedulers = self.schedulers.read().await;
        schedulers.contains_key(&probe_id)
    }

    /// Run the scheduler daemon in polling mode (no LISTEN/NOTIFY)
    ///
    /// This mode periodically syncs with the database using simple queries.
    /// Useful for testing or environments where LISTEN/NOTIFY is not available.
    async fn run_daemon_polling(self, sync_interval_seconds: u64) {
        tracing::info!(
            "Starting probe scheduler daemon in polling mode (sync every {}s)",
            sync_interval_seconds
        );

        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(sync_interval_seconds));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            if let Err(e) = self.sync_with_database().await {
                tracing::error!("Error syncing probe schedulers with database: {}", e);
            }
        }
    }

    /// Run the scheduler daemon using LISTEN/NOTIFY for immediate updates
    ///
    /// This establishes a LISTEN connection to receive notifications when probes change,
    /// allowing immediate reaction to changes. A periodic full sync runs as a fallback.
    ///
    /// Set `use_listen_notify` to false to use simple polling instead (useful for tests).
    pub async fn run_daemon(self, use_listen_notify: bool, fallback_sync_interval_seconds: u64) {
        if !use_listen_notify {
            return self.run_daemon_polling(fallback_sync_interval_seconds).await;
        }
        tracing::info!(
            "Starting probe scheduler daemon with LISTEN/NOTIFY (fallback sync every {}s)",
            fallback_sync_interval_seconds
        );

        loop {
            // Establish a dedicated connection for LISTEN
            let mut listener = match sqlx::postgres::PgListener::connect_with(&self.pool).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!("Failed to create LISTEN connection: {}", e);
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            // LISTEN on the probe_changes channel
            if let Err(e) = listener.listen("probe_changes").await {
                tracing::error!("Failed to LISTEN on probe_changes: {}", e);
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                continue;
            }

            tracing::info!("LISTEN connection established for probe changes");

            // Create a periodic fallback sync interval
            let mut fallback_interval = tokio::time::interval(tokio::time::Duration::from_secs(fallback_sync_interval_seconds));
            fallback_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    // Handle incoming notifications
                    notification = listener.recv() => {
                        match notification {
                            Ok(notif) => {
                                // Parse the notification payload
                                match serde_json::from_str::<serde_json::Value>(notif.payload()) {
                                    Ok(payload) => {
                                        if let (Some(probe_id), Some(active)) = (
                                            payload.get("probe_id").and_then(|v| v.as_str()).and_then(|s| Uuid::parse_str(s).ok()),
                                            payload.get("active").and_then(|v| v.as_bool())
                                        ) {
                                            tracing::debug!("Received probe change notification: probe_id={}, active={}", probe_id, active);
                                            if let Err(e) = self.handle_probe_change(probe_id, active).await {
                                                tracing::error!("Failed to handle probe change for {}: {}", probe_id, e);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("Failed to parse notification payload: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!("Error receiving notification: {}", e);
                                break; // Break inner loop to reconnect
                            }
                        }
                    }

                    // Periodic fallback sync
                    _ = fallback_interval.tick() => {
                        tracing::debug!("Running fallback sync");
                        if let Err(e) = self.sync_with_database().await {
                            tracing::error!("Error during fallback sync: {}", e);
                        }
                    }
                }
            }

            // If we broke out of the inner loop, the connection died
            tracing::warn!("LISTEN connection lost, reconnecting in 5s...");
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }
    }
}
