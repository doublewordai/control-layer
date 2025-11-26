//! Configuration synchronization to onwards routing layer.

use std::{collections::HashMap, num::NonZeroU32, sync::Arc};

use onwards::target::{
    Auth, ConcurrencyLimitParameters, ConfigFile, KeyDefinition, RateLimitParameters, TargetSpec, Targets, WatchTargetsStream,
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
    config::{ONWARDS_CONFIG_CHANGED_CHANNEL, ONWARDS_INPUT_TOKEN_PRICE_HEADER, ONWARDS_OUTPUT_TOKEN_PRICE_HEADER},
    types::{ApiKeyId, DeploymentId},
};

/// Complete data needed for one onwards target configuration
#[derive(Debug, Clone)]
struct OnwardsTarget {
    // Deployment info
    deployment_id: DeploymentId,
    model_name: String,
    alias: String,
    requests_per_second: Option<f32>,
    burst_size: Option<i32>,
    capacity: Option<i32>,
    upstream_input_price_per_token: Option<rust_decimal::Decimal>,
    upstream_output_price_per_token: Option<rust_decimal::Decimal>,

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
}

#[derive(Default)]
pub struct SyncConfig {
    status_tx: Option<mpsc::Sender<SyncStatus>>,
}

impl OnwardsConfigSync {
    /// Creates a new OnwardsConfigSync and returns it along with initial targets and a WatchTargetsStream
    #[allow(dead_code)]
    #[instrument(skip(db))]
    pub async fn new(db: PgPool) -> Result<(Self, Targets, WatchTargetsStream), anyhow::Error> {
        Self::new_with_daemon_limits(db, None).await
    }

    /// Creates a new OnwardsConfigSync with optional daemon capacity limits map
    #[instrument(skip(db, daemon_capacity_limits))]
    pub async fn new_with_daemon_limits(
        db: PgPool,
        daemon_capacity_limits: Option<Arc<dashmap::DashMap<String, usize>>>,
    ) -> Result<(Self, Targets, WatchTargetsStream), anyhow::Error> {
        // Load initial configuration
        let initial_targets = load_targets_from_db(&db).await?;

        // If daemon limits are provided, populate them
        if let Some(ref limits) = daemon_capacity_limits {
            update_daemon_capacity_limits(&db, limits).await?;
        }

        // Create watch channel with initial state
        let (sender, receiver) = watch::channel(initial_targets.clone());

        let integration = Self {
            db,
            sender,
            daemon_capacity_limits,
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
    pub async fn start(self, config: SyncConfig, shutdown_token: CancellationToken) -> Result<(), anyhow::Error> {
        // Debouncing: prevent rapid-fire reloads
        let mut last_reload_time = std::time::Instant::now();
        const MIN_RELOAD_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

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

                                // Debounce: skip if we just reloaded recently
                                if last_reload_time.elapsed() < MIN_RELOAD_INTERVAL {
                                    debug!("Skipping reload due to debouncing (last reload was {:?} ago)",
                                           last_reload_time.elapsed());
                                    continue;
                                }

                                // Reload configuration from database
                                last_reload_time = std::time::Instant::now();
                                match load_targets_from_db(&self.db).await {
                                    Ok(new_targets) => {
                                        info!("Loaded {} targets from database", new_targets.targets.len());
                                        for entry in new_targets.targets.iter() {
                                            let alias = entry.key();
                                            let target = entry.value();
                                            debug!("Target '{}': {} keys configured", alias,
                                                  target.keys.as_ref().map(|k| k.len()).unwrap_or(0));
                                        }

                                        // Update daemon capacity limits if configured
                                        if let Some(ref limits) = self.daemon_capacity_limits
                                            && let Err(e) = update_daemon_capacity_limits(&self.db, limits).await {
                                                error!("Failed to update daemon capacity limits: {}", e);
                                            }

                                        // Send update through watch channel
                                        if let Err(e) = self.sender.send(new_targets) {
                                            error!("Failed to send targets update: {}", e);
                                            // If all receivers are dropped, we can exit
                                            break;
                                        }
                                        info!("Updated onwards configuration successfully");
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
                }
            }
        }

        info!("Onwards configuration listener stopped gracefully");
        Ok(())
    }
}

/// Loads the current targets configuration from the database
#[tracing::instrument(skip(db))]
pub async fn load_targets_from_db(db: &PgPool) -> Result<Targets, anyhow::Error> {
    let query_start = std::time::Instant::now();
    debug!("Loading onwards targets from database");

    // Single mega-query to refresh the whole cache at once
    // - deployments (deployed_models)
    // - endpoints (inference_endpoints)
    // - api_keys with access control logic
    let rows = sqlx::query!(
        r#"
        WITH latest_balances AS (
            SELECT DISTINCT ON (user_id)
                user_id,
                balance_after
            FROM credits_transactions
            ORDER BY user_id, created_at DESC, id DESC
        )
        SELECT
            -- Deployment fields (only what we need)
            dm.id as deployment_id,
            dm.model_name,
            dm.alias,
            dm.hosted_on,
            dm.requests_per_second as deployment_requests_per_second,
            dm.burst_size as deployment_burst_size,
            dm.capacity,
            dm.upstream_input_price_per_token,
            dm.upstream_output_price_per_token,
            -- Endpoint fields (only what we need)
            ie.id as endpoint_id,
            ie.url as "endpoint_url!",
            ie.api_key as endpoint_api_key,
            ie.auth_header_name,
            ie.auth_header_prefix,
            -- API key fields (nullable due to LEFT JOIN)
            ak.id as "api_key_id?",
            ak.secret as "api_key_secret?",
            ak.requests_per_second as api_key_requests_per_second,
            ak.burst_size as api_key_burst_size
        FROM deployed_models dm
        INNER JOIN inference_endpoints ie ON dm.hosted_on = ie.id
        LEFT JOIN LATERAL (
            -- Get all API keys that have access to this deployment
            SELECT DISTINCT
                ak.id,
                ak.secret,
                ak.requests_per_second,
                ak.burst_size
            FROM api_keys ak
            WHERE (
                -- System user always has access
                ak.user_id = '00000000-0000-0000-0000-000000000000'

                -- OR user is in a group assigned to this deployment
                OR EXISTS (
                    SELECT 1 FROM user_groups ug
                    INNER JOIN deployment_groups dg ON ug.group_id = dg.group_id
                    WHERE dg.deployment_id = dm.id
                      AND ug.user_id = ak.user_id
                )

                -- OR deployment is in public group (nil UUID)
                OR EXISTS (
                    SELECT 1 FROM deployment_groups dg
                    WHERE dg.deployment_id = dm.id
                      AND dg.group_id = '00000000-0000-0000-0000-000000000000'
                )
            )
            -- Access control: require credit OR free model (except system user always passes)
            AND (
                ak.user_id = '00000000-0000-0000-0000-000000000000'
                OR EXISTS (
                    SELECT 1 FROM latest_balances lb
                    WHERE lb.user_id = ak.user_id AND lb.balance_after > 0
                )
                OR (
                    -- Free model check
                    (dm.upstream_input_price_per_token IS NULL OR dm.upstream_input_price_per_token = 0)
                    AND (dm.upstream_output_price_per_token IS NULL OR dm.upstream_output_price_per_token = 0)
                )
            )
        ) ak ON true
        WHERE dm.deleted = FALSE
        ORDER BY dm.id, ak.id
        "#
    )
    .fetch_all(db)
    .await?;

    let query_duration = query_start.elapsed();
    info!(
        "Mega-query completed in {:?}, fetched {} rows ({} rows/ms)",
        query_duration,
        rows.len(),
        if query_duration.as_millis() > 0 {
            rows.len() as u128 / query_duration.as_millis()
        } else {
            rows.len() as u128
        }
    );

    if query_duration.as_millis() > 500 {
        warn!("Mega-query took {:?}, which is slower than expected (>500ms).", query_duration);
    }

    // Group results into targets
    let mut targets_map: HashMap<DeploymentId, OnwardsTarget> = HashMap::new();

    for row in rows {
        let deployment_id = row.deployment_id;

        // Get or create target for this deployment
        let target = targets_map.entry(deployment_id).or_insert_with(|| OnwardsTarget {
            deployment_id,
            model_name: row.model_name.clone(),
            alias: row.alias.clone(),
            requests_per_second: row.deployment_requests_per_second,
            burst_size: row.deployment_burst_size,
            capacity: row.capacity,
            upstream_input_price_per_token: row.upstream_input_price_per_token,
            upstream_output_price_per_token: row.upstream_output_price_per_token,
            endpoint_url: url::Url::parse(&row.endpoint_url).expect("Invalid URL in database"),
            endpoint_api_key: row.endpoint_api_key.clone(),
            auth_header_name: row.auth_header_name.clone(),
            auth_header_prefix: row.auth_header_prefix.clone(),
            api_keys: Vec::new(),
        });

        // Add API key if present
        if let (Some(api_key_id), Some(api_key_secret)) = (row.api_key_id, row.api_key_secret) {
            target.api_keys.push(OnwardsApiKey {
                id: api_key_id,
                secret: api_key_secret,
                requests_per_second: row.api_key_requests_per_second,
                burst_size: row.api_key_burst_size,
            });
        }
    }

    let processing_start = std::time::Instant::now();
    let targets: Vec<_> = targets_map.into_values().collect();
    let total_api_keys: usize = targets.iter().map(|t| t.api_keys.len()).sum();

    info!(
        "Grouped into {} deployments with {} total API keys (processing took {:?})",
        targets.len(),
        total_api_keys,
        processing_start.elapsed()
    );

    for target in &targets {
        debug!(
            "Deployment '{}' ({}) has {} API keys",
            target.alias,
            target.deployment_id,
            target.api_keys.len()
        );
    }

    // Convert to ConfigFile format
    let config_start = std::time::Instant::now();
    let config = convert_to_config_file(targets);
    debug!("Config conversion took {:?}", config_start.elapsed());

    // Convert ConfigFile to Targets
    let onwards_start = std::time::Instant::now();
    let result = Targets::from_config(config);
    debug!("Onwards config instantiation took {:?}", onwards_start.elapsed());

    let total_duration = query_start.elapsed();
    info!(
        "Total load_targets_from_db took {:?} (query: {:?}, processing: {:?}, conversion: {:?}, onwards: {:?})",
        total_duration,
        query_duration,
        processing_start.elapsed(),
        config_start.elapsed(),
        onwards_start.elapsed()
    );

    result
}

/// Converts onwards targets to the ConfigFile format expected by onwards
#[tracing::instrument(skip(targets))]
fn convert_to_config_file(targets: Vec<OnwardsTarget>) -> ConfigFile {
    // Build both key_definitions and target specs in one iteration
    let mut key_definitions = HashMap::new();
    let target_specs = targets
        .into_iter()
        .map(|target| {
            // Add this target's API keys to key_definitions
            for api_key in &target.api_keys {
                // Build rate limit if configured
                let rate_limit = match (api_key.requests_per_second, api_key.burst_size) {
                    (Some(rps), burst) if rps > 0.0 => {
                        let rps_u32 = NonZeroU32::new((rps.max(1.0) as u32).max(1)).unwrap();
                        let burst_u32 = burst.and_then(|b| NonZeroU32::new(b.max(1) as u32));

                        debug!(
                            "API key '{}' configured with {}req/s rate limit, burst: {:?}",
                            api_key.secret, rps, burst_u32
                        );

                        Some(RateLimitParameters {
                            requests_per_second: rps_u32,
                            burst_size: burst_u32,
                        })
                    }
                    _ => None,
                };

                // Add all keys to key_definitions (whether they have rate limits or not)
                key_definitions.insert(
                    api_key.id.to_string(),
                    KeyDefinition {
                        key: api_key.secret.clone(),
                        rate_limit,
                        concurrency_limit: None, // Per-key concurrency limits not yet supported
                    },
                );
            }
            // Get API key secrets for this deployment (onwards validates against actual secrets)
            let keys = if target.api_keys.is_empty() {
                None
            } else {
                Some(target.api_keys.iter().map(|k| k.secret.clone().into()).collect())
            };

            // Build per-target rate limiting parameters if configured
            let rate_limit = match (target.requests_per_second, target.burst_size) {
                (Some(rps), burst) if rps > 0.0 => {
                    let rps_u32 = NonZeroU32::new((rps.max(1.0) as u32).max(1)).unwrap();
                    let burst_u32 = burst.and_then(|b| NonZeroU32::new(b.max(1) as u32));

                    debug!(
                        "Model '{}' configured with {}req/s rate limit, burst: {:?}",
                        target.alias, rps, burst_u32
                    );

                    Some(RateLimitParameters {
                        requests_per_second: rps_u32,
                        burst_size: burst_u32,
                    })
                }
                _ => None,
            };

            // Only set custom auth headers if they differ from defaults
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

            // Build concurrency limiting parameters if configured
            let concurrency_limit = target.capacity.map(|capacity| {
                debug!("Model '{}' configured with {} max concurrent requests", target.alias, capacity);

                ConcurrencyLimitParameters {
                    max_concurrent_requests: capacity as usize,
                }
            });

            // Convert pricing to response headers
            let response_headers = if target.upstream_input_price_per_token.is_some() || target.upstream_output_price_per_token.is_some() {
                let mut headers = HashMap::new();
                if let Some(price) = target.upstream_input_price_per_token {
                    headers.insert(ONWARDS_INPUT_TOKEN_PRICE_HEADER.to_string(), price.to_string());
                }
                if let Some(price) = target.upstream_output_price_per_token {
                    headers.insert(ONWARDS_OUTPUT_TOKEN_PRICE_HEADER.to_string(), price.to_string());
                }
                Some(headers)
            } else {
                None
            };

            let target_spec = TargetSpec {
                url: target.endpoint_url.clone(),
                keys,
                onwards_key: target.endpoint_api_key.clone(),
                onwards_model: Some(target.model_name.clone()),
                rate_limit,
                concurrency_limit,
                upstream_auth_header_name,
                upstream_auth_header_prefix,
                response_headers,
            };

            (target.alias, target_spec)
        })
        .collect();

    // Build auth section with key definitions (if any exist)
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
    }
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

    use tokio::{
        sync::{mpsc, watch},
        time::timeout,
    };
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    use crate::sync::onwards_config::{OnwardsTarget, SyncConfig, convert_to_config_file};

    // Helper function to create a test target
    fn create_test_target(model_name: &str, alias: &str, endpoint_url: &str) -> OnwardsTarget {
        OnwardsTarget {
            deployment_id: Uuid::new_v4(),
            model_name: model_name.to_string(),
            alias: alias.to_string(),
            requests_per_second: None,
            burst_size: None,
            capacity: None,
            upstream_input_price_per_token: None,
            upstream_output_price_per_token: None,
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
        let config = convert_to_config_file(targets);

        // Verify the config
        assert_eq!(config.targets.len(), 2);

        // Check model1 (using alias as key)
        let target1 = &config.targets["gpt4-alias"];
        assert_eq!(target1.url.as_str(), "https://api.openai.com/");
        assert_eq!(target1.onwards_model, Some("gpt-4".to_string()));
        // Since we provided empty key data, targets should have no keys configured
        assert!(target1.keys.is_none() || target1.keys.as_ref().unwrap().is_empty());

        // Check model2 (using alias as key)
        let target2 = &config.targets["claude-alias"];
        assert_eq!(target2.url.as_str(), "https://api.anthropic.com/");
        assert_eq!(target2.onwards_model, Some("claude-3".to_string()));
        assert!(target2.keys.is_none() || target2.keys.as_ref().unwrap().is_empty());
    }

    #[test]
    fn test_convert_to_config_file_with_single_target() {
        // Create a single test target
        let target = create_test_target("valid-model", "valid-alias", "https://api.valid.com");

        let targets = vec![target];
        let config = convert_to_config_file(targets);

        // Should have exactly one target
        assert_eq!(config.targets.len(), 1);
        assert!(config.targets.contains_key("valid-alias"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_pricing_update_triggers_notification_and_config_reload(pool: sqlx::PgPool) {
        // Setup: Create user and endpoint
        let user = crate::test_utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;

        let endpoint_result = sqlx::query!(
            r#"
            INSERT INTO inference_endpoints (name, description, url, created_by)
            VALUES ($1, $2, $3, $4)
            RETURNING id
            "#,
            "test-endpoint",
            "Test endpoint",
            "http://localhost:8000",
            user.id
        )
        .fetch_one(&pool)
        .await
        .expect("Failed to create endpoint");

        // Create deployed_model with initial pricing
        let original_input_price = rust_decimal::Decimal::from_str("0.00001").unwrap();
        let original_output_price = rust_decimal::Decimal::from_str("0.00003").unwrap();

        let model_result = sqlx::query!(
            r#"
            INSERT INTO deployed_models (
                alias, model_name, created_by, hosted_on, status,
                upstream_input_price_per_token, upstream_output_price_per_token
            )
            VALUES ($1, $2, $3, $4, 'active', $5, $6)
            RETURNING id
            "#,
            "test-gpt-4",
            "gpt-4",
            user.id,
            endpoint_result.id,
            original_input_price,
            original_output_price
        )
        .fetch_one(&pool)
        .await
        .expect("Failed to create deployed model");

        // Create the OnwardsConfigSync with a custom watch channel to monitor changes
        let initial_targets = super::load_targets_from_db(&pool).await.expect("Failed to load initial targets");
        let (sender, mut receiver) = watch::channel(initial_targets.clone());

        let shutdown_token = CancellationToken::new();
        let _drop_guard = shutdown_token.clone().drop_guard();

        let sync = super::OnwardsConfigSync {
            db: pool.clone(),
            sender,
            daemon_capacity_limits: None,
        };

        // Verify initial pricing
        let initial_target = initial_targets
            .targets
            .get("test-gpt-4")
            .expect("Model should be in initial targets");
        let initial_headers = initial_target
            .response_headers
            .as_ref()
            .expect("Response headers should be present");
        let initial_input_price = initial_headers
            .get(crate::config::ONWARDS_INPUT_TOKEN_PRICE_HEADER)
            .expect("Initial input price should be present");
        assert!(
            initial_input_price.starts_with("0.00001"),
            "Initial config should have original input price, got: {}",
            initial_input_price
        );

        // Start the sync task in the background
        let (status_tx, _status_rx) = mpsc::channel(10);
        let config = SyncConfig {
            status_tx: Some(status_tx),
        };
        tokio::spawn(async move {
            if let Err(e) = sync.start(config, shutdown_token).await {
                eprintln!("Sync task failed: {}", e);
            }
        });

        // Give the listener time to connect and start listening
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Update the model's pricing
        let new_input_price = rust_decimal::Decimal::from_str("0.00002").unwrap();
        let new_output_price = rust_decimal::Decimal::from_str("0.00006").unwrap();

        sqlx::query!(
            r#"
            UPDATE deployed_models
            SET upstream_input_price_per_token = $1,
                upstream_output_price_per_token = $2
            WHERE id = $3
            "#,
            new_input_price,
            new_output_price,
            model_result.id
        )
        .execute(&pool)
        .await
        .expect("Failed to update model pricing");

        // Wait for the receiver to detect a change
        // The receiver will be notified when the notification triggers a config reload
        let update_result = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.changed()).await;

        assert!(
            update_result.is_ok(),
            "Timeout waiting for config update - notification may not have triggered"
        );
        update_result
            .expect("Failed to receive update notification")
            .expect("Receiver error");

        // Get the updated targets from the receiver
        let updated_targets = receiver.borrow().clone();

        // Verify the updated pricing in the new config
        let updated_target = updated_targets
            .targets
            .get("test-gpt-4")
            .expect("Model should be in updated targets");
        let updated_headers = updated_target
            .response_headers
            .as_ref()
            .expect("Response headers should be present");

        let input_price_header = updated_headers
            .get(crate::config::ONWARDS_INPUT_TOKEN_PRICE_HEADER)
            .expect("Input price header should be present");
        let output_price_header = updated_headers
            .get(crate::config::ONWARDS_OUTPUT_TOKEN_PRICE_HEADER)
            .expect("Output price header should be present");

        // Verify the pricing was updated
        assert!(
            input_price_header.starts_with("0.00002"),
            "Config should have updated input price, got: {}",
            input_price_header
        );
        assert!(
            output_price_header.starts_with("0.00006"),
            "Config should have updated output price, got: {}",
            output_price_header
        );
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
}
