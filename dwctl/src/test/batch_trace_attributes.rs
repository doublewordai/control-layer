//! Customer attributes on the spans of a batch request, end to end.
//!
//! Runs a batch through the real fusillade daemon against a mock upstream that
//! rejects it, and checks `fusillade.process_request` (the account, completion
//! window and custom id) and, once the notification poller claims the finalized
//! batch, `fusillade.batch_finalized` (outcome, counts, failure class).
//!
//! `#[dwctl_test_macros::test]` runs on a current-thread runtime, so the
//! thread-local subscriber installed here sees the daemon's spawned tasks.

use crate::api::models::users::Role;
use crate::config::{DaemonConfig, DaemonEnabled};
use crate::db::handlers::api_keys::ApiKeys;
use crate::db::models::api_keys::ApiKeyPurpose;
use crate::test::utils::{
    add_deployment_to_group, add_user_to_group, create_test_app_with_config, create_test_config, create_test_endpoint, create_test_model,
    create_test_user_with_roles,
};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::{
    error::OTelSdkResult,
    trace::{SdkTracerProvider, SpanData, SpanExporter},
};
use sqlx::PgPool;
use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt;

#[derive(Clone, Debug, Default)]
struct Exported(Arc<Mutex<Vec<SpanData>>>);
impl SpanExporter for Exported {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        self.0.lock().unwrap().extend(batch);
        Ok(())
    }
}

fn attr(span: &SpanData, key: &str) -> Option<String> {
    span.attributes.iter().find(|a| a.key.as_str() == key).map(|a| a.value.to_string())
}

fn spans_named(exported: &Exported, name: &str) -> Vec<SpanData> {
    exported.0.lock().unwrap().iter().filter(|s| s.name == name).cloned().collect()
}

#[dwctl_test_macros::test]
async fn batch_request_and_batch_finalized_spans_carry_the_account(pool: PgPool) {
    let exported = Exported::default();
    let provider = SdkTracerProvider::builder().with_simple_exporter(exported.clone()).build();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(provider.tracer("batch-trace-test"))),
    );

    let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
    sqlx::query("UPDATE users SET display_name = 'Example Batch Co' WHERE id = $1")
        .bind(user.id)
        .execute(&pool)
        .await
        .unwrap();
    add_user_to_group(&pool, user.id, uuid::Uuid::nil()).await;
    let batch_api_key = {
        let mut conn = pool.acquire().await.expect("acquire");
        ApiKeys::new(&mut conn)
            .get_or_create_hidden_key(user.id, ApiKeyPurpose::Batch, user.id)
            .await
            .expect("batch api key")
    };

    let mock_server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": { "message": "rejected by the mock", "type": "invalid_request_error" }
        })))
        .mount(&mock_server)
        .await;

    let endpoint_id = create_test_endpoint(&pool, "trace-batch-endpoint", user.id).await;
    sqlx::query("UPDATE inference_endpoints SET url = $1 WHERE id = $2")
        .bind(mock_server.uri())
        .bind(endpoint_id)
        .execute(&pool)
        .await
        .unwrap();
    let deployment_id = create_test_model(&pool, "trace-batch-model", "trace-batch", endpoint_id, user.id).await;
    add_deployment_to_group(&pool, deployment_id, uuid::Uuid::nil(), user.id).await;

    let mut config = create_test_config();
    config.background_services.batch_daemon = DaemonConfig {
        enabled: DaemonEnabled::Always,
        claim_interval_ms: 100,
        max_retries: Some(0),
        ..Default::default()
    };
    config.background_services.onwards_sync.enabled = true;
    config.background_services.probe_scheduler.enabled = false;
    config.background_services.leader_election.enabled = false;
    // The poller emits the batch span whether or not email or webhooks are configured.
    config.background_services.notifications.poll_interval = std::time::Duration::from_millis(100);
    let (_server, _bg_services) = create_test_app_with_config(pool.clone(), config, false).await;

    let file_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO fusillade.files (id, name, purpose, size_bytes, status, uploaded_by, created_at)
         VALUES ($1, 'trace.jsonl', 'batch', 100, 'processed', $2, NOW())",
    )
    .bind(file_id)
    .bind(user.id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    let template_id = uuid::Uuid::new_v4();
    crate::test::utils::insert_fusillade_template(
        &pool,
        template_id,
        Some(file_id),
        "trace-batch",
        &batch_api_key,
        &mock_server.uri(),
        "/v1/chat/completions",
        &serde_json::json!({"model": "trace-batch", "messages": [{"role": "user", "content": "hi"}]}).to_string(),
        Some("custom-req-1"),
    )
    .await;
    let batch_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO fusillade.batches (id, created_by, file_id, endpoint, completion_window, expires_at, created_at, total_requests)
         VALUES ($1, $2, $3, '/v1/chat/completions', '24h', $4, NOW(), 1)",
    )
    .bind(batch_id)
    .bind(user.id.to_string())
    .bind(file_id)
    .bind(chrono::Utc::now() + chrono::Duration::hours(24))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO fusillade.requests (id, batch_id, template_id, model, custom_id, state, created_at)
         VALUES ($1, $2, $3, 'trace-batch', 'custom-req-1', 'pending', NOW())",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(batch_id)
    .bind(template_id)
    .execute(&pool)
    .await
    .unwrap();

    // Poll until the finalized batch has been claimed and its span exported.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let finalized = loop {
        provider.force_flush().unwrap();
        let found = spans_named(&exported, "fusillade.batch_finalized");
        if !found.is_empty() {
            break found;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no fusillade.batch_finalized span was emitted"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };

    let processed = spans_named(&exported, "fusillade.process_request");
    assert_eq!(processed.len(), 1, "one process_request span for the one request");
    assert_eq!(attr(&processed[0], "doubleword.account_id"), Some(user.id.to_string()));
    assert_eq!(attr(&processed[0], "completion_window").as_deref(), Some("24h"));
    assert_eq!(attr(&processed[0], "custom_id").as_deref(), Some("custom-req-1"));

    assert_eq!(finalized.len(), 1, "one batch_finalized span per batch");
    let batch = &finalized[0];
    assert_eq!(attr(batch, "batch_id"), Some(batch_id.to_string()));
    assert_eq!(attr(batch, "doubleword.account_id"), Some(user.id.to_string()));
    assert_eq!(attr(batch, "doubleword.account_name").as_deref(), Some("Example Batch Co"));
    assert_eq!(attr(batch, "gen_ai.request.model").as_deref(), Some("trace-batch"));
    assert_eq!(attr(batch, "batch.outcome").as_deref(), Some("failed"));
    assert_eq!(attr(batch, "batch.total_requests").as_deref(), Some("1"));
    assert_eq!(attr(batch, "batch.failed_requests").as_deref(), Some("1"));
    assert_eq!(attr(batch, "batch.sample_error").as_deref(), Some("http_400"));
}
