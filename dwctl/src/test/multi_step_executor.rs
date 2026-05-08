//! End-to-end integration test for `run_response_loop` wired to the
//! production tool executor.
//!
//! What we wire up:
//! - storage = real `FusilladeResponseStore` over the live fusillade
//!   schema in dwctl's database;
//! - tool executor = real `HttpToolExecutor` (the same instance the
//!   single-step in-process loop uses);
//! - tool registry = a `ResolvedToolSet` constructed from real
//!   `ToolDefinition` rows (the same struct the database query in
//!   `tool_injection.rs` produces);
//! - HTTP client = real onwards `HyperClient` (same connection pool,
//!   TLS, timeouts as single-step proxying);
//! - upstream model = wiremock;
//! - tool endpoint = wiremock.
//!
//! Because the executor and HTTP client are the production types,
//! these tests catch regressions in the routing layer in addition to
//! the multi-step orchestration itself: any change to
//! `HttpToolExecutor`, `HyperClient`, or `ToolSchema` that breaks the
//! multi-step path will fail here.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use fusillade::{
    PoolProvider as FusilladePoolProvider, PostgresRequestManager, PostgresResponseStepManager, ReqwestHttpClient, TestDbPools,
};
use onwards::client::HttpClient;
use onwards::traits::RequestContext;
use onwards::{
    ChainStep, LoopConfig, MultiStepStore, NextAction, RecordedStep, StepDescriptor, StepKind, StepState, StoreError, UpstreamTarget,
    run_response_loop,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::responses::store::{FusilladeResponseStore, PendingResponseInput};
use crate::tool_executor::{HttpToolExecutor, ResolvedToolSet, ResolvedTools, ToolDefinition};

use crate::test::utils::setup_fusillade_pool;

/// Stash a pending input in the bridge's side-channel and return the
/// generated head step UUID as a String — the value to thread into
/// `run_response_loop` as `request_id`.
///
/// In production this is what `warm_path_setup` does before kicking
/// off the loop. The integration tests need the same setup for
/// `record_step` to find the per-response context (api_key, base_url)
/// it stamps onto each per-step sub-request fusillade row.
fn register_test_response<P>(store: &FusilladeResponseStore<P>, base_url: &str) -> String
where
    P: FusilladePoolProvider + Clone + Send + Sync + 'static,
{
    store
        .register_pending(PendingResponseInput {
            body: r#"{"model":"test-model","input":"hi"}"#.to_string(),
            api_key: None,
            created_by: None,
            base_url: base_url.to_string(),
            resolved_tool_names: std::collections::HashSet::new(),
        })
        .to_string()
}

/// Production-shaped transition function over [`FusilladeResponseStore`].
/// Drives:
///   empty chain → model_call
///   model_call returned `wants_tool=true` → emit tool_call
///   tool_call returned → emit summarizing model_call
///   model_call returned `wants_tool=false` → Complete
struct TransitionStore<P: FusilladePoolProvider + Clone + Send + Sync + 'static> {
    inner: FusilladeResponseStore<P>,
}

#[async_trait]
impl<P: FusilladePoolProvider + Clone + Send + Sync + 'static> MultiStepStore for TransitionStore<P> {
    async fn next_action_for(&self, request_id: &str, scope_parent: Option<&str>) -> Result<NextAction, StoreError> {
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
            .ok_or_else(|| StoreError::StorageError("no terminal step in chain".into()))?;
        let last_payload = last
            .response_payload
            .as_ref()
            .ok_or_else(|| StoreError::StorageError("last step has no response_payload".into()))?;

        match (last.kind, last_payload["wants_tool"].as_bool()) {
            (StepKind::ModelCall, Some(true)) => {
                let tool_name = last_payload["tool_name"].as_str().unwrap_or("echo_args").to_string();
                let tool_args = last_payload.get("tool_args").cloned().unwrap_or(json!({}));
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
                let output_text = last_payload["output_text"].as_str().unwrap_or("").to_string();
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

    async fn record_step(&self, r: &str, s: Option<&str>, p: Option<&str>, d: &StepDescriptor) -> Result<RecordedStep, StoreError> {
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
    async fn list_chain(&self, r: &str, s: Option<&str>) -> Result<Vec<ChainStep>, StoreError> {
        self.inner.list_chain(r, s).await
    }
    async fn assemble_response(&self, _r: &str) -> Result<Value, StoreError> {
        Ok(json!({}))
    }
}

fn http_client_for_tests() -> Arc<dyn HttpClient + Send + Sync> {
    Arc::new(onwards::client::create_hyper_client(10, 30))
}

async fn store_with_real_fusillade(pool: PgPool) -> FusilladeResponseStore<TestDbPools> {
    let pools = TestDbPools::new(pool).await.unwrap();
    let request_manager = Arc::new(PostgresRequestManager::<_, ReqwestHttpClient>::new(
        pools.clone(),
        Default::default(),
    ));
    let step_manager = Arc::new(PostgresResponseStepManager::new(pools));
    FusilladeResponseStore::new(request_manager).with_step_manager(step_manager)
}

#[sqlx::test]
async fn loop_drives_real_tool_and_model_calls_through_production_executor(pool: PgPool) {
    // Wiremocks for the upstream model and the tool.
    let model_server = MockServer::start().await;
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
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "wants_tool": false,
            "output_text": "the answer is 42",
        })))
        .mount(&model_server)
        .await;

    let tool_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/echo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"echoed": {"x": 42}})))
        .mount(&tool_server)
        .await;

    // Real ToolDefinition / ResolvedToolSet — same struct the DB query
    // populates. This is the production tool registry data path.
    let mut tools = HashMap::new();
    tools.insert(
        "echo_args".to_string(),
        ToolDefinition {
            kind: "http".to_string(),
            url: format!("{}/echo", tool_server.uri()),
            api_key: None,
            timeout_secs: 5,
            tool_source_id: Uuid::new_v4(),
        },
    );
    let resolved = Arc::new(ResolvedToolSet::new(tools, HashMap::new()));

    // Real HttpToolExecutor — same instance type the single-step
    // in-process loop uses. RequestContext carries ResolvedTools the
    // same way the tool injection middleware delivers it in production.
    let tool_executor = HttpToolExecutor::new(reqwest::Client::new(), None);
    let tool_ctx = RequestContext::new().with_extension(ResolvedTools(resolved));

    // Real onwards HyperClient for the model fire path.
    let http_client = http_client_for_tests();
    let upstream = UpstreamTarget {
        url: format!("{}/v1/chat/completions", model_server.uri()),
        api_key: None,
    };

    // Real fusillade-backed storage.
    let pool = setup_fusillade_pool(&pool).await;
    let inner_store = store_with_real_fusillade(pool).await;
    // Stand in for the warm path's side-channel registration. The
    // base_url here doesn't matter for this test (per-step rows use
    // it for analytics surface, not for routing — the loop fires to
    // `upstream.url` directly), but it must be present so
    // record_step can stamp it onto sub-request rows.
    let request_id = register_test_response(&inner_store, &model_server.uri());
    let store = TransitionStore { inner: inner_store };

    // Drive it.
    let final_payload = run_response_loop(
        &store,
        &tool_executor,
        &tool_ctx,
        &upstream,
        http_client,
        None,
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

    // Both the model wiremock and the tool wiremock got called via the
    // production code paths (HyperClient + HttpToolExecutor).
    assert_eq!(
        model_server.received_requests().await.unwrap().len(),
        2,
        "model wiremock should have received initial + summarize POSTs"
    );
    assert_eq!(
        tool_server.received_requests().await.unwrap().len(),
        1,
        "tool wiremock should have received one POST through HttpToolExecutor"
    );

    // Persisted chain shape.
    let chain = store.list_chain(&request_id, None).await.unwrap();
    assert_eq!(chain.len(), 3);
    assert!(matches!(chain[0].kind, StepKind::ModelCall));
    assert!(matches!(chain[1].kind, StepKind::ToolCall));
    assert!(matches!(chain[2].kind, StepKind::ModelCall));
    for (i, step) in chain.iter().enumerate() {
        assert!(matches!(step.state, StepState::Completed));
        assert_eq!(step.sequence, (i + 1) as i64);
    }
    // Tool step's response_payload is the wiremock body verbatim — proves
    // the production HttpToolExecutor was invoked end-to-end.
    assert_eq!(chain[1].response_payload.as_ref().unwrap(), &json!({"echoed": {"x": 42}}));
}

// Removed: `agent_kind_tool_recurses_via_tool_schema`.
//
// Sub-agent dispatch was modeled under the old identity scheme where
// each scope (top-level + each sub-agent loop) had its own
// `parent_step_id`-keyed chain. Under fusillade 16.8's head-step
// identity, every step in a response — including sub-agent
// descendants — shares parent_step_id = head id. The loop's own
// `list_chain(request_id, scope_parent)` calls collapse to a single
// flat walk, so the existing sub-agent logic in onwards' loop won't
// produce a viable sub-scope view without prev_step_id branch-aware
// traversal in both the bridge and the transition function. That
// rework is explicitly out of scope for this PR (see fusillade plan
// §"Out of scope" and §"Sub-agent recursion"). Re-add a sub-agent
// integration test once that wiring lands.
