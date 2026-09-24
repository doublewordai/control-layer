//! Organization-level disabled modalities, end to end.
//!
//! An owner switches a product surface off for the workspace; the endpoints
//! then refuse every key the organization owns. Covers the owner-only API
//! gate, the batch entry points (files, batches) in org context, and the
//! realtime inference endpoints via the per-key policy cache.

use crate::api::models::api_keys::ApiKeyCreate;
use crate::api::models::users::Role;
use crate::db::handlers::repository::Repository;
use crate::db::handlers::{Organizations, api_keys::ApiKeys};
use crate::db::models::api_keys::{ApiKeyCreateDBRequest, ApiKeyPurpose};
use crate::modalities::Modality;
use crate::test::utils::{add_auth_headers, create_test_app, create_test_org, create_test_user, create_test_user_with_roles};
use crate::types::UserId;
use serde_json::json;
use sqlx::PgPool;

/// An org-scoped API key: owned by the organization, created by a member.
async fn create_org_api_key(pool: &PgPool, org_id: UserId, member_id: UserId) -> String {
    let mut conn = pool.acquire().await.unwrap();
    ApiKeys::new(&mut conn)
        .create(&ApiKeyCreateDBRequest::new(
            org_id,
            member_id,
            ApiKeyCreate {
                name: "org key".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Realtime,
                requests_per_second: None,
                burst_size: None,
                member_id: None,
                spend_limit: None,
                spend_limit_interval: None,
            },
        ))
        .await
        .unwrap()
        .secret
}

async fn set_disabled(pool: &PgPool, org_id: UserId, disabled: &[Modality]) {
    let mut conn = pool.acquire().await.unwrap();
    assert!(
        Organizations::new(&mut conn)
            .set_disabled_modalities(org_id, disabled)
            .await
            .unwrap()
    );
}

fn jsonl_upload() -> axum_test::multipart::MultipartForm {
    let jsonl = r#"{"custom_id":"r1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}}
"#;
    axum_test::multipart::MultipartForm::new()
        .add_text("purpose", "batch")
        .add_part("file", axum_test::multipart::Part::bytes(jsonl.as_bytes()).file_name("in.jsonl"))
}

#[sqlx::test]
#[test_log::test]
async fn owner_sets_disabled_modalities_and_the_org_reports_them(pool: PgPool) {
    let (server, _bg) = create_test_app(pool.clone(), false).await;
    let owner = create_test_user(&pool, Role::StandardUser).await;
    let h = add_auth_headers(&owner);

    let resp = server
        .post("/admin/api/v1/organizations")
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .json(&json!({ "name": "modality-org", "email": "contact@example.com" }))
        .await;
    resp.assert_status(axum::http::StatusCode::CREATED);
    let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

    // Nothing disabled by default, and the field is present so clients can
    // render the settings without a second call.
    let resp = server
        .get(&format!("/admin/api/v1/organizations/{org_id}"))
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .await;
    resp.assert_status(axum::http::StatusCode::OK);
    assert_eq!(resp.json::<serde_json::Value>()["disabled_modalities"], json!([]));

    let resp = server
        .patch(&format!("/admin/api/v1/organizations/{org_id}"))
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .json(&json!({ "disabled_modalities": ["batch", "realtime"] }))
        .await;
    resp.assert_status(axum::http::StatusCode::OK);
    // Reported in canonical order, whatever order was sent.
    assert_eq!(
        resp.json::<serde_json::Value>()["disabled_modalities"],
        json!(["realtime", "batch"])
    );

    let resp = server
        .get(&format!("/admin/api/v1/organizations/{org_id}"))
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .await;
    assert_eq!(
        resp.json::<serde_json::Value>()["disabled_modalities"],
        json!(["realtime", "batch"])
    );

    // A PATCH that leaves the field out leaves the set alone.
    let resp = server
        .patch(&format!("/admin/api/v1/organizations/{org_id}"))
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .json(&json!({ "display_name": "Renamed" }))
        .await;
    resp.assert_status(axum::http::StatusCode::OK);
    assert_eq!(
        resp.json::<serde_json::Value>()["disabled_modalities"],
        json!(["realtime", "batch"])
    );

    // An empty list re-enables everything.
    let resp = server
        .patch(&format!("/admin/api/v1/organizations/{org_id}"))
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .json(&json!({ "disabled_modalities": [] }))
        .await;
    resp.assert_status(axum::http::StatusCode::OK);
    assert_eq!(resp.json::<serde_json::Value>()["disabled_modalities"], json!([]));

    // Unknown surfaces are refused at the API, not stored.
    let resp = server
        .patch(&format!("/admin/api/v1/organizations/{org_id}"))
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .json(&json!({ "disabled_modalities": ["playground"] }))
        .await;
    assert!(resp.status_code().is_client_error(), "got {}", resp.status_code());
}

#[sqlx::test]
#[test_log::test]
async fn only_owners_change_disabled_modalities(pool: PgPool) {
    let (server, _bg) = create_test_app(pool.clone(), false).await;
    let owner = create_test_user(&pool, Role::StandardUser).await;
    let admin = create_test_user(&pool, Role::StandardUser).await;
    let member = create_test_user(&pool, Role::StandardUser).await;
    let owner_h = add_auth_headers(&owner);

    let resp = server
        .post("/admin/api/v1/organizations")
        .add_header(&owner_h[0].0, &owner_h[0].1)
        .add_header(&owner_h[1].0, &owner_h[1].1)
        .json(&json!({ "name": "gated-org", "email": "contact@example.com" }))
        .await;
    resp.assert_status(axum::http::StatusCode::CREATED);
    let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();
    for (user, role) in [(&admin, "admin"), (&member, "member")] {
        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/members"))
            .add_header(&owner_h[0].0, &owner_h[0].1)
            .add_header(&owner_h[1].0, &owner_h[1].1)
            .json(&json!({ "user_id": user.id, "role": role }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
    }

    for user in [&admin, &member] {
        let h = add_auth_headers(user);
        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&h[0].0, &h[0].1)
            .add_header(&h[1].0, &h[1].1)
            .json(&json!({ "disabled_modalities": ["batch"] }))
            .await;
        resp.assert_status(axum::http::StatusCode::FORBIDDEN);
    }

    // The refusals changed nothing.
    let resp = server
        .get(&format!("/admin/api/v1/organizations/{org_id}"))
        .add_header(&owner_h[0].0, &owner_h[0].1)
        .add_header(&owner_h[1].0, &owner_h[1].1)
        .await;
    assert_eq!(resp.json::<serde_json::Value>()["disabled_modalities"], json!([]));
}

#[sqlx::test]
#[test_log::test]
async fn batch_entry_points_refuse_an_org_with_batch_disabled(pool: PgPool) {
    let (server, _bg) = create_test_app(pool.clone(), false).await;
    let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
    let org = create_test_org(&pool, user.id).await;
    let h = add_auth_headers(&user);
    let org_cookie = format!("dw_active_org={}", org.id);

    set_disabled(&pool, org.id, &[Modality::Batch]).await;

    // In org context: files and batches are refused with the owner's message.
    let resp = server
        .post("/ai/v1/files")
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .add_header("cookie", &org_cookie)
        .multipart(jsonl_upload())
        .await;
    resp.assert_status(axum::http::StatusCode::FORBIDDEN);
    assert!(
        resp.text().contains("batch API is disabled for this organization"),
        "{}",
        resp.text()
    );

    let resp = server
        .post("/ai/v1/batches")
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .add_header("cookie", &org_cookie)
        .json(&json!({
            "input_file_id": uuid::Uuid::new_v4(),
            "endpoint": "/v1/chat/completions",
            "completion_window": "24h"
        }))
        .await;
    resp.assert_status(axum::http::StatusCode::FORBIDDEN);
    assert!(
        resp.text().contains("batch API is disabled for this organization"),
        "{}",
        resp.text()
    );

    // Listing what exists stays open: the switch is on creating new work.
    let resp = server
        .get("/ai/v1/batches")
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .add_header("cookie", &org_cookie)
        .await;
    resp.assert_status(axum::http::StatusCode::OK);

    // The same person's personal account is untouched by the org's switch.
    // (No model is configured in this test, so the upload is refused later,
    // at line validation, rather than by the modality gate.)
    let resp = server
        .post("/ai/v1/files")
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .multipart(jsonl_upload())
        .await;
    assert!(!resp.text().contains("disabled for this organization"), "{}", resp.text());
    assert!(resp.text().contains("has not been configured"), "{}", resp.text());

    // Re-enabling restores the org: the request gets past the gate again.
    set_disabled(&pool, org.id, &[]).await;
    let resp = server
        .post("/ai/v1/files")
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .add_header("cookie", &org_cookie)
        .multipart(jsonl_upload())
        .await;
    assert!(!resp.text().contains("disabled for this organization"), "{}", resp.text());
    assert!(resp.text().contains("has not been configured"), "{}", resp.text());
}

#[sqlx::test]
#[test_log::test]
async fn realtime_endpoints_refuse_org_keys_with_realtime_disabled(pool: PgPool) {
    let (server, bg) = create_test_app(pool.clone(), false).await;
    let user = create_test_user(&pool, Role::StandardUser).await;
    let org = create_test_org(&pool, user.id).await;
    let org_key = create_org_api_key(&pool, org.id, user.id).await;
    let personal_key = create_org_api_key(&pool, user.id, user.id).await;

    set_disabled(&pool, org.id, &[Modality::Realtime]).await;
    bg.sync_key_policy(&pool).await.unwrap();

    let chat = json!({ "model": "gpt-4", "messages": [{ "role": "user", "content": "hi" }] });
    let send = |key: &str, path: &str, body: serde_json::Value| {
        server
            .post(path)
            .add_header("Authorization", &format!("Bearer {key}"))
            .add_header("Content-Type", "application/json")
            .json(&body)
    };

    // Every service tier lands on these endpoints, so one switch covers all.
    for (path, body) in [
        ("/ai/v1/chat/completions", chat.clone()),
        (
            "/ai/v1/chat/completions",
            json!({ "model": "gpt-4", "service_tier": "flex", "messages": [{ "role": "user", "content": "hi" }] }),
        ),
        ("/ai/v1/responses", json!({ "model": "gpt-4", "input": "hi" })),
        ("/ai/v1/embeddings", json!({ "model": "embed", "input": "hi" })),
    ] {
        let resp = send(&org_key, path, body).await;
        resp.assert_status(axum::http::StatusCode::FORBIDDEN);
        let body = resp.json::<serde_json::Value>();
        assert_eq!(body["error"]["code"], "modality_disabled", "{path}: {body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Realtime inference is disabled"),
            "{path}: {body}"
        );
    }

    // The member's personal key is governed by the member's own account.
    let resp = send(&personal_key, "/ai/v1/chat/completions", chat.clone()).await;
    assert_ne!(resp.status_code(), axum::http::StatusCode::FORBIDDEN, "{}", resp.text());

    // Re-enabling takes effect once the policy map reloads (the column's
    // NOTIFY trigger drives that in production; here we refresh directly).
    set_disabled(&pool, org.id, &[]).await;
    let resp = send(&org_key, "/ai/v1/chat/completions", chat.clone()).await;
    assert_eq!(resp.status_code(), axum::http::StatusCode::FORBIDDEN, "stale map still refuses");
    bg.sync_key_policy(&pool).await.unwrap();
    let resp = send(&org_key, "/ai/v1/chat/completions", chat).await;
    assert!(!resp.text().contains("modality_disabled"), "{}", resp.text());
}
