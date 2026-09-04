//! Integration tests for the Open Responses API lifecycle.
//!
//! Tests verify that:
//! - POST /v1/responses creates a row in fusillade's requests table
//! - POST /v1/chat/completions creates a row in fusillade's requests table
//! - GET /v1/responses/{id} retrieves the response
//! - Batch requests (with X-Fusillade-Request-Id) don't create duplicate rows

use crate::api::models::users::Role;
use std::collections::HashMap;

use crate::test::utils::{
    add_auth_headers, create_test_admin_user, create_test_api_key_for_user, create_test_config, create_test_user, setup_fusillade_pool,
};
use fusillade::{DaemonStorage, RetainedResponseArchiveCutoffs, RetentionPolicy, Storage};
use fusillade_arsenal::{PostgresRequestManager, TestDbPools};
use sqlx::PgPool;

/// Helper to set up a test app with a wiremock endpoint, model, API key, and
/// return the server + api_key ready for making AI requests.
async fn setup_ai_test(
    pool: PgPool,
    mock_server: &wiremock::MockServer,
    strict_mode: bool,
) -> (axum_test::TestServer, String, crate::BackgroundServices) {
    let mut config = create_test_config();
    config.onwards.strict_mode = strict_mode;
    config.background_services.onwards_sync.enabled = true;
    // 1 record per flush keeps the test deterministic: each completed
    // realtime response lands in fusillade immediately rather than
    // waiting for a buffer of N to accumulate.
    config.background_services.task_workers.response_writer_batch_size = 1;

    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("Failed to create application");
    let (server, bg_services) = app.into_test_server();

    let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
    let admin_headers = add_auth_headers(&admin_user);

    // Create endpoint pointing to mock server
    let endpoint_response = server
        .post("/admin/api/v1/endpoints")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "name": "test-endpoint",
            "url": mock_server.uri(),
            "auto_sync_models": false
        }))
        .await;
    let endpoint: serde_json::Value = endpoint_response.json();
    let endpoint_id = endpoint["id"].as_str().unwrap();

    // Create model
    let model_response = server
        .post("/admin/api/v1/models")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": "gpt-4o",
            "alias": "gpt-4o",
            "hosted_on": endpoint_id
        }))
        .await;
    let model: serde_json::Value = model_response.json();
    let deployment_id = model["id"].as_str().unwrap();

    // Assign model to default group
    let group_id = "00000000-0000-0000-0000-000000000000";
    server
        .post(&format!("/admin/api/v1/groups/{}/models/{}", group_id, deployment_id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;

    // Create user with credits
    let user = create_test_user(&pool, Role::StandardUser).await;
    server
        .post("/admin/api/v1/transactions")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "user_id": user.id,
            "transaction_type": "admin_grant",
            "amount": 1000,
            "source_id": admin_user.id
        }))
        .await;

    // Create API key
    let key_response = server
        .post(&format!("/admin/api/v1/users/{}/api-keys", user.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&serde_json::json!({
            "purpose": "realtime",
            "name": "Responses test key"
        }))
        .await;
    let key_data: serde_json::Value = key_response.json();
    let api_key = key_data["key"].as_str().unwrap().to_string();

    // Sync onwards config and wait for model availability
    bg_services.sync_onwards_config(&pool).await.unwrap();

    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(3) {
        let check = server
            .get("/ai/v1/models")
            .add_header("Authorization", &format!("Bearer {}", api_key))
            .await;
        if check.status_code() == 200 {
            let models: serde_json::Value = check.json();
            if let Some(data) = models["data"].as_array()
                && data.iter().any(|m| m["id"].as_str() == Some("gpt-4o"))
            {
                break;
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
    }

    (server, api_key, bg_services)
}

/// Mount a wiremock mock for chat completions
async fn mount_chat_completions_mock(mock_server: &wiremock::MockServer) {
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test123",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello from the test!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        })))
        .mount(mock_server)
        .await;
}

/// Test that POST /v1/chat/completions with service_tier=priority creates a fusillade row
/// and GET /v1/responses/{id} retrieves it.
#[sqlx::test]
#[test_log::test]
async fn test_chat_completion_creates_retrievable_response(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    mount_chat_completions_mock(&mock_server).await;

    let (server, api_key, _bg) = setup_ai_test(pool.clone(), &mock_server, true).await;

    // Make a chat completion request with priority tier (realtime)
    let response = server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}],
            "service_tier": "priority"
        }))
        .await;

    assert_eq!(response.status_code(), 200, "Chat completion should succeed");

    // The outlet handler runs asynchronously in a background task, so poll
    // until the row transitions from 'processing' to 'completed'.
    let start = std::time::Instant::now();
    let mut id = uuid::Uuid::nil();
    let mut final_state = String::new();
    while start.elapsed() < std::time::Duration::from_secs(5) {
        let row = sqlx::query(
            "SELECT id, state, model, batch_id FROM fusillade.requests WHERE model = 'gpt-4o' ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_optional(&pool)
        .await
        .unwrap();

        if let Some(row) = row {
            id = sqlx::Row::get(&row, "id");
            final_state = sqlx::Row::get::<String, _>(&row, "state");
            let batch_id: Option<uuid::Uuid> = sqlx::Row::get(&row, "batch_id");
            // Realtime responses are batchless: the row has no parent batch.
            assert!(batch_id.is_none(), "Realtime response should not have a batch_id");
            if final_state == "completed" {
                break;
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
    assert_eq!(final_state, "completed", "Request should reach completed state");

    // Now retrieve it via GET /v1/responses/{id}
    let response_id = format!("resp_{}", id);
    let retrieve_response = server
        .get(&format!("/ai/v1/responses/{}", response_id))
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .await;

    // Note: GET /responses/{id} is on the batches router which requires auth
    // but uses the same state. Check it returns a valid response.
    assert_eq!(retrieve_response.status_code(), 200, "GET /v1/responses/{{id}} should return 200");

    let body: serde_json::Value = retrieve_response.json();
    assert_eq!(body["id"].as_str(), Some(response_id.as_str()));
    assert_eq!(body["status"].as_str(), Some("completed"));
    assert_eq!(body["model"].as_str(), Some("gpt-4o"));
    assert_eq!(body["object"].as_str(), Some("response"));

    // Verify the response body was captured (not empty)
    let db_row = sqlx::query("SELECT length(response_body) as body_len FROM fusillade.requests WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let body_len: i32 = sqlx::Row::get(&db_row, "body_len");
    assert!(
        body_len > 0,
        "Response body should be captured by outlet handler, got length {body_len}"
    );
}

/// Test that the blocking response ID returned to the client matches the fusillade ID.
#[sqlx::test]
#[test_log::test]
async fn test_blocking_response_id_matches_fusillade_id(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    mount_chat_completions_mock(&mock_server).await;

    let (server, api_key, _bg) = setup_ai_test(pool.clone(), &mock_server, true).await;

    let response = server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hi"}],
            "service_tier": "priority"
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let body: serde_json::Value = response.json();
    let client_id = body["id"].as_str().unwrap();

    // The ID returned to the client should be a fusillade resp_ ID
    assert!(
        client_id.starts_with("resp_"),
        "Client should receive fusillade ID, got: {client_id}"
    );

    // And it should be retrievable
    let start = std::time::Instant::now();
    let mut found = false;
    while start.elapsed() < std::time::Duration::from_secs(5) {
        let retrieve = server
            .get(&format!("/ai/v1/responses/{}", client_id))
            .add_header("Authorization", &format!("Bearer {}", api_key))
            .await;
        if retrieve.status_code() == 200 {
            let r: serde_json::Value = retrieve.json();
            if r["status"].as_str() == Some("completed") {
                found = true;
                break;
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
    assert!(found, "Response should be retrievable by the client-facing ID");
}

/// POST `/ai/v1/responses` (realtime) then GET it back by the client-facing id.
///
/// Exercises the whole edge path with the translation layer sitting inner to the
/// outlet: the request is translated to Chat Completions, the upstream
/// completion is translated back into a Responses object stamped with the
/// tracking id, the outlet persists that Responses object, and the client's own
/// id resolves via GET. This is the POST-then-GET coverage the id/placement fix
/// turns on - at the broken placement the client received `resp_<upstream chat
/// id>` while the row was keyed by `resp_<uuid>`, so this GET 404'd.
#[sqlx::test]
#[test_log::test]
async fn test_responses_post_then_get_by_client_id(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    mount_chat_completions_mock(&mock_server).await;

    let (server, api_key, _bg) = setup_ai_test(pool.clone(), &mock_server, true).await;

    let response = server
        .post("/ai/v1/responses")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "input": "Hello"
        }))
        .await;

    assert_eq!(response.status_code(), 200, "POST /responses should succeed");
    let body: serde_json::Value = response.json();
    // The client got a Responses object, not a chat completion.
    assert_eq!(
        body["object"].as_str(),
        Some("response"),
        "expected a Responses object, got: {body}"
    );
    assert_eq!(body["status"].as_str(), Some("completed"));
    assert_eq!(body["output"][0]["content"][0]["text"].as_str(), Some("Hello from the test!"));
    let client_id = body["id"].as_str().unwrap().to_string();
    assert!(client_id.starts_with("resp_"), "id should be a resp_ tracking id, got: {client_id}");

    // The client's OWN id must resolve (poll until the outlet writes the row).
    let start = std::time::Instant::now();
    let mut found = false;
    while start.elapsed() < std::time::Duration::from_secs(5) {
        let retrieve = server
            .get(&format!("/ai/v1/responses/{}", client_id))
            .add_header("Authorization", &format!("Bearer {}", api_key))
            .await;
        if retrieve.status_code() == 200 {
            let r: serde_json::Value = retrieve.json();
            if r["status"].as_str() == Some("completed") {
                assert_eq!(r["object"].as_str(), Some("response"), "GET must return a Responses object");
                assert_eq!(r["id"].as_str(), Some(client_id.as_str()), "GET id must equal the client-facing id");
                found = true;
                break;
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
    assert!(found, "the client-facing /responses id must resolve via GET");
}

/// Test that GET /v1/responses/{id} returns 404 for non-existent IDs.
#[sqlx::test]
#[test_log::test]
async fn test_get_response_returns_404_for_unknown_id(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    mount_chat_completions_mock(&mock_server).await;

    let (server, api_key, _bg) = setup_ai_test(pool.clone(), &mock_server, true).await;

    let fake_id = format!("resp_{}", uuid::Uuid::new_v4());
    let response = server
        .get(&format!("/ai/v1/responses/{}", fake_id))
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .await;

    assert_eq!(response.status_code(), 404);
}

async fn archive_response_graphs(pool: &PgPool, max_groups: i64) {
    let fusillade_pool = setup_fusillade_pool(pool).await;
    sqlx::query(
        "CREATE INDEX idx_requests_batchless_retention_due ON requests (service_tier, (CASE state WHEN 'completed' THEN completed_at WHEN 'failed' THEN failed_at WHEN 'canceled' THEN canceled_at END), id) WHERE batch_id IS NULL AND state IN ('completed', 'failed', 'canceled')",
    )
    .execute(&fusillade_pool)
    .await
    .unwrap();
    sqlx::query("SELECT ensure_retained_response_partition('2026-08-02', NULL)")
        .execute(&fusillade_pool)
        .await
        .unwrap();
    let manager = PostgresRequestManager::new(
        TestDbPools::new(fusillade_pool).await.unwrap(),
        fusillade_arsenal::PostgresStorageConfig::default(),
    )
    .with_retained_response_fence_seconds(Some(3_600));
    let observed_at = chrono::DateTime::parse_from_rfc3339("2026-08-01T12:00:00Z").unwrap().to_utc();
    let terminal_before = chrono::DateTime::parse_from_rfc3339("2026-08-01T10:00:00Z").unwrap().to_utc();
    let cutoffs = RetainedResponseArchiveCutoffs::new(observed_at, terminal_before, observed_at).unwrap();
    manager
        .archive_terminal_batchless_responses(
            &RetentionPolicy {
                batchless_seconds_by_service_tier: HashMap::from([("priority".to_owned(), 1)]),
                max_late_writer_seconds: Some(3_600),
                ..Default::default()
            },
            &cutoffs,
            max_groups,
            i64::MAX,
        )
        .await
        .expect("response graph must move into retained storage");
}

async fn response_lifecycle_manager(pool: &PgPool) -> (PgPool, PostgresRequestManager<TestDbPools>) {
    let fusillade_pool = setup_fusillade_pool(pool).await;
    let manager = PostgresRequestManager::new(
        TestDbPools::new(fusillade_pool.clone()).await.unwrap(),
        fusillade_arsenal::PostgresStorageConfig::default(),
    )
    .with_retained_response_fence_seconds(Some(3_600));
    (fusillade_pool, manager)
}

fn synthetic_context<'a>(request_id: uuid::Uuid, api_key: &'a str) -> crate::inference::store::CreateContext<'a> {
    crate::inference::store::CreateContext {
        request_id,
        request_body: r#"{"prompt":"must_not_leak"}"#,
        model: "store-test-model",
        endpoint: "/v1/responses",
        base_url: "https://example.invalid",
        api_key: Some(api_key),
    }
}

async fn seed_archivable_response(pool: &PgPool, manager: &PostgresRequestManager<TestDbPools>, request_id: uuid::Uuid, owner: &str) {
    manager
        .create_realtime(fusillade::CreateRealtimeInput {
            request_id,
            body: r#"{"prompt":"archived"}"#.to_owned(),
            model: "store-test-model".to_owned(),
            endpoint: "https://example.invalid".to_owned(),
            method: "POST".to_owned(),
            path: "/v1/responses".to_owned(),
            api_key: "store-test-key".to_owned(),
            created_by: owner.to_owned(),
        })
        .await
        .unwrap();
    manager
        .complete_request(fusillade::RequestId(request_id), r#"{"answer":"archived"}"#, 200)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE fusillade.requests SET service_tier = 'priority', \
         created_at = '2026-08-01 08:00:00Z', claimed_at = '2026-08-01 09:58:00Z', \
         started_at = '2026-08-01 09:59:00Z', completed_at = '2026-08-01 10:00:00Z', \
         updated_at = '2026-08-01 10:00:00Z' WHERE id = $1",
    )
    .bind(request_id)
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test]
async fn complete_response_treats_active_retained_graph_as_idempotent(pool: PgPool) {
    let user = create_test_user(&pool, Role::StandardUser).await;
    let key = create_test_api_key_for_user(&pool, user.id).await;
    let (fusillade_pool, manager) = response_lifecycle_manager(&pool).await;
    let request_id = uuid::Uuid::new_v4();
    seed_archivable_response(&pool, &manager, request_id, &user.id.to_string()).await;
    archive_response_graphs(&pool, 1).await;

    crate::inference::store::complete_response_idempotent(
        &manager,
        &pool,
        &format!("resp_{request_id}"),
        r#"{"must_not":"overwrite"}"#,
        200,
        synthetic_context(request_id, &key.secret),
    )
    .await
    .expect("an active retained response is an idempotent terminal write");

    let retained: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM retained_response_request_routes WHERE request_id = $1")
        .bind(request_id)
        .fetch_one(&fusillade_pool)
        .await
        .unwrap();
    assert_eq!(retained, 1);
}

#[sqlx::test]
async fn complete_response_fails_closed_for_erased_identity_without_synthesis(pool: PgPool) {
    let user = create_test_user(&pool, Role::StandardUser).await;
    let key = create_test_api_key_for_user(&pool, user.id).await;
    let (fusillade_pool, manager) = response_lifecycle_manager(&pool).await;
    let request_id = uuid::Uuid::new_v4();
    seed_archivable_response(&pool, &manager, request_id, &user.id.to_string()).await;
    archive_response_graphs(&pool, 1).await;
    manager.delete_response_group(request_id).await.unwrap();

    let error = crate::inference::store::complete_response_idempotent(
        &manager,
        &pool,
        &format!("resp_{request_id}"),
        r#"{"must_not":"survive"}"#,
        200,
        synthetic_context(request_id, &key.secret),
    )
    .await
    .expect_err("an erased identity must terminate without synthesis");
    assert_eq!(error.to_string(), "Response not found: Response unavailable");
    assert!(!format!("{error:?}").contains("must_not"));
    let live: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM requests WHERE id = $1")
        .bind(request_id)
        .fetch_one(&fusillade_pool)
        .await
        .unwrap();
    assert_eq!(live, 0);
    let leaked_templates: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_templates WHERE body LIKE '%must_not_leak%'")
        .fetch_one(&fusillade_pool)
        .await
        .unwrap();
    assert_eq!(leaked_templates, 0);
}

async fn wait_for_api_key_lock_waiter(pool: &PgPool) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS ( \
                 SELECT 1 FROM pg_locks lock \
                 JOIN pg_class relation ON relation.oid = lock.relation \
                 WHERE relation.relname = 'api_keys' AND NOT lock.granted)",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the attribution lookup must reach the deterministic table-lock gate");
}

#[sqlx::test]
async fn complete_response_rechecks_a_fence_created_after_the_initial_miss(pool: PgPool) {
    let user = create_test_user(&pool, Role::StandardUser).await;
    let key = create_test_api_key_for_user(&pool, user.id).await;
    let (fusillade_pool, manager) = response_lifecycle_manager(&pool).await;
    let request_id = uuid::Uuid::new_v4();
    let mut gate = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE api_keys IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *gate)
        .await
        .unwrap();
    let task_pool = pool.clone();
    let key_secret = key.secret;
    let mut completion = tokio::spawn(async move {
        crate::inference::store::complete_response_idempotent(
            &manager,
            &task_pool,
            &format!("resp_{request_id}"),
            r#"{"must_not":"survive"}"#,
            200,
            synthetic_context(request_id, &key_secret),
        )
        .await
    });
    tokio::select! {
        result = &mut completion => panic!("completion passed the attribution gate before fencing: {result:?}"),
        () = wait_for_api_key_lock_waiter(&pool) => {}
    }
    sqlx::query(
        "INSERT INTO retained_response_resurrection_fences \
         (object_id, reason, expires_at) VALUES ($1, 'erased', NOW() + INTERVAL '1 hour')",
    )
    .bind(request_id)
    .execute(&fusillade_pool)
    .await
    .unwrap();
    gate.commit().await.unwrap();

    let error = completion
        .await
        .unwrap()
        .expect_err("the post-miss synthesis must recheck the new fence");
    assert_eq!(error.to_string(), "Response not found: Response unavailable");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM requests WHERE id = $1")
            .bind(request_id)
            .fetch_one(&fusillade_pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM request_templates WHERE body LIKE '%must_not_leak%'",)
            .fetch_one(&fusillade_pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test]
async fn complete_response_does_not_ignore_an_unproven_synthetic_create_error(pool: PgPool) {
    let user = create_test_user(&pool, Role::StandardUser).await;
    let key = create_test_api_key_for_user(&pool, user.id).await;
    let (fusillade_pool, manager) = response_lifecycle_manager(&pool).await;
    sqlx::query(
        r#"
        CREATE FUNCTION reject_synthetic_template() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            RAISE EXCEPTION 'must_not_leak_database_detail';
        END
        $$
        "#,
    )
    .execute(&fusillade_pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_synthetic_template BEFORE INSERT ON request_templates \
         FOR EACH ROW EXECUTE FUNCTION reject_synthetic_template()",
    )
    .execute(&fusillade_pool)
    .await
    .unwrap();
    let request_id = uuid::Uuid::new_v4();

    let error = crate::inference::store::complete_response_idempotent(
        &manager,
        &pool,
        &format!("resp_{request_id}"),
        r#"{"must_not":"survive"}"#,
        200,
        synthetic_context(request_id, &key.secret),
    )
    .await
    .expect_err("a failed create without a concurrent winner must be returned");
    assert_eq!(error.to_string(), "Storage error: Failed to synthesize response row");
    let rendered = format!("{error:?}");
    assert!(!rendered.contains("must_not"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM requests WHERE id = $1")
            .bind(request_id)
            .fetch_one(&fusillade_pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test]
#[test_log::test]
async fn a_chain_can_extend_a_response_after_its_graph_moved_to_the_retained_store(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    mount_chat_completions_mock(&mock_server).await;
    let (server, api_key, _bg) = setup_ai_test(pool.clone(), &mock_server, true).await;
    server
        .post("/ai/v1/responses")
        .add_header("Authorization", &format!("Bearer {api_key}"))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({"model": "gpt-4o", "input": "first turn", "service_tier": "priority"}))
        .await
        .assert_status_ok();
    let id = poll_completed_row(&pool, uuid::Uuid::nil()).await;
    sqlx::query(
        "UPDATE fusillade.requests SET created_at = '2026-08-01 08:00:00Z', claimed_at = '2026-08-01 09:58:00Z', started_at = '2026-08-01 09:59:00Z', completed_at = '2026-08-01 10:00:00Z', updated_at = '2026-08-01 10:00:00Z' WHERE id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    // Extension works while the first turn is live...
    server
        .post("/ai/v1/responses")
        .add_header("Authorization", &format!("Bearer {api_key}"))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "input": "second turn while live",
            "previous_response_id": format!("resp_{id}"),
            "service_tier": "priority"
        }))
        .await
        .assert_status_ok();
    // ...and must keep working once its graph has moved to the retained store.
    archive_response_graphs(&pool, 1).await;
    let live: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fusillade.requests WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(live, 0, "the first turn must have moved to the retained store");

    let extended = server
        .post("/ai/v1/responses")
        .add_header("Authorization", &format!("Bearer {api_key}"))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "input": "second turn",
            "previous_response_id": format!("resp_{id}"),
            "service_tier": "priority"
        }))
        .await;
    extended.assert_status_ok();
    let extended_json: serde_json::Value = extended.json();
    assert_eq!(extended_json["previous_response_id"], format!("resp_{id}"));
    assert_ne!(extended_json["id"], format!("resp_{id}"));
}

#[sqlx::test]
#[test_log::test]
async fn previous_response_id_of_another_users_response_is_rejected(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    mount_chat_completions_mock(&mock_server).await;
    let (server, api_key, _bg) = setup_ai_test(pool.clone(), &mock_server, true).await;
    server
        .post("/ai/v1/responses")
        .add_header("Authorization", &format!("Bearer {api_key}"))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({"model": "gpt-4o", "input": "private first turn", "service_tier": "priority"}))
        .await
        .assert_status_ok();
    let id = poll_completed_row(&pool, uuid::Uuid::nil()).await;
    let other_user = create_test_user(&pool, Role::StandardUser).await;
    let other_api_key = format!("sk-foreign-chain-{}", uuid::Uuid::new_v4());
    sqlx::query("INSERT INTO api_keys (id, name, secret, purpose, user_id, created_by) VALUES ($1, $2, $3, 'realtime', $4, $4)")
        .bind(uuid::Uuid::new_v4())
        .bind("Foreign chain test key")
        .bind(&other_api_key)
        .bind(other_user.id)
        .execute(&pool)
        .await
        .unwrap();
    // The other user needs model access too, so a rejection is about the
    // previous response and not about the model.
    let extend = |key: String| {
        let server = &server;
        async move {
            server
                .post("/ai/v1/responses")
                .add_header("Authorization", &format!("Bearer {key}"))
                .add_header("Content-Type", "application/json")
                .json(&serde_json::json!({
                    "model": "gpt-4o",
                    "input": "second turn",
                    "previous_response_id": format!("resp_{id}"),
                    "service_tier": "priority"
                }))
                .await
        }
    };
    let foreign = extend(other_api_key.clone()).await;
    assert_eq!(foreign.status_code(), axum::http::StatusCode::BAD_REQUEST, "{}", foreign.text());
    assert!(foreign.text().contains("previous response not found"));
    // The first turn itself carried the text upstream once; the foreign
    // extension must not have sent it again.
    let private_text_sent_upstream = mock_server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| String::from_utf8_lossy(&r.body).contains("private first turn"))
        .count();
    assert_eq!(private_text_sent_upstream, 1, "another user's turn must never reach upstream");

    // After the graph moves to the retained store the answer is the same.
    sqlx::query(
        "UPDATE fusillade.requests SET created_at = '2026-08-01 08:00:00Z', claimed_at = '2026-08-01 09:58:00Z', started_at = '2026-08-01 09:59:00Z', completed_at = '2026-08-01 10:00:00Z', updated_at = '2026-08-01 10:00:00Z' WHERE id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    archive_response_graphs(&pool, 1).await;
    let foreign_after = extend(other_api_key).await;
    assert_eq!(foreign_after.status_code(), axum::http::StatusCode::BAD_REQUEST);
    extend(api_key.clone()).await.assert_status_ok();
}

#[sqlx::test]
#[test_log::test]
async fn read_retained_singleton_preserves_response_and_fails_closed_after_drop(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    mount_chat_completions_mock(&mock_server).await;
    let (server, api_key, _bg) = setup_ai_test(pool.clone(), &mock_server, true).await;

    server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", &format!("Bearer {api_key}"))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "retained singleton"}],
            "service_tier": "priority"
        }))
        .await
        .assert_status_ok();
    let id = poll_completed_row(&pool, uuid::Uuid::nil()).await;
    sqlx::query(
        "UPDATE fusillade.requests SET created_at = '2026-08-01 08:00:00Z', claimed_at = '2026-08-01 09:58:00Z', started_at = '2026-08-01 09:59:00Z', completed_at = '2026-08-01 10:00:00Z', updated_at = '2026-08-01 10:00:00Z' WHERE id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();

    let response_id = format!("resp_{id}");
    let before = server
        .get(&format!("/ai/v1/responses/{response_id}"))
        .add_header("Authorization", &format!("Bearer {api_key}"))
        .await;
    before.assert_status_ok();
    let before_json: serde_json::Value = before.json();

    archive_response_graphs(&pool, 1).await;
    let after = server
        .get(&format!("/ai/v1/responses/{response_id}"))
        .add_header("Authorization", &format!("Bearer {api_key}"))
        .await;
    after.assert_status_ok();
    assert_eq!(after.json::<serde_json::Value>(), before_json);

    sqlx::query("UPDATE fusillade.retained_response_buckets SET state = 'retiring' WHERE delete_on = '2026-08-02'")
        .execute(&pool)
        .await
        .unwrap();
    let retiring = server
        .get(&format!("/ai/v1/responses/{response_id}"))
        .add_header("Authorization", &format!("Bearer {api_key}"))
        .await;
    retiring.assert_status_not_found();

    sqlx::query("ALTER TABLE fusillade.retained_response_objects DETACH PARTITION fusillade.retained_response_objects_d20260802")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE fusillade.retained_response_objects_d20260802")
        .execute(&pool)
        .await
        .unwrap();
    let dropped = server
        .get(&format!("/ai/v1/responses/{response_id}"))
        .add_header("Authorization", &format!("Bearer {api_key}"))
        .await;
    dropped.assert_status_not_found();
}

#[sqlx::test]
#[test_log::test]
async fn read_retained_response_preserves_public_id_and_owner(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    mount_chat_completions_mock(&mock_server).await;
    let (server, api_key, _bg) = setup_ai_test(pool.clone(), &mock_server, true).await;

    server
        .post("/ai/v1/responses")
        .add_header("Authorization", &format!("Bearer {api_key}"))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({"model": "gpt-4o", "input": "retained response"}))
        .await
        .assert_status_ok();
    let request_id = poll_completed_row(&pool, uuid::Uuid::nil()).await;
    sqlx::query(
        "UPDATE fusillade.requests SET created_at = '2026-08-01 08:00:00Z', claimed_at = '2026-08-01 09:58:00Z', started_at = '2026-08-01 09:59:00Z', completed_at = '2026-08-01 10:00:00Z', updated_at = '2026-08-01 10:00:00Z' WHERE id = $1",
    )
    .bind(request_id)
    .execute(&pool)
    .await
    .unwrap();

    let response_id = format!("resp_{request_id}");
    let before = server
        .get(&format!("/ai/v1/responses/{response_id}"))
        .add_header("Authorization", &format!("Bearer {api_key}"))
        .await;
    before.assert_status_ok();
    let before_json: serde_json::Value = before.json();
    assert_eq!(before_json["id"], response_id);
    let other_user = create_test_user(&pool, Role::StandardUser).await;
    let other_api_key = format!("sk-retained-owner-{}", uuid::Uuid::new_v4());
    sqlx::query("INSERT INTO api_keys (id, name, secret, purpose, user_id, created_by) VALUES ($1, $2, $3, 'realtime', $4, $4)")
        .bind(uuid::Uuid::new_v4())
        .bind("Retained response ownership test key")
        .bind(&other_api_key)
        .bind(other_user.id)
        .execute(&pool)
        .await
        .unwrap();
    let unauthorized_before = server
        .get(&format!("/ai/v1/responses/{response_id}"))
        .add_header("Authorization", &format!("Bearer {other_api_key}"))
        .await;
    unauthorized_before.assert_status_not_found();

    archive_response_graphs(&pool, 1).await;
    let after = server
        .get(&format!("/ai/v1/responses/{response_id}"))
        .add_header("Authorization", &format!("Bearer {api_key}"))
        .await;
    after.assert_status_ok();
    let after_json: serde_json::Value = after.json();
    assert_eq!(after_json, before_json);
    assert_eq!(after_json["id"], response_id);
    let unauthorized_after = server
        .get(&format!("/ai/v1/responses/{response_id}"))
        .add_header("Authorization", &format!("Bearer {other_api_key}"))
        .await;
    unauthorized_after.assert_status_not_found();
}

/// Test that requests with X-Fusillade-Request-Id header don't create
/// duplicate rows (batch deduplication).
#[sqlx::test]
#[test_log::test]
async fn test_fusillade_header_skips_row_creation(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    mount_chat_completions_mock(&mock_server).await;

    let (server, api_key, _bg) = setup_ai_test(pool.clone(), &mock_server, true).await;

    // Count existing rows
    let before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM fusillade.requests")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Make a request WITH the fusillade header (simulating a batch daemon request)
    let _response = server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .add_header("Content-Type", "application/json")
        .add_header("X-Fusillade-Request-Id", &uuid::Uuid::new_v4().to_string())
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello from batch"}]
        }))
        .await;

    // Count rows after — should be the same (no new row created)
    let after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM fusillade.requests")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(
        before.0, after.0,
        "Requests with X-Fusillade-Request-Id should not create new fusillade rows"
    );
}

/// DELETE /ai/v1/responses/{id} hard-deletes the underlying fusillade row,
/// and a subsequent GET returns 404.
#[sqlx::test]
#[test_log::test]
async fn test_delete_response_removes_fusillade_row(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    mount_chat_completions_mock(&mock_server).await;

    let (server, api_key, _bg) = setup_ai_test(pool.clone(), &mock_server, true).await;

    // Create a chat completion (priority/realtime tier → batchless fusillade row).
    server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}],
            "service_tier": "priority"
        }))
        .await;

    // Poll for the row to reach completed (outlet writes asynchronously).
    let start = std::time::Instant::now();
    let mut id = uuid::Uuid::nil();
    while start.elapsed() < std::time::Duration::from_secs(5) {
        if let Some(row) = sqlx::query("SELECT id, state FROM fusillade.requests WHERE model = 'gpt-4o' ORDER BY created_at DESC LIMIT 1")
            .fetch_optional(&pool)
            .await
            .unwrap()
            && sqlx::Row::get::<String, _>(&row, "state") == "completed"
        {
            id = sqlx::Row::get(&row, "id");
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
    assert_ne!(id, uuid::Uuid::nil(), "row should reach completed state");

    // Denormalized analytics and billing records are intentionally outside
    // the response-content graph and must survive right-to-erasure deletion.
    let analytics_id: i64 = sqlx::query_scalar(
        "INSERT INTO http_analytics \
         (instance_id, correlation_id, timestamp, method, uri, fusillade_request_id) \
         VALUES ($1, 1, NOW(), 'POST', '/ai/v1/responses', $2) RETURNING id",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let owner_id: uuid::Uuid = sqlx::query_scalar("SELECT user_id FROM api_keys WHERE secret = $1")
        .bind(&api_key)
        .fetch_one(&pool)
        .await
        .unwrap();
    let billing_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO credits_transactions \
         (id, user_id, transaction_type, amount, source_id, balance_after, fusillade_request_id) \
         VALUES ($1, $2, 'usage', 0.01, $3, 0, $4)",
    )
    .bind(billing_id)
    .bind(owner_id)
    .bind(format!("response-erasure-fixture-{billing_id}"))
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();

    // DELETE the response — spec returns 200 with {id, object: "response",
    // deleted: true}.
    let response_id = format!("resp_{}", id);
    let resp = server
        .delete(&format!("/ai/v1/responses/{}", response_id))
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["id"].as_str(), Some(response_id.as_str()));
    assert_eq!(body["object"].as_str(), Some("response"));
    assert_eq!(body["deleted"].as_bool(), Some(true));

    // Row is gone from fusillade.requests.
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM fusillade.requests WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 0, "fusillade row should be hard-deleted");
    let analytics_survives: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM http_analytics WHERE id = $1)")
        .bind(analytics_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let billing_survives: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM credits_transactions WHERE id = $1)")
        .bind(billing_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(analytics_survives, "analytics fixture must be preserved");
    assert!(billing_survives, "billing fixture must be preserved");

    // GET now returns 404.
    let get = server
        .get(&format!("/ai/v1/responses/{}", response_id))
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .await;
    assert_eq!(get.status_code(), 404);
}

/// DELETE /ai/v1/responses/{id} returns 404 for a non-existent id.
#[sqlx::test]
#[test_log::test]
async fn test_delete_response_404_for_unknown_id(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    mount_chat_completions_mock(&mock_server).await;

    let (server, api_key, _bg) = setup_ai_test(pool.clone(), &mock_server, true).await;

    let fake_id = format!("resp_{}", uuid::Uuid::new_v4());
    let resp = server
        .delete(&format!("/ai/v1/responses/{}", fake_id))
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .await;
    assert_eq!(resp.status_code(), 404);
}

/// Flip the account that owns `api_key` to zero-data-retention, then refresh
/// the middleware's per-key cache so `is_zdr_request` sees it. The cache handle
/// is shared, so this takes effect on the live server immediately.
async fn enable_zdr_for_key(pool: &PgPool, bg: &crate::BackgroundServices, api_key: &str) {
    sqlx::query(
        "UPDATE users SET zero_data_retention = true \
         WHERE id = (SELECT user_id FROM api_keys WHERE secret = $1)",
    )
    .bind(api_key)
    .execute(pool)
    .await
    .unwrap();
    bg.sync_zdr_keys(pool).await.unwrap();
}

/// Poll for the newest completed gpt-4o realtime row whose id is not `exclude`
/// (pass `Uuid::nil()` to accept any). The outlet handler writes asynchronously.
async fn poll_completed_row(pool: &PgPool, exclude: uuid::Uuid) -> uuid::Uuid {
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(5) {
        let row = sqlx::query(
            "SELECT id FROM fusillade.requests \
             WHERE model = 'gpt-4o' AND state = 'completed' AND id <> $1 \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(exclude)
        .fetch_optional(pool)
        .await
        .unwrap();
        if let Some(row) = row {
            return sqlx::Row::get(&row, "id");
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
    panic!("no completed fusillade row (excluding {exclude}) within 5s");
}

async fn response_body_len(pool: &PgPool, id: uuid::Uuid) -> i32 {
    sqlx::query_scalar("SELECT length(coalesce(response_body, '')) FROM fusillade.requests WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn request_body_len(pool: &PgPool, id: uuid::Uuid) -> i32 {
    sqlx::query_scalar(
        "SELECT length(coalesce(t.body, '')) \
         FROM fusillade.requests r JOIN fusillade.request_templates t ON t.id = r.template_id \
         WHERE r.id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A ZDR realtime request is non-persistence: it still round-trips and records a
/// completion row, but the request and response bodies are suppressed at rest
/// (blank in fusillade), while an identical non-ZDR request stores them.
#[sqlx::test]
#[test_log::test]
async fn test_realtime_zdr_suppresses_stored_bodies(pool: PgPool) {
    let mock_server = wiremock::MockServer::start().await;
    mount_chat_completions_mock(&mock_server).await;

    let (server, api_key, bg) = setup_ai_test(pool.clone(), &mock_server, true).await;

    // Control (account NOT yet ZDR): the body IS stored. This makes the ZDR
    // blank below a real negative, not a dead pipeline.
    let control = server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "control marker"}],
            "service_tier": "priority"
        }))
        .await;
    assert_eq!(control.status_code(), 200);
    let control_id = poll_completed_row(&pool, uuid::Uuid::nil()).await;
    assert!(
        response_body_len(&pool, control_id).await > 0,
        "non-ZDR response body should be stored"
    );
    assert!(
        request_body_len(&pool, control_id).await > 0,
        "non-ZDR request body should be stored"
    );

    // Now the account is ZDR.
    enable_zdr_for_key(&pool, &bg, &api_key).await;

    let zdr = server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", &format!("Bearer {}", api_key))
        .add_header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "secret zdr marker"}],
            "service_tier": "priority"
        }))
        .await;
    assert_eq!(zdr.status_code(), 200, "ZDR realtime request should still round-trip");

    // A fresh completion row (not the control) with both bodies blanked.
    let zdr_id = poll_completed_row(&pool, control_id).await;
    assert_ne!(zdr_id, control_id);
    assert_eq!(
        response_body_len(&pool, zdr_id).await,
        0,
        "ZDR realtime response_body must be blank at rest"
    );
    assert_eq!(
        request_body_len(&pool, zdr_id).await,
        0,
        "ZDR realtime request_templates.body must be blank at rest"
    );
}
