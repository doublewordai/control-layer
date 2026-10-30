//! API keys authenticate by `secret_hash`, not by the plaintext `secret`
//! column (DW-1005).
//!
//! Each test replaces a key's stored plaintext with a decoy while leaving its
//! hash alone, then shows the real key still authenticates on the AI proxy
//! (dwctl middleware and onwards) and admin API paths, and the decoy does not.

use crate::api::models::users::Role;
use crate::crypto::hash_api_key;
use crate::test::utils::{add_auth_headers, create_test_admin_user, create_test_config, create_test_user};
use axum_test::TestServer;
use sqlx::PgPool;

/// Overwrite the plaintext `secret` of the key whose hash matches `secret`,
/// leaving `secret_hash` untouched. Returns the decoy now stored as `secret`.
async fn replace_stored_plaintext(pool: &PgPool, secret: &str) -> String {
    let decoy = format!("sk-decoy-{}", uuid::Uuid::new_v4().simple());
    let updated = sqlx::query("UPDATE api_keys SET secret = $1 WHERE secret_hash = $2")
        .bind(&decoy)
        .bind(&hash_api_key(secret)[..])
        .execute(pool)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(updated, 1, "the key's row is found by its hash");
    decoy
}

async fn create_key(server: &TestServer, admin: &crate::api::models::users::UserResponse, user_id: uuid::Uuid, purpose: &str) -> String {
    let h = add_auth_headers(admin);
    let key: serde_json::Value = server
        .post(&format!("/admin/api/v1/users/{user_id}/api-keys"))
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .json(&serde_json::json!({ "purpose": purpose, "name": format!("{purpose} hash auth key") }))
        .await
        .json();
    key["key"].as_str().unwrap().to_string()
}

#[dwctl_test_macros::test]
async fn ai_proxy_authenticates_by_hash(pool: PgPool) {
    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "gpt-4",
            "choices": [ { "index": 0, "message": { "role": "assistant", "content": "hi" }, "finish_reason": "stop" } ],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })))
        .mount(&mock)
        .await;

    let mut config = create_test_config();
    config.background_services.onwards_sync.enabled = true;
    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("app");
    let (server, bg) = app.into_test_server();

    let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
    let h = add_auth_headers(&admin);
    let endpoint: serde_json::Value = server
        .post("/admin/api/v1/endpoints")
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .json(&serde_json::json!({ "name": "mock", "url": mock.uri(), "auto_sync_models": false }))
        .await
        .json();
    let model: serde_json::Value = server
        .post("/admin/api/v1/models")
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .json(&serde_json::json!({ "type": "standard", "model_name": "gpt-4", "alias": "gpt-4", "hosted_on": endpoint["id"] }))
        .await
        .json();
    server
        .post(&format!(
            "/admin/api/v1/groups/00000000-0000-0000-0000-000000000000/models/{}",
            model["id"].as_str().unwrap()
        ))
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .await
        .assert_status_success();
    let user = create_test_user(&pool, Role::StandardUser).await;
    server
        .post("/admin/api/v1/transactions")
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .json(&serde_json::json!({ "user_id": user.id, "transaction_type": "admin_grant", "amount": 1000, "source_id": admin.id }))
        .await
        .assert_status_success();

    let api_key = create_key(&server, &admin, user.id, "realtime").await;
    let decoy = replace_stored_plaintext(&pool, &api_key).await;
    bg.sync_onwards_config(&pool).await.unwrap();
    bg.sync_key_policy(&pool).await.unwrap();

    let chat = |token: String| {
        server
            .post("/ai/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({ "model": "gpt-4", "messages": [ { "role": "user", "content": "hi" } ] }))
    };

    // The onwards cache is reloaded asynchronously: poll until the key works.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut response = chat(api_key.clone()).await;
    while response.status_code() != 200 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        response = chat(api_key.clone()).await;
    }
    assert_eq!(response.status_code(), 200, "real key: {}", response.text());
    assert_eq!(response.json::<serde_json::Value>()["choices"][0]["message"]["content"], "hi");

    // The dwctl-side lookups (model listing) also resolve the key by hash.
    let models = server
        .get("/ai/v1/models")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(models.status_code(), 200, "{}", models.text());

    // The plaintext now in the `secret` column is not a valid key.
    let refused = chat(decoy).await;
    assert!(
        refused.status_code().is_client_error(),
        "decoy plaintext must not authenticate: {}",
        refused.status_code()
    );
}

#[dwctl_test_macros::test]
async fn admin_api_authenticates_platform_key_by_hash(pool: PgPool) {
    let (server, _bg) = crate::test::utils::create_test_app(pool.clone(), false).await;
    let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
    let user = create_test_user(&pool, Role::StandardUser).await;
    let api_key = create_key(&server, &admin, user.id, "platform").await;
    let decoy = replace_stored_plaintext(&pool, &api_key).await;

    let me = server
        .get("/admin/api/v1/users/current")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(me.status_code(), 200, "{}", me.text());
    assert_eq!(me.json::<serde_json::Value>()["id"], user.id.to_string());

    let refused = server
        .get("/admin/api/v1/users/current")
        .add_header("Authorization", format!("Bearer {decoy}"))
        .await;
    assert_eq!(refused.status_code(), 401);
}
