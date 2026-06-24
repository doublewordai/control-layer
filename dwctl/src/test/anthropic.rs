//! End-to-end tests for the Anthropic `/v1/messages` ingress.
//!
//! These exercise the FULL real stack against a wiremock upstream: the
//! `/messages` route (onwards' strict alias), the edge translation middleware
//! (request -> Chat Completions, response/stream -> Anthropic), onwards routing
//! and sanitization, and the DB-driven model/key config. Unlike the unit tests
//! in `inference::translation`, onwards is the real (locally patched) crate here.

use crate::api::models::users::Role;
use crate::test::utils::{add_auth_headers, create_test_admin_user, create_test_config, create_test_user};
use axum_test::TestServer;
use sqlx::PgPool;

/// Seed a routable `gpt-4` deployment pointing at the wiremock upstream plus an
/// inference API key, and return the key secret. Caller must then sync onwards.
async fn seed_model_and_key(server: &TestServer, pool: &PgPool, mock_uri: &str) -> String {
    let admin = create_test_admin_user(pool, Role::PlatformManager).await;
    let h = add_auth_headers(&admin);

    let endpoint: serde_json::Value = server
        .post("/admin/api/v1/endpoints")
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .json(&serde_json::json!({ "name": "mock", "url": mock_uri, "description": "anthropic e2e", "auto_sync_models": false }))
        .await
        .json();
    let endpoint_id = endpoint["id"].as_str().unwrap();

    let model: serde_json::Value = server
        .post("/admin/api/v1/models")
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .json(&serde_json::json!({ "type": "standard", "model_name": "gpt-4", "alias": "gpt-4", "hosted_on": endpoint_id }))
        .await
        .json();
    let deployment_id = model["id"].as_str().unwrap();

    // Public group (all-zeros UUID) makes the model accessible to all users.
    let group_id = "00000000-0000-0000-0000-000000000000";
    let assoc = server
        .post(&format!("/admin/api/v1/groups/{group_id}/models/{deployment_id}"))
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .await;
    assert!(
        assoc.status_code().is_success(),
        "model-group association failed: {} {}",
        assoc.status_code(),
        assoc.text()
    );

    let user = create_test_user(pool, Role::StandardUser).await;
    let grant = server
        .post("/admin/api/v1/transactions")
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .json(&serde_json::json!({ "user_id": user.id, "transaction_type": "admin_grant", "amount": 1000, "source_id": admin.id }))
        .await;
    assert!(
        grant.status_code().is_success(),
        "credits grant failed: {} {}",
        grant.status_code(),
        grant.text()
    );

    let key: serde_json::Value = server
        .post(&format!("/admin/api/v1/users/{}/api-keys", user.id))
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .json(&serde_json::json!({ "purpose": "realtime", "name": "anthropic e2e key" }))
        .await
        .json();
    key["key"].as_str().unwrap().to_string()
}

/// Poll `/ai/v1/models` until onwards has picked up `gpt-4`.
async fn wait_for_model(server: &TestServer, api_key: &str) {
    let start = std::time::Instant::now();
    let mut ok = false;
    while !ok && start.elapsed() < std::time::Duration::from_secs(3) {
        let r = server
            .get("/ai/v1/models")
            .add_header("Authorization", &format!("Bearer {api_key}"))
            .await;
        if r.status_code() == 200 {
            let m: serde_json::Value = r.json();
            if let Some(d) = m["data"].as_array() {
                ok = d.iter().any(|x| x["id"].as_str() == Some("gpt-4"));
            }
        }
        if !ok {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }
    assert!(ok, "onwards did not pick up gpt-4 within 3s");
}

#[sqlx::test]
async fn anthropic_messages_blocking_end_to_end(pool: PgPool) {
    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "gpt-4",
            "choices": [ { "index": 0, "message": { "role": "assistant", "content": "Hello from the gateway" }, "finish_reason": "stop" } ],
            "usage": { "prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14 }
        })))
        .mount(&mock)
        .await;

    let mut config = create_test_config();
    config.onwards.strict_mode = true;
    config.background_services.onwards_sync.enabled = true;
    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("app");
    let (server, bg) = app.into_test_server();

    let api_key = seed_model_and_key(&server, &pool, &mock.uri()).await;
    bg.sync_onwards_config(&pool).await.unwrap();
    wait_for_model(&server, &api_key).await;

    // Anthropic request: x-api-key (not Bearer) + anthropic-version + system + message.
    let resp = server
        .post("/ai/v1/messages")
        .add_header("x-api-key", &api_key)
        .add_header("anthropic-version", "2023-06-01")
        .json(&serde_json::json!({
            "model": "gpt-4",
            "max_tokens": 64,
            "system": "be terse",
            "messages": [ { "role": "user", "content": "hi" } ]
        }))
        .await;

    let status = resp.status_code();
    let text = resp.text();
    assert_eq!(status, 200, "got {status}: {text}");
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["type"], "message");
    assert_eq!(body["role"], "assistant");
    assert_eq!(body["content"][0]["type"], "text");
    assert_eq!(body["content"][0]["text"], "Hello from the gateway");
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["usage"]["input_tokens"], 10);
    assert_eq!(body["usage"]["output_tokens"], 4);
}

#[sqlx::test]
async fn anthropic_messages_streaming_end_to_end(pool: PgPool) {
    let sse = concat!(
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw(sse.as_bytes().to_vec(), "text/event-stream"))
        .mount(&mock)
        .await;

    let mut config = create_test_config();
    config.onwards.strict_mode = true;
    config.background_services.onwards_sync.enabled = true;
    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("app");
    let (server, bg) = app.into_test_server();

    let api_key = seed_model_and_key(&server, &pool, &mock.uri()).await;
    bg.sync_onwards_config(&pool).await.unwrap();
    wait_for_model(&server, &api_key).await;

    let resp = server
        .post("/ai/v1/messages")
        .add_header("x-api-key", &api_key)
        .add_header("anthropic-version", "2023-06-01")
        .json(&serde_json::json!({
            "model": "gpt-4",
            "max_tokens": 64,
            "stream": true,
            "messages": [ { "role": "user", "content": "hi" } ]
        }))
        .await;

    let status = resp.status_code();
    let text = resp.text();
    assert_eq!(status, 200, "got {status}: {text}");
    for ev in [
        "event: message_start",
        "event: content_block_start",
        "event: content_block_delta",
        "event: message_stop",
    ] {
        assert!(text.contains(ev), "missing {ev} in:\n{text}");
    }
    assert!(text.contains(r#""text":"Hello""#), "{text}");
    assert!(text.contains(r#""stop_reason":"end_turn""#), "{text}");
}
