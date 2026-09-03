//! Respond-first SSE for the blocking flex streaming surfaces.
//!
//! Flex requests are daemon-processed and can sit queued for a long time, so the
//! handler returns `200 text/event-stream` immediately and polls the daemon
//! inside the stream, rendering the terminal result into SSE frames when it
//! lands. Shared by the chat-completions and responses flex streaming handlers
//! in `inference/middleware.rs`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::response::sse::{Event, KeepAlive, Sse};
use futures::StreamExt;
use onwards::strict::schemas::chat_completions::{ChatCompletionChunk, normalize_chat_completion_chunk_value};
use serde_json::Value;
use tokio::sync::{Notify, mpsc};
use tokio_stream::wrappers::ReceiverStream;

use super::translation::responses::streaming::{StreamingEvent, StreamingState};
use super::translation::responses::types::ResponsesRequest;
use super::zdr;

/// Buffer size for the flex replay channel: a finished request renders to a
/// small, bounded set of frames, so a shallow buffer is enough.
const FLEX_REPLAY_BUFFER: usize = 16;

/// `None` means `flex_stream_response` behaves exactly as before: a single
/// 500ms poll task, no relay subscriber.
#[derive(Clone)]
pub struct LiveRelayConfig {
    pub relay: crate::chunk_relay::ChunkRelay,
    /// Correctness fallback while the relay is primary — deliberately much
    /// slower than the 500ms used when live streaming is off.
    pub poll_fallback_interval: std::time::Duration,
    /// `Some` on the Responses surface: the client's own request plus its
    /// tracking id, used to reframe each relayed chunk into `response.*`
    /// events. Relayed chunks are Chat Completions shaped (Responses reaches
    /// `outbound_request` already translated to that shape — see its module
    /// docs), so they need the same reframing `ResponsesStreamReframer` gives
    /// the live warm path. `None` forwards chunks as-is (chat-completions).
    pub responses_reframe: Option<(ResponsesRequest, String)>,
}

/// Ensures only one of {poll task, relay task} sends the terminal frame(s).
struct Terminal {
    claimed: AtomicBool,
    notify: Notify,
}

impl Terminal {
    fn new() -> Self {
        Self {
            claimed: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    /// `true` for exactly one caller, ever; wakes anyone parked in
    /// `claimed_by_other` so they stand down immediately.
    fn claim(&self) -> bool {
        let already_claimed = self.claimed.swap(true, Ordering::SeqCst);
        if !already_claimed {
            self.notify.notify_waiters();
        }
        !already_claimed
    }

    /// The `Notified` future must be created before checking `claimed`:
    /// `notify_waiters()` only wakes already-registered waiters, so
    /// checking first can miss a concurrent `claim()`.
    async fn claimed_by_other(&self) {
        let notified = self.notify.notified();
        if self.claimed.load(Ordering::SeqCst) {
            return;
        }
        notified.await;
    }
}

/// One SSE frame to replay once a flex request reaches a terminal state.
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
///
/// `live_relay`, when set, races a relay task against the poll task (see
/// [`Terminal`]); `None` is exactly today's behavior.
pub async fn flex_stream_response<P, F>(
    request_manager: Arc<fusillade_arsenal::PostgresRequestManager<P>>,
    flex_input: fusillade::CreateFlexInput,
    request_id: uuid::Uuid,
    done_sentinel: bool,
    keystore: Option<crate::keystore::Keystore>,
    live_relay: Option<LiveRelayConfig>,
    render: F,
) -> axum::response::Response
where
    P: fusillade_arsenal::PoolProvider + Clone + Send + Sync + 'static,
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
    let terminal = Arc::new(Terminal::new());

    // 500ms when live streaming is off; the slower configured fallback once
    // a relay is racing it.
    let poll_interval = live_relay
        .as_ref()
        .map(|lr| lr.poll_fallback_interval)
        .unwrap_or(std::time::Duration::from_millis(500));

    if let Some(live_relay) = live_relay {
        let tx = tx.clone();
        let keystore = keystore.clone();
        let terminal = terminal.clone();
        tokio::spawn(run_live_relay(
            live_relay.relay,
            request_id,
            keystore,
            tx,
            terminal,
            done_sentinel,
            live_relay.responses_reframe,
        ));
    }

    // Poll task: the HTTP response is already returning; this fills the stream
    // once the daemon reaches a terminal state. Until then the channel is idle
    // and axum's keep-alive holds the connection open.
    //
    // Races against `terminal.claimed_by_other()` so a relay-delivered
    // response doesn't sit open until this task's own next poll tick.
    tokio::spawn(async move {
        let timeout = std::time::Duration::from_secs(3600);

        let result = tokio::select! {
            biased;

            _ = terminal.claimed_by_other() => return,

            result = crate::inference::store::poll_until_terminal(&request_manager, request_id, poll_interval, timeout, keystore.as_ref()) => result,
        };

        if !terminal.claim() {
            return;
        }

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

/// Subscribes to a request's relayed chunks and forwards them live,
/// decrypting first when ZDR. Races against `terminal` so it never
/// outlives the request even if the relay never produces a `done`.
async fn run_live_relay(
    relay: crate::chunk_relay::ChunkRelay,
    request_id: uuid::Uuid,
    keystore: Option<crate::keystore::Keystore>,
    tx: mpsc::Sender<Result<Event, std::convert::Infallible>>,
    terminal: Arc<Terminal>,
    done_sentinel: bool,
    responses_reframe: Option<(ResponsesRequest, String)>,
) {
    // Non-destructive fetch; `None` for a non-ZDR request.
    let zdr_key = match &keystore {
        Some(ks) => ks.get(&zdr::key_id(&request_id, zdr::KeyKind::Response)).await.ok().flatten(),
        None => None,
    };

    // On the Responses surface, relayed chunks are Chat Completions shaped and
    // need reframing into `response.*` events, mirroring `ResponsesStreamReframer`.
    let mut reframe = responses_reframe.map(|(req, response_id)| {
        let model = req.model.clone();
        let state = StreamingState::new(&req, Some(&response_id));
        (state, model, response_id)
    });

    let mut stream = relay.subscribe(request_id);

    loop {
        tokio::select! {
            biased;

            _ = terminal.claimed_by_other() => {
                return;
            }

            msg = stream.next() => {
                let Some(msg) = msg else {
                    return; // relay dropped the subscriber; poll fallback covers it
                };

                if msg.done {
                    if !terminal.claim() {
                        return;
                    }
                    match reframe.as_mut() {
                        // Nothing in the raw relayed stream says "response
                        // complete" — that only exists once reframed, so
                        // finalize() (response.completed/output_item.done etc.)
                        // has to run here rather than arriving as a message.
                        Some((state, _, _)) => {
                            for event in state.finalize() {
                                if send_streaming_event(&tx, &event).await.is_err() {
                                    return;
                                }
                            }
                        }
                        None if done_sentinel => {
                            let _ = tx.send(Ok(Event::default().data("[DONE]"))).await;
                        }
                        None => {}
                    }
                    return;
                }

                let payload = match &zdr_key {
                    Some(key) => match zdr::decrypt_body(key, &msg.data) {
                        Ok(plaintext) => plaintext,
                        Err(e) => {
                            tracing::debug!(%request_id, error = %e, "failed to decrypt relayed chunk, skipping");
                            continue;
                        }
                    },
                    None => msg.data,
                };

                let Ok(mut parsed) = serde_json::from_str::<Value>(&payload) else {
                    tracing::debug!(%request_id, "relayed chunk was not valid JSON, skipping");
                    continue;
                };

                match reframe.as_mut() {
                    Some((state, model, fallback_id)) => {
                        normalize_chat_completion_chunk_value(&mut parsed, model, fallback_id);
                        let Ok(chunk) = serde_json::from_value::<ChatCompletionChunk>(parsed) else {
                            tracing::debug!(%request_id, "relayed chunk did not parse as a chat completion chunk, skipping");
                            continue;
                        };
                        for event in state.process_chunk(&chunk) {
                            if send_streaming_event(&tx, &event).await.is_err() {
                                return;
                            }
                        }
                    }
                    None => {
                        if tx.send(Ok(Event::default().data(parsed.to_string()))).await.is_err() {
                            return; // client disconnected
                        }
                    }
                }
            }
        }
    }
}

async fn send_streaming_event(tx: &mpsc::Sender<Result<Event, std::convert::Infallible>>, event: &StreamingEvent) -> Result<(), ()> {
    let data = serde_json::to_string(event).unwrap_or_default();
    tx.send(Ok(Event::default().event(event.event_type.clone()).data(data)))
        .await
        .map_err(|_| ())
}
