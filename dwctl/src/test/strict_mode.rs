//! Tests for onwards strict mode functionality
//!
//! Strict mode enables schema validation and only accepts known OpenAI API paths.
//! This test module verifies that:
//! - Unknown endpoints are rejected with 404
//! - Allowed endpoints (chat/completions, embeddings, responses, models) work correctly
//! - Responses are properly sanitized and validated

use crate::api::models::users::Role;
use crate::test::utils::{add_auth_headers, create_test_admin_user, create_test_config, create_test_user};
use sqlx::PgPool;

/// Test that strict mode rejects unknown endpoints with 404
#[sqlx::test]
#[test_log::test]
async fn test_strict_mode_rejects_unknown_endpoints(pool: PgPool) {
    // Setup wiremock server (won't be reached for unknown endpoints)
    let _mock_server = wiremock::MockServer::start().await;

    // Create test app with strict mode enabled
    let mut config = create_test_config();
    config.onwards.strict_mode = true;
    config.background_services.onwards_sync.enabled = true;

    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("Failed to create application");
    let (server, _bg_services) = app.into_test_server();

    // Create user with API key
    let user = create_test_user(&pool, Role::StandardUser).await;

    // Create API key for the user
    let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
    let admin_headers = add_auth_headers(&admin_user);

    let key_response = server
        .post(&format!("/admin/api/v1/users/{}/api-keys", user.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "purpose": "realtime",
            "name": "Test key"
        }))
        .await;

    assert_eq!(key_response.status_code(), 201);
    let key_data: serde_json::Value = key_response.json();
    let api_key = key_data["key"].as_str().unwrap();

    // Test that unknown endpoints return 404 in strict mode
    // Note: /v1/files and /v1/batches are handled by dwctl's batch API, not onwards
    let unknown_endpoints = vec![
        "/v1/unknown/endpoint",
        "/v1/completions", // Legacy endpoint
        "/v1/engines/test/completions",
        "/v1/audio/speech",
        "/v1/images/generations",
    ];

    for endpoint in unknown_endpoints {
        let response = server
            .post(&format!("/ai{}", endpoint))
            .add_header("Authorization", &format!("Bearer {}", api_key))
            .add_header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "model": "gpt-4",
                "input": "test"
            }))
            .await;

        let status = response.status_code();
        let body = response.text();
        assert_eq!(
            status, 404,
            "Expected 404 for unknown endpoint {} in strict mode, got {} with body: {}",
            endpoint, status, body
        );
    }
}

/// Test that strict mode allows GET /v1/models endpoint
#[sqlx::test]
#[test_log::test]
async fn test_strict_mode_allows_models_endpoint(pool: PgPool) {
    // Create test app with strict mode enabled
    let mut config = create_test_config();
    config.onwards.strict_mode = true;
    config.background_services.onwards_sync.enabled = true;

    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("Failed to create application");
    let (server, bg_services) = app.into_test_server();

    // Create user with API key
    let user = create_test_user(&pool, Role::StandardUser).await;
    let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
    let admin_headers = add_auth_headers(&admin_user);

    let key_response = server
        .post(&format!("/admin/api/v1/users/{}/api-keys", user.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "purpose": "realtime",
            "name": "Test key"
        }))
        .await;

    let key_data: serde_json::Value = key_response.json();
    let api_key = key_data["key"].as_str().unwrap();

    // Sync onwards config to ensure API key is available
    bg_services.sync_onwards_config(&pool).await.unwrap();

    // Poll until onwards picks up the API key via LISTEN/NOTIFY
    // First verify initial state (key not yet available)
    let initial_response = server
        .get("/ai/v1/models")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .await;
    let initial_status = initial_response.status_code();

    // Poll until the key is synced (up to 3 seconds)
    let start = std::time::Instant::now();
    let mut synced = initial_status == 200;
    while !synced && start.elapsed() < std::time::Duration::from_secs(3) {
        let check_response = server
            .get("/ai/v1/models")
            .add_header("Authorization", &format!("Bearer {}", api_key))
            .await;
        synced = check_response.status_code() == 200;
        if !synced {
            tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
        }
    }
    assert!(synced, "API key should be synced to onwards within 3 seconds");

    // Test /ai/v1/models endpoint
    {
        let endpoint = "/ai/v1/models";
        let response = server
            .get(endpoint)
            .add_header("Authorization", &format!("Bearer {}", api_key))
            .await;

        let status = response.status_code();
        let text = response.text();
        assert_eq!(status, 200, "Expected 200 for {} endpoint in strict mode, got {}", endpoint, status);

        // Verify response format
        let body: serde_json::Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("Failed to parse JSON response for {}: {}. Response body: {}", endpoint, e, text));
        assert!(body["object"].as_str() == Some("list"), "Response should have object: list");
        assert!(body["data"].is_array(), "Response should have data array");
    }
}

/// Test that strict mode allows POST /v1/chat/completions with valid request
#[sqlx::test]
#[test_log::test]
async fn test_strict_mode_allows_chat_completions(pool: PgPool) {
    // Setup wiremock server to mock inference endpoint
    let mock_server = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-strict-test",
            "object": "chat.completion",
            "created": 1677652288,
            "model": "gpt-4",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "This is a test response in strict mode"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 8,
                "total_tokens": 18
            }
        })))
        .mount(&mock_server)
        .await;

    // Create test app with strict mode enabled
    let mut config = create_test_config();
    config.onwards.strict_mode = true;
    config.background_services.onwards_sync.enabled = true;

    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("Failed to create application");
    let (server, bg_services) = app.into_test_server();

    // Setup: Create endpoint, deployment, group, user, and API key
    let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
    let admin_headers = add_auth_headers(&admin_user);

    // Create inference endpoint (without auto-sync)
    let endpoint_response = server
        .post("/admin/api/v1/endpoints")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "name": "test-openai",
            "url": mock_server.uri(),
            "description": "Test OpenAI endpoint for strict mode",
            "auto_sync_models": false
        }))
        .await;

    assert_eq!(endpoint_response.status_code(), 201);
    let endpoint: serde_json::Value = endpoint_response.json();
    let endpoint_id = endpoint["id"].as_str().unwrap();

    // Manually create gpt-4 model
    let model_response = server
        .post("/admin/api/v1/models")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": "gpt-4",
            "alias": "gpt-4",
            "hosted_on": endpoint_id
        }))
        .await;

    assert!(
        model_response.status_code().is_success(),
        "Failed to create model: {}",
        model_response.status_code()
    );
    let model: serde_json::Value = model_response.json();
    let deployment_id = model["id"].as_str().unwrap();

    // Use the public group (all zeros UUID) so model is accessible to all users
    let group_id = "00000000-0000-0000-0000-000000000000";

    // Associate deployment with group
    let assoc_response = server
        .post(&format!("/admin/api/v1/groups/{}/models/{}", group_id, deployment_id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;
    assert!(
        assoc_response.status_code().is_success(),
        "Failed to associate model with group: {}",
        assoc_response.status_code()
    );

    // Create user (automatically has access via public group, no need to add explicitly)
    let user = create_test_user(&pool, Role::StandardUser).await;

    // Grant credits
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
            "name": "Strict mode test key"
        }))
        .await;

    let key_data: serde_json::Value = key_response.json();
    let api_key = key_data["key"].as_str().unwrap();

    // Sync onwards config
    bg_services.sync_onwards_config(&pool).await.unwrap();

    // Poll until onwards picks up the API key and model deployment via LISTEN/NOTIFY
    // Check that the gpt-4 model is available
    let start = std::time::Instant::now();
    let mut model_available = false;
    while !model_available && start.elapsed() < std::time::Duration::from_secs(3) {
        let check_response = server
            .get("/ai/v1/models")
            .add_header("Authorization", &format!("Bearer {}", api_key))
            .await;

        if check_response.status_code() == 200 {
            let models: serde_json::Value = check_response.json();
            if let Some(data) = models["data"].as_array() {
                model_available = data.iter().any(|m| m["id"].as_str() == Some("gpt-4"));
            }
        }

        if !model_available {
            tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
        }
    }
    assert!(model_available, "Model gpt-4 should be available in onwards within 3 seconds");

    // Make chat completion request
    let chat_response = server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": [{
                "role": "user",
                "content": "Hello, test strict mode"
            }]
        }))
        .await;

    let status = chat_response.status_code();
    let body_text = chat_response.text();
    assert_eq!(
        status, 200,
        "Expected 200 for chat completions in strict mode. Got {}: {}",
        status, body_text
    );

    // Verify response structure
    let body: serde_json::Value = serde_json::from_str(&body_text).expect("Response body should be valid JSON");
    assert_eq!(body["id"].as_str(), Some("chatcmpl-strict-test"));
    assert_eq!(body["object"].as_str(), Some("chat.completion"));
    assert_eq!(body["model"].as_str(), Some("gpt-4"));
    assert!(body["choices"].is_array());
    assert!(body["usage"].is_object());
}

/// Test that strict mode allows POST /v1/embeddings with valid request
#[sqlx::test]
#[test_log::test]
async fn test_strict_mode_allows_embeddings(pool: PgPool) {
    // Setup wiremock server to mock inference endpoint
    let mock_server = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/embeddings"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "data": [{
                "object": "embedding",
                "embedding": [0.1, 0.2, 0.3, 0.4, 0.5],
                "index": 0
            }],
            "model": "text-embedding-3-small",
            "usage": {
                "prompt_tokens": 5,
                "total_tokens": 5
            }
        })))
        .mount(&mock_server)
        .await;

    // Create test app with strict mode enabled
    let mut config = create_test_config();
    config.onwards.strict_mode = true;
    config.background_services.onwards_sync.enabled = true;

    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("Failed to create application");
    let (server, bg_services) = app.into_test_server();

    // Setup infrastructure (similar to chat completions test)
    let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
    let admin_headers = add_auth_headers(&admin_user);

    let endpoint_response = server
        .post("/admin/api/v1/endpoints")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "name": "test-embeddings",
            "url": mock_server.uri(),
            "auto_sync_models": false
        }))
        .await;

    let endpoint: serde_json::Value = endpoint_response.json();
    let endpoint_id = endpoint["id"].as_str().unwrap();

    // Manually create embedding model
    let model_response = server
        .post("/admin/api/v1/models")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": "text-embedding-3-small",
            "alias": "text-embedding-3-small",
            "hosted_on": endpoint_id
        }))
        .await;

    assert!(
        model_response.status_code().is_success(),
        "Failed to create embedding model: {}",
        model_response.status_code()
    );
    let model: serde_json::Value = model_response.json();
    let deployment_id = model["id"].as_str().unwrap();

    // Use the public group (all zeros UUID) so model is accessible to all users
    let group_id = "00000000-0000-0000-0000-000000000000";

    server
        .post(&format!("/admin/api/v1/groups/{}/models/{}", group_id, deployment_id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;

    // Create user (automatically has access via public group)
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

    let key_response = server
        .post(&format!("/admin/api/v1/users/{}/api-keys", user.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "purpose": "realtime",
            "name": "Embeddings test key"
        }))
        .await;

    let key_data: serde_json::Value = key_response.json();
    let api_key = key_data["key"].as_str().unwrap();

    bg_services.sync_onwards_config(&pool).await.unwrap();

    // Poll until onwards picks up the API key and model deployment via LISTEN/NOTIFY
    // Check that the text-embedding-3-small model is available
    let start = std::time::Instant::now();
    let mut model_available = false;
    while !model_available && start.elapsed() < std::time::Duration::from_secs(3) {
        let check_response = server
            .get("/ai/v1/models")
            .add_header("Authorization", &format!("Bearer {}", api_key))
            .await;

        if check_response.status_code() == 200 {
            let models: serde_json::Value = check_response.json();
            if let Some(data) = models["data"].as_array() {
                model_available = data.iter().any(|m| m["id"].as_str() == Some("text-embedding-3-small"));
            }
        }

        if !model_available {
            tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
        }
    }
    assert!(
        model_available,
        "Model text-embedding-3-small should be available in onwards within 3 seconds"
    );

    // Make embeddings request
    let embeddings_response = server
        .post("/ai/v1/embeddings")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "text-embedding-3-small",
            "input": "Test embeddings in strict mode"
        }))
        .await;

    assert_eq!(embeddings_response.status_code(), 200, "Expected 200 for embeddings in strict mode");

    // Verify response structure
    let body: serde_json::Value = embeddings_response.json();
    assert_eq!(body["object"].as_str(), Some("list"));
    assert!(body["data"].is_array());
    assert_eq!(body["model"].as_str(), Some("text-embedding-3-small"));
}

/// Test that strict mode rejects malformed requests
#[sqlx::test]
#[test_log::test]
async fn test_strict_mode_rejects_malformed_requests(pool: PgPool) {
    let mut config = create_test_config();
    config.onwards.strict_mode = true;
    config.background_services.onwards_sync.enabled = true;

    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("Failed to create application");
    let (server, _bg_services) = app.into_test_server();

    let user = create_test_user(&pool, Role::StandardUser).await;
    let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
    let admin_headers = add_auth_headers(&admin_user);

    let key_response = server
        .post(&format!("/admin/api/v1/users/{}/api-keys", user.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "purpose": "realtime",
            "name": "Test key"
        }))
        .await;

    let key_data: serde_json::Value = key_response.json();
    let api_key = key_data["key"].as_str().unwrap();

    // Test malformed JSON
    // Axum's JSON extractor returns 400 Bad Request for malformed JSON
    let response = server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .add_header("Content-Type", "application/json")
        .bytes("{invalid json".as_bytes().into())
        .await;

    assert_eq!(response.status_code(), 400, "Should reject malformed JSON with 400");

    // Test missing required fields
    // Axum's JSON extractor returns 422 Unprocessable Entity for serde validation errors
    let response = server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({
            // Missing "model" field
            "messages": [{"role": "user", "content": "test"}]
        }))
        .await;

    assert_eq!(
        response.status_code(),
        422,
        "Should reject request missing required fields with 422"
    );
}

/// Test that strict mode allows POST /v1/responses with valid request
#[sqlx::test]
#[test_log::test]
async fn test_strict_mode_allows_responses(pool: PgPool) {
    // Setup wiremock server to mock inference endpoint
    let mock_server = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/responses"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "resp-strict-test",
            "object": "response",
            "created_at": 1677652288,
            "completed_at": 1677652290,
            "status": "completed",
            "incomplete_details": null,
            "model": "gpt-4",
            "previous_response_id": null,
            "instructions": null,
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "This is a test response via /v1/responses"}]
            }],
            "error": null,
            "tools": [],
            "tool_choice": "auto",
            "truncation": "disabled",
            "parallel_tool_calls": true,
            "text": {
                "format": {
                    "type": "text"
                }
            },
            "top_p": 1.0,
            "presence_penalty": 0.0,
            "frequency_penalty": 0.0,
            "top_logprobs": 0,
            "temperature": 1.0,
            "reasoning": null,
            "usage": {
                "input_tokens": 10,
                "output_tokens": 9,
                "total_tokens": 19,
                "input_tokens_details": {
                    "cached_tokens": 0
                },
                "output_tokens_details": {
                    "reasoning_tokens": 0
                }
            },
            "max_output_tokens": null,
            "max_tool_calls": null,
            "store": false,
            "background": false,
            "service_tier": "default",
            "metadata": null,
            "safety_identifier": null,
            "prompt_cache_key": null
        })))
        .mount(&mock_server)
        .await;

    // Create test app with strict mode enabled
    let mut config = create_test_config();
    config.onwards.strict_mode = true;
    config.background_services.onwards_sync.enabled = true;

    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("Failed to create application");
    let (server, bg_services) = app.into_test_server();

    // Setup infrastructure
    let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
    let admin_headers = add_auth_headers(&admin_user);

    let endpoint_response = server
        .post("/admin/api/v1/endpoints")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "name": "test-responses",
            "url": mock_server.uri(),
            "auto_sync_models": false
        }))
        .await;

    let endpoint: serde_json::Value = endpoint_response.json();
    let endpoint_id = endpoint["id"].as_str().unwrap();

    let model_response = server
        .post("/admin/api/v1/models")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": "gpt-4",
            "alias": "gpt-4",
            "hosted_on": endpoint_id,
            "open_responses_adapter": false
        }))
        .await;

    let model: serde_json::Value = model_response.json();
    let deployment_id = model["id"].as_str().unwrap();

    let group_id = "00000000-0000-0000-0000-000000000000";

    server
        .post(&format!("/admin/api/v1/groups/{}/models/{}", group_id, deployment_id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;

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
    let api_key = key_data["key"].as_str().unwrap();

    bg_services.sync_onwards_config(&pool).await.unwrap();

    // Poll until model is available
    let start = std::time::Instant::now();
    let mut model_available = false;
    while !model_available && start.elapsed() < std::time::Duration::from_secs(3) {
        let check_response = server
            .get("/ai/v1/models")
            .add_header("Authorization", &format!("Bearer {}", api_key))
            .await;

        if check_response.status_code() == 200 {
            let models: serde_json::Value = check_response.json();
            if let Some(data) = models["data"].as_array() {
                model_available = data.iter().any(|m| m["id"].as_str() == Some("gpt-4"));
            }
        }

        if !model_available {
            tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
        }
    }
    assert!(model_available, "Model gpt-4 should be available");

    // Make /v1/responses request (using Responses API schema with "input" field)
    let response = server
        .post("/ai/v1/responses")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "gpt-4",
            "input": "Test /v1/responses endpoint"
        }))
        .await;

    assert_eq!(response.status_code(), 200, "Expected 200 for /v1/responses in strict mode");

    let body: serde_json::Value = response.json();
    assert_eq!(body["id"].as_str(), Some("resp-strict-test"));
    assert_eq!(body["object"].as_str(), Some("response"));
    assert_eq!(body["status"].as_str(), Some("completed"));
    assert!(body["output"].is_array());
    assert!(body["usage"].is_object());
}

/// Test that errors from non-trusted providers are sanitized in strict mode
#[sqlx::test]
#[test_log::test]
async fn test_strict_mode_sanitizes_provider_errors(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;

    // Mock provider returns error with sensitive internal details
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(500).set_body_json(serde_json::json!({
            "error": {
                "message": "Internal server error: database connection failed at 10.0.0.5:5432",
                "type": "internal_error",
                "code": "database_error",
                "details": {
                    "host": "internal-db.example.com",
                    "stack_trace": "Error at line 42 in auth.py"
                }
            }
        })))
        .mount(&mock_server)
        .await;

    let mut config = create_test_config();
    config.onwards.strict_mode = true;
    config.background_services.onwards_sync.enabled = true;

    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("Failed to create application");
    let (server, bg_services) = app.into_test_server();

    let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
    let admin_headers = add_auth_headers(&admin_user);

    let endpoint_response = server
        .post("/admin/api/v1/endpoints")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "name": "untrusted-provider",
            "url": mock_server.uri(),
            "auto_sync_models": false
        }))
        .await;

    let endpoint: serde_json::Value = endpoint_response.json();
    let endpoint_id = endpoint["id"].as_str().unwrap();

    // Create model with sanitize_responses=true (not trusted)
    let model_response = server
        .post("/admin/api/v1/models")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": "gpt-4",
            "alias": "gpt-4-untrusted",
            "hosted_on": endpoint_id,
            "sanitize_responses": true,
            "trusted": false
        }))
        .await;

    let model: serde_json::Value = model_response.json();
    let deployment_id = model["id"].as_str().unwrap();

    let group_id = "00000000-0000-0000-0000-000000000000";

    server
        .post(&format!("/admin/api/v1/groups/{}/models/{}", group_id, deployment_id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;

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

    let key_response = server
        .post(&format!("/admin/api/v1/users/{}/api-keys", user.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "purpose": "realtime",
            "name": "Error sanitization test"
        }))
        .await;

    let key_data: serde_json::Value = key_response.json();
    let api_key = key_data["key"].as_str().unwrap();

    bg_services.sync_onwards_config(&pool).await.unwrap();

    // Poll until model is available
    let start = std::time::Instant::now();
    let mut model_available = false;
    while !model_available && start.elapsed() < std::time::Duration::from_secs(3) {
        let check_response = server
            .get("/ai/v1/models")
            .add_header("Authorization", &format!("Bearer {}", api_key))
            .await;

        if check_response.status_code() == 200 {
            let models: serde_json::Value = check_response.json();
            if let Some(data) = models["data"].as_array() {
                model_available = data.iter().any(|m| m["id"].as_str() == Some("gpt-4-untrusted"));
            }
        }

        if !model_available {
            tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
        }
    }
    assert!(model_available, "Untrusted model should be available");

    // Make request that will trigger error from provider
    let response = server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "gpt-4-untrusted",
            "messages": [{
                "role": "user",
                "content": "trigger error"
            }]
        }))
        .await;

    let status = response.status_code();
    let body_text = response.text();
    let body: serde_json::Value = serde_json::from_str(&body_text).expect("Response should be valid JSON");

    // Should receive error but it should be sanitized
    assert_eq!(status, 500, "Should receive 500 error from provider");

    // Verify sensitive details are NOT present in sanitized response
    let error_str = body.to_string().to_lowercase();
    assert!(!error_str.contains("database"), "Sanitized error should not contain 'database'");
    assert!(!error_str.contains("10.0.0.5"), "Sanitized error should not contain internal IP");
    assert!(
        !error_str.contains("internal-db"),
        "Sanitized error should not contain internal hostname"
    );
    assert!(!error_str.contains("stack_trace"), "Sanitized error should not contain stack trace");
    assert!(!error_str.contains("auth.py"), "Sanitized error should not contain file paths");

    // Should have generic error structure
    assert!(body["error"].is_object(), "Should have error object");
}

/// Test that trusted providers bypass sanitization in strict mode
#[sqlx::test]
#[test_log::test]
async fn test_strict_mode_trusted_flag_bypasses_sanitization(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;

    // Mock provider returns error with internal details
    let error_response = serde_json::json!({
        "error": {
            "message": "Rate limit exceeded for organization org-123",
            "type": "rate_limit_error",
            "code": "rate_limit",
            "param": "requests",
            "details": {
                "organization_id": "org-123",
                "current_usage": 1000,
                "limit": 1000
            }
        }
    });

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(429).set_body_json(error_response.clone()))
        .mount(&mock_server)
        .await;

    let mut config = create_test_config();
    config.onwards.strict_mode = true;
    config.background_services.onwards_sync.enabled = true;

    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("Failed to create application");
    let (server, bg_services) = app.into_test_server();

    let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
    let admin_headers = add_auth_headers(&admin_user);

    let endpoint_response = server
        .post("/admin/api/v1/endpoints")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "name": "trusted-provider",
            "url": mock_server.uri(),
            "auto_sync_models": false
        }))
        .await;

    let endpoint: serde_json::Value = endpoint_response.json();
    let endpoint_id = endpoint["id"].as_str().unwrap();

    // Create model with trusted=true
    let model_response = server
        .post("/admin/api/v1/models")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": "gpt-4",
            "alias": "gpt-4-trusted",
            "hosted_on": endpoint_id,
            "trusted": true
        }))
        .await;

    let model: serde_json::Value = model_response.json();
    let deployment_id = model["id"].as_str().unwrap();

    let group_id = "00000000-0000-0000-0000-000000000000";

    server
        .post(&format!("/admin/api/v1/groups/{}/models/{}", group_id, deployment_id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;

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

    let key_response = server
        .post(&format!("/admin/api/v1/users/{}/api-keys", user.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "purpose": "realtime",
            "name": "Trusted test key"
        }))
        .await;

    let key_data: serde_json::Value = key_response.json();
    let api_key = key_data["key"].as_str().unwrap();

    bg_services.sync_onwards_config(&pool).await.unwrap();

    // Poll until model is available
    let start = std::time::Instant::now();
    let mut model_available = false;
    while !model_available && start.elapsed() < std::time::Duration::from_secs(3) {
        let check_response = server
            .get("/ai/v1/models")
            .add_header("Authorization", &format!("Bearer {}", api_key))
            .await;

        if check_response.status_code() == 200 {
            let models: serde_json::Value = check_response.json();
            if let Some(data) = models["data"].as_array() {
                model_available = data.iter().any(|m| m["id"].as_str() == Some("gpt-4-trusted"));
            }
        }

        if !model_available {
            tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
        }
    }
    assert!(model_available, "Trusted model should be available");

    // Make request to trusted provider
    let response = server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "gpt-4-trusted",
            "messages": [{
                "role": "user",
                "content": "test trusted provider"
            }]
        }))
        .await;

    let body_text = response.text();
    let body: serde_json::Value = serde_json::from_str(&body_text).expect("Response should be valid JSON");

    // Trusted provider errors should pass through unsanitized
    assert_eq!(response.status_code(), 429, "Should receive 429 from trusted provider");

    // Verify all details are present (not sanitized)
    assert_eq!(
        body["error"]["message"].as_str(),
        Some("Rate limit exceeded for organization org-123"),
        "Trusted provider error message should pass through"
    );
    assert_eq!(
        body["error"]["details"]["organization_id"].as_str(),
        Some("org-123"),
        "Trusted provider should include organization details"
    );
    assert_eq!(
        body["error"]["details"]["current_usage"].as_i64(),
        Some(1000),
        "Trusted provider should include usage details"
    );
}

/// Test various upstream error scenarios are handled correctly
#[sqlx::test]
#[test_log::test]
async fn test_strict_mode_handles_various_provider_errors(pool: PgPool) {
    // Test different error responses
    let test_cases = vec![
        (
            "malformed_json",
            400,
            "not valid json {{{",
            "Should handle malformed JSON from provider",
        ),
        (
            "invalid_api_key",
            401,
            r#"{"error": {"message": "Invalid API key", "type": "invalid_request_error"}}"#,
            "Should handle authentication errors",
        ),
        (
            "model_not_found",
            404,
            r#"{"error": {"message": "Model not found", "type": "invalid_request_error"}}"#,
            "Should handle model not found",
        ),
        (
            "timeout",
            504,
            r#"{"error": {"message": "Gateway timeout", "type": "timeout"}}"#,
            "Should handle timeouts",
        ),
    ];

    for (test_name, status_code, response_body, description) in test_cases {
        // Create a fresh mock server for each iteration to avoid conflicts
        let mock_server = wiremock::MockServer::start().await;

        // Mock GET /v1/models to return empty list (prevent auto-sync conflicts)
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [],
                "object": "list"
            })))
            .mount(&mock_server)
            .await;

        // Setup mock for this test case
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(status_code).set_body_string(response_body.to_string()))
            .mount(&mock_server)
            .await;

        let mut config = create_test_config();
        config.onwards.strict_mode = true;
        config.background_services.onwards_sync.enabled = true;

        let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
            .await
            .expect("Failed to create application");
        let (server, bg_services) = app.into_test_server();

        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let admin_headers = add_auth_headers(&admin_user);

        let endpoint_response = server
            .post("/admin/api/v1/endpoints")
            .add_header(&admin_headers[0].0, &admin_headers[0].1)
            .add_header(&admin_headers[1].0, &admin_headers[1].1)
            .json(&serde_json::json!({
                "name": format!("test-{}", test_name),
                "url": mock_server.uri(),
                "sync": false
            }))
            .await;

        // Endpoint creation is a required precondition for this test case; assert success
        assert_eq!(
            endpoint_response.status_code(),
            201,
            "{}: Endpoint creation returned unexpected status {}",
            description,
            endpoint_response.status_code()
        );

        let endpoint: serde_json::Value = endpoint_response.json();
        let endpoint_id = endpoint["id"].as_str().unwrap();

        let model_response = server
            .post("/admin/api/v1/models")
            .add_header(&admin_headers[0].0, &admin_headers[0].1)
            .add_header(&admin_headers[1].0, &admin_headers[1].1)
            .json(&serde_json::json!({
                "type": "standard",
                "model_name": "gpt-4",
                "alias": format!("gpt-4-{}", test_name),
                "hosted_on": endpoint_id,
                "sanitize_responses": true
            }))
            .await;

        let model: serde_json::Value = model_response.json();
        let deployment_id = model["id"].as_str().unwrap();

        let group_id = "00000000-0000-0000-0000-000000000000";

        server
            .post(&format!("/admin/api/v1/groups/{}/models/{}", group_id, deployment_id))
            .add_header(&admin_headers[0].0, &admin_headers[0].1)
            .add_header(&admin_headers[1].0, &admin_headers[1].1)
            .await;

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

        let key_response = server
            .post(&format!("/admin/api/v1/users/{}/api-keys", user.id))
            .add_header(&admin_headers[0].0, &admin_headers[0].1)
            .add_header(&admin_headers[1].0, &admin_headers[1].1)
            .json(&serde_json::json!({
                "purpose": "realtime",
                "name": format!("Test key {}", test_name)
            }))
            .await;

        let key_data: serde_json::Value = key_response.json();
        let api_key = key_data["key"].as_str().unwrap();

        bg_services.sync_onwards_config(&pool).await.unwrap();

        // Poll until model is available
        let start = std::time::Instant::now();
        let mut model_available = false;
        while !model_available && start.elapsed() < std::time::Duration::from_secs(3) {
            let check_response = server
                .get("/ai/v1/models")
                .add_header("Authorization", &format!("Bearer {}", api_key))
                .await;

            if check_response.status_code() == 200 {
                let models: serde_json::Value = check_response.json();
                if let Some(data) = models["data"].as_array() {
                    model_available = data.iter().any(|m| m["id"].as_str() == Some(&format!("gpt-4-{}", test_name)));
                }
            }

            if !model_available {
                tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
            }
        }
        assert!(model_available, "Model should be available for test: {}", test_name);

        // Make request
        let response = server
            .post("/ai/v1/chat/completions")
            .add_header("Authorization", &format!("Bearer {}", api_key))
            .add_header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "model": format!("gpt-4-{}", test_name),
                "messages": [{
                    "role": "user",
                    "content": "test error handling"
                }]
            }))
            .await;

        // Just verify we get a response (sanitization happens in onwards)
        let response_status = response.status_code();
        assert!(
            response_status.as_u16() >= 400,
            "{}: Should receive error status code. Got {}",
            description,
            response_status
        );
    }
}
