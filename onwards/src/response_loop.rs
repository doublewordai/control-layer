//! Multi-step Open Responses orchestration loop.
//!
//! [`run_response_loop`] drives a multi-step response from `pending` to
//! terminal state. It is a free function over the [`MultiStepStore`] and
//! [`ToolExecutor`] traits — both already exist in onwards for other
//! purposes, so the multi-step path adds no parallel execution
//! abstraction.
//!
//! ## Wiring
//!
//! - **Storage** — [`MultiStepStore`] handles CRUD + transition +
//!   chain walk + assembly.
//! - **Tool dispatch** — [`ToolExecutor::tools`] declares the tools and
//!   their [`ToolKind`]; [`ToolExecutor::execute`] runs `Http`-kind
//!   tools. `Agent`-kind tools cause the loop to recurse into a sub-loop
//!   instead of calling `execute`.
//! - **Model calls** — fired via [`fusillade::HttpClient`] against the
//!   configured [`UpstreamTarget`]. Going through fusillade gets us the
//!   `X-Fusillade-Request-Id` header stamping (using the
//!   `RecordedStep.sub_request_id` the store handed back) for free, so
//!   the `http_analytics` row produced by outlet middleware lines up
//!   with the right row in `fusillade.requests`. Streaming responses
//!   plug an [`fusillade::StreamEventCallback`] into the existing
//!   chunk-read loop so live token deltas reach the [`EventSink`]
//!   without losing fusillade's reassembly machinery.
//!
//! This means dwctl's existing `HttpToolExecutor` (which already
//! implements `ToolExecutor`) plugs straight in — no wrapping, no
//! adapter, no parallel multi-step trait.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use fusillade::{
    HttpClient as FusilladeHttpClient, RequestData, RequestId, StreamEvent, StreamEventCallback,
    TemplateId,
};
use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::streaming::EventSink;
use crate::traits::{
    ChainStep, ExecutorError, MultiStepStore, NextAction, RecordedStep, RequestContext,
    StepDescriptor, StepKind, StoreError, ToolError, ToolExecutor, ToolKind,
};

/// Type alias for the recursive sub-loop call. Required because async fns
/// can't directly recurse — the future type is its own type, infinitely.
type LoopFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, LoopError>> + Send + 'a>>;

#[cfg(test)]
#[path = "response_loop_tests.rs"]
mod tests;

/// Configuration for [`run_response_loop`]'s safety caps.
#[derive(Debug, Clone, Copy)]
pub struct LoopConfig {
    pub max_response_step_depth: u32,
    pub max_response_iterations: u32,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_response_step_depth: 8,
            max_response_iterations: 10,
        }
    }
}

/// Where the loop should send model_call HTTP requests.
///
/// Split into `endpoint` (base URL) + `path` rather than a single
/// concatenated URL because fusillade's `HttpClient` classifies
/// streaming-vs-buffered behavior off the `path` component of the
/// `RequestData` it receives — passing the full URL in `endpoint` with
/// `path = ""` defeats streamable-endpoint matching and forces every
/// model fire down the non-streaming path.
#[derive(Debug, Clone)]
pub struct UpstreamTarget {
    /// Base URL — protocol + host + any prefix path that's not
    /// streamable-dispatched (e.g. `http://127.0.0.1:3001/ai`).
    pub endpoint: String,
    /// Path component matched against fusillade's `streamable_endpoints`
    /// list (e.g. `/v1/chat/completions`).
    pub path: String,
    /// Bearer token for the upstream call. `None` and `Some("")` are
    /// equivalent: both result in no `Authorization` header on the
    /// outgoing request (fusillade's `ReqwestHttpClient` only stamps
    /// the header when the api_key is non-empty), so callers can use
    /// whichever shape matches their config-loading code.
    pub api_key: Option<String>,
}

/// Errors returned by [`run_response_loop`].
#[derive(Debug)]
pub enum LoopError {
    Failed(Value),
    MaxIterationsExceeded,
    MaxDepthExceeded,
    EmptyAction,
    Store(StoreError),
    Executor(ExecutorError),
}

impl fmt::Display for LoopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoopError::Failed(payload) => write!(f, "response loop failed: {}", payload),
            LoopError::MaxIterationsExceeded => {
                write!(f, "response loop exceeded max_response_iterations cap")
            }
            LoopError::MaxDepthExceeded => write!(
                f,
                "response loop sub-agent recursion exceeded max_response_step_depth cap"
            ),
            LoopError::EmptyAction => write!(
                f,
                "transition function returned AppendSteps with no descriptors"
            ),
            LoopError::Store(e) => write!(f, "response loop storage error: {}", e),
            LoopError::Executor(e) => write!(f, "response loop executor error: {}", e),
        }
    }
}

impl std::error::Error for LoopError {}

impl From<StoreError> for LoopError {
    fn from(e: StoreError) -> Self {
        LoopError::Store(e)
    }
}

impl From<ExecutorError> for LoopError {
    fn from(e: ExecutorError) -> Self {
        LoopError::Executor(e)
    }
}

/// Drive a multi-step Open Responses request to a terminal state.
///
/// `tool_executor` is the same trait dwctl already implements via
/// `HttpToolExecutor` for the single-step in-process loop — no separate
/// abstraction.
///
/// `model_target` is where model_call steps fire HTTP. The loop POSTs
/// the step's `request_payload` as JSON, optionally with a Bearer
/// token, and parses the response as JSON.
///
/// `tool_ctx` is the `RequestContext` passed to `ToolExecutor::tools`
/// and `::execute` — carries the per-request resolved tool set for
/// dwctl's middleware-driven model.
#[allow(clippy::too_many_arguments)]
pub fn run_response_loop<'a, S, T, H>(
    store: &'a S,
    tool_executor: &'a T,
    tool_ctx: &'a RequestContext,
    model_target: &'a UpstreamTarget,
    http_client: Arc<H>,
    event_sink: Option<&'a (dyn EventSink + 'a)>,
    request_id: &'a str,
    scope_parent: Option<&'a str>,
    config: LoopConfig,
    depth: u32,
) -> LoopFuture<'a>
where
    S: MultiStepStore + ?Sized,
    T: ToolExecutor + ?Sized,
    H: FusilladeHttpClient + 'static,
{
    Box::pin(async move {
        if depth > config.max_response_step_depth {
            return Err(LoopError::MaxDepthExceeded);
        }

        // Resolve tool kinds once at the start of this loop level; the
        // dispatch path looks up by name on each tool_call. Cached locally
        // so dispatch is O(1) and we don't spam the executor with
        // discovery calls.
        let kinds: HashMap<String, ToolKind> = tool_executor
            .tools(tool_ctx)
            .await
            .into_iter()
            .map(|s| (s.name, s.kind))
            .collect();

        let mut iterations: u32 = 0;
        // Resume-aware: chain new steps onto whatever this scope's
        // existing tail is. A worker picking up a partially-populated
        // chain (crash recovery, executor handoff) chains correctly
        // instead of starting a parallel chain at the head.
        let chain_at_start = store.list_chain(request_id, scope_parent).await?;
        let mut prev_step: Option<String> = chain_at_start.last().map(|s| s.id.clone());

        // Once-per-response `created` event for the user-visible loop
        // (top-level only; sub-agents don't emit their own created).
        if depth == 0
            && scope_parent.is_none()
            && let Some(sink) = event_sink
        {
            crate::streaming::try_emit(
                sink,
                crate::streaming::LoopEvent {
                    sequence: 0,
                    kind: crate::streaming::LoopEventKind::Created,
                    data: serde_json::json!({
                        "id": format!("resp_{request_id}"),
                        "object": "response",
                        "status": "in_progress",
                    }),
                },
            )
            .await;
        }

        loop {
            if iterations >= config.max_response_iterations {
                emit_terminal(
                    event_sink,
                    depth,
                    scope_parent,
                    LoopTerminal::Failed(serde_json::json!({"type": "max_iterations_exceeded"})),
                    next_terminal_sequence(&chain_at_start, iterations as i64),
                )
                .await;
                return Err(LoopError::MaxIterationsExceeded);
            }
            iterations += 1;

            let action = store.next_action_for(request_id, scope_parent).await?;

            match action {
                NextAction::Complete(payload) => {
                    emit_terminal(
                        event_sink,
                        depth,
                        scope_parent,
                        LoopTerminal::Completed(payload.clone()),
                        next_terminal_sequence_after_run(store, request_id, scope_parent).await,
                    )
                    .await;
                    return Ok(payload);
                }
                NextAction::Fail(payload) => {
                    emit_terminal(
                        event_sink,
                        depth,
                        scope_parent,
                        LoopTerminal::Failed(payload.clone()),
                        next_terminal_sequence_after_run(store, request_id, scope_parent).await,
                    )
                    .await;
                    return Err(LoopError::Failed(payload));
                }
                NextAction::AppendSteps(descriptors) => {
                    if descriptors.is_empty() {
                        return Err(LoopError::EmptyAction);
                    }

                    // Insert each step in order, chaining prev_step_id
                    // through siblings for stable transcript ordering even
                    // though execution is concurrent below. record_step
                    // allocates the sequence atomically — N siblings = N
                    // queries (no separate sequence-allocation
                    // round-trip). RecordedStep also carries the
                    // sub-request fusillade row id (Some for model_call,
                    // None for tool_call) so execute_step can stamp it
                    // as `X-Fusillade-Request-Id` on the outgoing HTTP
                    // fire, lining up with the analytics row.
                    let mut recorded_steps: Vec<RecordedStep> =
                        Vec::with_capacity(descriptors.len());
                    let mut current_prev: Option<String> = prev_step.clone();
                    for descriptor in &descriptors {
                        let recorded = store
                            .record_step(
                                request_id,
                                scope_parent,
                                current_prev.as_deref(),
                                descriptor,
                            )
                            .await?;
                        current_prev = Some(recorded.id.clone());
                        recorded_steps.push(recorded);
                    }

                    // Execute siblings concurrently. Per-step failures
                    // are persisted via fail_step and swallowed; storage
                    // failures propagate. Sub-agent recursion happens
                    // inside execute_step.
                    let futures = descriptors.iter().zip(recorded_steps.iter()).map(
                        |(descriptor, recorded)| {
                            execute_step(
                                store,
                                tool_executor,
                                tool_ctx,
                                model_target,
                                http_client.clone(),
                                event_sink,
                                &kinds,
                                request_id,
                                &recorded.id,
                                recorded.sequence,
                                recorded.sub_request_id,
                                descriptor,
                                config,
                                depth,
                            )
                        },
                    );
                    let results: Vec<Result<(), LoopError>> =
                        futures_util::future::join_all(futures).await;

                    for outcome in results {
                        outcome?;
                    }

                    prev_step = recorded_steps.last().map(|r| r.id.clone());
                }
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn execute_step<'a, S, T, H>(
    store: &'a S,
    tool_executor: &'a T,
    tool_ctx: &'a RequestContext,
    model_target: &'a UpstreamTarget,
    http_client: Arc<H>,
    event_sink: Option<&'a (dyn EventSink + 'a)>,
    kinds: &'a HashMap<String, ToolKind>,
    request_id: &'a str,
    step_id: &'a str,
    step_sequence: i64,
    sub_request_id: Option<RequestId>,
    descriptor: &'a StepDescriptor,
    config: LoopConfig,
    depth: u32,
) -> Pin<Box<dyn Future<Output = Result<(), LoopError>> + Send + 'a>>
where
    S: MultiStepStore + ?Sized,
    T: ToolExecutor + ?Sized,
    H: FusilladeHttpClient + 'static,
{
    Box::pin(async move {
        store.mark_step_processing(step_id).await?;

        let outcome: Result<Value, LoopError> = match descriptor.kind {
            StepKind::ModelCall => {
                // The store contract (RecordedStep) guarantees model_call
                // steps carry a sub_request_id — that row anchors the
                // analytics linkage. Surface a clean error rather than
                // panicking if a store implementation violates it.
                let sub_request_id = sub_request_id.ok_or_else(|| {
                    LoopError::Executor(ExecutorError::ExecutionError(
                        "model_call step has no sub_request_id; \
                         MultiStepStore::record_step must populate it for ModelCall"
                            .into(),
                    ))
                })?;
                fire_model_call(
                    &*http_client,
                    model_target,
                    sub_request_id,
                    &descriptor.request_payload,
                    event_sink,
                    step_sequence,
                )
                .await
            }
            StepKind::ToolCall => {
                let tool_name = descriptor
                    .request_payload
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        LoopError::Executor(ExecutorError::ExecutionError(
                            "tool_call request_payload missing 'name'".into(),
                        ))
                    })?;
                let kind = kinds.get(tool_name).copied().unwrap_or(ToolKind::Http);

                match kind {
                    ToolKind::Agent => {
                        if depth + 1 > config.max_response_step_depth {
                            Err(LoopError::MaxDepthExceeded)
                        } else {
                            run_response_loop(
                                store,
                                tool_executor,
                                tool_ctx,
                                model_target,
                                http_client.clone(),
                                event_sink,
                                request_id,
                                Some(step_id),
                                config,
                                depth + 1,
                            )
                            .await
                        }
                    }
                    ToolKind::Http => {
                        let args = descriptor
                            .request_payload
                            .get("args")
                            .cloned()
                            .unwrap_or(serde_json::json!({}));
                        tool_executor
                            .execute(tool_name, step_id, &args, tool_ctx)
                            .await
                            .map_err(|e| LoopError::Executor(translate_tool_error(e)))
                    }
                }
            }
        };

        match outcome {
            Ok(payload) => {
                store.complete_step(step_id, &payload).await?;
                // For tool_call steps, emit `output_item.done` with the
                // tool's full payload to the sink at completion. Token
                // deltas don't apply here (tools don't stream); the
                // step's full result is the natural unit. ModelCall
                // emissions are handled inside fire_model_call (text
                // deltas during streaming + the upstream's own done
                // marker).
                if matches!(descriptor.kind, StepKind::ToolCall)
                    && let Some(sink) = event_sink
                {
                    crate::streaming::try_emit(
                        sink,
                        crate::streaming::LoopEvent {
                            sequence: step_sequence,
                            kind: crate::streaming::LoopEventKind::OutputItemDone,
                            data: serde_json::json!({
                                "type": "function_call_output",
                                "call_id": format!("call_step_{step_sequence}"),
                                "output": serde_json::to_string(&payload).unwrap_or_default(),
                            }),
                        },
                    )
                    .await;
                }
                Ok(())
            }
            Err(loop_err @ LoopError::Store(_)) => Err(loop_err),
            Err(loop_err) => {
                let error_payload = error_to_payload(&loop_err);
                store.fail_step(step_id, &error_payload).await?;
                Ok(())
            }
        }
    })
}

/// Helpers for terminal event emission. Used by run_response_loop on
/// Complete / Fail / cap-violation paths.
enum LoopTerminal {
    Completed(Value),
    Failed(Value),
}

async fn emit_terminal(
    sink: Option<&(dyn EventSink + '_)>,
    depth: u32,
    scope_parent: Option<&str>,
    terminal: LoopTerminal,
    sequence: i64,
) {
    // Only top-level terminal events are surfaced to the user. Sub-loop
    // (sub-agent) terminals become the spawning step's response_payload
    // via the loop's normal return path; emitting them at user level
    // would conflate sub-agent and top-level state.
    if depth != 0 || scope_parent.is_some() {
        return;
    }
    let Some(sink) = sink else { return };
    let (kind, data) = match terminal {
        LoopTerminal::Completed(v) => (crate::streaming::LoopEventKind::Completed, v),
        LoopTerminal::Failed(v) => (crate::streaming::LoopEventKind::Failed, v),
    };
    crate::streaming::try_emit(
        sink,
        crate::streaming::LoopEvent {
            sequence,
            kind,
            data,
        },
    )
    .await;
}

/// Cheap sequence allocator for terminal events: max(chain_at_start) +
/// iterations + 1. Used when the loop terminates without doing any
/// extra storage I/O.
fn next_terminal_sequence(chain_at_start: &[ChainStep], iterations: i64) -> i64 {
    chain_at_start.iter().map(|s| s.sequence).max().unwrap_or(0) + iterations + 1
}

/// Sequence allocator for the post-loop-success terminal event: walk
/// the chain once more to find the highest persisted sequence and
/// return +1. Slightly more expensive but exact (matches what a
/// reconnect-with-cursor expects).
async fn next_terminal_sequence_after_run<S>(
    store: &S,
    request_id: &str,
    scope_parent: Option<&str>,
) -> i64
where
    S: MultiStepStore + ?Sized,
{
    match store.list_chain(request_id, scope_parent).await {
        Ok(chain) => chain.iter().map(|s| s.sequence).max().unwrap_or(0) + 1,
        // Best-effort fallback if the chain walk fails — terminal
        // events should still emit so clients can close their stream.
        Err(_) => i64::MAX,
    }
}

async fn fire_model_call<H: FusilladeHttpClient + 'static>(
    http_client: &H,
    target: &UpstreamTarget,
    sub_request_id: RequestId,
    request_payload: &Value,
    sink: Option<&dyn EventSink>,
    step_sequence: i64,
) -> Result<Value, LoopError> {
    // The user's `stream` flag propagates through the parent request →
    // transition function → request_payload here. When true, fusillade's
    // streaming HTTP path parses SSE events, our callback forwards token
    // deltas to the sink as each event arrives, and the reassembler
    // hands us the final body. When false, we POST and parse a single
    // JSON body, ignoring the sink even if one is supplied.
    let stream_mode = request_payload
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let body = serde_json::to_string(request_payload).map_err(|e| {
        LoopError::Executor(ExecutorError::ExecutionError(format!(
            "model call body serialize: {e}"
        )))
    })?;
    let model = request_payload
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let api_key = target.api_key.clone().unwrap_or_default();

    // Fusillade's RequestData is the input shape its HttpClient
    // implementations consume. Most fields don't matter for the HTTP
    // fire itself — they're metadata fusillade stamps as `x-fusillade-*`
    // headers for analytics correlation. The two that DO matter for the
    // wire are `endpoint + path` (URL composition) and `body`. `id` is
    // load-bearing because it becomes the `X-Fusillade-Request-Id`
    // header that lines the analytics row up with the sub-request row.
    let request_data = RequestData {
        id: sub_request_id,
        batch_id: None,
        template_id: TemplateId(Uuid::nil()),
        custom_id: None,
        endpoint: target.endpoint.clone(),
        method: "POST".to_string(),
        path: target.path.clone(),
        body,
        model,
        api_key: api_key.clone(),
        created_by: String::new(),
        batch_metadata: HashMap::new(),
    };

    let response = if stream_mode {
        // Bridge: the callback is synchronous (fusillade's contract — see
        // its StreamEventCallback docstring) but the sink is async. Push
        // LoopEvents through a bounded mpsc; a drain future running
        // concurrently with the model fire awaits them and forwards.
        // When fusillade's `execute_with_event_callback` returns, the
        // last Arc<dyn StreamEventCallback> drops, the Sender drops, the
        // channel closes, and the drain future completes naturally.
        //
        // Bounded capacity caps memory if the downstream sink stalls
        // (slow / disconnected client). Overflow drops the newest event
        // — `try_emit` already swallows sink-emit failures, so the
        // overall semantic is "best-effort live tail" either way.
        let (event_tx, event_rx) =
            mpsc::channel::<crate::streaming::LoopEvent>(SINK_BRIDGE_CAPACITY);
        let callback: Arc<dyn StreamEventCallback> = Arc::new(SinkChannelCallback {
            tx: event_tx,
            step_sequence,
        });
        let drain_fut = async {
            let mut rx = event_rx;
            while let Some(event) = rx.recv().await {
                if let Some(sink) = sink {
                    crate::streaming::try_emit(sink, event).await;
                }
            }
        };
        let fire_fut =
            http_client.execute_with_event_callback(&request_data, &api_key, Some(callback));
        let (fire_result, _) = tokio::join!(fire_fut, drain_fut);
        fire_result
    } else {
        http_client.execute(&request_data, &api_key).await
    }
    .map_err(|e| {
        LoopError::Executor(ExecutorError::ExecutionError(format!(
            "model call HTTP error: {e}"
        )))
    })?;

    if response.status < 200 || response.status >= 300 {
        return Err(LoopError::Executor(ExecutorError::ExecutionError(format!(
            "model call returned HTTP {} (response body: {} bytes)",
            response.status,
            response.body.len()
        ))));
    }

    serde_json::from_str::<Value>(&response.body).map_err(|e| {
        LoopError::Executor(ExecutorError::ExecutionError(format!(
            "model call body parse: {e}"
        )))
    })
}

/// Capacity of the bridge channel between fusillade's per-chunk
/// callback and the async event sink. Picked to hold a comfortable
/// burst of token deltas (an LLM emitting at hundreds-of-tokens/s
/// while the client is briefly stalled) without unbounded growth.
/// Overflow drops the newest event — fusillade's reassembler still
/// produces the correct final body, so only the live tail is affected.
const SINK_BRIDGE_CAPACITY: usize = 1024;

/// Synchronous [`StreamEventCallback`] that bridges fusillade's
/// chunk-by-chunk SSE callbacks into onwards' async [`EventSink`]
/// vocabulary via a bounded mpsc channel. The drain side of the
/// channel runs concurrently with the model fire (see
/// [`fire_model_call`]) and forwards each event to the sink.
///
/// Uses `try_send` so the callback stays synchronous — overflow
/// (drain side slow / client stalled) drops the event instead of
/// blocking the upstream-read loop and tripping `body_timeout`.
struct SinkChannelCallback {
    tx: mpsc::Sender<crate::streaming::LoopEvent>,
    step_sequence: i64,
}

impl StreamEventCallback for SinkChannelCallback {
    fn on_event(&self, event: &StreamEvent<'_>) {
        for loop_event in delta_loop_events(event, self.step_sequence) {
            // Drop on closed/full channel — same best-effort semantic
            // as `streaming::try_emit`'s closed-sink path. A full
            // channel here means the SSE consumer is too slow; the
            // reassembled final body is unaffected, only the live tail.
            let _ = self.tx.try_send(loop_event);
        }
    }
}

/// Translate one upstream SSE event into zero or more [`LoopEvent`]s.
///
/// Forwards two kinds of deltas:
/// - assistant text content → `response.output_text.delta`
/// - tool_call arguments → `response.function_call_arguments.delta`
///
/// Both carry the originating `step_sequence` as the SSE `id:` so
/// reconnect-with-cursor can resume. Malformed events and the `[DONE]`
/// marker return no events (fusillade's reassembler handles `[DONE]`
/// internally).
fn delta_loop_events(event: &StreamEvent, sequence: i64) -> Vec<crate::streaming::LoopEvent> {
    use crate::streaming::{LoopEvent, LoopEventKind};

    let mut out = Vec::new();
    let parsed: Value = match serde_json::from_str(event.data) {
        Ok(v) => v,
        Err(_) => return out,
    };
    let choices = match parsed.get("choices").and_then(|c| c.as_array()) {
        Some(arr) if !arr.is_empty() => arr,
        _ => return out,
    };
    let delta = match choices[0].get("delta") {
        Some(d) => d,
        None => return out,
    };

    if let Some(text) = delta.get("content").and_then(|c| c.as_str())
        && !text.is_empty()
    {
        out.push(LoopEvent {
            sequence,
            kind: LoopEventKind::OutputTextDelta,
            data: serde_json::json!({"delta": text}),
        });
    }

    if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
        for call in tool_calls {
            if let Some(args) = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                && !args.is_empty()
            {
                let call_id = call
                    .get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("call_unknown");
                out.push(LoopEvent {
                    sequence,
                    kind: LoopEventKind::FunctionCallArgumentsDelta,
                    data: serde_json::json!({"call_id": call_id, "delta": args}),
                });
            }
        }
    }

    out
}

fn translate_tool_error(e: ToolError) -> ExecutorError {
    match e {
        ToolError::NotFound(name) => ExecutorError::NotFound(name),
        ToolError::ExecutionError(msg)
        | ToolError::InvalidArguments(msg)
        | ToolError::Timeout(msg) => ExecutorError::ExecutionError(msg),
    }
}

fn error_to_payload(e: &LoopError) -> Value {
    match e {
        LoopError::Failed(payload) => payload.clone(),
        LoopError::MaxIterationsExceeded => serde_json::json!({
            "type": "max_iterations_exceeded",
            "message": e.to_string(),
        }),
        LoopError::MaxDepthExceeded => serde_json::json!({
            "type": "max_depth_exceeded",
            "message": e.to_string(),
        }),
        LoopError::EmptyAction => serde_json::json!({
            "type": "empty_action",
            "message": e.to_string(),
        }),
        LoopError::Store(err) => serde_json::json!({
            "type": "store_error",
            "message": err.to_string(),
        }),
        LoopError::Executor(err) => serde_json::json!({
            "type": "executor_error",
            "message": err.to_string(),
        }),
    }
}
