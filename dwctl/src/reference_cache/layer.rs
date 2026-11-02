//! The first-token hook: makes a streaming chat completion's prefixes visible to the
//! reference cache when its first chunk passes, instead of waiting for completion.
//!
//! Sits just inside outlet, so it sees the request exactly as outlet (and so the
//! analytics handler, which computes the estimate) captures it, and hashes the same bytes.
//! The request is never altered or delayed: the body is buffered (outlet already holds it
//! in full) and passed on unchanged, and the response is only observed.
//!
//! "First token" is the first response frame carrying a `"choices"` member. A frame
//! carrying `"error"` before that disarms the hook, so a stream that fails before
//! producing output writes nothing. Non-streaming responses are left to the completion
//! write.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http_body::{Frame, SizeHint};

use super::ReferenceCache;

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
        .is_some_and(|v| v.starts_with("text/event-stream"))
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
    Response::from_parts(
        parts,
        Body::new(FirstTokenBody {
            inner,
            on_first_token: Some(on_first_token),
        }),
    )
}

struct FirstTokenBody {
    inner: Body,
    /// Taken on the first chunk with output, or dropped on an earlier error frame.
    on_first_token: Option<Box<dyn FnOnce() + Send>>,
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
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
            if contains(data, b"\"choices\"") {
                if let Some(f) = self.on_first_token.take() {
                    f();
                }
            } else if contains(data, b"\"error\"") {
                self.on_first_token = None;
            }
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
            FirstTokenBody {
                inner: Body::from_stream(stream),
                on_first_token: Some(Box::new(move || {
                    f.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                })),
            },
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
}
