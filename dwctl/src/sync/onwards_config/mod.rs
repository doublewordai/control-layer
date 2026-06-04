//! Configuration synchronization to onwards routing layer.

use std::{collections::HashMap, num::NonZeroU32, sync::Arc};

use metrics::histogram;
use onwards::target::{
    Auth, BackoffConfig as OnwardsBackoffConfig, ConcurrencyLimitParameters, ConfigFile, FallbackConfig as OnwardsFallbackConfig,
    JitterStrategy as OnwardsJitterStrategy, KeyDefinition, LoadBalanceStrategy as OnwardsLoadBalanceStrategy, OpenResponsesConfig,
    PoolSpec, ProviderSpec, RateLimitParameters, RoutingAction, RoutingRule, TargetSpecOrList, Targets, WatchTargetsStream,
};
use sqlx::{PgPool, postgres::PgListener};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, warn};

/// Status events for testing/observability
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncStatus {
    Connecting,
    Connected,
    Disconnected,
    Reconnecting,
}

use crate::{
    config::{ONWARDS_CONFIG_CHANGED_CHANNEL, RateLimitTiersConfig},
    db::models::deployments::LoadBalancingStrategy,
    types::{ApiKeyId, DeploymentId},
};

/// A parsed change notification from the `auth_config_changed` channel.
///
/// Backward-compatible across payload formats: the legacy `table:epoch` (no entity
/// id ⇒ full reload), the enriched `table:op:id:epoch`, and the JSON
/// `{table, operation, id, timestamp}` form emitted by some triggers.
#[derive(Debug, Clone)]
struct NotifyChange {
    table: String,
    /// `INSERT` / `UPDATE` / `DELETE`, when the payload carries it.
    #[allow(dead_code)]
    op: Option<String>,
    /// The change's scope entity id (a deployment id or a user id, depending on the
    /// table), used to scope the delta. `None` ⇒ fall back to a full reload.
    scope_id: Option<uuid::Uuid>,
    /// Time from the database change to receipt, for the lag metric.
    lag: std::time::Duration,
}

fn parse_notify_payload(payload: &str) -> Option<NotifyChange> {
    let now_micros = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_micros() as i64;
    let lag_from = |epoch: i64| std::time::Duration::from_micros(now_micros.saturating_sub(epoch).max(0) as u64);

    let trimmed = payload.trim();
    if trimmed.starts_with('{') {
        // JSON form: {"table": ..., "operation": ..., "id": ..., "timestamp": ...}
        let v: serde_json::Value = serde_json::from_str(trimmed).ok()?;
        let table = v.get("table")?.as_str()?.to_string();
        let op = v.get("operation").and_then(|o| o.as_str()).map(str::to_string);
        let scope_id = v.get("id").and_then(|i| i.as_str()).and_then(|s| uuid::Uuid::parse_str(s).ok());
        let lag = v
            .get("timestamp")
            .and_then(serde_json::Value::as_i64)
            .map(lag_from)
            .unwrap_or_default();
        return Some(NotifyChange { table, op, scope_id, lag });
    }

    match trimmed.split(':').collect::<Vec<_>>().as_slice() {
        // Legacy `table:epoch` — no entity id, so the caller does a full reload.
        [table, epoch] => Some(NotifyChange {
            table: (*table).to_string(),
            op: None,
            scope_id: None,
            lag: lag_from(epoch.parse().ok()?),
        }),
        // Enriched `table:op:id:epoch`.
        [table, op, id, epoch] => Some(NotifyChange {
            table: (*table).to_string(),
            op: Some((*op).to_string()),
            scope_id: uuid::Uuid::parse_str(id).ok(),
            lag: lag_from(epoch.parse().ok()?),
        }),
        _ => None,
    }
}

/// Complete data needed for one onwards target configuration
#[derive(Debug, Clone)]
struct OnwardsTarget {
    // Deployment info
    model_name: String,
    alias: String,
    requests_per_second: Option<f32>,
    burst_size: Option<i32>,
    capacity: Option<i32>,
    sanitize_responses: bool,
    trusted: bool,
    open_responses_adapter: bool,
    /// Traffic routing rules from the model_traffic_rules table
    routing_rules: Vec<RoutingRule>,

    // Fallback / backoff config. Standard (single-provider) models only retry
    // when fallback is on AND `with_replacement` is true (otherwise the
    // SelectIter yields exactly once). The backoff fields gate the
    // inter-attempt sleep onwards inserts when retries happen.
    fallback_enabled: bool,
    fallback_on_rate_limit: bool,
    fallback_on_status: Vec<i32>,
    fallback_with_replacement: bool,
    fallback_max_attempts: Option<i32>,
    backoff_enabled: bool,
    backoff_initial_ms: i32,
    backoff_max_ms: i32,
    backoff_factor: f64,
    backoff_jitter: String,
    backoff_max_total_ms: Option<i32>,

    // Endpoint info
    endpoint_url: url::Url,
    endpoint_api_key: Option<String>,
    auth_header_name: String,
    auth_header_prefix: String,

    // API keys that have access to this deployment
    api_keys: Vec<OnwardsApiKey>,
}

/// Minimal API key data needed for onwards config
#[derive(Debug, Clone)]
struct OnwardsApiKey {
    id: ApiKeyId,
    secret: String,
    purpose: String,
    requests_per_second: Option<f32>,
    burst_size: Option<i32>,
    /// `verified` flag on the api_key's owning user (api_keys.user_id), used to
    /// pick between the verified/unverified default rate-limit tiers when this
    /// key has no per-key override.
    user_verified: bool,
}

/// In-memory assembled routing state held by the sync task.
///
/// Holding the assembled state lets a change patch only the affected slice (via a
/// scoped re-query) instead of re-running the heavy `load_full_state` query on every
/// reload. [`assemble`] rebuilds the onwards [`Targets`] from this — pure, no DB — on
/// each send, so the dwctl→onwards handoff stays a free in-process channel swap.
#[derive(Debug, Clone, Default)]
struct TargetState {
    /// Regular (non-composite) targets, keyed by deployment id.
    regular: HashMap<DeploymentId, OnwardsTarget>,
    /// Composite models (each carries its own deployment id internally).
    composites: Vec<OnwardsCompositeModel>,
}

/// Manages the integration between onwards-pilot and the onwards proxy
pub struct OnwardsConfigSync {
    db: PgPool,
    sender: watch::Sender<Targets>,
    /// Shared map of model batch capacity limits for the daemon
    daemon_capacity_limits: Option<Arc<dashmap::DashMap<String, usize>>>,
    /// Default batch concurrency for models without explicit batch_capacity
    default_batch_capacity: usize,
    /// Model aliases that batch API keys should have automatic access to (escalation targets)
    escalation_models: Vec<String>,
    /// Tracks previous-cycle gauge label sets for zeroing stale metrics
    cache_info_state: crate::metrics::CacheInfoState,
    /// Enable strict mode with schema validation
    strict_mode: bool,
    /// Default rate-limit tiers applied to API keys based on the owning user's
    /// `verified` flag. Used when a key has no per-key override.
    rate_limit_tiers: RateLimitTiersConfig,
    /// Assembled in-memory routing state. Patched in place by scoped deltas (and
    /// fully reloaded by cold start / fallback / reconnect), then reassembled into
    /// `Targets` on each publish.
    state: TargetState,
}

pub struct SyncConfig {
    pub status_tx: Option<mpsc::Sender<SyncStatus>>,
    /// Fallback sync interval in milliseconds (default: 10000ms = 10 seconds)
    ///
    /// Provides periodic full syncs independent of LISTEN/NOTIFY to guarantee eventual consistency.
    /// Set to 0 to disable fallback sync (not recommended).
    pub fallback_interval_milliseconds: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            status_tx: None,
            fallback_interval_milliseconds: 10000, // 10 seconds
        }
    }
}

impl OnwardsConfigSync {
    /// Creates a new OnwardsConfigSync and returns it along with initial targets and a WatchTargetsStream
    #[cfg(test)]
    #[instrument(skip(db))]
    pub async fn new(db: PgPool) -> Result<(Self, Targets, WatchTargetsStream), anyhow::Error> {
        Self::new_with_daemon_limits(db, None, 10, Vec::new(), false, RateLimitTiersConfig::default()).await
    }

    /// Creates a new OnwardsConfigSync with optional daemon capacity limits map and escalation models
    ///
    /// `daemon_capacity_limits` - Shared map populated with per-model concurrency limits for the batch daemon.
    /// `default_batch_capacity` - Default concurrency limit for models without explicit `batch_capacity`.
    /// `escalation_models` - Model aliases that batch API keys should have automatic access to.
    /// `strict_mode` - Enable strict mode with schema validation (only known OpenAI API paths accepted)
    /// `rate_limit_tiers` - Default rate limits applied per-key based on the owning user's `verified` flag.
    #[instrument(skip(db, daemon_capacity_limits, escalation_models, rate_limit_tiers))]
    pub async fn new_with_daemon_limits(
        db: PgPool,
        daemon_capacity_limits: Option<Arc<dashmap::DashMap<String, usize>>>,
        default_batch_capacity: usize,
        escalation_models: Vec<String>,
        strict_mode: bool,
        rate_limit_tiers: RateLimitTiersConfig,
    ) -> Result<(Self, Targets, WatchTargetsStream), anyhow::Error> {
        // Load initial configuration (including composite models) into the in-memory
        // state, then assemble the first snapshot from it.
        let state = load_full_state(&db, &escalation_models, &[]).await?;
        let initial_targets = assemble(&state, strict_mode, &rate_limit_tiers)?;

        // If daemon limits are provided, populate them
        if let Some(ref limits) = daemon_capacity_limits {
            update_daemon_capacity_limits(&db, limits, default_batch_capacity).await?;
        }

        // Populate cache info metrics on startup
        let mut cache_info_state = crate::metrics::CacheInfoState::new();
        if let Err(e) = crate::metrics::update_cache_info_metrics(&db, &initial_targets, &mut cache_info_state).await {
            error!("Failed to update cache info metrics: {}", e);
        }

        // Create watch channel with initial state
        let (sender, receiver) = watch::channel(initial_targets.clone());

        let integration = Self {
            db,
            sender,
            daemon_capacity_limits,
            default_batch_capacity,
            escalation_models,
            cache_info_state,
            strict_mode,
            rate_limit_tiers,
            state,
        };
        let stream = WatchTargetsStream::new(receiver);

        Ok((integration, initial_targets, stream))
    }

    /// Get a clone of the sender for manual sync triggering
    pub fn sender(&self) -> watch::Sender<Targets> {
        self.sender.clone()
    }

    /// Reload routing state and return the assembled `Targets` for the caller to
    /// publish. An empty `scope` does a full reload; a non-empty `scope` re-queries
    /// only those deployments and replaces that slice of the in-memory state in
    /// place. Both paths run the same SQL, so a scoped patch yields the same result
    /// a full reload would. Also refreshes daemon capacity limits and cache metrics.
    async fn reload_into_state(&mut self, scope: &[DeploymentId]) -> Result<Targets, anyhow::Error> {
        if scope.is_empty() {
            self.state = load_full_state(&self.db, &self.escalation_models, &[]).await?;
        } else {
            let slice = load_full_state(&self.db, &self.escalation_models, scope).await?;
            let affected: std::collections::HashSet<DeploymentId> = scope.iter().copied().collect();
            // Drop the affected deployments, then re-insert whatever still exists
            // (deleted / now-inaccessible deployments simply don't come back).
            self.state.regular.retain(|id, _| !affected.contains(id));
            self.state.composites.retain(|c| !affected.contains(&c.id));
            self.state.regular.extend(slice.regular);
            self.state.composites.extend(slice.composites);
        }

        let new_targets = assemble(&self.state, self.strict_mode, &self.rate_limit_tiers)?;

        if let Some(ref limits) = self.daemon_capacity_limits
            && let Err(e) = update_daemon_capacity_limits(&self.db, limits, self.default_batch_capacity).await
        {
            error!("Failed to update daemon capacity limits: {}", e);
        }

        if let Err(e) = crate::metrics::update_cache_info_metrics(&self.db, &new_targets, &mut self.cache_info_state).await {
            error!("Failed to update cache info metrics: {}", e);
        }

        Ok(new_targets)
    }

    /// Map a change notification to the deployments it affects, so the reload can be
    /// scoped to that slice. An empty result requests a full reload — used when the
    /// payload carries no scope id, for system-wide changes, and for tables whose
    /// impact can't be narrowed. Over-scoping is safe (extra deployments just get
    /// re-queried); under-scoping is not, so anything uncertain falls back to a full
    /// reload, and the periodic fallback is the ultimate self-heal.
    async fn resolve_change_scope(&self, change: Option<&NotifyChange>) -> Vec<DeploymentId> {
        let Some(change) = change else {
            return Vec::new();
        };
        let Some(scope_id) = change.scope_id else {
            return Vec::new();
        };
        match change.table.as_str() {
            // A deployed_models change (endpoint, rates, flags) is embedded by any composite
            // that uses it as a component, so re-query the deployment AND its parent composites.
            "deployed_models" => {
                let mut scope = vec![scope_id];
                match self.composites_for_component(scope_id).await {
                    Ok(parents) => scope.extend(parents),
                    Err(e) => {
                        error!("Failed to resolve composites for component {scope_id}: {e}; full reload");
                        return Vec::new();
                    }
                }
                scope
            }
            // The scope id is itself a deployment id: re-query just that deployment.
            // (The inference_endpoints trigger emits one notify per deployment on the
            // endpoint, so the scope id is already a deployment id for that table too.)
            "deployment_groups" | "model_tariffs" | "model_traffic_rules" | "deployed_model_components" | "inference_endpoints" => {
                vec![scope_id]
            }
            // The scope id is a user id: re-query every deployment that user can reach,
            // since a key/membership/balance change alters those deployments' key lists.
            // (`credits_transactions` is the app-level balance-crossing notify.) The
            // system user (nil uuid) reaches everything, so fall back to a full reload.
            "api_keys" | "user_groups" | "user_organizations" | "credits_transactions" | "users" if scope_id != uuid::Uuid::nil() => {
                match self.deployments_for_user(scope_id).await {
                    Ok(ids) => ids,
                    Err(e) => {
                        error!("Failed to resolve deployments for user {scope_id}: {e}; doing a full reload");
                        Vec::new()
                    }
                }
            }
            _ => Vec::new(),
        }
    }

    /// All deployments a user can reach — group-assigned, public, or batch-escalation
    /// targets. Ignores balance (the scoped re-query re-applies the balance gate) and
    /// deliberately over-includes, so a key/membership change is never missed.
    async fn deployments_for_user(&self, user_id: uuid::Uuid) -> Result<Vec<DeploymentId>, anyhow::Error> {
        let ids = sqlx::query_scalar!(
            r#"
            SELECT dm.id
            FROM deployed_models dm
            WHERE dm.deleted = FALSE
              AND (
                EXISTS (
                    SELECT 1 FROM user_groups ug
                    INNER JOIN deployment_groups dg ON ug.group_id = dg.group_id
                    WHERE dg.deployment_id = dm.id AND ug.user_id = $1
                )
                OR EXISTS (
                    SELECT 1 FROM deployment_groups dg
                    WHERE dg.deployment_id = dm.id
                      AND dg.group_id = '00000000-0000-0000-0000-000000000000'
                )
                OR dm.alias = ANY($2::text[])
              )
            "#,
            user_id,
            &self.escalation_models
        )
        .fetch_all(&self.db)
        .await?;
        Ok(ids)
    }

    /// Composite deployments that embed `component_id` as an enabled component. They copy the
    /// component's config (endpoint, rate limits, flags), so a change to the component must
    /// also re-query its parent composites — otherwise they'd serve stale component config.
    async fn composites_for_component(&self, component_id: DeploymentId) -> Result<Vec<DeploymentId>, anyhow::Error> {
        let ids = sqlx::query_scalar!(
            r#"
            SELECT DISTINCT composite_model_id
            FROM deployed_model_components
            WHERE deployed_model_id = $1 AND enabled = TRUE
            "#,
            component_id
        )
        .fetch_all(&self.db)
        .await?;
        Ok(ids)
    }

    /// Starts the background task that listens for database changes and updates the configuration
    #[instrument(skip(self, config, shutdown_token), err)]
    pub async fn start(mut self, config: SyncConfig, shutdown_token: CancellationToken) -> Result<(), anyhow::Error> {
        // Debouncing: prevent rapid-fire reloads
        let mut last_reload_time = std::time::Instant::now();
        const MIN_RELOAD_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

        // The constructor already loaded the initial state, so the first connect skips
        // the resync; each reconnect does a full resync to catch changes that arrived
        // while the listener was down (those NOTIFYs were missed).
        let mut first_connect = true;

        // Content hash of the routing-config tables as of our last successful sync. The
        // periodic fallback recomputes it and skips the full reload when unchanged —
        // balance and structural changes both arrive via NOTIFY (deltas) now, so the
        // full reload is only needed to catch a genuinely missed change.
        let mut last_config_hash: Option<String> = None;

        // Fallback sync interval (0 = disabled)
        let fallback_interval = if config.fallback_interval_milliseconds > 0 {
            Some(std::time::Duration::from_millis(config.fallback_interval_milliseconds))
        } else {
            None
        };

        'outer: loop {
            if let Some(tx) = &config.status_tx {
                tx.send(SyncStatus::Connecting).await?;
            }
            let mut listener = PgListener::connect_with(&self.db).await?;
            // Listen to auth config changes
            listener.listen(ONWARDS_CONFIG_CHANGED_CHANNEL).await?;

            if let Some(tx) = &config.status_tx {
                tx.send(SyncStatus::Connected).await?;
            }
            info!("Started onwards configuration listener");

            if first_connect {
                first_connect = false;
            } else {
                info!("Reconnected to database — running a full resync to catch missed changes");
                match self.reload_into_state(&[]).await {
                    Ok(targets) => {
                        if self.sender.send(targets).is_err() {
                            error!("Resync send failed (receivers dropped); exiting sync task");
                            break 'outer;
                        }
                        last_config_hash = config_content_hash(&self.db).await.ok();
                    }
                    Err(e) => error!("Resync after reconnect failed: {}", e),
                }
            }

            // Create fallback sync timer (if enabled)
            let mut fallback_timer = fallback_interval.map(|interval| {
                let mut timer = tokio::time::interval(interval);
                // Use Delay to avoid burst of syncs after runtime hiccups
                timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                timer
            });

            // Listen for notifications with graceful shutdown
            loop {
                tokio::select! {
                    // Handle shutdown signal
                    _ = shutdown_token.cancelled() => {
                        info!("Received shutdown signal, stopping onwards configuration listener");
                        break 'outer;
                    }

                    // Handle database notifications
                    notification_result = listener.try_recv() => {
                        debug!("Received notification from database");
                        match notification_result {
                            Ok(None) => {
                                info!("Connection lost, attempting to reconnect");
                                if let Some(tx) = &config.status_tx {
                                    debug!("Sending Disconnected status");
                                    tx.send(SyncStatus::Disconnected).await?;
                                }
                                // Try to reconnect for other errors
                                if let Some(tx) = &config.status_tx {
                                    debug!("Sending Reconnecting status");
                                    tx.send(SyncStatus::Reconnecting).await?;
                                }
                                break;

                            },
                            Ok(Some(notification)) => {
                                debug!("Received notification on channel: {} with payload: {:?}",
                                      notification.channel(), notification.payload());

                                // Parse the change notification (table + optional scope id + lag).
                                let change = parse_notify_payload(notification.payload());
                                if change.is_none() {
                                    debug!(
                                        "Unparseable NOTIFY payload, falling back to a full reload: {:?}",
                                        notification.payload()
                                    );
                                }

                                // Debounce: skip if we just reloaded recently
                                if last_reload_time.elapsed() < MIN_RELOAD_INTERVAL {
                                    debug!("Skipping reload due to debouncing (last reload was {:?} ago)",
                                           last_reload_time.elapsed());
                                    continue;
                                }

                                // Reload — scoped to the affected deployments when the
                                // payload identifies them, otherwise a full reload.
                                last_reload_time = std::time::Instant::now();
                                let scope = self.resolve_change_scope(change.as_ref()).await;
                                match self.reload_into_state(&scope).await {
                                    Ok(new_targets) => {
                                        debug!(
                                            "Reloaded {} targets ({})",
                                            new_targets.targets.len(),
                                            if scope.is_empty() { "full" } else { "delta" }
                                        );

                                        // Send update through watch channel
                                        if let Err(e) = self.sender.send(new_targets) {
                                            error!("Failed to send targets update: {}", e);
                                            // If all receivers are dropped, we can exit
                                            break;
                                        }

                                        // Record metric for LISTEN/NOTIFY sync
                                        metrics::counter!("dwctl_cache_sync_total", "source" => "listen_notify").increment(1);

                                        // Record cache sync lag metric (time from DB change to cache update)
                                        if let Some(ref c) = change {
                                            let lag_seconds = c.lag.as_secs_f64();
                                            histogram!("dwctl_cache_sync_lag_seconds", "table" => c.table.clone())
                                                .record(lag_seconds);
                                            info!("Updated onwards configuration successfully (sync lag: {:.3}ms from {})",
                                                  lag_seconds * 1000.0, c.table);
                                        } else {
                                            info!("Updated onwards configuration successfully");
                                        }

                                        // Remember the config we just synced so the periodic
                                        // fallback can skip the full reload until it changes again.
                                        last_config_hash = config_content_hash(&self.db).await.ok();
                                    }
                                    Err(e) => {
                                        error!("Failed to load targets from database: {}", e);
                                        // Return error if database operations fail consistently
                                        if e.to_string().contains("closed pool") || e.to_string().contains("connection closed") {
                                            error!("Database pool closed, exiting sync task");
                                            return Err(e);
                                        }
                                        // Continue listening for other types of errors
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Error receiving notification: {}", e);
                                if let Some(tx) = &config.status_tx {
                                    tx.send(SyncStatus::Disconnected).await?;
                                }
                                // Try to reconnect for other errors
                                if let Some(tx) = &config.status_tx {
                                    tx.send(SyncStatus::Reconnecting).await?;
                                }

                                // Check if this is a fatal error that should propagate
                                if e.to_string().contains("closed pool") || e.to_string().contains("connection closed") {
                                    error!("Database connection closed, exiting sync task");
                                    return Err(e.into());
                                }
                                break;
                            }
                        }
                    }

                    // Fallback periodic sync (if enabled)
                    _ = async {
                        match &mut fallback_timer {
                            Some(timer) => timer.tick().await,
                            None => std::future::pending().await, // Never resolve if disabled
                        }
                    } => {
                        debug!("Fallback periodic sync triggered");

                        // Skip if we just reloaded via notification (debounce)
                        if last_reload_time.elapsed() < MIN_RELOAD_INTERVAL {
                            debug!("Skipping fallback sync due to recent notification-triggered reload");
                            continue;
                        }

                        last_reload_time = std::time::Instant::now();

                        // Hash-gate the fallback: only run the expensive full reload when the
                        // routing config actually changed since our last sync. Balance and
                        // structural changes arrive via NOTIFY (deltas), so in steady state the
                        // hash is unchanged and the full reload is skipped entirely.
                        match config_content_hash(&self.db).await {
                            Ok(hash) if last_config_hash.as_deref() == Some(hash.as_str()) => {
                                debug!("Fallback sync: config unchanged, skipping full reload");
                                metrics::counter!("dwctl_cache_sync_total", "source" => "fallback_skipped").increment(1);
                            }
                            hash_result => {
                                // Hash changed (a missed change) or couldn't be computed — do
                                // the full reload to be safe.
                                match self.reload_into_state(&[]).await {
                                    Ok(new_targets) => {
                                        debug!("Fallback sync: reloaded {} targets", new_targets.targets.len());
                                        if let Err(e) = self.sender.send(new_targets) {
                                            error!("Failed to send targets update: {}", e);
                                            // If all receivers are dropped, we can exit
                                            break;
                                        }
                                        metrics::counter!("dwctl_cache_sync_total", "source" => "fallback").increment(1);
                                        debug!("Fallback sync: updated onwards configuration successfully");
                                        last_config_hash = hash_result.ok();
                                    }
                                    Err(e) => {
                                        error!("Fallback sync: failed to load targets from database: {}", e);
                                        metrics::counter!("dwctl_cache_sync_errors_total", "source" => "fallback").increment(1);
                                        // Continue - fallback sync errors shouldn't crash the service
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        info!("Onwards configuration listener stopped gracefully");
        Ok(())
    }
}

// ===== Composite Models Support =====
// Composite models are virtual models that distribute requests across multiple
// underlying deployed models based on configurable weights.
//
// Composite models are stored in the deployed_models table with is_composite = TRUE.
// They have NULL hosted_on and instead have components in deployed_model_components.

/// Data structure for composite model components (prepared for onwards integration)
#[derive(Debug, Clone)]
struct CompositeModelComponent {
    weight: i32,
    // Component target info (from the underlying deployed_model)
    target: OnwardsTarget,
}

/// Data structure for composite models (prepared for onwards integration)
#[derive(Debug, Clone)]
struct OnwardsCompositeModel {
    #[allow(dead_code)] // Useful for debug logging
    id: DeploymentId,
    alias: String,
    requests_per_second: Option<f32>,
    burst_size: Option<i32>,
    capacity: Option<i32>,
    /// Load balancing strategy (weighted_random or priority)
    lb_strategy: LoadBalancingStrategy,
    /// Fallback enabled
    fallback_enabled: bool,
    /// Fallback on rate limit
    fallback_on_rate_limit: bool,
    /// HTTP status codes that trigger fallback
    fallback_on_status: Vec<i32>,
    /// Sample with replacement during weighted random failover
    fallback_with_replacement: bool,
    /// Maximum number of failover attempts
    fallback_max_attempts: Option<i32>,
    /// Inter-attempt exponential backoff configuration. When `backoff_enabled`
    /// is false, the legacy zero-delay retry behavior is preserved.
    backoff_enabled: bool,
    backoff_initial_ms: i32,
    backoff_max_ms: i32,
    backoff_factor: f64,
    backoff_jitter: String,
    /// Optional cumulative budget cap across inter-attempt sleeps.
    /// Independent of `backoff_enabled` semantics; only consulted when
    /// backoff is enabled.
    backoff_max_total_ms: Option<i32>,
    /// Whether to sanitize/filter sensitive data from model responses
    sanitize_responses: bool,
    /// Whether to mark provider as trusted in strict mode
    #[allow(dead_code)] // Stored in DB but composite-level trust is not yet propagated to onwards
    trusted: bool,
    /// Whether to enable the open_responses adapter at the pool level
    open_responses_adapter: bool,
    /// Traffic routing rules from the database
    routing_rules: Vec<RoutingRule>,
    components: Vec<CompositeModelComponent>,
    // API keys that have access to this composite model
    api_keys: Vec<OnwardsApiKey>,
}

/// Loads composite models with their components and API keys from the database
#[tracing::instrument(skip(db, escalation_models))]
async fn load_composite_models_from_db(
    db: &PgPool,
    escalation_models: &[String],
    deployment_filter: &[DeploymentId],
) -> Result<Vec<OnwardsCompositeModel>, anyhow::Error> {
    debug!(
        "Loading composite models from database (escalation_models: {:?})",
        escalation_models
    );

    // Query composite models (deployed_models with is_composite = TRUE) with their components
    let component_rows = sqlx::query!(
        r#"
        SELECT
            cm.id as composite_model_id,
            cm.alias,
            cm.requests_per_second,
            cm.burst_size,
            cm.capacity,
            cm.lb_strategy,
            cm.fallback_enabled,
            cm.fallback_on_rate_limit,
            cm.fallback_on_status,
            cm.fallback_with_replacement,
            cm.fallback_max_attempts,
            cm.sanitize_responses as composite_sanitize_responses,
            cm.trusted as composite_trusted,
            cm.open_responses_adapter as "composite_open_responses_adapter?",
            -- Component info
            dmc.deployed_model_id,
            dmc.weight,
            -- Underlying deployment info
            dm.model_name,
            dm.alias as deployment_alias,
            dm.requests_per_second as deployment_requests_per_second,
            dm.burst_size as deployment_burst_size,
            dm.capacity as deployment_capacity,
            dm.sanitize_responses as deployment_sanitize_responses,
            dm.trusted as deployment_trusted,
            dm.open_responses_adapter as "deployment_open_responses_adapter?",
            -- Endpoint info
            ie.url as "endpoint_url!",
            ie.api_key as endpoint_api_key,
            ie.auth_header_name,
            ie.auth_header_prefix
        FROM deployed_models cm
        INNER JOIN deployed_model_components dmc ON cm.id = dmc.composite_model_id
        INNER JOIN deployed_models dm ON dmc.deployed_model_id = dm.id
        INNER JOIN inference_endpoints ie ON dm.hosted_on = ie.id
        WHERE cm.is_composite = TRUE
          AND cm.deleted = FALSE
          AND dmc.enabled = TRUE
          AND dm.deleted = FALSE
          AND (cardinality($1::uuid[]) = 0 OR cm.id = ANY($1::uuid[]))
        ORDER BY cm.id, dmc.sort_order ASC
        "#,
        deployment_filter
    )
    .fetch_all(db)
    .await?;

    // Query API keys with access to composite models (uses deployment_groups since composites are in deployed_models)
    let api_key_rows = sqlx::query!(
        r#"
        WITH user_balances AS (
            SELECT
                u.id as user_id,
                COALESCE(c.balance, 0) + COALESCE(
                    (SELECT SUM(
                        CASE WHEN ct.transaction_type IN ('purchase', 'admin_grant')
                        THEN ct.amount ELSE -ct.amount END
                    )
                    FROM credits_transactions ct
                    WHERE ct.user_id = u.id
                    AND ct.seq > COALESCE(c.checkpoint_seq, 0)),
                    0
                ) as balance
            FROM users u
            LEFT JOIN user_balance_checkpoints c ON c.user_id = u.id
        )
        SELECT
            cm.id as composite_model_id,
            ak.id as api_key_id,
            ak.secret as api_key_secret,
            ak.purpose as api_key_purpose,
            ak.requests_per_second,
            ak.burst_size,
            ak.user_verified
        FROM deployed_models cm
        CROSS JOIN LATERAL (
            SELECT DISTINCT
                ak.id,
                ak.secret,
                ak.purpose,
                ak.requests_per_second,
                ak.burst_size,
                u.verified as user_verified
            FROM api_keys ak
            JOIN users u ON u.id = ak.user_id
            WHERE (
                -- System user always has access
                ak.user_id = '00000000-0000-0000-0000-000000000000'
                -- OR user is in a group assigned to this composite model (via deployment_groups)
                OR EXISTS (
                    SELECT 1 FROM user_groups ug
                    INNER JOIN deployment_groups dg ON ug.group_id = dg.group_id
                    WHERE dg.deployment_id = cm.id
                      AND ug.user_id = ak.user_id
                )
                -- OR composite model is in public group (nil UUID)
                OR EXISTS (
                    SELECT 1 FROM deployment_groups dg
                    WHERE dg.deployment_id = cm.id
                      AND dg.group_id = '00000000-0000-0000-0000-000000000000'
                )
                -- OR this is a batch API key and composite model is an escalation target
                OR (
                    ak.purpose = 'batch'
                    AND cm.alias = ANY($1::text[])
                )
            )
            -- Require positive balance OR free model (system user always passes)
            AND (
                ak.user_id = '00000000-0000-0000-0000-000000000000'
                OR EXISTS (
                    SELECT 1 FROM user_balances ub
                    WHERE ub.user_id = ak.user_id AND ub.balance > 0
                )
                OR (
                    NOT EXISTS (
                        SELECT 1 FROM model_tariffs mt
                        WHERE mt.deployed_model_id = cm.id
                          AND mt.valid_until IS NULL
                          AND (mt.input_price_per_token > 0 OR mt.output_price_per_token > 0)
                    )
                )
            )
            AND ak.is_deleted = false
        ) ak
        WHERE cm.is_composite = TRUE
          AND cm.deleted = FALSE
          AND (cardinality($2::uuid[]) = 0 OR cm.id = ANY($2::uuid[]))
        ORDER BY cm.id, ak.id
        "#,
        escalation_models,
        deployment_filter
    )
    .fetch_all(db)
    .await?;

    // Query all composite model metadata (regardless of component status).
    // This ensures composites with zero enabled components still appear in the
    // routing table so that auth and routing rules (e.g., redirects) can run.
    let composite_metadata_rows = sqlx::query!(
        r#"
        SELECT
            id as composite_model_id,
            alias,
            requests_per_second,
            burst_size,
            capacity,
            lb_strategy,
            fallback_enabled,
            fallback_on_rate_limit,
            fallback_on_status,
            fallback_with_replacement,
            fallback_max_attempts,
            backoff_enabled,
            backoff_initial_ms,
            backoff_max_ms,
            backoff_factor,
            backoff_jitter,
            backoff_max_total_ms,
            sanitize_responses,
            trusted,
            open_responses_adapter as "open_responses_adapter?"
        FROM deployed_models
        WHERE is_composite = TRUE
          AND deleted = FALSE
          AND (cardinality($1::uuid[]) = 0 OR id = ANY($1::uuid[]))
        "#,
        deployment_filter
    )
    .fetch_all(db)
    .await?;

    // Seed the composite map with all composite models (even those with no enabled components)
    let mut composite_map: HashMap<DeploymentId, OnwardsCompositeModel> = HashMap::new();
    for row in composite_metadata_rows {
        let lb_strategy = row
            .lb_strategy
            .as_deref()
            .and_then(LoadBalancingStrategy::try_parse)
            .unwrap_or_default();

        composite_map.insert(
            row.composite_model_id,
            OnwardsCompositeModel {
                id: row.composite_model_id,
                alias: row.alias,
                requests_per_second: row.requests_per_second,
                burst_size: row.burst_size,
                capacity: row.capacity,
                lb_strategy,
                fallback_enabled: row.fallback_enabled.unwrap_or(true),
                fallback_on_rate_limit: row.fallback_on_rate_limit.unwrap_or(true),
                fallback_on_status: row.fallback_on_status.unwrap_or_else(|| vec![429, 500, 502, 503, 504]),
                fallback_with_replacement: row.fallback_with_replacement.unwrap_or(false),
                fallback_max_attempts: row.fallback_max_attempts,
                backoff_enabled: row.backoff_enabled,
                backoff_initial_ms: row.backoff_initial_ms,
                backoff_max_ms: row.backoff_max_ms,
                backoff_factor: row.backoff_factor,
                backoff_jitter: row.backoff_jitter,
                backoff_max_total_ms: row.backoff_max_total_ms,
                sanitize_responses: row.sanitize_responses,
                trusted: row.trusted,
                open_responses_adapter: row.open_responses_adapter.unwrap_or(true),
                routing_rules: Vec::new(), // Populated from separate query below
                components: Vec::new(),
                api_keys: Vec::new(),
            },
        );
    }

    // Add enabled components to their composite models
    for row in component_rows {
        // Parse the endpoint URL, skipping this component if invalid
        let endpoint_url = match url::Url::parse(&row.endpoint_url) {
            Ok(url) => url,
            Err(e) => {
                warn!(
                    "Skipping component for composite model '{}': invalid endpoint URL '{}': {}",
                    row.alias, row.endpoint_url, e
                );
                continue;
            }
        };

        if let Some(composite) = composite_map.get_mut(&row.composite_model_id) {
            composite.components.push(CompositeModelComponent {
                weight: row.weight,
                target: OnwardsTarget {
                    model_name: row.model_name.clone(),
                    alias: row.deployment_alias.clone(),
                    requests_per_second: row.deployment_requests_per_second,
                    burst_size: row.deployment_burst_size,
                    capacity: row.deployment_capacity,
                    sanitize_responses: row.deployment_sanitize_responses,
                    trusted: row.deployment_trusted,
                    open_responses_adapter: row.deployment_open_responses_adapter.unwrap_or(true),
                    routing_rules: Vec::new(), // Components don't have their own routing rules
                    // Components don't surface their own fallback/backoff —
                    // the composite's PoolSpec.fallback drives retries across
                    // the whole pool.
                    fallback_enabled: false,
                    fallback_on_rate_limit: false,
                    fallback_on_status: Vec::new(),
                    fallback_with_replacement: false,
                    fallback_max_attempts: None,
                    backoff_enabled: false,
                    backoff_initial_ms: 100,
                    backoff_max_ms: 5_000,
                    backoff_factor: 2.0,
                    backoff_jitter: "full".to_string(),
                    backoff_max_total_ms: None,
                    endpoint_url,
                    endpoint_api_key: row.endpoint_api_key.clone(),
                    auth_header_name: row.auth_header_name.clone(),
                    auth_header_prefix: row.auth_header_prefix.clone(),
                    api_keys: Vec::new(),
                },
            });
        }
    }

    // Add API keys to composite models
    for row in api_key_rows {
        if let Some(composite) = composite_map.get_mut(&row.composite_model_id) {
            // Avoid duplicates
            if !composite.api_keys.iter().any(|k| k.id == row.api_key_id) {
                composite.api_keys.push(OnwardsApiKey {
                    id: row.api_key_id,
                    secret: row.api_key_secret,
                    purpose: row.api_key_purpose.clone(),
                    requests_per_second: row.requests_per_second,
                    burst_size: row.burst_size,
                    user_verified: row.user_verified,
                });
            }
        }
    }

    let composites: Vec<_> = composite_map.into_values().collect();
    debug!(
        "Loaded {} composite models with {} total components",
        composites.len(),
        composites.iter().map(|c| c.components.len()).sum::<usize>()
    );

    Ok(composites)
}

/// Converts a composite model to a TargetSpecOrList with weighted providers
///
/// Uses onwards 0.10.0 weighted provider types for load balancing across
/// multiple underlying deployed models.
fn convert_composite_to_target_spec(
    composite: &OnwardsCompositeModel,
    key_definitions: &mut HashMap<String, KeyDefinition>,
    rate_limit_tiers: &RateLimitTiersConfig,
) -> (String, TargetSpecOrList) {
    // Add this composite model's API keys to key_definitions
    for api_key in &composite.api_keys {
        // The system key (nil UUID) carries internal traffic and is never tiered.
        let rate_limit = if api_key.id.is_nil() {
            None
        } else {
            resolve_key_rate_limit(
                api_key.requests_per_second,
                api_key.burst_size,
                api_key.user_verified,
                rate_limit_tiers,
            )
        };

        let labels = HashMap::from([("purpose".to_string(), api_key.purpose.clone())]);

        key_definitions.insert(
            api_key.id.to_string(),
            KeyDefinition {
                key: api_key.secret.clone(),
                rate_limit,
                concurrency_limit: None,
                labels,
            },
        );
    }

    // Get API key secrets for access control
    let keys = if composite.api_keys.is_empty() {
        None
    } else {
        Some(composite.api_keys.iter().map(|k| k.secret.clone().into()).collect())
    };

    // Build pool-level rate limiting
    let rate_limit = match (composite.requests_per_second, composite.burst_size) {
        (Some(rps), burst) if rps > 0.0 => {
            let rps_u32 = NonZeroU32::new((rps.max(1.0) as u32).max(1)).unwrap();
            let burst_u32 = burst.and_then(|b| NonZeroU32::new(b.max(1) as u32));
            debug!(
                "Composite model '{}' configured with {}req/s rate limit, burst: {:?}",
                composite.alias, rps, burst_u32
            );
            Some(RateLimitParameters {
                requests_per_second: rps_u32,
                burst_size: burst_u32,
            })
        }
        _ => None,
    };

    // Build pool-level concurrency limiting
    let concurrency_limit = composite.capacity.map(|capacity| {
        debug!(
            "Composite model '{}' configured with {} max concurrent requests",
            composite.alias, capacity
        );
        ConcurrencyLimitParameters {
            max_concurrent_requests: capacity as usize,
        }
    });

    // Convert our LoadBalancingStrategy to onwards LoadBalanceStrategy
    let strategy = match composite.lb_strategy {
        LoadBalancingStrategy::WeightedRandom => OnwardsLoadBalanceStrategy::WeightedRandom,
        LoadBalancingStrategy::Priority => OnwardsLoadBalanceStrategy::Priority,
    };

    // Build fallback configuration
    let fallback = if composite.fallback_enabled {
        let backoff = composite.backoff_enabled.then_some(OnwardsBackoffConfig {
            // The DB CHECK constraints guarantee these are positive and
            // ordered, but we use `max(1)` defensively to avoid panicking
            // the proxy if a row ever escaped validation.
            initial_ms: composite.backoff_initial_ms.max(1) as u64,
            max_ms: composite.backoff_max_ms.max(composite.backoff_initial_ms.max(1)) as u64,
            factor: composite.backoff_factor.max(1.0),
            jitter: match composite.backoff_jitter.as_str() {
                "none" => OnwardsJitterStrategy::None,
                _ => OnwardsJitterStrategy::Full,
            },
        });
        let max_total_backoff_ms = composite.backoff_max_total_ms.and_then(|n| u64::try_from(n).ok());
        Some(OnwardsFallbackConfig {
            enabled: true,
            on_rate_limit: composite.fallback_on_rate_limit,
            // Convert i32 status codes to u16 for onwards
            on_status: composite.fallback_on_status.iter().map(|&s| s as u16).collect(),
            with_replacement: composite.fallback_with_replacement,
            max_attempts: composite
                .fallback_max_attempts
                .and_then(|n| usize::try_from(n).ok().filter(|&v| v >= 1)),
            backoff,
            max_total_backoff_ms,
        })
    } else {
        None
    };

    // Build provider specs from components
    let providers: Vec<ProviderSpec> = composite
        .components
        .iter()
        .map(|component| {
            let target = &component.target;

            // Build provider-level rate limiting (from underlying deployment)
            let provider_rate_limit = match (target.requests_per_second, target.burst_size) {
                (Some(rps), burst) if rps > 0.0 => {
                    let rps_u32 = NonZeroU32::new((rps.max(1.0) as u32).max(1)).unwrap();
                    let burst_u32 = burst.and_then(|b| NonZeroU32::new(b.max(1) as u32));
                    Some(RateLimitParameters {
                        requests_per_second: rps_u32,
                        burst_size: burst_u32,
                    })
                }
                _ => None,
            };

            // Build provider-level concurrency limiting
            let provider_concurrency_limit = target.capacity.map(|capacity| ConcurrencyLimitParameters {
                max_concurrent_requests: capacity as usize,
            });

            {
                debug!(
                    "  Provider '{}' ({}): weight={}, sanitize_response={}, trusted={}",
                    target.alias, target.model_name, component.weight, composite.sanitize_responses, target.trusted
                );
                ProviderSpec {
                    url: target.endpoint_url.clone(),
                    onwards_key: target.endpoint_api_key.clone(),
                    onwards_model: Some(target.model_name.clone()),
                    weight: component.weight.max(1) as u32,
                    rate_limit: provider_rate_limit,
                    concurrency_limit: provider_concurrency_limit,
                    upstream_auth_header_name: if target.auth_header_name != "Authorization" {
                        Some(target.auth_header_name.clone())
                    } else {
                        None
                    },
                    upstream_auth_header_prefix: if target.auth_header_prefix != "Bearer " {
                        Some(target.auth_header_prefix.clone())
                    } else {
                        None
                    },
                    response_headers: None,
                    // For composite models, use the composite model's sanitize_responses setting
                    // This ensures the virtual model's toggle controls all providers
                    sanitize_response: composite.sanitize_responses,
                    open_responses: Some(OpenResponsesConfig {
                        adapter: target.open_responses_adapter,
                    }),
                    request_timeout_secs: None,
                    // Each provider uses its own trusted setting from the database
                    // This allows fine-grained control over which providers bypass error sanitization
                    trusted: Some(target.trusted),
                }
            }
        })
        .collect();

    debug!(
        "Composite model '{}' configured with {} providers, strategy: {:?}, fallback: {}, sanitize_responses: {}",
        composite.alias,
        providers.len(),
        strategy,
        composite.fallback_enabled,
        composite.sanitize_responses
    );

    // Create PoolSpec with weighted providers
    // Note: trusted is not set at the pool level for composite models
    // Each provider uses its own trusted setting via ProviderSpec.trusted
    let pool_spec = PoolSpec {
        keys,
        rate_limit,
        concurrency_limit,
        fallback,
        strategy,
        providers,
        response_headers: None,
        sanitize_response: composite.sanitize_responses,
        trusted: false, // Pool-level trusted defaults to false; providers set their own
        open_responses: Some(OpenResponsesConfig {
            adapter: composite.open_responses_adapter,
        }),
        routing_rules: composite.routing_rules.clone(),
    };

    (composite.alias.clone(), TargetSpecOrList::Pool(pool_spec))
}

/// Resolves the rate limit for an API key. A non-NULL per-key
/// `requests_per_second` always wins; otherwise we fall back to the
/// verified/unverified tier defaults from config, which may themselves be unset
/// (legacy "no limit unless overridden" behaviour).
fn resolve_key_rate_limit(
    per_key_rps: Option<f32>,
    per_key_burst: Option<i32>,
    user_verified: bool,
    tiers: &RateLimitTiersConfig,
) -> Option<RateLimitParameters> {
    let (rps, burst) = match per_key_rps {
        Some(rps) if rps > 0.0 => (rps, per_key_burst),
        _ => {
            let tier = if user_verified {
                tiers.verified.as_ref()
            } else {
                tiers.unverified.as_ref()
            };
            let tier = tier?;
            (tier.requests_per_second, tier.burst_size)
        }
    };

    let rps_u32 = NonZeroU32::new((rps.max(1.0) as u32).max(1))?;
    let burst_u32 = burst.and_then(|b| NonZeroU32::new(b.max(1) as u32));
    Some(RateLimitParameters {
        requests_per_second: rps_u32,
        burst_size: burst_u32,
    })
}

/// Converts both regular targets and composite models to ConfigFile format
#[tracing::instrument(skip(targets, composites, rate_limit_tiers))]
fn convert_to_config_file(
    targets: Vec<OnwardsTarget>,
    composites: Vec<OnwardsCompositeModel>,
    strict_mode: bool,
    rate_limit_tiers: &RateLimitTiersConfig,
) -> ConfigFile {
    let mut key_definitions = HashMap::new();

    // Convert regular deployed models (wrapped in TargetSpecOrList::Pool)
    let mut target_specs: HashMap<String, TargetSpecOrList> = targets
        .into_iter()
        .map(|target| {
            // Add this target's API keys to key_definitions
            for api_key in &target.api_keys {
                // The system key (nil UUID) carries internal traffic and is never tiered.
                let rate_limit = if api_key.id.is_nil() {
                    None
                } else {
                    resolve_key_rate_limit(
                        api_key.requests_per_second,
                        api_key.burst_size,
                        api_key.user_verified,
                        rate_limit_tiers,
                    )
                };

                // Build labels from API key purpose
                let labels = HashMap::from([("purpose".to_string(), api_key.purpose.clone())]);

                key_definitions.insert(
                    api_key.id.to_string(),
                    KeyDefinition {
                        key: api_key.secret.clone(),
                        rate_limit,
                        concurrency_limit: None,
                        labels,
                    },
                );
            }

            let keys = if target.api_keys.is_empty() {
                None
            } else {
                Some(target.api_keys.iter().map(|k| k.secret.clone().into()).collect())
            };

            let rate_limit = match (target.requests_per_second, target.burst_size) {
                (Some(rps), burst) if rps > 0.0 => {
                    let rps_u32 = NonZeroU32::new((rps.max(1.0) as u32).max(1)).unwrap();
                    let burst_u32 = burst.and_then(|b| NonZeroU32::new(b.max(1) as u32));
                    Some(RateLimitParameters {
                        requests_per_second: rps_u32,
                        burst_size: burst_u32,
                    })
                }
                _ => None,
            };

            let upstream_auth_header_name = if target.auth_header_name != "Authorization" {
                Some(target.auth_header_name.clone())
            } else {
                None
            };
            let upstream_auth_header_prefix = if target.auth_header_prefix != "Bearer " {
                Some(target.auth_header_prefix.clone())
            } else {
                None
            };

            let concurrency_limit = target.capacity.map(|capacity| ConcurrencyLimitParameters {
                max_concurrent_requests: capacity as usize,
            });

            // Build provider spec from target
            let provider = ProviderSpec {
                url: target.endpoint_url.clone(),
                onwards_key: target.endpoint_api_key.clone(),
                onwards_model: Some(target.model_name.clone()),
                rate_limit,
                concurrency_limit,
                upstream_auth_header_name,
                upstream_auth_header_prefix,
                response_headers: None,
                weight: 1,
                sanitize_response: target.sanitize_responses,
                open_responses: Some(OpenResponsesConfig {
                    adapter: target.open_responses_adapter,
                }),
                request_timeout_secs: None,
                trusted: Some(target.trusted),
            };

            // Build fallback configuration. For single-provider (standard)
            // models the SelectIter only yields more than once when
            // `with_replacement` is true; the backoff fields control the
            // inter-attempt sleep when retries do happen.
            let fallback = if target.fallback_enabled {
                let backoff = target.backoff_enabled.then_some(OnwardsBackoffConfig {
                    initial_ms: target.backoff_initial_ms.max(1) as u64,
                    max_ms: target.backoff_max_ms.max(target.backoff_initial_ms.max(1)) as u64,
                    factor: target.backoff_factor.max(1.0),
                    jitter: match target.backoff_jitter.as_str() {
                        "none" => OnwardsJitterStrategy::None,
                        _ => OnwardsJitterStrategy::Full,
                    },
                });
                let max_total_backoff_ms = target.backoff_max_total_ms.and_then(|n| u64::try_from(n).ok());
                Some(OnwardsFallbackConfig {
                    enabled: true,
                    on_rate_limit: target.fallback_on_rate_limit,
                    on_status: target.fallback_on_status.iter().map(|&s| s as u16).collect(),
                    with_replacement: target.fallback_with_replacement,
                    max_attempts: target
                        .fallback_max_attempts
                        .and_then(|n| usize::try_from(n).ok().filter(|&v| v >= 1)),
                    backoff,
                    max_total_backoff_ms,
                })
            } else {
                None
            };

            // Use PoolSpec so routing_rules are carried through
            let pool_spec = PoolSpec {
                keys,
                rate_limit: None,
                concurrency_limit: None,
                fallback,
                strategy: OnwardsLoadBalanceStrategy::default(),
                providers: vec![provider],
                response_headers: None,
                open_responses: None,
                sanitize_response: target.sanitize_responses,
                trusted: false,
                routing_rules: target.routing_rules,
            };

            (target.alias, TargetSpecOrList::Pool(pool_spec))
        })
        .collect();

    // Convert composite models (including those with no components - they'll return 503)
    for composite in composites {
        if composite.components.is_empty() {
            warn!(
                "Composite model '{}' has no enabled components - requests will return 503",
                composite.alias
            );
        }

        let (alias, spec) = convert_composite_to_target_spec(&composite, &mut key_definitions, rate_limit_tiers);
        target_specs.insert(alias, spec);
    }

    let auth = if key_definitions.is_empty() {
        None
    } else {
        Some(
            Auth::builder()
                .global_keys(std::collections::HashSet::new())
                .key_definitions(key_definitions)
                .build(),
        )
    };

    ConfigFile {
        targets: target_specs,
        auth,
        strict_mode,
        http_pool: None,
    }
}

/// Loads the complete routing state from the database (regular + composite models,
/// with routing rules attached).
///
/// This is the heavy query — the ledger-summing balance gate × the per-model key
/// cross-product. The sync runs it on cold start and the periodic self-heal resync
/// only; steady-state changes patch [`TargetState`] via scoped deltas instead.
///
/// `escalation_models` - Model aliases that batch API keys should have automatic access to.
#[tracing::instrument(skip(db, escalation_models))]
async fn load_full_state(
    db: &PgPool,
    escalation_models: &[String],
    deployment_filter: &[DeploymentId],
) -> Result<TargetState, anyhow::Error> {
    let query_start = std::time::Instant::now();
    debug!("Loading onwards targets from database (with composite models)");

    // Load regular deployed models (existing logic)
    // Note: We pass escalation_models to grant batch API keys access to escalation models
    let rows = sqlx::query!(
        r#"
        WITH user_balances AS (
            SELECT
                u.id as user_id,
                COALESCE(c.balance, 0) + COALESCE(
                    (SELECT SUM(
                        CASE WHEN ct.transaction_type IN ('purchase', 'admin_grant')
                        THEN ct.amount ELSE -ct.amount END
                    )
                    FROM credits_transactions ct
                    WHERE ct.user_id = u.id
                    AND ct.seq > COALESCE(c.checkpoint_seq, 0)),
                    0
                ) as balance
            FROM users u
            LEFT JOIN user_balance_checkpoints c ON c.user_id = u.id
        )
        SELECT
            dm.id as deployment_id,
            dm.model_name,
            dm.alias,
            dm.hosted_on,
            dm.requests_per_second as deployment_requests_per_second,
            dm.burst_size as deployment_burst_size,
            dm.capacity,
            dm.sanitize_responses,
            dm.trusted,
            dm.open_responses_adapter,
            dm.fallback_enabled,
            dm.fallback_on_rate_limit,
            dm.fallback_on_status,
            dm.fallback_with_replacement,
            dm.fallback_max_attempts,
            dm.backoff_enabled,
            dm.backoff_initial_ms,
            dm.backoff_max_ms,
            dm.backoff_factor,
            dm.backoff_jitter,
            dm.backoff_max_total_ms,
            ie.id as endpoint_id,
            ie.url as "endpoint_url!",
            ie.api_key as endpoint_api_key,
            ie.auth_header_name,
            ie.auth_header_prefix,
            ak.id as "api_key_id?",
            ak.secret as "api_key_secret?",
            ak.purpose as "api_key_purpose?",
            ak.requests_per_second as api_key_requests_per_second,
            ak.burst_size as api_key_burst_size,
            ak.user_verified as "api_key_user_verified?"
        FROM deployed_models dm
        INNER JOIN inference_endpoints ie ON dm.hosted_on = ie.id
        LEFT JOIN LATERAL (
            SELECT DISTINCT
                ak.id,
                ak.secret,
                ak.purpose,
                ak.requests_per_second,
                ak.burst_size,
                u.verified as user_verified
            FROM api_keys ak
            JOIN users u ON u.id = ak.user_id
            WHERE (
                -- System user always has access
                ak.user_id = '00000000-0000-0000-0000-000000000000'
                -- OR user is in a group assigned to this model
                OR EXISTS (
                    SELECT 1 FROM user_groups ug
                    INNER JOIN deployment_groups dg ON ug.group_id = dg.group_id
                    WHERE dg.deployment_id = dm.id
                      AND ug.user_id = ak.user_id
                )
                -- OR model is in public group
                OR EXISTS (
                    SELECT 1 FROM deployment_groups dg
                    WHERE dg.deployment_id = dm.id
                      AND dg.group_id = '00000000-0000-0000-0000-000000000000'
                )
                -- OR this is a batch API key and model is an escalation target
                OR (
                    ak.purpose = 'batch'
                    AND dm.alias = ANY($1::text[])
                )
            )
            AND (
                ak.user_id = '00000000-0000-0000-0000-000000000000'
                OR EXISTS (
                    SELECT 1 FROM user_balances ub
                    WHERE ub.user_id = ak.user_id AND ub.balance > 0
                )
                OR (
                    NOT EXISTS (
                        SELECT 1 FROM model_tariffs mt
                        WHERE mt.deployed_model_id = dm.id
                          AND mt.valid_until IS NULL
                          AND (mt.input_price_per_token > 0 OR mt.output_price_per_token > 0)
                    )
                )
            )
            AND ak.is_deleted = false
        ) ak ON true
        WHERE dm.deleted = FALSE
          AND dm.is_composite = FALSE
          AND (cardinality($2::uuid[]) = 0 OR dm.id = ANY($2::uuid[]))
        ORDER BY dm.id, ak.id
        "#,
        escalation_models,
        deployment_filter
    )
    .fetch_all(db)
    .await?;

    let query_duration = query_start.elapsed();
    debug!(
        "Regular (non-composite) deployed models query completed in {:?}, fetched {} rows",
        query_duration,
        rows.len()
    );

    // Group results into targets
    let mut targets_map: HashMap<DeploymentId, OnwardsTarget> = HashMap::new();
    for row in rows {
        let deployment_id = row.deployment_id;
        let target = targets_map.entry(deployment_id).or_insert_with(|| {
            OnwardsTarget {
                model_name: row.model_name.clone(),
                alias: row.alias.clone(),
                requests_per_second: row.deployment_requests_per_second,
                burst_size: row.deployment_burst_size,
                capacity: row.capacity,
                sanitize_responses: row.sanitize_responses,
                trusted: row.trusted,
                open_responses_adapter: row.open_responses_adapter.unwrap_or(true),
                routing_rules: Vec::new(), // Populated from separate query below
                fallback_enabled: row.fallback_enabled.unwrap_or(true),
                fallback_on_rate_limit: row.fallback_on_rate_limit.unwrap_or(true),
                fallback_on_status: row.fallback_on_status.clone().unwrap_or_else(|| vec![429, 500, 502, 503, 504]),
                fallback_with_replacement: row.fallback_with_replacement.unwrap_or(false),
                fallback_max_attempts: row.fallback_max_attempts,
                backoff_enabled: row.backoff_enabled,
                backoff_initial_ms: row.backoff_initial_ms,
                backoff_max_ms: row.backoff_max_ms,
                backoff_factor: row.backoff_factor,
                backoff_jitter: row.backoff_jitter.clone(),
                backoff_max_total_ms: row.backoff_max_total_ms,
                endpoint_url: url::Url::parse(&row.endpoint_url).expect("Invalid URL in database"),
                endpoint_api_key: row.endpoint_api_key.clone(),
                auth_header_name: row.auth_header_name.clone(),
                auth_header_prefix: row.auth_header_prefix.clone(),
                api_keys: Vec::new(),
            }
        });

        // user_verified is Option only because of the outer LEFT JOIN; whenever the
        // lateral subquery emits a row, the inner JOIN to users guarantees it. We
        // tie it to the same "row materialised" check as the other api_key columns
        // so a future schema/SQL change can't silently demote keys to the
        // unverified tier.
        if let (Some(api_key_id), Some(api_key_secret), Some(api_key_purpose), Some(user_verified)) =
            (row.api_key_id, row.api_key_secret, row.api_key_purpose, row.api_key_user_verified)
        {
            target.api_keys.push(OnwardsApiKey {
                id: api_key_id,
                secret: api_key_secret,
                purpose: api_key_purpose,
                requests_per_second: row.api_key_requests_per_second,
                burst_size: row.api_key_burst_size,
                user_verified,
            });
        }
    }

    debug!("Loaded {} deployed models", targets_map.len());

    // Load composite models (pass escalation_models to grant batch API keys access)
    let composites = load_composite_models_from_db(db, escalation_models, deployment_filter).await?;

    // Load traffic routing rules for all non-deleted models (regular + composite)
    let traffic_rule_rows = sqlx::query!(
        r#"
        SELECT mtr.deployed_model_id, mtr.api_key_purpose, mtr.action,
               dm.alias as "redirect_target_alias?"
        FROM model_traffic_rules mtr
        LEFT JOIN deployed_models dm ON dm.id = mtr.redirect_target_id
        WHERE mtr.deployed_model_id IN (
            SELECT id FROM deployed_models WHERE deleted = FALSE
        )
          AND (cardinality($1::uuid[]) = 0 OR mtr.deployed_model_id = ANY($1::uuid[]))
        ORDER BY mtr.deployed_model_id, mtr.api_key_purpose
        "#,
        deployment_filter
    )
    .fetch_all(db)
    .await?;

    // Build a map of deployment_id → routing rules
    let mut routing_rules_map: HashMap<DeploymentId, Vec<RoutingRule>> = HashMap::new();
    for rule_row in traffic_rule_rows {
        let routing_rule = RoutingRule {
            match_labels: HashMap::from([("purpose".to_string(), rule_row.api_key_purpose)]),
            action: match rule_row.action.as_str() {
                "deny" => RoutingAction::Deny,
                "redirect" => RoutingAction::Redirect {
                    target: rule_row.redirect_target_alias.unwrap_or_default(),
                },
                _ => continue,
            },
        };
        routing_rules_map.entry(rule_row.deployed_model_id).or_default().push(routing_rule);
    }

    // Attach routing rules to regular targets
    for (deployment_id, target) in &mut targets_map {
        if let Some(rules) = routing_rules_map.remove(deployment_id) {
            target.routing_rules = rules;
        }
    }

    // Attach routing rules to composite models
    let composites: Vec<_> = composites
        .into_iter()
        .map(|mut c| {
            if let Some(rules) = routing_rules_map.remove(&c.id) {
                c.routing_rules = rules;
            }
            c
        })
        .collect();

    Ok(TargetState {
        regular: targets_map,
        composites,
    })
}

/// Assembles the in-memory [`TargetState`] into the onwards [`Targets`] config.
///
/// Pure (no DB): clones the state and runs the existing conversion, so it can be
/// called on every send after an in-memory delta patch.
fn assemble(state: &TargetState, strict_mode: bool, rate_limit_tiers: &RateLimitTiersConfig) -> Result<Targets, anyhow::Error> {
    let targets: Vec<_> = state.regular.values().cloned().collect();
    let composites = state.composites.clone();
    let config = convert_to_config_file(targets, composites, strict_mode, rate_limit_tiers);
    Targets::from_config(config)
}

/// Cheap content hash over the small routing-config tables (NOT the 28M-row credits
/// ledger), used by the periodic fallback to skip the full reload when nothing has
/// changed. `row::text` captures every column, so the hash changes on any real content
/// change but is stable across no-op upserts (same content ⇒ same text). Ordering by the
/// row text makes it deterministic without assuming a primary key.
async fn config_content_hash(db: &PgPool) -> Result<String, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        SELECT md5(
            coalesce((SELECT string_agg(dm::text, ',' ORDER BY dm::text) FROM deployed_models dm), '') ||
            coalesce((SELECT string_agg(ak::text, ',' ORDER BY ak::text) FROM api_keys ak), '') ||
            coalesce((SELECT string_agg(ie::text, ',' ORDER BY ie::text) FROM inference_endpoints ie), '') ||
            coalesce((SELECT string_agg(dg::text, ',' ORDER BY dg::text) FROM deployment_groups dg), '') ||
            coalesce((SELECT string_agg(ug::text, ',' ORDER BY ug::text) FROM user_groups ug), '') ||
            coalesce((SELECT string_agg(mt::text, ',' ORDER BY mt::text) FROM model_tariffs mt), '') ||
            coalesce((SELECT string_agg(mtr::text, ',' ORDER BY mtr::text) FROM model_traffic_rules mtr), '') ||
            coalesce((SELECT string_agg(dmc::text, ',' ORDER BY dmc::text) FROM deployed_model_components dmc), '') ||
            -- users: only id + verified (the column that drives the rate-limit tier), not the
            -- whole row, to avoid thrashing the hash on volatile columns like last_login.
            coalesce((SELECT string_agg(u.id::text || ':' || u.verified::text, ',' ORDER BY u.id::text) FROM users u), '')
        ) AS "hash!"
        "#
    )
    .fetch_one(db)
    .await
}

/// Full reload helper: load the complete state and assemble it in one call.
///
/// Production code goes through [`OnwardsConfigSync::reload_into_state`], which keeps
/// the in-memory state for scoped deltas; this one-shot is retained for tests that
/// assert on a freshly-built `Targets`.
#[cfg(test)]
#[tracing::instrument(skip(db, escalation_models, rate_limit_tiers))]
pub async fn load_targets_from_db(
    db: &PgPool,
    escalation_models: &[String],
    strict_mode: bool,
    rate_limit_tiers: &RateLimitTiersConfig,
) -> Result<Targets, anyhow::Error> {
    let state = load_full_state(db, escalation_models, &[]).await?;
    assemble(&state, strict_mode, rate_limit_tiers)
}

/// Updates the daemon capacity limits DashMap from deployed_models.
///
/// Every non-deleted deployed model gets an entry: explicit `batch_capacity` if set,
/// otherwise `default_capacity`. This ensures the daemon will claim requests for all
/// deployed models, not just those with an explicit batch_capacity override.
async fn update_daemon_capacity_limits(
    db: &PgPool,
    limits: &Arc<dashmap::DashMap<String, usize>>,
    default_capacity: usize,
) -> Result<(), anyhow::Error> {
    let models = sqlx::query!(
        r#"
        SELECT alias, batch_capacity
        FROM deployed_models
        WHERE deleted = FALSE
        "#
    )
    .fetch_all(db)
    .await?;

    let mut active_models = std::collections::HashSet::new();

    for model in &models {
        let capacity = match model.batch_capacity {
            Some(c) if c > 0 => c as usize,
            Some(c) => {
                warn!(
                    alias = %model.alias,
                    batch_capacity = c,
                    "Invalid non-positive batch_capacity; using default_capacity"
                );
                default_capacity
            }
            None => default_capacity,
        };
        active_models.insert(model.alias.clone());
        limits.insert(model.alias.clone(), capacity);
        debug!("Updated daemon capacity limit for model '{}': {}", model.alias, capacity);
    }

    // Remove limits for models that were deleted
    limits.retain(|model_alias, _| active_models.contains(model_alias));

    debug!("Updated {} model capacity limits for daemon", limits.len());
    Ok(())
}

#[cfg(test)]
mod tests;
