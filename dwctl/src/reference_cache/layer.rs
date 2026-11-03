//! The first-token hook: makes a streaming chat completion's prefixes visible to the
//! reference cache when its first chunk passes, instead of waiting for completion.
//!
//! Sits just inside outlet, so it sees the request exactly as outlet (and so the
//! analytics handler, which computes the estimate) captures it, and hashes the same bytes.
//! The request is never altered or delayed: the body is buffered (outlet already holds it
//! in full) and passed on unchanged, and the response is only observed.
//!
//! "First token" is the first SSE event carrying a `"choices"` member. An event carrying
//! `"error"` before that disarms the hook, so a stream that fails before producing output
//! writes nothing. Events are reassembled across body frames and judged in order, so a
//! split marker is still found and an error coalesced ahead of output still disarms.
//! Non-streaming responses are left to the completion write.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bytes::BytesMut;
use http_body::{Frame, SizeHint};

use super::ReferenceCache;

/// Bytes of an unfinished event held while waiting for its end. Opening events are small;
/// past this the hook gives up and the completion write makes the request visible.
const MAX_PENDING_EVENT: usize = 64 * 1024;

/// State for [`reference_cache_middleware`].
#[derive(Clone)]
pub struct ReferenceLayerState {
    pub cache: Arc<ReferenceCache>,
    /// Max bytes to buffer; the same limit the rest of the inference path enforces.
    pub body_limit: usize,
}

fn bearer_token(req: &Request) -> Option<String> {
    req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(String::from)
}

fn is_event_stream(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|ct| ct.trim().eq_ignore_ascii_case("text/event-stream"))
}

pub async fn reference_cache_middleware(State(state): State<ReferenceLayerState>, req: Request, next: Next) -> Response {
    if req.method() != Method::POST || !req.uri().path().ends_with("/chat/completions") {
        return next.run(req).await;
    }
    let Some(token) = bearer_token(&req) else {
        return next.run(req).await;
    };
    let (parts, body) = req.into_parts();
    let body = match axum::body::to_bytes(body, state.body_limit).await {
        Ok(b) => b,
        Err(e) => {
            // The outer body limit already rejected anything oversized, so this is a
            // broken body. Answer as the other buffering layers do.
            let body = serde_json::json!({
                "error": {
                    "message": format!("failed to read request body: {e}"),
                    "type": "invalid_request_error",
                }
            });
            return (axum::http::StatusCode::BAD_REQUEST, axum::Json(body)).into_response();
        }
    };
    let response = next.run(Request::from_parts(parts, Body::from(body.clone()))).await;
    if !response.status().is_success() || !is_event_stream(&response) {
        return response;
    }
    let (parts, inner) = response.into_parts();
    let cache = state.cache;
    let on_first_token: Box<dyn FnOnce() + Send> = Box::new(move || cache.observe_first_token(token, body));
    Response::from_parts(parts, Body::new(FirstTokenBody::new(inner, on_first_token)))
}

struct FirstTokenBody {
    inner: Body,
    /// Taken on the first event with output, or dropped on an earlier error event.
    on_first_token: Option<Box<dyn FnOnce() + Send>>,
    /// The unfinished event so far, while armed. Frames are forwarded untouched; this is
    /// only a copy for inspection.
    pending: BytesMut,
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// End of the first complete event in `buf` (past its blank-line terminator).
fn event_end(buf: &[u8]) -> Option<usize> {
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|i| i + 2);
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

impl FirstTokenBody {
    fn new(inner: Body, on_first_token: Box<dyn FnOnce() + Send>) -> Self {
        Self {
            inner,
            on_first_token: Some(on_first_token),
            pending: BytesMut::new(),
        }
    }

    fn observe(&mut self, data: &[u8]) {
        self.pending.extend_from_slice(data);
        while self.on_first_token.is_some()
            && let Some(end) = event_end(&self.pending)
        {
            let event = self.pending.split_to(end);
            if contains(&event, b"\"choices\"") {
                if let Some(f) = self.on_first_token.take() {
                    f();
                }
            } else if contains(&event, b"\"error\"") {
                self.on_first_token = None;
            }
        }
        if self.on_first_token.is_none() || self.pending.len() > MAX_PENDING_EVENT {
            self.on_first_token = None;
            self.pending = BytesMut::new();
        }
    }
}

impl http_body::Body for FirstTokenBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let poll = Pin::new(&mut self.inner).poll_frame(cx);
        if self.on_first_token.is_some()
            && let Poll::Ready(Some(Ok(frame))) = &poll
            && let Some(data) = frame.data_ref()
        {
            let data = data.clone();
            self.observe(&data);
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn tracked(frames: Vec<&'static str>) -> (FirstTokenBody, Arc<std::sync::atomic::AtomicUsize>) {
        let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let f = fired.clone();
        let stream = futures::stream::iter(
            frames
                .into_iter()
                .map(|s| Ok::<_, std::io::Error>(Bytes::from_static(s.as_bytes()))),
        );
        (
            FirstTokenBody::new(
                Body::from_stream(stream),
                Box::new(move || {
                    f.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }),
            ),
            fired,
        )
    }

    async fn drain(body: FirstTokenBody) -> Vec<Bytes> {
        Body::new(body).into_data_stream().map(|r| r.unwrap()).collect().await
    }

    #[tokio::test]
    async fn fires_once_on_the_first_chunk_with_output_and_passes_frames_through() {
        let (body, fired) = tracked(vec![
            ": keep-alive\n\n",
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
            "data: [DONE]\n\n",
        ]);
        let frames = drain(body).await;
        assert_eq!(frames.len(), 4, "frames are forwarded untouched");
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_error_before_any_output_disarms() {
        let (body, fired) = tracked(vec![
            "data: {\"error\":{\"message\":\"overloaded\"}}\n\n",
            "data: {\"choices\":[]}\n\n",
        ]);
        drain(body).await;
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_error_after_output_does_not_undo_it() {
        let (body, fired) = tracked(vec!["data: {\"choices\":[]}\n\n", "data: {\"error\":{\"message\":\"cut\"}}\n\n"]);
        drain(body).await;
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_event_split_across_frames_still_fires() {
        let (body, fired) = tracked(vec!["data: {\"cho", "ices\":[{\"delta\":{\"content\":\"x\"}}]}", "\r\n\r\n"]);
        let frames = drain(body).await;
        assert_eq!(frames.len(), 3, "frames are forwarded as they came");
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_error_coalesced_ahead_of_output_disarms() {
        let (body, fired) = tracked(vec!["data: {\"error\":{\"message\":\"overloaded\"}}\n\ndata: {\"choices\":[]}\n\n"]);
        drain(body).await;
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_unterminated_oversized_event_gives_up() {
        let big: &'static str = Box::leak(format!("data: {{\"pad\":\"{}\"", "x".repeat(MAX_PENDING_EVENT)).into_boxed_str());
        let (body, fired) = tracked(vec![big, ",\"choices\":[]}\n\n"]);
        drain(body).await;
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 0, "left to the completion write");
    }

    #[test]
    fn the_content_type_match_ignores_case_and_parameters() {
        let with = |ct: &str| {
            let mut r = Response::new(Body::empty());
            r.headers_mut().insert(header::CONTENT_TYPE, ct.parse().unwrap());
            is_event_stream(&r)
        };
        assert!(with("text/event-stream"));
        assert!(with("Text/Event-Stream; charset=utf-8"));
        assert!(!with("text/event-streaming"));
        assert!(!with("application/json"));
    }
}
