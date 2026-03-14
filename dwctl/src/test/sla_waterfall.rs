//! SLA waterfall billing integration test
//!
//! Verifies that batch requests are priced at the correct SLA tier based on
//! elapsed time since batch creation:
//!
//! - Requests completing within the submitted window → charged at that tier (e.g. "1h")
//! - Requests completing after the submitted window but within a longer tier → falls
//!   through to cheaper pricing (e.g. "24h")
//! - Requests completing after all configured windows → free
//!
//! The waterfall is implemented in `batcher.rs`. The effective tier is not stored
//! in the database — it can be derived from the request timestamps and tariffs.
//! This test verifies the correct pricing was applied by grouping on
//! `input_price_per_token`.
//!
//! Instead of running the fusillade daemon, this test sends requests directly to
//! the onwards proxy with the batch headers that the analytics handler extracts.
//! This gives deterministic control over `batch_created_at` for each request.

use crate::api::models::users::Role;
use crate::db::handlers::api_keys::ApiKeys;
use crate::db::models::api_keys::ApiKeyPurpose;
use crate::test::utils::{add_auth_headers, create_test_admin_user, create_test_config, create_test_user};
use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::str::FromStr;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROMPT_TOKENS: i64 = 100;
const COMPLETION_TOKENS: i64 = 50;

/// Tariff prices for the test model
const INPUT_1H: &str = "0.00001000";
const OUTPUT_1H: &str = "0.00002000";
const INPUT_24H: &str = "0.00000100";
const OUTPUT_24H: &str = "0.00000200";

/// Pricing tier breakdown from http_analytics
#[derive(Debug)]
struct PricingTierResult {
    input_price: Decimal,
    output_price: Decimal,
    count: i64,
}

/// Query the pricing breakdown for a batch from http_analytics.
/// Groups by price to verify each tier has the expected number of requests.
async fn get_pricing_breakdown(pool: &PgPool, batch_id: Uuid) -> Vec<PricingTierResult> {
    sqlx::query_as::<_, (Decimal, Decimal, i64)>(
        r#"
        SELECT input_price_per_token, output_price_per_token, COUNT(*)
        FROM http_analytics
        WHERE fusillade_batch_id = $1 AND status_code BETWEEN 200 AND 299
        GROUP BY input_price_per_token, output_price_per_token
        ORDER BY input_price_per_token DESC
        "#,
    )
    .bind(batch_id)
    .fetch_all(pool)
    .await
    .expect("Failed to query pricing breakdown")
    .into_iter()
    .map(|(input, output, count)| PricingTierResult {
        input_price: input,
        output_price: output,
        count,
    })
    .collect()
}

/// Wait for analytics rows to appear for a batch (batcher flushes asynchronously).
async fn wait_for_analytics(pool: &PgPool, batch_id: Uuid, target: i64, timeout_secs: u64) {
    let start = tokio::time::Instant::now();
    let timeout = tokio::time::Duration::from_secs(timeout_secs);

    while start.elapsed() < timeout {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM http_analytics WHERE fusillade_batch_id = $1 AND status_code BETWEEN 200 AND 299")
                .bind(batch_id)
                .fetch_one(pool)
                .await
                .expect("Failed to query analytics");

        if count >= target {
            return;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }

    panic!("Timed out waiting for {} analytics rows for batch {}", target, batch_id);
}

/// All 3 SLA tiers in a single test: sends requests with different batch_created_at
/// timestamps to verify waterfall pricing across 1h, 24h, and free tiers.
#[sqlx::test]
#[test_log::test]
async fn test_sla_waterfall_all_tiers(pool: PgPool) {
    // Wiremock simulates the upstream model
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1677652288,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "Hello" },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": PROMPT_TOKENS,
                "completion_tokens": COMPLETION_TOKENS,
                "total_tokens": PROMPT_TOKENS + COMPLETION_TOKENS
            }
        })))
        .mount(&mock_server)
        .await;

    // Create app with onwards sync enabled
    let mut config = create_test_config();
    config.background_services.onwards_sync.enabled = true;
    config.background_services.probe_scheduler.enabled = false;
    config.background_services.leader_election.enabled = false;

    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("Failed to create application");
    let (server, bg_services) = app.into_test_server();

    // Create admin and regular user
    let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
    let admin_headers = add_auth_headers(&admin_user);
    let user = create_test_user(&pool, Role::StandardUser).await;

    // Admin creates group and adds user
    let group_response = server
        .post("/admin/api/v1/groups")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "name": "test-group",
            "description": "Test group for SLA waterfall"
        }))
        .await;
    assert_eq!(group_response.status_code(), 201, "Failed to create group");
    let group: serde_json::Value = group_response.json();
    let group_id = group["id"].as_str().unwrap();

    let add_user_resp = server
        .post(&format!("/admin/api/v1/groups/{}/users/{}", group_id, user.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;
    assert_eq!(add_user_resp.status_code(), 204, "Failed to add user to group");

    // Admin grants credits
    let credits_resp = server
        .post("/admin/api/v1/transactions")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "user_id": user.id,
            "transaction_type": "admin_grant",
            "amount": 10000,
            "source_id": admin_user.id,
            "description": "Test credits"
        }))
        .await;
    assert_eq!(credits_resp.status_code(), 201, "Failed to grant credits");

    // Admin creates inference endpoint pointing to wiremock
    let mock_endpoint_url = format!("{}/v1", mock_server.uri());
    let endpoint_resp = server
        .post("/admin/api/v1/endpoints")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "name": "Mock Endpoint",
            "url": mock_endpoint_url,
            "description": "Wiremock endpoint for SLA waterfall test"
        }))
        .await;
    assert_eq!(endpoint_resp.status_code(), 201, "Failed to create endpoint");
    let endpoint: serde_json::Value = endpoint_resp.json();

    // Admin creates deployment with batch tariffs
    let deployment_resp = server
        .post("/admin/api/v1/models")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": "test-model",
            "alias": "test-model",
            "description": "Test model for SLA waterfall",
            "hosted_on": endpoint["id"],
            "tariffs": [
                {
                    "name": "1h batch",
                    "input_price_per_token": INPUT_1H,
                    "output_price_per_token": OUTPUT_1H,
                    "api_key_purpose": "batch",
                    "completion_window": "1h"
                },
                {
                    "name": "24h batch",
                    "input_price_per_token": INPUT_24H,
                    "output_price_per_token": OUTPUT_24H,
                    "api_key_purpose": "batch",
                    "completion_window": "24h"
                }
            ]
        }))
        .await;
    assert_eq!(
        deployment_resp.status_code(),
        200,
        "Failed to create deployment: {}",
        deployment_resp.text()
    );
    let deployment: serde_json::Value = deployment_resp.json();

    // Admin adds deployment to group
    let add_deploy_resp = server
        .post(&format!(
            "/admin/api/v1/groups/{}/models/{}",
            group_id,
            deployment["id"].as_str().unwrap()
        ))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;
    assert_eq!(add_deploy_resp.status_code(), 204, "Failed to add deployment to group");

    // Create batch API key (batch purpose can't be created through the API)
    let mut conn = pool.acquire().await.expect("acquire");
    let mut api_keys_repo = ApiKeys::new(&mut conn);
    let api_key = api_keys_repo
        .get_or_create_hidden_key(user.id, ApiKeyPurpose::Batch, user.id)
        .await
        .expect("Failed to get batch API key");
    drop(api_keys_repo);
    drop(conn);

    // Shift tariff valid_from back so they're valid at any shifted batch_created_at
    let deployment_id = Uuid::from_str(deployment["id"].as_str().unwrap()).unwrap();
    sqlx::query("UPDATE model_tariffs SET valid_from = valid_from - INTERVAL '30 days' WHERE deployed_model_id = $1")
        .bind(deployment_id)
        .execute(&pool)
        .await
        .expect("Failed to shift tariff valid_from");

    // Trigger onwards sync (picks up model + batch API key)
    bg_services.sync_onwards_config(&pool).await.expect("Failed to sync onwards config");

    let mut onwards_ready = false;
    for _ in 0..50 {
        let resp = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", api_key))
            .json(&serde_json::json!({
                "model": "test-model",
                "messages": [{"role": "user", "content": "sync check"}]
            }))
            .await;
        let status = resp.status_code().as_u16();
        if status == 200 {
            onwards_ready = true;
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
    assert!(onwards_ready, "Onwards sync did not pick up model config");

    // Fake batch ID — we don't need a real fusillade batch, just consistent headers
    let batch_id = Uuid::new_v4();
    let now = Utc::now();

    // Phase 1: 2 requests with batch_created_at = now → within 1h window → 1h pricing
    let created_at_1h = now.to_rfc3339();
    for i in 0..2 {
        let resp = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", api_key))
            .add_header("x-fusillade-batch-id", batch_id.to_string())
            .add_header("x-fusillade-request-id", Uuid::new_v4().to_string())
            .add_header("x-fusillade-batch-completion-window", "1h")
            .add_header("x-fusillade-batch-created-at", &created_at_1h)
            .add_header("x-fusillade-custom-id", format!("1h-req-{}", i))
            .json(&serde_json::json!({
                "model": "test-model",
                "messages": [{"role": "user", "content": format!("1h request {}", i)}]
            }))
            .await;
        assert_eq!(resp.status_code().as_u16(), 200, "1h request {} failed", i);
    }

    // Phase 2: 2 requests with batch_created_at = 2h ago → past 1h, within 24h → 24h pricing
    let created_at_24h = (now - Duration::hours(2)).to_rfc3339();
    for i in 0..2 {
        let resp = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", api_key))
            .add_header("x-fusillade-batch-id", batch_id.to_string())
            .add_header("x-fusillade-request-id", Uuid::new_v4().to_string())
            .add_header("x-fusillade-batch-completion-window", "1h")
            .add_header("x-fusillade-batch-created-at", &created_at_24h)
            .add_header("x-fusillade-custom-id", format!("24h-req-{}", i))
            .json(&serde_json::json!({
                "model": "test-model",
                "messages": [{"role": "user", "content": format!("24h request {}", i)}]
            }))
            .await;
        assert_eq!(resp.status_code().as_u16(), 200, "24h request {} failed", i);
    }

    // Phase 3: 2 requests with batch_created_at = 25h ago → past all windows → free
    let created_at_free = (now - Duration::hours(25)).to_rfc3339();
    for i in 0..2 {
        let resp = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", api_key))
            .add_header("x-fusillade-batch-id", batch_id.to_string())
            .add_header("x-fusillade-request-id", Uuid::new_v4().to_string())
            .add_header("x-fusillade-batch-completion-window", "1h")
            .add_header("x-fusillade-batch-created-at", &created_at_free)
            .add_header("x-fusillade-custom-id", format!("free-req-{}", i))
            .json(&serde_json::json!({
                "model": "test-model",
                "messages": [{"role": "user", "content": format!("free request {}", i)}]
            }))
            .await;
        assert_eq!(resp.status_code().as_u16(), 200, "free request {} failed", i);
    }

    // Wait for all 6 analytics rows to be flushed by the batcher
    wait_for_analytics(&pool, batch_id, 6, 30).await;

    let breakdown = get_pricing_breakdown(&pool, batch_id).await;
    tracing::info!(?breakdown, "SLA waterfall pricing breakdown");

    assert_eq!(breakdown.len(), 3, "Expected exactly 3 pricing tiers, got: {:?}", breakdown);

    // Sorted by input_price DESC: 1h (highest) → 24h → free (zero)
    let tier_1h = breakdown
        .iter()
        .find(|t| t.input_price == Decimal::from_str(INPUT_1H).unwrap())
        .expect("Missing 1h pricing tier");
    let tier_24h = breakdown
        .iter()
        .find(|t| t.input_price == Decimal::from_str(INPUT_24H).unwrap())
        .expect("Missing 24h pricing tier");
    let tier_free = breakdown
        .iter()
        .find(|t| t.input_price == Decimal::ZERO)
        .expect("Missing free pricing tier");

    // 1h tier: 2 requests at 1h pricing
    assert_eq!(tier_1h.count, 2);
    assert_eq!(tier_1h.output_price, Decimal::from_str(OUTPUT_1H).unwrap());

    // 24h tier: 2 requests at 24h pricing (waterfall from 1h)
    assert_eq!(tier_24h.count, 2);
    assert_eq!(tier_24h.output_price, Decimal::from_str(OUTPUT_24H).unwrap());

    // free tier: 2 requests at zero pricing (exceeded all windows)
    assert_eq!(tier_free.count, 2);
    assert_eq!(tier_free.output_price, Decimal::ZERO);

    drop(bg_services);
}
