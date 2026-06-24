//! Full-stack tests for the cached-input pricing wiring (`onwards.cache_classifier_enabled`).
//!
//! Exercises the dwctl-owned cache tower layer end to end through a real
//! proxied chat completion against a mock upstream, with the layer in its production
//! slot (inner to outlet, wrapping the embedded onwards router):
//!
//! - flag OFF (the default) → the cache layer is not in the stack: the upstream
//!   `usage` is forwarded byte-for-byte (no `cache_*` fields).
//! - flag ON, model NOT opted in → the layer runs but the per-model gate returns
//!   all-zero stats, so injection is skipped (`is_zero()` short-circuit) and the
//!   usage is still untouched. Flipping the flag does nothing until a model opts in.
//! - flag ON, model opted in, cacheable prompt → the classifier tokenizes the
//!   marked prefix via tokenizer-svc and the real `cache_creation_*` fields are
//!   injected into the proxied response usage.
//!
//! The deep classify/inject/read behaviour is unit-covered in `prompt_cache::layer`
//! and `prompt_cache::classifier`; these tests prove the wiring + stack placement.

use crate::api::models::users::Role;
use crate::test::utils::{add_auth_headers, create_test_admin_user, create_test_config, create_test_user};
use sqlx::PgPool;

/// Options for [`proxied_usage`].
struct ProxiedOpts {
    /// `onwards.cache_classifier_enabled` — whether the cache layer is in the stack.
    cache_classifier_enabled: bool,
    /// When set, `onwards.tokenizer_url` points here (a mock tokenizer-svc).
    tokenizer_url: Option<String>,
    /// When true, the `cache-test` model is opted into cache pricing after creation.
    opt_in_cache: bool,
    /// The chat-completions request body to proxy.
    body: serde_json::Value,
}

/// Build an app with the given options, wire an endpoint→model routed at a mock
/// upstream that returns a fixed chat-completion `usage`, send one proxied request,
/// and return the `usage` object from the proxied response.
async fn proxied_usage(pool: &PgPool, opts: ProxiedOpts) -> serde_json::Value {
    // Mock upstream returns a normal OpenAI chat completion. Its usage has NO
    // cache_* fields, so anything cache-shaped in the proxied response was added
    // by the dwctl cache layer.
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
            "usage": { "prompt_tokens": 2000, "completion_tokens": 2, "total_tokens": 2002 }
        })))
        .mount(&mock_server)
        .await;

    let mut config = create_test_config();
    config.onwards.cache_classifier_enabled = opts.cache_classifier_enabled;
    if let Some(url) = &opts.tokenizer_url {
        config.onwards.tokenizer_url = url.clone();
    }
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

    // Enable cache pricing by inserting a tariff row (presence = enabled), keyed by
    // alias — the routing/gate key. All tiers set; min_prefix 1024.
    if opts.opt_in_cache {
        sqlx::query!(
            r#"INSERT INTO model_cache_tariffs
                 (deployed_model_id, write_multiplier_5m, write_multiplier_1h, write_multiplier_24h, min_prefix_tokens)
               SELECT id, 1.25, 2.0, 2.5, 1024 FROM deployed_models WHERE alias = 'cache-test'"#,
        )
        .execute(pool)
        .await
        .expect("insert cache tariff");
    }

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

    // Poll until the proxied request succeeds. The onwards config sync runs in the
    // background and propagates several things independently — model routing, the API
    // key, the user's credits, the group→model link — so under CI load there's a window
    // where the model is routable (no longer 404) but authorisation hasn't landed yet
    // (transient 403). Treat BOTH 404 and 403 as "not synced yet" and keep polling; a
    // short sleep per miss gives the sync task real wall-clock (`yield_now` alone starves
    // it under load). Any other status — or a 403 that never clears — is a real failure.
    for i in 0..150 {
        let resp = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", api_key))
            .json(&opts.body)
            .await;
        let status = resp.status_code().as_u16();
        if status == 200 {
            let body: serde_json::Value = resp.json();
            return body["usage"].clone();
        }
        assert!(
            status == 404 || status == 403,
            "unexpected proxied status {status} (expected eventual 200)"
        );
        assert!(i < 149, "request never succeeded once synced (last status {status})");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    unreachable!("polling loop returns or panics before exhausting iterations");
}

/// A plain (non-cacheable) chat body — no `cache_control` markers.
fn plain_body() -> serde_json::Value {
    serde_json::json!({
        "model": "cache-test",
        "messages": [{ "role": "user", "content": "test" }]
    })
}

/// flag OFF (default): the cache layer isn't in the stack — upstream usage is
/// forwarded byte-for-byte with no cache_* fields injected.
#[sqlx::test]
#[test_log::test]
async fn cache_disabled_leaves_usage_untouched(pool: PgPool) {
    let usage = proxied_usage(
        &pool,
        ProxiedOpts {
            cache_classifier_enabled: false,
            tokenizer_url: None,
            opt_in_cache: false,
            body: plain_body(),
        },
    )
    .await;

    assert_eq!(usage["prompt_tokens"], 2000);
    assert_eq!(usage["completion_tokens"], 2);
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

/// flag ON but the model hasn't opted in: the layer runs, the per-model gate
/// returns all-zero stats, and injection is skipped — usage is still untouched.
#[sqlx::test]
#[test_log::test]
async fn cache_enabled_but_model_not_opted_in_leaves_usage_untouched(pool: PgPool) {
    let usage = proxied_usage(
        &pool,
        ProxiedOpts {
            cache_classifier_enabled: true,
            tokenizer_url: None,
            opt_in_cache: false,
            body: plain_body(),
        },
    )
    .await;

    assert_eq!(usage["prompt_tokens"], 2000, "upstream usage preserved");
    assert_eq!(usage["completion_tokens"], 2);
    assert!(
        usage.get("cache_read_input_tokens").is_none(),
        "no model opt-in → gated to zero → no injection"
    );
    assert!(usage.get("cache_creation_input_tokens").is_none());
}

/// flag ON, model opted in, but a PLAIN prompt (no markers): the model is "active", so
/// the response still carries a uniform, all-zero `cache_*` block — clients of a
/// cache-enabled model always see the same usage shape, and billing always has stats.
#[sqlx::test]
#[test_log::test]
async fn cache_enabled_opted_in_plain_prompt_injects_zeros(pool: PgPool) {
    let usage = proxied_usage(
        &pool,
        ProxiedOpts {
            cache_classifier_enabled: true,
            tokenizer_url: None, // no markers → tokenizer is never consulted
            opt_in_cache: true,
            body: plain_body(),
        },
    )
    .await;

    assert_eq!(usage["prompt_tokens"], 2000, "upstream total preserved");
    // Fields are PRESENT and zero (uniform interface), not absent.
    assert_eq!(usage["cache_read_input_tokens"], 0);
    assert_eq!(usage["cache_creation_input_tokens"], 0);
    assert_eq!(usage["cache_creation"]["ephemeral_1h_input_tokens"], 0);
    assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 0);
}

/// flag ON, model opted in, cacheable prompt: the classifier tokenizes the marked
/// prefix via the (mock) tokenizer-svc and the real `cache_creation_*` fields are
/// injected into the proxied response usage — through the full app stack.
#[sqlx::test]
#[test_log::test]
async fn cache_enabled_and_opted_in_injects_creation(pool: PgPool) {
    // Mock tokenizer-svc: the marked system prefix tokenizes to 1500 tokens
    // (above the 1024 min-prefix floor → it becomes a creation write).
    let tok = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/v1/models"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{ "alias": "cache-test", "hf_repo": "o/m", "tokenizer_version": "sha256:ct1" }]
        })))
        .mount(&tok)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/tokenize"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "virtual_model": "cache-test", "tokenizer_version": "sha256:ct1",
            "segment_counts": [1500], "cumulative": [1500], "total": 1500
        })))
        .mount(&tok)
        .await;

    let body = serde_json::json!({
        "model": "cache-test",
        "messages": [
            {"role":"system","content":[{"type":"text","text":"big static preamble","cache_control":{"type":"ephemeral","ttl":"1h"}}]},
            {"role":"user","content":"test"}
        ]
    });

    let usage = proxied_usage(
        &pool,
        ProxiedOpts {
            cache_classifier_enabled: true,
            tokenizer_url: Some(tok.uri()),
            opt_in_cache: true,
            body,
        },
    )
    .await;

    // Upstream total preserved; the marked prefix is billed as a 1h creation write,
    // nothing read (cold index).
    assert_eq!(usage["prompt_tokens"], 2000, "upstream prompt_tokens preserved");
    assert_eq!(usage["cache_read_input_tokens"], 0);
    assert_eq!(usage["cache_creation_input_tokens"], 1500, "marked prefix billed as creation");
    assert_eq!(usage["cache_creation"]["ephemeral_1h_input_tokens"], 1500);
    assert_eq!(usage["cache_creation"]["ephemeral_5m_input_tokens"], 0);
    assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 0);
}
