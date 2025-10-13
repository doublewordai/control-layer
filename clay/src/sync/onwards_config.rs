use std::{collections::HashMap, num::NonZeroU32};

use onwards::target::{Auth, ConfigFile, KeyDefinition, RateLimitParameters, TargetSpec, Targets, WatchTargetsStream};
use sqlx::{postgres::PgListener, PgPool};
use tokio::sync::watch;
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::{debug, error, info, instrument};
use url::Url;

use crate::{
    db::{
        handlers::{api_keys::ApiKeys, deployments::DeploymentFilter, Deployments, InferenceEndpoints, Repository as _},
        models::{api_keys::ApiKeyDBResponse, deployments::DeploymentDBResponse},
    },
    types::{DeploymentId, InferenceEndpointId},
};

/// Manages the integration between onwards-pilot and the onwards proxy
#[derive(Debug)]
pub struct OnwardsConfigSync {
    db: PgPool,
    sender: watch::Sender<Targets>,
    shutdown_token: CancellationToken,
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
    #[instrument(skip(self))]
    pub async fn start(self) -> Result<(), anyhow::Error> {
        let mut listener = PgListener::connect_with(&self.db).await?;

        // Listen to auth config changes
        listener.listen("auth_config_changed").await?;

        info!("Started onwards configuration listener");

        // Debouncing: prevent rapid-fire reloads
        let mut last_reload_time = std::time::Instant::now();
        const MIN_RELOAD_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

        // Listen for notifications with graceful shutdown
        loop {
            tokio::select! {
                // Handle shutdown signal
                _ = self.shutdown_token.cancelled() => {
                    info!("Received shutdown signal, stopping onwards configuration listener");
                    break;
                }

                // Handle database notifications
                notification_result = listener.recv() => {
                    match notification_result {
                        Ok(notification) => {
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

                            // Check if this is a fatal error that should propagate
                            if e.to_string().contains("closed pool") || e.to_string().contains("connection closed") {
                                error!("Database connection closed, exiting sync task");
                                return Err(e.into());
                            }

                            // Try to reconnect for other errors
                            tokio::select! {
                                _ = self.shutdown_token.cancelled() => {
                                    info!("Received shutdown signal during reconnect, stopping");
                                    break;
                                }
                                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                                    // Try to reconnect after delay
                                    if let Err(e) = listener.listen("auth_config_changed").await {
                                        error!("Failed to re-listen to PostgreSQL channel: {}", e);
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

/// Loads the current targets configuration from the database
#[tracing::instrument(skip(db))]
async fn load_targets_from_db(db: &PgPool) -> Result<Targets, anyhow::Error> {
    info!("Loading onwards targets from database");

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
    let endpoint_api_keys: HashMap<InferenceEndpointId, Option<String>> = endpoints.into_iter().map(|(k, v)| (k, v.api_key)).collect();
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
    info!("Loaded {} deployments from database", models.len());

    // Convert to ConfigFile format
    let config = convert_to_config_file(models, &deployment_api_keys, &endpoint_urls, &endpoint_api_keys);

    // Convert ConfigFile to Targets
    Targets::from_config(config)
}

/// Converts database models to the ConfigFile format expected by onwards
#[tracing::instrument(skip(models, deployment_api_keys, endpoint_urls, endpoint_api_keys))]
fn convert_to_config_file(
    models: Vec<DeploymentDBResponse>,
    deployment_api_keys: &HashMap<DeploymentId, Vec<ApiKeyDBResponse>>,
    endpoint_urls: &HashMap<InferenceEndpointId, String>,
    endpoint_api_keys: &HashMap<InferenceEndpointId, Option<String>>,
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
            let target_spec = TargetSpec {
                url,
                keys,
                onwards_key: endpoint_api_key.cloned(),
                onwards_model: Some(model.model_name.clone()),
                rate_limit,
            };

            Some((model.alias, target_spec))
        })
        .collect();

    ConfigFile { targets, auth }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::Utc;
    use uuid::Uuid;

    use crate::{
        db::models::deployments::{DeploymentDBResponse, ModelStatus},
        sync::onwards_config::convert_to_config_file,
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

        let models = vec![model1.clone(), model2.clone()];
        let config = convert_to_config_file(models, &deployment_api_keys, &endpoint_urls, &endpoint_api_keys);

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
        let models = vec![model1, model2];
        let config = convert_to_config_file(models, &deployment_api_keys, &endpoint_urls, &endpoint_api_keys);

        // Should only have the valid model
        assert_eq!(config.targets.len(), 1);
        assert!(config.targets.contains_key("valid-alias"));
        assert!(!config.targets.contains_key("invalid-alias"));
    }
}
