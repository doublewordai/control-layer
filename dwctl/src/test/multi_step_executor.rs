//! End-to-end integration test for the real `DwctlStepExecutor`.
//!
//! Drives [`onwards::run_response_loop`] against the real
//! [`DwctlStepExecutor`] (wrapping the real `HttpToolExecutor` + a
//! [`StaticModelCaller`]) plus the real [`FusilladeResponseStore`]. The
//! upstream model and the tool are both wiremock servers, so the test
//! validates:
//!
//! - the `tool_sources.kind = 'http'` dispatch path: tool fires through
//!   `HttpToolExecutor::execute`, hits the wiremock, returns its body,
//!   which is then persisted as the step's `response_payload`;
//! - the `tool_sources.kind = 'agent'` dispatch path: a sub-agent
//!   tool returns `ToolDispatch::Recurse`, the loop recurses, the
//!   sub-loop completes, and the result is recorded as the spawning
//!   step's `response_payload`;
//! - the model-call path: requests POST through `StaticModelCaller`,
//!   responses parsed and persisted;
//! - the full multi-step lifecycle persists the chain in the right
//!   order with sequence and prev_step_id chaining.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use fusillade::{
    PoolProvider as FusilladePoolProvider, PostgresRequestManager, PostgresResponseStepManager,
    ReqwestHttpClient, TestDbPools,
};
use onwards::{
    ChainStep, LoopConfig, MultiStepStore, NextAction, RecordedStep, StepDescriptor, StepKind,
    StepState, StoreError, run_response_loop,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::responses::step_executor::{DwctlStepExecutor, StaticModelCaller};
use crate::responses::store::FusilladeResponseStore;
use crate::tool_executor::{HttpToolExecutor, ResolvedToolSet, ToolDefinition};

async fn fusillade_pool() -> PgPool {
    let url = std::env::var("MULTI_STEP_TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://postgres:password@localhost:5432/dwctl?options=-c%20search_path%3Dfusillade"
            .into()
    });
    PgPool::connect(&url).await.expect(
        "connect to dwctl's fusillade schema; run `cargo run` once first \
         so dwctl applies its migrations and the fusillade schema exists",
    )
}

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

/// Production-shaped transition function. Drives:
///   empty chain → model_call
///   model_call returned `wants_tool=true` → emit tool_call from `tool_name`/`tool_args`
///   tool_call returned → emit summarizing model_call
///   model_call returned `wants_tool=false` → Complete
struct TransitionStore<P: FusilladePoolProvider + Clone + Send + Sync + 'static> {
    inner: FusilladeResponseStore<P>,
}

#[async_trait]
impl<P: FusilladePoolProvider + Clone + Send + Sync + 'static> MultiStepStore
    for TransitionStore<P>
{
    async fn next_action_for(
        &self,
        request_id: &str,
        scope_parent: Option<&str>,
    ) -> Result<NextAction, StoreError> {
        let chain = self.inner.list_chain(request_id, scope_parent).await?;

        if chain.is_empty() {
            return Ok(NextAction::AppendSteps(vec![StepDescriptor {
                kind: StepKind::ModelCall,
                request_payload: json!({
                    "messages": [{"role": "user", "content": "hello"}],
                    "model": "test-model",
                }),
            }]));
        }

        let last = chain
            .iter()
            .rev()
            .find(|s| matches!(s.state, StepState::Completed | StepState::Failed))
            .ok_or_else(|| {
                StoreError::StorageError("no terminal step in chain".into())
            })?;

        let last_payload = last.response_payload.as_ref().ok_or_else(|| {
            StoreError::StorageError("last step has no response_payload".into())
        })?;

        match (last.kind, last_payload["wants_tool"].as_bool()) {
            (StepKind::ModelCall, Some(true)) => {
                let tool_name = last_payload["tool_name"]
                    .as_str()
                    .unwrap_or("static_echo")
                    .to_string();
                let tool_args = last_payload
                    .get("tool_args")
                    .cloned()
                    .unwrap_or(json!({}));
                Ok(NextAction::AppendSteps(vec![StepDescriptor {
                    kind: StepKind::ToolCall,
                    request_payload: json!({"name": tool_name, "args": tool_args}),
                }]))
            }
            (StepKind::ToolCall, _) => Ok(NextAction::AppendSteps(vec![StepDescriptor {
                kind: StepKind::ModelCall,
                request_payload: json!({
                    "messages": [{"role": "tool", "content": last_payload}],
                    "model": "test-model",
                }),
            }])),
            (StepKind::ModelCall, Some(false)) => {
                let output_text = last_payload["output_text"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                Ok(NextAction::Complete(json!({
                    "id": format!("resp_{request_id}"),
                    "object": "response",
                    "status": "completed",
                    "output_text": output_text,
                    "step_count": chain.len(),
                })))
            }
            _ => Err(StoreError::StorageError(format!(
                "unexpected chain state: kind={:?} wants_tool={:?}",
                last.kind,
                last_payload.get("wants_tool")
            ))),
        }
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

#[tokio::test]
async fn dwctl_step_executor_drives_real_tool_and_model_calls_against_wiremock() {
    let pool = fusillade_pool().await;
    let request_id = insert_parent_request(&pool, "fusillade").await;

    // ---- wiremock for the upstream model ----
    let model_server = MockServer::start().await;
    // First call: model says it wants the tool.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "wants_tool": true,
            "tool_name": "echo_args",
            "tool_args": {"x": 42},
        })))
        .up_to_n_times(1)
        .mount(&model_server)
        .await;
    // Second call: model returns final text.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "wants_tool": false,
            "output_text": "the answer is 42",
        })))
        .mount(&model_server)
        .await;

    // ---- wiremock for the tool ----
    let tool_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/echo"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"echoed": {"x": 42}})),
        )
        .mount(&tool_server)
        .await;

    // ---- resolved tool set: one HTTP tool registered ----
    let tool_source_id = Uuid::new_v4();
    let mut tools = HashMap::new();
    tools.insert(
        "echo_args".to_string(),
        ToolDefinition {
            kind: "http".to_string(),
            url: format!("{}/echo", tool_server.uri()),
            api_key: None,
            timeout_secs: 5,
            tool_source_id,
        },
    );
    let resolved = Arc::new(ResolvedToolSet::new(tools, HashMap::new()));

    let http_tool_executor = Arc::new(HttpToolExecutor::new(
        reqwest::Client::new(),
        None, // no analytics writes in this test
    ));
    let model_caller = Arc::new(StaticModelCaller {
        client: reqwest::Client::new(),
        url: format!("{}/v1/chat/completions", model_server.uri()),
        api_key: None,
    });
    let executor = DwctlStepExecutor::new(http_tool_executor, resolved, model_caller);

    // ---- store wired up to real fusillade ----
    let pools = TestDbPools::new(pool.clone()).await.unwrap();
    let request_manager = Arc::new(PostgresRequestManager::<_, ReqwestHttpClient>::new(
        pools.clone(),
        Default::default(),
    ));
    let step_manager = Arc::new(PostgresResponseStepManager::new(pools));
    let inner = FusilladeResponseStore::new(request_manager).with_step_manager(step_manager);
    let store = TransitionStore { inner };

    // ---- drive the loop ----
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
    assert_eq!(final_payload["output_text"], json!("the answer is 42"));
    assert_eq!(final_payload["step_count"], json!(3));

    // ---- both wiremocks were hit ----
    assert_eq!(
        model_server.received_requests().await.unwrap().len(),
        2,
        "model wiremock should have received two POSTs (initial + summarize)"
    );
    assert_eq!(
        tool_server.received_requests().await.unwrap().len(),
        1,
        "tool wiremock should have received one POST"
    );

    // ---- chain persisted correctly ----
    let chain = store.list_chain(&request_id, None).await.unwrap();
    assert_eq!(chain.len(), 3);
    assert!(matches!(chain[0].kind, StepKind::ModelCall));
    assert!(matches!(chain[1].kind, StepKind::ToolCall));
    assert!(matches!(chain[2].kind, StepKind::ModelCall));

    for (i, step) in chain.iter().enumerate() {
        assert!(matches!(step.state, StepState::Completed));
        assert_eq!(step.sequence, (i + 1) as i64);
    }
    // Tool step's response_payload is exactly what wiremock returned.
    assert_eq!(
        chain[1].response_payload.as_ref().unwrap(),
        &json!({"echoed": {"x": 42}})
    );
}

#[tokio::test]
async fn dwctl_step_executor_dispatches_subagent_tools_via_recurse() {
    // A `tool_sources.kind = 'agent'` tool should signal recursion
    // rather than fire HTTP. The test doesn't need a tool wiremock for
    // the agent tool — `ToolDispatch::Recurse` skips it entirely.
    let pool = fusillade_pool().await;
    let request_id = insert_parent_request(&pool, "fusillade").await;

    // Model wiremock: first call wants the agent tool; sub-agent's
    // first call returns final text immediately; back at top-level the
    // summarizing call also returns final text.
    let model_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "wants_tool": true,
            "tool_name": "delegate_subagent",
            "tool_args": {"task": "do thing"},
        })))
        .up_to_n_times(1)
        .mount(&model_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "wants_tool": false,
            "output_text": "subagent done",
        })))
        .up_to_n_times(1)
        .mount(&model_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "wants_tool": false,
            "output_text": "all done",
        })))
        .mount(&model_server)
        .await;

    let mut tools = HashMap::new();
    tools.insert(
        "delegate_subagent".to_string(),
        ToolDefinition {
            kind: "agent".to_string(), // <-- the key bit
            url: "http://unused".into(),
            api_key: None,
            timeout_secs: 5,
            tool_source_id: Uuid::new_v4(),
        },
    );
    let resolved = Arc::new(ResolvedToolSet::new(tools, HashMap::new()));

    let executor = DwctlStepExecutor::new(
        Arc::new(HttpToolExecutor::new(reqwest::Client::new(), None)),
        resolved,
        Arc::new(StaticModelCaller {
            client: reqwest::Client::new(),
            url: format!("{}/v1/chat/completions", model_server.uri()),
            api_key: None,
        }),
    );

    let pools = TestDbPools::new(pool.clone()).await.unwrap();
    let request_manager = Arc::new(PostgresRequestManager::<_, ReqwestHttpClient>::new(
        pools.clone(),
        Default::default(),
    ));
    let step_manager = Arc::new(PostgresResponseStepManager::new(pools));
    let inner = FusilladeResponseStore::new(request_manager).with_step_manager(step_manager);
    let store = TransitionStore { inner };

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
    assert_eq!(final_payload["output_text"], json!("all done"));

    // The agent tool step (top-level chain[1]) was completed with the
    // sub-loop's final payload (the sub-agent's "subagent done" body).
    let chain = store.list_chain(&request_id, None).await.unwrap();
    assert_eq!(chain.len(), 3, "top-level chain should have 3 steps");
    let tool_step = &chain[1];
    assert!(matches!(tool_step.kind, StepKind::ToolCall));
    let tool_payload = tool_step.response_payload.as_ref().unwrap();
    // The sub-loop's Complete payload was a constructed response object.
    // The sub-loop returned via run_response_loop, and its return value
    // gets persisted as the spawning step's response_payload.
    assert_eq!(tool_payload["status"], json!("completed"));
    assert_eq!(tool_payload["output_text"], json!("subagent done"));

    // The sub-loop's chain (scope_parent = the top-level tool step) has
    // at least one step — the model_call that returned wants_tool=false
    // and triggered the sub-loop's Complete. The exact count depends on
    // how many iterations the sub-loop ran before completing; the
    // important invariant is recursion happened (sub-loop steps exist
    // under the spawning tool step's scope).
    let sub_chain = store
        .list_chain(&request_id, Some(&tool_step.id))
        .await
        .unwrap();
    assert!(
        !sub_chain.is_empty(),
        "sub-loop should have produced at least one step under the spawning tool step's scope"
    );
    assert!(matches!(sub_chain[0].kind, StepKind::ModelCall));
    assert_eq!(
        sub_chain[0].parent_step_id.as_deref(),
        Some(tool_step.id.as_str()),
        "sub-loop step's parent_step_id must point at the spawning top-level tool step"
    );
}
