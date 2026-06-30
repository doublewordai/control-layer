//! ZDR sentinel verification harness (COR-501, part of COR-479).
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
//! ## What is covered elsewhere
//!
//! * **Logs / OTEL spans** — the no-payload-logging guarantee is enforced by
//!   per-component capture tests in onwards (`strict/handlers.rs`),
//!   fusillade (`request/types.rs`) and control-layer
//!   (`request_logging::analytics_handler`), and is regression-guarded in CI by
//!   `scripts/check-no-payload-logging.sh` (COR-500). Capturing the full app's
//!   tracing output here is intentionally avoided — `#[sqlx::test]` runs on a
//!   multi-threaded runtime where a thread-local subscriber would miss events
//!   emitted on background-task worker threads.
//!
//! ## What this harness will cover once ZDR capture-gating lands
//!
//! The body-bearing durable stores — outlet's `http_requests` / `http_responses`
//! and the fusillade `request_templates` / `requests` rows — still persist raw
//! bodies today (request logging captures them by design). They become
//! sentinel-checkable once the ZDR per-request capture gate (COR-479 Part 1/2)
//! is implemented. The scaffold for that assertion is
//! `zdr_sentinel_realtime_request_not_in_request_logs`, marked `#[ignore]` until
//! the gate exists; un-ignore it when ZDR request logging is gated.

use crate::api::models::users::Role;
use crate::test::utils::{add_auth_headers, create_test_admin_user, create_test_config, create_test_user};
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

/// Scaffold for the post-capture-gate world: once ZDR request logging is gated
/// (COR-479 Part 1/2), a ZDR request must also leave no prompt/response body in
/// outlet's `http_requests`. Today request logging stores bodies by design, so
/// this assertion is expected to fail and is `#[ignore]`d. Un-ignore it (and
/// enable `enable_request_logging`) when the capture gate exists.
#[ignore = "ZDR request-logging capture gate not yet implemented (COR-479 Part 1/2)"]
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
