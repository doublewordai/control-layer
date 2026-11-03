//! `api_keys.last_used` is recorded for every way a key authenticates (DW-979).
//!
//! Each request below uses its own key, so a key whose `last_used` is set
//! after the flush proves that path recorded it.

use crate::api::models::users::Role;
use crate::test::utils::{
    add_auth_headers, create_test_admin_user, create_test_api_key_for_user, create_test_config, create_test_user_with_roles,
};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

async fn last_used(pool: &PgPool, id: Uuid) -> Option<chrono::DateTime<chrono::Utc>> {
    sqlx::query_scalar("SELECT last_used FROM api_keys WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[dwctl_test_macros::test]
async fn every_authentication_path_records_last_used(pool: PgPool) {
    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
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
        .json(&json!({ "name": "mock", "url": mock.uri(), "auto_sync_models": false }))
        .await
        .json();
    let model: serde_json::Value = server
        .post("/admin/api/v1/models")
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .json(&json!({ "type": "standard", "model_name": "gpt-4", "alias": "gpt-4", "hosted_on": endpoint["id"] }))
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
    let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
    server
        .post("/admin/api/v1/transactions")
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .json(&json!({ "user_id": user.id, "transaction_type": "admin_grant", "amount": 1000, "source_id": admin.id }))
        .await
        .assert_status_success();

    let realtime = create_test_api_key_for_user(&pool, user.id).await;
    let flex = create_test_api_key_for_user(&pool, user.id).await;
    let files = create_test_api_key_for_user(&pool, user.id).await;
    let models = create_test_api_key_for_user(&pool, user.id).await;
    let unused = create_test_api_key_for_user(&pool, user.id).await;
    let platform = create_test_api_key_for_user(&pool, user.id).await;
    sqlx::query("UPDATE api_keys SET purpose = 'platform' WHERE id = $1")
        .bind(platform.id)
        .execute(&pool)
        .await
        .unwrap();
    bg.sync_onwards_config(&pool).await.unwrap();
    bg.sync_key_policy(&pool).await.unwrap();

    let bearer = |secret: &str| format!("Bearer {secret}");

    // Realtime, through onwards. The onwards cache reloads asynchronously.
    let chat = || {
        server
            .post("/ai/v1/chat/completions")
            .add_header("Authorization", bearer(&realtime.secret))
            .json(&json!({ "model": "gpt-4", "messages": [ { "role": "user", "content": "hi" } ] }))
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut response = chat().await;
    while response.status_code() != 200 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        response = chat().await;
    }
    assert_eq!(response.status_code(), 200, "{}", response.text());

    // Flex, accepted by the inference middleware without reaching onwards.
    server
        .post("/ai/v1/responses")
        .add_header("Authorization", bearer(&flex.secret))
        .json(&json!({ "model": "gpt-4", "input": "hi", "service_tier": "flex", "background": true }))
        .await
        .assert_status(axum::http::StatusCode::ACCEPTED);

    // Files, authenticated by the CurrentUser extractor.
    server
        .get("/ai/v1/files")
        .add_header("Authorization", bearer(&files.secret))
        .await
        .assert_status_ok();

    // Model listing, answered by dwctl.
    server
        .get("/ai/v1/models")
        .add_header("Authorization", bearer(&models.secret))
        .await
        .assert_status_ok();

    // Management API with a platform key.
    server
        .get("/admin/api/v1/users/current")
        .add_header("Authorization", bearer(&platform.secret))
        .await
        .assert_status_ok();

    for key in [realtime.id, flex.id, files.id, models.id, platform.id, unused.id] {
        assert_eq!(last_used(&pool, key).await, None, "nothing is written before the flush");
    }
    bg.flush_api_key_usage(&pool).await.unwrap();

    let before = chrono::Utc::now();
    for (name, key) in [
        ("realtime", realtime.id),
        ("flex", flex.id),
        ("files", files.id),
        ("models", models.id),
        ("platform", platform.id),
    ] {
        let used = last_used(&pool, key).await.unwrap_or_else(|| panic!("{name} key has no last_used"));
        assert!(used <= before && before - used < chrono::Duration::minutes(1), "{name}: {used}");
    }
    assert_eq!(last_used(&pool, unused.id).await, None);
}
