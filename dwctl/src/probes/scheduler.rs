//! Background scheduler daemon for executing probes at their configured intervals.
//!
//! This module provides the `ProbeScheduler` which runs as a background daemon
//! on the leader replica. It periodically polls the database for active probes
//! and manages background tasks that execute each probe at its configured interval.

use crate::errors::Error as AppError;
use crate::metrics::errors::component::PROBE_SCHEDULER;
use crate::probes::db::ProbeManager;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Backoff applied after a transient (non-`NotFound`) `get_probe` failure before
/// the per-probe scheduler retries. This keeps a momentary pool-acquire or
/// statement timeout from terminating the probe's background task: the task
/// backs off and retries instead of exiting, so the probe is not left
/// unmonitored. (`sync_with_database`/`is_scheduler_running` still prune a
/// finished handle as a safety net for any other exit path.)
const PROBE_FETCH_RETRY_BACKOFF: tokio::time::Duration = tokio::time::Duration::from_secs(30);

/// Background scheduler daemon for managing probe execution.
///
/// This runs independently of API operations and only needs to run on the leader replica.
/// It reads probe state from the database and manages background tasks accordingly.
#[derive(Clone)]
pub struct ProbeScheduler {
    /// Query traffic (pooled endpoint when configured).
    pool: sqlx_pool_router::DynPools,
    /// Direct connections for the probe-changes LISTEN session.
    listener_pool: sqlx_pool_router::DynPools,
    config: crate::config::Config,
    schedulers: Arc<RwLock<HashMap<Uuid, JoinHandle<()>>>>,
}

impl ProbeScheduler {
    /// Create a new ProbeScheduler instance
    pub fn new(pool: impl sqlx_pool_router::PoolProvider, config: crate::config::Config) -> Self {
        let pool = sqlx_pool_router::DynPools::new(pool);
        Self {
            listener_pool: pool.clone(),
            pool,
            config,
            schedulers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Use `listener_pool` (direct connections) for the LISTEN session instead
    /// of the query pool. Required when the query pool is a pooled endpoint.
    pub fn with_listener_pool(mut self, listener_pool: impl sqlx_pool_router::PoolProvider) -> Self {
        self.listener_pool = sqlx_pool_router::DynPools::new(listener_pool);
        self
    }

    /// Initialize schedulers for all active probes in the database.
    ///
    /// This should be called when the replica becomes the leader to start monitoring.
    pub async fn initialize(&self, shutdown_token: CancellationToken) -> Result<(), anyhow::Error> {
        // Check if we're already shutting down
        if shutdown_token.is_cancelled() {
            tracing::info!("Shutdown signal received, skipping scheduler initialization");
            return Ok(());
        }

        let probes = ProbeManager::list_active_probes(&self.pool.write()).await?;

        tracing::info!("Initializing schedulers for {} active probes", probes.len());

        for probe in probes {
            // Check for shutdown between spawning tasks
            if shutdown_token.is_cancelled() {
                tracing::info!("Shutdown signal received during initialization, stopping");
                break;
            }
            self.start_scheduler(probe.id, shutdown_token.clone()).await?;
        }

        Ok(())
    }

    /// Start a scheduler for a specific probe
    async fn start_scheduler(&self, probe_id: Uuid, shutdown_token: CancellationToken) -> Result<(), anyhow::Error> {
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
            // Check when the probe last executed to avoid immediate execution on restart.
            // On a transient fetch error we skip the startup delay and let the main loop
            // retry, rather than aborting the whole scheduler task (which would leave a
            // stale handle in the map until the next leader churn).
            match ProbeManager::get_recent_results(&pool.write(), probe_id, 1).await {
                Ok(results) => {
                    if let Some(last_result) = results.first() {
                        match ProbeManager::get_probe(&pool.write(), probe_id).await {
                            Ok(probe) => {
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

                                    tokio::select! {
                                        _ = tokio::time::sleep(tokio::time::Duration::from_secs(wait_secs)) => {}
                                        _ = shutdown_token.cancelled() => {
                                            tracing::info!("Shutdown signal received during startup delay, stopping scheduler for probe {}", probe_id);
                                            return;
                                        }
                                    }
                                } else {
                                    tracing::info!(
                                        "Probe {} last executed {}s ago (>{}s interval), executing immediately",
                                        probe.name,
                                        time_since_last.num_seconds(),
                                        probe.interval_seconds
                                    );
                                }
                            }
                            Err(AppError::NotFound { .. }) => {
                                tracing::info!("Probe {} not found at startup, not starting scheduler", probe_id);
                                return;
                            }
                            Err(e) => {
                                crate::background_error!(
                                    PROBE_SCHEDULER,
                                    "probe_fetch",
                                    Warning,
                                    "Error fetching probe {} at startup: {}, will retry from main loop",
                                    probe_id,
                                    e
                                );
                                // Skip the startup delay; the main loop retries the fetch.
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Error checking last execution for probe {}: {}, will execute immediately",
                        probe_id,
                        e
                    );
                }
            }

            loop {
                // Check for shutdown signal
                if shutdown_token.is_cancelled() {
                    tracing::info!("Shutdown signal received, stopping scheduler for probe {}", probe_id);
                    break;
                }

                // Get the probe to check if it's still active and get the interval.
                // A `NotFound` means the probe was deleted, so stop scheduling it.
                // Any other error is treated as transient (e.g. pool-acquire/statement
                // timeout): back off and retry instead of exiting, so a momentary DB
                // blip does not leave the probe unmonitored. finished handles are also
                // pruned by `sync_with_database`/`is_scheduler_running` as a safety net.
                let probe = match ProbeManager::get_probe(&pool.write(), probe_id).await {
                    Ok(p) => p,
                    Err(AppError::NotFound { .. }) => {
                        tracing::info!("Probe {} not found, stopping scheduler", probe_id);
                        break;
                    }
                    Err(e) => {
                        crate::background_error!(
                            PROBE_SCHEDULER,
                            "probe_fetch",
                            Warning,
                            "Error fetching probe {}: {}, retrying in {:?}",
                            probe_id,
                            e,
                            PROBE_FETCH_RETRY_BACKOFF
                        );
                        tokio::select! {
                            _ = tokio::time::sleep(PROBE_FETCH_RETRY_BACKOFF) => {}
                            _ = shutdown_token.cancelled() => {
                                tracing::info!("Shutdown signal received during fetch retry, stopping scheduler for probe {}", probe_id);
                                break;
                            }
                        }
                        continue;
                    }
                };

                // If probe is not active, stop the scheduler
                if !probe.active {
                    tracing::info!("Probe {} is not active, stopping scheduler", probe.name);
                    break;
                }

                // Execute the probe
                match ProbeManager::execute_probe(&pool.write(), probe_id, &config).await {
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
                        crate::background_error!(
                            PROBE_SCHEDULER,
                            "probe_execute",
                            Warning,
                            "Error executing probe {}: {}",
                            probe.name,
                            e
                        );
                    }
                }

                // Sleep for the configured interval or until shutdown
                tokio::select! {
                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(probe.interval_seconds as u64)) => {}
                    _ = shutdown_token.cancelled() => {
                        tracing::info!("Shutdown signal received during sleep, stopping scheduler for probe {}", probe_id);
                        break;
                    }
                }
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
    pub async fn sync_with_database(&self, shutdown_token: CancellationToken) -> Result<(), anyhow::Error> {
        let active_probes = ProbeManager::list_active_probes(&self.pool.write()).await?;
        let active_probe_ids: std::collections::HashSet<Uuid> = active_probes.iter().map(|p| p.id).collect();

        // Prune handles for scheduler tasks that have exited on their own (e.g. a
        // transient `get_probe` sqlx error that slipped past the in-loop retry, or a
        // panic/early return). Without this, a finished JoinHandle would keep its
        // probe in `running_probe_ids`, making both the start and stop set-differences
        // skip it and leaving the probe unmonitored for the rest of the leadership
        // session. Dropping a finished handle is harmless; live handles are retained.
        let mut schedulers = self.schedulers.write().await;
        schedulers.retain(|_, handle| !handle.is_finished());
        let running_probe_ids: std::collections::HashSet<Uuid> = schedulers.keys().copied().collect();
        drop(schedulers); // Release write lock

        // Start schedulers for probes that are active but not running
        for probe_id in active_probe_ids.difference(&running_probe_ids) {
            tracing::info!("Starting scheduler for newly activated probe {}", probe_id);
            if let Err(e) = self.start_scheduler(*probe_id, shutdown_token.clone()).await {
                crate::background_error!(
                    PROBE_SCHEDULER,
                    "scheduler_start",
                    Warning,
                    "Failed to start scheduler for probe {}: {}",
                    probe_id,
                    e
                );
            }
        }

        // Stop schedulers for probes that are running but not active
        for probe_id in running_probe_ids.difference(&active_probe_ids) {
            tracing::info!("Stopping scheduler for deactivated probe {}", probe_id);
            if let Err(e) = self.stop_scheduler(*probe_id).await {
                crate::background_error!(
                    PROBE_SCHEDULER,
                    "scheduler_stop",
                    Warning,
                    "Failed to stop scheduler for probe {}: {}",
                    probe_id,
                    e
                );
            }
        }

        Ok(())
    }

    /// Handle a probe change notification
    async fn handle_probe_change(&self, probe_id: Uuid, active: bool, shutdown_token: CancellationToken) -> Result<(), anyhow::Error> {
        if active {
            // Probe is now active - start its scheduler if not already running
            if !self.is_scheduler_running(probe_id).await {
                tracing::info!("Probe {} activated, starting scheduler", probe_id);
                self.start_scheduler(probe_id, shutdown_token).await?;
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
        // Prune any finished tasks first so a probe whose scheduler died (e.g. on a
        // transient DB error) is reported as not running and becomes eligible for
        // restart by `handle_probe_change`. Otherwise `contains_key` would return
        // true for a stale handle and the probe would never be rescheduled.
        let mut schedulers = self.schedulers.write().await;
        schedulers.retain(|_, handle| !handle.is_finished());
        schedulers.contains_key(&probe_id)
    }

    /// Run the scheduler daemon in polling mode (no LISTEN/NOTIFY)
    ///
    /// This mode periodically syncs with the database using simple queries.
    /// Useful for testing or environments where LISTEN/NOTIFY is not available.
    async fn run_daemon_polling(self, shutdown_token: CancellationToken, sync_interval_seconds: u64) {
        tracing::info!(
            "Starting probe scheduler daemon in polling mode (sync every {}s)",
            sync_interval_seconds
        );

        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(sync_interval_seconds));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(e) = self.sync_with_database(shutdown_token.clone()).await {
                        crate::background_error!(PROBE_SCHEDULER, "db_sync", Warning, "Error syncing probe schedulers with database: {}", e);
                    }
                }
                _ = shutdown_token.cancelled() => {
                    tracing::info!("Shutdown signal received, stopping probe scheduler daemon");
                    break;
                }
            }
        }
    }

    /// Run the scheduler daemon using LISTEN/NOTIFY for immediate updates
    ///
    /// This establishes a LISTEN connection to receive notifications when probes change,
    /// allowing immediate reaction to changes. A periodic full sync runs as a fallback.
    ///
    /// Set `use_listen_notify` to false to use simple polling instead (useful for tests).
    pub async fn run_daemon(self, shutdown_token: CancellationToken, use_listen_notify: bool, fallback_sync_interval_seconds: u64) {
        if !use_listen_notify {
            return self.run_daemon_polling(shutdown_token, fallback_sync_interval_seconds).await;
        }
        tracing::info!(
            "Starting probe scheduler daemon with LISTEN/NOTIFY (fallback sync every {}s)",
            fallback_sync_interval_seconds
        );

        loop {
            // Check for shutdown before reconnecting
            if shutdown_token.is_cancelled() {
                tracing::info!("Shutdown signal received, stopping probe scheduler daemon");
                break;
            }

            // Establish a dedicated connection for LISTEN
            let mut listener = match sqlx::postgres::PgListener::connect_with(&self.listener_pool.write()).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!("Failed to create LISTEN connection: {}", e);
                    tokio::select! {
                        _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {}
                        _ = shutdown_token.cancelled() => {
                            tracing::info!("Shutdown signal received during reconnect delay");
                            break;
                        }
                    }
                    continue;
                }
            };

            // LISTEN on the probe_changes channel
            if let Err(e) = listener.listen("probe_changes").await {
                tracing::error!("Failed to LISTEN on probe_changes: {}", e);
                tokio::select! {
                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {}
                    _ = shutdown_token.cancelled() => {
                        tracing::info!("Shutdown signal received during reconnect delay");
                        break;
                    }
                }
                continue;
            }

            tracing::info!("LISTEN connection established for probe changes");

            // Create a periodic fallback sync interval
            let mut fallback_interval = tokio::time::interval(tokio::time::Duration::from_secs(fallback_sync_interval_seconds));
            fallback_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    // Handle shutdown signal
                    _ = shutdown_token.cancelled() => {
                        tracing::info!("Shutdown signal received, stopping probe scheduler daemon");
                        return;
                    }

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
                                            if let Err(e) = self.handle_probe_change(probe_id, active, shutdown_token.clone()).await {
                                                crate::background_error!(PROBE_SCHEDULER, "probe_change", Warning, "Failed to handle probe change for {}: {}", probe_id, e);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        crate::background_error!(PROBE_SCHEDULER, "payload_parse", Warning, "Failed to parse notification payload: {}", e);
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
                        if let Err(e) = self.sync_with_database(shutdown_token.clone()).await {
                            crate::background_error!(PROBE_SCHEDULER, "db_sync", Warning, "Error during fallback sync: {}", e);
                        }
                    }
                }
            }

            // If we broke out of the inner loop, the connection died
            tracing::warn!("LISTEN connection lost, reconnecting in 5s...");
            tokio::select! {
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {}
                _ = shutdown_token.cancelled() => {
                    tracing::info!("Shutdown signal received during reconnect delay");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::probes::CreateProbe;
    use crate::probes::db::ProbeManager;

    async fn setup_test_deployment(pool: &sqlx::PgPool) -> Uuid {
        // Generate unique names using UUID
        let unique_id = Uuid::new_v4();
        let endpoint_name = format!("test-endpoint-{}", unique_id);
        let model_name = format!("test-model-{}", unique_id);

        // Create test endpoint
        let endpoint_id = sqlx::query_scalar!(
            "INSERT INTO inference_endpoints (name, url, created_by) VALUES ($1, $2, $3) RETURNING id",
            endpoint_name,
            "http://localhost:8080",
            Uuid::nil()
        )
        .fetch_one(pool)
        .await
        .unwrap();

        // Create test deployment
        sqlx::query_scalar!(
            "INSERT INTO deployed_models (model_name, alias, type, hosted_on, created_by) VALUES ($1, $2, $3, $4, $5) RETURNING id",
            model_name.clone(),
            model_name,
            "chat" as _,
            endpoint_id,
            Uuid::nil()
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    fn create_test_config() -> crate::config::Config {
        crate::test::utils::create_test_config()
    }

    #[sqlx::test]
    async fn test_scheduler_initialize(pool: sqlx::PgPool) {
        // Create separate deployments for each probe
        let deployment_id1 = setup_test_deployment(&pool).await;
        let deployment_id2 = setup_test_deployment(&pool).await;

        // Create active probes
        let _probe1 = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Probe 1".to_string(),
                deployment_id: deployment_id1,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        let _probe2 = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Probe 2".to_string(),
                deployment_id: deployment_id2,
                interval_seconds: 120,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        let config = create_test_config();
        let scheduler = ProbeScheduler::new(pool, config);

        scheduler.initialize(CancellationToken::new()).await.unwrap();

        // Check that schedulers are running
        let schedulers = scheduler.schedulers.read().await;
        assert_eq!(schedulers.len(), 2);
    }

    #[sqlx::test]
    async fn test_sync_starts_new_schedulers(pool: sqlx::PgPool) {
        let deployment_id = setup_test_deployment(&pool).await;

        let config = create_test_config();
        let scheduler = ProbeScheduler::new(pool.clone(), config);

        // Initially no schedulers
        scheduler.initialize(CancellationToken::new()).await.unwrap();
        let initial_count = scheduler.schedulers.read().await.len();
        assert_eq!(initial_count, 0);

        // Create a new probe
        let _probe = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "New Probe".to_string(),
                deployment_id,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        // Sync should start the scheduler
        scheduler.sync_with_database(CancellationToken::new()).await.unwrap();

        let new_count = scheduler.schedulers.read().await.len();
        assert_eq!(new_count, 1);
    }

    #[sqlx::test]
    async fn test_sync_stops_deactivated_schedulers(pool: sqlx::PgPool) {
        let deployment_id = setup_test_deployment(&pool).await;

        let probe = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Test Probe".to_string(),
                deployment_id,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        let config = create_test_config();
        let scheduler = ProbeScheduler::new(pool.clone(), config);

        scheduler.initialize(CancellationToken::new()).await.unwrap();
        assert_eq!(scheduler.schedulers.read().await.len(), 1);

        // Deactivate the probe
        ProbeManager::deactivate_probe(&pool, probe.id).await.unwrap();

        // Sync should stop the scheduler
        scheduler.sync_with_database(CancellationToken::new()).await.unwrap();
        assert_eq!(scheduler.schedulers.read().await.len(), 0);
    }

    #[sqlx::test]
    async fn test_stop_all_schedulers(pool: sqlx::PgPool) {
        // Create separate deployment for each probe
        for i in 0..3 {
            let deployment_id = setup_test_deployment(&pool).await;
            ProbeManager::create_probe(
                &pool,
                CreateProbe {
                    name: format!("Probe {}", i),
                    deployment_id,
                    interval_seconds: 60,
                    http_method: "POST".to_string(),
                    request_path: None,
                    request_body: None,
                },
            )
            .await
            .unwrap();
        }

        let config = create_test_config();
        let scheduler = ProbeScheduler::new(pool, config);

        scheduler.initialize(CancellationToken::new()).await.unwrap();
        assert_eq!(scheduler.schedulers.read().await.len(), 3);

        // Stop all
        scheduler.stop_all().await.unwrap();
        assert_eq!(scheduler.schedulers.read().await.len(), 0);
    }

    #[sqlx::test]
    async fn test_scheduler_ignores_inactive_probes(pool: sqlx::PgPool) {
        let deployment_id = setup_test_deployment(&pool).await;

        // Create probe and immediately deactivate
        let probe = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Inactive Probe".to_string(),
                deployment_id,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        ProbeManager::deactivate_probe(&pool, probe.id).await.unwrap();

        let config = create_test_config();
        let scheduler = ProbeScheduler::new(pool, config);

        scheduler.initialize(CancellationToken::new()).await.unwrap();

        // Should not have any schedulers
        assert_eq!(scheduler.schedulers.read().await.len(), 0);
    }

    /// Helper: create an already-finished `JoinHandle` that simulates a per-probe
    /// scheduler task which has exited on its own (e.g. a transient `get_probe`
    /// sqlx error), leaving a stale handle behind in the `schedulers` map.
    async fn finished_join_handle() -> JoinHandle<()> {
        let handle = tokio::spawn(async {});
        // The trivial task completes as soon as the runtime polls it; wait for
        // `is_finished()` to flip so callers can rely on it.
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !handle.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("trivial spawned task should finish immediately");
        handle
    }

    async fn create_active_probe(pool: &sqlx::PgPool, name: &str) -> crate::db::models::probes::Probe {
        let deployment_id = setup_test_deployment(pool).await;
        ProbeManager::create_probe(
            pool,
            CreateProbe {
                name: name.to_string(),
                deployment_id,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap()
    }

    /// Regression: the periodic fallback `sync_with_database` must prune a
    /// finished scheduler handle and restart the probe's scheduler, so a probe
    /// whose task died on a transient (non-`NotFound`) DB error is monitored
    /// again. Without the prune, the stale handle keeps the probe in both the
    /// "active" and "running" sets, so neither set-difference acts on it.
    #[sqlx::test]
    async fn test_sync_restarts_probe_with_finished_scheduler(pool: sqlx::PgPool) {
        let probe = create_active_probe(&pool, "Finished-handle probe").await;

        let config = create_test_config();
        let scheduler = ProbeScheduler::new(pool.clone(), config);

        // Simulate a scheduler task that has died on its own while the probe is
        // still active in the DB.
        {
            let mut schedulers = scheduler.schedulers.write().await;
            schedulers.insert(probe.id, finished_join_handle().await);
        }
        assert!(
            scheduler.schedulers.read().await.get(&probe.id).unwrap().is_finished(),
            "precondition: probe's scheduler handle should be finished"
        );

        scheduler.sync_with_database(CancellationToken::new()).await.unwrap();

        let schedulers = scheduler.schedulers.read().await;
        assert_eq!(schedulers.len(), 1, "still-active probe should have exactly one scheduler");
        let handle = schedulers.get(&probe.id).expect("probe scheduler should have been restarted");
        assert!(
            !handle.is_finished(),
            "restarted scheduler task should be live, not the stale finished handle"
        );
    }

    /// Regression: `is_scheduler_running` must prune a finished handle and
    /// report the probe as not running, so `handle_probe_change` can restart it
    /// on a (re)activation notification instead of skipping it due to a stale
    /// handle.
    #[sqlx::test]
    async fn test_is_scheduler_running_prunes_finished_scheduler(pool: sqlx::PgPool) {
        let probe = create_active_probe(&pool, "Finished-handle probe").await;

        let config = create_test_config();
        let scheduler = ProbeScheduler::new(pool, config);

        {
            let mut schedulers = scheduler.schedulers.write().await;
            schedulers.insert(probe.id, finished_join_handle().await);
        }

        assert!(
            !scheduler.is_scheduler_running(probe.id).await,
            "a probe with only a finished handle should not be considered running"
        );
        assert!(
            !scheduler.schedulers.read().await.contains_key(&probe.id),
            "the finished handle should have been pruned from the map"
        );
    }

    /// Regression: an activation change notification must (re)start a scheduler
    /// for a probe whose previous scheduler died, rather than skipping it
    /// because a stale finished handle is still keyed in the map.
    #[sqlx::test]
    async fn test_handle_probe_change_restarts_finished_scheduler(pool: sqlx::PgPool) {
        let probe = create_active_probe(&pool, "Finished-handle probe").await;

        let config = create_test_config();
        let scheduler = ProbeScheduler::new(pool.clone(), config);

        {
            let mut schedulers = scheduler.schedulers.write().await;
            schedulers.insert(probe.id, finished_join_handle().await);
        }

        scheduler
            .handle_probe_change(probe.id, true, CancellationToken::new())
            .await
            .unwrap();

        let schedulers = scheduler.schedulers.read().await;
        assert_eq!(schedulers.len(), 1);
        assert!(
            !schedulers.get(&probe.id).unwrap().is_finished(),
            "a live scheduler should have been (re)started for the probe"
        );
    }

    /// Regression: pruning must not disturb a live (still-running) scheduler.
    /// When a probe is active and its scheduler is running, `sync_with_database`
    /// should leave it untouched (not pruned, not replaced).
    #[sqlx::test]
    async fn test_sync_keeps_running_scheduler(pool: sqlx::PgPool) {
        let _probe = create_active_probe(&pool, "Live probe").await;

        let config = create_test_config();
        let scheduler = ProbeScheduler::new(pool, config);

        scheduler.initialize(CancellationToken::new()).await.unwrap();
        assert_eq!(scheduler.schedulers.read().await.len(), 1);

        let live_before = scheduler.schedulers.read().await.values().next().unwrap().is_finished();
        assert!(!live_before, "precondition: scheduler should be live after initialize");

        scheduler.sync_with_database(CancellationToken::new()).await.unwrap();

        let schedulers = scheduler.schedulers.read().await;
        assert_eq!(schedulers.len(), 1, "live scheduler should not have been churned");
        assert!(
            !schedulers.values().next().unwrap().is_finished(),
            "running scheduler should still be live after sync"
        );
    }

    /// Regression: a `NotFound` (probe deleted while the probe is no longer
    /// active in the DB) self-heals via the stop branch of `sync_with_database`.
    /// The finished handle is pruned; since the probe is absent from active IDs,
    /// no restart occurs and the map ends empty.
    #[sqlx::test]
    async fn test_sync_prunes_finished_and_does_not_restart_deleted(pool: sqlx::PgPool) {
        let probe = create_active_probe(&pool, "Deleted probe").await;

        let config = create_test_config();
        let scheduler = ProbeScheduler::new(pool.clone(), config);

        // Simulate a scheduler that died and a probe that was then deleted.
        {
            let mut schedulers = scheduler.schedulers.write().await;
            schedulers.insert(probe.id, finished_join_handle().await);
        }
        ProbeManager::delete_probe(&pool, probe.id).await.unwrap();

        scheduler.sync_with_database(CancellationToken::new()).await.unwrap();

        assert_eq!(
            scheduler.schedulers.read().await.len(),
            0,
            "deleted probe's finished handle should be pruned and not restarted"
        );
    }
}
