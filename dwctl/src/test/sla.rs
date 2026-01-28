//! End-to-end route-at-claim-time escalation test
//!
//! This test runs the full crate application with batch daemon enabled
//! and verifies that requests are routed to escalation models when
//! batches are close to expiry.

use crate::test::utils::{
    add_deployment_to_group, add_user_to_group, create_test_app_with_config, create_test_config, create_test_endpoint, create_test_model,
    create_test_user_with_roles,
};
use crate::{
    api::models::users::Role,
    config::{DaemonConfig, DaemonEnabled, ModelSource},
    db::handlers::api_keys::ApiKeys,
    db::models::api_keys::ApiKeyPurpose,
};
use chrono::{Duration, Utc};
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Helper to get a user's batch API key
async fn get_batch_api_key(pool: &PgPool, user_id: Uuid) -> String {
    let mut conn = pool.acquire().await.expect("Failed to acquire connection");
    let mut api_keys_repo = ApiKeys::new(&mut conn);
    api_keys_repo
        .get_or_create_hidden_key(user_id, ApiKeyPurpose::Batch)
        .await
        .expect("Failed to get batch API key")
}

/// Helper to get or create a realtime API key for a user (for actual inference requests)
async fn get_realtime_api_key(pool: &PgPool, user_id: Uuid) -> String {
    let mut conn = pool.acquire().await.expect("Failed to acquire connection");
    let mut api_keys_repo = ApiKeys::new(&mut conn);
    api_keys_repo
        .get_or_create_hidden_key(user_id, ApiKeyPurpose::Realtime)
        .await
        .expect("Failed to get realtime API key")
}

/// Test route-at-claim-time escalation when batch is near expiry
///
/// This test verifies that when a batch is within `escalation_threshold_seconds`
/// of expiry, requests are routed to the escalation model at claim time.
#[sqlx::test]
#[test_log::test]
async fn test_route_at_claim_time_escalation(pool: PgPool) {
    tracing::info!("🚀 Starting Route-at-Claim-Time Escalation Test");

    // Setup: Create user with BatchAPIUser role
    let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

    // Add user to everyone group so they can access models in that group
    add_user_to_group(&pool, user.id, uuid::Uuid::nil()).await;

    // Create batch API key for the user
    let user_batch_api_key = get_batch_api_key(&pool, user.id).await;
    let _user_realtime_api_key = get_realtime_api_key(&pool, user.id).await;

    tracing::info!("✅ Created batch and realtime API keys for user");

    // Setup mock server that always returns 200 OK
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "created": 1677652288,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Response"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        })))
        .mount(&mock_server)
        .await;

    tracing::info!("🌐 Mock server: {}", mock_server.uri());

    // Step 1: Create endpoint and models
    let endpoint_id = create_test_endpoint(&pool, "smart-endpoint", user.id).await;
    sqlx::query(
        r#"
        UPDATE inference_endpoints
        SET url = $1
        WHERE id = $2
        "#,
    )
    .bind(mock_server.uri())
    .bind(endpoint_id)
    .execute(&pool)
    .await
    .expect("Failed to update endpoint URL");

    // Create both models pointing to the same endpoint
    let deployment_id = create_test_model(&pool, "test-model", "test-model", endpoint_id, user.id).await;
    add_deployment_to_group(&pool, deployment_id, uuid::Uuid::nil(), user.id).await;

    let escalation_deployment_id = create_test_model(&pool, "test-model-escalation", "test-model-escalation", endpoint_id, user.id).await;
    add_deployment_to_group(&pool, escalation_deployment_id, uuid::Uuid::nil(), user.id).await;

    tracing::info!("🔧 Created models (both point to same smart server)");

    // Step 2: Create config with route-at-claim-time escalation
    let mut config = create_test_config();

    config.background_services.batch_daemon = DaemonConfig {
        enabled: DaemonEnabled::Always,
        claim_batch_size: 10,
        default_model_concurrency: 5,
        claim_interval_ms: 100,
        max_retries: None,
        stop_before_deadline_ms: None,
        backoff_ms: 100,
        backoff_factor: 2,
        max_backoff_ms: 1000,
        timeout_ms: 5000,
        claim_timeout_ms: 5000,
        processing_timeout_ms: 10000,
        status_log_interval_ms: Some(500),
        // Configure route-at-claim-time escalation
        // When batch is within 60 seconds of expiry, route to escalation model
        model_escalations: {
            let mut map = HashMap::new();
            map.insert(
                "test-model".to_string(),
                fusillade::ModelEscalationConfig {
                    escalation_model: "test-model-escalation".to_string(),
                    escalation_threshold_seconds: 60, // Escalate when < 60s remaining
                },
            );
            map
        },
        batch_metadata_fields: vec!["id".to_string(), "created_by".to_string()],
    };

    config.background_services.onwards_sync.enabled = true;
    config.background_services.probe_scheduler.enabled = false;
    config.background_services.leader_election.enabled = false;

    config.model_sources = vec![ModelSource {
        name: "test-source".to_string(),
        url: mock_server.uri().parse().expect("Failed to parse server URI"),
        api_key: None,
        sync_interval: std::time::Duration::from_secs(3600),
        default_models: None,
    }];

    tracing::info!("📋 Creating application with route-at-claim-time escalation");

    let (_server, bg_services) = create_test_app_with_config(pool.clone(), config, false).await;

    tracing::info!("✅ Application started");

    // Step 3: Create file and request templates directly in database
    let file_id = Uuid::new_v4();

    sqlx::query(
        r#"
        INSERT INTO fusillade.files (id, name, purpose, size_bytes, status, uploaded_by, created_at)
        VALUES ($1, 'test.jsonl', 'batch', 100, 'processed', $2, NOW())
        "#,
    )
    .bind(file_id)
    .bind(user.id.to_string())
    .execute(&pool)
    .await
    .expect("Failed to create file");

    // Create request templates
    for i in 1..=3 {
        let template_id = Uuid::new_v4();
        sqlx::query(
            r#"
            INSERT INTO fusillade.request_templates (id, file_id, model, api_key, endpoint, path, body, custom_id, method)
            VALUES ($1, $2, 'test-model', $3, $4, '/v1/chat/completions', $5, $6, 'POST')
            "#,
        )
        .bind(template_id)
        .bind(file_id)
        .bind(&user_batch_api_key)
        .bind(mock_server.uri())
        .bind(
            serde_json::to_string(
                &serde_json::json!({"model": "test-model", "messages": [{"role": "user", "content": format!("Test {}", i)}]}),
            )
            .unwrap(),
        )
        .bind(format!("req-{}", i))
        .execute(&pool)
        .await
        .expect("Failed to create request template");
    }

    tracing::info!("📄 File and request templates created: {}", file_id);

    // Step 4: Create batch that expires in 30 seconds (within 60s threshold)
    let batch_id = Uuid::new_v4();
    let expires_at = Utc::now() + Duration::seconds(30);

    sqlx::query(
        r#"
        INSERT INTO fusillade.batches (id, created_by, file_id, endpoint, completion_window, expires_at, created_at)
        VALUES ($1, $2, $3, '/v1/chat/completions', '24h', $4, NOW())
        "#,
    )
    .bind(batch_id)
    .bind(user.id.to_string())
    .bind(file_id)
    .bind(expires_at)
    .execute(&pool)
    .await
    .expect("Failed to create batch");

    tracing::info!("📦 Batch created (expires in 30s, threshold is 60s): {}", batch_id);

    // Step 5: Create request records from templates
    for i in 1..=3 {
        let request_id = Uuid::new_v4();
        let template_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT id FROM fusillade.request_templates
            WHERE file_id = $1 AND custom_id = $2
            "#,
        )
        .bind(file_id)
        .bind(format!("req-{}", i))
        .fetch_one(&pool)
        .await
        .expect("Failed to find template");

        sqlx::query(
            r#"
            INSERT INTO fusillade.requests (id, batch_id, template_id, model, state, created_at)
            VALUES ($1, $2, $3, 'test-model', 'pending', NOW())
            "#,
        )
        .bind(request_id)
        .bind(batch_id)
        .bind(template_id)
        .execute(&pool)
        .await
        .expect("Failed to create request");
    }

    tracing::info!("📝 Created 3 pending requests for the batch");

    // Step 6: Wait for daemon to process requests
    tracing::info!("⏳ Waiting for daemon to claim and process requests...");

    let start = tokio::time::Instant::now();
    let timeout = tokio::time::Duration::from_secs(10);

    while start.elapsed() < timeout {
        // Check if all requests are completed
        let completed_count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM fusillade.requests
            WHERE batch_id = $1 AND state = 'completed'
            "#,
        )
        .bind(batch_id)
        .fetch_one(&pool)
        .await
        .expect("Failed to query completed requests");

        if completed_count >= 3 {
            tracing::info!("✅ All requests completed");
            break;
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    // Step 7: Verify requests were routed to escalation model via routed_model column
    // Note: The request body isn't modified (it's pre-built in the template), so we
    // verify escalation by checking the routed_model column in the database, which
    // tracks what model was actually used for routing.
    let routed_models: Vec<Option<String>> = sqlx::query_scalar(
        r#"
        SELECT routed_model
        FROM fusillade.requests
        WHERE batch_id = $1
        "#,
    )
    .bind(batch_id)
    .fetch_all(&pool)
    .await
    .expect("Failed to query routed_model");

    tracing::info!("\n📊 Final Results:");
    for (i, routed_model) in routed_models.iter().enumerate() {
        tracing::info!("   Request {}: routed_model = {:?}", i + 1, routed_model);
    }

    for routed_model in &routed_models {
        assert_eq!(
            routed_model.as_deref(),
            Some("test-model-escalation"),
            "Expected routed_model to be 'test-model-escalation' (batch was within threshold)"
        );
    }

    tracing::info!("\n✅ Route-at-Claim-Time Escalation Test PASSED!");
    tracing::info!("   ✓ Batch created with expiry within threshold");
    tracing::info!("   ✓ Requests routed to escalation model at claim time");
    tracing::info!("   ✓ routed_model column correctly populated");

    drop(bg_services);
}

/// Test that requests are NOT escalated when batch has plenty of time remaining
#[sqlx::test]
#[test_log::test]
async fn test_no_escalation_when_not_near_expiry(pool: PgPool) {
    tracing::info!("🚀 Starting No-Escalation Test (batch not near expiry)");

    // Setup: Create user with BatchAPIUser role
    let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
    add_user_to_group(&pool, user.id, uuid::Uuid::nil()).await;

    let user_batch_api_key = get_batch_api_key(&pool, user.id).await;
    let _user_realtime_api_key = get_realtime_api_key(&pool, user.id).await;

    // Setup mock server
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "created": 1677652288,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "Response" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        })))
        .mount(&mock_server)
        .await;

    // Create endpoint and models
    let endpoint_id = create_test_endpoint(&pool, "smart-endpoint", user.id).await;
    sqlx::query("UPDATE inference_endpoints SET url = $1 WHERE id = $2")
        .bind(mock_server.uri())
        .bind(endpoint_id)
        .execute(&pool)
        .await
        .expect("Failed to update endpoint URL");

    let deployment_id = create_test_model(&pool, "test-model", "test-model", endpoint_id, user.id).await;
    add_deployment_to_group(&pool, deployment_id, uuid::Uuid::nil(), user.id).await;

    let escalation_deployment_id = create_test_model(&pool, "test-model-escalation", "test-model-escalation", endpoint_id, user.id).await;
    add_deployment_to_group(&pool, escalation_deployment_id, uuid::Uuid::nil(), user.id).await;

    // Create config
    let mut config = create_test_config();
    config.background_services.batch_daemon = DaemonConfig {
        enabled: DaemonEnabled::Always,
        claim_batch_size: 10,
        default_model_concurrency: 5,
        claim_interval_ms: 100,
        max_retries: None,
        stop_before_deadline_ms: None,
        backoff_ms: 100,
        backoff_factor: 2,
        max_backoff_ms: 1000,
        timeout_ms: 5000,
        claim_timeout_ms: 5000,
        processing_timeout_ms: 10000,
        status_log_interval_ms: None,
        model_escalations: {
            let mut map = HashMap::new();
            map.insert(
                "test-model".to_string(),
                fusillade::ModelEscalationConfig {
                    escalation_model: "test-model-escalation".to_string(),
                    escalation_threshold_seconds: 60, // Escalate when < 60s remaining
                },
            );
            map
        },
        batch_metadata_fields: vec!["id".to_string()],
    };

    config.background_services.onwards_sync.enabled = true;
    config.background_services.probe_scheduler.enabled = false;
    config.background_services.leader_election.enabled = false;

    config.model_sources = vec![ModelSource {
        name: "test-source".to_string(),
        url: mock_server.uri().parse().unwrap(),
        api_key: None,
        sync_interval: std::time::Duration::from_secs(3600),
        default_models: None,
    }];

    let (_server, bg_services) = create_test_app_with_config(pool.clone(), config, false).await;

    // Create file and templates
    let file_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO fusillade.files (id, name, purpose, size_bytes, status, uploaded_by, created_at) VALUES ($1, 'test.jsonl', 'batch', 100, 'processed', $2, NOW())",
    )
    .bind(file_id)
    .bind(user.id.to_string())
    .execute(&pool)
    .await
    .expect("Failed to create file");

    for i in 1..=3 {
        let template_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO fusillade.request_templates (id, file_id, model, api_key, endpoint, path, body, custom_id, method) VALUES ($1, $2, 'test-model', $3, $4, '/v1/chat/completions', $5, $6, 'POST')",
        )
        .bind(template_id)
        .bind(file_id)
        .bind(&user_batch_api_key)
        .bind(mock_server.uri())
        .bind(serde_json::to_string(&serde_json::json!({"model": "test-model", "messages": [{"role": "user", "content": format!("Test {}", i)}]})).unwrap())
        .bind(format!("req-{}", i))
        .execute(&pool)
        .await
        .expect("Failed to create template");
    }

    // Create batch that expires in 24 HOURS (well outside 60s threshold)
    let batch_id = Uuid::new_v4();
    let expires_at = Utc::now() + Duration::hours(24);

    sqlx::query(
        "INSERT INTO fusillade.batches (id, created_by, file_id, endpoint, completion_window, expires_at, created_at) VALUES ($1, $2, $3, '/v1/chat/completions', '24h', $4, NOW())",
    )
    .bind(batch_id)
    .bind(user.id.to_string())
    .bind(file_id)
    .bind(expires_at)
    .execute(&pool)
    .await
    .expect("Failed to create batch");

    tracing::info!("📦 Batch created (expires in 24h, threshold is 60s): {}", batch_id);

    // Create request records
    for i in 1..=3 {
        let request_id = Uuid::new_v4();
        let template_id: Uuid = sqlx::query_scalar("SELECT id FROM fusillade.request_templates WHERE file_id = $1 AND custom_id = $2")
            .bind(file_id)
            .bind(format!("req-{}", i))
            .fetch_one(&pool)
            .await
            .expect("Failed to find template");

        sqlx::query("INSERT INTO fusillade.requests (id, batch_id, template_id, model, state, created_at) VALUES ($1, $2, $3, 'test-model', 'pending', NOW())")
            .bind(request_id)
            .bind(batch_id)
            .bind(template_id)
            .execute(&pool)
            .await
            .expect("Failed to create request");
    }

    // Wait for processing
    let start = tokio::time::Instant::now();
    let timeout = tokio::time::Duration::from_secs(10);

    while start.elapsed() < timeout {
        let completed_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM fusillade.requests WHERE batch_id = $1 AND state = 'completed'")
                .bind(batch_id)
                .fetch_one(&pool)
                .await
                .expect("Failed to query");

        if completed_count >= 3 {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    // Verify NO escalation happened - check routed_model shows original model
    let routed_models: Vec<Option<String>> = sqlx::query_scalar("SELECT routed_model FROM fusillade.requests WHERE batch_id = $1")
        .bind(batch_id)
        .fetch_all(&pool)
        .await
        .expect("Failed to query");

    tracing::info!("\n📊 Final Results:");
    for (i, routed_model) in routed_models.iter().enumerate() {
        tracing::info!("   Request {}: routed_model = {:?}", i + 1, routed_model);
    }

    for routed_model in &routed_models {
        assert_eq!(
            routed_model.as_deref(),
            Some("test-model"),
            "Expected routed_model to be 'test-model' (batch not near expiry)"
        );
    }

    tracing::info!("\n✅ No-Escalation Test PASSED!");
    tracing::info!("   ✓ Batch had plenty of time remaining");
    tracing::info!("   ✓ Requests routed to primary model (no escalation)");

    drop(bg_services);
}
