//! End-to-end integration test for the multi-step orchestration loop.
//!
//! Drives [`onwards::run_response_loop`] against:
//! - a real `FusilladeResponseStore` backed by `PostgresResponseStepManager`
//!   over the live test database;
//! - a toy transition function (`ToyTransitionStore`) that scripts a
//!   model_call → tool_call → model_call → complete flow;
//! - a toy `StepExecutor` that returns synthetic payloads.
//!
//! What this validates:
//! - storage primitives (`record_step`, `mark_step_processing`,
//!   `complete_step`, `list_chain`) work end-to-end against PostgreSQL;
//! - sequence allocation is monotonic per request_id;
//! - the loop's prev_step_id chaining persists correctly;
//! - the executor / store split actually drives the full lifecycle of a
//!   multi-step response.
//!
//! What this does *not* validate:
//! - the production transition function (deferred to COR-346/347 — the
//!   real Open Responses tool-call semantics);
//! - assembly into the OpenAI Response JSON shape (deferred to COR-348);
//! - upstream model HTTP fan-out (deferred to COR-349).
//!
//! When those follow-ups land, this test stays as a regression — the toy
//! transition + executor exercise the same trait surface the real ones
//! will plug into.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fusillade::{
    PoolProvider as FusilladePoolProvider, PostgresRequestManager, PostgresResponseStepManager,
    ReqwestHttpClient, TestDbPools,
};
use onwards::{
    ChainStep, ExecutorError, LoopConfig, LoopError, MultiStepStore, NextAction, RecordedStep,
    StepDescriptor, StepExecutor, StepKind, StepState, StoreError, ToolDispatch,
    run_response_loop,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::responses::store::FusilladeResponseStore;

/// Toy transition function. Wraps a `FusilladeResponseStore` (delegating
/// every persistence call to it) and scripts `next_action_for` based on
/// what the chain so far looks like.
///
/// The flow it drives:
///
/// 1. Empty chain → emit a `model_call` step.
/// 2. After the model_call completes with `{"wants_tool": true}` →
///    emit a `tool_call` step.
/// 3. After the tool_call completes → emit another `model_call` to
///    consume the tool result.
/// 4. After that final model_call → `Complete` with the assembled
///    payload.
struct ToyTransitionStore<P: FusilladePoolProvider + Clone + Send + Sync + 'static> {
    inner: FusilladeResponseStore<P>,
}

#[async_trait]
impl<P: FusilladePoolProvider + Clone + Send + Sync + 'static> MultiStepStore
    for ToyTransitionStore<P>
{
    async fn next_action_for(
        &self,
        request_id: &str,
        scope_parent: Option<&str>,
    ) -> Result<NextAction, StoreError> {
        let chain = self.inner.list_chain(request_id, scope_parent).await?;

        // 1. Empty chain → first model_call
        if chain.is_empty() {
            return Ok(NextAction::AppendSteps(vec![StepDescriptor {
                kind: StepKind::ModelCall,
                request_payload: json!({"prompt": "what is 2+2?"}),
            }]));
        }

        // Look at the last completed step to decide.
        let last = chain.iter().rev().find(|s| {
            matches!(
                s.state,
                StepState::Completed | StepState::Failed | StepState::Canceled
            )
        });
        let Some(last) = last else {
            // Step is still pending/processing. Should not happen in this
            // toy flow because the loop completes each step before
            // calling next_action_for again, but defend against it.
            return Err(StoreError::StorageError(
                "next_action_for called with no terminal step in chain".into(),
            ));
        };

        match (last.kind, last.response_payload.as_ref()) {
            (StepKind::ModelCall, Some(payload)) if payload["wants_tool"] == json!(true) => {
                // 2. Model called for a tool — emit the tool_call step.
                Ok(NextAction::AppendSteps(vec![StepDescriptor {
                    kind: StepKind::ToolCall,
                    request_payload: json!({"name": "static_echo", "args": {"x": 42}}),
                }]))
            }
            (StepKind::ToolCall, Some(_)) => {
                // 3. Tool returned — emit the synthesizing model_call.
                Ok(NextAction::AppendSteps(vec![StepDescriptor {
                    kind: StepKind::ModelCall,
                    request_payload: json!({"prompt": "summarize tool result"}),
                }]))
            }
            (StepKind::ModelCall, Some(_)) => {
                // 4. Final model_call done — complete.
                Ok(NextAction::Complete(json!({
                    "id": format!("resp_{}", request_id),
                    "object": "response",
                    "status": "completed",
                    "step_count": chain.len(),
                })))
            }
            _ => Err(StoreError::StorageError(
                "unexpected chain state in toy transition".into(),
            )),
        }
    }

    async fn record_step(
        &self,
        request_id: &str,
        scope_parent: Option<&str>,
        prev_step: Option<&str>,
        descriptor: &StepDescriptor,
    ) -> Result<RecordedStep, StoreError> {
        self.inner
            .record_step(request_id, scope_parent, prev_step, descriptor)
            .await
    }

    async fn mark_step_processing(&self, step_id: &str) -> Result<(), StoreError> {
        self.inner.mark_step_processing(step_id).await
    }

    async fn complete_step(&self, step_id: &str, payload: &Value) -> Result<(), StoreError> {
        self.inner.complete_step(step_id, payload).await
    }

    async fn fail_step(&self, step_id: &str, error: &Value) -> Result<(), StoreError> {
        self.inner.fail_step(step_id, error).await
    }

    async fn list_chain(
        &self,
        request_id: &str,
        scope_parent: Option<&str>,
    ) -> Result<Vec<ChainStep>, StoreError> {
        self.inner.list_chain(request_id, scope_parent).await
    }

    async fn assemble_response(&self, request_id: &str) -> Result<Value, StoreError> {
        // The toy transition's `Complete` payload is already a fully-
        // formed response object, so this is unused in this test. Kept
        // for trait completeness.
        let chain = self.inner.list_chain(request_id, None).await?;
        Ok(json!({
            "id": format!("resp_{}", request_id),
            "step_count": chain.len(),
        }))
    }
}

/// Toy executor. Returns synthetic payloads for both step kinds. The
/// first model_call signals "I want a tool" via `wants_tool: true`; the
/// second model_call returns final output text. Tool calls echo their
/// arguments.
struct ToyExecutor {
    /// How many times execute_model_call has been invoked. Used to flip
    /// the response shape between the first call (asks for tool) and the
    /// second call (returns final text).
    model_call_count: Mutex<usize>,
}

#[async_trait]
impl StepExecutor for ToyExecutor {
    async fn execute_model_call(
        &self,
        _step_id: &str,
        _request_payload: &Value,
    ) -> Result<Value, ExecutorError> {
        let mut count = self.model_call_count.lock().unwrap();
        *count += 1;
        let n = *count;
        if n == 1 {
            Ok(json!({
                "wants_tool": true,
                "tool_name": "static_echo",
                "tool_args": {"x": 42},
            }))
        } else {
            Ok(json!({
                "wants_tool": false,
                "output_text": "the answer is 4",
            }))
        }
    }

    async fn dispatch_tool_call(
        &self,
        _step_id: &str,
        request_payload: &Value,
    ) -> Result<ToolDispatch, ExecutorError> {
        // Synthetic "static_echo" tool — return the args verbatim wrapped
        // in an output envelope.
        let args = request_payload.get("args").cloned().unwrap_or(json!({}));
        Ok(ToolDispatch::Executed(json!({
            "tool_output": args,
            "echoed_at": "test",
        })))
    }
}

/// Connect to dwctl's fusillade-schema pool. dwctl auto-applies the
/// fusillade migrator on startup against its own database under the
/// `fusillade` schema (see `Setting up fusillade batch processing pool`
/// in the boot log), so the schema is guaranteed to exist as long as
/// dwctl has been run at least once against this DB.
///
/// Tests skip `#[sqlx::test]` because that runs dwctl's migrations
/// against a per-test pool, and dwctl's migrator does not include the
/// fusillade migrations — we'd be left without the response_steps
/// table. Connecting to the dev DB (which dwctl already migrated) keeps
/// the test setup minimal; per-test UUIDs prevent cross-test
/// interference.
async fn fusillade_pool() -> PgPool {
    let url = std::env::var("MULTI_STEP_TEST_DATABASE_URL").unwrap_or_else(|_| {
        // dwctl manages a `fusillade` schema inside the dwctl DB.
        "postgres://postgres:password@localhost:5432/dwctl?options=-c%20search_path%3Dfusillade"
            .into()
    });
    PgPool::connect(&url).await.expect(
        "connect to dwctl's fusillade schema; run `cargo run` once first \
         so dwctl applies its migrations and the fusillade schema exists",
    )
}

/// Insert a parent fusillade `requests` row directly so the
/// `response_steps.request_id` foreign key is satisfied. Returns the
/// request id formatted the way onwards passes it (UUID string).
async fn insert_parent_request(pool: &PgPool, schema: &str) -> String {
    let template_id = Uuid::new_v4();
    let request_id = Uuid::new_v4();

    let create_template = format!(
        "INSERT INTO {schema}.request_templates \
         (id, file_id, custom_id, endpoint, method, path, body, model, api_key, body_byte_size) \
         VALUES ($1, NULL, NULL, $2, 'POST', '/v1/responses', '{{}}', 'test-model', '', 0)"
    );
    sqlx::query(&create_template)
        .bind(template_id)
        .bind("http://upstream")
        .execute(pool)
        .await
        .expect("insert template");

    let create_request = format!(
        "INSERT INTO {schema}.requests \
         (id, batch_id, template_id, model, custom_id, state) \
         VALUES ($1, NULL, $2, 'test-model', NULL, 'pending')"
    );
    sqlx::query(&create_request)
        .bind(request_id)
        .bind(template_id)
        .execute(pool)
        .await
        .expect("insert request");

    request_id.to_string()
}

#[tokio::test]
async fn loop_drives_full_lifecycle_against_live_db() {
    let pool = fusillade_pool().await;
    let request_id = insert_parent_request(&pool, "fusillade").await;

    let pools = TestDbPools::new(pool.clone()).await.unwrap();

    // PostgresRequestManager is needed to construct FusilladeResponseStore;
    // the loop test never actually fires upstream HTTP, so the http
    // client config is irrelevant.
    let request_manager = Arc::new(PostgresRequestManager::<_, ReqwestHttpClient>::new(
        pools.clone(),
        Default::default(),
    ));
    let step_manager = Arc::new(PostgresResponseStepManager::new(pools));
    let inner_store = FusilladeResponseStore::new(request_manager).with_step_manager(step_manager);

    let store = ToyTransitionStore { inner: inner_store };
    let executor = ToyExecutor {
        model_call_count: Mutex::new(0),
    };

    let final_payload = run_response_loop(
        &store,
        &executor,
        &request_id,
        None,
        LoopConfig::default(),
        0,
    )
    .await
    .expect("loop should complete");

    assert_eq!(final_payload["status"], json!("completed"));
    assert_eq!(final_payload["step_count"], json!(3));

    // Walk the persisted chain and assert exact shape: model_call →
    // tool_call → model_call, all completed, in monotonic sequence,
    // chained via prev_step_id.
    let chain = store.list_chain(&request_id, None).await.unwrap();
    assert_eq!(chain.len(), 3, "expected 3 top-level steps, got {chain:?}");

    assert!(matches!(chain[0].kind, StepKind::ModelCall));
    assert!(matches!(chain[1].kind, StepKind::ToolCall));
    assert!(matches!(chain[2].kind, StepKind::ModelCall));

    for (i, step) in chain.iter().enumerate() {
        assert!(
            matches!(step.state, StepState::Completed),
            "step {i} should be Completed, got {:?}",
            step.state
        );
        assert_eq!(step.sequence, (i + 1) as i64, "sequence should be {}", i + 1);
        assert!(step.response_payload.is_some(), "step {i} should have payload");
    }

    // prev_step_id chains linearly
    assert_eq!(chain[0].prev_step_id, None);
    assert_eq!(chain[1].prev_step_id.as_deref(), Some(chain[0].id.as_str()));
    assert_eq!(chain[2].prev_step_id.as_deref(), Some(chain[1].id.as_str()));

    // The first model_call's response carries the wants_tool=true marker
    // that drove the transition's tool_call decision.
    assert_eq!(
        chain[0].response_payload.as_ref().unwrap()["wants_tool"],
        json!(true)
    );
    // The tool_call step persisted the synthetic echo output.
    assert_eq!(
        chain[1].response_payload.as_ref().unwrap()["tool_output"],
        json!({"x": 42})
    );
    // The final model_call returned final text.
    assert_eq!(
        chain[2].response_payload.as_ref().unwrap()["wants_tool"],
        json!(false)
    );
}

#[tokio::test]
async fn loop_resumes_existing_chain_under_simulated_recovery() {
    let pool = fusillade_pool().await;
    // Simulates: a previous worker recorded + completed step 1, then
    // crashed before recording step 2. The new worker (this test) must
    // pick up from where the previous one left off — chaining the next
    // step onto the existing tail rather than starting a parallel chain.
    let request_id = insert_parent_request(&pool, "fusillade").await;
    let pools = TestDbPools::new(pool.clone()).await.unwrap();
    let request_manager = Arc::new(PostgresRequestManager::<_, ReqwestHttpClient>::new(
        pools.clone(),
        Default::default(),
    ));
    let step_manager = Arc::new(PostgresResponseStepManager::new(pools));
    let inner_store = FusilladeResponseStore::new(request_manager).with_step_manager(step_manager);

    // Pre-populate: record + process + complete step 1 directly via the
    // store (mimicking what the previous worker did before crashing).
    // The fusillade field-presence constraints require pending → processing
    // → completed, so mark_step_processing is needed before complete_step.
    let preexisting = inner_store
        .record_step(
            &request_id,
            None,
            None,
            &StepDescriptor {
                kind: StepKind::ModelCall,
                request_payload: json!({"prompt": "previous worker"}),
            },
        )
        .await
        .unwrap();
    inner_store
        .mark_step_processing(&preexisting.id)
        .await
        .unwrap();
    inner_store
        .complete_step(
            &preexisting.id,
            &json!({"wants_tool": true, "tool_name": "static_echo"}),
        )
        .await
        .unwrap();

    // Now run the loop. The toy transition will see a completed
    // model_call with wants_tool=true and emit a tool_call.
    let store = ToyTransitionStore { inner: inner_store };
    let executor = ToyExecutor {
        model_call_count: Mutex::new(1), // pretend we've already done one
    };

    let _ = run_response_loop(
        &store,
        &executor,
        &request_id,
        None,
        LoopConfig::default(),
        0,
    )
    .await
    .expect("loop should complete");

    let chain = store.list_chain(&request_id, None).await.unwrap();
    assert_eq!(chain.len(), 3, "should be original step + 2 new = 3");
    assert_eq!(chain[0].id, preexisting.id, "preexisting step kept its id");
    assert_eq!(
        chain[1].prev_step_id.as_deref(),
        Some(preexisting.id.as_str()),
        "new tool_call must chain onto the preexisting tail, not start fresh"
    );
    assert_eq!(chain[1].sequence, preexisting.sequence + 1);
}

/// Catches a regression in the LoopError::Failed flow: when the
/// transition function returns Fail mid-chain, the loop returns
/// LoopError::Failed and the persisted chain shows what happened.
#[tokio::test]
async fn loop_surfaces_failed_transition() {
    let pool = fusillade_pool().await;
    let request_id = insert_parent_request(&pool, "fusillade").await;
    let pools = TestDbPools::new(pool.clone()).await.unwrap();
    let request_manager = Arc::new(PostgresRequestManager::<_, ReqwestHttpClient>::new(
        pools.clone(),
        Default::default(),
    ));
    let step_manager = Arc::new(PostgresResponseStepManager::new(pools));
    let inner_store = FusilladeResponseStore::new(request_manager).with_step_manager(step_manager);

    // A store whose transition immediately fails, regardless of state.
    struct AlwaysFail<P: FusilladePoolProvider + Clone + Send + Sync + 'static> {
        inner: FusilladeResponseStore<P>,
    }
    #[async_trait]
    impl<P: FusilladePoolProvider + Clone + Send + Sync + 'static> MultiStepStore for AlwaysFail<P> {
        async fn next_action_for(
            &self,
            _r: &str,
            _s: Option<&str>,
        ) -> Result<NextAction, StoreError> {
            Ok(NextAction::Fail(json!({"reason": "synthetic"})))
        }
        async fn record_step(
            &self,
            r: &str,
            s: Option<&str>,
            p: Option<&str>,
            d: &StepDescriptor,
        ) -> Result<RecordedStep, StoreError> {
            self.inner.record_step(r, s, p, d).await
        }
        async fn mark_step_processing(&self, id: &str) -> Result<(), StoreError> {
            self.inner.mark_step_processing(id).await
        }
        async fn complete_step(&self, id: &str, p: &Value) -> Result<(), StoreError> {
            self.inner.complete_step(id, p).await
        }
        async fn fail_step(&self, id: &str, e: &Value) -> Result<(), StoreError> {
            self.inner.fail_step(id, e).await
        }
        async fn list_chain(
            &self,
            r: &str,
            s: Option<&str>,
        ) -> Result<Vec<ChainStep>, StoreError> {
            self.inner.list_chain(r, s).await
        }
        async fn assemble_response(&self, _r: &str) -> Result<Value, StoreError> {
            Ok(json!({}))
        }
    }

    let store = AlwaysFail { inner: inner_store };
    let executor = ToyExecutor {
        model_call_count: Mutex::new(0),
    };

    let result = run_response_loop(
        &store,
        &executor,
        &request_id,
        None,
        LoopConfig::default(),
        0,
    )
    .await;

    match result {
        Err(LoopError::Failed(payload)) => {
            assert_eq!(payload, json!({"reason": "synthetic"}));
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}
