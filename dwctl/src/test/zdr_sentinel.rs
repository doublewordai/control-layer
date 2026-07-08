//! ZDR sentinel verification harness.
//!
//! These tests drive a real `/ai/v1` request whose prompt **and** the mocked
//! upstream response carry unique sentinel strings, then assert the sentinels do
//! not appear in the platform's durable product stores.
//!
//! ## What this harness covers today
//!
//! * **`http_analytics`** — the realtime analytics/billing store. It is body-free
//!   by schema (only token counts, status, model, timing, IDs), so a sentinel
//!   prompt/response must never appear in any of its columns. `zdr_sentinel_realtime_request_does_not_persist_to_analytics`
//!   proves this end-to-end while also confirming the allowed metadata (status,
//!   model, token counts) *is* still recorded.
//!
//! * **Logs (async/flex path)** — `zdr_sentinel_async_batch_failure_does_not_log_payload`
//!   runs a batch end-to-end through the real fusillade daemon against a mock
//!   upstream that returns a sentinel error body, capturing all tracing output
//!   and asserting the sentinel does not appear. `#[sqlx::test]` runs on a
//!   `current_thread` tokio runtime (sqlx `test_block_on`), so a thread-local
//!   subscriber reliably captures the daemon's spawned-task logs — the test
//!   includes a positive control that proves capture works. It is currently
//!   `#[ignore]`d: it already caught a real leak — the *published* fusillade
//!   logs the provider error body at WARN in `to_error_message()`, which a
//!   later fusillade release scrubs — so it activates once control-layer bumps fusillade to
//!   the release containing that fix. (The prompt-sentinel half already passes.)
//!
//! ## What is covered elsewhere
//!
//! * **Per-component log tests + CI guard** — the no-payload-logging guarantee
//!   is also enforced by focused capture tests in onwards (`strict/handlers.rs`),
//!   fusillade (`request/types.rs`) and control-layer
//!   (`request_logging::analytics_handler`), and regression-guarded in CI by
//!   `scripts/check-no-payload-logging.sh`.
//!
//! ## What this harness will cover once ZDR capture-gating lands
//!
//! The body-bearing durable stores — outlet's `http_requests` / `http_responses`
//! and the fusillade `request_templates` / `requests` rows — still persist raw
//! bodies today (request logging captures them by design). They become
//! sentinel-checkable once the ZDR per-request capture gate
//! is implemented. The scaffold for that assertion is
//! `zdr_sentinel_realtime_request_not_in_request_logs`, marked `#[ignore]` until
//! the gate exists; un-ignore it when ZDR request logging is gated.

use crate::api::models::users::Role;
use crate::config::{DaemonConfig, DaemonEnabled};
use crate::db::handlers::api_keys::ApiKeys;
use crate::db::models::api_keys::ApiKeyPurpose;
use crate::test::utils::{
    add_auth_headers, add_deployment_to_group, add_user_to_group, create_test_admin_user, create_test_app_with_config, create_test_config,
    create_test_endpoint, create_test_model, create_test_user, create_test_user_with_roles,
};
use sqlx::PgPool;

/// Unique markers that must never escape into durable product stores.
const SENTINEL_PROMPT: &str = "ZDR-SENTINEL-PROMPT-7e1d4a";
const SENTINEL_COMPLETION: &str = "ZDR-SENTINEL-COMPLETION-9b2c5f";

/// The alias the test routes against.
const MODEL_ALIAS: &str = "zdr-sentinel";

/// Build a wiremock upstream that returns a chat completion whose content is the
/// completion sentinel, wire it up as an inference endpoint + model granted to a
/// fresh user, sync onwards, and return everything needed to make a realtime
/// request and inspect the result.
struct SentinelFixture {
    server: axum_test::TestServer,
    _bg_services: crate::BackgroundServices,
    _mock_server: wiremock::MockServer,
    realtime_key: String,
    user_id: uuid::Uuid,
}

async fn setup_sentinel_fixture(pool: &PgPool) -> SentinelFixture {
    let mock_server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-zdr-sentinel",
            "object": "chat.completion",
            "created": 1_677_652_288,
            "model": "upstream-model",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": SENTINEL_COMPLETION },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8 }
        })))
        .mount(&mock_server)
        .await;

    let mut config = create_test_config();
    config.background_services.onwards_sync.enabled = true;

    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("Failed to create application");
    let (server, bg_services) = app.into_test_server();

    let admin_user = create_test_admin_user(pool, Role::PlatformManager).await;
    let admin_headers = add_auth_headers(&admin_user);
    let user = create_test_user(pool, Role::StandardUser).await;
    let user_headers = add_auth_headers(&user);

    let group: crate::api::models::groups::GroupResponse = server
        .post("/admin/api/v1/groups")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({ "name": "zdr-sentinel-group", "description": "ZDR sentinel test" }))
        .await
        .json();

    server
        .post(&format!("/admin/api/v1/groups/{}/users/{}", group.id, user.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;

    server
        .post("/admin/api/v1/transactions")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "user_id": user.id,
            "transaction_type": "admin_grant",
            "amount": 1000,
            "source_id": admin_user.id,
            "description": "Credits for ZDR sentinel test"
        }))
        .await;

    let endpoint: crate::api::models::inference_endpoints::InferenceEndpointResponse = server
        .post("/admin/api/v1/endpoints")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({ "name": "ZDR Sentinel Endpoint", "url": format!("{}/v1", mock_server.uri()) }))
        .await
        .json();

    let model: crate::api::models::deployments::DeployedModelResponse = server
        .post("/admin/api/v1/models")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": "zdr-sentinel-model",
            "alias": MODEL_ALIAS,
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

    server
        .post(&format!("/admin/api/v1/groups/{}/models/{}", group.id, model.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;

    let realtime_key: crate::api::models::api_keys::ApiKeyResponse = server
        .post(&format!("/admin/api/v1/users/{}/api-keys", user.id))
        .add_header(&user_headers[0].0, &user_headers[0].1)
        .add_header(&user_headers[1].0, &user_headers[1].1)
        .json(&serde_json::json!({ "name": "ZDR Realtime Key", "purpose": "realtime" }))
        .await
        .json();

    bg_services.sync_onwards_config(pool).await.expect("Failed to sync onwards config");

    SentinelFixture {
        server,
        _bg_services: bg_services,
        _mock_server: mock_server,
        realtime_key: realtime_key.key,
        user_id: user.id,
    }
}

/// Send the sentinel-bearing realtime request, polling until onwards has the
/// model (sync is asynchronous). Returns once a 200 is observed.
async fn send_sentinel_request(fixture: &SentinelFixture) {
    let body = serde_json::json!({
        "model": MODEL_ALIAS,
        "messages": [{ "role": "user", "content": SENTINEL_PROMPT }]
    });
    for attempt in 0..100 {
        let resp = fixture
            .server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", fixture.realtime_key))
            .json(&body)
            .await;
        if resp.status_code().as_u16() != 404 {
            assert_eq!(
                resp.status_code().as_u16(),
                200,
                "realtime request should succeed; body: {}",
                resp.text()
            );
            return;
        }
        assert!(attempt < 99, "model never became routable after polling");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Poll for the most-recent row of `table` matching `where_sql`, ordered by
/// `order_col`, returning it as a JSON string once the async writer flushes.
/// The readiness polling in `send_sentinel_request` issues several requests, so
/// 404 probe-rows also land in `http_analytics`; callers filter to the row they
/// care about (e.g. `status_code = 200`). Uses an unchecked query so no `.sqlx`
/// cache entry is needed.
async fn poll_latest_row_json(pool: &PgPool, table: &str, where_sql: &str, order_col: &str) -> Option<String> {
    let query = format!("SELECT to_jsonb(t)::text FROM {table} t WHERE {where_sql} ORDER BY t.{order_col} DESC LIMIT 1");
    for _ in 0..200 {
        let row: Option<(String,)> = sqlx::query_as(&query).fetch_optional(pool).await.expect("query durable store");
        if let Some((json,)) = row {
            return Some(json);
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    None
}

/// Concatenate every row of `table` into one JSON blob, so a sentinel-absence
/// check covers all rows (including the 404 readiness probes).
async fn all_rows_json(pool: &PgPool, table: &str) -> String {
    let query = format!("SELECT coalesce(string_agg(to_jsonb(t)::text, ' '), '') FROM {table} t");
    let row: (String,) = sqlx::query_as(&query).fetch_one(pool).await.expect("aggregate durable store rows");
    row.0
}

/// A ZDR realtime request must not persist prompt or response content into
/// `http_analytics`, while still recording the allowed billing/ops metadata.
#[sqlx::test]
async fn zdr_sentinel_realtime_request_does_not_persist_to_analytics(pool: PgPool) {
    let fixture = setup_sentinel_fixture(&pool).await;
    send_sentinel_request(&fixture).await;

    // Wait for the successful request's analytics row (the readiness probes also
    // write 404 rows; we want the one that actually reached the upstream).
    let success_row = poll_latest_row_json(&pool, "http_analytics", "status_code = 200", "timestamp")
        .await
        .expect("an http_analytics row should be written for the successful request");

    // ZDR: neither the prompt nor the completion may appear in ANY analytics row.
    let all = all_rows_json(&pool, "http_analytics").await;
    assert!(!all.contains(SENTINEL_PROMPT), "prompt sentinel leaked into http_analytics: {all}");
    assert!(
        !all.contains(SENTINEL_COMPLETION),
        "completion sentinel leaked into http_analytics: {all}"
    );

    // Allowed metadata must still be recorded (status, model, token counts).
    let v: serde_json::Value = serde_json::from_str(&success_row).expect("row is valid json");
    assert_eq!(v["status_code"].as_i64(), Some(200), "status should be recorded");
    assert_eq!(v["total_tokens"].as_i64(), Some(8), "token usage should be recorded");
    assert!(v["model"].is_string(), "model should be recorded: {success_row}");
}

/// Scaffold for the post-capture-gate world: once ZDR request logging is gated,
/// a ZDR request must also leave no prompt/response body in
/// outlet's `http_requests`. Today request logging stores bodies by design, so
/// this assertion is expected to fail and is `#[ignore]`d. Un-ignore it (and
/// enable `enable_request_logging`) when the capture gate exists.
#[ignore = "ZDR request-logging capture gate not yet implemented"]
#[sqlx::test]
async fn zdr_sentinel_realtime_request_not_in_request_logs(pool: PgPool) {
    let fixture = setup_sentinel_fixture(&pool).await;
    send_sentinel_request(&fixture).await;

    let row = poll_latest_row_json(&pool, "outlet.http_requests", "true", "correlation_id")
        .await
        .expect("an http_requests row should be written when request logging is enabled");

    assert!(!row.contains(SENTINEL_PROMPT), "prompt sentinel persisted to http_requests: {row}");
    assert!(
        !row.contains(SENTINEL_COMPLETION),
        "completion sentinel persisted to http_requests: {row}"
    );
    let _ = fixture.user_id;
}

// ===========================================================================
// Async / flex (batch) path — log-capture sentinel test
// ===========================================================================

// Lifelike sentinels: a regression that logs the body shows up as readable text.
const ASYNC_PROMPT_SENTINEL: &str = "pikachu-async-prompt-3f9a17";
const ASYNC_ERROR_SENTINEL: &str = "pikachu-fainted-error-body-8c2d04";

/// A `tracing_subscriber` `MakeWriter` that appends all emitted log bytes into a
/// shared buffer, so the test can assert what did (and did not) reach logging.
#[derive(Clone)]
struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The async/flex (batch) failure path must not log prompt or provider-error
/// body content. Runs a batch end-to-end through the real fusillade daemon
/// against a mock upstream that 400s with a sentinel body, capturing all tracing
/// output and asserting neither the prompt nor the error-body sentinel appears.
///
/// `#[sqlx::test]` uses a `current_thread` runtime (sqlx `test_block_on`), so the
/// thread-local subscriber installed here captures the daemon's spawned-task
/// logs. A positive control (a marker logged from a spawned task) proves that.
///
/// IGNORED until control-layer's `fusillade` dependency is bumped to a release
/// containing that fix. This test was written *first* and immediately caught the
/// real leak: published fusillade (19.0.1) logs the provider error body verbatim
/// at WARN in `FailureReason::to_error_message()` on terminal failure — live in
/// prod. A later fusillade release scrubs it; un-ignore once that lands here via the
/// dependency bump. (The prompt-sentinel half of this test already passes.)
#[ignore = "async/flex error-body leak fixed in fusillade; un-ignore after the control-layer fusillade bump"]
#[sqlx::test]
async fn zdr_sentinel_async_batch_failure_does_not_log_payload(pool: PgPool) {
    // Capture every tracing event on this (single) test thread for the whole test.
    let log_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(CaptureWriter(log_buf.clone()))
        .finish();
    let _log_guard = tracing::subscriber::set_default(subscriber);

    // A user allowed to run batches, in the everyone-group, with a batch key.
    let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
    add_user_to_group(&pool, user.id, uuid::Uuid::nil()).await;
    let batch_api_key = {
        let mut conn = pool.acquire().await.expect("acquire");
        ApiKeys::new(&mut conn)
            .get_or_create_hidden_key(user.id, ApiKeyPurpose::Batch, user.id)
            .await
            .expect("batch api key")
    };

    // Mock upstream: 400 with a body that echoes the prompt sentinel — exactly the
    // shape that could leak prompt/response content through failure logging.
    let mock_server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": { "message": ASYNC_ERROR_SENTINEL, "type": "invalid_request_error" }
        })))
        .mount(&mock_server)
        .await;

    let endpoint_id = create_test_endpoint(&pool, "zdr-async-endpoint", user.id).await;
    sqlx::query("UPDATE inference_endpoints SET url = $1 WHERE id = $2")
        .bind(mock_server.uri())
        .bind(endpoint_id)
        .execute(&pool)
        .await
        .expect("update endpoint url");
    let deployment_id = create_test_model(&pool, "pikachu-async-model", "pikachu-async", endpoint_id, user.id).await;
    add_deployment_to_group(&pool, deployment_id, uuid::Uuid::nil(), user.id).await;

    // App with the real batch daemon running.
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
    let (_server, _bg_services) = create_test_app_with_config(pool.clone(), config, false).await;

    // Insert a batch whose single request carries the prompt sentinel.
    let file_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO fusillade.files (id, name, purpose, size_bytes, status, uploaded_by, created_at)
         VALUES ($1, 'zdr.jsonl', 'batch', 100, 'processed', $2, NOW())",
    )
    .bind(file_id)
    .bind(user.id.to_string())
    .execute(&pool)
    .await
    .expect("insert file");

    let template_id = uuid::Uuid::new_v4();
    let request_body = serde_json::json!({
        "model": "pikachu-async",
        "messages": [{ "role": "user", "content": ASYNC_PROMPT_SENTINEL }]
    })
    .to_string();
    sqlx::query(
        "INSERT INTO fusillade.request_templates (id, file_id, model, api_key, endpoint, path, body, custom_id, method)
         VALUES ($1, $2, 'pikachu-async', $3, $4, '/v1/chat/completions', $5, 'req-1', 'POST')",
    )
    .bind(template_id)
    .bind(file_id)
    .bind(&batch_api_key)
    .bind(mock_server.uri())
    .bind(&request_body)
    .execute(&pool)
    .await
    .expect("insert template");

    let batch_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO fusillade.batches (id, created_by, file_id, endpoint, completion_window, expires_at, created_at)
         VALUES ($1, $2, $3, '/v1/chat/completions', '24h', $4, NOW())",
    )
    .bind(batch_id)
    .bind(user.id.to_string())
    .bind(file_id)
    .bind(chrono::Utc::now() + chrono::Duration::hours(24))
    .execute(&pool)
    .await
    .expect("insert batch");

    let request_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO fusillade.requests (id, batch_id, template_id, model, state, created_at)
         VALUES ($1, $2, $3, 'pikachu-async', 'pending', NOW())",
    )
    .bind(request_id)
    .bind(batch_id)
    .bind(template_id)
    .execute(&pool)
    .await
    .expect("insert request");

    // Wait for the daemon to claim, dispatch, get the 400, and mark it failed.
    let mut failed = false;
    for _ in 0..150 {
        let state: Option<String> = sqlx::query_scalar("SELECT state::text FROM fusillade.requests WHERE id = $1")
            .bind(request_id)
            .fetch_optional(&pool)
            .await
            .expect("query request state");
        if state.as_deref() == Some("failed") {
            failed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(failed, "daemon never marked the request failed");

    // Positive control: prove the capturing subscriber sees events emitted from a
    // spawned task (the same mechanism the daemon uses). Without this, an empty
    // capture could make the sentinel assertions pass vacuously.
    const PROBE: &str = "zdr-capture-probe-marker-do-not-remove";
    tokio::spawn(async { tracing::warn!("{PROBE}") }).await.unwrap();

    let logs = String::from_utf8_lossy(&log_buf.lock().unwrap()).into_owned();
    assert!(
        logs.contains(PROBE),
        "log capture is not observing spawned-task events; the sentinel assertions below would be vacuous"
    );

    // The actual ZDR assertions: no prompt or provider-error body in the logs.
    assert!(!logs.contains(ASYNC_PROMPT_SENTINEL), "prompt content leaked into async/flex logs");
    assert!(
        !logs.contains(ASYNC_ERROR_SENTINEL),
        "provider error body leaked into async/flex logs (fusillade daemon \
         terminal-failure log). Fixed in fusillade — un-ignore this \
         test once control-layer's fusillade dependency is bumped to the release \
         containing that scrub."
    );
}
