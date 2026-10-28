//! Authentication provenance and cross-account authorization regressions.
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::api::models::users::{AuthMethod, CurrentUser, Role};
use crate::test::utils::{add_auth_headers, create_test_admin_user, create_test_api_key_for_user, create_test_app, create_test_user};
use crate::types::{Operation, Resource};

#[test]
fn key_and_native_permission_ceiling_precedes_admin_and_roles() {
    let user: CurrentUser = serde_json::from_value(json!({
        "id": uuid::Uuid::new_v4(), "username": "operator", "email": "operator@example.com",
        "is_admin": true, "roles": ["StandardUser", "PlatformManager", "BillingManager", "RequestViewer", "BatchAPIUser"],
        "display_name": null, "avatar_url": null, "payment_provider_id": null,
        "organizations": [], "active_organization": null
    }))
    .unwrap();
    for method in [AuthMethod::ApiKey, AuthMethod::NativeSession] {
        // Deliberately leave admin/roles intact to exercise the permission ceiling itself.
        let mut actor = user.clone();
        actor.auth_method = method;
        for resource in [
            Resource::Users,
            Resource::Credits,
            Resource::Requests,
            Resource::Analytics,
            Resource::Models,
        ] {
            assert!(
                !super::permissions::has_permission(&actor, resource, Operation::ReadAll),
                "{method:?} {resource:?}"
            );
        }
        assert!(super::permissions::has_permission(&actor, Resource::Files, Operation::ReadOwn));
        assert!(super::permissions::has_permission(&actor, Resource::ApiKeys, Operation::CreateOwn));
        let resolved = actor.with_auth_method(method);
        assert!(!resolved.is_admin);
        assert!(!resolved.roles.contains(&Role::PlatformManager));
        assert!(resolved.roles.contains(&Role::BatchAPIUser));
    }
}

#[dwctl_test_macros::test]
async fn elevated_keys_are_ordinary_but_sso_keeps_credits_and_zdr(pool: PgPool) {
    let (app, _services) = create_test_app(pool.clone(), false).await;
    for admin in [false, true] {
        let actor = if admin {
            create_test_admin_user(&pool, Role::StandardUser).await
        } else {
            create_test_user(&pool, Role::PlatformManager).await
        };
        let customer = create_test_user(&pool, Role::BatchAPIUser).await;
        let key = create_test_api_key_for_user(&pool, actor.id).await;
        sqlx::query("UPDATE api_keys SET purpose = 'platform' WHERE id = $1")
            .bind(key.id)
            .execute(&pool)
            .await
            .unwrap();
        let bearer = format!("Bearer {}", key.secret);
        let headers = add_auth_headers(&actor);
        let grant = json!({"user_id": customer.id, "transaction_type": "admin_grant", "amount": "10",
            "source_id": actor.id.to_string(), "description": "Promotional contribution"});
        let target = format!("/admin/api/v1/users/{}", customer.id);

        app.post("/admin/api/v1/transactions")
            .add_header("Authorization", &bearer)
            .json(&grant)
            .await
            .assert_status_forbidden();
        app.patch(&target)
            .add_header("Authorization", &bearer)
            .json(&json!({"zero_data_retention": true}))
            .await
            .assert_status_forbidden();
        app.get("/admin/api/v1/users")
            .add_header("Authorization", &bearer)
            .await
            .assert_status_forbidden();
        let own = app.get("/admin/api/v1/users/current").add_header("Authorization", &bearer).await;
        own.assert_status_ok();
        assert_eq!(own.json::<Value>()["is_admin"], false);
        assert!(!own.json::<Value>()["roles"].as_array().unwrap().contains(&json!("PlatformManager")));
        app.post("/admin/api/v1/users/current/api-keys")
            .add_header("Authorization", &bearer)
            .json(&json!({"name": "own key", "purpose": "realtime"}))
            .await
            .assert_status(StatusCode::CREATED);

        // A valid key wins over forged/provided SSO headers; an invalid key cannot fall back.
        for authorization in [&bearer, "Bearer invalid", "bearer invalid", "Basic invalid"] {
            let response = app
                .post("/admin/api/v1/transactions")
                .add_header("Authorization", authorization)
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
                .json(&grant)
                .await;
            assert!(!response.status_code().is_success());
        }
        app.post("/admin/api/v1/transactions")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .json(&grant)
            .await
            .assert_status(StatusCode::CREATED);
        let zdr = app
            .patch(&target)
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .json(&json!({"zero_data_retention": true}))
            .await;
        zdr.assert_status_ok();
        assert_eq!(zdr.json::<Value>()["zero_data_retention"], true);
        for body in [
            json!({"zero_data_retention": false}),
            json!({"zero_data_retention": true, "roles": ["PlatformManager"]}),
            json!({"zero_data_retention": true, "auto_topup_amount": 50}),
        ] {
            app.patch(&target)
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
                .json(&body)
                .await
                .assert_status_forbidden();
        }
        let org = crate::test::utils::create_test_org(&pool, customer.id).await;
        let org_path = format!("/admin/api/v1/organizations/{}", org.id);
        app.patch(&org_path)
            .add_header("Authorization", &bearer)
            .json(&json!({"zero_data_retention": true}))
            .await
            .assert_status_forbidden();
        app.patch(&org_path)
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .json(&json!({"zero_data_retention": true}))
            .await
            .assert_status_ok();
        for body in [
            json!({"zero_data_retention": false}),
            json!({"zero_data_retention": true, "auto_join_enabled": true}),
        ] {
            app.patch(&org_path)
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
                .json(&body)
                .await
                .assert_status_forbidden();
        }
        app.post(&format!("{org_path}/members"))
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .json(&json!({"user_id": actor.id, "role": "owner"}))
            .await
            .assert_status_forbidden();

        // Even trusted SSO must not mint customer credentials or grant itself roles.
        app.post(&format!("{target}/api-keys"))
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .json(&json!({"name": "forbidden", "purpose": "realtime"}))
            .await
            .assert_status_forbidden();
        app.patch(&format!("/admin/api/v1/users/{}", actor.id))
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .json(&json!({"roles": ["RequestViewer"]}))
            .await
            .assert_status_forbidden();
    }
}

#[dwctl_test_macros::test]
async fn customer_content_requires_ownership_for_sso_and_keys(pool: PgPool) {
    use crate::test::utils::{create_test_org, insert_fusillade_template};
    let (app, _services) = create_test_app(pool.clone(), false).await;
    for admin in [false, true] {
        let actor = crate::test::utils::create_test_user_on_domain(
            &pool,
            if admin { Role::StandardUser } else { Role::PlatformManager },
            &format!("{}.example.test", uuid::Uuid::new_v4()),
        )
        .await;
        sqlx::query("UPDATE users SET is_admin = $2 WHERE id = $1")
            .bind(actor.id)
            .bind(admin)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO user_roles (user_id, role) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(actor.id)
            .bind(Role::BatchAPIUser)
            .execute(&pool)
            .await
            .unwrap();
        let customer = create_test_user(&pool, Role::BatchAPIUser).await;
        let org = create_test_org(&pool, actor.id).await;
        let headers = add_auth_headers(&actor);
        let key = create_test_api_key_for_user(&pool, actor.id).await;
        let platform_key = create_test_api_key_for_user(&pool, actor.id).await;
        sqlx::query("UPDATE api_keys SET purpose = 'platform' WHERE id = $1")
            .bind(platform_key.id)
            .execute(&pool)
            .await
            .unwrap();
        let mut fixtures = vec![];
        for owner in [actor.id, customer.id, org.id] {
            let file = uuid::Uuid::new_v4();
            let batch = uuid::Uuid::new_v4();
            let response = uuid::Uuid::new_v4();
            sqlx::query("INSERT INTO fusillade.files (id, name, purpose, uploaded_by) VALUES ($1, 'private.jsonl', 'batch', $2)")
                .bind(file)
                .bind(owner.to_string())
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO fusillade.batches (id, file_id, created_by, expires_at) VALUES ($1, $2, $3, NOW() + interval '24 hours')",
            )
            .bind(batch)
            .bind(file)
            .bind(owner.to_string())
            .execute(&pool)
            .await
            .unwrap();
            let template = uuid::Uuid::new_v4();
            insert_fusillade_template(
                &pool,
                template,
                None,
                "test-model",
                "test-key",
                "http://test",
                "/v1/chat/completions",
                r#"{"model":"test-model","messages":[{"role":"user","content":"private content"}]}"#,
                None,
            )
            .await;
            sqlx::query(
                "INSERT INTO fusillade.requests (id, template_id, model, state, created_by) VALUES ($1, $2, 'test-model', 'pending', $3)",
            )
            .bind(response)
            .bind(template)
            .bind(owner.to_string())
            .execute(&pool)
            .await
            .unwrap();
            fixtures.push((file, batch, response));
        }
        for sso in [true, false] {
            let auth_request = |path: &str| {
                let request = app.get(path);
                if sso {
                    request
                        .add_header(&headers[0].0, &headers[0].1)
                        .add_header(&headers[1].0, &headers[1].1)
                } else {
                    request.add_header(
                        "Authorization",
                        format!(
                            "Bearer {}",
                            if path.starts_with("/admin") {
                                &platform_key.secret
                            } else {
                                &key.secret
                            }
                        ),
                    )
                }
            };
            for path in ["/ai/v1/files", "/ai/v1/batches", "/admin/api/v1/batches/requests"] {
                let response = auth_request(path).await;
                response.assert_status_ok();
                let body = response.json::<Value>();
                assert_eq!(
                    body["data"].as_array().unwrap().len(),
                    1,
                    "admin={admin}, sso={sso}, {path}: {body}"
                );
            }
            for (index, (file, batch, response)) in fixtures.iter().enumerate() {
                let expected = if index == 0 { StatusCode::OK } else { StatusCode::NOT_FOUND };
                for path in [
                    format!("/ai/v1/files/{file}"),
                    format!("/ai/v1/batches/{batch}"),
                    format!("/admin/api/v1/batches/requests/{response}"),
                ] {
                    auth_request(&path).await.assert_status(expected);
                }
                if index != 0 {
                    for path in [format!("/ai/v1/files/{file}/content"), format!("/ai/v1/batches/{batch}/results")] {
                        auth_request(&path).await.assert_status_not_found();
                    }
                }
            }
            // Valid organization context grants access only to that organization's content.
            auth_request(&format!("/ai/v1/files/{}", fixtures[2].0))
                .add_header("X-Organization-Id", org.id.to_string())
                .await
                .assert_status_ok();
            // A header naming an unrelated customer is not authorization.
            auth_request(&format!("/ai/v1/files/{}", fixtures[1].0))
                .add_header("X-Organization-Id", customer.id.to_string())
                .await
                .assert_status_not_found();
        }
    }
}
