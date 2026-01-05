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
