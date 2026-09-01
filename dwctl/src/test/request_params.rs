//! End-to-end coverage for out-of-band functional request parameters (COR-614).
//!
//! The unit tests in [`crate::inference::params`] pin the parsing rules. These pin the two
//! things only a full stack can show: that a parameter we own never reaches the provider, and
//! that a request carrying none of them is forwarded exactly as before.

use axum_test::TestServer;
use serde_json::Value;
use sqlx::PgPool;
use wiremock::matchers::{method, path};

use super::{StreamingFixture, cleanup_fixture, setup_streaming_fixture_with_config, wait_for_model};

/// The upstream model id every fixture below deploys behind its alias.
const PROVIDER_MODEL: &str = "gpt-3.5-turbo";
/// The alias clients call.
const ALIAS: &str = "test-model";

/// A non-streaming chat completion, enough for the proxy path to succeed.
fn chat_response() -> Value {
    serde_json::json!({
        "id": "chatcmpl-123",
        "object": "chat.completion",
        "created": 1_677_652_288,
        "model": PROVIDER_MODEL,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "Hi"},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 9, "completion_tokens": 12, "total_tokens": 21},
    })
}

/// Mount a catch-all chat-completions responder on a fresh mock provider.
async fn mock_provider() -> wiremock::MockServer {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(chat_response()))
        .mount(&server)
        .await;
    server
}

/// Build the standard fixture with the model-name suffix channel enabled.
async fn fixture_with_suffix(pool: &PgPool, mock_uri: String) -> StreamingFixture {
    setup_streaming_fixture_with_config(pool, format!("{mock_uri}/v1"), PROVIDER_MODEL, ALIAS, |config| {
        config.request_params.model_suffix = true;
    })
    .await
}

/// The single request the provider received, as (query string, JSON body).
async fn only_upstream_request(mock: &wiremock::MockServer) -> (String, Value) {
    let requests = mock.received_requests().await.expect("mock server records requests");
    assert_eq!(requests.len(), 1, "expected exactly one upstream request, got {}", requests.len());
    let request = &requests[0];
    let query = request.url.query().unwrap_or_default().to_string();
    let body: Value = serde_json::from_slice(&request.body).expect("upstream body is JSON");
    (query, body)
}

/// POST a chat completion through the proxy as the fixture's API key.
async fn post_chat(server: &TestServer, api_key: &str, url: &str, body: Value) -> axum_test::TestResponse {
    server
        .post(url)
        .add_header("authorization", format!("Bearer {api_key}"))
        .json(&body)
        .await
}

#[sqlx::test]
#[test_log::test]
async fn query_param_service_tier_is_applied_and_never_reaches_the_provider(pool: PgPool) {
    let mock = mock_provider().await;
    let fixture = fixture_with_suffix(&pool, mock.uri()).await;

    let response = post_chat(
        &fixture.server,
        &fixture.api_key,
        "/ai/v1/chat/completions?serviceTier=priority",
        serde_json::json!({"model": ALIAS, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(response.status_code().as_u16(), 200);

    let (query, body) = only_upstream_request(&mock).await;
    // The single most important assertion: onwards forwards `path_and_query` verbatim, so a
    // param we own must have been stripped before it got there.
    assert!(!query.contains("serviceTier"), "serviceTier leaked to the provider in {query:?}");
    // ...and it was normalised into the body, exactly as if the client had sent the field.
    assert_eq!(body["service_tier"], "priority");

    cleanup_fixture(fixture).await;
}

#[sqlx::test]
#[test_log::test]
async fn cache_breakpoint_is_unrecognised_when_the_cache_layer_is_absent(pool: PgPool) {
    // `cache.enabled` is false by default, so nothing downstream would strip an injected
    // marker. The param must therefore pass through untouched rather than be honoured.
    let mock = mock_provider().await;
    let fixture = fixture_with_suffix(&pool, mock.uri()).await;

    let response = post_chat(
        &fixture.server,
        &fixture.api_key,
        "/ai/v1/chat/completions?cacheBreakpoint=lastUserMessage",
        serde_json::json!({"model": ALIAS, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(response.status_code().as_u16(), 200);

    let (_query, body) = only_upstream_request(&mock).await;
    assert!(
        body.get("cache_control").is_none(),
        "no marker should be injected without the cache layer"
    );

    cleanup_fixture(fixture).await;
}

#[sqlx::test]
#[test_log::test]
async fn model_suffix_is_stripped_before_the_provider_sees_it(pool: PgPool) {
    let mock = mock_provider().await;
    let fixture = fixture_with_suffix(&pool, mock.uri()).await;

    let response = post_chat(
        &fixture.server,
        &fixture.api_key,
        "/ai/v1/chat/completions",
        serde_json::json!({
            "model": format!("{ALIAS}-dw-priority"),
            "messages": [{"role": "user", "content": "hi"}],
        }),
    )
    .await;
    assert_eq!(response.status_code().as_u16(), 200);

    let (_query, body) = only_upstream_request(&mock).await;
    // onwards rewrites the alias to the provider's own model id; either way the suffix must
    // be long gone.
    assert_eq!(body["model"], PROVIDER_MODEL);
    assert!(!body["model"].as_str().unwrap_or_default().contains("priority"));
    assert_eq!(body["service_tier"], "priority");

    cleanup_fixture(fixture).await;
}

#[sqlx::test]
#[test_log::test]
async fn flex_suffix_enqueues_instead_of_proxying(pool: PgPool) {
    // `background: true` on /responses returns 202 immediately, so this exercises the queued
    // dispatch without the blocking flex poll.
    let mock = mock_provider().await;
    let fixture = fixture_with_suffix(&pool, mock.uri()).await;

    let response = post_chat(
        &fixture.server,
        &fixture.api_key,
        "/ai/v1/responses",
        serde_json::json!({
            "model": format!("{ALIAS}-dw-flex"),
            "input": "hi",
            "background": true,
        }),
    )
    .await;
    assert_eq!(
        response.status_code().as_u16(),
        202,
        "flex + background should be accepted for the daemon"
    );

    let body: Value = response.json();
    assert_eq!(body["service_tier"], "flex", "the suffix must have selected the flex tier");

    let requests = mock.received_requests().await.expect("mock server records requests");
    assert!(requests.is_empty(), "an enqueued request must not reach the provider inline");

    cleanup_fixture(fixture).await;
}

#[sqlx::test]
#[test_log::test]
async fn exact_alias_match_wins_over_suffix_stripping(pool: PgPool) {
    // The safety property: a deployment whose alias genuinely ends in `-dw-priority` must route
    // to itself, not be split into `test-model` + a tier.
    let collision_alias = format!("{ALIAS}-dw-priority");
    let mock = mock_provider().await;
    let fixture = fixture_with_suffix(&pool, mock.uri()).await;

    // Reuse the fixture's endpoint: creating another one runs model auto-discovery, which
    // 409s on aliases the first endpoint already claimed.
    let deployment_response = fixture
        .server
        .post("/admin/api/v1/models")
        .add_header(&fixture.admin_headers[0].0, &fixture.admin_headers[0].1)
        .add_header(&fixture.admin_headers[1].0, &fixture.admin_headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": "collision-provider-model",
            "alias": collision_alias,
            "hosted_on": fixture.endpoint_id,
            "tariffs": [{
                "name": "batch",
                "input_price_per_token": "0.001",
                "output_price_per_token": "0.003",
                "api_key_purpose": "realtime",
            }],
        }))
        .await;
    assert_eq!(deployment_response.status_code(), 200);
    let deployment: crate::api::models::deployments::DeployedModelResponse = deployment_response.json();

    let add_response = fixture
        .server
        .post(&format!("/admin/api/v1/groups/{}/models/{}", fixture.group_id, deployment.id))
        .add_header(&fixture.admin_headers[0].0, &fixture.admin_headers[0].1)
        .add_header(&fixture.admin_headers[1].0, &fixture.admin_headers[1].1)
        .await;
    assert_eq!(add_response.status_code(), 204);

    fixture.bg_services.sync_onwards_config(&pool).await.expect("sync onwards config");
    wait_for_model(&fixture.server, &fixture.api_key, &collision_alias).await;

    let response = post_chat(
        &fixture.server,
        &fixture.api_key,
        "/ai/v1/chat/completions",
        serde_json::json!({"model": collision_alias, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(response.status_code().as_u16(), 200);

    let (_query, body) = only_upstream_request(&mock).await;
    // Routed to the colliding deployment's own provider model, and no tier was inferred.
    assert_eq!(body["model"], "collision-provider-model");
    assert!(body.get("service_tier").is_none(), "an exact alias match must not infer a tier");

    cleanup_fixture(fixture).await;
}

#[sqlx::test]
#[test_log::test]
async fn unknown_model_with_a_suffix_looking_tail_is_404_not_400(pool: PgPool) {
    // A suffix isn't distinguishable from a typo, so "model not found" is the honest error.
    let mock = mock_provider().await;
    let fixture = fixture_with_suffix(&pool, mock.uri()).await;

    let response = post_chat(
        &fixture.server,
        &fixture.api_key,
        "/ai/v1/chat/completions",
        serde_json::json!({"model": "no-such-model-dw-priority", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(response.status_code().as_u16(), 404);

    let requests = mock.received_requests().await.expect("mock server records requests");
    assert!(requests.is_empty());

    cleanup_fixture(fixture).await;
}

#[sqlx::test]
#[test_log::test]
async fn unknown_suffix_on_a_real_model_is_400(pool: PgPool) {
    // The base alias resolved, so the delimiter was deliberate — name the supported set rather
    // than returning the `model_not_found` the caller would otherwise have to decode.
    let mock = mock_provider().await;
    let fixture = fixture_with_suffix(&pool, mock.uri()).await;

    let response = post_chat(
        &fixture.server,
        &fixture.api_key,
        "/ai/v1/chat/completions",
        serde_json::json!({"model": format!("{ALIAS}-dw-turbo"), "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(response.status_code().as_u16(), 400);

    let body: Value = response.json();
    assert_eq!(body["error"]["code"], "invalid_request_param");
    assert_eq!(body["error"]["param"], "model");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("turbo"), "message should name the offending suffix: {message}");
    assert!(message.contains("flex"), "message should name the supported suffixes: {message}");

    let requests = mock.received_requests().await.expect("mock server records requests");
    assert!(requests.is_empty(), "a rejected request must never be forwarded");

    cleanup_fixture(fixture).await;
}

#[sqlx::test]
#[test_log::test]
async fn invalid_query_param_value_is_rejected_before_forwarding(pool: PgPool) {
    let mock = mock_provider().await;
    let fixture = fixture_with_suffix(&pool, mock.uri()).await;

    let response = post_chat(
        &fixture.server,
        &fixture.api_key,
        "/ai/v1/chat/completions?serviceTier=turbo",
        serde_json::json!({"model": ALIAS, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(response.status_code().as_u16(), 400, "a typo must not silently select another tier");

    let body: Value = response.json();
    assert_eq!(body["error"]["code"], "invalid_request_param");
    assert_eq!(body["error"]["param"], "serviceTier");

    let requests = mock.received_requests().await.expect("mock server records requests");
    assert!(requests.is_empty(), "a rejected request must never be forwarded");

    cleanup_fixture(fixture).await;
}

#[sqlx::test]
#[test_log::test]
async fn a_request_carrying_no_parameters_is_forwarded_unchanged(pool: PgPool) {
    // Guards the `changed`-gated re-serialise: with the feature enabled but nothing to apply,
    // the body must gain nothing.
    let mock = mock_provider().await;
    let fixture = fixture_with_suffix(&pool, mock.uri()).await;

    let response = post_chat(
        &fixture.server,
        &fixture.api_key,
        "/ai/v1/chat/completions",
        serde_json::json!({"model": ALIAS, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(response.status_code().as_u16(), 200);

    let (query, body) = only_upstream_request(&mock).await;
    assert!(query.is_empty(), "no query should have been added, got {query:?}");
    assert!(body.get("service_tier").is_none(), "no tier should be invented");
    assert!(body.get("cache_control").is_none(), "no marker should be invented");
    assert_eq!(body["messages"][0]["content"], "hi");

    cleanup_fixture(fixture).await;
}

#[sqlx::test]
#[test_log::test]
async fn models_listing_is_unchanged_by_the_feature(pool: PgPool) {
    // Expanded discovery is deliberately out of scope: a suffixed model works when typed, but
    // the default listing must not grow a variant per model.
    let mock = mock_provider().await;
    let fixture = fixture_with_suffix(&pool, mock.uri()).await;

    let response = fixture
        .server
        .get("/ai/v1/models")
        .add_header("authorization", format!("Bearer {}", fixture.api_key))
        .await;
    assert_eq!(response.status_code().as_u16(), 200);

    let body: Value = response.json();
    let ids: Vec<&str> = body["data"]
        .as_array()
        .expect("data array")
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert!(ids.contains(&ALIAS));
    assert!(
        !ids.iter().any(|id| id.ends_with("-dw-flex") || id.ends_with("-dw-priority")),
        "listing grew suffix variants: {ids:?}"
    );

    cleanup_fixture(fixture).await;
}

#[sqlx::test]
#[test_log::test]
async fn suffix_resolves_the_model_on_the_anthropic_messages_surface(pool: PgPool) {
    // `model` is top-level in every protocol, so the parse works before translation flattens
    // the request to chat completions.
    //
    // Only the model resolution is asserted here. The Anthropic translator deliberately
    // forwards `service_tier` only when it is `flex` (see `translation::anthropic::request`),
    // because Anthropic's own tier vocabulary differs — so a `-dw-priority` suffix correctly
    // routes as realtime in dwctl and is correctly absent from the upstream body. The tier
    // still reaches dwctl's own dispatch, which reads it before translation runs.
    let mock = mock_provider().await;
    let fixture = fixture_with_suffix(&pool, mock.uri()).await;

    let response = fixture
        .server
        .post("/ai/v1/messages")
        .add_header("authorization", format!("Bearer {}", fixture.api_key))
        .add_header("anthropic-version", "2023-06-01")
        .json(&serde_json::json!({
            "model": format!("{ALIAS}-dw-priority"),
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .await;
    assert_eq!(response.status_code().as_u16(), 200);

    let (_query, body) = only_upstream_request(&mock).await;
    // The suffix was stripped, so the alias resolved and onwards rewrote it to the provider id.
    assert_eq!(body["model"], PROVIDER_MODEL);

    cleanup_fixture(fixture).await;
}

#[sqlx::test]
#[test_log::test]
async fn suffix_is_inert_when_the_channel_is_disabled(pool: PgPool) {
    // The shipped default. Model resolution is untouched, so a suffixed name is simply an
    // unknown model.
    let mock = mock_provider().await;
    let fixture = setup_streaming_fixture_with_config(&pool, format!("{}/v1", mock.uri()), PROVIDER_MODEL, ALIAS, |_| {}).await;

    let response = post_chat(
        &fixture.server,
        &fixture.api_key,
        "/ai/v1/chat/completions",
        serde_json::json!({"model": format!("{ALIAS}-dw-priority"), "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(response.status_code().as_u16(), 404);

    cleanup_fixture(fixture).await;
}

#[sqlx::test]
#[test_log::test]
async fn suffix_and_query_param_work_under_strict_mode(pool: PgPool) {
    // Strict mode takes a different route through onwards — typed handlers, and the inbound
    // URI (with its query) is discarded rather than forwarded. The parse happens upstream of
    // all of that, in dwctl, so both channels must behave identically here.
    let mock = mock_provider().await;
    let fixture = setup_streaming_fixture_with_config(&pool, format!("{}/v1", mock.uri()), PROVIDER_MODEL, ALIAS, |config| {
        config.request_params.model_suffix = true;
        config.onwards.strict_mode = true;
    })
    .await;

    let response = post_chat(
        &fixture.server,
        &fixture.api_key,
        "/ai/v1/chat/completions?serviceTier=default",
        serde_json::json!({
            "model": format!("{ALIAS}-dw-priority"),
            "messages": [{"role": "user", "content": "hi"}],
        }),
    )
    .await;
    assert_eq!(response.status_code().as_u16(), 200);

    let (query, body) = only_upstream_request(&mock).await;
    assert!(!query.contains("serviceTier"), "serviceTier leaked to the provider in {query:?}");
    assert_eq!(body["model"], PROVIDER_MODEL);
    // The suffix beats the query param, under strict mode as everywhere else.
    assert_eq!(body["service_tier"], "priority");

    cleanup_fixture(fixture).await;
}
