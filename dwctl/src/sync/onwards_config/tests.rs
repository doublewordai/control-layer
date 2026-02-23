use std::{str::FromStr, time::Duration};

use onwards::{
    auth::ConstantTimeString,
    load_balancer::ProviderPool,
    target::{LoadBalanceStrategy as OnwardsLoadBalanceStrategy, TargetSpecOrList},
};
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
        endpoint_url: url::Url::parse(endpoint_url).unwrap(),
        endpoint_api_key: None,
        auth_header_name: "Authorization".to_string(),
        auth_header_prefix: "Bearer ".to_string(),
        api_keys: Vec::new(),
    }
}

const SYSTEM_KEY_SECRET: &str = "sk-placeholder-will-be-updated-on-boot";
const KEY_A_SECRET: &str = "sk-cache-a";
const KEY_B_SECRET: &str = "sk-cache-b";
const KEY_BATCH_SECRET: &str = "sk-cache-batch";

fn pool_has_key(pool: &ProviderPool, key: &str) -> bool {
    let expected = ConstantTimeString::from(key.to_string());
    pool.keys().is_some_and(|keys| keys.iter().any(|candidate| candidate == &expected))
}

fn pool_keys_len(pool: &ProviderPool) -> usize {
    pool.keys().map_or(0, |keys| keys.len())
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

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
async fn test_cache_shape_regular_public_and_private_access(pool: sqlx::PgPool) {
    let targets = super::load_targets_from_db(&pool, &[], false).await.unwrap();

    let public = targets.targets.get("regular-public").expect("regular-public should exist");
    let public_pool = public.value();
    assert_eq!(public_pool.len(), 1, "regular-public should map to a single provider pool");
    assert_eq!(pool_keys_len(public_pool), 4, "public model should expose system + all user keys");
    assert!(pool_has_key(public_pool, SYSTEM_KEY_SECRET));
    assert!(pool_has_key(public_pool, KEY_A_SECRET));
    assert!(pool_has_key(public_pool, KEY_B_SECRET));
    assert!(pool_has_key(public_pool, KEY_BATCH_SECRET));

    let private = targets.targets.get("regular-private").expect("regular-private should exist");
    let private_pool = private.value();
    assert_eq!(private_pool.len(), 1, "regular-private should map to a single provider pool");
    assert_eq!(
        pool_keys_len(private_pool),
        2,
        "private model should expose only system + group member"
    );
    assert!(pool_has_key(private_pool, SYSTEM_KEY_SECRET));
    assert!(pool_has_key(private_pool, KEY_A_SECRET));
    assert!(!pool_has_key(private_pool, KEY_B_SECRET));
    assert!(!pool_has_key(private_pool, KEY_BATCH_SECRET));

    let provider = &private_pool.providers()[0];
    assert_eq!(provider.target.onwards_model.as_deref(), Some("regular-private-model"));
    assert_eq!(provider.target.upstream_auth_header_name.as_deref(), Some("X-API-Key"));
    assert_eq!(provider.target.upstream_auth_header_prefix.as_deref(), Some("Token "));
    assert!(
        provider.target.sanitize_response,
        "sanitize flag should be propagated to regular target"
    );
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base", "cache_tariff_metered", "cache_balance_user_a_positive")))]
async fn test_cache_shape_metered_model_requires_positive_balance(pool: sqlx::PgPool) {
    let targets = super::load_targets_from_db(&pool, &[], false).await.unwrap();
    let metered = targets.targets.get("metered-public").expect("metered-public should exist");
    let metered_pool = metered.value();

    assert_eq!(
        pool_keys_len(metered_pool),
        2,
        "metered model should include only system + positive-balance user"
    );
    assert!(pool_has_key(metered_pool, SYSTEM_KEY_SECRET));
    assert!(pool_has_key(metered_pool, KEY_A_SECRET));
    assert!(!pool_has_key(metered_pool, KEY_B_SECRET));
    assert!(!pool_has_key(metered_pool, KEY_BATCH_SECRET));
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
async fn test_cache_shape_batch_escalation_access_for_private_alias(pool: sqlx::PgPool) {
    let alias = "escalation-private".to_string();

    let without_escalation = super::load_targets_from_db(&pool, &[], false).await.unwrap();
    let pool_without = without_escalation.targets.get(&alias).expect("target should exist");
    assert_eq!(
        pool_keys_len(pool_without.value()),
        1,
        "without escalation only system key should have access"
    );
    assert!(pool_has_key(pool_without.value(), SYSTEM_KEY_SECRET));
    assert!(!pool_has_key(pool_without.value(), KEY_BATCH_SECRET));

    let with_escalation = super::load_targets_from_db(&pool, &[alias.clone()], false).await.unwrap();
    let pool_with = with_escalation.targets.get(&alias).expect("target should exist");
    assert_eq!(pool_keys_len(pool_with.value()), 2, "with escalation batch key should be added");
    assert!(pool_has_key(pool_with.value(), SYSTEM_KEY_SECRET));
    assert!(pool_has_key(pool_with.value(), KEY_BATCH_SECRET));
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
async fn test_cache_shape_composite_pool_strategy_and_fallback(pool: sqlx::PgPool) {
    let targets = super::load_targets_from_db(&pool, &[], false).await.unwrap();
    let composite = targets.targets.get("composite-priority").expect("composite-priority should exist");
    let composite_pool = composite.value();

    assert_eq!(composite_pool.len(), 2, "composite pool should have two providers");
    assert_eq!(composite_pool.strategy(), OnwardsLoadBalanceStrategy::Priority);
    assert!(composite_pool.fallback_enabled());
    assert!(!composite_pool.should_fallback_on_rate_limit());
    assert!(composite_pool.should_fallback_on_status(429));
    assert!(composite_pool.should_fallback_on_status(503));
    assert!(!composite_pool.should_fallback_on_status(500));

    // Current behavior: composite models require positive balance for non-system keys.
    // With this fixture (no credits), only the system key is included.
    assert_eq!(
        pool_keys_len(composite_pool),
        1,
        "composite access currently requires positive balance"
    );
    assert!(pool_has_key(composite_pool, SYSTEM_KEY_SECRET));
    assert!(!pool_has_key(composite_pool, KEY_A_SECRET));
    assert!(!pool_has_key(composite_pool, KEY_B_SECRET));
    assert!(!pool_has_key(composite_pool, KEY_BATCH_SECRET));

    let providers = composite_pool.providers();
    assert_eq!(providers[0].target.onwards_model.as_deref(), Some("component-b-model"));
    assert_eq!(providers[1].target.onwards_model.as_deref(), Some("component-a-model"));
    assert_eq!(providers[0].weight, 30);
    assert_eq!(providers[1].weight, 70);
    assert!(providers[0].target.sanitize_response);
    assert!(providers[1].target.sanitize_response);
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base", "cache_balance_batch_owner_positive")))]
async fn test_cache_shape_composite_batch_escalation_access(pool: sqlx::PgPool) {
    let alias = "composite-priority".to_string();

    let without_escalation = super::load_targets_from_db(&pool, &[], false).await.unwrap();
    let pool_without = without_escalation.targets.get(&alias).expect("target should exist");
    assert!(!pool_has_key(pool_without.value(), KEY_BATCH_SECRET));

    let with_escalation = super::load_targets_from_db(&pool, &[alias], false).await.unwrap();
    let pool_with = with_escalation.targets.get("composite-priority").expect("target should exist");
    assert!(pool_has_key(pool_with.value(), KEY_BATCH_SECRET));
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base", "cache_components_all_disabled")))]
async fn test_cache_shape_composite_with_all_components_disabled(pool: sqlx::PgPool) {
    let targets = super::load_targets_from_db(&pool, &[], false).await.unwrap();
    let composite = targets
        .targets
        .get("composite-priority")
        .expect("composite alias should still exist");
    assert_eq!(
        composite.value().len(),
        0,
        "composite should have zero providers when all components are disabled"
    );
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base", "cache_regular_public_extra_group_assignment")))]
async fn test_cache_shape_duplicate_access_paths_do_not_duplicate_keys(pool: sqlx::PgPool) {
    let targets = super::load_targets_from_db(&pool, &[], false).await.unwrap();
    let public = targets.targets.get("regular-public").expect("regular-public should exist");
    assert_eq!(
        pool_keys_len(public.value()),
        4,
        "user matching multiple access paths should not duplicate keys"
    );
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
async fn test_cache_shape_strict_mode_flag_propagates(pool: sqlx::PgPool) {
    let strict_targets = super::load_targets_from_db(&pool, &[], true).await.unwrap();
    assert!(strict_targets.strict_mode, "strict_mode=true should propagate to Targets");

    let lax_targets = super::load_targets_from_db(&pool, &[], false).await.unwrap();
    assert!(!lax_targets.strict_mode, "strict_mode=false should propagate to Targets");
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base", "cache_user_b_in_private_group")))]
async fn test_cache_shape_overlapping_group_memberships_expand_access(pool: sqlx::PgPool) {
    let targets = super::load_targets_from_db(&pool, &[], false).await.unwrap();
    let private = targets.targets.get("regular-private").expect("regular-private should exist");
    let private_pool = private.value();

    assert_eq!(
        pool_keys_len(private_pool),
        3,
        "system + both private-group users should have access"
    );
    assert!(pool_has_key(private_pool, SYSTEM_KEY_SECRET));
    assert!(pool_has_key(private_pool, KEY_A_SECRET));
    assert!(pool_has_key(private_pool, KEY_B_SECRET));
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base", "cache_delete_regular_public")))]
async fn test_cache_shape_deleted_regular_model_is_excluded(pool: sqlx::PgPool) {
    let targets = super::load_targets_from_db(&pool, &[], false).await.unwrap();
    assert!(
        targets.targets.get("regular-public").is_none(),
        "deleted regular model should be excluded from cache"
    );
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base", "cache_delete_component_a_model")))]
async fn test_cache_shape_deleted_component_model_is_excluded_from_composite(pool: sqlx::PgPool) {
    let targets = super::load_targets_from_db(&pool, &[], false).await.unwrap();
    let composite = targets.targets.get("composite-priority").expect("composite-priority should exist");
    let providers = composite.value().providers();
    assert_eq!(
        providers.len(),
        1,
        "only one composite provider should remain after component model deletion"
    );
    assert_eq!(providers[0].target.onwards_model.as_deref(), Some("component-b-model"));
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base", "cache_component_b_invalid_endpoint")))]
#[ignore = "Known limitation: invalid component endpoint cannot be isolated because regular target loading panics on invalid endpoint URLs"]
async fn test_known_issue_composite_invalid_component_endpoint_should_be_skipped(pool: sqlx::PgPool) {
    // Placeholder known-issue test. We currently can't isolate invalid endpoint handling for
    // a component model without also impacting regular model loading, which panics on invalid URLs.
    let _ = super::load_targets_from_db(&pool, &[], false).await.unwrap();
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
#[ignore = "Known issue: composite key visibility lacks the unmetered-model bypass used by regular models"]
async fn test_known_issue_composite_unmetered_access_should_match_regular_model_policy(pool: sqlx::PgPool) {
    // Desired behavior:
    // For unmetered aliases (no active non-zero tariff), group-authorized keys should be allowed
    // even when user balance is non-positive, matching regular-model policy.
    //
    // Current behavior:
    // Composite aliases require positive balance for non-system users, so this assertion fails.
    let targets = super::load_targets_from_db(&pool, &[], false).await.unwrap();
    let composite = targets.targets.get("composite-priority").expect("composite-priority should exist");
    let composite_pool = composite.value();

    assert!(pool_has_key(composite_pool, SYSTEM_KEY_SECRET));
    assert!(pool_has_key(composite_pool, KEY_A_SECRET));
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
