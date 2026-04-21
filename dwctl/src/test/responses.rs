//! Integration tests for the Open Responses API lifecycle.
//!
//! Tests verify that:
//! - POST /v1/responses creates a row in fusillade's requests table
//! - POST /v1/chat/completions creates a row in fusillade's requests table
//! - GET /v1/responses/{id} retrieves the response
//! - Batch requests (with X-Fusillade-Request-Id) don't create duplicate rows

use crate::api::models::users::Role;
use crate::test::utils::{add_auth_headers, create_test_admin_user, create_test_config, create_test_user};
use sqlx::PgPool;

/// Helper to set up a test app with a wiremock endpoint, model, API key, and
/// return the server + api_key ready for making AI requests.
async fn setup_ai_test(
    pool: PgPool,
    mock_server: &wiremock::MockServer,
    strict_mode: bool,
) -> (axum_test::TestServer, String, crate::BackgroundServices) {
    let mut config = create_test_config();
    config.onwards.strict_mode = strict_mode;
    config.background_services.onwards_sync.enabled = true;

    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("Failed to create application");
    let (server, bg_services) = app.into_test_server();

    let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
    let admin_headers = add_auth_headers(&admin_user);

    // Create endpoint pointing to mock server
    let endpoint_response = server
        .post("/admin/api/v1/endpoints")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "name": "test-endpoint",
            "url": mock_server.uri(),
            "auto_sync_models": false
        }))
        .await;
    let endpoint: serde_json::Value = endpoint_response.json();
    let endpoint_id = endpoint["id"].as_str().unwrap();

    // Create model
    let model_response = server
        .post("/admin/api/v1/models")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": "gpt-4o",
            "alias": "gpt-4o",
            "hosted_on": endpoint_id,
            "open_responses_adapter": true
        }))
        .await;
    let model: serde_json::Value = model_response.json();
    let deployment_id = model["id"].as_str().unwrap();

    // Assign model to default group
    let group_id = "00000000-0000-0000-0000-000000000000";
    server
        .post(&format!(
            "/admin/api/v1/groups/{}/models/{}",
            group_id, deployment_id
        ))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;

    // Create user with credits
    let user = create_test_user(&pool, Role::StandardUser).await;
    server
        .post("/admin/api/v1/transactions")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "user_id": user.id,
            "transaction_type": "admin_grant",
            "amount": 1000,
            "source_id": admin_user.id
        }))
        .await;

    // Create API key
    let key_response = server
        .post(&format!("/admin/api/v1/users/{}/api-keys", user.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "purpose": "realtime",
            "name": "Responses test key"
        }))
        .await;
    let key_data: serde_json::Value = key_response.json();
    let api_key = key_data["key"].as_str().unwrap().to_string();

    // Sync onwards config and wait for model availability
    bg_services.sync_onwards_config(&pool).await.unwrap();

    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(3) {
        let check = server
            .get("/ai/v1/models")
            .add_header("Authorization", &format!("Bearer {}", api_key))
            .await;
        if check.status_code() == 200 {
            let models: serde_json::Value = check.json();
            if let Some(data) = models["data"].as_array() {
                if data.iter().any(|m| m["id"].as_str() == Some("gpt-4o")) {
                    break;
                }
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
    }

    (server, api_key, bg_services)
}

/// Mount a wiremock mock for chat completions
async fn mount_chat_completions_mock(mock_server: &wiremock::MockServer) {
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-test123",
                "object": "chat.completion",
                "created": 1700000000,
                "model": "gpt-4o",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Hello from the test!"
                    },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 5,
                    "total_tokens": 15
                }
            })),
        )
        .mount(mock_server)
        .await;
}

/// Test that POST /v1/chat/completions with service_tier=priority creates a fusillade row
/// and GET /v1/responses/{id} retrieves it.
#[sqlx::test]
#[test_log::test]
async fn test_chat_completion_creates_retrievable_response(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    mount_chat_completions_mock(&mock_server).await;

    let (server, api_key, _bg) = setup_ai_test(pool.clone(), &mock_server, true).await;

    // Make a chat completion request with priority tier (realtime)
    let response = server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}],
            "service_tier": "priority"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Chat completion should succeed"
    );

    // The outlet handler runs asynchronously in a background task, so poll
    // until the row transitions from 'processing' to 'completed'.
    let start = std::time::Instant::now();
    let mut id = uuid::Uuid::nil();
    let mut final_state = String::new();
    while start.elapsed() < std::time::Duration::from_secs(5) {
        let row = sqlx::query(
            "SELECT id, state, model FROM fusillade.requests WHERE batch_id IS NULL ORDER BY created_at DESC LIMIT 1"
        )
        .fetch_optional(&pool)
        .await
        .unwrap();

        if let Some(row) = row {
            id = sqlx::Row::get(&row, "id");
            final_state = sqlx::Row::get::<String, _>(&row, "state");
            let model: &str = sqlx::Row::get(&row, "model");
            assert_eq!(model, "gpt-4o");
            if final_state == "completed" {
                break;
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
    assert_eq!(final_state, "completed", "Request should reach completed state");

    // Now retrieve it via GET /v1/responses/{id}
    let response_id = format!("resp_{}", id);
    let retrieve_response = server
        .get(&format!("/ai/v1/responses/{}", response_id))
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .await;

    // Note: GET /responses/{id} is on the batches router which requires auth
    // but uses the same state. Check it returns a valid response.
    assert_eq!(
        retrieve_response.status_code(),
        200,
        "GET /v1/responses/{{id}} should return 200"
    );

    let body: serde_json::Value = retrieve_response.json();
    assert_eq!(body["id"].as_str(), Some(response_id.as_str()));
    assert_eq!(body["status"].as_str(), Some("completed"));
    assert_eq!(body["model"].as_str(), Some("gpt-4o"));
    assert_eq!(body["object"].as_str(), Some("response"));
}

/// Test that POST /v1/responses with service_tier=priority (via adapter) creates a retrievable row.
#[sqlx::test]
#[test_log::test]
async fn test_responses_api_creates_retrievable_response(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    mount_chat_completions_mock(&mock_server).await;

    let (server, api_key, _bg) = setup_ai_test(pool.clone(), &mock_server, true).await;

    // Make a responses API request with priority tier (adapter mode, realtime)
    let response = server
        .post("/ai/v1/responses")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "input": "Hello from responses API",
            "service_tier": "priority"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Responses API request should succeed"
    );

    // Poll until the outlet handler completes the row
    let start = std::time::Instant::now();
    let mut id = uuid::Uuid::nil();
    let mut final_state = String::new();
    while start.elapsed() < std::time::Duration::from_secs(5) {
        let row = sqlx::query(
            "SELECT r.id, r.state FROM fusillade.requests r JOIN fusillade.request_templates t ON r.template_id = t.id WHERE r.batch_id IS NULL AND t.endpoint LIKE '%responses%' ORDER BY r.created_at DESC LIMIT 1"
        )
        .fetch_optional(&pool)
        .await
        .unwrap();

        if let Some(row) = row {
            id = sqlx::Row::get(&row, "id");
            final_state = sqlx::Row::get::<String, _>(&row, "state");
            if final_state == "completed" {
                break;
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
    assert_eq!(final_state, "completed");

    // Retrieve via GET
    let response_id = format!("resp_{}", id);
    let retrieve = server
        .get(&format!("/ai/v1/responses/{}", response_id))
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .await;

    assert_eq!(retrieve.status_code(), 200);
    let body: serde_json::Value = retrieve.json();
    assert_eq!(body["status"].as_str(), Some("completed"));
}

/// Test that GET /v1/responses/{id} returns 404 for non-existent IDs.
#[sqlx::test]
#[test_log::test]
async fn test_get_response_returns_404_for_unknown_id(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    mount_chat_completions_mock(&mock_server).await;

    let (server, api_key, _bg) = setup_ai_test(pool.clone(), &mock_server, true).await;

    let fake_id = format!("resp_{}", uuid::Uuid::new_v4());
    let response = server
        .get(&format!("/ai/v1/responses/{}", fake_id))
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .await;

    assert_eq!(response.status_code(), 404);
}

/// Test that requests with X-Fusillade-Request-Id header don't create
/// duplicate rows (batch deduplication).
#[sqlx::test]
#[test_log::test]
async fn test_fusillade_header_skips_row_creation(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    mount_chat_completions_mock(&mock_server).await;

    let (server, api_key, _bg) = setup_ai_test(pool.clone(), &mock_server, true).await;

    // Count existing rows
    let before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM fusillade.requests WHERE batch_id IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Make a request WITH the fusillade header (simulating a batch daemon request)
    let _response = server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .add_header("Content-Type", "application/json")
        .add_header(
            "X-Fusillade-Request-Id",
            &uuid::Uuid::new_v4().to_string(),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello from batch"}]
        }))
        .await;

    // Count rows after — should be the same (no new row created)
    let after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM fusillade.requests WHERE batch_id IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(
        before.0, after.0,
        "Requests with X-Fusillade-Request-Id should not create new fusillade rows"
    );
}

/// Test that a flex service_tier request with background=true returns 202
/// and creates a batch of 1 in fusillade.
#[sqlx::test]
#[test_log::test]
async fn test_flex_background_returns_202_with_queued_status(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    mount_chat_completions_mock(&mock_server).await;

    let (server, api_key, _bg) = setup_ai_test(pool.clone(), &mock_server, true).await;

    // Make a responses API request with flex tier + background=true
    let response = server
        .post("/ai/v1/responses")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "input": "Process this asynchronously",
            "service_tier": "flex",
            "background": true
        }))
        .await;

    assert_eq!(
        response.status_code(),
        202,
        "Async background request should return 202"
    );

    let body: serde_json::Value = response.json();
    assert_eq!(body["status"].as_str(), Some("queued"));
    assert_eq!(body["object"].as_str(), Some("response"));
    assert!(body["id"].as_str().unwrap().starts_with("resp_"));
    assert_eq!(body["background"].as_bool(), Some(true));

    // Verify the request was created in fusillade with a batch
    let resp_id = body["id"].as_str().unwrap();
    let uuid_str = resp_id.strip_prefix("resp_").unwrap();
    let request_id: uuid::Uuid = uuid_str.parse().unwrap();

    let row = sqlx::query("SELECT state, batch_id FROM fusillade.requests WHERE id = $1")
        .bind(request_id)
        .fetch_optional(&pool)
        .await
        .unwrap();

    assert!(row.is_some(), "Request row should exist in fusillade");
    let row = row.unwrap();
    let state: &str = sqlx::Row::get(&row, "state");
    let batch_id: Option<uuid::Uuid> = sqlx::Row::get(&row, "batch_id");

    assert_eq!(state, "pending", "Async request should be in pending state");
    assert!(batch_id.is_some(), "Async request should have a batch_id");
}

/// Test that flex + background=false holds the connection and returns the
/// completed response once the daemon finishes processing.
#[sqlx::test]
#[test_log::test]
async fn test_flex_blocking_waits_for_completion(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    mount_chat_completions_mock(&mock_server).await;

    let (server, api_key, _bg) = setup_ai_test(pool.clone(), &mock_server, true).await;

    // Spawn a task that will simulate the daemon: after a short delay, find
    // the pending request in fusillade and mark it as completed.
    let pool_clone = pool.clone();
    let daemon_task = tokio::spawn(async move {
        // Wait for the request to appear
        let start = std::time::Instant::now();
        loop {
            let row = sqlx::query(
                "SELECT id FROM fusillade.requests WHERE state = 'pending' AND batch_id IS NOT NULL ORDER BY created_at DESC LIMIT 1"
            )
            .fetch_optional(&pool_clone)
            .await
            .unwrap();

            if let Some(row) = row {
                let id: uuid::Uuid = sqlx::Row::get(&row, "id");
                // Simulate daemon completing the request
                sqlx::query(
                    "UPDATE fusillade.requests SET state = 'completed', response_status = 200,
                     response_body = '{\"output\": [{\"type\": \"message\", \"content\": \"flex result\"}], \"usage\": {\"input_tokens\": 5, \"output_tokens\": 3}}',
                     completed_at = NOW()
                     WHERE id = $1",
                )
                .bind(id)
                .execute(&pool_clone)
                .await
                .unwrap();
                return;
            }

            if start.elapsed() > std::time::Duration::from_secs(5) {
                panic!("Daemon simulator: no pending request appeared");
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
    });

    // Make a blocking flex request — this should hold the connection
    // until the simulated daemon completes it
    let response = server
        .post("/ai/v1/responses")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "input": "Process this synchronously at flex pricing",
            "service_tier": "flex"
        }))
        .await;

    // Wait for daemon simulator to finish
    daemon_task.await.unwrap();

    assert_eq!(
        response.status_code(),
        200,
        "Blocking flex should return 200 after daemon completes"
    );

    let body: serde_json::Value = response.json();
    assert_eq!(body["status"].as_str(), Some("completed"));
    assert_eq!(body["object"].as_str(), Some("response"));
}

/// Test that priority + background=true returns 202, then the spawned task
/// completes the request and it's retrievable via GET.
#[sqlx::test]
#[test_log::test]
async fn test_priority_background_completes_and_is_retrievable(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    mount_chat_completions_mock(&mock_server).await;

    let (server, api_key, _bg) = setup_ai_test(pool.clone(), &mock_server, true).await;

    let response = server
        .post("/ai/v1/responses")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "input": "Process in background but at realtime priority",
            "service_tier": "priority",
            "background": true
        }))
        .await;

    assert_eq!(
        response.status_code(),
        202,
        "Priority background should return 202"
    );

    let body: serde_json::Value = response.json();
    assert_eq!(body["status"].as_str(), Some("in_progress"));
    assert_eq!(body["background"].as_bool(), Some(true));
    let resp_id = body["id"].as_str().unwrap();
    assert!(resp_id.starts_with("resp_"));

    // Poll GET /v1/responses/{id} until it transitions to completed
    // (the spawned task proxies through onwards, outlet handler writes the body)
    let start = std::time::Instant::now();
    let mut final_status = String::new();
    while start.elapsed() < std::time::Duration::from_secs(5) {
        let retrieve = server
            .get(&format!("/ai/v1/responses/{}", resp_id))
            .add_header("Authorization", &format!("Bearer {}", api_key))
            .await;

        if retrieve.status_code() == 200 {
            let resp: serde_json::Value = retrieve.json();
            final_status = resp["status"].as_str().unwrap_or("").to_string();
            if final_status == "completed" {
                break;
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    assert_eq!(
        final_status, "completed",
        "Background priority request should eventually complete"
    );
}
