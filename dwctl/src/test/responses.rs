//! Integration tests for the Open Responses API lifecycle.
//!
//! Tests verify that:
//! - POST /v1/responses creates a row in fusillade's requests table
//! - POST /v1/chat/completions creates a row in fusillade's requests table
//! - GET /v1/responses/{id} retrieves the response
//! - Batch requests (with X-Fusillade-Request-Id) don't create duplicate rows

use crate::api::models::users::Role;
use crate::test::utils::{add_auth_headers, create_test_admin_user, create_test_config, create_test_user};
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
    config.background_services.task_workers.response_workers = 1;

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
            "hosted_on": endpoint_id,
            "open_responses_adapter": true
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
            if let Some(data) = models["data"].as_array() {
                if data.iter().any(|m| m["id"].as_str() == Some("gpt-4o")) {
                    break;
                }
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
            // Realtime requests are now tracked via a single-request batch created
            // by the create-response underway job — the row must have a batch_id.
            assert!(batch_id.is_some(), "Realtime request should be tracked via a single-request batch");
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

/// A multi-step `/v1/responses` chain — head model_call, server-side
/// tool_call, summarizing model_call — assembles into the OpenAI
/// Response shape and retrieves cleanly via GET /v1/responses/{id}.
///
/// The warm path runs the loop in-process and fires per-step model_calls
/// over HTTP loopback, which the axum_test in-memory transport doesn't
/// expose — so we drive the bridge primitives directly from the test.
/// Same code path the loop drives in production (record_step →
/// complete_step → finalize_head_request → get_response), just without
/// the loop wrapper. End-to-end POST→GET coverage of the warm path
/// itself lives under the loopback-listener integration suite (out of
/// scope for unit tests).
#[sqlx::test]
#[test_log::test]
async fn test_multi_step_chain_assembles_and_is_retrievable_via_get(pool: PgPool) {
    use crate::responses::store::{FusilladeResponseStore, PendingResponseInput};
    use crate::test::utils::setup_fusillade_pool;
    use fusillade::{PostgresRequestManager, PostgresResponseStepManager, ReqwestHttpClient, TestDbPools};
    use onwards::{MultiStepStore, StepDescriptor, StepKind as OnwardsStepKind};
    use serde_json::json;
    use std::sync::Arc;

    let pool = setup_fusillade_pool(&pool).await;
    let test_pools = TestDbPools::new(pool).await.unwrap();
    let request_manager = Arc::new(PostgresRequestManager::<_, ReqwestHttpClient>::new(
        test_pools.clone(),
        Default::default(),
    ));
    let step_manager = Arc::new(PostgresResponseStepManager::new(test_pools));
    let store = FusilladeResponseStore::new(request_manager).with_step_manager(step_manager);

    // Stand in for warm_path_setup — register the user request body
    // and reserve a head step uuid as the response identity.
    let head_uuid = store.register_pending(PendingResponseInput {
        body: json!({"model": "gpt-4o", "input": "weather in Paris?"}).to_string(),
        api_key: None,
        created_by: Some("test-user".to_string()),
        base_url: "http://upstream-mock".to_string(),
    });
    let request_id = head_uuid.to_string();

    // Step 1: head model_call returns a tool_call.
    let head_descriptor = StepDescriptor {
        kind: OnwardsStepKind::ModelCall,
        request_payload: json!({
            "model": "gpt-4o",
            "messages": [{"role":"user","content":"weather in Paris?"}],
        }),
    };
    let head = MultiStepStore::record_step(&store, &request_id, None, None, &head_descriptor)
        .await
        .unwrap();
    MultiStepStore::mark_step_processing(&store, &head.id).await.unwrap();
    MultiStepStore::complete_step(
        &store,
        &head.id,
        &json!({
            "choices": [{
                "message": {
                    "role":"assistant",
                    "tool_calls":[{
                        "id":"call_1",
                        "type":"function",
                        "function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}
                    }]
                },
                "finish_reason":"tool_calls"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5},
        }),
    )
    .await
    .unwrap();

    // Step 2: tool_call (server-side dispatch).
    let tool_descriptor = StepDescriptor {
        kind: OnwardsStepKind::ToolCall,
        request_payload: json!({"name":"get_weather","args":{"city":"Paris"},"call_id":"call_1"}),
    };
    let tool = MultiStepStore::record_step(&store, &request_id, None, Some(&head.id), &tool_descriptor)
        .await
        .unwrap();
    MultiStepStore::mark_step_processing(&store, &tool.id).await.unwrap();
    MultiStepStore::complete_step(&store, &tool.id, &json!({"temp_c": 18, "condition": "cloudy"}))
        .await
        .unwrap();

    // Step 3: summarizing model_call returns final assistant text.
    let summary_descriptor = StepDescriptor {
        kind: OnwardsStepKind::ModelCall,
        request_payload: json!({
            "model": "gpt-4o",
            "messages": [
                {"role":"user","content":"weather in Paris?"},
                {"role":"tool","tool_call_id":"call_1","content":"{\"temp_c\":18}"},
            ],
        }),
    };
    let summary = MultiStepStore::record_step(&store, &request_id, None, Some(&tool.id), &summary_descriptor)
        .await
        .unwrap();
    MultiStepStore::mark_step_processing(&store, &summary.id).await.unwrap();
    MultiStepStore::complete_step(
        &store,
        &summary.id,
        &json!({
            "choices": [{
                "message": {"role":"assistant","content":"It's 18°C and cloudy in Paris."},
                "finish_reason":"stop"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 8},
        }),
    )
    .await
    .unwrap();

    // Finalize like the warm path does on Ok exit: assemble the chain
    // and stamp the result onto the head step's sub-request fusillade
    // row so GET retrieval surfaces a completed response.
    let assembled = MultiStepStore::assemble_response(&store, &request_id).await.unwrap();
    store.finalize_head_request(&request_id, 200, assembled).await.unwrap();

    // GET retrieval: the response is completed, the id matches the head
    // step (not any internal sub-request id), and the chain is listable.
    let resp_id = format!("resp_{request_id}");
    let response = store.get_response(&resp_id).await.unwrap().expect("response should be retrievable");
    assert_eq!(response["id"].as_str(), Some(resp_id.as_str()));
    assert_eq!(response["status"].as_str(), Some("completed"));

    let chain = MultiStepStore::list_chain(&store, &request_id, None).await.unwrap();
    assert_eq!(chain.len(), 3, "chain should have head + tool + summary");
    assert_eq!(chain[0].id, head.id);
    assert_eq!(chain[1].id, tool.id);
    assert_eq!(chain[2].id, summary.id);
    assert!(matches!(chain[0].kind, onwards::StepKind::ModelCall));
    assert!(matches!(chain[1].kind, onwards::StepKind::ToolCall));
    assert!(matches!(chain[2].kind, onwards::StepKind::ModelCall));
    assert!(chain.iter().all(|s| matches!(s.state, onwards::StepState::Completed)));
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

// Removed: `test_flex_background_returns_202_with_queued_status`,
// `test_flex_blocking_waits_for_completion`,
// `test_priority_background_completes_and_is_retrievable`.
//
// All three asserted on the pre-warm-path routing where flex went
// through the fusillade daemon (returning `status=queued`) and
// background invoked the realtime path with daemon polling. Since
// commit `6a7c24d7` every `/v1/responses` engages the multi-step warm
// path regardless of tier or background flag — the daemon-driven and
// queued/in_progress assertions don't model current behavior. End-to-
// end coverage of the warm path itself needs a real HTTP loopback
// listener (the axum_test in-memory transport doesn't bind a port);
// that integration suite is tracked under COR-349 (daemon-path
// rewrite) and the loopback fixture follow-up. Bridge-primitive
// coverage for the multi-step path lives in
// `test_multi_step_chain_assembles_and_is_retrievable_via_get` above
// and `test::multi_step_executor::loop_drives_real_tool_and_model_calls_through_production_executor`.
