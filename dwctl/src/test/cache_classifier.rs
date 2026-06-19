//! Tests for the cached-input-classification wiring (`onwards.cache_classifier_enabled`).
//!
//! Mirrors the `strict_mode` tests: they exercise the dwctl→onwards seam end to end
//! through a real proxied chat completion against a mock upstream.
//!
//! - flag ON  → the [`onwards::NoopCacheClassifier`] is wired, so onwards forks the
//!   request and injects the `cache_*` usage fields into the response. The no-op
//!   returns all-zero stats, so the injected fields are present and zero.
//! - flag OFF (the default) → onwards is dormant: no fork, no injection, and the
//!   upstream `usage` object is forwarded byte-for-byte (no `cache_*` fields).

use crate::api::models::users::Role;
use crate::test::utils::{add_auth_headers, create_test_admin_user, create_test_config, create_test_user};
use sqlx::PgPool;

/// Build an app with the given `cache_classifier_enabled` flag, wire an
/// endpoint→model routed at a mock upstream that returns a fixed chat-completion
/// `usage`, send one proxied request, and return the `usage` object from the
/// proxied response.
async fn proxied_usage_with_flag(pool: &PgPool, cache_classifier_enabled: bool) -> serde_json::Value {
    // Mock upstream returns a normal OpenAI chat completion. Its usage has NO
    // cache_* fields, so anything cache-shaped in the proxied response was added
    // by onwards.
    let mock_server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-cache-test",
            "object": "chat.completion",
            "created": 1677652288,
            "model": "cache-test-model",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hi" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12 }
        })))
        .mount(&mock_server)
        .await;

    let mut config = create_test_config();
    config.onwards.cache_classifier_enabled = cache_classifier_enabled;
    config.background_services.onwards_sync.enabled = true;

    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("Failed to create application");
    let (server, bg_services) = app.into_test_server();

    let admin_user = create_test_admin_user(pool, Role::PlatformManager).await;
    let admin_headers = add_auth_headers(&admin_user);
    let user = create_test_user(pool, Role::StandardUser).await;
    let user_headers = add_auth_headers(&user);

    // Group + membership so the user can reach the model.
    let group: serde_json::Value = server
        .post("/admin/api/v1/groups")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({ "name": "cache-test-group", "description": "cache wiring test" }))
        .await
        .json();
    let group_id = group["id"].as_str().expect("group id");

    server
        .post(&format!("/admin/api/v1/groups/{}/users/{}", group_id, user.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;

    // Credits so the proxied request isn't rejected for balance.
    server
        .post("/admin/api/v1/transactions")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "user_id": user.id,
            "transaction_type": "admin_grant",
            "amount": 1000,
            "source_id": admin_user.id,
            "description": "Credits for cache wiring test"
        }))
        .await;

    // Endpoint pointed at the mock upstream.
    let endpoint: serde_json::Value = server
        .post("/admin/api/v1/endpoints")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({ "name": "Cache Test Endpoint", "url": format!("{}/v1", mock_server.uri()) }))
        .await
        .json();
    let endpoint_id = endpoint["id"].as_str().expect("endpoint id");

    // Model (alias `cache-test`) with a realtime tariff.
    let model: serde_json::Value = server
        .post("/admin/api/v1/models")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": "cache-test-model",
            "alias": "cache-test",
            "hosted_on": endpoint_id,
            "tariffs": [{
                "name": "default",
                "input_price_per_token": "0.001",
                "output_price_per_token": "0.003",
                "api_key_purpose": "realtime"
            }]
        }))
        .await
        .json();
    let model_id = model["id"].as_str().expect("model id");

    server
        .post(&format!("/admin/api/v1/groups/{}/models/{}", group_id, model_id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;

    // Realtime API key for the proxied call.
    let key: serde_json::Value = server
        .post(&format!("/admin/api/v1/users/{}/api-keys", user.id))
        .add_header(&user_headers[0].0, &user_headers[0].1)
        .add_header(&user_headers[1].0, &user_headers[1].1)
        .json(&serde_json::json!({ "name": "Realtime Key", "purpose": "realtime" }))
        .await
        .json();
    let api_key = key["key"].as_str().expect("api key secret").to_string();

    // Push config to onwards, then poll until the model is routable (avoid sleeps).
    bg_services.sync_onwards_config(pool).await.expect("Failed to sync onwards config");

    let chat_body = serde_json::json!({
        "model": "cache-test",
        "messages": [{ "role": "user", "content": "test" }]
    });

    for i in 0..50 {
        let resp = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", api_key))
            .json(&chat_body)
            .await;
        let status = resp.status_code().as_u16();
        if status != 404 {
            assert_eq!(status, 200, "proxied request should succeed once synced");
            let body: serde_json::Value = resp.json();
            return body["usage"].clone();
        }
        assert!(i < 49, "model never became routable (last status {status})");
        tokio::task::yield_now().await;
    }
    unreachable!("polling loop returns or panics before exhausting iterations");
}

/// flag ON: onwards injects the (zeroed) cache_* usage fields while preserving the
/// upstream usage.
#[sqlx::test]
#[test_log::test]
async fn cache_classifier_enabled_injects_zeroed_cache_usage(pool: PgPool) {
    let usage = proxied_usage_with_flag(&pool, true).await;

    // Upstream usage is preserved untouched...
    assert_eq!(usage["prompt_tokens"], 10, "upstream prompt_tokens preserved");
    assert_eq!(usage["completion_tokens"], 2);

    // ...and onwards (driven by the wired NoopCacheClassifier) injects the cache
    // fields, all zero because the no-op returns all-zero stats.
    assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 0, "missing injected cached_tokens");
    assert_eq!(usage["cache_read_input_tokens"], 0, "missing injected cache_read_input_tokens");
    assert_eq!(usage["cache_creation_input_tokens"], 0);
    assert_eq!(usage["cache_creation"]["ephemeral_5m_input_tokens"], 0);
    assert_eq!(usage["cache_creation"]["ephemeral_1h_input_tokens"], 0);
    assert_eq!(usage["cache_creation"]["ephemeral_24h_input_tokens"], 0);
}

/// flag OFF (default): onwards is dormant — upstream usage is forwarded as-is with
/// no cache_* fields injected.
#[sqlx::test]
#[test_log::test]
async fn cache_classifier_disabled_leaves_usage_untouched(pool: PgPool) {
    let usage = proxied_usage_with_flag(&pool, false).await;

    // Upstream usage forwarded byte-for-byte.
    assert_eq!(usage["prompt_tokens"], 10);
    assert_eq!(usage["completion_tokens"], 2);

    // No injection on the dormant path.
    assert!(
        usage.get("cache_read_input_tokens").is_none(),
        "dormant path must not inject cache_read_input_tokens"
    );
    assert!(usage.get("cache_creation_input_tokens").is_none());
    assert!(usage.get("cache_creation").is_none());
    assert!(
        usage.get("prompt_tokens_details").is_none(),
        "dormant path must not add prompt_tokens_details"
    );
}
