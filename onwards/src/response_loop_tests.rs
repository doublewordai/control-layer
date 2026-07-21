//! Unit tests for [`super::run_response_loop`].
//!
//! Two in-memory mocks: `MockStore` (implements [`MultiStepStore`])
//! drives transition decisions via a script; `ScriptedToolExecutor`
//! (implements [`ToolExecutor`]) records calls, lets tests override
//! per-step results, and declares each tool's [`ToolKind`] so the
//! loop's HTTP-vs-Agent dispatch can be exercised.
//!
//! Model calls are fired via fusillade's `ReqwestHttpClient` against a
//! wiremock server, so the loop's HTTP path is exercised by the same
//! code production uses for batch / single-step fires (which gets us
//! header-stamping for free).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use fusillade::{RequestId, ReqwestHttpClient};
use serde_json::json;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::response_loop::{LoopConfig, LoopError, UpstreamTarget, run_response_loop};
use crate::traits::{
    ChainStep, MultiStepStore, NextAction, RecordedStep, RequestContext, StepDescriptor, StepKind,
    StepState, StoreError, ToolError, ToolExecutor, ToolKind, ToolSchema,
};

#[derive(Debug, Default)]
struct StoreState {
    actions: HashMap<Option<String>, std::collections::VecDeque<NextAction>>,
    steps: HashMap<String, StoredStep>,
    next_seq: HashMap<String, i64>,
    record_order: Vec<String>,
    mark_processing_order: Vec<String>,
    complete_order: Vec<(String, serde_json::Value)>,
    fail_order: Vec<(String, serde_json::Value)>,
    id_counter: u64,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct StoredStep {
    request_id: String,
    scope_parent: Option<String>,
    prev_step: Option<String>,
    kind: StepKind,
    sequence: i64,
    state: StepState,
    response_payload: Option<serde_json::Value>,
    error: Option<serde_json::Value>,
}

#[derive(Default)]
struct MockStore {
    inner: Mutex<StoreState>,
}

impl MockStore {
    fn new() -> Self {
        Self::default()
    }

    fn script(&self, scope: Option<&str>, actions: Vec<NextAction>) {
        let mut state = self.inner.lock().unwrap();
        state
            .actions
            .entry(scope.map(|s| s.to_string()))
            .or_default()
            .extend(actions);
    }

    fn snapshot(&self) -> StoreSnapshot {
        let state = self.inner.lock().unwrap();
        StoreSnapshot {
            steps: state.steps.clone(),
            record_order: state.record_order.clone(),
            mark_processing_order: state.mark_processing_order.clone(),
            complete_order: state.complete_order.clone(),
            fail_order: state.fail_order.clone(),
        }
    }
}

#[allow(dead_code)]
struct StoreSnapshot {
    steps: HashMap<String, StoredStep>,
    record_order: Vec<String>,
    mark_processing_order: Vec<String>,
    complete_order: Vec<(String, serde_json::Value)>,
    fail_order: Vec<(String, serde_json::Value)>,
}

#[async_trait]
impl MultiStepStore for MockStore {
    async fn next_action_for(
        &self,
        _request_id: &str,
        scope_parent: Option<&str>,
    ) -> Result<NextAction, StoreError> {
        let mut state = self.inner.lock().unwrap();
        let key = scope_parent.map(|s| s.to_string());
        let queue = state.actions.get_mut(&key).ok_or_else(|| {
            StoreError::StorageError(format!("no scripted actions for scope {:?}", scope_parent))
        })?;
        queue.pop_front().ok_or_else(|| {
            StoreError::StorageError(format!(
                "scripted actions exhausted for scope {:?}",
                scope_parent
            ))
        })
    }

    async fn record_step(
        &self,
        request_id: &str,
        scope_parent: Option<&str>,
        prev_step: Option<&str>,
        descriptor: &StepDescriptor,
    ) -> Result<RecordedStep, StoreError> {
        let mut state = self.inner.lock().unwrap();
        state.id_counter += 1;
        let id = format!("step_{:03}", state.id_counter);
        let sequence = {
            let next = state.next_seq.entry(request_id.to_string()).or_insert(0);
            *next += 1;
            *next
        };
        state.steps.insert(
            id.clone(),
            StoredStep {
                request_id: request_id.to_string(),
                scope_parent: scope_parent.map(|s| s.to_string()),
                prev_step: prev_step.map(|s| s.to_string()),
                kind: descriptor.kind,
                sequence,
                state: StepState::Pending,
                response_payload: None,
                error: None,
            },
        );
        state.record_order.push(id.clone());
        // Mirror the storage contract: model_call steps allocate a
        // fusillade sub-request row; tool_call steps don't. Tests don't
        // verify the UUID value itself — they only need it to be `Some`
        // so `execute_step` doesn't bail with "missing sub_request_id".
        let sub_request_id = match descriptor.kind {
            StepKind::ModelCall => Some(RequestId(Uuid::new_v4())),
            StepKind::ToolCall => None,
        };
        Ok(RecordedStep {
            id,
            sequence,
            sub_request_id,
        })
    }

    async fn mark_step_processing(&self, step_id: &str) -> Result<(), StoreError> {
        let mut state = self.inner.lock().unwrap();
        if let Some(step) = state.steps.get_mut(step_id) {
            step.state = StepState::Processing;
        }
        state.mark_processing_order.push(step_id.to_string());
        Ok(())
    }

    async fn complete_step(
        &self,
        step_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(), StoreError> {
        let mut state = self.inner.lock().unwrap();
        if let Some(step) = state.steps.get_mut(step_id) {
            step.state = StepState::Completed;
            step.response_payload = Some(payload.clone());
        }
        state
            .complete_order
            .push((step_id.to_string(), payload.clone()));
        Ok(())
    }

    async fn fail_step(&self, step_id: &str, error: &serde_json::Value) -> Result<(), StoreError> {
        let mut state = self.inner.lock().unwrap();
        if let Some(step) = state.steps.get_mut(step_id) {
            step.state = StepState::Failed;
            step.error = Some(error.clone());
        }
        state.fail_order.push((step_id.to_string(), error.clone()));
        Ok(())
    }

    async fn list_chain(
        &self,
        request_id: &str,
        scope_parent: Option<&str>,
    ) -> Result<Vec<ChainStep>, StoreError> {
        let state = self.inner.lock().unwrap();
        let mut out: Vec<ChainStep> = state
            .steps
            .iter()
            .filter(|(_, step)| {
                step.request_id == request_id && step.scope_parent.as_deref() == scope_parent
            })
            .map(|(id, step)| ChainStep {
                id: id.clone(),
                kind: step.kind,
                state: step.state,
                sequence: step.sequence,
                prev_step_id: step.prev_step.clone(),
                parent_step_id: step.scope_parent.clone(),
                response_payload: step.response_payload.clone(),
                error: step.error.clone(),
            })
            .collect();
        out.sort_by_key(|s| s.sequence);
        Ok(out)
    }

    async fn assemble_response(&self, _request_id: &str) -> Result<serde_json::Value, StoreError> {
        Ok(json!({"assembled": true}))
    }
}

/// In-memory ToolExecutor for tests. Declares a configurable map of
/// (tool_name → (kind, result-override)) and records every execute call.
#[derive(Default)]
struct ScriptedToolExecutor {
    inner: Mutex<ScriptedExecutorState>,
}

#[derive(Default, Debug)]
struct ScriptedExecutorState {
    /// tool name → kind
    kinds: HashMap<String, ToolKind>,
    /// tool_name → optional result override (Err goes to ToolError::ExecutionError)
    results: HashMap<String, Result<serde_json::Value, String>>,
    /// every execute() call, in order: (tool_name, args)
    calls: Vec<(String, serde_json::Value)>,
}

impl ScriptedToolExecutor {
    fn new() -> Self {
        Self::default()
    }
    fn register(&self, name: &str, kind: ToolKind) {
        self.inner
            .lock()
            .unwrap()
            .kinds
            .insert(name.to_string(), kind);
    }
    #[allow(dead_code)]
    fn fail_with(&self, name: &str, msg: &str) {
        self.inner
            .lock()
            .unwrap()
            .results
            .insert(name.to_string(), Err(msg.to_string()));
    }
    fn calls(&self) -> Vec<(String, serde_json::Value)> {
        self.inner.lock().unwrap().calls.clone()
    }
}

#[async_trait]
impl ToolExecutor for ScriptedToolExecutor {
    async fn tools(&self, _ctx: &RequestContext) -> Vec<ToolSchema> {
        self.inner
            .lock()
            .unwrap()
            .kinds
            .iter()
            .map(|(name, kind)| ToolSchema {
                name: name.clone(),
                description: String::new(),
                parameters: json!({"type": "object"}),
                strict: false,
                kind: *kind,
            })
            .collect()
    }

    async fn execute(
        &self,
        tool_name: &str,
        _tool_call_id: &str,
        arguments: &serde_json::Value,
        _ctx: &RequestContext,
    ) -> Result<serde_json::Value, ToolError> {
        let mut state = self.inner.lock().unwrap();
        state.calls.push((tool_name.to_string(), arguments.clone()));
        match state.results.remove(tool_name) {
            Some(Ok(payload)) => Ok(payload),
            Some(Err(err)) => Err(ToolError::ExecutionError(err)),
            None => Ok(json!({"tool_output": format!("tool:{tool_name}")})),
        }
    }
}

fn model_call(payload: serde_json::Value) -> StepDescriptor {
    StepDescriptor {
        kind: StepKind::ModelCall,
        request_payload: payload,
    }
}

fn tool_call(name: &str, args: serde_json::Value) -> StepDescriptor {
    StepDescriptor {
        kind: StepKind::ToolCall,
        request_payload: json!({"name": name, "args": args}),
    }
}

/// Spin up a wiremock server that returns a sequence of model
/// responses on POSTs to /chat. Returns the server (kept alive), the
/// `endpoint` (host base) and `path` (`/chat`) for use as
/// [`UpstreamTarget`] fields — fusillade's HttpClient consumes them
/// separately, and its streaming dispatch keys on `path`.
async fn model_wiremock(responses: Vec<serde_json::Value>) -> (MockServer, String, String) {
    let server = MockServer::start().await;
    for body in responses {
        Mock::given(method("POST"))
            .and(path("/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .up_to_n_times(1)
            .mount(&server)
            .await;
    }
    let endpoint = server.uri();
    (server, endpoint, "/chat".to_string())
}

/// HTTP client for tests — fusillade's real `ReqwestHttpClient` with
/// generous timeouts. `streamable_endpoints` is empty so every fire
/// goes through the buffered branch by default (wiremock-returned
/// non-SSE JSON bodies would otherwise be passed to the SSE
/// reassembler and produce empty chunks). The streaming test wires up
/// its own client via `streaming_http_client_for_tests`.
fn http_client_for_tests() -> Arc<ReqwestHttpClient> {
    Arc::new(ReqwestHttpClient::new(
        Duration::from_secs(30),
        Duration::from_secs(30),
        Duration::from_secs(60),
        Vec::new(),
    ))
}

/// Client variant for the streaming SSE test: marks `/chat` streamable
/// so fusillade reads the wiremock SSE response chunk-by-chunk and the
/// per-event callback drives the sink-forwarding bridge in
/// `fire_model_call`.
fn streaming_http_client_for_tests() -> Arc<ReqwestHttpClient> {
    Arc::new(ReqwestHttpClient::new(
        Duration::from_secs(30),
        Duration::from_secs(30),
        Duration::from_secs(60),
        vec!["/chat".to_string()],
    ))
}

#[tokio::test]
async fn complete_immediately_returns_payload() {
    let store = MockStore::new();
    let tool_exec = ScriptedToolExecutor::new();
    let ctx = RequestContext::new();
    let target = UpstreamTarget {
        endpoint: "http://unused".into(),
        path: "/chat".into(),
        api_key: None,
    };
    store.script(None, vec![NextAction::Complete(json!({"final": true}))]);

    let result = run_response_loop(
        &store,
        &tool_exec,
        &ctx,
        &target,
        http_client_for_tests(),
        None,
        "req_1",
        None,
        LoopConfig::default(),
        0,
    )
    .await;

    assert_eq!(result.unwrap(), json!({"final": true}));
    assert!(store.snapshot().record_order.is_empty());
    assert!(tool_exec.calls().is_empty());
}

#[tokio::test]
async fn fail_immediately_returns_loop_error_failed() {
    let store = MockStore::new();
    let tool_exec = ScriptedToolExecutor::new();
    let ctx = RequestContext::new();
    let target = UpstreamTarget {
        endpoint: "http://unused".into(),
        path: "/chat".into(),
        api_key: None,
    };
    store.script(None, vec![NextAction::Fail(json!({"reason": "bad"}))]);

    let result = run_response_loop(
        &store,
        &tool_exec,
        &ctx,
        &target,
        http_client_for_tests(),
        None,
        "req_1",
        None,
        LoopConfig::default(),
        0,
    )
    .await;
    match result {
        Err(LoopError::Failed(payload)) => assert_eq!(payload, json!({"reason": "bad"})),
        other => panic!("expected LoopError::Failed, got {:?}", other),
    }
}

#[tokio::test]
async fn single_model_call_then_complete_routes_through_real_http_client() {
    // Exercises the model fire path: real fusillade ReqwestHttpClient
    // POSTs to wiremock, body parsed back into the step's response_payload.
    let (_model, endpoint, path) = model_wiremock(vec![json!({"output": "hello"})]).await;

    let store = MockStore::new();
    let tool_exec = ScriptedToolExecutor::new();
    let ctx = RequestContext::new();
    let target = UpstreamTarget {
        endpoint,
        path,
        api_key: None,
    };
    store.script(
        None,
        vec![
            NextAction::AppendSteps(vec![model_call(json!({"prompt": "hi"}))]),
            NextAction::Complete(json!({"done": true})),
        ],
    );

    let result = run_response_loop(
        &store,
        &tool_exec,
        &ctx,
        &target,
        http_client_for_tests(),
        None,
        "req_1",
        None,
        LoopConfig::default(),
        0,
    )
    .await
    .unwrap();
    assert_eq!(result, json!({"done": true}));

    let snap = store.snapshot();
    assert_eq!(snap.complete_order.len(), 1);
    assert_eq!(
        snap.complete_order[0].1,
        json!({"output": "hello"}),
        "step's response_payload should be the wiremock body verbatim"
    );
}

#[tokio::test]
async fn parallel_fan_out_chains_prev_step_id() {
    let store = MockStore::new();
    let tool_exec = ScriptedToolExecutor::new();
    tool_exec.register("a", ToolKind::Http);
    tool_exec.register("b", ToolKind::Http);
    tool_exec.register("c", ToolKind::Http);
    let ctx = RequestContext::new();
    let target = UpstreamTarget {
        endpoint: "http://unused".into(),
        path: "/chat".into(),
        api_key: None,
    };

    store.script(
        None,
        vec![
            NextAction::AppendSteps(vec![
                tool_call("a", json!({})),
                tool_call("b", json!({})),
                tool_call("c", json!({})),
            ]),
            NextAction::Complete(json!({"final": "ok"})),
        ],
    );

    run_response_loop(
        &store,
        &tool_exec,
        &ctx,
        &target,
        http_client_for_tests(),
        None,
        "req_1",
        None,
        LoopConfig::default(),
        0,
    )
    .await
    .unwrap();

    let snap = store.snapshot();
    assert_eq!(snap.record_order, vec!["step_001", "step_002", "step_003"]);
    assert_eq!(snap.steps["step_001"].prev_step, None);
    assert_eq!(snap.steps["step_002"].prev_step, Some("step_001".into()));
    assert_eq!(snap.steps["step_003"].prev_step, Some("step_002".into()));
    assert_eq!(snap.steps["step_001"].sequence, 1);
    assert_eq!(snap.steps["step_002"].sequence, 2);
    assert_eq!(snap.steps["step_003"].sequence, 3);
    assert_eq!(tool_exec.calls().len(), 3);
}

#[tokio::test]
async fn step_failure_does_not_abort_loop() {
    // A failing model call (wiremock returns 500 once) is persisted via
    // fail_step; the next iteration sees the failed sibling and the
    // transition function recovers.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream broke"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&server)
        .await;

    let store = MockStore::new();
    let tool_exec = ScriptedToolExecutor::new();
    let ctx = RequestContext::new();
    let target = UpstreamTarget {
        endpoint: server.uri(),
        path: "/chat".into(),
        api_key: None,
    };
    store.script(
        None,
        vec![
            NextAction::AppendSteps(vec![model_call(json!({}))]),
            NextAction::AppendSteps(vec![model_call(json!({}))]),
            NextAction::Complete(json!({"final": "ok"})),
        ],
    );

    run_response_loop(
        &store,
        &tool_exec,
        &ctx,
        &target,
        http_client_for_tests(),
        None,
        "req_1",
        None,
        LoopConfig::default(),
        0,
    )
    .await
    .unwrap();

    let snap = store.snapshot();
    assert_eq!(snap.fail_order.len(), 1, "first call fails");
    assert_eq!(snap.fail_order[0].0, "step_001");
    assert_eq!(snap.complete_order.len(), 1, "second call completes");
    assert_eq!(snap.complete_order[0].0, "step_002");
}

#[tokio::test]
async fn non_2xx_model_error_body_is_not_persisted() {
    const PROVIDER_BODY_SENTINEL: &str = "provider-private-error-body-do-not-expose";

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat"))
        .respond_with(ResponseTemplate::new(502).set_body_string(PROVIDER_BODY_SENTINEL))
        .mount(&server)
        .await;

    let store = MockStore::new();
    let tool_exec = ScriptedToolExecutor::new();
    let ctx = RequestContext::new();
    let target = UpstreamTarget {
        endpoint: server.uri(),
        path: "/chat".into(),
        api_key: None,
    };
    store.script(
        None,
        vec![
            NextAction::AppendSteps(vec![model_call(json!({}))]),
            NextAction::Complete(json!({"final": "recovered"})),
        ],
    );

    let result = run_response_loop(
        &store,
        &tool_exec,
        &ctx,
        &target,
        http_client_for_tests(),
        None,
        "req_provider_error",
        None,
        LoopConfig::default(),
        0,
    )
    .await
    .unwrap();

    assert_eq!(result, json!({"final": "recovered"}));
    let snap = store.snapshot();
    assert_eq!(snap.fail_order.len(), 1);
    let persisted_error = snap.fail_order[0].1.to_string();
    assert!(
        !persisted_error.contains(PROVIDER_BODY_SENTINEL),
        "provider response body escaped into the stored executor error: {persisted_error}"
    );
    assert!(persisted_error.contains("HTTP 502"));
    assert!(persisted_error.contains(&format!("{} bytes", PROVIDER_BODY_SENTINEL.len())));
}

#[tokio::test]
async fn max_iterations_cap_fires() {
    let store = MockStore::new();
    let tool_exec = ScriptedToolExecutor::new();
    tool_exec.register("a", ToolKind::Http);
    let ctx = RequestContext::new();
    let target = UpstreamTarget {
        endpoint: "http://unused".into(),
        path: "/chat".into(),
        api_key: None,
    };
    let mut script = Vec::new();
    for _ in 0..20 {
        script.push(NextAction::AppendSteps(vec![tool_call("a", json!({}))]));
    }
    store.script(None, script);

    let config = LoopConfig {
        max_response_step_depth: 8,
        max_response_iterations: 3,
    };
    let result = run_response_loop(
        &store,
        &tool_exec,
        &ctx,
        &target,
        http_client_for_tests(),
        None,
        "req_1",
        None,
        config,
        0,
    )
    .await;
    assert!(matches!(result, Err(LoopError::MaxIterationsExceeded)));
}

#[tokio::test]
async fn agent_kind_tool_triggers_recursion() {
    // A tool registered with ToolKind::Agent must cause the loop to
    // recurse instead of calling tool_executor.execute. Sub-loop
    // completes immediately; its return value is persisted as the
    // spawning tool step's response_payload.
    let store = MockStore::new();
    let tool_exec = ScriptedToolExecutor::new();
    tool_exec.register("delegate", ToolKind::Agent);
    let ctx = RequestContext::new();
    let target = UpstreamTarget {
        endpoint: "http://unused".into(),
        path: "/chat".into(),
        api_key: None,
    };
    store.script(
        None,
        vec![
            NextAction::AppendSteps(vec![tool_call("delegate", json!({"task": "x"}))]),
            NextAction::Complete(json!({"top": "done"})),
        ],
    );
    store.script(
        Some("step_001"),
        vec![NextAction::Complete(json!({"sub": "result"}))],
    );

    run_response_loop(
        &store,
        &tool_exec,
        &ctx,
        &target,
        http_client_for_tests(),
        None,
        "req_1",
        None,
        LoopConfig::default(),
        0,
    )
    .await
    .unwrap();

    // The Agent-kind tool was NOT executed via tool_executor.execute.
    assert!(
        tool_exec.calls().is_empty(),
        "Agent-kind tool must not be passed to ToolExecutor::execute"
    );
    let snap = store.snapshot();
    let top_tool_step = &snap.steps["step_001"];
    assert!(matches!(top_tool_step.kind, StepKind::ToolCall));
    // step_001 was completed with the sub-loop's return value.
    let (id, payload) = &snap.complete_order[0];
    assert_eq!(id, "step_001");
    assert_eq!(payload, &json!({"sub": "result"}));
}

#[tokio::test]
async fn http_kind_tool_routes_through_tool_executor() {
    let store = MockStore::new();
    let tool_exec = ScriptedToolExecutor::new();
    tool_exec.register("calculator", ToolKind::Http);
    let ctx = RequestContext::new();
    let target = UpstreamTarget {
        endpoint: "http://unused".into(),
        path: "/chat".into(),
        api_key: None,
    };
    store.script(
        None,
        vec![
            NextAction::AppendSteps(vec![tool_call("calculator", json!({"x": 1, "y": 2}))]),
            NextAction::Complete(json!({"final": "ok"})),
        ],
    );

    run_response_loop(
        &store,
        &tool_exec,
        &ctx,
        &target,
        http_client_for_tests(),
        None,
        "req_1",
        None,
        LoopConfig::default(),
        0,
    )
    .await
    .unwrap();

    let calls = tool_exec.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "calculator");
    assert_eq!(calls[0].1, json!({"x": 1, "y": 2}));
}

#[tokio::test]
async fn empty_action_returns_empty_action_error() {
    let store = MockStore::new();
    let tool_exec = ScriptedToolExecutor::new();
    let ctx = RequestContext::new();
    let target = UpstreamTarget {
        endpoint: "http://unused".into(),
        path: "/chat".into(),
        api_key: None,
    };
    store.script(None, vec![NextAction::AppendSteps(vec![])]);

    let result = run_response_loop(
        &store,
        &tool_exec,
        &ctx,
        &target,
        http_client_for_tests(),
        None,
        "req_1",
        None,
        LoopConfig::default(),
        0,
    )
    .await;
    assert!(matches!(result, Err(LoopError::EmptyAction)));
}

#[tokio::test]
async fn resume_picks_up_chain_tail_for_prev_step_id() {
    let store = MockStore::new();
    let tool_exec = ScriptedToolExecutor::new();
    tool_exec.register("a", ToolKind::Http);
    let ctx = RequestContext::new();
    let target = UpstreamTarget {
        endpoint: "http://unused".into(),
        path: "/chat".into(),
        api_key: None,
    };

    let preexisting = store
        .record_step(
            "req_1",
            None,
            None,
            &StepDescriptor {
                kind: StepKind::ToolCall,
                request_payload: json!({"name": "a", "args": {}}),
            },
        )
        .await
        .unwrap();
    store
        .complete_step(&preexisting.id, &json!({"prior": true}))
        .await
        .unwrap();

    store.script(
        None,
        vec![
            NextAction::AppendSteps(vec![tool_call("a", json!({}))]),
            NextAction::Complete(json!({"done": true})),
        ],
    );

    run_response_loop(
        &store,
        &tool_exec,
        &ctx,
        &target,
        http_client_for_tests(),
        None,
        "req_1",
        None,
        LoopConfig::default(),
        0,
    )
    .await
    .unwrap();

    let chain = store.list_chain("req_1", None).await.unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].id, preexisting.id);
    assert_eq!(
        chain[1].prev_step_id.as_deref(),
        Some(preexisting.id.as_str()),
        "resumed step must chain onto existing tail"
    );
    assert_eq!(chain[1].sequence, preexisting.sequence + 1);
}

#[tokio::test]
async fn streaming_model_call_forwards_token_deltas_and_emits_terminals() {
    use crate::streaming::{LoopEventKind, RecordingSink};

    // Wiremock that returns SSE chunks for a chat completions stream.
    let server = MockServer::start().await;
    let sse_body = "\
        data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"}}]}\n\n\
        data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n\
        data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
        data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body.as_bytes(), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let store = MockStore::new();
    let tool_exec = ScriptedToolExecutor::new();
    let ctx = RequestContext::new();
    let target = UpstreamTarget {
        endpoint: server.uri(),
        path: "/chat".into(),
        api_key: None,
    };
    let sink = RecordingSink::new();

    store.script(
        None,
        vec![
            NextAction::AppendSteps(vec![StepDescriptor {
                kind: StepKind::ModelCall,
                request_payload: json!({"prompt": "hi", "stream": true}),
            }]),
            NextAction::Complete(json!({"done": true})),
        ],
    );

    run_response_loop(
        &store,
        &tool_exec,
        &ctx,
        &target,
        streaming_http_client_for_tests(),
        Some(&sink),
        "req_1",
        None,
        LoopConfig::default(),
        0,
    )
    .await
    .unwrap();

    let events = sink.events();
    let kinds: Vec<LoopEventKind> = events.iter().map(|e| e.kind).collect();

    // Created at start, two text deltas (one per chunk), then completed
    // at terminal. The exact sequencing is:
    //   Created (sequence 0) → OutputTextDelta × 2 (sequence 1) →
    //   Completed (sequence > 1).
    assert!(
        kinds.contains(&LoopEventKind::Created),
        "missing Created, got {:?}",
        kinds
    );
    assert!(
        kinds
            .iter()
            .filter(|k| **k == LoopEventKind::OutputTextDelta)
            .count()
            >= 2,
        "expected ≥2 OutputTextDelta events, got {:?}",
        kinds
    );
    assert!(
        kinds.contains(&LoopEventKind::Completed),
        "missing Completed terminal, got {:?}",
        kinds
    );

    // Text deltas carry the assistant content verbatim.
    let text_deltas: Vec<String> = events
        .iter()
        .filter(|e| e.kind == LoopEventKind::OutputTextDelta)
        .map(|e| e.data["delta"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(text_deltas, vec!["Hello".to_string(), " world".to_string()]);

    // The Created event has sequence 0 (the canonical start cursor).
    let created = events
        .iter()
        .find(|e| e.kind == LoopEventKind::Created)
        .unwrap();
    assert_eq!(created.sequence, 0);
}

#[tokio::test]
async fn tool_call_emits_output_item_done_to_sink() {
    use crate::streaming::{LoopEventKind, RecordingSink};

    let store = MockStore::new();
    let tool_exec = ScriptedToolExecutor::new();
    tool_exec.register("calculator", ToolKind::Http);
    let ctx = RequestContext::new();
    let target = UpstreamTarget {
        endpoint: "http://unused".into(),
        path: "/chat".into(),
        api_key: None,
    };
    let sink = RecordingSink::new();

    store.script(
        None,
        vec![
            NextAction::AppendSteps(vec![tool_call("calculator", json!({"x": 1}))]),
            NextAction::Complete(json!({"done": true})),
        ],
    );

    run_response_loop(
        &store,
        &tool_exec,
        &ctx,
        &target,
        http_client_for_tests(),
        Some(&sink),
        "req_1",
        None,
        LoopConfig::default(),
        0,
    )
    .await
    .unwrap();

    let events = sink.events();
    let tool_done: Vec<&crate::streaming::LoopEvent> = events
        .iter()
        .filter(|e| e.kind == LoopEventKind::OutputItemDone)
        .filter(|e| e.data["type"] == "function_call_output")
        .collect();
    assert_eq!(
        tool_done.len(),
        1,
        "tool_call should emit one output_item.done"
    );
    assert!(
        events.iter().any(|e| e.kind == LoopEventKind::Completed),
        "terminal Completed event should be emitted"
    );
}

#[tokio::test]
async fn non_streaming_model_call_emits_no_token_deltas() {
    // When stream=false, model_call uses single-shot HTTP. No token
    // deltas should be emitted to the sink, but Created and Completed
    // terminal events still fire.
    use crate::streaming::{LoopEventKind, RecordingSink};

    let (_model, endpoint, path) = model_wiremock(vec![json!({"output": "hello"})]).await;
    let store = MockStore::new();
    let tool_exec = ScriptedToolExecutor::new();
    let ctx = RequestContext::new();
    let target = UpstreamTarget {
        endpoint,
        path,
        api_key: None,
    };
    let sink = RecordingSink::new();

    store.script(
        None,
        vec![
            NextAction::AppendSteps(vec![model_call(json!({"stream": false}))]),
            NextAction::Complete(json!({"done": true})),
        ],
    );

    run_response_loop(
        &store,
        &tool_exec,
        &ctx,
        &target,
        http_client_for_tests(),
        Some(&sink),
        "req_1",
        None,
        LoopConfig::default(),
        0,
    )
    .await
    .unwrap();

    let kinds: Vec<LoopEventKind> = sink.events().iter().map(|e| e.kind).collect();
    assert!(
        !kinds.contains(&LoopEventKind::OutputTextDelta),
        "non-streaming model call must not emit token deltas, got {:?}",
        kinds
    );
    assert!(kinds.contains(&LoopEventKind::Completed));
}
