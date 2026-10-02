use super::model_aliases::ModelAliasMap;
use sqlx::PgPool;
use uuid::Uuid;

async fn add_fast_alias(pool: &PgPool) -> (Uuid, Uuid) {
    let endpoint = Uuid::new_v4();
    let model = Uuid::new_v4();
    let class = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO inference_endpoints (id,name,url,created_by)
         VALUES ($1,'gateway','http://gateway.test','00000000-0000-0000-0000-000000000000')",
    )
    .bind(endpoint)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO deployed_models (id,model_name,alias,hosted_on,created_by)
         VALUES ($1,'old-upstream','example/model',$2,'00000000-0000-0000-0000-000000000000')",
    )
    .bind(model)
    .bind(endpoint)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO model_serving_classes
         (id,deployed_model_id,class_key,display_name,inference_endpoint_id,upstream_model_name)
         VALUES ($1,$2,'fast','Fast',$3,'dynamo-example/fast')",
    )
    .bind(class)
    .bind(model)
    .bind(endpoint)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO model_aliases (alias,deployed_model_id,serving_class_id) VALUES ('example/model-fast',$1,$2)")
        .bind(model)
        .bind(class)
        .execute(pool)
        .await
        .unwrap();
    (model, class)
}

async fn active_model(pool: &PgPool) -> (Uuid, Uuid) {
    let ids = add_fast_alias(pool).await;
    sqlx::query("INSERT INTO model_serving_classes (deployed_model_id,class_key,display_name,inference_endpoint_id,upstream_model_name) SELECT id,'standard','Standard',hosted_on,'dynamo-example/throughput' FROM deployed_models WHERE id=$1")
        .bind(ids.0).execute(pool).await.unwrap();
    sqlx::query("UPDATE deployed_models SET routing_mode='class_routes' WHERE id=$1")
        .bind(ids.0)
        .execute(pool)
        .await
        .unwrap();
    ids
}

async fn targets(pool: &PgPool) -> onwards::target::Targets {
    crate::sync::onwards_config::load_targets_from_db(pool, &[], false, &Default::default())
        .await
        .unwrap()
}

#[sqlx::test]
async fn activation_selects_classes_without_replacing_model_and_rollback_restores_legacy(pool: PgPool) {
    let (model, _) = active_model(&pool).await;
    let aliases = ModelAliasMap::load(&pool).await.unwrap();
    let live = targets(&pool).await;
    let resolve = |name| {
        crate::inference::model_aliases::resolve_class_route(&aliases, &live, name, false)
            .unwrap()
            .unwrap()
    };
    let (fast_name, fast) = resolve("example/model:fast");
    assert_eq!(fast_name, "example/model:fast");
    assert_eq!(fast.model_id, model);
    assert_eq!(fast.upstream_model_name, "dynamo-example/fast");
    assert_eq!(resolve("example/model-fast"), (fast_name, fast.clone()));
    assert_eq!(resolve("example/model").1.class_key, "standard");
    assert!(
        !live.targets.contains_key("example/model-fast"),
        "discovery must never enumerate synonyms"
    );
    sqlx::query("UPDATE deployed_models SET routing_mode='legacy' WHERE id=$1")
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();
    let legacy = targets(&pool).await;
    assert!(!legacy.targets.contains_key("example/model:fast"));
    assert!(
        legacy
            .targets
            .get("example/model")
            .unwrap()
            .default_pool()
            .class_identity()
            .is_none()
    );
    assert!(crate::inference::model_aliases::resolve_class_route(&aliases, &legacy, "example/model-fast", false).is_err());
    let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM model_serving_classes WHERE deployed_model_id=$1")
        .bind(model)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kept, 2);
}

#[sqlx::test]
async fn batch_and_flex_force_standard_for_fast_and_synonym(pool: PgPool) {
    active_model(&pool).await;
    let aliases = ModelAliasMap::load(&pool).await.unwrap();
    let live = targets(&pool).await;
    for submitted in ["example/model", "example/model:fast", "example/model-fast"] {
        let (name, identity) = crate::inference::model_aliases::resolve_class_route(&aliases, &live, submitted, true)
            .unwrap()
            .unwrap();
        assert_eq!(name, "example/model");
        assert_eq!(identity.class_key, "standard");
        assert_eq!(identity.upstream_model_name, "dynamo-example/throughput");
    }
}

#[sqlx::test]
async fn live_destination_edits_do_not_reload_synonym_snapshot(pool: PgPool) {
    let (model, _) = active_model(&pool).await;
    let aliases = ModelAliasMap::load(&pool).await.unwrap();
    let before = targets(&pool).await;
    let old = crate::inference::model_aliases::resolve_class_route(&aliases, &before, "example/model-fast", false)
        .unwrap()
        .unwrap()
        .1;
    sqlx::query("UPDATE model_serving_classes SET upstream_model_name='another-upstream' WHERE deployed_model_id=$1 AND class_key='fast'")
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();
    let after = targets(&pool).await;
    let new = crate::inference::model_aliases::resolve_class_route(&aliases, &after, "example/model-fast", false)
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(old.class_id, new.class_id);
    assert_eq!(new.upstream_model_name, "another-upstream");
}

#[sqlx::test]
async fn incomplete_activation_refuses_to_build_a_partial_route_view(pool: PgPool) {
    let (model, _) = add_fast_alias(&pool).await;
    sqlx::query("UPDATE deployed_models SET routing_mode='class_routes' WHERE id=$1")
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        crate::sync::onwards_config::load_targets_from_db(&pool, &[], false, &Default::default())
            .await
            .is_err()
    );
}

#[sqlx::test]
async fn discovery_lists_primary_classes_without_synonyms_or_inaccessible_models(pool: PgPool) {
    use crate::test::utils::{create_test_api_key_for_user, create_test_user};
    let (model, _) = active_model(&pool).await;
    let app = crate::Application::new_with_pool(crate::test::utils::create_test_config(), Some(pool.clone()), None)
        .await
        .unwrap();
    let (server, _background) = app.into_test_server();
    let user = create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
    let key = create_test_api_key_for_user(&pool, user.id).await;
    let response = server
        .get("/ai/v1/models")
        .add_header("Authorization", format!("Bearer {}", key.secret))
        .await;
    response.assert_status_ok();
    assert!(
        response.json::<serde_json::Value>()["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| !v["id"].as_str().unwrap().starts_with("example/"))
    );
    sqlx::query("INSERT INTO deployment_groups (deployment_id,group_id,granted_by) VALUES ($1,$2,$2)")
        .bind(model)
        .bind(Uuid::nil())
        .execute(&pool)
        .await
        .unwrap();
    let response = server
        .get("/ai/v1/models")
        .add_header("Authorization", format!("Bearer {}", key.secret))
        .await;
    response.assert_status_ok();
    let ids: Vec<String> = response.json::<serde_json::Value>()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_owned())
        .filter(|s| s.starts_with("example/"))
        .collect();
    assert_eq!(ids, ["example/model", "example/model:fast"]);
    sqlx::query("UPDATE deployed_models SET routing_mode='legacy' WHERE id=$1")
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();
    let response = server
        .get("/ai/v1/models")
        .add_header("Authorization", format!("Bearer {}", key.secret))
        .await;
    response.assert_status_ok();
    assert!(
        !response.json::<serde_json::Value>()["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["id"] == "example/model:fast")
    );
}

#[sqlx::test]
async fn class_requests_translate_once_keep_public_responses_and_preserve_deadline_priority(pool: PgPool) {
    use axum::http::StatusCode;
    use serde_json::json;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers};
    let upstream = MockServer::start().await;
    Mock::given(matchers::method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            ResponseTemplate::new(200).set_body_json(json!({
                "id":"test-response","object":"chat.completion","created":0,"model":body["model"],
                "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
            }))
        })
        .mount(&upstream)
        .await;
    active_model(&pool).await;
    sqlx::query("UPDATE inference_endpoints SET url=$1,kind='dynamo',accepts_scheduling_priority=true WHERE name='gateway'")
        .bind(upstream.uri())
        .execute(&pool)
        .await
        .unwrap();
    let mut config = crate::test::utils::create_test_config();
    config.onwards.strict_mode = true;
    config.background_services.onwards_sync.enabled = true;
    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None).await.unwrap();
    let (server, background) = app.into_test_server();
    background.sync_onwards_config(&pool).await.unwrap();
    let key: String =
        sqlx::query_scalar("SELECT secret FROM api_keys WHERE user_id='00000000-0000-0000-0000-000000000000' AND NOT is_deleted LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    for alias in ["example/model", "example/model:fast", "example/model-fast"] {
        let response = server
            .post("/ai/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {key}"))
            .json(&json!({"model":alias,"messages":[{"role":"user","content":"hello"}],"priority":999,
                "nvext":{"router":{"ttft_target":1},"agent_hints":{"priority":999}}}))
            .await;
        assert_eq!(response.status_code(), StatusCode::OK, "{}", response.text());
        assert_eq!(response.json::<serde_json::Value>()["model"], alias);
    }
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    for (request, expected) in requests
        .iter()
        .zip(["dynamo-example/throughput", "dynamo-example/fast", "dynamo-example/fast"])
    {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["model"], expected);
        assert_eq!(body["nvext"]["agent_hints"]["priority"], 0);
        assert!(body["nvext"]["router"].get("ttft_target").is_none());
    }
    let response = server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {key}"))
        .add_header("x-fusillade-request-id", Uuid::new_v4().to_string())
        .json(
            &json!({"model":"example/model-fast","messages":[{"role":"user","content":"queued"}],
            "nvext":{"agent_hints":{"priority":-42}}}),
        )
        .await;
    assert_eq!(response.status_code(), StatusCode::OK, "{}", response.text());
    let requests = upstream.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
    assert_eq!(body["model"], "dynamo-example/throughput");
    assert_eq!(body["nvext"]["agent_hints"]["priority"], -42);
}
