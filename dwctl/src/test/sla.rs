//! End-to-end SLA escalation test
//!
//! This test runs the full crate application with batch daemon enabled
//! and verifies that SLA escalation actually works.

use crate::test::utils::{
    add_auth_headers, add_deployment_to_group, create_test_config, create_test_endpoint, create_test_model, create_test_user_with_roles,
};
use crate::{
    api::models::{files::FileResponse, users::Role},
    config::{DaemonConfig, DaemonEnabled, ModelSource},
};
use axum::http::StatusCode;
use axum_test::multipart::MultipartForm;
use chrono::{Duration, Utc};
use fusillade::daemon::SlaAction;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Helper struct to track which server received requests
#[derive(Debug, Clone)]
struct RequestTracker {
    primary_count: Arc<AtomicUsize>,
    escalation_count: Arc<AtomicUsize>,
}

impl RequestTracker {
    fn new() -> Self {
        Self {
            primary_count: Arc::new(AtomicUsize::new(0)),
            escalation_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn increment_primary(&self) {
        self.primary_count.fetch_add(1, Ordering::SeqCst);
    }

    fn increment_escalation(&self) {
        self.escalation_count.fetch_add(1, Ordering::SeqCst);
    }

    fn get_counts(&self) -> (usize, usize) {
        (
            self.primary_count.load(Ordering::SeqCst),
            self.escalation_count.load(Ordering::SeqCst),
        )
    }
}

/// Helper to create test JSONL file
fn create_test_jsonl() -> String {
    let requests = [
        r#"{"custom_id": "req-1", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "test-model", "messages": [{"role": "user", "content": "Test 1"}]}}"#,
        r#"{"custom_id": "req-2", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "test-model", "messages": [{"role": "user", "content": "Test 2"}]}}"#,
        r#"{"custom_id": "req-3", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "test-model", "messages": [{"role": "user", "content": "Test 3"}]}}"#,
    ];
    requests.join("\n")
}

/// Helper to update batch expiry directly in database
async fn update_batch_expiry(pool: &PgPool, batch_id: Uuid, new_expiry: chrono::DateTime<Utc>) {
    sqlx::query(
        r#"
        UPDATE fusillade.batches
        SET expires_at = $1
        WHERE id = $2
        "#,
    )
    .bind(new_expiry)
    .bind(batch_id)
    .execute(pool)
    .await
    .expect("Failed to update batch expiry");
}

#[sqlx::test]
#[test_log::test]
async fn test_sla_escalation_e2e(pool: PgPool) {
    tracing::info!("🚀 Starting SLA Escalation E2E Test");

    // Setup: Create user with BatchAPIUser role
    let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
    let auth_headers = add_auth_headers(&user);

    // Setup: Start mock servers
    let tracker = RequestTracker::new();

    // Primary server - will timeout (simulates SLA breach)
    let primary_server = MockServer::start().await;
    let primary_tracker = tracker.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_req: &wiremock::Request| {
            primary_tracker.increment_primary();
            tracing::info!("🔴 Primary server received request (will timeout)");
            // Simulate slow server that triggers timeout
            ResponseTemplate::new(408).set_delay(std::time::Duration::from_secs(300))
        })
        .mount(&primary_server)
        .await;

    // Escalation server - responds quickly (SLA fallback)
    let escalation_server = MockServer::start().await;
    let escalation_tracker = tracker.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_req: &wiremock::Request| {
            escalation_tracker.increment_escalation();
            tracing::info!("🟢 Escalation server received request (responding)");
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-escalated-123",
                "object": "chat.completion",
                "created": 1677652288,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Escalated response from fallback server"
                    },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 5,
                    "total_tokens": 15
                }
            }))
        })
        .mount(&escalation_server)
        .await;

    tracing::info!("🌐 Mock servers started:");
    tracing::info!("   Primary: {}", primary_server.uri());
    tracing::info!("   Escalation: {}", escalation_server.uri());

    // Step 1: Create endpoint for primary server
    let primary_endpoint_id = create_test_endpoint(&pool, "primary-endpoint", user.id).await;
    sqlx::query(
        r#"
        UPDATE inference_endpoints
        SET url = $1
        WHERE id = $2
        "#,
    )
    .bind(primary_server.uri())
    .bind(primary_endpoint_id)
    .execute(&pool)
    .await
    .expect("Failed to update primary endpoint URL");

    // Step 2: Create model/deployment pointing to primary endpoint
    let deployment_id = create_test_model(&pool, "test-model", "test-model", primary_endpoint_id, user.id).await;

    // Add model to everyone group so the test user can access it
    add_deployment_to_group(&pool, deployment_id, uuid::Uuid::nil(), user.id).await;

    // Step 3: Create custom config with SLA daemon enabled
    let mut config = create_test_config();

    // Enable batch daemon with aggressive SLA checking
    config.background_services.batch_daemon = DaemonConfig {
        enabled: DaemonEnabled::Always,
        claim_batch_size: 10,
        default_model_concurrency: 5,
        claim_interval_ms: 100, // Check frequently
        max_retries: None,
        stop_before_deadline_ms: None,
        backoff_ms: 100,
        backoff_factor: 2,
        max_backoff_ms: 1000,
        timeout_ms: 5000, // Short timeout to trigger failures quickly
        claim_timeout_ms: 5000,
        processing_timeout_ms: 10000,
        status_log_interval_ms: Some(500),
        // Configure priority endpoint for SLA escalation
        priority_endpoints: {
            let mut map = HashMap::new();
            map.insert(
                "test-model".to_string(),
                fusillade::PriorityEndpointConfig {
                    endpoint: escalation_server.uri(),
                    api_key: None,
                    model_override: Some("test-model".to_string()),
                    path_override: None,
                },
            );
            map
        },
        sla_check_interval_seconds: 1, // Check every second
        sla_thresholds: vec![fusillade::SlaThreshold {
            name: "test-sla".to_string(),
            threshold_seconds: 30, // Escalate if batch expires within 30 seconds
            action: SlaAction::Escalate,
            allowed_states: vec![fusillade::RequestStateFilter::Pending, fusillade::RequestStateFilter::Processing],
        }],
        batch_metadata_fields: vec!["id".to_string(), "created_by".to_string()],
    };

    // Disable other background services for cleaner test
    config.background_services.onwards_sync.enabled = false;
    config.background_services.probe_scheduler.enabled = false;
    config.background_services.leader_election.enabled = false;

    // Add model source (without default models since we create the model manually)
    config.model_sources = vec![ModelSource {
        name: "test-source".to_string(),
        url: primary_server.uri().parse().expect("Failed to parse server URI"),
        api_key: None,
        sync_interval: std::time::Duration::from_secs(3600), // Don't sync during test
        default_models: None,                                // We create the model manually via create_test_model
    }];

    tracing::info!("📋 Creating application with SLA daemon enabled");

    // Create app with custom config
    let app = crate::Application::new_with_pool(config, Some(pool.clone()))
        .await
        .expect("Failed to create application");

    let (test_server, _bg_services) = app.into_test_server();

    tracing::info!("✅ Application started with SLA daemon running");

    // Step 4: Upload test JSONL file
    let jsonl_content = create_test_jsonl();
    let multipart = MultipartForm::new().add_text("purpose", "batch").add_text("file", jsonl_content);

    let upload_response = test_server
        .post("/ai/v1/files")
        .add_header(&auth_headers[0].0, &auth_headers[0].1)
        .add_header(&auth_headers[1].0, &auth_headers[1].1)
        .multipart(multipart)
        .await;

    upload_response.assert_status(StatusCode::CREATED);
    let file_response: FileResponse = upload_response.json();
    let file_id = file_response.id;

    tracing::info!("📄 File uploaded: {}", file_id);

    // Step 5: Create batch
    let create_batch_json = serde_json::json!({
        "input_file_id": file_id,
        "endpoint": "/v1/chat/completions",
        "completion_window": "24h"
    });

    let batch_response = test_server
        .post("/ai/v1/batches")
        .add_header(&auth_headers[0].0, &auth_headers[0].1)
        .add_header(&auth_headers[1].0, &auth_headers[1].1)
        .json(&create_batch_json)
        .await;

    batch_response.assert_status(StatusCode::CREATED);
    let batch: crate::api::models::batches::BatchResponse = batch_response.json();
    let batch_id = Uuid::parse_str(&batch.id).expect("Invalid batch ID");

    tracing::info!("📦 Batch created: {}", batch_id);

    // Step 6: Update batch expiry to trigger SLA (set to expire in 25 seconds)
    // This is within our 30-second threshold, so it should trigger escalation
    let new_expiry = Utc::now() + Duration::seconds(25);
    update_batch_expiry(&pool, batch_id, new_expiry).await;

    tracing::info!("⏰ Batch expiry set to: {}", new_expiry);
    tracing::info!("   (expires in 25 seconds, SLA threshold is 30 seconds)");

    // Step 7: Wait for the SLA daemon to detect and escalate requests
    tracing::info!("⏳ Waiting for SLA daemon to detect and escalate...");

    // Poll for escalation to happen (max 10 seconds)
    let mut escalation_detected = false;
    let start = tokio::time::Instant::now();
    let timeout = tokio::time::Duration::from_secs(10);

    while start.elapsed() < timeout {
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let (primary_count, escalation_count) = tracker.get_counts();

        if escalation_count >= 3 {
            escalation_detected = true;
            tracing::info!("🎯 Escalation detected! {} requests sent to fallback server", escalation_count);
            break;
        }

        // Log progress every second
        if start.elapsed().as_secs().is_multiple_of(1) && start.elapsed().as_millis() % 1000 < 100 {
            tracing::info!(
                "   Check ({}s): primary={}, escalation={}",
                start.elapsed().as_secs(),
                primary_count,
                escalation_count
            );
        }
    }

    // Step 8: Poll until all escalated requests complete (max 5 seconds)
    if escalation_detected {
        tracing::info!("⏳ Waiting for escalated requests to complete...");
        let start = tokio::time::Instant::now();
        let timeout = tokio::time::Duration::from_secs(5);

        while start.elapsed() < timeout {
            // Check database for completed escalated requests
            let completed_count: i64 = sqlx::query_scalar(
                r#"
                SELECT COUNT(*)
                FROM fusillade.requests
                WHERE batch_id = $1
                  AND is_escalated = true
                  AND state = 'completed'
                "#,
            )
            .bind(batch_id)
            .fetch_one(&pool)
            .await
            .expect("Failed to query completed requests");

            if completed_count >= 3 {
                tracing::info!("✅ All escalated requests completed ({})", completed_count);
                break;
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }

    // Step 9: Verify escalation happened
    let (primary_count, escalation_count) = tracker.get_counts();

    tracing::info!("\n📊 Final Results:");
    tracing::info!("   Primary server requests: {}", primary_count);
    tracing::info!("   Escalation server requests: {}", escalation_count);

    assert!(
        escalation_detected,
        "SLA escalation should have occurred! Expected requests to escalation server, got 0"
    );

    assert!(
        escalation_count >= 3,
        "Expected at least 3 escalated requests (one per batch request), got {}",
        escalation_count
    );

    tracing::info!("\n✅ SLA Escalation E2E Test PASSED!");
    tracing::info!("   ✓ Batch created with expiry within SLA threshold");
    tracing::info!("   ✓ SLA daemon detected approaching deadline");
    tracing::info!("   ✓ Requests successfully escalated to fallback server");
    tracing::info!("   ✓ {} requests handled by escalation server", escalation_count);
}

/// Test that SLA escalation works even for batches that have already failed
///
/// This is a regression test for the bug where batches with `failed_at` set were
/// excluded from SLA escalation. The scenario:
/// 1. All requests fail immediately (HTTP 502)
/// 2. Batch gets marked as failed within seconds
/// 3. SLA daemon should still escalate the failed requests before expiry
///
/// Without the fix (with `b.failed_at IS NULL` in the query), this test fails.
/// With the fix (removed failed_at check), escalation works as expected.
#[sqlx::test]
#[test_log::test]
async fn test_sla_escalation_for_failed_batch(pool: PgPool) {
    tracing::info!("🚀 Starting SLA Escalation for Failed Batch Test");

    // Setup: Create user with BatchAPIUser role
    let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
    let auth_headers = add_auth_headers(&user);

    // Setup: Start mock servers
    let tracker = RequestTracker::new();

    // Track captured authorization headers for API key verification
    let captured_auth_header = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

    // Primary server - returns immediate 502 errors (simulates failing backend)
    let primary_server = MockServer::start().await;
    let primary_tracker = tracker.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_req: &wiremock::Request| {
            primary_tracker.increment_primary();
            tracing::info!("🔴 Primary server received request (returning 502)");
            // Return immediate error - no delay
            ResponseTemplate::new(502).set_body_json(serde_json::json!({
                "error": {
                    "message": "Bad Gateway - upstream service unavailable",
                    "type": "server_error"
                }
            }))
        })
        .mount(&primary_server)
        .await;

    // Escalation server - responds quickly with success and captures Authorization header
    let escalation_server = MockServer::start().await;
    let escalation_tracker = tracker.clone();
    let auth_header_clone = captured_auth_header.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &wiremock::Request| {
            escalation_tracker.increment_escalation();

            // Capture Authorization header
            if let Some(auth_header) = req.headers.get("Authorization") {
                let header_value = auth_header.to_str().unwrap_or("").to_string();
                tracing::info!("🔑 Captured Authorization header: {}", header_value);
                let mut headers = auth_header_clone.lock().unwrap();
                headers.push(header_value);
            } else {
                tracing::warn!("⚠️  No Authorization header found in escalated request!");
            }

            tracing::info!("🟢 Escalation server received request (responding with success)");
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-escalated-456",
                "object": "chat.completion",
                "created": 1677652288,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Recovered via escalation!"
                    },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 5,
                    "total_tokens": 15
                }
            }))
        })
        .mount(&escalation_server)
        .await;

    tracing::info!("🌐 Mock servers started:");
    tracing::info!("   Primary (failing): {}", primary_server.uri());
    tracing::info!("   Escalation (working): {}", escalation_server.uri());

    // Step 1: Create endpoint for primary server
    let primary_endpoint_id = create_test_endpoint(&pool, "failing-endpoint", user.id).await;
    sqlx::query(
        r#"
        UPDATE inference_endpoints
        SET url = $1
        WHERE id = $2
        "#,
    )
    .bind(primary_server.uri())
    .bind(primary_endpoint_id)
    .execute(&pool)
    .await
    .expect("Failed to update primary endpoint URL");

    // Step 2: Create model/deployment pointing to primary endpoint
    let deployment_id = create_test_model(&pool, "test-model-fail", "test-model-fail", primary_endpoint_id, user.id).await;

    // Add model to everyone group
    add_deployment_to_group(&pool, deployment_id, uuid::Uuid::nil(), user.id).await;

    // Step 3: Create custom config with SLA daemon enabled
    let mut config = create_test_config();

    // Allow 1h completion window for this test
    config.batches.allowed_completion_windows = vec!["1h".to_string(), "24h".to_string()];

    // Enable batch daemon with aggressive SLA checking and retry settings
    config.background_services.batch_daemon = DaemonConfig {
        enabled: DaemonEnabled::Always,
        claim_batch_size: 10,
        default_model_concurrency: 5,
        claim_interval_ms: 100,
        max_retries: Some(0), // No retries - fail immediately
        stop_before_deadline_ms: None,
        backoff_ms: 100,
        backoff_factor: 2,
        max_backoff_ms: 1000,
        timeout_ms: 2000, // Short timeout
        claim_timeout_ms: 5000,
        processing_timeout_ms: 10000,
        status_log_interval_ms: None, // Disable status logging to prevent lazy failed_at computation
        // Configure priority endpoint for SLA escalation with API key
        priority_endpoints: {
            let mut map = HashMap::new();
            map.insert(
                "test-model-fail".to_string(),
                fusillade::PriorityEndpointConfig {
                    endpoint: escalation_server.uri(),
                    api_key: Some("test-sla-api-key-secret-123".to_string()), // API key for priority endpoint
                    model_override: Some("test-model-fail".to_string()),
                    path_override: None,
                },
            );
            map
        },
        sla_check_interval_seconds: 5, // Check every 5 seconds - gives batch time to fail first
        sla_thresholds: vec![fusillade::SlaThreshold {
            name: "test-sla-failed".to_string(),
            threshold_seconds: 60, // Escalate if batch expires within 60 seconds
            action: SlaAction::Escalate,
            // IMPORTANT: Include Failed state to allow escalation of failed requests
            allowed_states: vec![
                fusillade::RequestStateFilter::Pending,
                fusillade::RequestStateFilter::Processing,
                fusillade::RequestStateFilter::Failed, // ← Key: allow escalation of failed requests
            ],
        }],
        batch_metadata_fields: vec!["id".to_string(), "created_by".to_string()],
    };

    // Disable other background services for cleaner test
    config.background_services.onwards_sync.enabled = false;
    config.background_services.probe_scheduler.enabled = false;
    config.background_services.leader_election.enabled = false;

    // Add model source
    config.model_sources = vec![ModelSource {
        name: "test-source-fail".to_string(),
        url: primary_server.uri().parse().expect("Failed to parse server URI"),
        api_key: None,
        sync_interval: std::time::Duration::from_secs(3600),
        default_models: None,
    }];

    tracing::info!("📋 Creating application with SLA daemon enabled");

    // Create app with custom config
    let app = crate::Application::new_with_pool(config, Some(pool.clone()))
        .await
        .expect("Failed to create application");

    let (test_server, _bg_services) = app.into_test_server();

    tracing::info!("✅ Application started with SLA daemon running");

    // Step 4: Upload test JSONL file (with model name matching our test model)
    let jsonl_content = r#"{"custom_id": "req-1", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "test-model-fail", "messages": [{"role": "user", "content": "Test 1"}]}}
{"custom_id": "req-2", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "test-model-fail", "messages": [{"role": "user", "content": "Test 2"}]}}
{"custom_id": "req-3", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "test-model-fail", "messages": [{"role": "user", "content": "Test 3"}]}}"#;
    let multipart = MultipartForm::new().add_text("purpose", "batch").add_text("file", jsonl_content);

    let upload_response = test_server
        .post("/ai/v1/files")
        .add_header(&auth_headers[0].0, &auth_headers[0].1)
        .add_header(&auth_headers[1].0, &auth_headers[1].1)
        .multipart(multipart)
        .await;

    upload_response.assert_status(StatusCode::CREATED);
    let file_response: FileResponse = upload_response.json();
    let file_id = file_response.id;

    tracing::info!("📄 File uploaded: {}", file_id);

    // Step 5: Create batch with 1 hour completion window
    let create_batch_json = serde_json::json!({
        "input_file_id": file_id,
        "endpoint": "/v1/chat/completions",
        "completion_window": "1h"
    });

    let batch_response = test_server
        .post("/ai/v1/batches")
        .add_header(&auth_headers[0].0, &auth_headers[0].1)
        .add_header(&auth_headers[1].0, &auth_headers[1].1)
        .json(&create_batch_json)
        .await;

    batch_response.assert_status(StatusCode::CREATED);
    let batch: crate::api::models::batches::BatchResponse = batch_response.json();
    let batch_id = Uuid::parse_str(&batch.id).expect("Invalid batch ID");

    tracing::info!("📦 Batch created: {}", batch_id);

    // Step 6: Update batch expiry to trigger SLA (set to expire in 25 seconds)
    // This ensures the batch is within the 30-second SLA threshold
    let new_expiry = Utc::now() + Duration::seconds(25);
    update_batch_expiry(&pool, batch_id, new_expiry).await;

    tracing::info!("⏰ Batch expiry set to: {}", new_expiry);
    tracing::info!("   (expires in 25 seconds, SLA threshold is 30 seconds)");

    // Step 7: Wait for requests to fail
    tracing::info!("⏳ Waiting for requests to fail at primary server...");

    let start = tokio::time::Instant::now();
    let timeout = tokio::time::Duration::from_secs(10);
    let mut batch_failed = false;

    while start.elapsed() < timeout {
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Check if batch has failed
        let failed_at: Option<chrono::DateTime<Utc>> = sqlx::query_scalar(
            r#"
            SELECT failed_at
            FROM fusillade.batches
            WHERE id = $1
            "#,
        )
        .bind(batch_id)
        .fetch_one(&pool)
        .await
        .expect("Failed to query batch");

        if failed_at.is_some() {
            batch_failed = true;
            tracing::info!("💥 Batch marked as failed at: {:?}", failed_at.unwrap());
            break;
        }
    }

    assert!(batch_failed, "Batch should have failed quickly due to immediate 502 errors");

    // Step 7.5: Manually set batch.failed_at since the trigger was removed for performance
    // This simulates what would happen if the batch terminal state was computed
    tracing::info!("🔧 Manually setting batch.failed_at to simulate terminal state computation...");
    sqlx::query(
        r#"
        UPDATE fusillade.batches
        SET failed_at = NOW()
        WHERE id = $1
          AND failed_at IS NULL
        "#,
    )
    .bind(batch_id)
    .execute(&pool)
    .await
    .expect("Failed to set batch.failed_at");

    tracing::info!("✅ Batch.failed_at has been set - batch is now in failed state");

    // Step 8: Poll for SLA daemon to escalate the failed requests
    // The SLA daemon checks every 5 seconds, so we poll until escalation happens
    tracing::info!("🔍 Polling for SLA daemon to escalate failed requests...");

    let (initial_primary, initial_escalation) = tracker.get_counts();
    tracing::info!("   Initial state: primary={}, escalation={}", initial_primary, initial_escalation);
    // Note: We don't assert escalation==0 here because the SLA daemon might have already
    // run between setting batch.failed_at and now. The real verification happens later.

    let mut escalation_detected = initial_escalation >= 3;

    if escalation_detected {
        tracing::info!(
            "🎯 Escalation already detected! {} requests sent to fallback server",
            initial_escalation
        );
    } else {
        let start = tokio::time::Instant::now();
        let timeout = tokio::time::Duration::from_secs(10); // Give it up to 10 seconds (SLA check runs every 5s)

        while start.elapsed() < timeout {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

            let (primary_count, escalation_count) = tracker.get_counts();

            if escalation_count >= 3 {
                escalation_detected = true;
                tracing::info!(
                    "🎯 Escalation detected! {} requests sent to fallback server despite batch failure",
                    escalation_count
                );
                break;
            }

            // Log progress every second
            if start.elapsed().as_secs().is_multiple_of(1) && start.elapsed().as_millis() % 1000 < 100 {
                tracing::info!(
                    "   Check ({}s): primary={}, escalation={}",
                    start.elapsed().as_secs(),
                    primary_count,
                    escalation_count
                );
            }
        }
    }

    // Step 9: Poll until all escalated requests complete
    if escalation_detected {
        tracing::info!("⏳ Waiting for escalated requests to complete...");
        let start = tokio::time::Instant::now();
        let timeout = tokio::time::Duration::from_secs(5);

        while start.elapsed() < timeout {
            let completed_count: i64 = sqlx::query_scalar(
                r#"
                SELECT COUNT(*)
                FROM fusillade.requests
                WHERE batch_id = $1
                  AND is_escalated = true
                  AND state = 'completed'
                "#,
            )
            .bind(batch_id)
            .fetch_one(&pool)
            .await
            .expect("Failed to query completed requests");

            if completed_count >= 3 {
                tracing::info!("✅ All escalated requests completed ({})", completed_count);
                break;
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }

    // Step 10: Verify escalation happened
    let (primary_count, escalation_count) = tracker.get_counts();

    tracing::info!("\n📊 Final Results:");
    tracing::info!("   Primary server requests: {} (all failed with 502)", primary_count);
    tracing::info!("   Escalation server requests: {} (recovered)", escalation_count);

    assert!(
        escalation_detected,
        "SLA escalation should have occurred for failed batch! This test catches the bug where \
         batches with failed_at set were excluded from escalation. Expected requests to escalation \
         server, got 0"
    );

    assert!(
        escalation_count >= 3,
        "Expected at least 3 escalated requests (one per batch request), got {}",
        escalation_count
    );

    // Step 11: Verify Authorization headers
    let auth_headers_captured = captured_auth_header.lock().unwrap();
    tracing::info!("🔑 Captured {} Authorization headers", auth_headers_captured.len());

    assert!(
        !auth_headers_captured.is_empty(),
        "Expected Authorization headers to be captured from escalated requests"
    );

    // Verify all headers have the correct Bearer token
    let expected_auth = "Bearer test-sla-api-key-secret-123";
    for (i, header) in auth_headers_captured.iter().enumerate() {
        tracing::info!("   Header {}: {}", i + 1, header);
        assert_eq!(
            header, expected_auth,
            "Authorization header mismatch! Expected '{}', got '{}'",
            expected_auth, header
        );
    }

    tracing::info!("\n✅ SLA Escalation for Failed Batch Test PASSED!");
    tracing::info!("   ✓ All requests failed immediately at primary server");
    tracing::info!("   ✓ Batch was marked as failed");
    tracing::info!("   ✓ SLA daemon detected approaching deadline despite batch failure");
    tracing::info!("   ✓ Failed requests successfully escalated to working fallback server");
    tracing::info!("   ✓ {} requests recovered via escalation", escalation_count);
    tracing::info!(
        "   ✓ {} escalated requests included correct API key in Authorization header",
        auth_headers_captured.len()
    );
    tracing::info!("   ✓ This test verifies the fix for excluding failed batches from SLA");
    tracing::info!("   ✓ This test also verifies API key is correctly passed to priority endpoints");
}
