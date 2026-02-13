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

    // Small delay to allow background task to process the update
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Test both /models and /v1/models endpoints
    for endpoint in &["/ai/models", "/ai/v1/models"] {
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
        .and(wiremock::matchers::path("/v1/chat/completions"))
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

    // Small delay to allow background task to process the update
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

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
    let body: serde_json::Value =
        serde_json::from_str(&body_text).expect("Response body should be valid JSON");
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
        .and(wiremock::matchers::path("/v1/embeddings"))
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

    // Small delay to allow background task to process the update
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

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
