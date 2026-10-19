//! Respond-first SSE for the blocking flex streaming surfaces.
//!
//! Flex requests are daemon-processed and can sit queued for a long time, so the
//! handler returns `200 text/event-stream` immediately and polls the daemon
//! inside the stream, rendering the terminal result into SSE frames when it
//! lands. Shared by the chat-completions and responses flex streaming handlers
//! in `inference/middleware.rs`.

use std::sync::Arc;

use axum::response::sse::{Event, KeepAlive, Sse};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Buffer size for the flex replay channel: a finished request renders to a
/// small, bounded set of frames, so a shallow buffer is enough.
const FLEX_REPLAY_BUFFER: usize = 16;

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
pub async fn flex_stream_response<P, F>(
    request_manager: Arc<fusillade_arsenal::PostgresRequestManager<P>>,
    flex_input: fusillade::CreateFlexInput,
    request_id: uuid::Uuid,
    done_sentinel: bool,
    keystore: Option<crate::keystore::Keystore>,
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

    // Poll task: the HTTP response is already returning; this fills the stream
    // once the daemon reaches a terminal state. Until then the channel is idle
    // and axum's keep-alive holds the connection open.
    tokio::spawn(async move {
        let poll_interval = std::time::Duration::from_millis(500);
        let timeout = std::time::Duration::from_secs(3600);
        let result =
            crate::inference::store::poll_until_terminal(&request_manager, request_id, poll_interval, timeout, keystore.as_ref()).await;

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
