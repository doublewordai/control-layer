//! End-to-end SLA escalation test
//!
//! This test runs the full crate application with batch daemon enabled
//! and verifies that SLA escalation actually works.

use crate::test::utils::{
    add_deployment_to_group, add_user_to_group, create_test_admin_user, create_test_app_with_config, create_test_config,
    create_test_endpoint, create_test_model, create_test_user_with_roles,
};
use crate::{
    api::models::users::Role,
    config::{DaemonConfig, DaemonEnabled, ModelSource},
    db::handlers::api_keys::ApiKeys,
    db::models::api_keys::ApiKeyPurpose,
};
use chrono::{Duration, Utc};
use fusillade::daemon::SlaAction;
use sqlx::{PgPool, Row};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Helper to get a user's batch API key
async fn get_batch_api_key(pool: &PgPool, user_id: Uuid) -> String {
    let mut conn = pool.acquire().await.expect("Failed to acquire connection");
    let mut api_keys_repo = ApiKeys::new(&mut conn);
    let api_key = api_keys_repo
        .get_or_create_hidden_key(user_id, ApiKeyPurpose::Batch)
        .await
        .expect("Failed to get batch API key");
    api_key
}

/// Helper to get or create a realtime API key for a user (for actual inference requests)
async fn get_realtime_api_key(pool: &PgPool, user_id: Uuid) -> String {
    let mut conn = pool.acquire().await.expect("Failed to acquire connection");
    let mut api_keys_repo = ApiKeys::new(&mut conn);
    let api_key = api_keys_repo
        .get_or_create_hidden_key(user_id, ApiKeyPurpose::Realtime)
        .await
        .expect("Failed to get realtime API key");
    api_key
}

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

    // Add user to everyone group so they can access models in that group
    add_user_to_group(&pool, user.id, uuid::Uuid::nil()).await;

    // Create BOTH batch and realtime API keys for the user BEFORE starting the app
    // Batch key: for uploading files and creating batches
    // Realtime key: for the actual chat completion requests made by fusillade
    let user_batch_api_key = get_batch_api_key(&pool, user.id).await;
    let _user_realtime_api_key = get_realtime_api_key(&pool, user.id).await;

    tracing::info!("✅ Created batch and realtime API keys for user");

    // Setup: Start ONE smart mock server that responds differently based on model field
    let tracker = RequestTracker::new();

    let smart_server = MockServer::start().await;
    let smart_tracker = tracker.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &wiremock::Request| {
            // Parse request body to check model field
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
            let model = body.get("model").and_then(|m| m.as_str()).unwrap_or("");

            if model == "test-model-escalation" {
                // ESCALATION MODEL: Fast response
                smart_tracker.increment_escalation();
                tracing::info!("🟢 Escalation model request received ({}), responding quickly", model);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "chatcmpl-escalated-123",
                    "object": "chat.completion",
                    "created": 1677652288,
                    "model": "test-model-escalation",
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": "Fast response from escalation model"
                        },
                        "finish_reason": "stop"
                    }],
                    "usage": {
                        "prompt_tokens": 10,
                        "completion_tokens": 5,
                        "total_tokens": 15
                    }
                }))
            } else {
                // PRIMARY MODEL: Slow response (exceeds SLA)
                smart_tracker.increment_primary();
                tracing::info!("🐢 Primary model request received ({}), will be slow", model);
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_secs(2)) // Slow but successful (exceeds SLA)
                    .set_body_json(serde_json::json!({
                        "id": "chatcmpl-primary-slow-123",
                        "object": "chat.completion",
                        "created": 1677652288,
                        "model": "test-model",
                        "choices": [{
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "Slow response from primary model"
                            },
                            "finish_reason": "stop"
                        }],
                        "usage": {
                            "prompt_tokens": 10,
                            "completion_tokens": 5,
                            "total_tokens": 15
                        }
                    }))
            }
        })
        .mount(&smart_server)
        .await;

    tracing::info!("🌐 Smart server: {} (responds based on model field)", smart_server.uri());

    // Step 1: Create ONE endpoint pointing to the smart server
    let endpoint_id = create_test_endpoint(&pool, "smart-endpoint", user.id).await;
    sqlx::query(
        r#"
        UPDATE inference_endpoints
        SET url = $1
        WHERE id = $2
        "#,
    )
    .bind(smart_server.uri())
    .bind(endpoint_id)
    .execute(&pool)
    .await
    .expect("Failed to update smart endpoint URL");

    // Step 2: Create BOTH models pointing to the SAME endpoint
    let deployment_id = create_test_model(&pool, "test-model", "test-model", endpoint_id, user.id).await;
    add_deployment_to_group(&pool, deployment_id, uuid::Uuid::nil(), user.id).await;

    let escalation_deployment_id = create_test_model(
        &pool,
        "test-model-escalation",
        "test-model-escalation",
        endpoint_id, // SAME endpoint as primary!
        user.id,
    )
    .await;
    add_deployment_to_group(&pool, escalation_deployment_id, uuid::Uuid::nil(), user.id).await;

    tracing::info!("🔧 Created models (both point to same smart server):");
    tracing::info!("   - test-model → {}", smart_server.uri());
    tracing::info!("   - test-model-escalation → {}", smart_server.uri());

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
        // Configure model escalation for SLA
        model_escalations: {
            let mut map = HashMap::new();
            map.insert(
                "test-model".to_string(),
                fusillade::ModelEscalationConfig {
                    escalation_model: "test-model-escalation".to_string(),
                    escalation_api_key: None, // Use same API key for escalation
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

    // Enable onwards sync so the routing layer knows about our test models
    config.background_services.onwards_sync.enabled = true;
    config.background_services.probe_scheduler.enabled = false;
    config.background_services.leader_election.enabled = false;

    // Add model source (without default models since we create the model manually)
    config.model_sources = vec![ModelSource {
        name: "test-source".to_string(),
        url: smart_server.uri().parse().expect("Failed to parse server URI"),
        api_key: None,
        sync_interval: std::time::Duration::from_secs(3600), // Don't sync during test
        default_models: None,                                // We create the model manually via create_test_model
    }];

    // Debug: Check group memberships and deployment access BEFORE starting app
    let user_groups = sqlx::query!("SELECT group_id FROM user_groups WHERE user_id = $1", user.id)
        .fetch_all(&pool)
        .await
        .expect("Failed to fetch user groups");
    tracing::info!("🔍 User {} is in {} groups:", user.username, user_groups.len());
    for ug in &user_groups {
        tracing::info!("   - Group ID: {}", ug.group_id);
    }

    let everyone_deployments = sqlx::query!("SELECT deployment_id FROM deployment_groups WHERE group_id = $1", uuid::Uuid::nil())
        .fetch_all(&pool)
        .await
        .expect("Failed to fetch everyone group deployments");
    tracing::info!("🔍 Everyone group (nil) has {} deployments:", everyone_deployments.len());
    for gd in &everyone_deployments {
        tracing::info!("   - Deployment ID: {}", gd.deployment_id);
    }

    tracing::info!("📋 Creating application with SLA daemon enabled");

    // Create test app (no real TCP port needed - fusillade will hit mock server directly)
    let (_server, bg_services) = create_test_app_with_config(pool.clone(), config, false).await;

    tracing::info!("✅ Application started with SLA daemon running");

    // Step 4: Create file and request templates directly in database (bypassing API)
    let file_id = Uuid::new_v4();
    let jsonl_content = create_test_jsonl();

    // Create file in fusillade database
    sqlx::query(
        r#"
        INSERT INTO fusillade.files (id, name, purpose, size_bytes, status, uploaded_by, created_at)
        VALUES ($1, 'test.jsonl', 'batch', $2, 'processed', $3, NOW())
        "#,
    )
    .bind(file_id)
    .bind(jsonl_content.len() as i64)
    .bind(user.id.to_string())
    .execute(&pool)
    .await
    .expect("Failed to create file");

    // Create request templates pointing directly to smart mock server
    // The smart server will respond differently based on model field in request body
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
        .bind(smart_server.uri())
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

    tracing::info!("📄 File and request templates created directly in database: {}", file_id);

    // Step 5: Create batch directly in database
    let batch_id = Uuid::new_v4();
    let expires_at = Utc::now() + Duration::hours(24);

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

    tracing::info!("📦 Batch created directly in database: {}", batch_id);

    // Step 5.5: Create request records from templates (this is what the API does automatically)
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

    // Step 6: Update batch expiry to trigger SLA (set to expire in 25 seconds)
    // This is within our 30-second threshold, so it should trigger escalation
    let new_expiry = Utc::now() + Duration::seconds(25);
    update_batch_expiry(&pool, batch_id, new_expiry).await;

    tracing::info!("⏰ Batch expiry set to: {}", new_expiry);
    tracing::info!("   (expires in 25 seconds, SLA threshold is 30 seconds)");

    // Debug: Show initial request templates
    let templates = sqlx::query(
        r#"
        SELECT id, file_id, model, api_key, endpoint, custom_id
        FROM fusillade.request_templates
        WHERE file_id = $1
        ORDER BY custom_id
        "#,
    )
    .bind(file_id)
    .fetch_all(&pool)
    .await
    .expect("Failed to fetch templates");

    tracing::info!("📋 Initial request templates (count: {}):", templates.len());
    for template in &templates {
        let custom_id: Option<String> = template.try_get("custom_id").ok();
        let model: Option<String> = template.try_get("model").ok();
        let endpoint: String = template.try_get("endpoint").unwrap_or_default();
        tracing::info!("   - custom_id: {:?}, model: {:?}, endpoint: {}", custom_id, model, &endpoint);
    }

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

            // Debug: Show all templates once escalation templates are created
            if escalation_count > 0 && start.elapsed().as_secs() >= 1 {
                let all_templates = sqlx::query(
                    r#"
                    SELECT id, file_id, model, api_key, endpoint, custom_id
                    FROM fusillade.request_templates
                    WHERE file_id = $1
                    ORDER BY custom_id
                    "#,
                )
                .bind(file_id)
                .fetch_all(&pool)
                .await
                .expect("Failed to fetch templates");

                if all_templates.len() > 3 {
                    tracing::info!("📋 All request templates (count: {}):", all_templates.len());
                    for template in &all_templates {
                        let custom_id: Option<String> = template.try_get("custom_id").ok();
                        let model: Option<String> = template.try_get("model").ok();
                        let endpoint: String = template.try_get("endpoint").unwrap_or_default();
                        tracing::info!("   - custom_id: {:?}, model: {:?}, endpoint: {}", custom_id, model, &endpoint);
                    }
                }
            }
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

    // Cleanup: Drop background services (automatic cleanup)
    drop(bg_services);
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

    // Add user to everyone group so they can access models in that group
    add_user_to_group(&pool, user.id, uuid::Uuid::nil()).await;

    // Create BOTH batch and realtime API keys for the user BEFORE starting the app
    // Batch key: for uploading files and creating batches
    // Realtime key: for the actual chat completion requests made by fusillade
    let user_batch_api_key = get_batch_api_key(&pool, user.id).await;
    let _user_realtime_api_key = get_realtime_api_key(&pool, user.id).await;

    tracing::info!("✅ Created batch and realtime API keys for user");

    // Setup: Start mock servers
    let tracker = RequestTracker::new();

    // Track captured authorization headers for API key verification
    let captured_auth_header = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

    // Smart server - responds based on model in request body
    // - test-model-fail (original): immediate 502 error to trigger batch failure
    // - test-model-fail-escalation: success (200) with fast response
    let primary_server = MockServer::start().await;
    let primary_tracker = tracker.clone();
    let escalation_tracker = tracker.clone();
    let auth_header_clone = captured_auth_header.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &wiremock::Request| {
            // Parse request body to check model
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
            let model = body.get("model").and_then(|m| m.as_str()).unwrap_or("");

            if model == "test-model-fail-escalation" {
                // Escalated request - respond quickly with success
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

                tracing::info!("🟢 Escalation request received (model: {}, responding with success)", model);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "chatcmpl-escalated-456",
                    "object": "chat.completion",
                    "created": 1677652288,
                    "model": "test-model-fail-escalation",
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
            } else {
                // Original request - return immediate 502 error
                primary_tracker.increment_primary();
                tracing::info!("🔴 Primary request received (model: {}, returning 502)", model);
                ResponseTemplate::new(502).set_body_json(serde_json::json!({
                    "error": {
                        "message": "Bad Gateway - upstream service unavailable",
                        "type": "server_error"
                    }
                }))
            }
        })
        .mount(&primary_server)
        .await;

    tracing::info!("🌐 Smart mock server started: {}", primary_server.uri());
    tracing::info!("   - test-model-fail → 502 error (triggers batch failure)");
    tracing::info!("   - test-model-fail-escalation → 200 success");

    // IMPORTANT: Create models BEFORE starting the application
    // Both original and escalation models point to the same smart server
    // The server differentiates based on the model name in the request body

    // Step 1: Create endpoint pointing to smart mock server
    let endpoint_id = create_test_endpoint(&pool, "smart-endpoint-fail", user.id).await;
    sqlx::query(
        r#"
        UPDATE inference_endpoints
        SET url = $1
        WHERE id = $2
        "#,
    )
    .bind(primary_server.uri())
    .bind(endpoint_id)
    .execute(&pool)
    .await
    .expect("Failed to update endpoint URL");

    // Step 2: Create primary model pointing to smart server
    let deployment_id = create_test_model(&pool, "test-model-fail", "test-model-fail", endpoint_id, user.id).await;
    add_deployment_to_group(&pool, deployment_id, uuid::Uuid::nil(), user.id).await;

    // Step 2.5: Create platform manager for escalation access
    let platform_manager = create_test_admin_user(&pool, Role::PlatformManager).await;
    let escalation_api_key = get_batch_api_key(&pool, platform_manager.id).await;

    // Step 2.6: Create escalation model also pointing to smart server
    let escalation_deployment_id = create_test_model(
        &pool,
        "test-model-fail-escalation",
        "test-model-fail-escalation",
        endpoint_id, // Same endpoint as primary!
        user.id,
    )
    .await;

    // Add escalation model to everyone group so it can be accessed
    add_deployment_to_group(&pool, escalation_deployment_id, uuid::Uuid::nil(), user.id).await;

    tracing::info!(
        "🔧 Created models: test-model-fail and test-model-fail-escalation (both → {})",
        primary_server.uri()
    );

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
        // Configure model escalation for SLA with API key override
        model_escalations: {
            let mut map = HashMap::new();
            map.insert(
                "test-model-fail".to_string(),
                fusillade::ModelEscalationConfig {
                    escalation_model: "test-model-fail-escalation".to_string(),
                    escalation_api_key: Some(escalation_api_key.clone()), // Use platform manager API key for escalation
                },
            );
            map
        },
        sla_check_interval_seconds: 2, // Check every 2 seconds
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

    // Enable onwards sync so the routing layer knows about our test models
    config.background_services.onwards_sync.enabled = true;
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

    // Create test app (no real TCP port needed - fusillade will hit mock servers directly)
    let (_server, bg_services) = create_test_app_with_config(pool.clone(), config, false).await;

    tracing::info!("✅ Application started with SLA daemon running");

    // Step 4: Create file and request templates directly in database (bypassing API)
    let file_id = Uuid::new_v4();
    let jsonl_content = r#"{"custom_id": "req-1", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "test-model-fail", "messages": [{"role": "user", "content": "Test 1"}]}}
{"custom_id": "req-2", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "test-model-fail", "messages": [{"role": "user", "content": "Test 2"}]}}
{"custom_id": "req-3", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "test-model-fail", "messages": [{"role": "user", "content": "Test 3"}]}}"#;

    // Create file in fusillade database
    sqlx::query(
        r#"
        INSERT INTO fusillade.files (id, name, purpose, size_bytes, status, uploaded_by, created_at)
        VALUES ($1, 'test-fail.jsonl', 'batch', $2, 'processed', $3, NOW())
        "#,
    )
    .bind(file_id)
    .bind(jsonl_content.len() as i64)
    .bind(user.id.to_string())
    .execute(&pool)
    .await
    .expect("Failed to create file");

    // Create request templates pointing directly to primary mock server
    for i in 1..=3 {
        let template_id = Uuid::new_v4();
        sqlx::query(
            r#"
            INSERT INTO fusillade.request_templates (id, file_id, model, api_key, endpoint, path, body, custom_id, method)
            VALUES ($1, $2, 'test-model-fail', $3, $4, '/v1/chat/completions', $5, $6, 'POST')
            "#,
        )
        .bind(template_id)
        .bind(file_id)
        .bind(&user_batch_api_key)
        .bind(primary_server.uri())
        .bind(
            serde_json::to_string(
                &serde_json::json!({"model": "test-model-fail", "messages": [{"role": "user", "content": format!("Test {}", i)}]}),
            )
            .unwrap(),
        )
        .bind(format!("req-{}", i))
        .execute(&pool)
        .await
        .expect("Failed to create request template");
    }

    tracing::info!("📄 File and request templates created directly in database: {}", file_id);

    // Step 5: Create batch directly in database with 1 hour completion window
    let batch_id = Uuid::new_v4();
    let expires_at = Utc::now() + Duration::hours(1);

    sqlx::query(
        r#"
        INSERT INTO fusillade.batches (id, created_by, file_id, endpoint, completion_window, expires_at, created_at)
        VALUES ($1, $2, $3, '/v1/chat/completions', '1h', $4, NOW())
        "#,
    )
    .bind(batch_id)
    .bind(user.id.to_string())
    .bind(file_id)
    .bind(expires_at)
    .execute(&pool)
    .await
    .expect("Failed to create batch");

    tracing::info!("📦 Batch created directly in database: {}", batch_id);

    // Step 5.5: Create request records from templates (this is what the API does automatically)
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
            VALUES ($1, $2, $3, 'test-model-fail', 'pending', NOW())
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

    // Step 6: Update batch expiry to trigger SLA (set to expire in 25 seconds)
    // This ensures the batch is within the 30-second SLA threshold
    let new_expiry = Utc::now() + Duration::seconds(25);
    update_batch_expiry(&pool, batch_id, new_expiry).await;

    tracing::info!("⏰ Batch expiry set to: {}", new_expiry);
    tracing::info!("   (expires in 25 seconds, SLA threshold is 30 seconds)");

    // Step 7: Wait for requests to fail
    tracing::info!("⏳ Waiting for requests to fail at primary server...");

    // Poll until all requests have failed
    let start = tokio::time::Instant::now();
    let timeout = tokio::time::Duration::from_secs(5);
    loop {
        let failed_count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)::bigint
            FROM fusillade.requests
            WHERE batch_id = $1 AND state = 'failed'
            "#,
        )
        .bind(batch_id)
        .fetch_one(&pool)
        .await
        .expect("Failed to count failed requests");

        if failed_count >= 3 {
            tracing::info!("✅ All 3 requests have failed");
            break;
        }

        if start.elapsed() >= timeout {
            panic!("Timeout waiting for requests to fail");
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

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
    // The SLA daemon checks every 2 seconds, so we poll until escalation happens
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
        let timeout = tokio::time::Duration::from_secs(5); // Give it up to 5 seconds (SLA check runs every 2s)

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

    // Verify all headers have the correct Bearer token (platform manager's batch API key)
    let expected_auth = format!("Bearer {}", escalation_api_key);
    for (i, header) in auth_headers_captured.iter().enumerate() {
        tracing::info!("   Header {}: {}", i + 1, header);
        assert_eq!(
            header, &expected_auth,
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

    // Cleanup: Drop background services (automatic cleanup)
    drop(bg_services);
}
