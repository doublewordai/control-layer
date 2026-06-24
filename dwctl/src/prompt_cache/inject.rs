//! OpenAI-shaped request sanitisation and response usage injection (plan §5.2/§5.3),
//! relocated into dwctl for the dwctl-owned cache layer (§0).
//!
//! Two jobs, both run by the cache tower layer (only when a cacheable request is
//! classified):
//!
//! 1. **Outbound request sanitisation** ([`strip_cache_control`]): recursively remove
//!    every `cache_control` marker from the request body, and ensure
//!    `stream_options.include_usage = true` so a streaming response carries a terminal
//!    usage frame to edit. Markers are a billing signal consumed here, not forwarded.
//! 2. **Response usage injection** ([`inject_cache_stats_into_response`]): splice the
//!    neutral [`CacheStats`] into the OpenAI `usage` object — `prompt_tokens_details.
//!    cached_tokens` plus the doubleword extension fields. Non-streaming edits the JSON
//!    body; streaming edits *only* the terminal usage frame before `[DONE]`, never
//!    buffering the whole stream.

use std::pin::Pin;
use std::task::{Context, Poll};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::Stream;
use http_body_util::BodyExt;
use serde_json::Value;
use tokio::sync::oneshot;
use tracing::error;

use super::sse::SseBufferedStream;
use super::stats::CacheStats;

/// Whether the cache-index write may be committed — the success signal that must match
/// what billing uses (§0.2). A streamed response is HTTP 200 the moment it opens, so the
/// status alone is not enough: a mid-stream error frame (which billing reclassifies to
/// 500 and bills as zero) must veto the write. The verdict is therefore:
/// `status < 400` **and** no error frame **and** a terminal usage frame was seen.
pub enum CommitGate {
    /// Non-streaming: the verdict is known as soon as the body is buffered.
    Ready(bool),
    /// Streaming: the verdict only settles when the stream drains (or the client
    /// disconnects). The receiver yields `true` iff the stream completed successfully
    /// with a usage frame and no error frame; a dropped sender (task aborted) → `false`.
    Deferred(oneshot::Receiver<bool>),
}

/// Recursively remove all `cache_control` fields from a JSON value. Returns true if any
/// marker was removed.
fn remove_cache_control(value: &mut Value) -> bool {
    let mut removed = false;
    match value {
        Value::Object(map) => {
            if map.remove("cache_control").is_some() {
                removed = true;
            }
            for v in map.values_mut() {
                removed |= remove_cache_control(v);
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                removed |= remove_cache_control(v);
            }
        }
        _ => {}
    }
    removed
}

/// Sanitise an outbound request body: strip every `cache_control` marker and, for
/// streaming requests, ensure `stream_options.include_usage = true`. Returns the
/// rewritten bytes when anything changed, or `None` to leave the original untouched.
pub fn strip_cache_control(body: &[u8]) -> Option<Bytes> {
    let mut json: Value = serde_json::from_slice(body).ok()?;
    let stripped = remove_cache_control(&mut json);

    let mut usage_set = false;
    if let Some(obj) = json.as_object_mut() {
        let is_streaming = obj.get("stream").and_then(Value::as_bool) == Some(true);
        if is_streaming {
            let opts = obj.entry("stream_options").or_insert_with(|| serde_json::json!({}));
            if let Some(opts_obj) = opts.as_object_mut() {
                let already = opts_obj.get("include_usage").and_then(Value::as_bool) == Some(true);
                if !already {
                    opts_obj.insert("include_usage".to_string(), serde_json::json!(true));
                    usage_set = true;
                }
            }
        }
    }

    if stripped || usage_set {
        serde_json::to_vec(&json).ok().map(Bytes::from)
    } else {
        None
    }
}

/// Splice the OpenAI-shaped cache fields (§5.2) into a `usage` object in place.
/// `prompt_tokens` is left as the full input count; only the cache breakdown is added.
fn splice_cache_fields(usage: &mut serde_json::Map<String, Value>, stats: &CacheStats) {
    let details = usage.entry("prompt_tokens_details").or_insert_with(|| serde_json::json!({}));
    if let Some(details_obj) = details.as_object_mut() {
        details_obj.insert("cached_tokens".to_string(), serde_json::json!(stats.read));
    }
    usage.insert("cache_read_input_tokens".to_string(), serde_json::json!(stats.read));
    usage.insert("cache_creation_input_tokens".to_string(), serde_json::json!(stats.creation_total()));
    usage.insert(
        "cache_creation".to_string(),
        serde_json::json!({
            "ephemeral_5m_input_tokens": stats.creation_5m,
            "ephemeral_1h_input_tokens": stats.creation_1h,
            "ephemeral_24h_input_tokens": stats.creation_24h,
        }),
    );
}

/// Inject the cache stats into a non-streaming chat-completion JSON body. Returns the
/// rewritten body, or `None` if it can't be parsed or has no `usage` object.
pub fn inject_into_usage_json(body: &[u8], stats: &CacheStats) -> Option<Bytes> {
    let mut json: Value = serde_json::from_slice(body).ok()?;
    let obj = json.as_object_mut()?;
    let usage = obj.get_mut("usage")?.as_object_mut()?;
    splice_cache_fields(usage, stats);
    serde_json::to_vec(&json).ok().map(Bytes::from)
}

/// The outcome of scanning one SSE body chunk: the (optionally) rewritten bytes plus the
/// two billing-success signals observed in it. Accumulated across chunks by the streaming
/// path so the cache-commit gate matches what billing sees (§0.2).
struct SseScan {
    /// `Some` only if a usage frame was found *and* injected this call.
    rewritten: Option<Bytes>,
    /// A `data:` frame carrying an `error` payload (mid-stream provider failure).
    saw_error: bool,
    /// A `data:` frame carrying a `usage` object (the terminal usage frame).
    saw_usage: bool,
}

/// Scan an SSE body for error/usage frames and, unless `already_edited`, inject the cache
/// fields into the first usage frame found. Editing touches only that one frame; every
/// other line (deltas, `[DONE]`) is preserved byte-for-byte. Assumes uncompressed UTF-8
/// `text/event-stream`; non-UTF-8 bodies are a graceful no-op (no scan, no edit).
///
/// Each `data:` line is parsed as a complete JSON object. The SSE spec permits one object
/// to span several `data:` lines (joined by `\n`), but every OpenAI-compatible
/// chat-completions provider emits one compact line per frame, so we don't reassemble.
/// This deliberately matches the billing-path scanner `extract_cache_tokens`
/// (request_logging::serializers) line-for-line: the commit gate's "saw a usage frame" and
/// billing's "found usage" must make the *same* call, or the cache could commit a write for
/// a frame billing reads as zero. If a multi-line provider ever appears, both must learn to
/// reassemble together — not this one alone.
fn scan_inject_sse(body: &[u8], stats: &CacheStats, already_edited: bool) -> SseScan {
    let Ok(body_str) = std::str::from_utf8(body) else {
        return SseScan {
            rewritten: None,
            saw_error: false,
            saw_usage: false,
        };
    };

    let mut out = String::with_capacity(body_str.len() + 256);
    let mut edited = false;
    let mut saw_error = false;
    let mut saw_usage = false;

    let mut first = true;
    for line in body_str.split('\n') {
        if !first {
            out.push('\n');
        }
        first = false;

        // SSE allows `data:<value>` and `data: <value>` — strip the colon, then an
        // optional single space (matches onwards' own SSE parser).
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.strip_prefix(' ').unwrap_or(data);
            let trimmed = data.trim();
            if trimmed != "[DONE]"
                && let Ok(mut chunk) = serde_json::from_str::<Value>(trimmed)
                && let Some(chunk_obj) = chunk.as_object_mut()
            {
                // Observe billing signals on every frame, even after we've injected.
                if chunk_obj.contains_key("error") {
                    saw_error = true;
                }
                if let Some(usage) = chunk_obj.get_mut("usage")
                    && let Some(usage_obj) = usage.as_object_mut()
                {
                    saw_usage = true;
                    if !already_edited && !edited {
                        splice_cache_fields(usage_obj, stats);
                        if let Ok(reserialized) = serde_json::to_string(&chunk) {
                            out.push_str("data: ");
                            out.push_str(&reserialized);
                            edited = true;
                            continue;
                        }
                    }
                }
            }
        }
        out.push_str(line);
    }

    SseScan {
        rewritten: if edited { Some(Bytes::from(out)) } else { None },
        saw_error,
        saw_usage,
    }
}

/// Inject the cache stats into the terminal usage frame of an SSE body. `None` if no usage
/// frame is found. (Thin wrapper over [`scan_inject_sse`]; the streaming path uses the
/// scan directly to also collect the commit-gate signals.)
pub fn inject_into_sse_body(body: &[u8], stats: &CacheStats) -> Option<Bytes> {
    scan_inject_sse(body, stats, false).rewritten
}

/// Wraps the buffered SSE stream: injects the cache fields into the terminal usage frame
/// as it flows, accumulates the two billing-success signals (`saw_error` / `saw_usage`),
/// and fires the commit verdict on the oneshot when the stream **ends or is dropped**.
///
/// Sending from `Drop` (not from a "saw the last chunk" branch) is what makes an early
/// client disconnect resolve to a veto: the terminal usage frame never arrived, so
/// `saw_usage` stays false → the verdict is `false` → no cache write for an unbilled call.
struct VerdictStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>,
    stats: CacheStats,
    edited: bool,
    status_ok: bool,
    saw_error: bool,
    saw_usage: bool,
    tx: Option<oneshot::Sender<bool>>,
}

impl Stream for VerdictStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut(); // Self: Unpin (all fields are)
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                let scan = scan_inject_sse(&chunk, &this.stats, this.edited);
                this.saw_error |= scan.saw_error;
                this.saw_usage |= scan.saw_usage;
                match scan.rewritten {
                    Some(rewritten) => {
                        this.edited = true;
                        Poll::Ready(Some(Ok(rewritten)))
                    }
                    None => Poll::Ready(Some(Ok(chunk))),
                }
            }
            // A transport error mid-stream is a failure: veto the write.
            Poll::Ready(Some(Err(e))) => {
                this.saw_error = true;
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for VerdictStream {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(self.status_ok && !self.saw_error && self.saw_usage);
        }
    }
}

/// Inject the cache stats into a chat-completion response, dispatching on content type:
/// streaming SSE edits only the terminal usage frame as it flows (never fully buffered);
/// non-streaming buffers the JSON body, splices `usage`, and rebuilds. If there is nothing
/// to edit, the body is preserved.
///
/// Returns the (possibly rewritten) response **and** a [`CommitGate`] reporting whether the
/// request succeeded for billing purposes — the caller gates the cache-index write on it.
pub async fn inject_cache_stats_into_response(mut response: Response, stats: &CacheStats) -> (Response, CommitGate) {
    let status_ok = response.status().is_success();

    let is_sse = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);

    if is_sse {
        use futures::StreamExt;

        let (tx, rx) = oneshot::channel();
        let body_stream = BodyExt::into_data_stream(std::mem::take(response.body_mut()));
        // Re-aggregate provider chunks into complete SSE events so a terminal usage
        // frame split across body chunks isn't missed; normalise the error type once.
        let buffered = SseBufferedStream::new(body_stream).map(|r| r.map_err(std::io::Error::other));
        let transformed = VerdictStream {
            inner: Box::pin(buffered),
            stats: *stats,
            edited: false,
            status_ok,
            saw_error: false,
            saw_usage: false,
            tx: Some(tx),
        };

        *response.body_mut() = axum::body::Body::from_stream(transformed);
        response.headers_mut().remove(axum::http::header::CONTENT_LENGTH);
        (response, CommitGate::Deferred(rx))
    } else {
        // Only JSON can carry a chat-completion `usage`; don't buffer explicitly
        // non-JSON bodies (preserve pass-through). Missing/unknown CT → try JSON.
        let is_json = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("application/json"))
            .unwrap_or(true);
        if !is_json {
            return (response, CommitGate::Ready(false));
        }
        let (mut parts, body) = response.into_parts();
        let body_bytes = match axum::body::to_bytes(body, usize::MAX).await {
            Ok(b) => b,
            Err(e) => {
                // Buffering the upstream response failed (e.g. the upstream connection broke
                // mid-read). Forwarding an empty body would hand the client a misleading 200
                // with no content; instead return a structured 5xx and veto the commit.
                error!("Failed to buffer response body for cache injection: {}", e);
                let err_body = serde_json::json!({
                    "error": {
                        "message": format!("failed to read upstream response body: {e}"),
                        "type": "internal_error",
                        "code": "response_body_read_failed",
                    }
                });
                return (
                    (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(err_body)).into_response(),
                    CommitGate::Ready(false),
                );
            }
        };

        // A present `usage` object is billing's success signal for a non-streamed call
        // (it's where token counts come from); combined with a 2xx status, it gates the write.
        match inject_into_usage_json(&body_bytes, stats) {
            Some(rewritten) => {
                let len = rewritten.len();
                parts.headers.remove(axum::http::header::TRANSFER_ENCODING);
                // We emit plain JSON (parse succeeded), so drop any stale Content-Encoding.
                parts.headers.remove(axum::http::header::CONTENT_ENCODING);
                parts
                    .headers
                    .insert(axum::http::header::CONTENT_LENGTH, axum::http::HeaderValue::from(len as u64));
                (
                    Response::from_parts(parts, axum::body::Body::from(rewritten)),
                    CommitGate::Ready(status_ok),
                )
            }
            None => {
                // No usage object (error body, or non-completion JSON) → never commit.
                let len = body_bytes.len();
                parts.headers.remove(axum::http::header::TRANSFER_ENCODING);
                parts
                    .headers
                    .insert(axum::http::header::CONTENT_LENGTH, axum::http::HeaderValue::from(len as u64));
                (
                    Response::from_parts(parts, axum::body::Body::from(body_bytes)),
                    CommitGate::Ready(false),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats() -> CacheStats {
        CacheStats {
            read: 1024,
            creation_5m: 10,
            creation_1h: 20,
            creation_24h: 30,
        }
    }

    #[test]
    fn strip_removes_nested_cache_control_and_sets_include_usage() {
        let body = serde_json::json!({
            "stream": true,
            "messages": [{"role":"system","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral"}}]}]
        })
        .to_string();
        let out = strip_cache_control(body.as_bytes()).expect("changed");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(!out.windows(13).any(|w| w == b"cache_control"));
        assert_eq!(v["stream_options"]["include_usage"], true);
    }

    #[test]
    fn strip_none_when_nothing_to_do() {
        let body = serde_json::json!({"messages":[{"role":"user","content":"hi"}]}).to_string();
        assert!(strip_cache_control(body.as_bytes()).is_none());
    }

    #[test]
    fn inject_non_streaming_adds_cache_fields() {
        let body = serde_json::json!({"usage":{"prompt_tokens":2000,"completion_tokens":5}}).to_string();
        let out = inject_into_usage_json(body.as_bytes(), &stats()).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["usage"]["prompt_tokens"], 2000, "total preserved");
        assert_eq!(v["usage"]["prompt_tokens_details"]["cached_tokens"], 1024);
        assert_eq!(v["usage"]["cache_read_input_tokens"], 1024);
        assert_eq!(v["usage"]["cache_creation_input_tokens"], 60);
        assert_eq!(v["usage"]["cache_creation"]["ephemeral_1h_input_tokens"], 20);
    }

    #[test]
    fn inject_non_streaming_none_when_no_usage() {
        let body = serde_json::json!({"choices":[]}).to_string();
        assert!(inject_into_usage_json(body.as_bytes(), &stats()).is_none());
    }

    #[test]
    fn inject_sse_edits_only_terminal_usage_frame() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":2000}}\n\ndata: [DONE]\n\n";
        let out = inject_into_sse_body(sse.as_bytes(), &stats()).unwrap();
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("\"cached_tokens\":1024"));
        assert!(s.contains("\"cache_read_input_tokens\":1024"));
        // exactly one injected frame; the delta + [DONE] are untouched.
        assert_eq!(s.matches("cached_tokens").count(), 1);
        assert!(s.contains("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}"));
        assert!(s.contains("data: [DONE]"));
    }

    #[test]
    fn inject_sse_none_when_no_usage_frame() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        assert!(inject_into_sse_body(sse.as_bytes(), &stats()).is_none());
    }

    #[test]
    fn inject_sse_handles_data_prefix_without_space() {
        // `data:{…}` (no space after the colon) is valid SSE and must still be injected.
        let sse = "data:{\"choices\":[],\"usage\":{\"prompt_tokens\":2000}}\n\ndata:[DONE]\n\n";
        let out = inject_into_sse_body(sse.as_bytes(), &stats()).expect("no-space data: frame is injected");
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("\"cache_read_input_tokens\":1024"), "got: {s}");
    }

    #[tokio::test]
    async fn inject_response_streaming_edits_split_usage_frame() {
        use axum::body::Body;
        // The terminal usage frame is split across two body chunks.
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"data: {\"choices\":[],\"usage\":{\"prompt_")),
            Ok(Bytes::from_static(b"tokens\":2000}}\n\ndata: [DONE]\n\n")),
        ];
        let resp = Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(futures::stream::iter(chunks)))
            .unwrap();
        let (out, gate) = inject_cache_stats_into_response(resp, &stats()).await;
        let collected = axum::body::to_bytes(out.into_body(), usize::MAX).await.unwrap();
        let s = std::str::from_utf8(&collected).unwrap();
        assert!(s.contains("\"cached_tokens\":1024"), "got: {s}");
        // Draining the stream (a clean 200 with a usage frame, no error) → commit allowed.
        match gate {
            CommitGate::Deferred(rx) => assert!(rx.await.unwrap(), "clean stream → commit"),
            CommitGate::Ready(_) => panic!("streaming response must yield a deferred gate"),
        }
    }

    #[tokio::test]
    async fn inject_response_streaming_error_frame_vetoes_commit() {
        use axum::body::Body;
        // A mid-stream error frame arrives after some deltas; no usage frame follows.
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n")),
            Ok(Bytes::from_static(b"data: {\"error\":{\"message\":\"upstream exploded\"}}\n\n")),
        ];
        let resp = Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(futures::stream::iter(chunks)))
            .unwrap();
        let (out, gate) = inject_cache_stats_into_response(resp, &stats()).await;
        let _ = axum::body::to_bytes(out.into_body(), usize::MAX).await.unwrap();
        match gate {
            CommitGate::Deferred(rx) => assert!(!rx.await.unwrap(), "error frame → veto"),
            CommitGate::Ready(_) => panic!("streaming response must yield a deferred gate"),
        }
    }

    #[tokio::test]
    async fn inject_response_streaming_disconnect_vetoes_commit() {
        use axum::body::Body;
        // A clean stream WITH a usage frame — but the client disconnects before draining
        // it (we drop the response body without reading it). VerdictStream's Drop must then
        // fire the verdict, and because the terminal usage frame was never polled, the
        // verdict is `false` → no commit for a stream the client never finished paying for.
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n")),
            Ok(Bytes::from_static(
                b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":2000}}\n\ndata: [DONE]\n\n",
            )),
        ];
        let resp = Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(futures::stream::iter(chunks)))
            .unwrap();
        let (out, gate) = inject_cache_stats_into_response(resp, &stats()).await;
        // Client disconnect: drop the response (and its body) WITHOUT consuming the stream.
        drop(out);
        match gate {
            CommitGate::Deferred(rx) => assert!(!rx.await.unwrap(), "disconnect before terminal frame → veto"),
            CommitGate::Ready(_) => panic!("streaming response must yield a deferred gate"),
        }
    }

    #[tokio::test]
    async fn inject_non_streaming_error_body_vetoes_commit() {
        use axum::body::Body;
        // A 400 JSON error body has no usage object → no injection, no commit.
        let resp = Response::builder()
            .status(400)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::json!({"error":{"message":"bad request"}}).to_string()))
            .unwrap();
        let (_out, gate) = inject_cache_stats_into_response(resp, &stats()).await;
        match gate {
            CommitGate::Ready(ok) => assert!(!ok, "error body → no commit"),
            CommitGate::Deferred(_) => panic!("non-streaming response must yield a ready gate"),
        }
    }
}
