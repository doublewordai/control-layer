use std::{collections::HashMap, num::NonZeroU32};

use onwards::target::{Auth, ConfigFile, KeyDefinition, RateLimitParameters, TargetSpec, Targets, WatchTargetsStream};
use sqlx::{postgres::PgListener, PgPool};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::{debug, error, info, instrument, warn};
use url::Url;

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
    db::{
        handlers::{api_keys::ApiKeys, deployments::DeploymentFilter, Deployments, InferenceEndpoints, Repository as _},
        models::{api_keys::ApiKeyDBResponse, deployments::DeploymentDBResponse},
    },
    types::{DeploymentId, InferenceEndpointId},
};

/// Manages the integration between onwards-pilot and the onwards proxy
pub struct OnwardsConfigSync {
    db: PgPool,
    sender: watch::Sender<Targets>,
    shutdown_token: CancellationToken,
}

#[derive(Default)]
pub struct SyncConfig {
    status_tx: Option<mpsc::Sender<SyncStatus>>,
}

impl OnwardsConfigSync {
    /// Creates a new OnwardsConfigSync and returns it along with initial targets, a WatchTargetsStream, and a drop guard for shutdown
    #[instrument(skip(db))]
    pub async fn new(db: PgPool) -> Result<(Self, Targets, WatchTargetsStream, DropGuard), anyhow::Error> {
        // Load initial configuration
        let initial_targets = load_targets_from_db(&db).await?;

        // Create watch channel with initial state
        let (sender, receiver) = watch::channel(initial_targets.clone());

        // Create shutdown token and drop guard
        let shutdown_token = CancellationToken::new();
        let drop_guard = shutdown_token.clone().drop_guard();

        let integration = Self {
            db,
            sender,
            shutdown_token,
        };
        let stream = WatchTargetsStream::new(receiver);

        Ok((integration, initial_targets, stream, drop_guard))
    }

    /// Starts the background task that listens for database changes and updates the configuration
    #[instrument(skip(self, config), err)]
    pub async fn start(self, config: SyncConfig) -> Result<(), anyhow::Error> {
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
                    _ = self.shutdown_token.cancelled() => {
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
async fn load_targets_from_db(db: &PgPool) -> Result<Targets, anyhow::Error> {
    debug!("Loading onwards targets from database");

    let mut tx = db.begin().await?;
    let models;
    {
        let mut deployments_repo = Deployments::new(&mut tx);

        // Fetch all deployments
        models = deployments_repo.list(&DeploymentFilter::new(0, i64::MAX)).await?;
    }

    let endpoints;
    {
        let mut endpoints_repo = InferenceEndpoints::new(&mut tx);
        // Fetch all endpoints to create a mapping
        endpoints = endpoints_repo.get_bulk(models.iter().map(|m| m.hosted_on).collect()).await?;
    }
    let endpoint_urls: HashMap<InferenceEndpointId, String> = endpoints.iter().map(|(k, v)| (*k, v.url.to_string())).collect();
    let endpoint_api_keys: HashMap<InferenceEndpointId, Option<String>> = endpoints.iter().map(|(k, v)| (*k, v.api_key.clone())).collect();
    let endpoint_auth_header_names: HashMap<InferenceEndpointId, String> =
        endpoints.iter().map(|(k, v)| (*k, v.auth_header_name.clone())).collect();
    let endpoint_auth_header_prefixes: HashMap<InferenceEndpointId, String> =
        endpoints.into_iter().map(|(k, v)| (k, v.auth_header_prefix.clone())).collect();
    let mut deployment_api_keys = HashMap::new();

    {
        let mut api_keys_repo = ApiKeys::new(&mut tx);

        // Fetch API keys for each deployment
        for model in &models {
            match api_keys_repo.get_api_keys_for_deployment(model.id).await {
                Ok(keys) => {
                    debug!("Found {} API keys for deployment '{}' ({})", keys.len(), model.alias, model.id);
                    deployment_api_keys.insert(model.id, keys);
                }
                Err(e) => {
                    debug!("Failed to get API keys for deployment '{}' ({}): {}", model.alias, model.id, e);
                }
            }
        }
    }
    tx.commit().await?;
    debug!("Loaded {} deployments from database", models.len());

    // Convert to ConfigFile format
    let config = convert_to_config_file(
        models,
        &deployment_api_keys,
        &endpoint_urls,
        &endpoint_api_keys,
        &endpoint_auth_header_names,
        &endpoint_auth_header_prefixes,
    );

    // Convert ConfigFile to Targets
    Targets::from_config(config)
}

/// Converts database models to the ConfigFile format expected by onwards
#[tracing::instrument(skip(
    models,
    deployment_api_keys,
    endpoint_urls,
    endpoint_api_keys,
    endpoint_auth_header_names,
    endpoint_auth_header_prefixes
))]
fn convert_to_config_file(
    models: Vec<DeploymentDBResponse>,
    deployment_api_keys: &HashMap<DeploymentId, Vec<ApiKeyDBResponse>>,
    endpoint_urls: &HashMap<InferenceEndpointId, String>,
    endpoint_api_keys: &HashMap<InferenceEndpointId, Option<String>>,
    endpoint_auth_header_names: &HashMap<InferenceEndpointId, String>,
    endpoint_auth_header_prefixes: &HashMap<InferenceEndpointId, String>,
) -> ConfigFile {
    // Build key_definitions for per-API-key rate limits
    let mut key_definitions = HashMap::new();
    for api_keys in deployment_api_keys.values() {
        for api_key in api_keys {
            // Only add keys that have rate limits configured
            if api_key.requests_per_second.is_some() || api_key.burst_size.is_some() {
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

                if rate_limit.is_some() {
                    key_definitions.insert(
                        api_key.id.to_string(),
                        KeyDefinition {
                            key: api_key.secret.clone(),
                            rate_limit,
                        },
                    );
                }
            }
        }
    }

    // Build auth section with key definitions (if any exist)
    let auth = if key_definitions.is_empty() {
        None
    } else {
        // Create Auth with key definitions but no global keys
        Some(
            Auth::builder()
                .global_keys(std::collections::HashSet::new())
                .key_definitions(key_definitions)
                .build(),
        )
    };

    // Build targets with model rate limits and key references
    let targets = models
        .into_iter()
        .filter_map(|model| {
            // Get API keys for this deployment
            let api_keys = deployment_api_keys.get(&model.id);
            let keys = api_keys.map(|keys| keys.iter().map(|k| k.secret.clone().into()).collect());

            // Determine the URL for this model
            let url = match endpoint_urls.get(&model.hosted_on) {
                Some(url_str) => match Url::parse(url_str) {
                    Ok(url) => url,
                    Err(_) => {
                        error!(
                            "Model '{}' has invalid endpoint URL '{}', skipping from config",
                            model.model_name, url_str
                        );
                        return None;
                    }
                },
                None => {
                    error!(
                        "Model '{}' references non-existent endpoint {}, skipping from config",
                        model.model_name, model.hosted_on
                    );
                    return None;
                }
            };

            // Get the API key for this endpoint (for downstream authentication)
            let endpoint_api_key = endpoint_api_keys.get(&model.hosted_on).and_then(|k| k.as_ref());

            // Get the auth header configuration for this endpoint
            let auth_header_name = endpoint_auth_header_names.get(&model.hosted_on).cloned();
            let auth_header_prefix = endpoint_auth_header_prefixes.get(&model.hosted_on).cloned();

            // Build rate limiting parameters if configured
            let rate_limit = match (model.requests_per_second, model.burst_size) {
                (Some(rps), burst) if rps > 0.0 => {
                    // Convert f32 to NonZeroU32, ensuring it's at least 1
                    let rps_u32 = NonZeroU32::new((rps.max(1.0) as u32).max(1)).unwrap();
                    let burst_u32 = burst.and_then(|b| NonZeroU32::new(b.max(1) as u32));

                    debug!(
                        "Model '{}' configured with {}req/s rate limit, burst: {:?}",
                        model.alias, rps, burst_u32
                    );

                    Some(RateLimitParameters {
                        requests_per_second: rps_u32,
                        burst_size: burst_u32,
                    })
                }
                _ => None,
            };

            // Build target spec with all parameters
            // Only set custom auth headers if they differ from defaults
            let upstream_auth_header_name = auth_header_name.and_then(|name| if name != "Authorization" { Some(name) } else { None });
            let upstream_auth_header_prefix = auth_header_prefix.and_then(|prefix| if prefix != "Bearer " { Some(prefix) } else { None });

            // Convert pricing from database format to hashmap of response headers
            let response_headers = model.pricing.as_ref().and_then(|p| {
                p.upstream.as_ref().map(|upstream| {
                    let mut headers = HashMap::new();
                    upstream
                        .input_price_per_token
                        .map(|d| headers.insert(ONWARDS_INPUT_TOKEN_PRICE_HEADER.to_string(), d.to_string()));
                    upstream
                        .output_price_per_token
                        .map(|d| headers.insert(ONWARDS_OUTPUT_TOKEN_PRICE_HEADER.to_string(), d.to_string()));
                    headers
                })
            });

            let target_spec = TargetSpec {
                url,
                keys,
                onwards_key: endpoint_api_key.cloned(),
                onwards_model: Some(model.model_name.clone()),
                rate_limit,
                upstream_auth_header_name,
                upstream_auth_header_prefix,
                response_headers,
            };

            Some((model.alias, target_spec))
        })
        .collect();

    ConfigFile { targets, auth }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, str::FromStr, time::Duration};

    use chrono::Utc;
    use tokio::{sync::{watch, mpsc}, time::timeout};
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    use crate::{
        db::models::deployments::{DeploymentDBResponse, ModelStatus},
        sync::onwards_config::{convert_to_config_file, SyncConfig},
    };

    // Helper function to create a test deployed model
    fn create_test_model(name: &str, alias: &str, endpoint_id: Uuid) -> DeploymentDBResponse {
        DeploymentDBResponse {
            id: Uuid::new_v4(),
            model_name: name.to_string(),
            alias: alias.to_string(),
            description: None,
            model_type: None,
            capabilities: None,
            created_by: Uuid::nil(),
            hosted_on: endpoint_id,
            status: ModelStatus::Active,
            last_sync: None,
            deleted: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            requests_per_second: None,
            burst_size: None,
            pricing: None,
        }
    }

    #[test]
    fn test_convert_to_config_file() {
        // Create test models
        let model1 = create_test_model(
            "gpt-4",
            "gpt4-alias",
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
        );
        let model2 = create_test_model(
            "claude-3",
            "claude-alias",
            Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
        );

        // Create endpoint URL mapping
        let mut endpoint_urls = HashMap::new();
        endpoint_urls.insert(
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            "https://api.openai.com".to_string(),
        );
        endpoint_urls.insert(
            Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
            "https://api.anthropic.com".to_string(),
        );

        // Create empty deployment API keys to test the case where no keys are configured
        let deployment_api_keys = HashMap::new();

        // Create endpoint API keys
        let endpoint_api_keys = HashMap::new();

        // Create endpoint auth header names (using defaults)
        let mut endpoint_auth_header_names = HashMap::new();
        endpoint_auth_header_names.insert(
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            "Authorization".to_string(),
        );
        endpoint_auth_header_names.insert(
            Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
            "Authorization".to_string(),
        );

        // Create endpoint auth header prefixes (using defaults)
        let mut endpoint_auth_header_prefixes = HashMap::new();
        endpoint_auth_header_prefixes.insert(
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            "Bearer ".to_string(),
        );
        endpoint_auth_header_prefixes.insert(
            Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
            "Bearer ".to_string(),
        );

        let models = vec![model1.clone(), model2.clone()];
        let config = convert_to_config_file(
            models,
            &deployment_api_keys,
            &endpoint_urls,
            &endpoint_api_keys,
            &endpoint_auth_header_names,
            &endpoint_auth_header_prefixes,
        );

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
    fn test_convert_to_config_file_skips_invalid() {
        let model1 = create_test_model(
            "valid-model",
            "valid-alias",
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
        );
        let model2 = create_test_model(
            "invalid-model",
            "invalid-alias",
            Uuid::parse_str("99999999-9999-9999-9999-999999999999").unwrap(),
        ); // Non-existent endpoint

        let mut endpoint_urls = HashMap::new();
        endpoint_urls.insert(
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            "https://api.valid.com".to_string(),
        );
        // Note: endpoint 999 doesn't exist

        let deployment_api_keys = HashMap::new();
        let endpoint_api_keys = HashMap::new();

        // Create endpoint auth header names (using defaults)
        let mut endpoint_auth_header_names = HashMap::new();
        endpoint_auth_header_names.insert(
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            "Authorization".to_string(),
        );

        // Create endpoint auth header prefixes (using defaults)
        let mut endpoint_auth_header_prefixes = HashMap::new();
        endpoint_auth_header_prefixes.insert(
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            "Bearer ".to_string(),
        );

        let models = vec![model1, model2];
        let config = convert_to_config_file(
            models,
            &deployment_api_keys,
            &endpoint_urls,
            &endpoint_api_keys,
            &endpoint_auth_header_names,
            &endpoint_auth_header_prefixes,
        );

        // Should only have the valid model
        assert_eq!(config.targets.len(), 1);
        assert!(config.targets.contains_key("valid-alias"));
        assert!(!config.targets.contains_key("invalid-alias"));
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
            shutdown_token: shutdown_token.clone(),
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
        let (status_tx, mut status_rx) = mpsc::channel(10);
        let config = SyncConfig {
            status_tx: Some(status_tx),
        };
        tokio::spawn(async move {
            if let Err(e) = sync.start(config).await {
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
        let (sync, _initial_targets, _stream, _drop_guard) = super::OnwardsConfigSync::new(pool.clone())
            .await
            .expect("Failed to create OnwardsConfigSync");

        let (status_tx, mut status_rx) = mpsc::channel(10);
        let config = SyncConfig {
            status_tx: Some(status_tx),
        };
        let mut sync_handle = tokio::spawn(async move { sync.start(config).await });

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

        // Give the connection time to detect it's been terminated
        tokio::time::sleep(Duration::from_millis(500)).await;

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
