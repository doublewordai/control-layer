//! Warm-path SSE handler for `/v1/responses` requests with `stream:
//! true`.
//!
//! When the user sends `stream: true` (and `background: false`), the
//! POST handler can't enqueue a daemon job — the user's HTTP connection
//! must stay open while tokens flow through. So this module owns the
//! "inline" execution path:
//!
//! 1. Insert the fusillade `requests` row directly in `processing`
//!    state with our daemon_id as owner, so the batch daemon doesn't
//!    pick it up.
//! 2. Open an axum SSE response held by the caller.
//! 3. Spawn a task that runs [`onwards::run_response_loop`] inline
//!    with an [`SseEventSink`] wrapping a tokio mpsc.
//! 4. Each [`onwards::LoopEvent`] from the loop becomes one
//!    `axum::response::sse::Event` on the SSE response.
//! 5. When the loop terminates, transition the parent row to
//!    `completed` (or `failed`), close the SSE channel.
//!
//! The path matches the existing realtime single-step path's row
//! ownership pattern (rows in `processing`, not claimable by the
//! daemon) — see `responses/middleware.rs::handle_realtime`. The
//! difference is the work runs inline in this process instead of
//! being proxied through onwards.
//!
//! Reconnect mid-stream is not handled here; the warm-path stream
//! is a once-off live feed. A reconnect-with-Last-Event-ID cold
//! path would walk the chain on a new GET endpoint and replay
//! events from the persisted step rows. That's a follow-up.

use std::sync::Arc;

use async_trait::async_trait;
use axum::response::sse::{Event, KeepAlive, Sse};
use fusillade::ReqwestHttpClient;
use futures::stream::Stream;
use onwards::traits::RequestContext;
use onwards::{EventSink, EventSinkError, LoopConfig, LoopError, LoopEvent, MultiStepStore, UpstreamTarget};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::inference::store::FusilladeResponseStore;
use crate::inference::tools::HttpToolExecutor;

/// Buffer size for the SSE event channel between the loop and the
/// HTTP response stream. Sized for the largest expected per-iteration
/// burst (a model_call's chunks don't queue up — they flush as fast as
/// the network sends them — but the buffer prevents
/// stalls if the client TCP socket back-pressures momentarily).
const SSE_CHANNEL_BUFFER: usize = 256;

/// Sink that translates [`LoopEvent`]s into axum SSE [`Event`]s and
/// pushes them onto a tokio mpsc.
pub struct SseEventSink {
    tx: mpsc::Sender<Result<Event, axum::Error>>,
}

impl SseEventSink {
    pub fn new(tx: mpsc::Sender<Result<Event, axum::Error>>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl EventSink for SseEventSink {
    async fn emit(&self, event: LoopEvent) -> Result<(), EventSinkError> {
        let data_str = serde_json::to_string(&event.data).map_err(|e| EventSinkError(format!("serialize SSE data: {e}")))?;
        let sse_event = Event::default()
            .id(event.sequence.to_string())
            .event(event.kind.as_str())
            .data(data_str);
        self.tx
            .send(Ok(sse_event))
            .await
            .map_err(|e| EventSinkError(format!("SSE channel closed: {e}")))
    }
}

/// Buffer for the flex replay channel. A flex replay emits only a handful
/// of frames (role/content/finish, or created/completed), so this is tiny —
/// it exists only to decouple the poll task from the HTTP writer.
const FLEX_REPLAY_BUFFER: usize = 16;

/// One rendered SSE frame for the **flex replay** paths.
///
/// Flex tiers are daemon-processed: by the time the result exists it is
/// already complete, so there is no live loop to forward. When a flex
/// client asked for `stream:true` we render the finished result into a
/// sequence of these frames and emit them via [`flex_stream_response`].
///
/// `event` is the SSE `event:` name — `None` emits an unnamed `data:` frame
/// (the chat-completions chunk shape), `Some` names the event (`response.*`
/// for the Responses surface). `data` is the JSON payload.
pub struct ReplayFrame {
    pub event: Option<&'static str>,
    pub data: Value,
}

impl ReplayFrame {
    /// Unnamed `data:`-only frame — the chat-completions chunk shape.
    pub fn unnamed(data: Value) -> Self {
        Self { event: None, data }
    }

    /// Named event frame — the Responses `response.*` shape.
    pub fn named(event: &'static str, data: Value) -> Self {
        Self { event: Some(event), data }
    }
}

/// Respond-first SSE for the blocking flex streaming surfaces
/// (chat-completions and responses).
///
/// Flex is daemon-processed and can sit queued for a long time, so we return
/// `200 text/event-stream` immediately and poll the daemon *inside* the
/// stream. axum's [`KeepAlive`] injects `:` comments while we wait, keeping
/// the client connection warm past idle timeouts (a poll-then-respond design
/// would send no bytes — not even headers — until the daemon finished, and a
/// client idle timeout could fire first).
///
/// When the request reaches a terminal state, `render` turns the outcome —
/// `Ok(detail)` on a terminal row, `Err(msg)` on timeout/poll failure — into
/// the frames to emit: success chunks/events on 2xx, an in-stream error frame
/// otherwise. Errors are delivered *down the stream*, not as an HTTP status,
/// because the `200` was already committed.
///
/// Enqueue failure is the one exception: it happens before any byte is sent,
/// so it still returns a clean JSON `500`.
///
/// `done_sentinel` appends a trailing `data: [DONE]` (the chat-completions
/// terminator); the Responses surface ends on `response.completed`/`.failed`
/// and passes `false`.
pub async fn flex_stream_response<P, F>(
    request_manager: Arc<fusillade::PostgresRequestManager<P, ReqwestHttpClient>>,
    flex_input: fusillade::CreateFlexInput,
    request_id: uuid::Uuid,
    done_sentinel: bool,
    keystore: Option<crate::keystore::Keystore>,
    render: F,
) -> axum::response::Response
where
    P: fusillade::PoolProvider + Clone + Send + Sync + 'static,
    F: FnOnce(Result<&fusillade::RequestDetail, &str>) -> Vec<ReplayFrame> + Send + 'static,
{
    use axum::response::IntoResponse;

    // Enqueue synchronously so an enqueue failure is a clean JSON 500 — it
    // happens before the stream opens, so we're not yet committed to a 200.
    if let Err(e) = fusillade::Storage::create_flex(&*request_manager, flex_input).await {
        tracing::error!(error = %e, "Failed to create streaming flex batch in fusillade");
        return axum::response::Response::builder()
            .status(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({
                    "error": { "message": "Failed to enqueue request", "type": "server_error", "code": 500 }
                })
                .to_string(),
            ))
            .unwrap();
    }

    let (tx, rx) = mpsc::channel::<Result<Event, std::convert::Infallible>>(FLEX_REPLAY_BUFFER);

    // Poll task: the HTTP response is already returning; this fills the stream
    // once the daemon reaches a terminal state. Until then the channel is idle
    // and axum's keep-alive holds the connection open.
    tokio::spawn(async move {
        let poll_interval = std::time::Duration::from_millis(500);
        let timeout = std::time::Duration::from_secs(3600);
        let result = crate::inference::store::poll_until_terminal(&request_manager, request_id, poll_interval, timeout, keystore.as_ref()).await;

        let frames = match &result {
            Ok(detail) => render(Ok(detail)),
            Err(e) => {
                tracing::error!(error = %e, request_id = %request_id, "Streaming flex poll failed");
                render(Err(&e.to_string()))
            }
        };

        for frame in frames {
            let mut event = Event::default().data(frame.data.to_string());
            if let Some(name) = frame.event {
                event = event.event(name);
            }
            if tx.send(Ok(event)).await.is_err() {
                return; // client disconnected
            }
        }
        if done_sentinel {
            let _ = tx.send(Ok(Event::default().data("[DONE]"))).await;
        }
    });

    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default()).into_response()
}

/// Run the multi-step loop inline against an SSE response.
///
/// Called by the inference middleware when the user requested
/// `stream: true` (and not `background: true`). Returns an axum
/// `Sse<Stream>` ready to be sent as the HTTP response.
///
/// `request_id` is the pre-allocated UUID for the parent fusillade
/// row; the caller is responsible for inserting that row in
/// `processing` state with the appropriate daemon_id before invoking
/// this function so the batch daemon doesn't double-claim.
#[allow(clippy::too_many_arguments)]
pub fn run_inline_streaming<P>(
    response_store: Arc<FusilladeResponseStore<P>>,
    tool_executor: Arc<HttpToolExecutor>,
    tool_resolved: Arc<crate::inference::tools::ResolvedToolSet>,
    http_client: Arc<ReqwestHttpClient>,
    upstream: UpstreamTarget,
    loop_config: LoopConfig,
    request_id: String,
    model_alias: String,
) -> Sse<impl Stream<Item = Result<Event, axum::Error>>>
where
    P: fusillade::PoolProvider + Clone + Send + Sync + 'static,
{
    let (tx, rx) = mpsc::channel::<Result<Event, axum::Error>>(SSE_CHANNEL_BUFFER);

    // Spawn the loop runner. The HTTP response holds the rx side of the
    // channel; when the loop completes (Ok or Err), we drop tx and the
    // SSE response naturally closes.
    tokio::spawn(async move {
        let sink = SseEventSink::new(tx.clone());
        let tool_ctx = RequestContext::new()
            .with_model(model_alias)
            .with_extension(crate::inference::tools::ResolvedTools(tool_resolved));

        let result = onwards::run_response_loop(
            &*response_store,
            &*tool_executor,
            &tool_ctx,
            &upstream,
            http_client,
            Some(&sink),
            &request_id,
            None,
            loop_config,
            0,
        )
        .await;

        // Persist the parent fusillade row's terminal state. The loop
        // already emitted its own response.completed / response.failed
        // event to the SSE stream; this is just for GET retrieval and
        // analytics.
        match &result {
            Ok(_) => {
                if let Err(e) = persist_terminal_completed(&response_store, &request_id).await {
                    tracing::warn!(error = %e, "Failed to persist warm-path terminal state");
                    // Try to surface to the client via the sink as a
                    // best-effort followup; ignore send errors.
                    let _ = tx
                        .send(Ok(Event::default()
                            .event("response.failed")
                            .data(format!("{{\"type\":\"persist_failed\",\"message\":\"{e}\"}}"))))
                        .await;
                }
            }
            Err(LoopError::Failed(payload)) => {
                if let Err(e) = persist_terminal_failed(&response_store, &request_id, payload).await {
                    tracing::warn!(error = %e, "Failed to persist warm-path failure state");
                }
            }
            Err(other) => {
                let payload = serde_json::json!({
                    "type": "loop_error",
                    "message": other.to_string(),
                });
                if let Err(e) = persist_terminal_failed(&response_store, &request_id, &payload).await {
                    tracing::warn!(error = %e, "Failed to persist warm-path error state");
                }
            }
        }

        // The pending input was registered by warm_path_setup so the
        // bridge could re-parse the user body on every iteration.
        // The loop has terminated — drop the side-channel entry so the
        // map doesn't grow unbounded.
        response_store.unregister_pending(&request_id);

        drop(tx);
    });

    let stream = ReceiverStream::new(rx);
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn persist_terminal_completed<P>(response_store: &FusilladeResponseStore<P>, request_id: &str) -> Result<(), String>
where
    P: fusillade::PoolProvider + Clone + Send + Sync + 'static,
{
    // Assemble the final response JSON from the chain (same path the
    // daemon processor uses), then write it onto the head step's
    // sub-request fusillade row — the listing-visible row for this
    // response. There's no longer a parent /v1/responses row to
    // finalize after the schema re-anchoring (fusillade 16.8).
    let assembled = response_store
        .assemble_response(request_id)
        .await
        .map_err(|e| format!("assemble: {e}"))?;
    response_store
        .finalize_head_request(request_id, 200, assembled)
        .await
        .map_err(|e| format!("finalize head: {e}"))
}

/// Run the multi-step loop inline and return the final assembled
/// response as a single JSON value. Used by the warm-path blocking
/// handler when the user requested `stream: false, background: false`
/// on `/v1/responses` — we still need full multi-step orchestration
/// (tools, sub-agents) but the user expects one HTTP response, not an
/// SSE stream.
///
/// On success, returns the final response JSON; on failure, returns
/// the loop's error payload as JSON. Persistence of the parent
/// fusillade row happens here (same `complete_request` /
/// `fail_request` calls the streaming path uses) so subsequent
/// `GET /v1/responses/{id}` retrievals see the same data.
#[allow(clippy::too_many_arguments)]
pub async fn run_inline_blocking<P>(
    response_store: Arc<FusilladeResponseStore<P>>,
    tool_executor: Arc<HttpToolExecutor>,
    tool_resolved: Arc<crate::inference::tools::ResolvedToolSet>,
    http_client: Arc<ReqwestHttpClient>,
    upstream: UpstreamTarget,
    loop_config: LoopConfig,
    request_id: String,
    model_alias: String,
) -> Result<Value, Value>
where
    P: fusillade::PoolProvider + Clone + Send + Sync + 'static,
{
    let tool_ctx = RequestContext::new()
        .with_model(model_alias)
        .with_extension(crate::inference::tools::ResolvedTools(tool_resolved));

    let result = onwards::run_response_loop(
        &*response_store,
        &*tool_executor,
        &tool_ctx,
        &upstream,
        http_client,
        None,
        &request_id,
        None,
        loop_config,
        0,
    )
    .await;

    let outcome = match result {
        Ok(_) => {
            if let Err(e) = persist_terminal_completed(&response_store, &request_id).await {
                tracing::warn!(error = %e, "Failed to persist warm-path-blocking terminal state");
            }
            response_store
                .assemble_response(&request_id)
                .await
                .map_err(|e| serde_json::json!({"type": "assemble_failed", "message": e.to_string()}))
        }
        Err(LoopError::Failed(payload)) => {
            if let Err(e) = persist_terminal_failed(&response_store, &request_id, &payload).await {
                tracing::warn!(error = %e, "Failed to persist warm-path-blocking failure");
            }
            Err(payload)
        }
        Err(other) => {
            let payload = serde_json::json!({
                "type": "loop_error",
                "message": other.to_string(),
            });
            if let Err(e) = persist_terminal_failed(&response_store, &request_id, &payload).await {
                tracing::warn!(error = %e, "Failed to persist warm-path-blocking error");
            }
            Err(payload)
        }
    };

    // Drop the side-channel entry — it was registered by
    // warm_path_setup so the bridge could re-parse the user body on
    // every iteration; the loop has terminated.
    response_store.unregister_pending(&request_id);
    outcome
}

async fn persist_terminal_failed<P>(response_store: &FusilladeResponseStore<P>, request_id: &str, error: &Value) -> Result<(), String>
where
    P: fusillade::PoolProvider + Clone + Send + Sync + 'static,
{
    response_store
        .finalize_head_request(request_id, 500, error.clone())
        .await
        .map_err(|e| format!("finalize head: {e}"))
}
