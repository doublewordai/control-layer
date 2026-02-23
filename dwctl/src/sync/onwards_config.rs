//! Configuration synchronization to onwards routing layer.

use std::{collections::HashMap, num::NonZeroU32, sync::Arc};

use metrics::histogram;
use onwards::target::{
    Auth, ConcurrencyLimitParameters, ConfigFile, FallbackConfig as OnwardsFallbackConfig, KeyDefinition,
    LoadBalanceStrategy as OnwardsLoadBalanceStrategy, OpenResponsesConfig, PoolSpec, ProviderSpec, RateLimitParameters, TargetSpec,
    TargetSpecOrList, Targets, WatchTargetsStream,
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
    config::ONWARDS_CONFIG_CHANGED_CHANNEL,
    db::models::deployments::LoadBalancingStrategy,
    types::{ApiKeyId, DeploymentId},
};

/// Parse the NOTIFY payload to extract the timestamp
/// Payload format: "table_name:epoch_microseconds"
/// Returns the table name and the elapsed time since the notification was sent
fn parse_notify_payload(payload: &str) -> Option<(&str, std::time::Duration)> {
    let parts: Vec<&str> = payload.split(':').collect();
    if parts.len() != 2 {
        return None;
    }
    let table_name = parts[0];
    let epoch_micros: i64 = parts[1].parse().ok()?;

    // Calculate elapsed time since the notification was sent
    let now_micros = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_micros() as i64;

    let lag_micros = now_micros.saturating_sub(epoch_micros);
    Some((table_name, std::time::Duration::from_micros(lag_micros as u64)))
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
    requests_per_second: Option<f32>,
    burst_size: Option<i32>,
}

/// Manages the integration between onwards-pilot and the onwards proxy
pub struct OnwardsConfigSync {
    db: PgPool,
    sender: watch::Sender<Targets>,
    /// Shared map of model batch capacity limits for the daemon
    daemon_capacity_limits: Option<Arc<dashmap::DashMap<String, usize>>>,
    /// Model aliases that batch API keys should have automatic access to (escalation targets)
    escalation_models: Vec<String>,
    /// Tracks previous-cycle gauge label sets for zeroing stale metrics
    cache_info_state: crate::metrics::CacheInfoState,
    /// Enable strict mode with schema validation
    strict_mode: bool,
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
        Self::new_with_daemon_limits(db, None, Vec::new(), false).await
    }

    /// Creates a new OnwardsConfigSync with optional daemon capacity limits map and escalation models
    ///
    /// `escalation_models` - Model aliases that batch API keys should have automatic access to.
    /// This enables batch processing to route requests to escalation models without needing
    /// separate API key configuration.
    /// `strict_mode` - Enable strict mode with schema validation (only known OpenAI API paths accepted)
    #[instrument(skip(db, daemon_capacity_limits, escalation_models))]
    pub async fn new_with_daemon_limits(
        db: PgPool,
        daemon_capacity_limits: Option<Arc<dashmap::DashMap<String, usize>>>,
        escalation_models: Vec<String>,
        strict_mode: bool,
    ) -> Result<(Self, Targets, WatchTargetsStream), anyhow::Error> {
        // Load initial configuration (including composite models)
        let initial_targets = load_targets_from_db(&db, &escalation_models, strict_mode).await?;

        // If daemon limits are provided, populate them
        if let Some(ref limits) = daemon_capacity_limits {
            update_daemon_capacity_limits(&db, limits).await?;
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
            escalation_models,
            cache_info_state,
            strict_mode,
        };
        let stream = WatchTargetsStream::new(receiver);

        Ok((integration, initial_targets, stream))
    }

    /// Get a clone of the sender for manual sync triggering
    pub fn sender(&self) -> watch::Sender<Targets> {
        self.sender.clone()
    }

    /// Starts the background task that listens for database changes and updates the configuration
    #[instrument(skip(self, config, shutdown_token), err)]
    pub async fn start(mut self, config: SyncConfig, shutdown_token: CancellationToken) -> Result<(), anyhow::Error> {
        // Debouncing: prevent rapid-fire reloads
        let mut last_reload_time = std::time::Instant::now();
        const MIN_RELOAD_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

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
                        info!("Received notification from database");
                        match notification_result {
                            Ok(None) => {
                                info!("Connection lost, attempting to reconnect");
                                if let Some(tx) = &config.status_tx {
                                    info!("Sending Disconnected status");
                                    tx.send(SyncStatus::Disconnected).await?;
                                }
                                // Try to reconnect for other errors
                                if let Some(tx) = &config.status_tx {
                                    info!("Sending Reconnecting status");
                                    tx.send(SyncStatus::Reconnecting).await?;
                                }
                                break;

                            },
                            Ok(Some(notification)) => {
                                debug!("Received notification on channel: {} with payload: {:?}",
                                      notification.channel(), notification.payload());

                                // Parse the notification timestamp for lag measurement
                                let notify_info = parse_notify_payload(notification.payload());

                                // Debounce: skip if we just reloaded recently
                                if last_reload_time.elapsed() < MIN_RELOAD_INTERVAL {
                                    debug!("Skipping reload due to debouncing (last reload was {:?} ago)",
                                           last_reload_time.elapsed());
                                    continue;
                                }

                                // Reload configuration from database (including composite models)
                                last_reload_time = std::time::Instant::now();
                                match load_targets_from_db(&self.db, &self.escalation_models, self.strict_mode).await {
                                    Ok(new_targets) => {
                                        info!("Loaded {} targets from database", new_targets.targets.len());
                                        for entry in new_targets.targets.iter() {
                                            let alias = entry.key();
                                            debug!("Target '{}' loaded", alias);
                                        }

                                        // Update daemon capacity limits if configured
                                        if let Some(ref limits) = self.daemon_capacity_limits
                                            && let Err(e) = update_daemon_capacity_limits(&self.db, limits).await {
                                                error!("Failed to update daemon capacity limits: {}", e);
                                            }

                                        // Update cache info metrics
                                        if let Err(e) = crate::metrics::update_cache_info_metrics(&self.db, &new_targets, &mut self.cache_info_state).await {
                                            error!("Failed to update cache info metrics: {}", e);
                                        }

                                        // Send update through watch channel
                                        if let Err(e) = self.sender.send(new_targets) {
                                            error!("Failed to send targets update: {}", e);
                                            // If all receivers are dropped, we can exit
                                            break;
                                        }

                                        // Record metric for LISTEN/NOTIFY sync
                                        metrics::counter!("dwctl_cache_sync_total", "source" => "listen_notify").increment(1);

                                        // Record cache sync lag metric (time from DB change to cache update)
                                        if let Some((table_name, lag)) = notify_info {
                                            let lag_seconds = lag.as_secs_f64();
                                            histogram!("dwctl_cache_sync_lag_seconds", "table" => table_name.to_string())
                                                .record(lag_seconds);
                                            info!("Updated onwards configuration successfully (sync lag: {:.3}ms from {})",
                                                  lag_seconds * 1000.0, table_name);
                                        } else {
                                            info!("Updated onwards configuration successfully");
                                        }
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
                        match load_targets_from_db(&self.db, &self.escalation_models, self.strict_mode).await {
                            Ok(new_targets) => {
                                info!("Fallback sync: loaded {} targets from database", new_targets.targets.len());

                                // Update daemon capacity limits if configured
                                if let Some(ref limits) = self.daemon_capacity_limits
                                    && let Err(e) = update_daemon_capacity_limits(&self.db, limits).await {
                                        error!("Failed to update daemon capacity limits: {}", e);
                                    }

                                // Update cache info metrics
                                if let Err(e) = crate::metrics::update_cache_info_metrics(&self.db, &new_targets, &mut self.cache_info_state).await {
                                    error!("Failed to update cache info metrics: {}", e);
                                }

                                // Send update through watch channel
                                if let Err(e) = self.sender.send(new_targets) {
                                    error!("Failed to send targets update: {}", e);
                                    // If all receivers are dropped, we can exit
                                    break;
                                }

                                // Record metric for fallback sync
                                metrics::counter!("dwctl_cache_sync_total", "source" => "fallback").increment(1);
                                info!("Fallback sync: updated onwards configuration successfully");
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
    /// Whether to sanitize/filter sensitive data from model responses
    sanitize_responses: bool,
    /// Whether to mark provider as trusted in strict mode
    #[allow(dead_code)] // Stored in DB but composite-level trust is not yet propagated to onwards
    trusted: bool,
    /// Whether to enable the open_responses adapter at the pool level
    open_responses_adapter: bool,
    components: Vec<CompositeModelComponent>,
    // API keys that have access to this composite model
    api_keys: Vec<OnwardsApiKey>,
}

/// Loads composite models with their components and API keys from the database
#[tracing::instrument(skip(db, escalation_models))]
async fn load_composite_models_from_db(db: &PgPool, escalation_models: &[String]) -> Result<Vec<OnwardsCompositeModel>, anyhow::Error> {
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
        ORDER BY cm.id, dmc.sort_order ASC
        "#
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
            ak.requests_per_second,
            ak.burst_size
        FROM deployed_models cm
        CROSS JOIN LATERAL (
            SELECT DISTINCT
                ak.id,
                ak.secret,
                ak.requests_per_second,
                ak.burst_size
            FROM api_keys ak
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
            -- Require positive balance (system user always passes)
            AND (
                ak.user_id = '00000000-0000-0000-0000-000000000000'
                OR EXISTS (
                    SELECT 1 FROM user_balances ub
                    WHERE ub.user_id = ak.user_id AND ub.balance > 0
                )
            )
        ) ak
        WHERE cm.is_composite = TRUE
          AND cm.deleted = FALSE
        ORDER BY cm.id, ak.id
        "#,
        escalation_models
    )
    .fetch_all(db)
    .await?;

    // Group components by composite model
    let mut composite_map: HashMap<DeploymentId, OnwardsCompositeModel> = HashMap::new();

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

        let composite = composite_map.entry(row.composite_model_id).or_insert_with(|| {
            // Parse lb_strategy from string, defaulting to WeightedRandom
            let lb_strategy = row
                .lb_strategy
                .as_deref()
                .and_then(LoadBalancingStrategy::try_parse)
                .unwrap_or_default();

            OnwardsCompositeModel {
                id: row.composite_model_id,
                alias: row.alias.clone(),
                requests_per_second: row.requests_per_second,
                burst_size: row.burst_size,
                capacity: row.capacity,
                lb_strategy,
                fallback_enabled: row.fallback_enabled.unwrap_or(true),
                fallback_on_rate_limit: row.fallback_on_rate_limit.unwrap_or(true),
                fallback_on_status: row.fallback_on_status.clone().unwrap_or_else(|| vec![429, 500, 502, 503, 504]),
                fallback_with_replacement: row.fallback_with_replacement.unwrap_or(false),
                fallback_max_attempts: row.fallback_max_attempts,
                sanitize_responses: row.composite_sanitize_responses,
                trusted: row.composite_trusted,
                open_responses_adapter: row.composite_open_responses_adapter.unwrap_or(true),
                components: Vec::new(),
                api_keys: Vec::new(),
            }
        });

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
                endpoint_url,
                endpoint_api_key: row.endpoint_api_key.clone(),
                auth_header_name: row.auth_header_name.clone(),
                auth_header_prefix: row.auth_header_prefix.clone(),
                api_keys: Vec::new(),
            },
        });
    }

    // Add API keys to composite models
    for row in api_key_rows {
        if let Some(composite) = composite_map.get_mut(&row.composite_model_id) {
            // Avoid duplicates
            if !composite.api_keys.iter().any(|k| k.id == row.api_key_id) {
                composite.api_keys.push(OnwardsApiKey {
                    id: row.api_key_id,
                    secret: row.api_key_secret,
                    requests_per_second: row.requests_per_second,
                    burst_size: row.burst_size,
                });
            }
        }
    }

    let composites: Vec<_> = composite_map.into_values().collect();
    info!(
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
) -> (String, TargetSpecOrList) {
    // Add this composite model's API keys to key_definitions
    for api_key in &composite.api_keys {
        let rate_limit = match (api_key.requests_per_second, api_key.burst_size) {
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

        key_definitions.insert(
            api_key.id.to_string(),
            KeyDefinition {
                key: api_key.secret.clone(),
                rate_limit,
                concurrency_limit: None,
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
        Some(OnwardsFallbackConfig {
            enabled: true,
            on_rate_limit: composite.fallback_on_rate_limit,
            // Convert i32 status codes to u16 for onwards
            on_status: composite.fallback_on_status.iter().map(|&s| s as u16).collect(),
            with_replacement: composite.fallback_with_replacement,
            max_attempts: composite
                .fallback_max_attempts
                .and_then(|n| usize::try_from(n).ok().filter(|&v| v >= 1)),
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
                    request_timeout_secs: None,
                    open_responses: Some(OpenResponsesConfig {
                        adapter: target.open_responses_adapter,
                    }),
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
    };

    (composite.alias.clone(), TargetSpecOrList::Pool(pool_spec))
}

/// Converts both regular targets and composite models to ConfigFile format
#[tracing::instrument(skip(targets, composites))]
fn convert_to_config_file(targets: Vec<OnwardsTarget>, composites: Vec<OnwardsCompositeModel>, strict_mode: bool) -> ConfigFile {
    let mut key_definitions = HashMap::new();

    // Convert regular deployed models (wrapped in TargetSpecOrList::Single)
    let mut target_specs: HashMap<String, TargetSpecOrList> = targets
        .into_iter()
        .map(|target| {
            // Add this target's API keys to key_definitions
            for api_key in &target.api_keys {
                let rate_limit = match (api_key.requests_per_second, api_key.burst_size) {
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

                key_definitions.insert(
                    api_key.id.to_string(),
                    KeyDefinition {
                        key: api_key.secret.clone(),
                        rate_limit,
                        concurrency_limit: None,
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

            let target_spec = TargetSpec {
                url: target.endpoint_url.clone(),
                keys,
                onwards_key: target.endpoint_api_key.clone(),
                onwards_model: Some(target.model_name.clone()),
                rate_limit,
                concurrency_limit,
                upstream_auth_header_name,
                upstream_auth_header_prefix,
                response_headers: None,
                weight: 1, // Default weight for single-provider targets
                sanitize_response: target.sanitize_responses,
                trusted: target.trusted,
                request_timeout_secs: None,
                open_responses: Some(OpenResponsesConfig {
                    adapter: target.open_responses_adapter,
                }),
            };

            (target.alias, TargetSpecOrList::Single(target_spec))
        })
        .collect();

    // Convert composite models (including those with no components - they'll return 502)
    for composite in composites {
        if composite.components.is_empty() {
            debug!("Composite model '{}' has no enabled components - will return 502", composite.alias);
        }

        let (alias, spec) = convert_composite_to_target_spec(&composite, &mut key_definitions);
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

/// Loads the current targets configuration from the database (including composite models)
///
/// `escalation_models` - Model aliases that batch API keys should have automatic access to.
/// This enables batch processing to route requests to escalation models without needing
/// separate API key configuration.
/// `strict_mode` - Enable strict mode with schema validation (only known OpenAI API paths accepted)
#[tracing::instrument(skip(db, escalation_models))]
pub async fn load_targets_from_db(db: &PgPool, escalation_models: &[String], strict_mode: bool) -> Result<Targets, anyhow::Error> {
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
            ie.id as endpoint_id,
            ie.url as "endpoint_url!",
            ie.api_key as endpoint_api_key,
            ie.auth_header_name,
            ie.auth_header_prefix,
            ak.id as "api_key_id?",
            ak.secret as "api_key_secret?",
            ak.requests_per_second as api_key_requests_per_second,
            ak.burst_size as api_key_burst_size
        FROM deployed_models dm
        INNER JOIN inference_endpoints ie ON dm.hosted_on = ie.id
        LEFT JOIN LATERAL (
            SELECT DISTINCT
                ak.id,
                ak.secret,
                ak.requests_per_second,
                ak.burst_size
            FROM api_keys ak
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
        ) ak ON true
        WHERE dm.deleted = FALSE
          AND dm.is_composite = FALSE
        ORDER BY dm.id, ak.id
        "#,
        escalation_models
    )
    .fetch_all(db)
    .await?;

    let query_duration = query_start.elapsed();
    info!(
        "Regular (non-composite) deployed models query completed in {:?}, fetched {} rows",
        query_duration,
        rows.len()
    );

    // Group results into targets
    let mut targets_map: HashMap<DeploymentId, OnwardsTarget> = HashMap::new();
    for row in rows {
        let deployment_id = row.deployment_id;
        let target = targets_map.entry(deployment_id).or_insert_with(|| OnwardsTarget {
            model_name: row.model_name.clone(),
            alias: row.alias.clone(),
            requests_per_second: row.deployment_requests_per_second,
            burst_size: row.deployment_burst_size,
            capacity: row.capacity,
            sanitize_responses: row.sanitize_responses,
            trusted: row.trusted,
            open_responses_adapter: row.open_responses_adapter.unwrap_or(true),
            endpoint_url: url::Url::parse(&row.endpoint_url).expect("Invalid URL in database"),
            endpoint_api_key: row.endpoint_api_key.clone(),
            auth_header_name: row.auth_header_name.clone(),
            auth_header_prefix: row.auth_header_prefix.clone(),
            api_keys: Vec::new(),
        });

        if let (Some(api_key_id), Some(api_key_secret)) = (row.api_key_id, row.api_key_secret) {
            target.api_keys.push(OnwardsApiKey {
                id: api_key_id,
                secret: api_key_secret,
                requests_per_second: row.api_key_requests_per_second,
                burst_size: row.api_key_burst_size,
            });
        }
    }

    let targets: Vec<_> = targets_map.into_values().collect();
    info!("Loaded {} deployed models", targets.len());

    // Load composite models (pass escalation_models to grant batch API keys access)
    let composites = load_composite_models_from_db(db, escalation_models).await?;

    // Convert to ConfigFile format
    let config = convert_to_config_file(targets, composites, strict_mode);

    // Convert ConfigFile to Targets
    Targets::from_config(config)
}

/// Updates the daemon capacity limits DashMap with batch_capacity values from deployed_models
/// Atomically updates the map without clearing it to avoid a window with no limits
async fn update_daemon_capacity_limits(db: &PgPool, limits: &Arc<dashmap::DashMap<String, usize>>) -> Result<(), anyhow::Error> {
    // Query all models with their batch_capacity (including nulls to know what to remove)
    let models = sqlx::query!(
        r#"
        SELECT alias, batch_capacity
        FROM deployed_models
        WHERE deleted = FALSE
        "#
    )
    .fetch_all(db)
    .await?;

    // Build a set of models that should have limits
    let mut models_with_limits = std::collections::HashSet::new();

    // Insert/update limits for models with batch_capacity
    for model in &models {
        if let Some(batch_capacity) = model.batch_capacity {
            models_with_limits.insert(model.alias.clone());
            limits.insert(model.alias.clone(), batch_capacity as usize);
            debug!("Updated daemon capacity limit for model '{}': {}", model.alias, batch_capacity);
        }
    }

    // Remove limits for models that no longer have batch_capacity or were deleted
    limits.retain(|model_alias, _| models_with_limits.contains(model_alias));

    info!("Updated {} model capacity limits for daemon", limits.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, time::Duration};

    use onwards::target::TargetSpecOrList;
    use tokio::{sync::mpsc, time::timeout};
    use tokio_util::sync::CancellationToken;

    use crate::sync::onwards_config::{OnwardsTarget, SyncConfig, convert_to_config_file, parse_notify_payload};

    // Helper function to create a test target
    fn create_test_target(model_name: &str, alias: &str, endpoint_url: &str) -> OnwardsTarget {
        OnwardsTarget {
            model_name: model_name.to_string(),
            alias: alias.to_string(),
            requests_per_second: None,
            burst_size: None,
            capacity: None,
            sanitize_responses: true,
            trusted: false,
            open_responses_adapter: true,
            endpoint_url: url::Url::parse(endpoint_url).unwrap(),
            endpoint_api_key: None,
            auth_header_name: "Authorization".to_string(),
            auth_header_prefix: "Bearer ".to_string(),
            api_keys: Vec::new(),
        }
    }

    #[test]
    fn test_convert_to_config_file() {
        // Create test targets
        let target1 = create_test_target("gpt-4", "gpt4-alias", "https://api.openai.com");
        let target2 = create_test_target("claude-3", "claude-alias", "https://api.anthropic.com");

        let targets = vec![target1, target2];
        let config = convert_to_config_file(targets, vec![], false);

        // Verify the config
        assert_eq!(config.targets.len(), 2);

        // Check model1 (using alias as key)
        let target1 = &config.targets["gpt4-alias"];
        if let TargetSpecOrList::Single(spec) = target1 {
            assert_eq!(spec.url.as_str(), "https://api.openai.com/");
            assert_eq!(spec.onwards_model, Some("gpt-4".to_string()));
            // Since we provided empty key data, targets should have no keys configured
            assert!(spec.keys.is_none() || spec.keys.as_ref().unwrap().is_empty());
        } else {
            panic!("Expected Single target spec");
        }

        // Check model2 (using alias as key)
        let target2 = &config.targets["claude-alias"];
        if let TargetSpecOrList::Single(spec) = target2 {
            assert_eq!(spec.url.as_str(), "https://api.anthropic.com/");
            assert_eq!(spec.onwards_model, Some("claude-3".to_string()));
            assert!(spec.keys.is_none() || spec.keys.as_ref().unwrap().is_empty());
        } else {
            panic!("Expected Single target spec");
        }
    }

    #[test]
    fn test_convert_to_config_file_with_single_target() {
        // Create a single test target
        let target = create_test_target("valid-model", "valid-alias", "https://api.valid.com");

        let targets = vec![target];
        let config = convert_to_config_file(targets, vec![], false);

        // Should have exactly one target
        assert_eq!(config.targets.len(), 1);
        assert!(config.targets.contains_key("valid-alias"));
    }

    #[test]
    fn test_parse_notify_payload() {
        // Test valid payload
        let now_micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as i64;
        let payload = format!("api_keys:{}", now_micros);
        let result = parse_notify_payload(&payload);
        assert!(result.is_some());
        let (table_name, lag) = result.unwrap();
        assert_eq!(table_name, "api_keys");
        // Lag should be very small (< 100ms) since we just created the timestamp
        assert!(lag.as_millis() < 100, "Lag should be < 100ms, got {:?}", lag);

        // Test payload from 1 second ago
        let old_micros = now_micros - 1_000_000; // 1 second ago
        let old_payload = format!("deployed_models:{}", old_micros);
        let result = parse_notify_payload(&old_payload);
        assert!(result.is_some());
        let (table_name, lag) = result.unwrap();
        assert_eq!(table_name, "deployed_models");
        // Lag should be around 1 second
        assert!(
            lag.as_millis() >= 1000 && lag.as_millis() < 1100,
            "Lag should be ~1s, got {:?}",
            lag
        );

        // Test invalid payloads
        assert!(parse_notify_payload("").is_none());
        assert!(parse_notify_payload("no_colon").is_none());
        assert!(parse_notify_payload("table:not_a_number").is_none());
        assert!(parse_notify_payload("too:many:colons").is_none());
    }

    #[sqlx::test]
    /// Test that tariff changes trigger onwards config reload via Postgres NOTIFY
    async fn test_onwards_config_reloads_on_tariff_change(pool: sqlx::PgPool) {
        use crate::Role;
        use crate::db::handlers::{Deployments, InferenceEndpoints, Repository, Tariffs};
        use crate::db::models::{
            deployments::DeploymentCreateDBRequest, inference_endpoints::InferenceEndpointCreateDBRequest, tariffs::TariffCreateDBRequest,
        };
        use rust_decimal::Decimal;
        use sqlx::postgres::PgListener;

        // Create test user
        let test_user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

        // Set up a listener to verify notifications are sent
        let mut listener = PgListener::connect_with(&pool).await.unwrap();
        listener.listen("auth_config_changed").await.unwrap();

        // Create test endpoint
        let mut endpoint_tx = pool.begin().await.unwrap();
        let mut endpoints_repo = InferenceEndpoints::new(&mut endpoint_tx);
        let endpoint = endpoints_repo
            .create(&InferenceEndpointCreateDBRequest {
                created_by: test_user.id,
                name: "test-endpoint".to_string(),
                description: None,
                url: url::Url::from_str("https://api.test.com").unwrap(),
                api_key: None,
                model_filter: None,
                auth_header_name: Some("Authorization".to_string()),
                auth_header_prefix: Some("Bearer ".to_string()),
            })
            .await
            .unwrap();
        endpoint_tx.commit().await.unwrap();

        // Create test deployment
        let mut deployment_tx = pool.begin().await.unwrap();
        let mut deployments_repo = Deployments::new(&mut deployment_tx);
        let deployment = deployments_repo
            .create(&DeploymentCreateDBRequest {
                created_by: test_user.id,
                model_name: "test-model".to_string(),
                alias: "test-alias".to_string(),
                description: None,
                model_type: None,
                capabilities: None,
                hosted_on: Some(endpoint.id),
                requests_per_second: None,
                burst_size: None,
                capacity: None,
                batch_capacity: None,
                throughput: None,
                provider_pricing: None,
                // Composite model fields (regular model = not composite)
                is_composite: false,
                lb_strategy: None,
                fallback_enabled: None,
                fallback_on_rate_limit: None,
                fallback_on_status: None,
                fallback_with_replacement: None,
                fallback_max_attempts: None,
                sanitize_responses: true,
                trusted: false,
                open_responses_adapter: true,
            })
            .await
            .unwrap();
        deployment_tx.commit().await.unwrap();

        // Drain any pending notifications from setup
        tokio::time::sleep(Duration::from_millis(100)).await;
        while timeout(Duration::from_millis(10), listener.try_recv()).await.is_ok() {
            // Drain
        }

        // Now create a tariff - this should trigger a notification
        let mut tariff_tx = pool.begin().await.unwrap();
        let mut tariffs_repo = Tariffs::new(&mut tariff_tx);
        tariffs_repo
            .create(&TariffCreateDBRequest {
                deployed_model_id: deployment.id,
                name: "default".to_string(),
                input_price_per_token: Decimal::new(1, 6),  // $0.000001
                output_price_per_token: Decimal::new(2, 6), // $0.000002
                api_key_purpose: None,
                completion_window: None,
                valid_from: None,
            })
            .await
            .unwrap();
        tariff_tx.commit().await.unwrap();

        // Wait for notification
        let notification = timeout(Duration::from_secs(2), listener.recv())
            .await
            .expect("Timeout waiting for tariff change notification")
            .expect("Failed to receive notification");

        // Verify notification contains tariff table reference
        assert!(
            notification.payload().contains("model_tariffs"),
            "Notification should reference model_tariffs table"
        );
    }

    #[sqlx::test]
    /// Test that batch API keys get automatic access to composite escalation targets
    async fn test_batch_api_key_access_to_composite_escalation_target(pool: sqlx::PgPool) {
        use std::str::FromStr;

        use onwards::auth::ConstantTimeString;

        use crate::Role;
        use crate::db::handlers::{Deployments, InferenceEndpoints, Repository, api_keys::ApiKeys};
        use crate::db::models::{
            api_keys::{ApiKeyCreateDBRequest, ApiKeyPurpose},
            deployments::{DeploymentCreateDBRequest, LoadBalancingStrategy},
            inference_endpoints::InferenceEndpointCreateDBRequest,
        };

        // Create test user
        let test_user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

        // Grant credits to the user (required for API key access)
        sqlx::query!(
            r#"
            INSERT INTO credits_transactions (user_id, amount, transaction_type, source_id, balance_after, description)
            VALUES ($1, 1000000, 'admin_grant', 'test-grant', 1000000, 'Test credits for API key access')
            "#,
            test_user.id
        )
        .execute(&pool)
        .await
        .unwrap();

        // Create test endpoint
        let mut endpoint_tx = pool.begin().await.unwrap();
        let mut endpoints_repo = InferenceEndpoints::new(&mut endpoint_tx);
        let endpoint = endpoints_repo
            .create(&InferenceEndpointCreateDBRequest {
                created_by: test_user.id,
                name: "test-endpoint".to_string(),
                description: None,
                url: url::Url::from_str("https://api.test.com").unwrap(),
                api_key: None,
                model_filter: None,
                auth_header_name: Some("Authorization".to_string()),
                auth_header_prefix: Some("Bearer ".to_string()),
            })
            .await
            .unwrap();
        endpoint_tx.commit().await.unwrap();

        // Create component model (regular deployment)
        let mut component_tx = pool.begin().await.unwrap();
        let mut deployments_repo = Deployments::new(&mut component_tx);
        let component_model = deployments_repo
            .create(&DeploymentCreateDBRequest {
                created_by: test_user.id,
                model_name: "gpt-4".to_string(),
                alias: "gpt-4-component".to_string(),
                description: None,
                model_type: None,
                capabilities: None,
                hosted_on: Some(endpoint.id),
                requests_per_second: None,
                burst_size: None,
                capacity: None,
                batch_capacity: None,
                throughput: None,
                provider_pricing: None,
                is_composite: false,
                lb_strategy: None,
                fallback_enabled: None,
                fallback_on_rate_limit: None,
                fallback_on_status: None,
                fallback_with_replacement: None,
                fallback_max_attempts: None,
                sanitize_responses: true,
                trusted: false,
                open_responses_adapter: true,
            })
            .await
            .unwrap();
        component_tx.commit().await.unwrap();

        // Create composite model with escalation alias
        let composite_alias = "escalation-composite".to_string();
        let mut composite_tx = pool.begin().await.unwrap();
        let mut deployments_repo = Deployments::new(&mut composite_tx);
        let composite_model = deployments_repo
            .create(&DeploymentCreateDBRequest {
                created_by: test_user.id,
                model_name: "composite-model".to_string(),
                alias: composite_alias.clone(),
                description: Some("Composite escalation target".to_string()),
                model_type: None,
                capabilities: None,
                hosted_on: None, // Composite models have no direct endpoint
                requests_per_second: None,
                burst_size: None,
                capacity: None,
                batch_capacity: None,
                throughput: None,
                provider_pricing: None,
                is_composite: true,
                lb_strategy: Some(LoadBalancingStrategy::WeightedRandom),
                fallback_enabled: Some(true),
                fallback_on_rate_limit: Some(true),
                fallback_on_status: Some(vec![429, 500, 502, 503, 504]),
                fallback_with_replacement: None,
                fallback_max_attempts: None,
                sanitize_responses: true,
                trusted: false,
                open_responses_adapter: true,
            })
            .await
            .unwrap();
        composite_tx.commit().await.unwrap();

        // Link component to composite model
        sqlx::query!(
            r#"
            INSERT INTO deployed_model_components (composite_model_id, deployed_model_id, weight, sort_order, enabled)
            VALUES ($1, $2, 100, 0, TRUE)
            "#,
            composite_model.id,
            component_model.id,
        )
        .execute(&pool)
        .await
        .unwrap();

        // Create batch-purpose API key
        let mut api_key_tx = pool.begin().await.unwrap();
        let mut api_keys_repo = ApiKeys::new(&mut api_key_tx);
        let batch_api_key = api_keys_repo
            .create(&ApiKeyCreateDBRequest {
                user_id: test_user.id,
                name: "batch-key".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Batch,
                requests_per_second: None,
                burst_size: None,
            })
            .await
            .unwrap();
        api_key_tx.commit().await.unwrap();

        // Load targets with composite alias in escalation_models
        let escalation_models = vec![composite_alias.clone()];
        let targets = super::load_targets_from_db(&pool, &escalation_models, false).await.unwrap();

        // Find the composite model in targets (DashMap)
        let composite_target = targets.targets.get(&composite_alias).expect("Composite model should be in targets");

        // Access the ProviderPool from the DashMap entry
        let pool_spec = composite_target.value();

        // Verify batch API key has access
        // Keys are stored as ConstantTimeString in onwards
        let batch_key_ct = ConstantTimeString::from(batch_api_key.secret.clone());
        let keys = pool_spec.keys().expect("Composite model should have keys");
        let has_batch_key = keys.iter().any(|k| k == &batch_key_ct);

        assert!(has_batch_key, "Batch API key should have access to composite escalation target");
    }

    /// Regression test: onwards_config should reconnect after connection loss
    /// and successfully resume receiving notifications.
    #[sqlx::test]
    #[test_log::test]
    async fn test_onwards_config_reconnects_after_connection_loss(pool: sqlx::PgPool) {
        // Start the onwards config sync with status channel
        let (sync, _initial_targets, _stream) = super::OnwardsConfigSync::new(pool.clone())
            .await
            .expect("Failed to create OnwardsConfigSync");

        let (status_tx, mut status_rx) = mpsc::channel(10);
        let config = SyncConfig {
            status_tx: Some(status_tx),
            fallback_interval_milliseconds: 10000,
        };
        let shutdown_token = CancellationToken::new();
        let mut sync_handle = tokio::spawn({
            let shutdown = shutdown_token.clone();
            async move { sync.start(config, shutdown).await }
        });

        // Wait for initial connection
        println!("Waiting for Connecting status...");
        assert_eq!(status_rx.recv().await, Some(super::SyncStatus::Connecting));
        println!("Waiting for Connected status...");
        assert_eq!(status_rx.recv().await, Some(super::SyncStatus::Connected));
        println!("Initial connection established");

        // Kill the LISTEN connection to simulate network interruption
        // First, get the PIDs of LISTEN connections
        let pids: Vec<i32> = sqlx::query_scalar(
            "SELECT pid FROM pg_stat_activity
             WHERE query LIKE '%LISTEN%auth_config_changed%'
             AND pid != pg_backend_pid()",
        )
        .fetch_all(&pool)
        .await
        .expect("Failed to find LISTEN connections");

        assert!(!pids.is_empty(), "Should have found at least one LISTEN connection");
        println!("Found {} LISTEN connections to kill: {:?}", pids.len(), pids);

        // Now kill them one by one
        for pid in &pids {
            let _: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
                .bind(pid)
                .fetch_one(&pool)
                .await
                .expect("Failed to terminate backend");
        }
        println!("Killed LISTEN connections");

        // Wait for reconnection status events
        println!("Waiting for Disconnected status...");
        // Add a timeout in case the Disconnected status never arrives
        let status = timeout(Duration::from_secs(2), status_rx.recv())
            .await
            .expect("Timeout waiting for Disconnected status - the dead connection wasn't detected");
        assert_eq!(
            status,
            Some(super::SyncStatus::Disconnected),
            "Should receive Disconnected after kill"
        );

        println!("Waiting for Reconnecting status...");
        let status = status_rx.recv().await;
        assert_eq!(status, Some(super::SyncStatus::Reconnecting), "Should receive Reconnecting");

        // Wait up to 7 seconds for successful reconnection (5s delay + 2s buffer)
        let reconnected = timeout(Duration::from_secs(7), async {
            loop {
                match status_rx.recv().await {
                    Some(super::SyncStatus::Connected) => return true,
                    Some(status) => println!("Received status: {:?}", status),
                    None => return false,
                }
            }
        })
        .await;

        assert!(
            reconnected.is_ok(),
            "Should reconnect after connection loss (BUG: current code calls listen() on broken connection)"
        );

        // Verify task is still running
        let result = timeout(Duration::from_millis(100), &mut sync_handle).await;
        assert!(result.is_err(), "Task should still be running after reconnection");
        sync_handle.abort();
    }

    #[sqlx::test]
    #[test_log::test]
    /// Test that fallback sync triggers periodic reloads even without LISTEN/NOTIFY activity
    async fn test_fallback_sync_triggers_without_notifications(pool: sqlx::PgPool) {
        use tokio::sync::mpsc;
        use tokio_util::sync::CancellationToken;

        // Create the sync service
        let (sync, _initial_targets, _stream) = super::OnwardsConfigSync::new(pool.clone())
            .await
            .expect("Failed to create OnwardsConfigSync");

        // Create sync config with 200ms fallback interval for fast testing
        let (status_tx, mut status_rx) = mpsc::channel(10);
        let config = SyncConfig {
            status_tx: Some(status_tx),
            fallback_interval_milliseconds: 20,
        };

        let shutdown_token = CancellationToken::new();
        let mut sync_handle = tokio::spawn({
            let token = shutdown_token.clone();
            async move { sync.start(config, token).await }
        });

        // Wait for initial connection
        println!("Waiting for Connecting status...");
        assert_eq!(status_rx.recv().await, Some(super::SyncStatus::Connecting));
        println!("Waiting for Connected status...");
        assert_eq!(status_rx.recv().await, Some(super::SyncStatus::Connected));
        println!("Initial connection established");

        // Poll task health to ensure fallback sync doesn't crash
        // Use interval to poll every 100ms for 500ms total (at least 2 fallback syncs at 200ms each)
        println!("Polling task health while waiting for fallback sync...");
        let mut poll_interval = tokio::time::interval(Duration::from_millis(100));
        poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        for i in 0..5 {
            poll_interval.tick().await;

            // Check task is still running (timeout ensures we don't block if it finished)
            let result = timeout(Duration::from_millis(10), &mut sync_handle).await;
            assert!(
                result.is_err(),
                "Task should still be running at poll {} (proves fallback timer doesn't crash)",
                i
            );
        }

        println!("✅ Fallback sync working: task remained healthy through 5 health polls over 500ms");

        // Cleanup
        shutdown_token.cancel();
        let _ = timeout(Duration::from_secs(1), sync_handle).await;
    }
}
