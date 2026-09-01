//! Integration tests for the response_steps storage layer.
//!
//! Each test gets a fresh sqlx pool via `#[sqlx::test]`, which automatically
//! runs every migration in `migrations/`. We insert a minimal request row by
//! hand (avoiding the full file/template/batch chain) since these tests are
//! scoped to the response_step module only.
//!
//! The new semantics (see plan
//! `docs/plans/2026-04-30-response-steps-request-linkage.md`):
//!
//! - `response_steps.request_id` is `Option<RequestId>` and points at the
//!   sub-request fusillade row created for *this* step's HTTP fire (one
//!   row per `model_call`); `None` for `tool_call` steps.
//! - `parent_step_id` points at the head (root) step of the chain;
//!   `None` only on the head itself.
//! - `prev_step_id` is a tree edge — multiple steps may share one
//!   (parallel tool_calls; sub-agent head + outer continuation).
//! - There is no chain-uniqueness constraint; idempotency is the
//!   transition function's responsibility (chain-walk frontier check).

use fusillade_arsenal::{PostgresResponseStepManager, TestDbPools};
use fusillade_core::request::RequestId;
use fusillade_core::response_step::{CreateStepInput, ResponseStepStore, StepKind, StepState};
use serde_json::json;
use uuid::Uuid;

/// Insert a minimal `requests` row so `response_steps.request_id`'s FK is
/// satisfied. Each call returns a fresh `RequestId` so callers can model
/// "one fusillade row per model_call step" naturally.
async fn insert_request(pool: &sqlx::PgPool) -> RequestId {
    let template_id = Uuid::new_v4();
    let request_id = Uuid::new_v4();

    sqlx::query(
        "INSERT INTO request_templates \
         (id, file_id, custom_id, endpoint, method, path, body, model, api_key, body_byte_size) \
         VALUES ($1, NULL, NULL, $2, 'POST', '/v1/responses', '{}', 'test-model', '', 0)",
    )
    .bind(template_id)
    .bind("http://upstream")
    .execute(pool)
    .await
    .expect("insert template");

    sqlx::query(
        "INSERT INTO requests \
         (id, batch_id, template_id, model, custom_id, state, created_by) \
         VALUES ($1, NULL, $2, 'test-model', NULL, 'pending', 'test-user')",
    )
    .bind(request_id)
    .bind(template_id)
    .execute(pool)
    .await
    .expect("insert request");

    RequestId(request_id)
}

#[sqlx::test]
async fn create_and_fetch_head_step(pool: sqlx::PgPool) {
    let request_id = insert_request(&pool).await;
    let pools = TestDbPools::new(pool).await.unwrap();
    let store = PostgresResponseStepManager::new(pools);

    let step_id = store
        .create_step(CreateStepInput {
            id: None,
            request_id: Some(request_id),
            prev_step_id: None,
            parent_step_id: None,
            step_kind: StepKind::ModelCall,
            step_sequence: 1,
            request_payload: json!({"messages": [{"role": "user", "content": "hi"}]}),
        })
        .await
        .unwrap();

    let step = store.get_step(step_id).await.unwrap().unwrap();
    assert_eq!(step.id, step_id);
    assert_eq!(step.request_id, Some(request_id));
    assert_eq!(step.parent_step_id, None);
    assert_eq!(step.prev_step_id, None);
    assert_eq!(step.step_kind, StepKind::ModelCall);
    assert_eq!(step.state, StepState::Pending);
    assert_eq!(step.step_sequence, 1);
}

#[sqlx::test]
async fn tool_call_step_has_null_request_id(pool: sqlx::PgPool) {
    let head_request_id = insert_request(&pool).await;
    let pools = TestDbPools::new(pool).await.unwrap();
    let store = PostgresResponseStepManager::new(pools);

    let head_id = store
        .create_step(CreateStepInput {
            id: None,
            request_id: Some(head_request_id),
            prev_step_id: None,
            parent_step_id: None,
            step_kind: StepKind::ModelCall,
            step_sequence: 1,
            request_payload: json!({"messages": []}),
        })
        .await
        .unwrap();

    // tool_call step: request_id is None (tool dispatch lives outside requests).
    let tool_step_id = store
        .create_step(CreateStepInput {
            id: None,
            request_id: None,
            prev_step_id: Some(head_id),
            parent_step_id: Some(head_id),
            step_kind: StepKind::ToolCall,
            step_sequence: 2,
            request_payload: json!({"name": "get_weather", "args": {"city": "Paris"}}),
        })
        .await
        .unwrap();

    let step = store.get_step(tool_step_id).await.unwrap().unwrap();
    assert_eq!(step.request_id, None);
    assert_eq!(step.step_kind, StepKind::ToolCall);
}

#[sqlx::test]
async fn step_kind_request_id_check_constraint_rejects_invalid_combos(pool: sqlx::PgPool) {
    // The DB-level CHECK constraint enforces:
    //   model_call ⇒ request_id IS NOT NULL  (the sub-request fusillade row)
    //   tool_call  ⇒ request_id IS NULL      (analytics live in tool_call_analytics)
    // Without it, get_step_by_request and analytics attribution would
    // have to defend against malformed rows at every read.
    let head_request_id = insert_request(&pool).await;
    let pools = TestDbPools::new(pool).await.unwrap();
    let store = PostgresResponseStepManager::new(pools);

    // model_call with NULL request_id is rejected.
    let model_no_request = store
        .create_step(CreateStepInput {
            id: None,
            request_id: None,
            prev_step_id: None,
            parent_step_id: None,
            step_kind: StepKind::ModelCall,
            step_sequence: 1,
            request_payload: json!({}),
        })
        .await;
    assert!(
        model_no_request.is_err(),
        "model_call with NULL request_id should violate the CHECK constraint"
    );

    // tool_call with non-NULL request_id is rejected.
    let tool_with_request = store
        .create_step(CreateStepInput {
            id: None,
            request_id: Some(head_request_id),
            prev_step_id: None,
            parent_step_id: None,
            step_kind: StepKind::ToolCall,
            step_sequence: 1,
            request_payload: json!({}),
        })
        .await;
    assert!(
        tool_with_request.is_err(),
        "tool_call with non-NULL request_id should violate the CHECK constraint"
    );
}

#[sqlx::test]
async fn list_chain_returns_head_plus_descendants_in_order(pool: sqlx::PgPool) {
    let pools = TestDbPools::new(pool.clone()).await.unwrap();
    let store = PostgresResponseStepManager::new(pools);

    // Head (model_call). request_id = first sub-request row.
    let head_req = insert_request(&pool).await;
    let head_id = store
        .create_step(CreateStepInput {
            id: None,
            request_id: Some(head_req),
            prev_step_id: None,
            parent_step_id: None,
            step_kind: StepKind::ModelCall,
            step_sequence: 1,
            request_payload: json!({"step": "head"}),
        })
        .await
        .unwrap();

    // tool_call (no request_id).
    let tool_id = store
        .create_step(CreateStepInput {
            id: None,
            request_id: None,
            prev_step_id: Some(head_id),
            parent_step_id: Some(head_id),
            step_kind: StepKind::ToolCall,
            step_sequence: 2,
            request_payload: json!({"step": "tool"}),
        })
        .await
        .unwrap();

    // Second model_call (its own sub-request row).
    let second_req = insert_request(&pool).await;
    let _second_id = store
        .create_step(CreateStepInput {
            id: None,
            request_id: Some(second_req),
            prev_step_id: Some(tool_id),
            parent_step_id: Some(head_id),
            step_kind: StepKind::ModelCall,
            step_sequence: 3,
            request_payload: json!({"step": "second_model"}),
        })
        .await
        .unwrap();

    let chain = store.list_chain(head_id).await.unwrap();
    assert_eq!(chain.len(), 3);
    assert_eq!(chain[0].step_sequence, 1);
    assert_eq!(chain[0].id, head_id);
    assert_eq!(chain[0].parent_step_id, None);
    assert_eq!(chain[1].step_sequence, 2);
    assert_eq!(chain[1].step_kind, StepKind::ToolCall);
    assert_eq!(chain[1].parent_step_id, Some(head_id));
    assert_eq!(chain[2].step_sequence, 3);
    assert_eq!(chain[2].request_id, Some(second_req));
    assert_eq!(chain[2].parent_step_id, Some(head_id));
}

#[sqlx::test]
async fn list_chain_isolates_chains_from_one_another(pool: sqlx::PgPool) {
    // Two independent responses: each is its own head + child. list_chain on
    // one head must not return the other's steps.
    let pools = TestDbPools::new(pool.clone()).await.unwrap();
    let store = PostgresResponseStepManager::new(pools);

    let r1 = insert_request(&pool).await;
    let head1 = store
        .create_step(CreateStepInput {
            id: None,
            request_id: Some(r1),
            prev_step_id: None,
            parent_step_id: None,
            step_kind: StepKind::ModelCall,
            step_sequence: 1,
            request_payload: json!({"chain": 1}),
        })
        .await
        .unwrap();
    let r1b = insert_request(&pool).await;
    let _child1 = store
        .create_step(CreateStepInput {
            id: None,
            request_id: Some(r1b),
            prev_step_id: Some(head1),
            parent_step_id: Some(head1),
            step_kind: StepKind::ModelCall,
            step_sequence: 2,
            request_payload: json!({"chain": 1, "step": "child"}),
        })
        .await
        .unwrap();

    let r2 = insert_request(&pool).await;
    let head2 = store
        .create_step(CreateStepInput {
            id: None,
            request_id: Some(r2),
            prev_step_id: None,
            parent_step_id: None,
            step_kind: StepKind::ModelCall,
            step_sequence: 1,
            request_payload: json!({"chain": 2}),
        })
        .await
        .unwrap();

    let chain1 = store.list_chain(head1).await.unwrap();
    assert_eq!(chain1.len(), 2);
    let chain2 = store.list_chain(head2).await.unwrap();
    assert_eq!(chain2.len(), 1);
    assert_eq!(chain2[0].id, head2);
}

#[sqlx::test]
async fn get_step_by_request_finds_model_call_owner(pool: sqlx::PgPool) {
    let head_req = insert_request(&pool).await;
    let pools = TestDbPools::new(pool.clone()).await.unwrap();
    let store = PostgresResponseStepManager::new(pools);

    let head_id = store
        .create_step(CreateStepInput {
            id: None,
            request_id: Some(head_req),
            prev_step_id: None,
            parent_step_id: None,
            step_kind: StepKind::ModelCall,
            step_sequence: 1,
            request_payload: json!({}),
        })
        .await
        .unwrap();

    let found = store.get_step_by_request(head_req).await.unwrap().unwrap();
    assert_eq!(found.id, head_id);

    // A non-step-bound request (e.g. a single-step chat/completions row) returns None.
    let unrelated = insert_request(&pool).await;
    assert!(
        store
            .get_step_by_request(unrelated)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test]
async fn parallel_tool_calls_with_same_prev_succeed(pool: sqlx::PgPool) {
    // Branch point: a model_call returning multiple tool_calls. All
    // tool_call children share (parent_step_id, prev_step_id, step_kind);
    // there is no DB-level uniqueness, so insertion should succeed.
    let head_req = insert_request(&pool).await;
    let pools = TestDbPools::new(pool.clone()).await.unwrap();
    let store = PostgresResponseStepManager::new(pools);

    let head_id = store
        .create_step(CreateStepInput {
            id: None,
            request_id: Some(head_req),
            prev_step_id: None,
            parent_step_id: None,
            step_kind: StepKind::ModelCall,
            step_sequence: 1,
            request_payload: json!({}),
        })
        .await
        .unwrap();

    for seq in 2..=4 {
        store
            .create_step(CreateStepInput {
                id: None,
                request_id: None,
                prev_step_id: Some(head_id),
                parent_step_id: Some(head_id),
                step_kind: StepKind::ToolCall,
                step_sequence: seq,
                request_payload: json!({"tool_index": seq - 2}),
            })
            .await
            .unwrap_or_else(|e| panic!("parallel tool_call insert failed at seq {seq}: {e}"));
    }

    let chain = store.list_chain(head_id).await.unwrap();
    assert_eq!(chain.len(), 4);
    let tool_calls: Vec<_> = chain
        .iter()
        .filter(|s| s.step_kind == StepKind::ToolCall)
        .collect();
    assert_eq!(tool_calls.len(), 3);
}

#[sqlx::test]
async fn lifecycle_transitions(pool: sqlx::PgPool) {
    let request_id = insert_request(&pool).await;
    let pools = TestDbPools::new(pool).await.unwrap();
    let store = PostgresResponseStepManager::new(pools);

    let step_id = store
        .create_step(CreateStepInput {
            id: None,
            request_id: Some(request_id),
            prev_step_id: None,
            parent_step_id: None,
            step_kind: StepKind::ModelCall,
            step_sequence: 1,
            request_payload: json!({}),
        })
        .await
        .unwrap();

    assert_eq!(
        store.get_step(step_id).await.unwrap().unwrap().state,
        StepState::Pending
    );

    store.mark_step_processing(step_id).await.unwrap();
    let step = store.get_step(step_id).await.unwrap().unwrap();
    assert_eq!(step.state, StepState::Processing);
    assert!(step.started_at.is_some());

    // Idempotency: re-marking processing on an already-processing step is a no-op.
    store.mark_step_processing(step_id).await.unwrap();

    store
        .complete_step(step_id, json!({"result": "ok"}))
        .await
        .unwrap();
    let step = store.get_step(step_id).await.unwrap().unwrap();
    assert_eq!(step.state, StepState::Completed);
    assert_eq!(step.response_payload, Some(json!({"result": "ok"})));
    assert!(step.completed_at.is_some());

    // Completing again is rejected (terminal state).
    let err = store
        .complete_step(step_id, json!({"second": true}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not in completable state"));
}

#[sqlx::test]
async fn fail_step_records_error_payload(pool: sqlx::PgPool) {
    let request_id = insert_request(&pool).await;
    let pools = TestDbPools::new(pool).await.unwrap();
    let store = PostgresResponseStepManager::new(pools);

    let step_id = store
        .create_step(CreateStepInput {
            id: None,
            request_id: Some(request_id),
            prev_step_id: None,
            parent_step_id: None,
            step_kind: StepKind::ModelCall,
            step_sequence: 1,
            request_payload: json!({}),
        })
        .await
        .unwrap();

    store
        .fail_step(step_id, json!({"type": "upstream_error", "code": 503}))
        .await
        .unwrap();
    let step = store.get_step(step_id).await.unwrap().unwrap();
    assert_eq!(step.state, StepState::Failed);
    assert!(step.failed_at.is_some());
    assert_eq!(
        step.error,
        Some(json!({"type": "upstream_error", "code": 503}))
    );
}

#[sqlx::test]
async fn cancel_step_records_canceled_at_and_blocks_terminal_transitions(pool: sqlx::PgPool) {
    let request_id = insert_request(&pool).await;
    let pools = TestDbPools::new(pool).await.unwrap();
    let store = PostgresResponseStepManager::new(pools);

    let step_id = store
        .create_step(CreateStepInput {
            id: None,
            request_id: Some(request_id),
            prev_step_id: None,
            parent_step_id: None,
            step_kind: StepKind::ModelCall,
            step_sequence: 1,
            request_payload: json!({}),
        })
        .await
        .unwrap();

    store.cancel_step(step_id).await.unwrap();
    let step = store.get_step(step_id).await.unwrap().unwrap();
    assert_eq!(step.state, StepState::Canceled);
    assert!(step.canceled_at.is_some());

    // Subsequent terminal transitions are rejected.
    let err = store
        .complete_step(step_id, json!({}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not in completable state"));
}

#[sqlx::test]
async fn requeue_for_retry_increments_attempt(pool: sqlx::PgPool) {
    let request_id = insert_request(&pool).await;
    let pools = TestDbPools::new(pool).await.unwrap();
    let store = PostgresResponseStepManager::new(pools);

    let step_id = store
        .create_step(CreateStepInput {
            id: None,
            request_id: Some(request_id),
            prev_step_id: None,
            parent_step_id: None,
            step_kind: StepKind::ModelCall,
            step_sequence: 1,
            request_payload: json!({}),
        })
        .await
        .unwrap();
    store.mark_step_processing(step_id).await.unwrap();

    store.requeue_step_for_retry(step_id).await.unwrap();
    let step = store.get_step(step_id).await.unwrap().unwrap();
    assert_eq!(step.state, StepState::Pending);
    assert_eq!(step.retry_attempt, 1);
    assert!(step.started_at.is_none());
}
