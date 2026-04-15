pub mod databases;
pub mod sla;
pub mod strict_mode;
pub mod utils;

use crate::tasks::TaskState;
use crate::{AppState, create_initial_admin_user};
use crate::{
    api::models::{groups::GroupResponse, users::Role},
    auth::password,
    db::handlers::Users,
    openapi::{AdminApiDoc, AiApiDoc},
    request_logging::{AiRequest, AiResponse},
};
use axum_test::TestServer;
use outlet_postgres::RequestFilter;
use sqlx::PgPool;
use sqlx_pool_router::{DbPools, PoolProvider};
use tracing::info;
use utils::{add_auth_headers, create_test_admin_user, create_test_config, create_test_user};
use uuid::Uuid;
use wiremock::matchers::{body_partial_json, method, path};

struct StreamingFixture {
    server: TestServer,
    bg_services: crate::BackgroundServices,
    admin_headers: Vec<(String, String)>,
    regular_user_id: Uuid,
    api_key: String,
    group_id: Uuid,
}

async fn setup_streaming_fixture(
    pool: &PgPool,
    mock_endpoint_url: String,
    model_name: &str,
    alias: &str,
    open_responses_adapter: Option<bool>,
) -> StreamingFixture {
    let mut config = crate::test::utils::create_test_config();
    config.background_services.onwards_sync.enabled = true;
    config.enable_request_logging = true;

    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("Failed to create application");
    let (server, bg_services) = app.into_test_server();

    let admin_user = create_test_admin_user(pool, Role::PlatformManager).await;
    let admin_headers = add_auth_headers(&admin_user);

    let regular_user = create_test_user(pool, Role::StandardUser).await;
    let regular_headers = add_auth_headers(&regular_user);

    let group_response = server
        .post("/admin/api/v1/groups")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "name": format!("test-group-{}", Uuid::new_v4()),
            "description": "Test group for streaming E2E"
        }))
        .await;
    assert_eq!(group_response.status_code(), 201, "Failed to create group");
    let group: GroupResponse = group_response.json();

    let add_user_response = server
        .post(&format!("/admin/api/v1/groups/{}/users/{}", group.id, regular_user.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;
    assert_eq!(add_user_response.status_code(), 204, "Failed to add user to group");

    let credits_response = server
        .post("/admin/api/v1/transactions")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "user_id": regular_user.id,
            "transaction_type": "admin_grant",
            "amount": 1000,
            "source_id": admin_user.id,
            "description": "Test credits for streaming E2E"
        }))
        .await;
    assert_eq!(credits_response.status_code(), 201, "Failed to grant credits");

    let endpoint_response = server
        .post("/admin/api/v1/endpoints")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "name": format!("Mock Endpoint {}", alias),
            "url": mock_endpoint_url,
            "description": "Mock OpenAI-compatible endpoint for streaming E2E"
        }))
        .await;
    assert_eq!(endpoint_response.status_code(), 201, "Failed to create endpoint");
    let endpoint: crate::api::models::inference_endpoints::InferenceEndpointResponse = endpoint_response.json();

    let deployment_response = server
        .post("/admin/api/v1/models")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": model_name,
            "alias": alias,
            "description": "Test model deployment",
            "hosted_on": endpoint.id,
            "open_responses_adapter": open_responses_adapter,
            "tariffs": [{
                "name": "batch",
                "input_price_per_token": "0.001",
                "output_price_per_token": "0.003",
                "api_key_purpose": "realtime"
            }]
        }))
        .await;
    assert_eq!(deployment_response.status_code(), 200, "Failed to create deployment");
    let deployment: crate::api::models::deployments::DeployedModelResponse = deployment_response.json();

    let add_deployment_response = server
        .post(&format!("/admin/api/v1/groups/{}/models/{}", group.id, deployment.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;
    assert_eq!(add_deployment_response.status_code(), 204, "Failed to add deployment to group");

    let api_key_response = server
        .post(&format!("/admin/api/v1/users/{}/api-keys", regular_user.id))
        .add_header(&regular_headers[0].0, &regular_headers[0].1)
        .add_header(&regular_headers[1].0, &regular_headers[1].1)
        .json(&serde_json::json!({
            "name": "Test Inference Key",
            "description": "API key for streaming E2E",
            "purpose": "realtime"
        }))
        .await;
    assert_eq!(api_key_response.status_code(), 201, "Failed to create API key");
    let api_key: crate::api::models::api_keys::ApiKeyResponse = api_key_response.json();

    bg_services.sync_onwards_config(pool).await.expect("Failed to sync onwards config");
    wait_for_model(&server, &api_key.key, alias).await;

    StreamingFixture {
        server,
        bg_services,
        admin_headers,
        regular_user_id: regular_user.id,
        api_key: api_key.key,
        group_id: group.id,
    }
}

async fn wait_for_model(server: &TestServer, api_key: &str, alias: &str) {
    let poll_start = std::time::Instant::now();
    let mut status = 404;
    let mut attempts = 0;

    for i in 0..50 {
        attempts = i + 1;
        let test_response = server
            .get("/ai/v1/models")
            .add_header("authorization", format!("Bearer {}", api_key))
            .await;

        status = test_response.status_code().as_u16();
        if status == 200 {
            let models: serde_json::Value = test_response.json();
            if let Some(data) = models["data"].as_array()
                && data.iter().any(|m| m["id"].as_str() == Some(alias))
            {
                break;
            }
        }
        tokio::task::yield_now().await;
    }

    println!(
        "Polled for {:?} over {} attempts, final status: {}",
        poll_start.elapsed(),
        attempts,
        status
    );
    assert_eq!(status, 200, "Model should be available in onwards config after polling");
}

async fn assert_usage_recorded(fixture: &StreamingFixture, expected_uri: &str, prompt_tokens: i64, completion_tokens: i64) {
    let mut tries = 0;
    let usage_tx = loop {
        let transactions_response = fixture
            .server
            .get(&format!("/admin/api/v1/transactions?user_id={}", fixture.regular_user_id))
            .add_header(&fixture.admin_headers[0].0, &fixture.admin_headers[0].1)
            .add_header(&fixture.admin_headers[1].0, &fixture.admin_headers[1].1)
            .await;

        assert_eq!(transactions_response.status_code(), 200, "Should fetch transactions");
        let transactions: serde_json::Value = transactions_response.json();
        info!("Received {:?}", serde_json::to_string(&transactions));

        let usage_tx = transactions["data"]
            .as_array()
            .and_then(|x| x.iter().find(|tx| tx["transaction_type"] == "usage"));

        if let Some(tx) = usage_tx {
            let page_start_balance: f64 = transactions["page_start_balance"].as_str().unwrap().parse().unwrap();
            break (tx.clone(), page_start_balance);
        }

        tries += 1;
        if tries >= 100 {
            panic!("Usage transaction not found after {} attempts", tries);
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    };

    assert_eq!(usage_tx.0["transaction_type"], "usage", "Should be usage transaction");
    let amount: f64 = usage_tx.0["amount"].as_str().unwrap().parse().unwrap();
    let balance = usage_tx.1;
    assert!(amount > 0.0, "Usage amount should be positive (absolute value), got: {}", amount);
    assert!(
        balance < 1000.0,
        "Balance should be less than initial 1000 due to credit deduction, got: {}",
        balance
    );

    let requests_response = fixture
        .server
        .get(&format!("/admin/api/v1/requests?user_id={}&limit=1", fixture.regular_user_id))
        .add_header(&fixture.admin_headers[0].0, &fixture.admin_headers[0].1)
        .add_header(&fixture.admin_headers[1].0, &fixture.admin_headers[1].1)
        .await;

    assert_eq!(requests_response.status_code(), 200, "Should fetch logged requests");
    let requests: serde_json::Value = requests_response.json();
    let logged_entry = &requests["entries"][0];

    assert_eq!(logged_entry["method"], "POST");
    assert_eq!(logged_entry["uri"], expected_uri);
    assert_eq!(logged_entry["status_code"], 200);
    assert_eq!(logged_entry["prompt_tokens"], prompt_tokens);
    assert_eq!(logged_entry["completion_tokens"], completion_tokens);
    assert_eq!(logged_entry["total_tokens"], prompt_tokens + completion_tokens);
}

async fn cleanup_fixture(fixture: StreamingFixture) {
    let delete_group_response = fixture
        .server
        .delete(&format!("/admin/api/v1/groups/{}", fixture.group_id))
        .add_header(&fixture.admin_headers[0].0, &fixture.admin_headers[0].1)
        .add_header(&fixture.admin_headers[1].0, &fixture.admin_headers[1].1)
        .await;
    assert_eq!(delete_group_response.status_code(), 204, "Should delete test group");
    fixture.bg_services.shutdown().await;
}

#[sqlx::test]
#[test_log::test]
async fn test_e2e_ai_proxy_streaming_chat_completions_with_fusillade_header(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    let sse_response = "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1677652288,\"model\":\"gpt-3.5-turbo\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello! How can I help you today?\"}}],\"usage\":null}\n\ndata: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1677652288,\"model\":\"gpt-3.5-turbo\",\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":12,\"total_tokens\":21}}\n\ndata: [DONE]\n\n";

    wiremock::Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(serde_json::json!({
            "stream": true,
            "stream_options": { "include_usage": true }
        })))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .insert_header("cache-control", "no-cache")
                .set_body_raw(sse_response, "text/event-stream"),
        )
        .mount(&mock_server)
        .await;

    let fixture = setup_streaming_fixture(&pool, format!("{}/v1", mock_server.uri()), "gpt-3.5-turbo", "test-model", None).await;

    let inference_response = fixture
        .server
        .post("/ai/v1/chat/completions")
        .add_header("authorization", format!("Bearer {}", fixture.api_key))
        .add_header("x-fusillade-stream", "true")
        .json(&serde_json::json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "Hello from E2E test"}]
        }))
        .await;

    assert_eq!(inference_response.status_code().as_u16(), 200);
    assert_eq!(inference_response.text(), sse_response);
    assert_usage_recorded(&fixture, "http://localhost/chat/completions", 9, 12).await;
    cleanup_fixture(fixture).await;
}

#[sqlx::test]
#[test_log::test]
async fn test_e2e_ai_proxy_streaming_completions_with_fusillade_header(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    let sse_response = "data: {\"id\":\"cmpl-123\",\"object\":\"text_completion\",\"created\":1677652288,\"model\":\"gpt-3.5-turbo-instruct\",\"choices\":[{\"text\":\"Hello world\",\"index\":0}]}\n\ndata: {\"id\":\"cmpl-123\",\"object\":\"text_completion\",\"created\":1677652288,\"model\":\"gpt-3.5-turbo-instruct\",\"choices\":[],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":12,\"total_tokens\":20}}\n\ndata: [DONE]\n\n";

    wiremock::Mock::given(method("POST"))
        .and(path("/v1/completions"))
        .and(body_partial_json(serde_json::json!({
            "stream": true,
            "stream_options": { "include_usage": true }
        })))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .insert_header("cache-control", "no-cache")
                .set_body_raw(sse_response, "text/event-stream"),
        )
        .mount(&mock_server)
        .await;

    let fixture = setup_streaming_fixture(
        &pool,
        format!("{}/v1", mock_server.uri()),
        "gpt-3.5-turbo-instruct",
        "test-model",
        None,
    )
    .await;

    let inference_response = fixture
        .server
        .post("/ai/v1/completions")
        .add_header("authorization", format!("Bearer {}", fixture.api_key))
        .add_header("x-fusillade-stream", "true")
        .json(&serde_json::json!({
            "model": "test-model",
            "prompt": "Hello from E2E test"
        }))
        .await;

    assert_eq!(inference_response.status_code().as_u16(), 200);
    assert_eq!(inference_response.text(), sse_response);
    assert_usage_recorded(&fixture, "http://localhost/completions", 8, 12).await;
    cleanup_fixture(fixture).await;
}

#[sqlx::test]
#[test_log::test]
async fn test_e2e_ai_proxy_streaming_responses_with_fusillade_header(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    let response_json = serde_json::json!({
        "id": "resp-123",
        "object": "response",
        "created_at": 1677652288,
        "status": "completed",
        "model": "gpt-4o",
        "output": [],
        "usage": {
            "input_tokens": 15,
            "output_tokens": 25,
            "total_tokens": 40,
            "input_tokens_details": { "cached_tokens": 0 },
            "output_tokens_details": { "reasoning_tokens": 0 }
        }
    });
    let sse_response = format!(
        "data: {{\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"item_id\":\"item_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"Hello from responses\"}}\n\ndata: {{\"type\":\"response.completed\",\"sequence_number\":5,\"response\":{response_json}}}\n\n"
    );

    wiremock::Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_partial_json(serde_json::json!({
            "stream": true
        })))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .insert_header("cache-control", "no-cache")
                .set_body_raw(sse_response.clone(), "text/event-stream"),
        )
        .mount(&mock_server)
        .await;

    let fixture = setup_streaming_fixture(&pool, mock_server.uri(), "gpt-4o", "test-model", Some(false)).await;

    let inference_response = fixture
        .server
        .post("/ai/v1/responses")
        .add_header("authorization", format!("Bearer {}", fixture.api_key))
        .add_header("x-fusillade-stream", "true")
        .json(&serde_json::json!({
            "model": "test-model",
            "input": "Hello from E2E test"
        }))
        .await;

    if inference_response.status_code().as_u16() != 200 {
        let received = mock_server.received_requests().await.unwrap_or_default();
        panic!(
            "Responses streaming request failed with status {}. Mock received: {:?}",
            inference_response.status_code().as_u16(),
            received
        );
    }
    assert_eq!(inference_response.status_code().as_u16(), 200);
    assert_eq!(inference_response.text(), sse_response);
    assert_usage_recorded(&fixture, "http://localhost/responses", 15, 25).await;
    cleanup_fixture(fixture).await;
}

/// End-to-end test: Traffic routing rules are enforced by onwards after sync.
/// Covers three scenarios: baseline allow, deny by purpose, and redirect by purpose.
#[sqlx::test]
#[test_log::test]
async fn test_e2e_traffic_routing_rules(pool: PgPool) {
    // Setup wiremock server to mock inference endpoint
    let mock_server = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-routing-test",
            "object": "chat.completion",
            "created": 1677652288,
            "model": "gpt-3.5-turbo",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Routed successfully"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 3,
                "total_tokens": 8
            }
        })))
        .mount(&mock_server)
        .await;

    // Create app with onwards sync enabled
    let mut config = create_test_config();
    config.background_services.onwards_sync.enabled = true;

    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("Failed to create application");
    let (server, bg_services) = app.into_test_server();

    // --- Setup: admin, user, group, endpoint, model ---

    let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
    let admin_headers = add_auth_headers(&admin_user);

    let regular_user = create_test_user(&pool, Role::StandardUser).await;
    let regular_headers = add_auth_headers(&regular_user);

    // Create group
    let group: GroupResponse = server
        .post("/admin/api/v1/groups")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "name": "routing-test-group",
            "description": "Group for traffic routing test"
        }))
        .await
        .json();

    // Add user to group
    server
        .post(&format!("/admin/api/v1/groups/{}/users/{}", group.id, regular_user.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;

    // Grant credits
    server
        .post("/admin/api/v1/transactions")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "user_id": regular_user.id,
            "transaction_type": "admin_grant",
            "amount": 1000,
            "source_id": admin_user.id,
            "description": "Credits for routing test"
        }))
        .await;

    // Create endpoint
    let mock_endpoint_url = format!("{}/v1", mock_server.uri());
    let endpoint: crate::api::models::inference_endpoints::InferenceEndpointResponse = server
        .post("/admin/api/v1/endpoints")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "name": "Routing Test Endpoint",
            "url": mock_endpoint_url,
        }))
        .await
        .json();

    // Create source model (no traffic rules initially)
    let source_model: crate::api::models::deployments::DeployedModelResponse = server
        .post("/admin/api/v1/models")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": "traffic-src-model",
            "alias": "traffic-src",
            "hosted_on": endpoint.id,
            "tariffs": [{
                "name": "default",
                "input_price_per_token": "0.001",
                "output_price_per_token": "0.003",
                "api_key_purpose": "realtime"
            }]
        }))
        .await
        .json();

    // Add model to group
    server
        .post(&format!("/admin/api/v1/groups/{}/models/{}", group.id, source_model.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;

    // Create realtime API key
    let realtime_key: crate::api::models::api_keys::ApiKeyResponse = server
        .post(&format!("/admin/api/v1/users/{}/api-keys", regular_user.id))
        .add_header(&regular_headers[0].0, &regular_headers[0].1)
        .add_header(&regular_headers[1].0, &regular_headers[1].1)
        .json(&serde_json::json!({
            "name": "Realtime Key",
            "purpose": "realtime"
        }))
        .await
        .json();

    // Sync and poll until model available (baseline allow)
    bg_services.sync_onwards_config(&pool).await.expect("Failed to sync onwards config");

    let chat_body = serde_json::json!({
        "model": "traffic-src",
        "messages": [{"role": "user", "content": "test"}]
    });

    for i in 0..50 {
        let resp = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", realtime_key.key))
            .json(&chat_body)
            .await;
        if resp.status_code().as_u16() != 404 {
            assert_eq!(resp.status_code().as_u16(), 200, "Baseline request should succeed");
            break;
        }
        assert!(i < 49, "Model never became available after polling");
        tokio::task::yield_now().await;
    }

    // ===== Scenario 1: Deny batch purpose =====

    // Get batch API key (auto-created as hidden key during user setup)
    let batch_key_secret: String = sqlx::query_scalar!(
        "SELECT secret FROM api_keys WHERE user_id = $1 AND purpose = 'batch' AND hidden = true",
        regular_user.id,
    )
    .fetch_one(&pool)
    .await
    .expect("Batch hidden key should exist");

    // Add deny rule for batch purpose
    let patch_resp = server
        .patch(&format!("/admin/api/v1/models/{}", source_model.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "traffic_routing_rules": [
                { "api_key_purpose": "batch", "action": { "type": "deny" } }
            ]
        }))
        .await;
    assert_eq!(patch_resp.status_code(), 200, "Should update model with deny rule");

    // Sync onwards config
    bg_services.sync_onwards_config(&pool).await.expect("Failed to sync onwards config");

    // Small delay to let the config watcher task finish updating DashMaps
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Poll with batch key until deny rule takes effect
    let mut deny_status = 0u16;
    for i in 0..50 {
        let resp = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", batch_key_secret))
            .json(&chat_body)
            .await;
        deny_status = resp.status_code().as_u16();
        if deny_status == 403 {
            break;
        }
        assert!(i < 49, "Deny rule never took effect, last status: {deny_status}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(deny_status, 403, "Batch key should be denied");

    // Realtime key should still work (not affected by batch deny rule)
    let realtime_resp = server
        .post("/ai/v1/chat/completions")
        .add_header("authorization", format!("Bearer {}", realtime_key.key))
        .json(&chat_body)
        .await;
    assert_eq!(
        realtime_resp.status_code().as_u16(),
        200,
        "Realtime key should still work despite batch deny rule"
    );

    // ===== Scenario 2: Redirect playground purpose =====

    // Create redirect target model on same endpoint
    let target_model: crate::api::models::deployments::DeployedModelResponse = server
        .post("/admin/api/v1/models")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": "traffic-redirect-target-model",
            "alias": "traffic-redirect-target",
            "hosted_on": endpoint.id,
            "tariffs": [{
                "name": "default",
                "input_price_per_token": "0.001",
                "output_price_per_token": "0.003",
                "api_key_purpose": "realtime"
            }]
        }))
        .await
        .json();

    // Add target model to group
    server
        .post(&format!("/admin/api/v1/groups/{}/models/{}", group.id, target_model.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;

    // Get playground API key (auto-created as hidden key during user setup)
    let playground_key_secret: String = sqlx::query_scalar!(
        "SELECT secret FROM api_keys WHERE user_id = $1 AND purpose = 'playground' AND hidden = true",
        regular_user.id,
    )
    .fetch_one(&pool)
    .await
    .expect("Playground hidden key should exist");

    // Add redirect rule on source model (keep existing deny rule, add playground redirect)
    let patch_resp = server
        .patch(&format!("/admin/api/v1/models/{}", source_model.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "traffic_routing_rules": [
                { "api_key_purpose": "batch", "action": { "type": "deny" } },
                { "api_key_purpose": "playground", "action": { "type": "redirect", "target": "traffic-redirect-target" } }
            ]
        }))
        .await;
    assert_eq!(patch_resp.status_code(), 200, "Should update model with redirect rule");

    // Sync onwards config
    bg_services.sync_onwards_config(&pool).await.expect("Failed to sync onwards config");

    // Small delay to let the config watcher task finish updating DashMaps
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Poll with playground key: request targets "traffic-src" but should be transparently
    // redirected to "traffic-redirect-target" and succeed (same mock endpoint responds)
    for i in 0..50 {
        let resp = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", playground_key_secret))
            .json(&chat_body)
            .await;
        let status = resp.status_code().as_u16();
        if status == 200 {
            break; // Redirect worked, mock server responded
        }
        assert!(i < 49, "Redirect never took effect, last status: {status}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // Cleanup
    let delete_resp = server
        .delete(&format!("/admin/api/v1/groups/{}", group.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;
    assert_eq!(delete_resp.status_code(), 204, "Should delete test group");

    bg_services.shutdown().await;
}

#[sqlx::test]
#[test_log::test]
async fn test_database_seeding_behavior(pool: PgPool) {
    use crate::config::ModelSource;
    use url::Url;
    use uuid::Uuid;

    // Create test model sources
    let sources = vec![
        ModelSource {
            name: "test-endpoint-1".to_string(),
            url: Url::parse("http://localhost:8001").unwrap(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(10),
            default_models: None,
        },
        ModelSource {
            name: "test-endpoint-2".to_string(),
            url: Url::parse("http://localhost:8002").unwrap(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(10),
            default_models: None,
        },
    ];

    // Create a system API key row to test the update behavior
    let system_api_key_id = Uuid::nil();
    let original_secret = "original_test_secret";
    sqlx::query!(
        "INSERT INTO api_keys (id, name, secret, purpose, user_id, created_by) VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (id) DO UPDATE SET secret = $3",
        system_api_key_id,
        "System API Key",
        original_secret,
        "batch",
        system_api_key_id,
        system_api_key_id,
    )
    .execute(&pool)
    .await
    .expect("Should be able to create system API key");

    // Verify initial state - no seeding flag set
    let initial_seeded = sqlx::query_scalar!("SELECT value FROM system_config WHERE key = 'endpoints_seeded'")
        .fetch_optional(&pool)
        .await
        .expect("Should be able to query system_config");
    assert_eq!(initial_seeded, Some(false), "Initial seeded flag should be false");

    // First call should seed both endpoints and API key
    super::seed_database(&sources, &pool).await.expect("First seeding should succeed");

    // Verify endpoints were created
    let endpoint_count =
        sqlx::query_scalar!("SELECT COUNT(*) FROM inference_endpoints WHERE name IN ('test-endpoint-1', 'test-endpoint-2')")
            .fetch_one(&pool)
            .await
            .expect("Should be able to count endpoints");
    assert_eq!(endpoint_count, Some(2), "Should have created 2 endpoints");

    // Verify API key was updated
    let updated_secret = sqlx::query_scalar!("SELECT secret FROM api_keys WHERE id = $1", system_api_key_id)
        .fetch_one(&pool)
        .await
        .expect("Should be able to get API key secret");
    assert_ne!(updated_secret, original_secret, "API key secret should have been updated");
    assert!(updated_secret.len() > 10, "New API key should be a reasonable length");

    // Verify seeded flag is now true
    let seeded_after_first = sqlx::query_scalar!("SELECT value FROM system_config WHERE key = 'endpoints_seeded'")
        .fetch_one(&pool)
        .await
        .expect("Should be able to query seeded flag");
    assert!(seeded_after_first, "Seeded flag should be true after first run");

    // Manually modify one endpoint and the API key to test non-overwrite behavior
    sqlx::query!("UPDATE inference_endpoints SET url = 'http://modified-url:9999' WHERE name = 'test-endpoint-1'")
        .execute(&pool)
        .await
        .expect("Should be able to update endpoint");

    let manual_secret = "manually_set_secret";
    sqlx::query!("UPDATE api_keys SET secret = $1 WHERE id = $2", manual_secret, system_api_key_id)
        .execute(&pool)
        .await
        .expect("Should be able to update API key");

    // Second call should skip all seeding (because seeded flag is true)
    super::seed_database(&sources, &pool)
        .await
        .expect("Second seeding should succeed but skip");

    // Verify the manual changes were NOT overwritten
    let preserved_url = sqlx::query_scalar!("SELECT url FROM inference_endpoints WHERE name = 'test-endpoint-1'")
        .fetch_one(&pool)
        .await
        .expect("Should be able to get endpoint URL");
    assert_eq!(preserved_url, "http://modified-url:9999", "Manual URL change should be preserved");

    let preserved_secret = sqlx::query_scalar!("SELECT secret FROM api_keys WHERE id = $1", system_api_key_id)
        .fetch_one(&pool)
        .await
        .expect("Should be able to get API key secret");
    assert_eq!(preserved_secret, manual_secret, "Manual API key change should be preserved");

    // Verify endpoint count is still correct
    let final_count = sqlx::query_scalar!("SELECT COUNT(*) FROM inference_endpoints WHERE name IN ('test-endpoint-1', 'test-endpoint-2')")
        .fetch_one(&pool)
        .await
        .expect("Should be able to count endpoints");
    assert_eq!(final_count, Some(2), "Should still have 2 endpoints");
}

#[sqlx::test]
#[test_log::test]
async fn test_request_logging_enabled(pool: PgPool) {
    // Create test config with request logging enabled
    let mut config = crate::test::utils::create_test_config();
    config.enable_request_logging = true;
    config.background_services.leader_election.enabled = false;

    // Create application using proper setup (which will create outlet_db)
    let app = crate::Application::new_with_pool(config, Some(pool), None)
        .await
        .expect("Failed to create application");

    // Get outlet_db from app_state to query logs
    let outlet_pool = app.app_state.outlet_db.clone().expect("outlet_db should exist");
    let repository: outlet_postgres::RequestRepository<DbPools, AiRequest, AiResponse> =
        outlet_postgres::RequestRepository::new(outlet_pool);

    let (server, _drop_guard) = app.into_test_server();

    // Make a test request to /ai/ endpoint which should be logged
    let _ = server.get("/ai/v1/models").await;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let result = loop {
        let result = repository
            .query(RequestFilter {
                method: Some("GET".into()),
                ..Default::default()
            })
            .await
            .expect("Should be able to query requests");

        if result.len() == 1 || tokio::time::Instant::now() >= deadline {
            break result;
        }

        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    };
    assert_eq!(result.len(), 1, "Request should be logged after async write-through completes");
}

#[sqlx::test]
#[test_log::test]
async fn test_request_logging_disabled(pool: PgPool) {
    // Create test config with request logging disabled
    let mut config = crate::test::utils::create_test_config();
    config.enable_request_logging = false;
    config.enable_analytics = false; // Disable to avoid spawning background batcher task

    // Build router with request logging disabled
    let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(
        DbPools::new(pool.clone()),
        Default::default(),
    ));
    let limiters = crate::limits::Limiters::new(&config.limits);
    let shared_config = crate::SharedConfig::new(config);
    underway::run_migrations(&pool).await.expect("Failed to run underway migrations");
    let task_state = TaskState {
        request_manager: request_manager.clone(),
        dwctl_pool: pool.clone(),
        config: shared_config.clone(),
        encryption_key: None,
        ingest_file_job: std::sync::Arc::new(std::sync::OnceLock::new()),
        activate_batch_job: std::sync::Arc::new(std::sync::OnceLock::new()),
        create_batch_job: std::sync::Arc::new(std::sync::OnceLock::new()),
    };
    let task_runner = std::sync::Arc::new(
        crate::tasks::TaskRunner::new(pool.clone(), task_state)
            .await
            .expect("Failed to create task runner"),
    );
    let mut app_state = AppState::builder()
        .db(DbPools::new(pool.clone()))
        .config(shared_config)
        .request_manager(request_manager)
        .task_runner(task_runner)
        .limiters(limiters)
        .build();
    let onwards_router = axum::Router::new(); // Empty onwards router for testing
    let router = super::build_router(&mut app_state, onwards_router, None, None, false)
        .await
        .expect("Failed to build router");

    let server = axum_test::TestServer::new(router).expect("Failed to create test server");

    // Make a test request to /healthz endpoint
    let response = server.get("/healthz").await;
    assert_eq!(response.status_code().as_u16(), 200);
    assert_eq!(response.text(), "OK");

    tokio::task::yield_now().await;

    // Verify that no outlet schema or tables exist when logging is disabled
    let schema_exists =
        sqlx::query_scalar::<_, Option<i64>>("SELECT COUNT(*) FROM information_schema.schemata WHERE schema_name = 'outlet'")
            .fetch_one(&pool)
            .await
            .expect("Should be able to query information_schema");

    if schema_exists.unwrap_or(0) == 0 {
        // Schema doesn't exist, which is expected when logging is disabled
        return;
    } else {
        panic!("Outlet schema should not exist when request logging is disabled");
    }
}

#[sqlx::test]
#[test_log::test]
async fn test_dedicated_databases_for_components(pool: PgPool) {
    use crate::config::{ComponentDb, PoolSettings};
    use crate::test::databases::TestDatabases;

    // Create dedicated databases for fusillade and outlet
    let test_dbs = TestDatabases::new(&pool, "dedicated_components")
        .await
        .expect("Failed to create test databases");

    // Create config with dedicated database mode
    let mut config = crate::test::utils::create_test_config();
    config.enable_request_logging = true;
    config.batches.enabled = true;
    config.background_services.leader_election.enabled = false;

    // Configure fusillade to use dedicated database
    config.database = crate::config::DatabaseConfig::External {
        url: "ignored".to_string(), // Will be overridden by pool
        replica_url: None,
        pool: PoolSettings::default(),
        replica_pool: None,
        fusillade: ComponentDb::Dedicated {
            url: test_dbs.fusillade_url.clone(),
            replica_url: None,
            pool: PoolSettings {
                max_connections: 4,
                min_connections: 0,
                ..Default::default()
            },
            replica_pool: None,
        },
        outlet: ComponentDb::Dedicated {
            url: test_dbs.outlet_url.clone(),
            replica_url: None,
            pool: PoolSettings {
                max_connections: 4,
                min_connections: 0,
                ..Default::default()
            },
            replica_pool: None,
        },
        underway_pool: crate::config::default_underway_pool(),
    };

    // Create application - this will run migrations on the dedicated databases
    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("Failed to create application with dedicated databases");

    // Verify fusillade tables exist in the dedicated database
    let fusillade_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_dbs.fusillade_url)
        .await
        .expect("Should connect to fusillade database");
    let fusillade_tables: Vec<(String,)> = sqlx::query_as(
        "SELECT table_name FROM information_schema.tables
         WHERE table_schema = 'public' AND table_type = 'BASE TABLE'",
    )
    .fetch_all(&fusillade_pool)
    .await
    .expect("Should list fusillade tables");
    assert!(
        fusillade_tables.iter().any(|(name,)| name == "batches"),
        "Fusillade dedicated database should have batches table after migrations"
    );

    // Verify outlet_db exists and is using the dedicated database
    let outlet_pool = app.app_state.outlet_db.clone().expect("outlet_db should exist");

    // Verify we can query the outlet database
    let outlet_tables: Vec<(String,)> = sqlx::query_as(
        "SELECT table_name FROM information_schema.tables
         WHERE table_schema = 'public' AND table_type = 'BASE TABLE'",
    )
    .fetch_all(outlet_pool.read())
    .await
    .expect("Should list outlet tables");
    assert!(
        outlet_tables.iter().any(|(name,)| name == "http_requests"),
        "Outlet database should have http_requests table after migration"
    );

    // Verify the main database does NOT have the outlet schema (since we're using dedicated)
    let outlet_schema_in_main: Option<i64> =
        sqlx::query_scalar("SELECT COUNT(*) FROM information_schema.schemata WHERE schema_name = 'outlet'")
            .fetch_one(&pool)
            .await
            .expect("Should query main db");
    assert_eq!(
        outlet_schema_in_main,
        Some(0),
        "Main database should not have outlet schema when using dedicated database"
    );

    // Make a request and verify it gets logged to the dedicated outlet database
    let repository: outlet_postgres::RequestRepository<DbPools, AiRequest, AiResponse> =
        outlet_postgres::RequestRepository::new(outlet_pool);

    let (server, bg_services) = app.into_test_server();

    // Make a test request
    let _ = server.get("/ai/v1/models").await;

    // Wait for logging to complete
    tokio::task::yield_now().await;

    let result = repository
        .query(RequestFilter {
            method: Some("GET".into()),
            ..Default::default()
        })
        .await
        .expect("Should be able to query requests from dedicated outlet db");
    assert_eq!(result.len(), 1, "Request should be logged to dedicated outlet database");

    // Create a batch user and verify batch is stored in dedicated fusillade database
    use crate::api::models::users::Role;
    use crate::test::utils::{
        add_auth_headers, add_deployment_to_group, create_test_endpoint, create_test_model, create_test_user_with_roles,
    };
    use axum::http::StatusCode;
    use axum_test::multipart::MultipartForm;

    let batch_user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
    let auth_headers = add_auth_headers(&batch_user);

    // Set up a model for batch validation
    let endpoint_id = create_test_endpoint(&pool, "test-endpoint", batch_user.id).await;
    let deployment_id = create_test_model(&pool, "test-model", "test-model", endpoint_id, batch_user.id).await;
    add_deployment_to_group(&pool, deployment_id, uuid::Uuid::nil(), batch_user.id).await;

    // Upload a batch file
    let jsonl_content = r#"{"custom_id": "req-1", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "test-model", "messages": [{"role": "user", "content": "Test"}]}}"#;
    let multipart = MultipartForm::new().add_text("purpose", "batch").add_text("file", jsonl_content);

    let file_response = server
        .post("/ai/v1/files")
        .add_header(&auth_headers[0].0, &auth_headers[0].1)
        .add_header(&auth_headers[1].0, &auth_headers[1].1)
        .multipart(multipart)
        .await;
    file_response.assert_status(StatusCode::CREATED);
    let file: crate::api::models::files::FileResponse = file_response.json();

    // Create a batch
    let create_batch_json = serde_json::json!({
        "input_file_id": file.id,
        "endpoint": "/v1/chat/completions",
        "completion_window": "24h"
    });
    let batch_response = server
        .post("/ai/v1/batches")
        .add_header(&auth_headers[0].0, &auth_headers[0].1)
        .add_header(&auth_headers[1].0, &auth_headers[1].1)
        .json(&create_batch_json)
        .await;
    batch_response.assert_status(StatusCode::CREATED);
    let batch: crate::api::models::batches::BatchResponse = batch_response.json();

    // Verify batch exists in the dedicated fusillade database
    let batch_count: Option<i64> = sqlx::query_scalar("SELECT COUNT(*) FROM batches WHERE id = $1")
        .bind(uuid::Uuid::parse_str(&batch.id).unwrap())
        .fetch_one(&fusillade_pool)
        .await
        .expect("Should query fusillade database");
    assert_eq!(batch_count, Some(1), "Batch should be stored in dedicated fusillade database");

    // Cleanup
    fusillade_pool.close().await;
    bg_services.shutdown().await;
    test_dbs.cleanup().await.expect("Failed to cleanup test databases");
}

#[sqlx::test]
async fn test_create_initial_admin_user_new_user(pool: PgPool) {
    let test_email = "new-admin@example.com";

    // User should not exist initially
    let mut user_conn = pool.acquire().await.unwrap();
    let mut users_repo = Users::new(&mut user_conn);
    let initial_user = users_repo.get_user_by_email(test_email).await;
    assert!(initial_user.is_err() || initial_user.unwrap().is_none());

    // Create the initial admin user
    let user_id = create_initial_admin_user(
        test_email,
        None,
        password::Argon2Params {
            memory_kib: 128,
            iterations: 1,
            parallelism: 1,
        },
        &pool,
    )
    .await
    .expect("Should create admin user successfully");

    // Verify user was created with correct properties
    let created_user = users_repo
        .get_user_by_email(test_email)
        .await
        .expect("Should be able to query user")
        .expect("User should exist");

    assert_eq!(created_user.id, user_id);
    assert_eq!(created_user.email, test_email);
    assert_eq!(created_user.username, test_email);
    assert!(created_user.is_admin);
    assert_eq!(created_user.auth_source, "system");
    assert!(created_user.roles.contains(&Role::PlatformManager));
}

#[sqlx::test]
async fn test_create_initial_admin_user_existing_user(pool: PgPool) {
    let test_email = "existing-admin@example.com";

    // Create user first with create_test_admin_user
    let existing_user = create_test_admin_user(&pool, Role::PlatformManager).await;
    let existing_user_id = existing_user.id;

    // Update the user's email to our test email to simulate an existing admin
    sqlx::query!("UPDATE users SET email = $1 WHERE id = $2", test_email, existing_user_id)
        .execute(&pool)
        .await
        .expect("Should update user email");

    // Call create_initial_admin_user - should be idempotent
    let returned_user_id = create_initial_admin_user(
        test_email,
        None,
        password::Argon2Params {
            memory_kib: 128,
            iterations: 1,
            parallelism: 1,
        },
        &pool,
    )
    .await
    .expect("Should handle existing user successfully");

    // Should return the existing user's ID
    assert_eq!(returned_user_id, existing_user_id);

    // User should still exist and be admin
    let mut user_conn2 = pool.acquire().await.unwrap();
    let mut users_repo = Users::new(&mut user_conn2);
    let user = users_repo
        .get_user_by_email(test_email)
        .await
        .expect("Should be able to query user")
        .expect("User should still exist");

    assert_eq!(user.id, existing_user_id);
    assert!(user.is_admin);
    assert!(user.roles.contains(&Role::PlatformManager));
}

#[tokio::test]
async fn test_openapi_json_endpoints() {
    use axum::routing::get;
    use utoipa::OpenApi;
    use utoipa_scalar::{Scalar, Servable};

    // Create a test router with both OpenAPI endpoints
    let router = axum::Router::new()
        .route("/admin/openapi.json", get(|| async { axum::Json(AdminApiDoc::openapi()) }))
        .route("/ai/openapi.json", get(|| async { axum::Json(AiApiDoc::openapi()) }))
        .merge(Scalar::with_url("/admin/docs", AdminApiDoc::openapi()))
        .merge(Scalar::with_url("/ai/docs", AiApiDoc::openapi()));

    let server = axum_test::TestServer::new(router).expect("Failed to create test server");

    // Test admin API OpenAPI spec
    let admin_response = server.get("/admin/openapi.json").await;
    assert_eq!(admin_response.status_code().as_u16(), 200);
    let admin_content = admin_response.text();
    assert!(admin_content.contains("\"openapi\""));
    assert!(admin_content.contains("Admin API"));

    // Test AI API OpenAPI spec
    let ai_response = server.get("/ai/openapi.json").await;
    assert_eq!(ai_response.status_code().as_u16(), 200);
    let ai_content = ai_response.text();
    assert!(ai_content.contains("\"openapi\""));
    assert!(ai_content.contains("AI API"));
    // Should include proxied endpoints
    assert!(ai_content.contains("/chat/completions"));
    assert!(ai_content.contains("/embeddings"));
}

#[sqlx::test]
async fn test_build_router_with_metrics_disabled(pool: PgPool) {
    let mut config = create_test_config();
    config.enable_metrics = false;
    config.enable_analytics = false; // Disable to avoid spawning background batcher task

    let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(
        DbPools::new(pool.clone()),
        Default::default(),
    ));
    let limiters = crate::limits::Limiters::new(&config.limits);
    underway::run_migrations(&pool).await.expect("Failed to run underway migrations");
    let task_state = crate::tasks::TaskState {
        request_manager: request_manager.clone(),
        dwctl_pool: pool.clone(),
        config: crate::SharedConfig::new(config.clone()),
        encryption_key: None,
        ingest_file_job: std::sync::Arc::new(std::sync::OnceLock::new()),
        activate_batch_job: std::sync::Arc::new(std::sync::OnceLock::new()),
        create_batch_job: std::sync::Arc::new(std::sync::OnceLock::new()),
    };
    let task_runner = std::sync::Arc::new(
        crate::tasks::TaskRunner::new(pool.clone(), task_state)
            .await
            .expect("Failed to create task runner"),
    );
    let mut app_state = AppState::builder()
        .db(DbPools::new(pool))
        .config(crate::SharedConfig::new(config))
        .request_manager(request_manager)
        .task_runner(task_runner)
        .limiters(limiters)
        .build();

    let onwards_router = axum::Router::new();
    let router = super::build_router(&mut app_state, onwards_router, None, None, false)
        .await
        .expect("Failed to build router");
    let server = axum_test::TestServer::new(router).expect("Failed to create test server");

    // Metrics endpoint should not exist - falls through to SPA fallback
    let metrics_response = server.get("/internal/metrics").await;
    let metrics_content = metrics_response.text();
    // Should not contain Prometheus metrics format
    assert!(!metrics_content.contains("# HELP") && !metrics_content.contains("# TYPE"));
}

#[sqlx::test]
async fn test_build_router_with_metrics_enabled(pool: PgPool) {
    let mut config = create_test_config();
    config.enable_metrics = true;
    config.enable_analytics = false; // Disable to avoid spawning background batcher task

    let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(
        DbPools::new(pool.clone()),
        Default::default(),
    ));
    let limiters = crate::limits::Limiters::new(&config.limits);
    underway::run_migrations(&pool).await.expect("Failed to run underway migrations");
    let task_state = TaskState {
        request_manager: request_manager.clone(),
        dwctl_pool: pool.clone(),
        config: crate::SharedConfig::new(config.clone()),
        encryption_key: None,
        ingest_file_job: std::sync::Arc::new(std::sync::OnceLock::new()),
        activate_batch_job: std::sync::Arc::new(std::sync::OnceLock::new()),
        create_batch_job: std::sync::Arc::new(std::sync::OnceLock::new()),
    };
    let task_runner = std::sync::Arc::new(
        crate::tasks::TaskRunner::new(pool.clone(), task_state)
            .await
            .expect("Failed to create task runner"),
    );
    let mut app_state = AppState::builder()
        .db(DbPools::new(pool))
        .config(crate::SharedConfig::new(config))
        .request_manager(request_manager)
        .task_runner(task_runner)
        .limiters(limiters)
        .build();

    let onwards_router = axum::Router::new();
    let router = super::build_router(&mut app_state, onwards_router, None, None, false)
        .await
        .expect("Failed to build router");
    let server = axum_test::TestServer::new(router).expect("Failed to create test server");

    // Metrics endpoint should exist and return Prometheus format
    let metrics_response = server.get("/internal/metrics").await;
    assert_eq!(metrics_response.status_code().as_u16(), 200);

    let metrics_content = metrics_response.text();
    // The route should render some metrics output without depending on exact
    // Prometheus comment lines, which can be sensitive to recorder state/order.
    assert!(!metrics_content.trim().is_empty());
}

// ===== Composite Model Tests =====

/// Test creating a composite model
#[sqlx::test]
#[test_log::test]
async fn test_create_composite_model(pool: PgPool) {
    let (server, _bg) = utils::create_test_app(pool.clone(), false).await;
    let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
    let headers = add_auth_headers(&admin);

    // Create a composite model
    let response = server
        .post("/admin/api/v1/models")
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "type": "composite",
            "model_name": "test-composite",
            "alias": "Test Composite Model",
            "description": "A composite model for testing",
            "lb_strategy": "weighted_random",
            "fallback_enabled": true
        }))
        .await;

    assert_eq!(response.status_code(), 200, "Should create composite model");
    let model: serde_json::Value = response.json();
    assert_eq!(model["alias"], "Test Composite Model");
    assert_eq!(model["is_composite"], true);
    assert!(model["hosted_on"].is_null(), "Composite models should not have hosted_on");
}

/// Test adding components to a composite model
#[sqlx::test]
#[test_log::test]
async fn test_add_component_to_composite_model(pool: PgPool) {
    let (server, _bg) = utils::create_test_app(pool.clone(), false).await;
    let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
    let headers = add_auth_headers(&admin);

    // Create an endpoint first
    let endpoint_response = server
        .post("/admin/api/v1/endpoints")
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "name": "Test Endpoint",
            "url": "https://api.example.com/v1"
        }))
        .await;
    assert_eq!(endpoint_response.status_code(), 201);
    let endpoint: serde_json::Value = endpoint_response.json();

    // Create a standard model (component)
    let component_response = server
        .post("/admin/api/v1/models")
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": "gpt-4",
            "alias": "GPT-4 Provider",
            "hosted_on": endpoint["id"]
        }))
        .await;
    assert_eq!(component_response.status_code(), 200);
    let component: serde_json::Value = component_response.json();

    // Create a composite model
    let composite_response = server
        .post("/admin/api/v1/models")
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "type": "composite",
            "model_name": "multi-gpt",
            "alias": "Multi-Provider GPT"
        }))
        .await;
    assert_eq!(composite_response.status_code(), 200);
    let composite: serde_json::Value = composite_response.json();

    // Add component to composite model
    let add_response = server
        .post(&format!(
            "/admin/api/v1/models/{}/components/{}",
            composite["id"].as_str().unwrap(),
            component["id"].as_str().unwrap()
        ))
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "weight": 50,
            "enabled": true
        }))
        .await;
    assert_eq!(add_response.status_code(), 200, "Should add component");

    let added: serde_json::Value = add_response.json();
    assert_eq!(added["weight"], 50);
    assert_eq!(added["enabled"], true);
}

/// Test that adding a composite model as a component fails
#[sqlx::test]
#[test_log::test]
async fn test_cannot_add_composite_as_component(pool: PgPool) {
    let (server, _bg) = utils::create_test_app(pool.clone(), false).await;
    let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
    let headers = add_auth_headers(&admin);

    // Create two composite models
    let composite1_response = server
        .post("/admin/api/v1/models")
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "type": "composite",
            "model_name": "composite-1"
        }))
        .await;
    assert_eq!(composite1_response.status_code(), 200);
    let composite1: serde_json::Value = composite1_response.json();

    let composite2_response = server
        .post("/admin/api/v1/models")
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "type": "composite",
            "model_name": "composite-2"
        }))
        .await;
    assert_eq!(composite2_response.status_code(), 200);
    let composite2: serde_json::Value = composite2_response.json();

    // Try to add composite2 as a component of composite1 - should fail
    let add_response = server
        .post(&format!(
            "/admin/api/v1/models/{}/components/{}",
            composite1["id"].as_str().unwrap(),
            composite2["id"].as_str().unwrap()
        ))
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "weight": 50
        }))
        .await;
    assert_eq!(add_response.status_code(), 400, "Should not allow composite as component");
}

/// Test that adding components to a non-composite model fails
#[sqlx::test]
#[test_log::test]
async fn test_cannot_add_component_to_standard_model(pool: PgPool) {
    let (server, _bg) = utils::create_test_app(pool.clone(), false).await;
    let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
    let headers = add_auth_headers(&admin);

    // Create an endpoint
    let endpoint_response = server
        .post("/admin/api/v1/endpoints")
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "name": "Test Endpoint",
            "url": "https://api.example.com/v1"
        }))
        .await;
    assert_eq!(endpoint_response.status_code(), 201);
    let endpoint: serde_json::Value = endpoint_response.json();

    // Create two standard models
    let model1_response = server
        .post("/admin/api/v1/models")
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": "model-1",
            "hosted_on": endpoint["id"]
        }))
        .await;
    assert_eq!(model1_response.status_code(), 200);
    let model1: serde_json::Value = model1_response.json();

    let model2_response = server
        .post("/admin/api/v1/models")
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": "model-2",
            "hosted_on": endpoint["id"]
        }))
        .await;
    assert_eq!(model2_response.status_code(), 200);
    let model2: serde_json::Value = model2_response.json();

    // Try to add model2 as component of model1 (standard model) - should fail
    let add_response = server
        .post(&format!(
            "/admin/api/v1/models/{}/components/{}",
            model1["id"].as_str().unwrap(),
            model2["id"].as_str().unwrap()
        ))
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "weight": 50
        }))
        .await;
    assert_eq!(
        add_response.status_code(),
        400,
        "Should not allow adding components to standard model"
    );
}

/// Test updating a component's weight and enabled status
#[sqlx::test]
#[test_log::test]
async fn test_update_component(pool: PgPool) {
    let (server, _bg) = utils::create_test_app(pool.clone(), false).await;
    let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
    let headers = add_auth_headers(&admin);

    // Create endpoint, standard model, and composite model
    let endpoint_response = server
        .post("/admin/api/v1/endpoints")
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "name": "Test Endpoint",
            "url": "https://api.example.com/v1"
        }))
        .await;
    let endpoint: serde_json::Value = endpoint_response.json();

    let component_response = server
        .post("/admin/api/v1/models")
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": "gpt-4",
            "hosted_on": endpoint["id"]
        }))
        .await;
    let component: serde_json::Value = component_response.json();

    let composite_response = server
        .post("/admin/api/v1/models")
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "type": "composite",
            "model_name": "multi-gpt"
        }))
        .await;
    let composite: serde_json::Value = composite_response.json();

    // Add component
    let add_response = server
        .post(&format!(
            "/admin/api/v1/models/{}/components/{}",
            composite["id"].as_str().unwrap(),
            component["id"].as_str().unwrap()
        ))
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "weight": 50,
            "enabled": true
        }))
        .await;
    assert_eq!(add_response.status_code(), 200);

    // Update component weight and disable it
    let update_response = server
        .patch(&format!(
            "/admin/api/v1/models/{}/components/{}",
            composite["id"].as_str().unwrap(),
            component["id"].as_str().unwrap()
        ))
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "weight": 75,
            "enabled": false
        }))
        .await;
    assert_eq!(update_response.status_code(), 200, "Should update component");

    let updated: serde_json::Value = update_response.json();
    assert_eq!(updated["weight"], 75);
    assert_eq!(updated["enabled"], false);
}

/// Test removing a component from a composite model
#[sqlx::test]
#[test_log::test]
async fn test_remove_component(pool: PgPool) {
    let (server, _bg) = utils::create_test_app(pool.clone(), false).await;
    let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
    let headers = add_auth_headers(&admin);

    // Create endpoint, standard model, and composite model
    let endpoint_response = server
        .post("/admin/api/v1/endpoints")
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "name": "Test Endpoint",
            "url": "https://api.example.com/v1"
        }))
        .await;
    let endpoint: serde_json::Value = endpoint_response.json();

    let component_response = server
        .post("/admin/api/v1/models")
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": "gpt-4",
            "hosted_on": endpoint["id"]
        }))
        .await;
    let component: serde_json::Value = component_response.json();

    let composite_response = server
        .post("/admin/api/v1/models")
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "type": "composite",
            "model_name": "multi-gpt"
        }))
        .await;
    let composite: serde_json::Value = composite_response.json();

    // Add component
    let add_response = server
        .post(&format!(
            "/admin/api/v1/models/{}/components/{}",
            composite["id"].as_str().unwrap(),
            component["id"].as_str().unwrap()
        ))
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "weight": 50
        }))
        .await;
    assert_eq!(add_response.status_code(), 200);

    // Remove component
    let remove_response = server
        .delete(&format!(
            "/admin/api/v1/models/{}/components/{}",
            composite["id"].as_str().unwrap(),
            component["id"].as_str().unwrap()
        ))
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .await;
    assert_eq!(remove_response.status_code(), 200, "Should remove component");

    // Verify component is removed by listing components
    let list_response = server
        .get(&format!("/admin/api/v1/models/{}/components", composite["id"].as_str().unwrap()))
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .await;
    assert_eq!(list_response.status_code(), 200);
    let components: Vec<serde_json::Value> = list_response.json();
    assert!(components.is_empty(), "Component list should be empty after removal");
}

/// Test weight validation (must be 1-100)
#[sqlx::test]
#[test_log::test]
async fn test_component_weight_validation(pool: PgPool) {
    let (server, _bg) = utils::create_test_app(pool.clone(), false).await;
    let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
    let headers = add_auth_headers(&admin);

    // Create endpoint, standard model, and composite model
    let endpoint_response = server
        .post("/admin/api/v1/endpoints")
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "name": "Test Endpoint",
            "url": "https://api.example.com/v1"
        }))
        .await;
    let endpoint: serde_json::Value = endpoint_response.json();

    let component_response = server
        .post("/admin/api/v1/models")
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": "gpt-4",
            "hosted_on": endpoint["id"]
        }))
        .await;
    let component: serde_json::Value = component_response.json();

    let composite_response = server
        .post("/admin/api/v1/models")
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "type": "composite",
            "model_name": "multi-gpt"
        }))
        .await;
    let composite: serde_json::Value = composite_response.json();

    // Try to add component with weight 0 - should fail
    let add_response = server
        .post(&format!(
            "/admin/api/v1/models/{}/components/{}",
            composite["id"].as_str().unwrap(),
            component["id"].as_str().unwrap()
        ))
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "weight": 0
        }))
        .await;
    assert_eq!(add_response.status_code(), 400, "Weight 0 should be rejected");

    // Try to add component with weight 101 - should fail
    let add_response = server
        .post(&format!(
            "/admin/api/v1/models/{}/components/{}",
            composite["id"].as_str().unwrap(),
            component["id"].as_str().unwrap()
        ))
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "weight": 101
        }))
        .await;
    assert_eq!(add_response.status_code(), 400, "Weight 101 should be rejected");

    // Valid weight should succeed
    let add_response = server
        .post(&format!(
            "/admin/api/v1/models/{}/components/{}",
            composite["id"].as_str().unwrap(),
            component["id"].as_str().unwrap()
        ))
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&serde_json::json!({
            "weight": 100
        }))
        .await;
    assert_eq!(add_response.status_code(), 200, "Weight 100 should be accepted");
}

/// Test that read-only pool enforcement catches write operations.
///
/// This demonstrates sqlx_pool_router::TestDbPools' ability to catch pool routing violations.
/// The replica pool is configured with `default_transaction_read_only = on`,
/// so any write operations will fail with a PostgreSQL error.
#[sqlx::test]
async fn test_read_pool_enforces_readonly(pool: PgPool) {
    // Create test pools with read-only enforcement on replica using sqlx_pool_router
    let db_pools = sqlx_pool_router::TestDbPools::new(pool).await.unwrap();

    // Try to execute a write operation on the read pool - this should fail
    // TestDbPools creates a read-only replica by default
    let result = sqlx::query("CREATE TEMPORARY TABLE test_readonly_violation (id INT)")
        .execute(db_pools.read())
        .await;

    // The query should fail because the replica is read-only
    assert!(result.is_err(), "Write operation on read pool should fail with read-only error");

    // Verify it's the right kind of error (read-only violation)
    let error = result.unwrap_err();
    let error_msg = error.to_string();
    assert!(
        error_msg.contains("read-only") || error_msg.contains("cannot execute"),
        "Error should indicate read-only violation, got: {}",
        error_msg
    );

    // Verify the same operation succeeds on the write pool
    let result = sqlx::query("CREATE TEMPORARY TABLE test_write_ok (id INT)")
        .execute(db_pools.write())
        .await;

    assert!(result.is_ok(), "Write operation on write pool should succeed");
}
