//! Respond-first SSE for the blocking flex streaming surfaces.
//!
//! Flex requests are daemon-processed and can sit queued for a long time, so the
//! handler returns `200 text/event-stream` immediately and polls the daemon
//! inside the stream, rendering the terminal result into SSE frames when it
//! lands. Shared by the chat-completions and responses flex streaming handlers
//! in `inference/middleware.rs`.

use std::sync::Arc;

use axum::response::sse::{Event, KeepAlive, Sse};

use crate::inference::store::AbandonGuard;
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
/// If the client disconnects while we are still polling, the request is
/// cancelled (see [`AbandonGuard`]): nobody will collect the result, and
/// leaving it queued or in flight only spends engine time. The same happens
/// when the poll itself gives up, since the client has by then been answered
/// with an error frame. The guard is armed before the enqueue so a disconnect
/// during the INSERT still cancels a row that committed.
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

    let abandon_guard = AbandonGuard::arm(request_manager.clone(), request_id);

    // Enqueue synchronously so an enqueue failure is a clean JSON 500 — it
    // happens before the stream opens, so we're not yet committed to a 200.
    // The guard is left armed on this path too: the cancel is a no-op if the
    // row never made it in, and covers a commit that raced the error.
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
        let result = tokio::select! {
            result = crate::inference::store::poll_until_terminal(&request_manager, request_id, poll_interval, timeout, keystore.as_ref()) => result,
            // The receiver lives in the response body; it is dropped when the
            // client goes away.
            _ = tx.closed() => {
                tracing::info!(request_id = %request_id, "Client disconnected before flex request finished");
                drop(abandon_guard); // cancels the row
                return;
            }
        };

        let frames = match &result {
            Ok(detail) => render(Ok(detail)),
            Err(e) => {
                tracing::error!(error = %e, request_id = %request_id, "Streaming flex poll failed");
                render(Err(&e.to_string()))
            }
        };
        match result {
            Ok(_) => abandon_guard.disarm(),
            Err(_) => drop(abandon_guard), // cancels the row
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use fusillade_arsenal::PostgresRequestManager;
    use sqlx_pool_router::TestDbPools;

    fn flex_input(request_id: uuid::Uuid) -> fusillade::CreateFlexInput {
        fusillade::CreateFlexInput {
            request_id,
            body: r#"{"model":"m","messages":[]}"#.to_string(),
            model: "m".to_string(),
            endpoint: "http://localhost/ai".to_string(),
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            api_key: "k".to_string(),
            created_by: "owner".to_string(),
            metadata: None,
        }
    }

    async fn request_state(pool: &sqlx::PgPool, request_id: uuid::Uuid) -> String {
        sqlx::query_scalar("SELECT state FROM requests WHERE id = $1")
            .bind(request_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn dropping_the_stream_cancels_the_flex_request(pool: sqlx::PgPool) {
        let fusillade_pool = crate::test::utils::setup_fusillade_pool(&pool).await;
        let request_manager = Arc::new(PostgresRequestManager::new(
            TestDbPools::new(fusillade_pool.clone()).await.unwrap(),
            Default::default(),
        ));
        let request_id = uuid::Uuid::new_v4();

        let response = flex_stream_response(request_manager, flex_input(request_id), request_id, true, None, |_| Vec::new()).await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(request_state(&fusillade_pool, request_id).await, "pending");

        // Client disconnect: the SSE body (and its receiver) is dropped.
        drop(response);

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let state = request_state(&fusillade_pool, request_id).await;
            if state == "canceled" {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "request stayed {state} after the stream was dropped"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}
