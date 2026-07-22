use std::{str::FromStr, time::Duration};

use onwards::{
    auth::ConstantTimeString,
    load_balancer::ProviderPool,
    target::{LoadBalanceStrategy as OnwardsLoadBalanceStrategy, RoutingAction, TargetSpecOrList},
};
use tokio::{sync::mpsc, time::timeout};
use tokio_util::sync::CancellationToken;

use crate::config::RateLimitTiersConfig;
use crate::sync::onwards_config::{OnwardsTarget, SyncConfig, convert_to_config_file, parse_notify_payload};

#[test]
fn test_balance_eligibility_reads_read_model_and_filters_deleted_users() {
    let source = include_str!("mod.rs");

    // Both eligibility queries (regular + composite models) read the total
    // user_balance_checkpoints read model as a point lookup - never the
    // credits_transactions ledger.
    assert_eq!(source.matches("FROM user_balance_checkpoints ub").count(), 2);
    assert_eq!(source.matches("credits_transactions").count(), 0);

    // Key deletion is not implied by user deletion, so the deleted-user guard
    // on the balance arm is load-bearing in both queries.
    assert_eq!(source.matches("u.is_deleted = false AND EXISTS").count(), 2);
}

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
        reasoning_translation: None,
        endpoint_url: url::Url::parse(endpoint_url).unwrap(),
        routing_rules: Vec::new(),
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
    let config = convert_to_config_file(targets, vec![], false, &RateLimitTiersConfig::default());

    // Verify the config
    assert_eq!(config.targets.len(), 2);

    // Check model1 (using alias as key)
    let target1 = &config.targets["gpt4-alias"];
    if let TargetSpecOrList::Pool(pool) = target1 {
        assert_eq!(pool.providers.len(), 1);
        assert_eq!(pool.providers[0].url.as_str(), "https://api.openai.com/");
        assert_eq!(pool.providers[0].onwards_model, Some("gpt-4".to_string()));
        // Since we provided empty key data, targets should have no keys configured
        assert!(pool.keys.is_none() || pool.keys.as_ref().unwrap().is_empty());
    } else {
        panic!("Expected Pool target spec");
    }

    // Check model2 (using alias as key)
    let target2 = &config.targets["claude-alias"];
    if let TargetSpecOrList::Pool(pool) = target2 {
        assert_eq!(pool.providers.len(), 1);
        assert_eq!(pool.providers[0].url.as_str(), "https://api.anthropic.com/");
        assert_eq!(pool.providers[0].onwards_model, Some("claude-3".to_string()));
        assert!(pool.keys.is_none() || pool.keys.as_ref().unwrap().is_empty());
    } else {
        panic!("Expected Pool target spec");
    }
}

#[test]
fn test_convert_to_config_file_with_single_target() {
    // Create a single test target
    let target = create_test_target("valid-model", "valid-alias", "https://api.valid.com");

    let targets = vec![target];
    let config = convert_to_config_file(targets, vec![], false, &RateLimitTiersConfig::default());

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
    let targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();

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

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
async fn test_endpoint_reasoning_default_reaches_standard_provider(pool: sqlx::PgPool) {
    let endpoint_config = serde_json::json!({
        "chat_completions": {
            "unsupported_efforts": ["minimal", "xhigh", "max"],
            "writes": [{
                "target_path": "/chat_template_kwargs/thinking",
                "values": {"none": false, "low": true, "medium": true, "high": true}
            }]
        }
    });
    sqlx::query("UPDATE inference_endpoints SET reasoning_translation = $1 WHERE id = '30000000-0000-0000-0000-000000000002'")
        .bind(endpoint_config)
        .execute(&pool)
        .await
        .unwrap();

    let targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    let target = targets.targets.get("regular-private").unwrap();
    let provider = &target.value().providers()[0];
    assert_eq!(
        provider
            .target
            .reasoning_translation
            .as_ref()
            .unwrap()
            .chat_completions
            .as_ref()
            .unwrap()
            .writes[0]
            .target_path,
        "/chat_template_kwargs/thinking"
    );
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
async fn test_chat_override_preserves_endpoint_responses_default(pool: sqlx::PgPool) {
    let endpoint_config = serde_json::json!({
        "chat_completions": {
            "unsupported_efforts": ["minimal", "xhigh", "max"],
            "writes": [{
                "target_path": "/chat_template_kwargs/thinking",
                "values": {"none": false, "low": true, "medium": true, "high": true}
            }]
        },
        "responses": {
            "unsupported_efforts": [],
            "writes": [{
                "target_path": "/reasoning/effort",
                "values": {
                    "none": "none", "minimal": "minimal", "low": "low", "medium": "medium",
                    "high": "high", "xhigh": "xhigh", "max": "max"
                }
            }]
        }
    });
    sqlx::query("UPDATE inference_endpoints SET reasoning_translation = $1 WHERE id = '30000000-0000-0000-0000-000000000002'")
        .bind(endpoint_config)
        .execute(&pool)
        .await
        .unwrap();

    let model_override = serde_json::json!({
        "chat_completions": {
            "mode": "override",
            "translation": {
                "unsupported_efforts": ["minimal", "xhigh", "max"],
                "writes": [{
                    "target_path": "/thinking/type",
                    "values": {"none": "disabled", "low": "enabled", "medium": "enabled", "high": "enabled"}
                }]
            }
        },
        "responses": {"mode": "inherit"}
    });
    sqlx::query("UPDATE deployed_models SET reasoning_translation_overrides = $1 WHERE alias = 'regular-private'")
        .bind(model_override)
        .execute(&pool)
        .await
        .unwrap();

    let targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    let target = targets.targets.get("regular-private").unwrap();
    let provider = &target.value().providers()[0];
    assert_eq!(
        provider
            .target
            .reasoning_translation
            .as_ref()
            .unwrap()
            .chat_completions
            .as_ref()
            .unwrap()
            .writes[0]
            .target_path,
        "/thinking/type"
    );
    assert_eq!(
        provider
            .target
            .reasoning_translation
            .as_ref()
            .unwrap()
            .responses
            .as_ref()
            .unwrap()
            .writes[0]
            .target_path,
        "/reasoning/effort"
    );
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
async fn test_disabling_one_reasoning_surface_preserves_the_other(pool: sqlx::PgPool) {
    let endpoint_config = serde_json::json!({
        "chat_completions": {
            "unsupported_efforts": ["minimal", "xhigh", "max"],
            "writes": [{
                "target_path": "/chat_template_kwargs/thinking",
                "values": {"none": false, "low": true, "medium": true, "high": true}
            }]
        },
        "responses": {
            "unsupported_efforts": [],
            "writes": [{
                "target_path": "/reasoning/effort",
                "values": {
                    "none": "none", "minimal": "minimal", "low": "low", "medium": "medium",
                    "high": "high", "xhigh": "xhigh", "max": "max"
                }
            }]
        }
    });
    sqlx::query("UPDATE inference_endpoints SET reasoning_translation = $1 WHERE id = '30000000-0000-0000-0000-000000000002'")
        .bind(endpoint_config)
        .execute(&pool)
        .await
        .unwrap();
    let model_overrides = serde_json::json!({
        "chat_completions": {"mode": "disabled"},
        "responses": {"mode": "inherit"}
    });
    sqlx::query("UPDATE deployed_models SET reasoning_translation_overrides = $1 WHERE alias = 'regular-private'")
        .bind(model_overrides)
        .execute(&pool)
        .await
        .unwrap();

    let targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    let target = targets.targets.get("regular-private").unwrap();
    let pool = target.value();
    let provider = &pool.providers()[0];
    let config = provider.target.reasoning_translation.as_ref().unwrap();
    assert!(config.chat_completions.is_none());
    assert!(config.responses.is_some());
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
async fn test_disabling_both_reasoning_surfaces_removes_provider_config(pool: sqlx::PgPool) {
    let endpoint_config = serde_json::json!({
        "chat_completions": {
            "unsupported_efforts": ["minimal", "xhigh", "max"],
            "writes": [{
                "target_path": "/chat_template_kwargs/thinking",
                "values": {"none": false, "low": true, "medium": true, "high": true}
            }]
        },
        "responses": {
            "unsupported_efforts": [],
            "writes": [{
                "target_path": "/reasoning/effort",
                "values": {
                    "none": "none", "minimal": "minimal", "low": "low", "medium": "medium",
                    "high": "high", "xhigh": "xhigh", "max": "max"
                }
            }]
        }
    });
    sqlx::query("UPDATE inference_endpoints SET reasoning_translation = $1 WHERE id = '30000000-0000-0000-0000-000000000002'")
        .bind(endpoint_config)
        .execute(&pool)
        .await
        .unwrap();
    let model_overrides = serde_json::json!({
        "chat_completions": {"mode": "disabled"},
        "responses": {"mode": "disabled"}
    });
    sqlx::query("UPDATE deployed_models SET reasoning_translation_overrides = $1 WHERE alias = 'regular-private'")
        .bind(model_overrides)
        .execute(&pool)
        .await
        .unwrap();

    let targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    let target = targets.targets.get("regular-private").unwrap();
    let pool = target.value();
    let provider = &pool.providers()[0];
    assert!(provider.target.reasoning_translation.is_none());
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
async fn test_token_budget_multi_write_survives_provider_sync(pool: sqlx::PgPool) {
    let endpoint_config = serde_json::json!({
        "chat_completions": {
            "unsupported_efforts": ["none", "minimal", "low", "medium", "xhigh", "max"],
            "writes": [
                {"target_path": "/reasoning_effort", "values": {"high": "high"}},
                {"target_path": "/thinking_token_budget", "values": {"high": 8192}}
            ]
        }
    });
    sqlx::query("UPDATE inference_endpoints SET reasoning_translation = $1 WHERE id = '30000000-0000-0000-0000-000000000002'")
        .bind(endpoint_config)
        .execute(&pool)
        .await
        .unwrap();

    let targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    let target = targets.targets.get("regular-private").unwrap();
    let pool = target.value();
    let provider = &pool.providers()[0];
    let writes = &provider
        .target
        .reasoning_translation
        .as_ref()
        .unwrap()
        .chat_completions
        .as_ref()
        .unwrap()
        .writes;
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0].target_path, "/reasoning_effort");
    assert_eq!(writes[1].target_path, "/thinking_token_budget");
    assert_eq!(
        writes[1].values[&onwards::reasoning::ReasoningEffort::High],
        serde_json::json!(8192)
    );
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
async fn test_composite_components_keep_distinct_effective_reasoning_translations(pool: sqlx::PgPool) {
    let endpoint_a = serde_json::json!({
        "chat_completions": {
            "unsupported_efforts": ["minimal", "xhigh", "max"],
            "writes": [{
                "target_path": "/chat_template_kwargs/thinking",
                "values": {"none": false, "low": true, "medium": true, "high": true}
            }]
        }
    });
    let endpoint_b = serde_json::json!({
        "chat_completions": {
            "unsupported_efforts": [],
            "writes": [{
                "target_path": "/reasoning_effort",
                "values": {
                    "none": "none", "minimal": "minimal", "low": "low", "medium": "medium",
                    "high": "high", "xhigh": "xhigh", "max": "max"
                }
            }]
        }
    });
    sqlx::query("UPDATE inference_endpoints SET reasoning_translation = CASE id WHEN '30000000-0000-0000-0000-000000000001' THEN $1::jsonb ELSE $2::jsonb END WHERE id IN ('30000000-0000-0000-0000-000000000001', '30000000-0000-0000-0000-000000000002')")
        .bind(endpoint_a)
        .bind(endpoint_b)
        .execute(&pool)
        .await
        .unwrap();
    let component_b_override = serde_json::json!({
        "chat_completions": {
            "mode": "override",
            "translation": {
                "unsupported_efforts": ["minimal", "xhigh", "max"],
                "writes": [{
                    "target_path": "/thinking/type",
                    "values": {"none": "disabled", "low": "enabled", "medium": "enabled", "high": "enabled"}
                }]
            }
        }
    });
    sqlx::query("UPDATE deployed_models SET reasoning_translation_overrides = $1 WHERE alias = 'component-b'")
        .bind(component_b_override)
        .execute(&pool)
        .await
        .unwrap();

    let targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    let target = targets.targets.get("composite-priority").unwrap();
    let pool = target.value();
    let providers = pool.providers();
    let paths = providers
        .iter()
        .map(|provider| {
            let path = provider
                .target
                .reasoning_translation
                .as_ref()
                .unwrap()
                .chat_completions
                .as_ref()
                .unwrap()
                .writes[0]
                .target_path
                .as_str();
            (provider.target.onwards_model.as_deref().unwrap(), path)
        })
        .collect::<std::collections::HashMap<_, _>>();

    assert_eq!(paths["component-a-model"], "/chat_template_kwargs/thinking");
    assert_eq!(paths["component-b-model"], "/thinking/type");
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
async fn test_cache_shape_zero_data_retention_label_reflects_owner(pool: sqlx::PgPool) {
    // User A opts into zero data retention; User B does not. The onwards sync
    // must surface the owning user's flag as a per-key "zdr" label so onwards
    // can act on it later. The label is always emitted ("true"/"false").
    sqlx::query!("UPDATE users SET zero_data_retention = true WHERE id = '00000000-0000-0000-0000-0000000000a1'")
        .execute(&pool)
        .await
        .unwrap();

    let targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();

    let key_a_labels = targets.key_labels.get(KEY_A_SECRET).expect("user A's key should carry labels");
    assert_eq!(
        key_a_labels.get("zdr"),
        Some(&"true".to_string()),
        "ZDR-enabled owner's key must be labelled true"
    );
    assert_eq!(key_a_labels.get("purpose"), Some(&"realtime".to_string()));

    let key_b_labels = targets.key_labels.get(KEY_B_SECRET).expect("user B's key should carry labels");
    assert_eq!(
        key_b_labels.get("zdr"),
        Some(&"false".to_string()),
        "non-ZDR owner's key must still carry an explicit false label"
    );
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base", "cache_tariff_metered", "cache_balance_user_a_positive")))]
async fn test_cache_shape_metered_model_requires_positive_balance(pool: sqlx::PgPool) {
    let targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
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

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base", "cache_tariff_metered", "cache_balance_user_a_positive")))]
async fn test_balance_change_toggles_paid_access_on_reload(pool: sqlx::PgPool) {
    let user_a: uuid::Uuid = "00000000-0000-0000-0000-0000000000a1".parse().unwrap();
    let tiers = RateLimitTiersConfig::default();

    // Baseline: user A has positive balance, so their key is in the metered pool.
    let targets = super::load_targets_from_db(&pool, &[], false, &tiers).await.unwrap();
    assert!(pool_has_key(targets.targets.get("metered-public").unwrap().value(), KEY_A_SECRET));

    // Deplete user A in the read model, as a usage fold would; the next
    // (crossing-triggered) reload drops their key from paid pools only.
    sqlx::query("UPDATE user_balance_checkpoints SET balance = -1 WHERE user_id = $1")
        .bind(user_a)
        .execute(&pool)
        .await
        .unwrap();

    let targets = super::load_targets_from_db(&pool, &[], false, &tiers).await.unwrap();
    assert!(
        !pool_has_key(targets.targets.get("metered-public").unwrap().value(), KEY_A_SECRET),
        "depleted user must lose paid-model access"
    );
    assert!(
        pool_has_key(targets.targets.get("regular-public").unwrap().value(), KEY_A_SECRET),
        "free-model access survives depletion"
    );
    assert!(
        pool_has_key(targets.targets.get("composite-priority").unwrap().value(), KEY_A_SECRET),
        "free composite access survives depletion"
    );

    // Restore the balance: paid access returns on the next reload.
    sqlx::query("UPDATE user_balance_checkpoints SET balance = 50 WHERE user_id = $1")
        .bind(user_a)
        .execute(&pool)
        .await
        .unwrap();

    let targets = super::load_targets_from_db(&pool, &[], false, &tiers).await.unwrap();
    assert!(
        pool_has_key(targets.targets.get("metered-public").unwrap().value(), KEY_A_SECRET),
        "restored user regains paid-model access"
    );
}

/// Spending-cap gate: an exhausted scope loses paid-model access as a unit
/// (capped root AND its hidden batch child), free models stay usable, one-off
/// caps never self-heal, and a windowed cap readmits at the calendar boundary
/// with no fold or traffic required (the lazy-readmission path the fallback
/// sync provides).
#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base", "cache_tariff_metered", "cache_balance_user_a_positive")))]
async fn test_spend_cap_toggles_scope_access_on_reload(pool: sqlx::PgPool) {
    use crate::db::handlers::api_keys::ApiKeys;

    let tiers = RateLimitTiersConfig::default();
    let key_a_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM api_keys WHERE secret = $1")
        .bind(KEY_A_SECRET)
        .fetch_one(&pool)
        .await
        .unwrap();

    // Cap key A (one-off, $10) and mint its cap-scope child.
    sqlx::query("UPDATE api_keys SET spend_limit = 10 WHERE id = $1")
        .bind(key_a_id)
        .execute(&pool)
        .await
        .unwrap();
    let child_secret = {
        let mut conn = pool.acquire().await.unwrap();
        let (secret, _) = ApiKeys::new(&mut conn).get_or_create_child_hidden_key(key_a_id).await.unwrap();
        secret
    };

    // Under the cap: both scope keys are eligible for the paid pool.
    let targets = super::load_targets_from_db(&pool, &[], false, &tiers).await.unwrap();
    let metered = targets.targets.get("metered-public").unwrap();
    assert!(pool_has_key(metered.value(), KEY_A_SECRET));
    assert!(pool_has_key(metered.value(), &child_secret), "child shares the scope's eligibility");

    // Exhaust the scope: window_spend reaches the limit.
    sqlx::query("INSERT INTO api_key_spend_checkpoints (api_key_id, total_spend, window_spend) VALUES ($1, 10, 10)")
        .bind(key_a_id)
        .execute(&pool)
        .await
        .unwrap();
    let targets = super::load_targets_from_db(&pool, &[], false, &tiers).await.unwrap();
    let metered = targets.targets.get("metered-public").unwrap();
    assert!(!pool_has_key(metered.value(), KEY_A_SECRET), "exhausted scope loses the paid pool");
    assert!(!pool_has_key(metered.value(), &child_secret), "the child is yanked with its root");
    assert!(
        pool_has_key(targets.targets.get("regular-public").unwrap().value(), KEY_A_SECRET),
        "free models stay usable on an exhausted scope, like the balance gate"
    );

    // One-off caps never roll over: an arbitrarily old window stays exhausted.
    sqlx::query("UPDATE api_key_spend_checkpoints SET window_started_at = now() - interval '40 days' WHERE api_key_id = $1")
        .bind(key_a_id)
        .execute(&pool)
        .await
        .unwrap();
    let targets = super::load_targets_from_db(&pool, &[], false, &tiers).await.unwrap();
    assert!(
        !pool_has_key(targets.targets.get("metered-public").unwrap().value(), KEY_A_SECRET),
        "one-off cap must not self-heal"
    );

    // Windowed cap: the same stale window is no longer current, so the next
    // reload readmits the whole scope — no fold, no traffic, no reset job.
    sqlx::query("UPDATE api_keys SET spend_limit_interval = 'daily' WHERE id = $1")
        .bind(key_a_id)
        .execute(&pool)
        .await
        .unwrap();
    let targets = super::load_targets_from_db(&pool, &[], false, &tiers).await.unwrap();
    let metered = targets.targets.get("metered-public").unwrap();
    assert!(pool_has_key(metered.value(), KEY_A_SECRET), "rolled window readmits the root");
    assert!(pool_has_key(metered.value(), &child_secret), "rolled window readmits the child");
}

/// Semantics of `api_key_cap_near_boundary` (migration 123), the pre-boundary
/// readmission grace used by the sync predicate and the error enricher. Tested
/// with injected grace values because `now()` cannot be mocked in SQL: grace 0
/// is never near a boundary, a grace longer than the period always is, and
/// one-off caps (NULL interval) have no boundary at all.
#[sqlx::test]
async fn test_cap_near_boundary_function_semantics(pool: sqlx::PgPool) {
    let check = |interval: Option<&'static str>, grace: i32| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, bool>("SELECT api_key_cap_near_boundary($1, $2)")
                .bind(interval)
                .bind(grace)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };

    // Grace longer than the whole period: always inside it.
    assert!(check(Some("daily"), 90_000).await); // > 24h
    assert!(check(Some("weekly"), 700_000).await); // > 7d
    assert!(check(Some("monthly"), 3_000_000).await); // > 31d

    // Zero grace: never near (now() is strictly before the boundary).
    assert!(!check(Some("daily"), 0).await);
    assert!(!check(Some("weekly"), 0).await);
    assert!(!check(Some("monthly"), 0).await);

    // One-off caps have no boundary regardless of grace.
    assert!(!check(None, 3_000_000).await);

    // Unknown intervals (prevented by CHECK constraint) fail safe to "not near".
    assert!(!check(Some("hourly"), 3_000_000).await);
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
async fn test_cache_shape_batch_escalation_access_for_private_alias(pool: sqlx::PgPool) {
    let alias = "escalation-private".to_string();

    let without_escalation = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    let pool_without = without_escalation.targets.get(&alias).expect("target should exist");
    assert_eq!(
        pool_keys_len(pool_without.value()),
        1,
        "without escalation only system key should have access"
    );
    assert!(pool_has_key(pool_without.value(), SYSTEM_KEY_SECRET));
    assert!(!pool_has_key(pool_without.value(), KEY_BATCH_SECRET));

    let with_escalation = super::load_targets_from_db(&pool, std::slice::from_ref(&alias), false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    let pool_with = with_escalation.targets.get(&alias).expect("target should exist");
    assert_eq!(pool_keys_len(pool_with.value()), 2, "with escalation batch key should be added");
    assert!(pool_has_key(pool_with.value(), SYSTEM_KEY_SECRET));
    assert!(pool_has_key(pool_with.value(), KEY_BATCH_SECRET));
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
async fn test_cache_shape_composite_pool_strategy_and_fallback(pool: sqlx::PgPool) {
    let targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    let composite = targets.targets.get("composite-priority").expect("composite-priority should exist");
    let composite_pool = composite.value();

    assert_eq!(composite_pool.len(), 2, "composite pool should have two providers");
    assert_eq!(composite_pool.strategy(), OnwardsLoadBalanceStrategy::Priority);
    assert!(composite_pool.fallback_enabled());
    assert!(!composite_pool.should_fallback_on_rate_limit());
    assert!(composite_pool.should_fallback_on_status(429));
    assert!(!composite_pool.should_fallback_on_status(499));
    assert!(composite_pool.should_fallback_on_status(503));
    assert!(!composite_pool.should_fallback_on_status(500));

    let fallback = composite_pool.fallback().expect("fallback should be set");
    assert_eq!(
        fallback.on_status,
        vec![429, 503],
        "explicit stored statuses must remain authoritative"
    );

    // Composite model has no tariff in this fixture, so it's free.
    // Free models allow group-authorized keys regardless of balance.
    // User A is in group cache-private-a which has access to this composite.
    assert_eq!(
        pool_keys_len(composite_pool),
        2,
        "free composite should include system key and group-authorized keys"
    );
    assert!(pool_has_key(composite_pool, SYSTEM_KEY_SECRET));
    assert!(pool_has_key(composite_pool, KEY_A_SECRET));
    assert!(!pool_has_key(composite_pool, KEY_B_SECRET));
    assert!(!pool_has_key(composite_pool, KEY_BATCH_SECRET));

    let providers = composite_pool.providers();
    assert_eq!(providers[0].target.onwards_model.as_deref(), Some("component-b-model"));
    assert_eq!(providers[1].target.onwards_model.as_deref(), Some("component-a-model"));
    assert_eq!(providers[0].weight, 30);
    assert_eq!(providers[1].weight, 70);
    assert!(providers[0].target.sanitize_response);
    assert!(providers[1].target.sanitize_response);

    // Default migration state: no backoff configured. The fallback config
    // surfaces to onwards with `backoff: None`, which preserves the legacy
    // zero-delay retry behavior for composites that haven't opted in.
    assert!(fallback.backoff.is_none(), "backoff should default to None");
    assert!(fallback.max_total_backoff_ms.is_none());
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
async fn test_cache_shape_null_composite_uses_application_fallback_status_default(pool: sqlx::PgPool) {
    sqlx::query("UPDATE deployed_models SET fallback_on_status = NULL WHERE alias = 'composite-priority'")
        .execute(&pool)
        .await
        .unwrap();

    let targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    let composite = targets.targets.get("composite-priority").expect("composite-priority should exist");
    let fallback = composite.value().fallback().expect("fallback should be set");

    assert_eq!(fallback.on_status, vec![429, 499, 500, 502, 503, 504]);
}

/// When an admin sets the per-model backoff knobs on a composite, the values
/// round-trip from the deployed_models row through the sync layer into the
/// in-memory `onwards::FallbackConfig.backoff`. The migration's DB CHECK
/// constraints reject silly values, so the conversion can trust them.
#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
async fn test_cache_shape_composite_backoff_round_trips(pool: sqlx::PgPool) {
    sqlx::query!(
        r#"
        UPDATE deployed_models
           SET backoff_enabled = TRUE,
               backoff_initial_ms = 250,
               backoff_max_ms = 4000,
               backoff_factor = 3.0,
               backoff_jitter = 'none',
               backoff_max_total_ms = 6000
         WHERE alias = 'composite-priority'
        "#
    )
    .execute(&pool)
    .await
    .unwrap();

    let targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    let composite = targets.targets.get("composite-priority").expect("composite-priority should exist");
    let fallback = composite.value().fallback().expect("fallback should be present");

    let backoff = fallback.backoff.as_ref().expect("backoff should be Some");
    assert_eq!(backoff.initial_ms, 250);
    assert_eq!(backoff.max_ms, 4_000);
    assert!((backoff.factor - 3.0).abs() < f64::EPSILON);
    assert_eq!(backoff.jitter, onwards::target::JitterStrategy::None);
    assert_eq!(fallback.max_total_backoff_ms, Some(6_000));
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base", "cache_balance_batch_owner_positive")))]
async fn test_cache_shape_composite_batch_escalation_access(pool: sqlx::PgPool) {
    let alias = "composite-priority".to_string();

    let without_escalation = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    let pool_without = without_escalation.targets.get(&alias).expect("target should exist");
    assert!(!pool_has_key(pool_without.value(), KEY_BATCH_SECRET));

    let with_escalation = super::load_targets_from_db(&pool, &[alias], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    let pool_with = with_escalation.targets.get("composite-priority").expect("target should exist");
    assert!(pool_has_key(pool_with.value(), KEY_BATCH_SECRET));
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base", "cache_components_all_disabled")))]
async fn test_cache_shape_composite_with_all_components_disabled(pool: sqlx::PgPool) {
    let targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    let pool_entry = targets
        .targets
        .get("composite-priority")
        .expect("composite should still exist in cache even with all components disabled");
    assert!(
        pool_entry.is_empty(),
        "composite pool should have zero providers when all components are disabled"
    );
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base", "cache_regular_public_extra_group_assignment")))]
async fn test_cache_shape_duplicate_access_paths_do_not_duplicate_keys(pool: sqlx::PgPool) {
    let targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    let public = targets.targets.get("regular-public").expect("regular-public should exist");
    assert_eq!(
        pool_keys_len(public.value()),
        4,
        "user matching multiple access paths should not duplicate keys"
    );
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
async fn test_cache_shape_strict_mode_flag_propagates(pool: sqlx::PgPool) {
    let strict_targets = super::load_targets_from_db(&pool, &[], true, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    assert!(strict_targets.strict_mode, "strict_mode=true should propagate to Targets");

    let lax_targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    assert!(!lax_targets.strict_mode, "strict_mode=false should propagate to Targets");
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base", "cache_user_b_in_private_group")))]
async fn test_cache_shape_overlapping_group_memberships_expand_access(pool: sqlx::PgPool) {
    let targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
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
    let targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    assert!(
        targets.targets.get("regular-public").is_none(),
        "deleted regular model should be excluded from cache"
    );
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base", "cache_delete_component_a_model")))]
async fn test_cache_shape_deleted_component_model_is_excluded_from_composite(pool: sqlx::PgPool) {
    let targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    let composite = targets.targets.get("composite-priority").expect("composite-priority should exist");
    let providers = composite.value().providers();
    assert_eq!(
        providers.len(),
        1,
        "only one composite provider should remain after component model deletion"
    );
    assert_eq!(providers[0].target.onwards_model.as_deref(), Some("component-b-model"));
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base", "cache_traffic_routing_rules")))]
async fn test_cache_shape_regular_model_routing_rules(pool: sqlx::PgPool) {
    let targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    let regular_private = targets.targets.get("regular-private").expect("regular-private should exist");
    let rules = regular_private.value().routing_rules();

    assert_eq!(rules.len(), 2, "regular-private should expose two routing rules");

    assert_eq!(rules[0].match_labels.get("purpose"), Some(&"batch".to_string()));
    assert!(matches!(rules[0].action, RoutingAction::Deny));

    assert_eq!(rules[1].match_labels.get("purpose"), Some(&"realtime".to_string()));
    match &rules[1].action {
        RoutingAction::Redirect { target } => assert_eq!(target, "regular-public"),
        _ => panic!("expected redirect rule for realtime"),
    }
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base", "cache_traffic_routing_rules")))]
async fn test_cache_shape_composite_model_routing_rules(pool: sqlx::PgPool) {
    let targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    let composite = targets.targets.get("composite-priority").expect("composite-priority should exist");
    let rules = composite.value().routing_rules();

    assert_eq!(rules.len(), 2, "composite-priority should expose two routing rules");

    assert_eq!(rules[0].match_labels.get("purpose"), Some(&"batch".to_string()));
    match &rules[0].action {
        RoutingAction::Redirect { target } => assert_eq!(target, "escalation-private"),
        _ => panic!("expected redirect rule for batch"),
    }

    assert_eq!(rules[1].match_labels.get("purpose"), Some(&"realtime".to_string()));
    assert!(matches!(rules[1].action, RoutingAction::Deny));
}
#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base", "cache_component_b_invalid_endpoint")))]
#[ignore = "Known limitation: invalid component endpoint cannot be isolated because regular target loading panics on invalid endpoint URLs"]
async fn test_known_issue_composite_invalid_component_endpoint_should_be_skipped(pool: sqlx::PgPool) {
    // Expected behavior:
    // - A composite component with an invalid endpoint URL is skipped.
    // - Remaining valid models/components still load.
    // - Loader returns Result::Ok without panicking.
    //
    // Bug outline:
    // - Composite path is defensive and skips invalid URLs.
    // - Regular-model path uses Url::parse(...).expect(...), which panics on invalid DB URL.
    // - Because endpoints are shared across deployments in this fixture, regular loading panics
    //   before we can assert composite skip behavior.
    let _ = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
}

#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
async fn test_composite_unmetered_access_matches_regular_model_policy(pool: sqlx::PgPool) {
    // For unmetered aliases (no active non-zero tariff), group-authorized keys are allowed
    // even when user balance is non-positive. Composite and regular aliases follow the same
    // key visibility policy.
    let targets = super::load_targets_from_db(&pool, &[], false, &RateLimitTiersConfig::default())
        .await
        .unwrap();
    let composite = targets.targets.get("composite-priority").expect("composite-priority should exist");
    let composite_pool = composite.value();

    assert!(pool_has_key(composite_pool, SYSTEM_KEY_SECRET));
    assert!(pool_has_key(composite_pool, KEY_A_SECRET));
}

/// End-to-end check that the verified/unverified tier reaches the onwards
/// limiter for a real key loaded from the DB. Exercises the full path: the
/// `JOIN users` that fetches `verified`, `resolve_key_rate_limit`, and
/// `Targets::from_config` building the governor limiter. We assert behaviour
/// (burst enforcement) rather than the configured numbers because governor
/// does not expose the quota once built.
#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
async fn test_unverified_key_gets_tier_limiter_verified_key_does_not(pool: sqlx::PgPool) {
    use crate::config::RateLimitTierConfig;

    // User A (KEY_A_SECRET) starts unverified (column default) and owns a key
    // with no per-key override, with access to the unmetered `regular-private`
    // model. Configure an unverified tier and leave the verified tier unset.
    let tiers = RateLimitTiersConfig {
        verified: None,
        unverified: Some(RateLimitTierConfig {
            requests_per_second: 1.0,
            burst_size: Some(3),
        }),
    };

    let targets = super::load_targets_from_db(&pool, &[], false, &tiers).await.unwrap();

    // The unverified user's key has a limiter, and it enforces burst = 3:
    // three immediate checks pass, the fourth is throttled. All four run
    // back-to-back so no replenishment happens between them.
    {
        let limiter = targets
            .key_rate_limiters
            .get(KEY_A_SECRET)
            .expect("unverified user's key should have a rate limiter");
        assert!(limiter.check().is_ok(), "1st request within burst");
        assert!(limiter.check().is_ok(), "2nd request within burst");
        assert!(limiter.check().is_ok(), "3rd request within burst");
        assert!(limiter.check().is_err(), "4th request exceeds burst of 3");
    }

    // Flip the user to verified. With the verified tier unset, the key should
    // now have no limiter at all (unlimited).
    sqlx::query!("UPDATE users SET verified = true WHERE id = '00000000-0000-0000-0000-0000000000a1'")
        .execute(&pool)
        .await
        .unwrap();

    let targets = super::load_targets_from_db(&pool, &[], false, &tiers).await.unwrap();
    assert!(
        targets.key_rate_limiters.get(KEY_A_SECRET).is_none(),
        "verified user with an unset verified tier should have no limiter"
    );
}

/// The system key (nil UUID) carries internal traffic (DB probes, deployment
/// access) and must never be subject to a tier limit, even when both tiers are
/// configured restrictively.
#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
async fn test_system_key_is_immune_to_rate_limit_tiers(pool: sqlx::PgPool) {
    use crate::config::RateLimitTierConfig;

    let restrictive = RateLimitTierConfig {
        requests_per_second: 1.0,
        burst_size: Some(1),
    };
    let tiers = RateLimitTiersConfig {
        verified: Some(restrictive),
        unverified: Some(restrictive),
    };

    let targets = super::load_targets_from_db(&pool, &[], false, &tiers).await.unwrap();

    assert!(
        targets.key_rate_limiters.get(SYSTEM_KEY_SECRET).is_none(),
        "system key must never receive a tier rate limiter"
    );
}

/// Endpoint defaults are consumed directly by the Onwards provider cache, so
/// changing one must wake the listener even when no deployment row changes.
#[sqlx::test(fixtures(path = "fixtures", scripts("cache_base")))]
async fn test_endpoint_reasoning_update_notifies_onwards_config_listener(pool: sqlx::PgPool) {
    use crate::config::ONWARDS_CONFIG_CHANGED_CHANNEL;
    use sqlx::postgres::PgListener;

    let mut listener = PgListener::connect_with(&pool).await.unwrap();
    listener.listen(ONWARDS_CONFIG_CHANGED_CHANNEL).await.unwrap();

    let endpoint_id = uuid::Uuid::parse_str("30000000-0000-0000-0000-000000000002").unwrap();
    let reasoning_translation = serde_json::json!({
        "chat_completions": {
            "unsupported_efforts": ["minimal", "xhigh", "max"],
            "writes": [{
                "target_path": "/chat_template_kwargs/thinking",
                "values": {"none": false, "low": true, "medium": true, "high": true}
            }]
        }
    });
    let result = sqlx::query("UPDATE inference_endpoints SET reasoning_translation = $1 WHERE id = $2")
        .bind(reasoning_translation)
        .bind(endpoint_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(result.rows_affected(), 1);

    let notification = timeout(Duration::from_secs(1), listener.recv())
        .await
        .expect("endpoint reasoning update should notify Onwards")
        .unwrap();
    assert_eq!(notification.channel(), ONWARDS_CONFIG_CHANGED_CHANNEL);
    let (table, _) = parse_notify_payload(notification.payload()).expect("timestamped notify payload");
    assert_eq!(table, "inference_endpoints");
}

/// Test that tariff changes trigger onwards config reload via Postgres NOTIFY
#[sqlx::test]
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
            reasoning_translation: None,
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
            display_name: None,
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
            backoff_enabled: false,
            backoff_initial_ms: 100,
            backoff_max_ms: 5_000,
            backoff_factor: 2.0,
            backoff_jitter: "full".to_string(),
            backoff_max_total_ms: None,
            sanitize_responses: true,
            trusted: false,
            open_responses_adapter: true,
            reasoning_translation_overrides: None,
            allowed_batch_completion_windows: None,
            metadata: None,
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

/// Regression test for the api_keys NOTIFY storm.
///
/// `get_or_create_hidden_key` runs `INSERT ... ON CONFLICT DO NOTHING` ~15k+
/// times/day. On the common "key already exists" path it changes nothing, so it
/// must NOT trigger a full onwards config reload. The statement-level NOTIFY
/// triggers use transition tables and notify only when rows were actually
/// inserted/deleted or an auth-relevant column changed, so a no-op upsert
/// (empty NEW transition table) stays silent.
///
/// Covers: no-op upsert (silent), real insert (notify), metadata-only update
/// (silent), auth-relevant update (notify).
#[sqlx::test]
async fn test_api_keys_noop_upsert_does_not_trigger_notify(pool: sqlx::PgPool) {
    use sqlx::postgres::PgListener;

    use crate::Role;
    use crate::db::handlers::api_keys::ApiKeys;
    use crate::db::models::api_keys::ApiKeyPurpose;

    let user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

    let mut listener = PgListener::connect_with(&pool).await.unwrap();
    listener.listen("auth_config_changed").await.unwrap();

    // First call actually inserts the hidden key -> must notify.
    let key_id = {
        let mut tx = pool.begin().await.unwrap();
        let mut repo = ApiKeys::new(&mut tx);
        let (_secret, id) = repo
            .get_or_create_hidden_key_with_id(user.id, ApiKeyPurpose::Realtime, user.id)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        id
    };
    let first = timeout(Duration::from_secs(2), listener.recv())
        .await
        .expect("first get_or_create_hidden_key (real insert) should notify")
        .expect("failed to receive notification");
    assert!(
        first.payload().contains("api_keys"),
        "insert notification should reference api_keys, got: {}",
        first.payload()
    );

    // Drain anything still pending from the first insert. Loop only while an actual
    // notification is received -- `.is_ok()` would also be true for Ok(None) (no
    // notification / closed connection) and could spin.
    while let Ok(Ok(Some(_))) = timeout(Duration::from_millis(50), listener.try_recv()).await {}

    // Second call hits ON CONFLICT DO NOTHING (0 rows written) -> must NOT notify.
    {
        let mut tx = pool.begin().await.unwrap();
        let mut repo = ApiKeys::new(&mut tx);
        repo.get_or_create_hidden_key(user.id, ApiKeyPurpose::Realtime, user.id)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    match timeout(Duration::from_millis(750), listener.recv()).await {
        Err(_) => {} // timed out waiting for a notification -> correct: no-op did not notify
        Ok(Ok(n)) => panic!("no-op upsert must NOT trigger a config-change notification, got: {}", n.payload()),
        Ok(Err(e)) => panic!("listener error: {e}"),
    }

    // Metadata-only UPDATE (name) is not consumed by onwards -> must NOT notify.
    sqlx::query("UPDATE api_keys SET name = 'renamed' WHERE id = $1")
        .bind(key_id)
        .execute(&pool)
        .await
        .unwrap();
    match timeout(Duration::from_millis(500), listener.recv()).await {
        Err(_) => {} // correct: metadata-only update did not notify
        Ok(Ok(n)) => panic!("metadata-only update must NOT notify, got: {}", n.payload()),
        Ok(Err(e)) => panic!("listener error: {e}"),
    }

    // Auth-relevant UPDATE (requests_per_second) -> must notify.
    sqlx::query("UPDATE api_keys SET requests_per_second = 5 WHERE id = $1")
        .bind(key_id)
        .execute(&pool)
        .await
        .unwrap();
    let updated = timeout(Duration::from_secs(2), listener.recv())
        .await
        .expect("auth-relevant update should notify")
        .expect("failed to receive notification");
    assert!(
        updated.payload().contains("api_keys"),
        "update notification should reference api_keys, got: {}",
        updated.payload()
    );
}

/// Test that batch API keys get automatic access to composite escalation targets
#[sqlx::test]
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
            reasoning_translation: None,
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
            display_name: None,
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
            backoff_enabled: false,
            backoff_initial_ms: 100,
            backoff_max_ms: 5_000,
            backoff_factor: 2.0,
            backoff_jitter: "full".to_string(),
            backoff_max_total_ms: None,
            allowed_batch_completion_windows: None,
            metadata: None,
            sanitize_responses: true,
            trusted: false,
            open_responses_adapter: true,
            reasoning_translation_overrides: None,
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
            display_name: None,
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
            fallback_on_status: Some(vec![429, 499, 500, 502, 503, 504]),
            fallback_with_replacement: None,
            allowed_batch_completion_windows: None,
            fallback_max_attempts: None,
            backoff_enabled: false,
            backoff_initial_ms: 100,
            backoff_max_ms: 5_000,
            backoff_factor: 2.0,
            backoff_jitter: "full".to_string(),
            backoff_max_total_ms: None,
            metadata: None,
            sanitize_responses: true,
            trusted: false,
            open_responses_adapter: true,
            reasoning_translation_overrides: None,
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
            created_by: test_user.id,
        })
        .await
        .unwrap();
    api_key_tx.commit().await.unwrap();

    // Load targets with composite alias in escalation_models
    let escalation_models = vec![composite_alias.clone()];
    let targets = super::load_targets_from_db(&pool, &escalation_models, false, &RateLimitTiersConfig::default())
        .await
        .unwrap();

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

/// Test that fallback sync triggers periodic reloads even without LISTEN/NOTIFY activity
#[sqlx::test]
#[test_log::test]
async fn test_fallback_sync_triggers_without_notifications(pool: sqlx::PgPool) {
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    // Create the sync service
    let (sync, _initial_targets, _stream) = super::OnwardsConfigSync::new(pool.clone())
        .await
        .expect("Failed to create OnwardsConfigSync");

    // Create sync config with 20ms fallback interval for fast testing
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
    // Use interval to poll every 100ms for 500ms total (at least 2 fallback syncs at 20ms each)
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

#[cfg(test)]
mod resolve_key_rate_limit_tests {
    use super::*;
    use crate::config::RateLimitTierConfig;
    use std::num::NonZeroU32;

    fn tiers(verified: Option<(f32, Option<i32>)>, unverified: Option<(f32, Option<i32>)>) -> RateLimitTiersConfig {
        let make = |t: (f32, Option<i32>)| RateLimitTierConfig {
            requests_per_second: t.0,
            burst_size: t.1,
        };
        RateLimitTiersConfig {
            verified: verified.map(make),
            unverified: unverified.map(make),
        }
    }

    #[test]
    fn per_key_override_beats_tier() {
        let t = tiers(Some((1.0, None)), Some((2.0, None)));
        let rl = super::super::resolve_key_rate_limit(Some(10.0), Some(20), true, &t).unwrap();
        assert_eq!(rl.requests_per_second, NonZeroU32::new(10).unwrap());
        assert_eq!(rl.burst_size, Some(NonZeroU32::new(20).unwrap()));
    }

    #[test]
    fn unverified_user_with_no_override_gets_unverified_tier() {
        let t = tiers(Some((100.0, None)), Some((5.0, Some(10))));
        let rl = super::super::resolve_key_rate_limit(None, None, false, &t).unwrap();
        assert_eq!(rl.requests_per_second, NonZeroU32::new(5).unwrap());
        assert_eq!(rl.burst_size, Some(NonZeroU32::new(10).unwrap()));
    }

    #[test]
    fn verified_user_with_no_override_gets_verified_tier() {
        let t = tiers(Some((100.0, None)), Some((5.0, None)));
        let rl = super::super::resolve_key_rate_limit(None, None, true, &t).unwrap();
        assert_eq!(rl.requests_per_second, NonZeroU32::new(100).unwrap());
    }

    #[test]
    fn no_tier_configured_and_no_override_means_no_limit() {
        let t = tiers(None, None);
        assert!(super::super::resolve_key_rate_limit(None, None, false, &t).is_none());
        assert!(super::super::resolve_key_rate_limit(None, None, true, &t).is_none());
    }

    #[test]
    fn only_one_tier_configured_other_tier_unrestricted() {
        let t = tiers(None, Some((5.0, None)));
        // Verified user falls through to None because verified tier is unset.
        assert!(super::super::resolve_key_rate_limit(None, None, true, &t).is_none());
        // Unverified user gets the configured tier.
        let rl = super::super::resolve_key_rate_limit(None, None, false, &t).unwrap();
        assert_eq!(rl.requests_per_second, NonZeroU32::new(5).unwrap());
    }
}
