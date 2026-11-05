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
async fn legacy_admin_keys_are_ordinary_but_sso_keeps_credits_and_zdr(pool: PgPool) {
    let mut config = crate::test::utils::create_test_config();
    config.auth.native.enabled = true;
    let (app, _services) = crate::test::utils::create_test_app_with_config(pool.clone(), config.clone(), false).await;
    let admin = true;
    let actor = create_test_admin_user(&pool, Role::StandardUser).await;
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

    // Allowed self-profile writes must return the same effective authority as GET.
    let mut identity = serde_json::to_value(&actor).unwrap();
    identity["organizations"] = json!([]);
    let jwt = crate::auth::session::create_session_token(&serde_json::from_value::<CurrentUser>(identity).unwrap(), &config).unwrap();
    let cookie = format!("{}={jwt}", config.auth.native.session.cookie_name);
    for (header, credential) in [("Authorization", bearer.as_str()), ("Cookie", cookie.as_str())] {
        let updated = app
            .patch(&format!("/admin/api/v1/users/{}", actor.id))
            .add_header(header, credential)
            .json(&json!({"display_name": "Updated own profile"}))
            .await;
        updated.assert_status_ok();
        let body = updated.json::<Value>();
        assert_eq!(body["display_name"], "Updated own profile");
        assert_eq!(body["is_admin"], false, "admin={admin}, {header}");
        assert_eq!(body["roles"], own.json::<Value>()["roles"], "admin={admin}, {header}");
    }

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

#[dwctl_test_macros::test]
async fn keys_require_live_accounts_and_active_org_membership(pool: PgPool) {
    let (app, _services) = create_test_app(pool.clone(), false).await;
    for case in [
        "deleted_organization",
        "deleted_holder",
        "revoked_membership",
        "deleted_personal_account",
    ] {
        let holder = create_test_user(&pool, Role::BatchAPIUser).await;
        let account_id = if case == "deleted_personal_account" {
            holder.id
        } else {
            crate::test::utils::create_test_org(&pool, holder.id).await.id
        };
        let key = create_test_api_key_for_user(&pool, holder.id).await;
        sqlx::query("UPDATE api_keys SET user_id = $1 WHERE id = $2")
            .bind(account_id)
            .bind(key.id)
            .execute(&pool)
            .await
            .unwrap();
        let bearer = format!("Bearer {}", key.secret);
        app.get("/ai/v1/files")
            .add_header("Authorization", &bearer)
            .await
            .assert_status_ok();
        if case == "revoked_membership" {
            sqlx::query("DELETE FROM user_organizations WHERE user_id = $1 AND organization_id = $2")
                .bind(holder.id)
                .bind(account_id)
                .execute(&pool)
                .await
                .unwrap();
        } else {
            let deleted_id = if case == "deleted_organization" { account_id } else { holder.id };
            // Leave keys and memberships intact to ensure authentication itself
            // fails closed, without relying on asynchronous deletion cleanup.
            sqlx::query("UPDATE users SET is_deleted = true WHERE id = $1")
                .bind(deleted_id)
                .execute(&pool)
                .await
                .unwrap();
        }
        let response = app.get("/ai/v1/files").add_header("Authorization", &bearer).await;
        assert_eq!(response.status_code(), StatusCode::UNAUTHORIZED, "{case}: {}", response.text());
    }
}

#[test]
fn platform_configuration_and_system_authority_are_distinct() {
    use super::permissions::has_permission;
    use uuid::Uuid;

    let mut actor: CurrentUser = serde_json::from_value(json!({
        "id": Uuid::new_v4(), "username": "operator", "email": "operator@example.com",
        "is_admin": true, "roles": ["StandardUser", "PlatformManager", "BillingManager", "RequestViewer"],
        "display_name": null, "avatar_url": null, "payment_provider_id": null,
        "organizations": [], "active_organization": null
    }))
    .unwrap();
    actor.api_key_id = Some(Uuid::new_v4());
    actor = actor.with_auth_method(AuthMethod::PlatformApiKey);
    assert!(!actor.is_admin);
    assert!(actor.roles.contains(&Role::PlatformManager));
    assert!(!actor.roles.contains(&Role::RequestViewer));
    assert!(!actor.roles.contains(&Role::BillingManager));
    for resource in [
        Resource::Models,
        Resource::CompositeModels,
        Resource::Endpoints,
        Resource::Groups,
        Resource::Credits,
    ] {
        for operation in [Operation::CreateAll, Operation::ReadAll, Operation::UpdateAll, Operation::DeleteAll] {
            assert!(has_permission(&actor, resource, operation), "{resource:?} {operation:?}");
        }
    }
    assert!(has_permission(&actor, Resource::Users, Operation::CreateAll));
    assert!(has_permission(&actor, Resource::Users, Operation::ReadAll));
    for resource in [
        Resource::Files,
        Resource::Batches,
        Resource::ApiKeys,
        Resource::Connections,
        Resource::Webhooks,
    ] {
        for operation in [Operation::CreateAll, Operation::ReadAll, Operation::UpdateAll, Operation::DeleteAll] {
            assert!(!has_permission(&actor, resource, operation), "{resource:?} {operation:?}");
        }
    }
    assert!(!has_permission(&actor, Resource::Users, Operation::UpdateAll));
    assert!(!has_permission(&actor, Resource::Users, Operation::DeleteAll));
    assert!(!has_permission(&actor, Resource::Requests, Operation::ReadAll));

    // Both reserved IDs and verified platform-key authentication are required.
    actor.auth_method = AuthMethod::SystemApiKey;
    assert!(!actor.is_system());
    actor.id = Uuid::nil();
    assert!(!actor.is_system());
    actor.api_key_id = Some(Uuid::nil());
    assert!(actor.is_system());
    actor.roles.clear();
    for resource in [
        Resource::Users,
        Resource::Organizations,
        Resource::Files,
        Resource::Batches,
        Resource::ApiKeys,
        Resource::Connections,
        Resource::Webhooks,
        Resource::Requests,
    ] {
        for operation in [
            Operation::CreateAll,
            Operation::ReadAll,
            Operation::UpdateAll,
            Operation::DeleteAll,
            Operation::SystemAccess,
        ] {
            assert!(has_permission(&actor, resource, operation), "system {resource:?} {operation:?}");
        }
    }
    for method in [
        AuthMethod::ApiKey,
        AuthMethod::PlatformApiKey,
        AuthMethod::NativeSession,
        AuthMethod::TrustedSso,
    ] {
        actor.auth_method = method;
        assert!(!actor.is_system(), "{method:?}");
        assert!(!has_permission(&actor, Resource::ApiKeys, Operation::CreateAll), "{method:?}");
    }
    // Neither provenance nor the authenticating key ID can be injected via JSON.
    let mut wire = serde_json::to_value(&actor).unwrap();
    wire["auth_method"] = json!("SystemApiKey");
    wire["api_key_id"] = json!(Uuid::nil());
    let decoded: CurrentUser = serde_json::from_value(wire).unwrap();
    assert!(!decoded.is_system());
    assert_eq!(decoded.api_key_id, None);
}

#[dwctl_test_macros::test]
async fn platform_keys_manage_configuration_without_customer_takeover(pool: PgPool) {
    let (app, _services) = create_test_app(pool.clone(), false).await;
    let actor = create_test_user(&pool, Role::PlatformManager).await;
    let customer = create_test_user(&pool, Role::BatchAPIUser).await;
    let key = create_test_api_key_for_user(&pool, actor.id).await;
    sqlx::query("UPDATE api_keys SET purpose = 'platform' WHERE id = $1")
        .bind(key.id)
        .execute(&pool)
        .await
        .unwrap();
    let bearer = format!("Bearer {}", key.secret);
    let own = app.get("/admin/api/v1/users/current").add_header("Authorization", &bearer).await;
    own.assert_status_ok();
    assert_eq!(own.json::<Value>()["is_admin"], false);
    assert!(own.json::<Value>()["roles"].as_array().unwrap().contains(&json!("PlatformManager")));
    app.get("/admin/api/v1/users")
        .add_header("Authorization", &bearer)
        .await
        .assert_status_ok();
    let endpoint = app
        .post("/admin/api/v1/endpoints")
        .add_header("Authorization", &bearer)
        .json(&json!({"name": "automation-fixture", "url": "http://127.0.0.1:1/v1", "sync": false, "skip_fetch": true, "kind": "hosted"}))
        .await;
    endpoint.assert_status(StatusCode::CREATED);
    let endpoint_id = endpoint.json::<Value>()["id"].as_str().unwrap().to_string();
    app.delete(&format!("/admin/api/v1/endpoints/{endpoint_id}"))
        .add_header("Authorization", &bearer)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    for role in [
        "StandardUser",
        "BatchAPIUser",
        "ConnectionsUser",
        "BackgroundInferenceUser",
        "PlatformManager",
        "BillingManager",
        "RequestViewer",
    ] {
        let roles = if role == "StandardUser" {
            json!([role])
        } else {
            json!(["StandardUser", role])
        };
        let response = app
            .post("/admin/api/v1/users")
            .add_header("Authorization", &bearer)
            .json(&json!({"username": format!("provisioned-{role}"), "email": format!("provisioned-{role}@example.test"), "roles": roles}))
            .await;
        let expected = if ["PlatformManager", "BillingManager", "RequestViewer"].contains(&role) {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::CREATED
        };
        assert_eq!(response.status_code(), expected, "{role}: {}", response.text());
    }
    let target = format!("/admin/api/v1/users/{}", customer.id);
    app.post("/admin/api/v1/transactions")
        .add_header("Authorization", &bearer)
        .json(&json!({"user_id":customer.id, "transaction_type":"admin_grant", "amount":"5", "source_id":"automation-grant"}))
        .await
        .assert_status(StatusCode::CREATED);
    app.patch(&target)
        .add_header("Authorization", &bearer)
        .json(&json!({"zero_data_retention":true}))
        .await
        .assert_status_ok();
    for body in [
        json!({"zero_data_retention":false}),
        json!({"roles":["PlatformManager"]}),
        json!({"display_name":"Changed"}),
    ] {
        app.patch(&target)
            .add_header("Authorization", &bearer)
            .json(&body)
            .await
            .assert_status_forbidden();
    }
    app.post(&format!("{target}/api-keys"))
        .add_header("Authorization", &bearer)
        .json(&json!({"name":"forbidden-customer-key", "purpose":"platform"}))
        .await
        .assert_status_forbidden();
    app.delete(&target)
        .add_header("Authorization", &bearer)
        .await
        .assert_status_forbidden();
    let org = crate::test::utils::create_test_org(&pool, customer.id).await;
    app.patch(&format!("/admin/api/v1/organizations/{}", org.id))
        .add_header("Authorization", &bearer)
        .json(&json!({"zero_data_retention":true}))
        .await
        .assert_status_ok();
    app.post(&format!("/admin/api/v1/organizations/{}/members", org.id))
        .add_header("Authorization", &bearer)
        .json(&json!({"user_id":actor.id, "role":"owner"}))
        .await
        .assert_status_forbidden();
    // Supplying SSO headers cannot make an inference key into a platform key.
    let inference_key = create_test_api_key_for_user(&pool, actor.id).await;
    let headers = add_auth_headers(&actor);
    for credential in [format!("Bearer {}", inference_key.secret), "Bearer invalid".to_string()] {
        let response = app
            .get("/admin/api/v1/users")
            .add_header("Authorization", &credential)
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await;
        assert!(matches!(response.status_code(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN));
    }
}

#[dwctl_test_macros::test]
async fn org_issued_platform_keys_do_not_carry_member_platform_roles(pool: PgPool) {
    let (app, _services) = create_test_app(pool.clone(), false).await;
    let manager = create_test_user(&pool, Role::PlatformManager).await;
    let owner = create_test_user(&pool, Role::StandardUser).await;
    let org = crate::test::utils::create_test_org(&pool, owner.id).await;
    let headers = add_auth_headers(&owner);
    // Org owners add members without consent and may issue keys attributed to them.
    app.post(&format!("/admin/api/v1/organizations/{}/members", org.id))
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&json!({"user_id": manager.id}))
        .await
        .assert_status(StatusCode::CREATED);
    let issued = app
        .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&json!({"name": "attributed-platform-key", "purpose": "platform", "member_id": manager.id}))
        .await;
    issued.assert_status(StatusCode::CREATED);
    let issued = issued.json::<Value>();
    assert_eq!(issued["created_by"], json!(manager.id));
    let bearer = format!("Bearer {}", issued["key"].as_str().unwrap());

    // The secret belongs to the issuer, so it must not exercise the member's platform role.
    let current = app.get("/admin/api/v1/users/current").add_header("Authorization", &bearer).await;
    current.assert_status_ok();
    assert!(
        !current.json::<Value>()["roles"]
            .as_array()
            .unwrap()
            .contains(&json!("PlatformManager"))
    );
    app.get("/admin/api/v1/users")
        .add_header("Authorization", &bearer)
        .await
        .assert_status_forbidden();
    app.post("/admin/api/v1/users")
        .add_header("Authorization", &bearer)
        .json(&json!({"username": "via-org-key", "email": "via-org-key@example.test", "roles": ["StandardUser"]}))
        .await
        .assert_status_forbidden();
}

#[dwctl_test_macros::test]
async fn reserved_system_key_can_provision_and_retire_accounts(pool: PgPool) {
    let (app, _services) = create_test_app(pool.clone(), false).await;
    let secret = crate::crypto::generate_api_key();
    let hash = crate::crypto::hash_api_key(&secret);
    let installed = sqlx::query("UPDATE api_keys SET secret = $1, secret_hash = $2, secret_prefix = $3, purpose = 'platform' WHERE id = '00000000-0000-0000-0000-000000000000'")
        .bind(&secret).bind(&hash[..]).bind(crate::crypto::api_key_prefix(&secret)).execute(&pool).await.unwrap();
    assert_eq!(installed.rows_affected(), 1);
    let bearer = format!("Bearer {secret}");
    let created = app.post("/admin/api/v1/users").add_header("Authorization", &bearer)
        .json(&json!({"username":"system-provisioned", "email":"system-provisioned@example.test", "roles":["StandardUser","PlatformManager","BatchAPIUser"]})).await;
    created.assert_status(StatusCode::CREATED);
    let user = created.json::<Value>();
    let id = user["id"].as_str().unwrap();
    let target = format!("/admin/api/v1/users/{id}");
    let secondary = create_test_api_key_for_user(&pool, uuid::Uuid::nil()).await;
    sqlx::query("UPDATE api_keys SET purpose = 'platform' WHERE id = $1")
        .bind(secondary.id)
        .execute(&pool)
        .await
        .unwrap();
    app.post(&format!("{target}/api-keys"))
        .add_header("Authorization", &format!("Bearer {}", secondary.secret))
        .json(&json!({"name":"not-the-system-key", "purpose":"platform"}))
        .await
        .assert_status_forbidden();
    let secondary_bearer = format!("Bearer {}", secondary.secret);
    app.get("/admin/api/v1/users")
        .add_header("Authorization", &secondary_bearer)
        .await
        .assert_status_forbidden();
    let file_id = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO fusillade.files (id, name, purpose, uploaded_by) VALUES ($1, 'private.jsonl', 'batch', $2)")
        .bind(file_id)
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    app.get(&format!("/ai/v1/files/{file_id}"))
        .add_header("Authorization", &bearer)
        .await
        .assert_status_ok();
    let webhook = app
        .post("/admin/api/v1/users/current/webhooks")
        .add_header("Authorization", &bearer)
        .json(&json!({"url":"https://hooks.example.test/events", "scope":"platform", "event_types":["api_key.created"]}))
        .await;
    webhook.assert_status(StatusCode::CREATED);
    app.patch(&target)
        .add_header("Authorization", &bearer)
        .json(&json!({"roles":["StandardUser","BatchAPIUser"],"zero_data_retention":false}))
        .await
        .assert_status_ok();
    let issued = app
        .post(&format!("{target}/api-keys"))
        .add_header("Authorization", &bearer)
        .json(&json!({"name":"bootstrap", "purpose":"platform"}))
        .await;
    issued.assert_status(StatusCode::CREATED);
    let key_id = issued.json::<Value>()["id"].as_str().unwrap().to_string();
    app.post(&format!("{target}/api-keys/{key_id}/rotate"))
        .add_header("Authorization", &bearer)
        .await
        .assert_status_ok();
    app.delete(&format!("{target}/api-keys/{key_id}"))
        .add_header("Authorization", &bearer)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    app.get("/ai/v1/files")
        .add_header("Authorization", &bearer)
        .await
        .assert_status_ok();
    let org = crate::test::utils::create_test_org(&pool, uuid::Uuid::parse_str(id).unwrap()).await;
    let org_path = format!("/admin/api/v1/organizations/{}", org.id);
    app.patch(&org_path)
        .add_header("Authorization", &bearer)
        .json(&json!({"zero_data_retention":false,"display_name":"Service managed"}))
        .await
        .assert_status_ok();
    app.delete(&org_path)
        .add_header("Authorization", &bearer)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    app.delete(&target)
        .add_header("Authorization", &bearer)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    sqlx::query("UPDATE api_keys SET is_deleted = true WHERE id = '00000000-0000-0000-0000-000000000000'")
        .execute(&pool)
        .await
        .unwrap();
    app.get("/admin/api/v1/users")
        .add_header("Authorization", &bearer)
        .await
        .assert_status_unauthorized();
}

#[dwctl_test_macros::test]
async fn non_admin_platform_manager_sso_keeps_credits_and_enable_only_zdr(pool: PgPool) {
    let (app, _services) = create_test_app(pool.clone(), false).await;
    let actor = create_test_user(&pool, Role::PlatformManager).await;
    assert!(!actor.is_admin);
    let customer = create_test_user(&pool, Role::BatchAPIUser).await;
    let headers = add_auth_headers(&actor);
    let target = format!("/admin/api/v1/users/{}", customer.id);
    let grant = json!({"user_id": customer.id, "transaction_type": "admin_grant", "amount": "10",
        "source_id": actor.id.to_string(), "description": "Promotional contribution"});
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
}
