//! End-to-end proof that flex live-streaming delivers real per-token
//! deltas — not the poll-and-replay mega-delta — once
//! `flex_live_streaming.enabled` is on, and that it's off unchanged.
//!
//! Asserts on content shape (small per-chunk deltas vs. one mega-delta),
//! not timing: `TestServer` buffers the whole SSE body before the request
//! future resolves, so inter-frame arrival can't be observed directly.
//! Wall-clock time is checked too, as a corroborating signal only.

use std::time::Duration;

use axum_test::TestServer;
use sqlx::PgPool;

use crate::api::models::api_keys::ApiKeyResponse;
use crate::api::models::deployments::DeployedModelResponse;
use crate::api::models::groups::GroupResponse;
use crate::api::models::inference_endpoints::InferenceEndpointResponse;
use crate::api::models::users::Role;
use crate::chunk_relay::ChunkRelayConfig;
use crate::config::{DaemonConfig, DaemonEnabled, FlexLiveStreamingConfig};
use crate::test::utils::{
    add_auth_headers, create_test_admin_user, create_test_app_with_real_loopback, create_test_config, create_test_user,
};

fn test_redis_url() -> String {
    std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string())
}

/// A real, socket-bound SSE mock server with real wall-clock gaps between
/// frames — `wiremock` can't do this (its `Respond` trait returns one fixed
/// `ResponseTemplate`, no mid-body delays).
async fn start_slow_sse_mock_server(frames: Vec<(u64, String)>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock server");
    let addr = listener.local_addr().expect("mock server local addr");

    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move || {
            let frames = frames.clone();
            async move {
                let stream = async_stream::stream! {
                    for (delay_ms, frame) in frames {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        yield Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(format!("data: {frame}\n\n")));
                    }
                };
                axum::response::Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(axum::body::Body::from_stream(stream))
                    .unwrap()
            }
        }),
    );

    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock server serve");
    });

    format!("http://{addr}")
}

fn role_chunk() -> String {
    r#"{"id":"chatcmpl-live","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#.to_string()
}

fn content_chunk(content: &str) -> String {
    format!(
        r#"{{"id":"chatcmpl-live","object":"chat.completion.chunk","created":1,"model":"m","choices":[{{"index":0,"delta":{{"content":"{content}"}},"finish_reason":null}}]}}"#
    )
}

fn finish_chunk() -> String {
    r#"{"id":"chatcmpl-live","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#.to_string()
}

/// Role chunk, two content chunks 150ms apart, a finish chunk, `[DONE]` —
/// paced the way a real streaming provider paces tokens.
fn mock_frames() -> Vec<(u64, String)> {
    vec![
        (0, role_chunk()),
        (150, content_chunk("Hello")),
        (150, content_chunk(" world")),
        (150, finish_chunk()),
        (0, "[DONE]".to_string()),
    ]
}

fn parse_sse_content_deltas(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .filter_map(|v| v["choices"][0]["delta"]["content"].as_str().map(str::to_string))
        .filter(|s| !s.is_empty())
        .collect()
}

struct FlexFixture {
    server: TestServer,
    // Held for the fixture's lifetime — dropping this stops the daemon.
    bg_services: crate::BackgroundServices,
    api_key: String,
}

async fn setup_flex_fixture(pool: &PgPool, live_streaming_enabled: bool) -> FlexFixture {
    let mock_endpoint_url = start_slow_sse_mock_server(mock_frames()).await;

    let mut config = create_test_config();
    config.background_services.onwards_sync.enabled = true;
    config.background_services.probe_scheduler.enabled = false;
    config.background_services.leader_election.enabled = false;
    config.background_services.batch_daemon = DaemonConfig {
        enabled: DaemonEnabled::Always,
        claim_interval_ms: 50,
        max_retries: Some(0),
        streamable_endpoints: vec!["/v1/chat/completions".to_string(), "/v1/responses".to_string()],
        ..Default::default()
    };
    config.flex_live_streaming = FlexLiveStreamingConfig {
        enabled: live_streaming_enabled,
        chunk_relay: Some(ChunkRelayConfig {
            redis_url: test_redis_url(),
            ..Default::default()
        }),
        // Much slower than the mock's ~450ms send time, so a fast test
        // proves the relay delivered it, not a lucky poll tick.
        poll_fallback_interval_ms: 5000,
    };

    let (server, bg_services) = create_test_app_with_real_loopback(pool.clone(), config).await;

    let admin_user = create_test_admin_user(pool, Role::PlatformManager).await;
    let admin_headers = add_auth_headers(&admin_user);
    let regular_user = create_test_user(pool, Role::StandardUser).await;
    let regular_headers = add_auth_headers(&regular_user);

    let group_response = server
        .post("/admin/api/v1/groups")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "name": format!("flex-live-streaming-{}", uuid::Uuid::new_v4()),
            "description": "Flex live-streaming E2E"
        }))
        .await;
    assert_eq!(group_response.status_code(), 201, "failed to create group");
    let group: GroupResponse = group_response.json();

    let add_user_response = server
        .post(&format!("/admin/api/v1/groups/{}/users/{}", group.id, regular_user.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;
    assert_eq!(add_user_response.status_code(), 204, "failed to add user to group");

    let credits_response = server
        .post("/admin/api/v1/transactions")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "user_id": regular_user.id,
            "transaction_type": "admin_grant",
            "amount": 1000,
            "source_id": admin_user.id,
            "description": "Flex live-streaming E2E credits"
        }))
        .await;
    assert_eq!(credits_response.status_code(), 201, "failed to grant credits");

    let endpoint_response = server
        .post("/admin/api/v1/endpoints")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "name": format!("flex-live-streaming-mock-{}", uuid::Uuid::new_v4()),
            // onwards appends "/chat/completions" directly to this URL.
            "url": format!("{mock_endpoint_url}/v1"),
            "description": "Mock OpenAI-compatible endpoint for flex live-streaming E2E"
        }))
        .await;
    assert_eq!(endpoint_response.status_code(), 201, "failed to create endpoint");
    let endpoint: InferenceEndpointResponse = endpoint_response.json();

    let deployment_response = server
        .post("/admin/api/v1/models")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": "flex-live-model",
            "alias": "flex-live-alias",
            "description": "Flex live-streaming test deployment",
            "hosted_on": endpoint.id,
            "tariffs": [{
                "name": "batch",
                "input_price_per_token": "0.001",
                "output_price_per_token": "0.003",
                "api_key_purpose": "realtime"
            }]
        }))
        .await;
    assert_eq!(deployment_response.status_code(), 200, "failed to create deployment");
    let deployment: DeployedModelResponse = deployment_response.json();

    let add_deployment_response = server
        .post(&format!("/admin/api/v1/groups/{}/models/{}", group.id, deployment.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;
    assert_eq!(add_deployment_response.status_code(), 204, "failed to add deployment to group");

    let api_key_response = server
        .post(&format!("/admin/api/v1/users/{}/api-keys", regular_user.id))
        .add_header(&regular_headers[0].0, &regular_headers[0].1)
        .add_header(&regular_headers[1].0, &regular_headers[1].1)
        .json(&serde_json::json!({
            "name": "flex-live-streaming-key",
            "description": "API key for flex live-streaming E2E",
            "purpose": "realtime"
        }))
        .await;
    assert_eq!(api_key_response.status_code(), 201, "failed to create API key");
    let api_key: ApiKeyResponse = api_key_response.json();

    bg_services.sync_onwards_config(pool).await.expect("sync onwards config");

    for attempt in 0..50 {
        let resp = server
            .get("/ai/v1/models")
            .add_header("authorization", format!("Bearer {}", api_key.key))
            .await;
        if resp.status_code() == 200 {
            let body: serde_json::Value = resp.json();
            let has_model = body["data"]
                .as_array()
                .is_some_and(|models| models.iter().any(|m| m["id"] == "flex-live-alias"));
            if has_model {
                break;
            }
        }
        assert!(attempt < 49, "model never became routable");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    FlexFixture {
        server,
        bg_services,
        api_key: api_key.key,
    }
}

async fn send_flex_streaming_request(fixture: &FlexFixture) -> (axum_test::TestResponse, Duration) {
    let started = std::time::Instant::now();
    let response = fixture
        .server
        .post("/ai/v1/chat/completions")
        .add_header("authorization", format!("Bearer {}", fixture.api_key))
        .json(&serde_json::json!({
            "model": "flex-live-alias",
            "messages": [{"role": "user", "content": "Hello from flex live-streaming E2E"}],
            "stream": true,
            "service_tier": "flex"
        }))
        .await;
    (response, started.elapsed())
}

#[sqlx::test]
#[test_log::test]
async fn flex_live_streaming_delivers_real_incremental_deltas(pool: PgPool) {
    let fixture = setup_flex_fixture(&pool, true).await;

    let (response, elapsed) = send_flex_streaming_request(&fixture).await;

    assert_eq!(response.status_code().as_u16(), 200);
    let content_type = response
        .headers()
        .get("content-type")
        .map_or("", |v| v.to_str().unwrap_or_default())
        .to_string();
    assert!(
        content_type.starts_with("text/event-stream"),
        "expected an SSE response, got {content_type:?}"
    );

    let deltas = parse_sse_content_deltas(&response.text());
    assert_eq!(
        deltas,
        vec!["Hello".to_string(), " world".to_string()],
        "expected the upstream's own per-chunk deltas, not a poll-fallback mega-delta"
    );

    assert!(
        elapsed < Duration::from_millis(3000),
        "expected the relay, not the 5000ms poll fallback, to deliver the answer; took {elapsed:?}"
    );

    fixture.bg_services.shutdown().await;
}

/// Control: same mock and setup, live streaming off — proves the assertion
/// above is discriminating, not coincidental.
#[sqlx::test]
#[test_log::test]
async fn flex_streaming_without_live_relay_still_replays_the_mega_delta(pool: PgPool) {
    let fixture = setup_flex_fixture(&pool, false).await;

    let (response, _elapsed) = send_flex_streaming_request(&fixture).await;

    assert_eq!(response.status_code().as_u16(), 200);
    let deltas = parse_sse_content_deltas(&response.text());
    assert_eq!(deltas, vec!["Hello world".to_string()], "flag off must behave exactly as before");

    fixture.bg_services.shutdown().await;
}

async fn send_flex_responses_streaming_request(fixture: &FlexFixture) -> axum_test::TestResponse {
    fixture
        .server
        .post("/ai/v1/responses")
        .add_header("authorization", format!("Bearer {}", fixture.api_key))
        .json(&serde_json::json!({
            "model": "flex-live-alias",
            "input": "Hello from flex live-streaming E2E",
            "stream": true,
            "service_tier": "flex"
        }))
        .await
}

// Keyed off the "type" field, not the SSE "event:" line — axum emits
// fields in call order here (data before event), not a fixed order.
fn parse_response_output_text_deltas(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .filter(|v| v["type"] == "response.output_text.delta")
        .filter_map(|v| v["delta"].as_str().map(str::to_string))
        .collect()
}

/// Same proof as the chat-completions test, on the Responses surface: real
/// per-chunk `response.output_text.delta` events, not one delta replaying
/// the whole answer.
#[sqlx::test]
#[test_log::test]
async fn flex_live_streaming_delivers_real_incremental_response_deltas(pool: PgPool) {
    let fixture = setup_flex_fixture(&pool, true).await;

    let response = send_flex_responses_streaming_request(&fixture).await;

    assert_eq!(response.status_code().as_u16(), 200);
    let deltas = parse_response_output_text_deltas(&response.text());
    assert_eq!(
        deltas,
        vec!["Hello".to_string(), " world".to_string()],
        "expected the upstream's own per-chunk deltas on the Responses surface too"
    );

    fixture.bg_services.shutdown().await;
}

/// Control for the Responses surface: same mock, live streaming off.
#[sqlx::test]
#[test_log::test]
async fn flex_responses_streaming_without_live_relay_still_replays_the_mega_delta(pool: PgPool) {
    let fixture = setup_flex_fixture(&pool, false).await;

    let response = send_flex_responses_streaming_request(&fixture).await;

    assert_eq!(response.status_code().as_u16(), 200);
    let deltas = parse_response_output_text_deltas(&response.text());
    assert_eq!(deltas, vec!["Hello world".to_string()], "flag off must behave exactly as before");

    fixture.bg_services.shutdown().await;
}
