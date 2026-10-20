//! Inference model discovery must expose only the caller's customer prices.

use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::PgPool;
use utoipa::OpenApi;
use uuid::Uuid;

use crate::{
    api::models::{api_keys::ApiKeyCreate, users::Role},
    db::{
        handlers::{Repository, api_keys::ApiKeys},
        models::api_keys::{ApiKeyCreateDBRequest, ApiKeyPurpose},
    },
    openapi::AiApiDoc,
    test::utils::{
        add_deployment_to_group, add_user_to_group, create_test_admin_user, create_test_api_key_for_user, create_test_app,
        create_test_deployment, create_test_group, create_test_org, create_test_user,
    },
};

#[dwctl_test_macros::test]
async fn ai_models_pricing_is_opt_in_and_preserves_inference_access(pool: PgPool) {
    let (server, _services) = create_test_app(pool.clone(), false).await;
    let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
    let user = create_test_user(&pool, Role::StandardUser).await;
    let key = create_test_api_key_for_user(&pool, user.id).await;
    let visible = create_test_deployment(&pool, admin.id, "priced", "priced").await;
    let hidden = create_test_deployment(&pool, admin.id, "hidden", "hidden").await;
    sqlx::query("INSERT INTO deployment_groups (deployment_id, group_id, granted_by) VALUES ($1, $2, $3)")
        .bind(visible.id)
        .bind(Uuid::nil())
        .bind(admin.id)
        .execute(&pool)
        .await
        .unwrap();
    for model in [visible.id, hidden.id] {
        sqlx::query("INSERT INTO model_tariffs (deployed_model_id, name, input_price_per_token, output_price_per_token, api_key_purpose) VALUES ($1, 'realtime', 0.000001, 0.000002, 'realtime')")
            .bind(model).execute(&pool).await.unwrap();
    }

    for query in ["", "?include=unknown"] {
        let response = server
            .get(&format!("/ai/v1/models{query}"))
            .add_header("authorization", format!("Bearer {}", key.secret))
            .await;
        response.assert_status_ok();
        let body: Value = response.json();
        assert_eq!(body["object"], "list");
        assert_eq!(body["data"].as_array().unwrap().len(), 1);
        let model = &body["data"][0];
        assert_eq!(model["id"], "priced");
        assert_eq!(model.as_object().unwrap().len(), 4, "default must retain the OpenAI shape");
    }

    let response = server
        .get("/ai/v1/models?include=pricing")
        .add_header("authorization", format!("Bearer {}", key.secret))
        .await;
    response.assert_status_ok();
    let body: Value = response.json();
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    let model = &body["data"][0];
    assert_eq!(model["id"], "priced");
    for (field, price) in [("input_price_per_token", 1), ("output_price_per_token", 2)] {
        assert_eq!(
            model["tariffs"][0][field].as_str().unwrap().parse::<Decimal>().unwrap(),
            Decimal::new(price, 6)
        );
    }
    assert_eq!(model["cache_pricing"]["enabled"], false);
    assert_eq!(model.as_object().unwrap().len(), 6, "only customer pricing fields may be added");

    // Inference keys still cannot use management operations.
    let response = server
        .get("/admin/api/v1/models?include=pricing")
        .add_header("authorization", format!("Bearer {}", key.secret))
        .await;
    response.assert_status_unauthorized();

    for authorization in [None, Some("Bearer unknown-key")] {
        let mut request = server.get("/ai/v1/models?include=pricing");
        if let Some(header) = authorization {
            request = request.add_header("authorization", header);
        }
        request.await.assert_status_unauthorized();
    }

    // Pricing discovery composes with existing optional model capabilities.
    let response = server
        .get("/ai/v1/models?include=unknown,%20pricing%20&include_reasoning_capabilities=true&available_for_realtime=true")
        .add_header("authorization", format!("Bearer {}", key.secret))
        .await;
    response.assert_status_ok();
    let body: Value = response.json();
    assert_eq!(body["data"][0]["tariffs"], model["tariffs"]);
    assert_eq!(body["object"], json!("list"));
}

#[dwctl_test_macros::test]
async fn ai_models_pricing_filters_unavailable_models_tariffs_and_keys(pool: PgPool) {
    let (server, _services) = create_test_app(pool.clone(), false).await;
    let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
    let user = create_test_user(&pool, Role::StandardUser).await;
    let key = create_test_api_key_for_user(&pool, user.id).await;
    let group = create_test_group(&pool).await;
    let other_group = create_test_group(&pool).await;
    add_user_to_group(&pool, user.id, group.id).await;
    let model = create_test_deployment(&pool, admin.id, "batch-only", "batch-only").await;
    let deleted = create_test_deployment(&pool, admin.id, "deleted", "deleted").await;
    let inactive = create_test_deployment(&pool, admin.id, "inactive", "inactive").await;
    let hidden = create_test_deployment(&pool, admin.id, "private", "private").await;
    let unpriced = create_test_deployment(&pool, admin.id, "unpriced", "unpriced").await;
    for id in [model.id, deleted.id, inactive.id, unpriced.id] {
        add_deployment_to_group(&pool, id, group.id, admin.id).await;
    }
    add_deployment_to_group(&pool, hidden.id, other_group.id, admin.id).await;
    sqlx::query("UPDATE deployed_models SET deleted = TRUE WHERE id = $1")
        .bind(deleted.id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE deployed_models SET status = 'inactive' WHERE id = $1")
        .bind(inactive.id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE deployed_models SET allowed_batch_completion_windows = ARRAY['24h'] WHERE id = $1")
        .bind(model.id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO model_traffic_rules (deployed_model_id,api_key_purpose,action) VALUES ($1,'realtime','deny')")
        .bind(model.id)
        .execute(&pool)
        .await
        .unwrap();
    for (purpose, window) in [("realtime", None), ("batch", Some("24h")), ("batch", Some("1h"))] {
        sqlx::query("INSERT INTO model_tariffs (deployed_model_id,name,input_price_per_token,output_price_per_token,api_key_purpose,completion_window) VALUES ($1,'active',0.000001,0.000002,$2,$3)")
            .bind(model.id).bind(purpose).bind(window).execute(&pool).await.unwrap();
    }
    // Historical and scheduled prices must not appear in the current catalogue.
    for (name, start, end) in [("expired", -3, -2), ("future", 2, 3)] {
        sqlx::query("INSERT INTO model_tariffs (deployed_model_id,name,input_price_per_token,output_price_per_token,api_key_purpose,completion_window,valid_from,valid_until) VALUES ($1,$2,0.000009,0.000009,'batch','24h',NOW()+$3*INTERVAL '1 day',NOW()+$4*INTERVAL '1 day')")
            .bind(model.id).bind(name).bind(start as f64).bind(end as f64).execute(&pool).await.unwrap();
    }

    for purpose in ["realtime", "batch", "playground", "continuation"] {
        sqlx::query("UPDATE api_keys SET purpose = $1 WHERE id = $2")
            .bind(purpose)
            .bind(key.id)
            .execute(&pool)
            .await
            .unwrap();
        let response = server
            .get("/ai/v1/models?include=pricing")
            .add_header("authorization", format!("Bearer {}", key.secret))
            .await;
        response.assert_status_ok();
        let body: Value = response.json();
        let models = body["data"].as_array().unwrap();
        assert_eq!(models.len(), 2, "{purpose}: hidden, inactive and deleted models must be excluded");
        let priced = models.iter().find(|m| m["id"] == "batch-only").unwrap();
        let tariffs = priced["tariffs"].as_array().unwrap();
        assert_eq!(tariffs.len(), 1, "{purpose}: only the active, allowed tariff");
        assert_eq!(tariffs[0]["name"], "active");
        assert_eq!(tariffs[0]["api_key_purpose"], "batch");
        assert_eq!(tariffs[0]["completion_window"], "24h");
        let unpriced = models.iter().find(|m| m["id"] == "unpriced").unwrap();
        assert_eq!(unpriced["tariffs"], json!([]));
        assert_eq!(unpriced["cache_pricing"]["enabled"], false);
    }
    for (query, expected_ids) in [
        (format!("group={}&available_for_realtime=false", group.id), json!(["batch-only"])),
        (format!("group={}&available_for_realtime=true", group.id), json!(["unpriced"])),
        (format!("group={}", other_group.id), json!([])),
    ] {
        let response = server
            .get(&format!("/ai/v1/models?include=pricing&{query}"))
            .add_header("authorization", format!("Bearer {}", key.secret))
            .await;
        response.assert_status_ok();
        let body: Value = response.json();
        let ids: Vec<_> = body["data"].as_array().unwrap().iter().map(|m| m["id"].clone()).collect();
        assert_eq!(json!(ids), expected_ids, "{query}");
    }
    for (purpose, deleted) in [("platform", false), ("realtime", true)] {
        sqlx::query("UPDATE api_keys SET purpose = $1, is_deleted = $2 WHERE id = $3")
            .bind(purpose)
            .bind(deleted)
            .bind(key.id)
            .execute(&pool)
            .await
            .unwrap();
        let response = server
            .get("/ai/v1/models?include=pricing")
            .add_header("authorization", format!("Bearer {}", key.secret))
            .await;
        response.assert_status_unauthorized();
    }
}

#[dwctl_test_macros::test]
async fn ai_models_pricing_uses_billing_account_without_operator_details(pool: PgPool) {
    let (server, _services) = create_test_app(pool.clone(), false).await;
    // A manager's inference key must still receive only its billed account's prices.
    let owner = create_test_admin_user(&pool, Role::PlatformManager).await;
    let org = create_test_org(&pool, owner.id).await;
    let other_org = create_test_org(&pool, owner.id).await;
    let personal_key = create_test_api_key_for_user(&pool, owner.id).await;
    let org_key = {
        let mut conn = pool.acquire().await.unwrap();
        ApiKeys::new(&mut conn)
            .create(&ApiKeyCreateDBRequest::new(
                org.id,
                owner.id,
                ApiKeyCreate {
                    name: "org inference".into(),
                    description: None,
                    purpose: ApiKeyPurpose::Realtime,
                    member_id: None,
                    spend_limit: None,
                    spend_limit_interval: None,
                },
            ))
            .await
            .unwrap()
    };
    let model = create_test_deployment(&pool, owner.id, "account-prices", "account-prices").await;
    add_deployment_to_group(&pool, model.id, Uuid::nil(), owner.id).await;

    for (account, class, price) in [
        (None, None, 3),
        (Some(owner.id), None, 4),
        (Some(org.id), None, 2),
        (Some(other_org.id), None, 1),
        (Some(org.id), Some("interactive"), 9),
    ] {
        sqlx::query("INSERT INTO model_tariffs (deployed_model_id, user_id, serving_class, name, input_price_per_token, output_price_per_token, api_key_purpose) VALUES ($1,$2,$3,'realtime',$4,$4,'realtime')")
            .bind(model.id).bind(account).bind(class).bind(Decimal::new(price, 6))
            .execute(&pool).await.unwrap();
    }
    // Both accounts fall back to the model's general batch tariff.
    sqlx::query("INSERT INTO model_tariffs (deployed_model_id, name, input_price_per_token, output_price_per_token, api_key_purpose, completion_window) VALUES ($1,'batch',0.000001,0.000001,'batch','24h')")
        .bind(model.id).execute(&pool).await.unwrap();

    for (account, class, multiplier) in [
        (None, None, 1),
        (Some(owner.id), None, 3),
        (Some(org.id), None, 2),
        (Some(other_org.id), None, 5),
        (Some(org.id), Some("interactive"), 9),
    ] {
        sqlx::query("INSERT INTO model_cache_tariffs (deployed_model_id,user_id,serving_class,write_multiplier_5m,write_multiplier_1h,write_multiplier_24h,read_multiplier,min_prefix_tokens) VALUES ($1,$2,$3,1,2,3,$4,$5)")
            .bind(model.id).bind(account).bind(class).bind(Decimal::new(multiplier,1))
            .bind(if account.is_none() {2048} else {1})
            .execute(&pool).await.unwrap();
    }

    for (case, key, price, multiplier) in [
        ("manager personal", &personal_key.secret, 4, 3),
        ("organization", &org_key.secret, 2, 2),
    ] {
        let response = server
            .get("/ai/v1/models?include=pricing,endpoints,groups,metrics,components,provider_pricing")
            .add_header("authorization", format!("Bearer {key}"))
            .await;
        response.assert_status_ok();
        let body: Value = response.json();
        let model = &body["data"][0];
        let tariffs = model["tariffs"].as_array().unwrap();
        assert_eq!(tariffs.len(), 2, "{case}: other scopes must be hidden");
        for (purpose, expected) in [("realtime", price), ("batch", 1)] {
            let tariff = tariffs.iter().find(|t| t["api_key_purpose"] == purpose).unwrap();
            assert_eq!(
                tariff["input_price_per_token"].as_str().unwrap().parse::<Decimal>().unwrap(),
                Decimal::new(expected, 6),
                "{case}: {purpose}"
            );
            assert!(tariff.get("organization_id").is_none(), "{case}: ownership must stay private");
            assert!(tariff.get("serving_class").is_none(), "{case}: class must stay private");
        }
        assert_eq!(
            model["cache_pricing"]["read_multiplier"]
                .as_str()
                .unwrap()
                .parse::<Decimal>()
                .unwrap(),
            Decimal::new(multiplier, 1),
            "{case}"
        );
        assert_eq!(model["cache_pricing"]["min_prefix_tokens"], 2048);
        assert_eq!(model.as_object().unwrap().len(), 6, "{case}: no operator configuration");
    }
}

#[test]
fn ai_models_pricing_openapi_documents_opt_in_prices_and_cache_multipliers() {
    let spec = serde_json::to_value(AiApiDoc::openapi()).unwrap();
    let include = spec["paths"]["/models"]["get"]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "include")
        .unwrap();
    assert_eq!(include["in"], "query");
    assert_eq!(include["schema"]["type"], "string");
    assert!(include["description"].as_str().unwrap().contains("pricing"));
    let model = &spec["components"]["schemas"]["ModelObject"];
    for field in ["tariffs", "cache_pricing"] {
        assert!(model["properties"][field].is_object(), "document {field}");
        assert!(!model["required"].as_array().unwrap().contains(&json!(field)), "{field} is opt-in");
    }
    assert_eq!(
        spec["components"]["schemas"]["TariffResponse"]["properties"]["input_price_per_token"]["type"],
        "string"
    );
    assert!(spec["components"]["schemas"]["CachePricingResponse"]["properties"]["read_multiplier"].is_object());
}
