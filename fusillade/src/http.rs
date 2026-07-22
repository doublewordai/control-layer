//! HTTP client abstraction for making requests.
//!
//! This module defines the `HttpClient` trait to abstract HTTP request execution,
//! enabling testability with mock implementations.

use crate::error::Result;
pub use crate::request::HttpResponse;
/// Function signature for stream reassemblers.
///
/// A reassembler takes a slice of collected SSE events and produces a single
/// response body string. Use [`openai_reassembler::reassemble`] for
/// OpenAI-compatible endpoints.
pub type StreamReassembler = fn(&[eventsource_stream::Event]) -> anyhow::Result<String>;
use crate::request::RequestData;
use async_trait::async_trait;
use opentelemetry::trace::TraceContextExt;
use std::sync::Arc;
use std::time::Duration;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// One Server-Sent Event parsed from an upstream streaming response.
///
/// Field shape mirrors the SSE wire format (RFC 8895 / WHATWG): every
/// event has an `event` type, a `data` payload, an `id` cursor for
/// reconnection, and an optional `retry` hint. For LLM streams the
/// `data` field carries a JSON chunk per token; `event` is typically
/// empty.
///
/// The string fields are borrowed from the underlying SSE event
/// buffer to keep per-chunk overhead near zero — a single LLM response
/// emits hundreds of these. Callbacks that need to store an event
/// beyond the invocation should clone the fields they care about
/// (typically only `data` needs parsing/forwarding for token-delta
/// extraction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEvent<'a> {
    pub event: &'a str,
    pub data: &'a str,
    pub id: &'a str,
    pub retry: Option<Duration>,
}

impl<'a> From<&'a eventsource_stream::Event> for StreamEvent<'a> {
    fn from(e: &'a eventsource_stream::Event) -> Self {
        Self {
            event: &e.event,
            data: &e.data,
            id: &e.id,
            retry: e.retry,
        }
    }
}

/// Sink invoked once per SSE event as the streaming HTTP client reads
/// chunks from an upstream response, before reassembly. Consumers that
/// need live access to model token deltas (e.g. forwarding to a
/// client-facing SSE channel) plug an implementation into
/// [`HttpClient::execute_with_event_callback`].
///
/// `on_event` is intentionally synchronous: it runs inline in the
/// chunk-read loop between successive `stream.next()` calls. The
/// `chunk_timeout` only wraps `stream.next()` itself — callback time
/// is *not* measured against it — but the overall `body_timeout` does
/// cover the loop body, so a slow callback eats into that budget and
/// can trip a `BodyTimeout` failure for the whole response.
/// Implementations that need to do async work should dispatch via
/// `tokio::sync::mpsc::UnboundedSender::send` (sync, never blocks) or
/// `tokio::spawn` a fire-and-forget task — anything that returns to
/// the caller in microseconds.
///
/// # Panics
///
/// A panic inside `on_event` will unwind through fusillade's streaming
/// loop and fail the request (it surfaces to the trait caller as an
/// HTTP-client error and may be classified as retriable by upstream
/// retry policy). Implementations should be panic-free; catch
/// expected errors inside the callback and degrade gracefully rather
/// than letting them propagate.
pub trait StreamEventCallback: Send + Sync {
    fn on_event(&self, event: &StreamEvent<'_>);
}

/// Minimal representation of a provider error embedded in an SSE stream.
/// Used to detect and extract error status codes from events before reassembly.
#[derive(serde::Deserialize)]
struct EmbeddedErrorEnvelope {
    error: EmbeddedErrorBody,
}

#[derive(serde::Deserialize)]
struct EmbeddedErrorBody {
    code: Option<serde_json::Value>,
}

/// Trait for executing HTTP requests.
///
/// This abstraction allows for different implementations (production vs. testing)
/// and makes the daemon processing logic testable without making real HTTP calls.
///
/// # Example
/// ```ignore
/// let client = ReqwestHttpClient::new(Duration::from_secs(300), Duration::from_secs(30), Duration::from_secs(600), vec![]);
/// let response = client.execute(&request_data, "api-key").await?;
/// println!("Status: {}, Body: {}", response.status, response.body);
/// ```
#[async_trait]
pub trait HttpClient: Send + Sync + Clone {
    /// Execute an HTTP request.
    ///
    /// Timeout behavior is configured at client construction time, not per-request.
    ///
    /// # Arguments
    /// * `request` - The request data containing endpoint, method, path, and body
    /// * `api_key` - API key to include in Authorization: Bearer header
    ///
    /// # Errors
    /// Returns an error if:
    /// - The request fails due to network issues
    /// - The request times out (either waiting for headers or between body chunks)
    /// - The URL is invalid
    async fn execute(&self, request: &RequestData, api_key: &str) -> Result<HttpResponse>;

    /// As [`execute`], but invokes `on_event` for each SSE event read
    /// from a streaming upstream response, in arrival order, before
    /// reassembly.
    ///
    /// `on_event` is only fired for streaming responses (paths that the
    /// implementation classifies as streamable). Non-streaming responses
    /// never invoke the callback.
    ///
    /// The default implementation delegates to [`execute`] and discards
    /// `on_event`. Implementations that read SSE chunk-by-chunk
    /// (notably [`ReqwestHttpClient`]) override this so consumers like
    /// onwards' multi-step loop can forward token deltas to a
    /// client-facing SSE channel as they arrive, while still benefiting
    /// from this client's header stamping and stream reassembly.
    async fn execute_with_event_callback(
        &self,
        request: &RequestData,
        api_key: &str,
        _on_event: Option<Arc<dyn StreamEventCallback>>,
    ) -> Result<HttpResponse> {
        self.execute(request, api_key).await
    }
}

// ============================================================================
// Production Implementation using reqwest
// ============================================================================

/// Production HTTP client using reqwest.
///
/// This implementation makes real HTTP requests to external endpoints.
/// Timeouts are configured at construction time and applied differently
/// depending on whether the request path matches a streamable endpoint:
///
/// **Non-streaming** (path not in `streamable_endpoints`): uses
/// `first_chunk_timeout + body_timeout` as a single overall reqwest timeout
/// covering the entire request.
///
/// **Streaming** (path in `streamable_endpoints`): an `X-Fusillade-Stream`
/// header is added and the response is read as SSE with split timeouts:
/// - `first_chunk_timeout`: max time to first token (connect + headers + first body chunk)
/// - `chunk_timeout`: max idle time between subsequent body chunks
/// - `body_timeout`: max total time for the entire response body
///
/// In both modes the request body upload is additionally bounded by
/// `upload_stall_timeout` (default 60s): if the transport accepts no body
/// bytes for that long before the upload completes, the attempt is aborted
/// with [`FusilladeError::UploadStallTimeout`] so the retry machinery can
/// dispatch a fresh one. This keeps send-phase hangs (a wedged connection,
/// a stalled write) from silently consuming the much longer response
/// timeouts, which are sized for slow upstreams rather than slow uploads.
#[derive(Clone)]
pub struct ReqwestHttpClient {
    client: reqwest::Client,
    first_chunk_timeout: Duration,
    chunk_timeout: Duration,
    body_timeout: Duration,
    upload_stall_timeout: Duration,
    upload_chunk_bytes: usize,
    upload_stall_poll: Duration,
    stream_reassembler: Option<StreamReassembler>,
    streamable_endpoints: Vec<String>,
}

/// Default cap on how long a request body upload may make no progress.
pub(crate) const DEFAULT_UPLOAD_STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Request bodies are handed to the transport in chunks of this size so the
/// upload watchdog can observe progress.
pub(crate) const DEFAULT_UPLOAD_CHUNK_BYTES: usize = 64 * 1024;

/// Poll interval for the upload stall watchdog.
pub(crate) const DEFAULT_UPLOAD_STALL_POLL: Duration = Duration::from_millis(100);

impl ReqwestHttpClient {
    /// Create a new reqwest-based HTTP client with the given timeouts.
    pub fn new(
        first_chunk_timeout: Duration,
        chunk_timeout: Duration,
        body_timeout: Duration,
        streamable_endpoints: Vec<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            first_chunk_timeout,
            chunk_timeout,
            body_timeout,
            upload_stall_timeout: DEFAULT_UPLOAD_STALL_TIMEOUT,
            upload_chunk_bytes: DEFAULT_UPLOAD_CHUNK_BYTES,
            upload_stall_poll: DEFAULT_UPLOAD_STALL_POLL,
            stream_reassembler: Some(openai_reassembler::reassemble),
            streamable_endpoints,
        }
    }

    /// Override how long the request body upload may make no progress before
    /// the attempt is aborted (default 60s). This bounds only the send phase;
    /// how long the upstream may take to answer is governed by the other
    /// timeouts.
    pub fn with_upload_stall_timeout(mut self, timeout: Duration) -> Self {
        self.upload_stall_timeout = timeout;
        self
    }

    /// Override the request-body chunk size used to observe upload progress
    /// (default 64 KiB). Smaller values provide finer progress granularity at
    /// the cost of more body frames.
    ///
    /// # Panics
    ///
    /// Panics if `chunk_bytes` is zero.
    pub fn with_upload_chunk_bytes(mut self, chunk_bytes: usize) -> Self {
        assert!(
            chunk_bytes > 0,
            "upload chunk size must be greater than zero"
        );
        self.upload_chunk_bytes = chunk_bytes;
        self
    }

    /// Override how often the upload stall watchdog checks progress (default
    /// 100ms). A stall may be detected up to roughly one poll interval after
    /// `upload_stall_timeout` expires.
    ///
    /// # Panics
    ///
    /// Panics if `interval` is zero.
    pub fn with_upload_stall_poll_interval(mut self, interval: Duration) -> Self {
        assert!(
            !interval.is_zero(),
            "upload stall poll interval must be greater than zero"
        );
        self.upload_stall_poll = interval;
        self
    }

    /// Set a stream reassembler function that converts collected SSE events
    /// into a single response body. Without this, streaming responses are stored as
    /// newline-delimited event data payloads.
    ///
    /// # Example
    /// ```ignore
    /// use openai_reassembler::reassemble;
    /// let client = ReqwestHttpClient::new(...)
    ///     .with_stream_reassembler(|events| reassemble(events).map_err(Into::into));
    /// ```
    pub fn with_stream_reassembler(mut self, reassembler: StreamReassembler) -> Self {
        self.stream_reassembler = Some(reassembler);
        self
    }
}

impl Default for ReqwestHttpClient {
    fn default() -> Self {
        Self::new(
            ONE_DAY_DURATION,
            ONE_DAY_DURATION,
            ONE_DAY_DURATION,
            Vec::new(),
        )
    }
}

/// Long but finite fallback timeout (24 hours) used when no explicit timeout is configured.
const ONE_DAY_DURATION: Duration = Duration::from_secs(86_400);

/// Invoke an optional [`StreamEventCallback`] for one SSE event,
/// borrowing the underlying [`eventsource_stream::Event`] (no
/// per-chunk clone). Pulled into a free function so the streaming
/// loop's "first event" and "subsequent events" branches stay
/// identical at the call site.
fn fire_callback(on_event: Option<&dyn StreamEventCallback>, event: &eventsource_stream::Event) {
    if let Some(cb) = on_event {
        cb.on_event(&StreamEvent::from(event));
    }
}

fn map_reqwest_error(error: reqwest::Error) -> crate::error::FusilladeError {
    if error.is_builder() {
        crate::error::FusilladeError::HttpRequestBuilder(error.to_string())
    } else if error.is_timeout() {
        crate::error::FusilladeError::HttpClientTimeout(error.to_string())
    } else {
        crate::error::FusilladeError::HttpClient(error.to_string())
    }
}

#[async_trait]
impl HttpClient for ReqwestHttpClient {
    async fn execute(&self, request: &RequestData, api_key: &str) -> Result<HttpResponse> {
        self.execute_with_event_callback(request, api_key, None)
            .await
    }

    #[tracing::instrument(name = "fusillade.execute", skip(self, request, api_key, on_event), fields(
        otel.name = %format!("{} {}", request.method, request.path),
    ))]
    async fn execute_with_event_callback(
        &self,
        request: &RequestData,
        api_key: &str,
        on_event: Option<Arc<dyn StreamEventCallback>>,
    ) -> Result<HttpResponse> {
        let url = format!("{}{}", request.endpoint, request.path);
        let span = tracing::Span::current();
        span.set_attribute("otel.kind", "Client");
        span.set_attribute("http.request.method", request.method.clone());
        span.set_attribute("url.path", request.path.clone());
        span.set_attribute("url.full", url.clone());

        tracing::debug!(
            url.full = %url,
            upload_stall_timeout_ms = self.upload_stall_timeout.as_millis() as u64,
            upload_chunk_bytes = self.upload_chunk_bytes,
            upload_stall_poll_ms = self.upload_stall_poll.as_millis() as u64,
            first_chunk_timeout_ms = self.first_chunk_timeout.as_millis() as u64,
            chunk_timeout_ms = self.chunk_timeout.as_millis() as u64,
            body_timeout_ms = self.body_timeout.as_millis() as u64,
            "Executing HTTP request"
        );

        let mut req = self.client.request(
            request.method.parse().map_err(|e| {
                tracing::error!(method = %request.method, error = %e, "Invalid HTTP method");
                anyhow::anyhow!("Invalid HTTP method '{}': {}", request.method, e)
            })?,
            &url,
        );

        // Only add Authorization header if api_key is not empty
        if !api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", api_key));
            tracing::trace!(request_id = %request.id, "Added Authorization header");
        }

        // Add fusillade request ID header for analytics correlation in dwctl
        // Use the full UUID (request.id.0) instead of the Display impl which only shows 8 chars
        req = req.header("X-Fusillade-Request-Id", request.id.0.to_string());

        // Add batch metadata as headers (x-fusillade-batch-COLUMN-NAME)
        // This includes id, created_by, endpoint, completion_window, etc.
        // Convert underscores to hyphens for standard HTTP header naming
        for (key, value) in &request.batch_metadata {
            let header_name = format!("x-fusillade-batch-{}", key.replace('_', "-"));
            req = req.header(&header_name, value);
        }

        // Add custom_id header if present for analytics correlation
        if let Some(custom_id) = &request.custom_id {
            req = req.header("X-Fusillade-Custom-Id", custom_id.clone());
            tracing::trace!(request_id = %request.id, custom_id = %custom_id, "Added X-Fusillade-Custom-Id header");
        }

        // Inject W3C traceparent header for distributed tracing.
        // dwctl extracts this in its TraceLayer to parent its request span
        // under this execute span, producing one continuous trace.
        let ctx = tracing::Span::current().context();
        let span_ref = ctx.span();
        let span_ctx = span_ref.span_context();
        if span_ctx.is_valid() {
            let traceparent = format!(
                "00-{}-{}-{:02x}",
                span_ctx.trace_id(),
                span_ctx.span_id(),
                span_ctx.trace_flags().to_u8()
            );
            req = req.header("traceparent", &traceparent);
            tracing::trace!(request_id = %request.id, traceparent = %traceparent, "Added traceparent header for distributed tracing");
        }

        // Only add body and Content-Type for methods that support a body.
        // The body is wrapped so the upload watchdog can observe progress;
        // its exact size hint preserves Content-Length framing on the wire.
        let mut upload: Option<Arc<UploadProgress>> = None;
        let method_upper = request.method.to_uppercase();
        if method_upper != "GET"
            && method_upper != "HEAD"
            && method_upper != "DELETE"
            && !request.body.is_empty()
        {
            let body = bytes::Bytes::from(request.body.clone().into_bytes());
            let progress = UploadProgress::new(body.len() as u64);
            req = req
                .header("Content-Type", "application/json")
                .body(reqwest::Body::wrap(ProgressBody {
                    remaining: body,
                    progress: progress.clone(),
                    chunk_bytes: self.upload_chunk_bytes,
                }));
            upload = Some(progress);
            tracing::trace!(
                request_id = %request.id,
                body_len = request.body.len(),
                "Added request body"
            );
        }

        let stream = self.streamable_endpoints.iter().any(|e| e == &request.path);
        span.set_attribute("fusillade.streaming", stream);
        if stream {
            req = req.header("X-Fusillade-Stream", "true");
            self.execute_streaming(request, req, &url, upload, on_event)
                .await
        } else {
            self.execute_non_streaming(request, req, &url, upload).await
        }
    }
}

/// Shared view of request-body upload progress between the instrumented body
/// and the watchdog racing the send future.
struct UploadProgress {
    started: std::time::Instant,
    last_progress_ms: std::sync::atomic::AtomicU64,
    sent: std::sync::atomic::AtomicU64,
    total: u64,
}

impl UploadProgress {
    fn new(total: u64) -> Arc<Self> {
        Arc::new(Self {
            started: std::time::Instant::now(),
            last_progress_ms: std::sync::atomic::AtomicU64::new(0),
            sent: std::sync::atomic::AtomicU64::new(0),
            total,
        })
    }

    fn record(&self, bytes: usize) {
        use std::sync::atomic::Ordering;
        self.sent.fetch_add(bytes as u64, Ordering::Relaxed);
        self.last_progress_ms
            .store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn is_complete(&self) -> bool {
        self.sent_bytes() >= self.total
    }

    fn stalled_for(&self) -> Duration {
        let last = Duration::from_millis(
            self.last_progress_ms
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        self.started.elapsed().saturating_sub(last)
    }

    fn sent_bytes(&self) -> u64 {
        self.sent.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Request body that reports upload progress to an [`UploadProgress`] handle.
///
/// The body is handed to the transport in configurable chunks; each chunk
/// the transport accepts counts as progress. The exact `size_hint` preserves
/// Content-Length framing, so the wire format is identical to sending the
/// buffered body directly.
struct ProgressBody {
    remaining: bytes::Bytes,
    progress: Arc<UploadProgress>,
    chunk_bytes: usize,
}

/// Owns one body chunk until Hyper has consumed it from its write buffer.
/// Dropping the last `Bytes` reference is the closest per-request signal
/// reqwest exposes that the transport writer accepted the complete chunk.
struct TrackedUploadChunk {
    bytes: bytes::Bytes,
    progress: Arc<UploadProgress>,
}

impl AsRef<[u8]> for TrackedUploadChunk {
    fn as_ref(&self) -> &[u8] {
        self.bytes.as_ref()
    }
}

impl Drop for TrackedUploadChunk {
    fn drop(&mut self) {
        self.progress.record(self.bytes.len());
    }
}

impl http_body::Body for ProgressBody {
    type Data = bytes::Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<std::result::Result<http_body::Frame<Self::Data>, Self::Error>>>
    {
        let this = self.get_mut();
        if this.remaining.is_empty() {
            return std::task::Poll::Ready(None);
        }
        let take = this.remaining.len().min(this.chunk_bytes);
        let chunk = this.remaining.split_to(take);
        let tracked = bytes::Bytes::from_owner(TrackedUploadChunk {
            bytes: chunk,
            progress: this.progress.clone(),
        });
        std::task::Poll::Ready(Some(Ok(http_body::Frame::data(tracked))))
    }

    fn is_end_stream(&self) -> bool {
        self.remaining.is_empty()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::SizeHint::with_exact(self.remaining.len() as u64)
    }
}

/// Race `send` against an upload stall watchdog.
///
/// The watchdog fires only while Hyper still owns queued request-body bytes:
/// if the transport writer consumes no complete chunk for `stall_timeout`,
/// the attempt is aborted so the daemon's retry machinery can dispatch a
/// fresh one. Once all chunks have been consumed the watchdog disarms and
/// `send`'s own timeouts take over. Requests without a body (`upload` is
/// `None`) are unaffected.
async fn race_upload_stall<T>(
    send: impl std::future::Future<Output = T>,
    upload: Option<Arc<UploadProgress>>,
    stall_timeout: Duration,
    poll_interval: Duration,
    url: &str,
) -> Result<T> {
    let Some(progress) = upload else {
        return Ok(send.await);
    };
    tokio::pin!(send);
    loop {
        tokio::select! {
            out = &mut send => return Ok(out),
            _ = tokio::time::sleep(poll_interval) => {
                if progress.is_complete() {
                    return Ok(send.await);
                }
                if progress.stalled_for() >= stall_timeout {
                    return Err(crate::error::FusilladeError::UploadStallTimeout(format!(
                        "request upload to {} stalled: {} of {} bytes accepted by the transport writer, no progress for {}ms",
                        url,
                        progress.sent_bytes(),
                        progress.total,
                        stall_timeout.as_millis(),
                    )));
                }
            }
        }
    }
}

impl ReqwestHttpClient {
    /// Execute a non-streaming request with a single overall timeout.
    /// Uses first_chunk_timeout + body_timeout as the total allowed time,
    /// since non-streaming responses return everything at once.
    async fn execute_non_streaming(
        &self,
        request: &RequestData,
        req: reqwest::RequestBuilder,
        url: &str,
        upload: Option<Arc<UploadProgress>>,
    ) -> Result<HttpResponse> {
        let total_timeout = self.first_chunk_timeout + self.body_timeout;
        let response = race_upload_stall(
            req.timeout(total_timeout).send(),
            upload,
            self.upload_stall_timeout,
            self.upload_stall_poll,
            url,
        )
        .await
        .inspect_err(|e| {
            tracing::error!(
                request_id = %request.id,
                url.full = %url,
                error = %e,
                "HTTP request upload stalled"
            );
        })?
        .map_err(|e| {
            if e.is_builder() {
                tracing::error!(
                    request_id = %request.id,
                    url.full = %url,
                    error = %e.to_string(),
                    custom_id = ?request.custom_id,
                    batch_metadata_keys = ?request.batch_metadata.keys().collect::<Vec<_>>(),
                    "Failed to build HTTP request (not retriable) - likely invalid header value"
                );
            } else {
                tracing::error!(
                    request_id = %request.id,
                    url.full = %url,
                    error = %e,
                    "HTTP request failed"
                );
            }
            map_reqwest_error(e)
        })?;

        let status = response.status().as_u16();
        let body = response.text().await.map_err(map_reqwest_error)?;

        tracing::debug!(
            request_id = %request.id,
            status = status,
            response_len = body.len(),
            "HTTP request completed"
        );

        Ok(HttpResponse { status, body })
    }

    /// Execute a streaming request with split timeouts:
    /// - first_chunk_timeout: connect + headers + first body chunk (time-to-first-token).
    ///   Handles servers (like vLLM) that return headers immediately but queue the request.
    /// - chunk_timeout: max idle time between subsequent SSE events.
    /// - body_timeout: max total time for the entire response body.
    ///
    /// Successful and SSE error responses are parsed via `eventsource-stream`.
    /// Non-SSE error responses are returned verbatim so provider error details
    /// survive downstream persistence. SSE events are collected, then passed to
    /// the stream reassembler (which skips `[DONE]` and empty events internally).
    /// Without a reassembler, the fallback path filters these events and
    /// newline-joins the remaining data payloads.
    async fn execute_streaming(
        &self,
        request: &RequestData,
        req: reqwest::RequestBuilder,
        url: &str,
        upload: Option<Arc<UploadProgress>>,
        on_event: Option<Arc<dyn StreamEventCallback>>,
    ) -> Result<HttpResponse> {
        use eventsource_stream::Eventsource;
        use futures::StreamExt;

        // Phase 1: connect, get headers, and wait for the first SSE event within
        // one shared first_chunk_timeout deadline (time-to-first-token). The
        // upload watchdog separately bounds the body send phase.
        let first_chunk_deadline = tokio::time::Instant::now() + self.first_chunk_timeout;
        let send = tokio::time::timeout_at(first_chunk_deadline, async {
            req.send()
                .await
                .map_err(map_reqwest_error)
                .inspect_err(|e| {
                    tracing::error!(
                        request_id = %request.id,
                        url.full = %url,
                        error = %e,
                        custom_id = ?request.custom_id,
                        batch_metadata_keys = ?request.batch_metadata.keys().collect::<Vec<_>>(),
                        "HTTP request failed"
                    );
                })
        });
        let resp = race_upload_stall(
            send,
            upload,
            self.upload_stall_timeout,
            self.upload_stall_poll,
            url,
        )
        .await
        .inspect_err(|e| {
            tracing::error!(
                request_id = %request.id,
                url.full = %url,
                error = %e,
                "HTTP request upload stalled"
            );
        })?
        .map_err(|_| {
            crate::error::FusilladeError::FirstChunkTimeout(format!(
                "No response headers from {} within {}ms",
                url,
                self.first_chunk_timeout.as_millis()
            ))
        })??;

        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let is_event_stream = content_type
            .split(';')
            .next()
            .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("text/event-stream"));

        // A streamable request can be rejected before streaming begins. In
        // that case providers commonly return an ordinary JSON error body;
        // feeding it into an SSE parser produces zero events and loses the
        // diagnostic. Read non-SSE errors directly instead.
        if status >= 400 && !is_event_stream {
            let body = tokio::time::timeout(self.body_timeout, resp.text())
                .await
                .map_err(|_| {
                    crate::error::FusilladeError::BodyTimeout(format!(
                        "Total body read from {} exceeded {}ms",
                        url,
                        self.body_timeout.as_millis()
                    ))
                })?
                .map_err(map_reqwest_error)?;

            tracing::debug!(
                request_id = %request.id,
                status = status,
                content_type = %content_type,
                response_len = body.len(),
                "Non-SSE streaming error response completed"
            );

            return Ok(HttpResponse { status, body });
        }

        let mut stream = resp.bytes_stream().eventsource();
        let first_event = tokio::time::timeout_at(first_chunk_deadline, async {
            match stream.next().await {
                Some(Ok(event)) => Ok::<_, crate::error::FusilladeError>(Some(event)),
                Some(Err(e)) => Err(anyhow::anyhow!("SSE parse error from {}: {}", url, e).into()),
                None => Ok(None),
            }
        })
        .await
        .map_err(|_| {
            crate::error::FusilladeError::FirstChunkTimeout(format!(
                "No first SSE event from {} within {}ms",
                url,
                self.first_chunk_timeout.as_millis()
            ))
        })??;

        // Phase 2: collect all SSE events with per-event and total body
        // timeouts. If `on_event` is set, fire it per event in arrival
        // order via `fire_callback` before stashing into the reassembly
        // vec. The callback runs outside `stream.next()`, so its time
        // counts against `body_timeout` (not `chunk_timeout`) — see the
        // `StreamEventCallback` trait docs for the rationale.
        let collected = tokio::time::timeout(self.body_timeout, async {
            let mut events: Vec<eventsource_stream::Event> = Vec::new();
            if let Some(event) = first_event {
                fire_callback(on_event.as_deref(), &event);
                events.push(event);
            }
            loop {
                match tokio::time::timeout(self.chunk_timeout, stream.next()).await {
                    Ok(Some(Ok(event))) => {
                        fire_callback(on_event.as_deref(), &event);
                        events.push(event);
                    }
                    Ok(Some(Err(e))) => {
                        return Err(anyhow::anyhow!("SSE parse error from {}: {}", url, e).into());
                    }
                    Ok(None) => break,
                    Err(_) => {
                        return Err(crate::error::FusilladeError::TokensTimeout(format!(
                            "SSE stream stalled from {} after {}ms ({} events received)",
                            url,
                            self.chunk_timeout.as_millis(),
                            events.len()
                        )));
                    }
                }
            }
            Ok(events)
        })
        .await
        .map_err(|_| {
            crate::error::FusilladeError::BodyTimeout(format!(
                "Total body read from {} exceeded {}ms",
                url,
                self.body_timeout.as_millis()
            ))
        })??;

        // Check for provider error objects before reassembly. Some providers
        // return HTTP 200 but embed an error in the SSE stream. If found, use
        // the error JSON directly as the body and override the status with the
        // embedded code so downstream retry logic classifies it correctly.
        // The reassembler doesn't handle error objects and would mangle them.
        if let Some(event) = collected.iter().find(|e| e.data.starts_with("{\"error\""))
            && let Ok(envelope) = serde_json::from_str::<EmbeddedErrorEnvelope>(&event.data)
        {
            let code = envelope
                .error
                .code
                .as_ref()
                .and_then(|c| c.as_u64())
                .map(|c| c as u16)
                .filter(|c| (400..600).contains(c))
                .unwrap_or(500);

            tracing::warn!(
                request_id = %request.id,
                embedded_status = code,
                "Provider returned error inside SSE stream, reclassifying as HTTP error"
            );

            return Ok(HttpResponse {
                status: code,
                body: event.data.clone(),
            });
        }

        let body = match &self.stream_reassembler {
            // Only reassemble successful streams. An error response (empty or a
            // contentless chunk) must not be synthesized into a degenerate
            // `{"choices":[],"usage":null}` completion — return its raw payload
            // so the real HTTP status drives retry classification.
            Some(reassemble) if status < 400 => reassemble(&collected)?,
            _ => collected
                .iter()
                .filter(|e| !e.data.is_empty() && e.data != "[DONE]")
                .map(|e| e.data.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        };

        tracing::debug!(
            request_id = %request.id,
            status = status,
            response_len = body.len(),
            "Streaming HTTP request completed"
        );

        Ok(HttpResponse { status, body })
    }
}

// ============================================================================
// Test/Mock Implementation
// ============================================================================

// TODO: this should be a separate file within an http/ module.
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::oneshot;

/// Mock HTTP client for testing.
///
/// Allows configuring predetermined responses for specific requests without
/// making actual HTTP calls.
///
/// # Example
/// ```ignore
/// let mock = MockHttpClient::new();
/// mock.add_response(
///     "POST /v1/chat/completions",
///     HttpResponse {
///         status: 200,
///         body: r#"{"result": "success"}"#.to_string(),
///     },
/// );
/// ```
#[derive(Clone)]
pub struct MockHttpClient {
    responses: Arc<Mutex<HashMap<String, Vec<MockResponse>>>>,
    calls: Arc<Mutex<Vec<MockCall>>>,
    in_flight: Arc<AtomicUsize>,
}

/// A mock response that can optionally wait for a trigger before completing.
enum MockResponse {
    /// Immediate response
    Immediate(Result<HttpResponse>),
    /// Response that waits for a trigger signal before completing
    Triggered {
        response: Result<HttpResponse>,
        trigger: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    },
}

/// Record of a call made to the mock HTTP client.
#[derive(Debug, Clone)]
pub struct MockCall {
    pub method: String,
    pub endpoint: String,
    pub path: String,
    pub body: String,
    pub api_key: String,
    pub batch_metadata: std::collections::HashMap<String, String>,
}

impl MockHttpClient {
    /// Create a new mock HTTP client.
    pub fn new() -> Self {
        Self {
            responses: Arc::new(Mutex::new(HashMap::new())),
            calls: Arc::new(Mutex::new(Vec::new())),
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Add a predetermined response for a specific method and path.
    ///
    /// The key is formatted as "{method} {path}". Multiple responses can be
    /// added for the same key - they will be returned in FIFO order.
    pub fn add_response(&self, key: &str, response: Result<HttpResponse>) {
        self.responses
            .lock()
            .entry(key.to_string())
            .or_default()
            .push(MockResponse::Immediate(response));
    }

    /// Add a response that will wait for a manual trigger before completing.
    ///
    /// Returns a sender that when triggered (by sending `()` or dropping) will
    /// cause the HTTP request to complete with the given response.
    ///
    /// # Example
    /// ```ignore
    /// let trigger = mock.add_response_with_trigger(
    ///     "POST /test",
    ///     Ok(HttpResponse { status: 200, body: "ok".to_string() })
    /// );
    /// // ... request is now blocked waiting ...
    /// trigger.send(()).unwrap(); // Now it completes
    /// ```
    pub fn add_response_with_trigger(
        &self,
        key: &str,
        response: Result<HttpResponse>,
    ) -> oneshot::Sender<()> {
        let (tx, rx) = oneshot::channel();
        self.responses
            .lock()
            .entry(key.to_string())
            .or_default()
            .push(MockResponse::Triggered {
                response,
                trigger: Arc::new(Mutex::new(Some(rx))),
            });
        tx
    }

    /// Get all calls that have been made to this mock client.
    pub fn get_calls(&self) -> Vec<MockCall> {
        self.calls.lock().clone()
    }

    /// Clear all recorded calls.
    pub fn clear_calls(&self) {
        self.calls.lock().clear();
    }

    /// Get the number of calls made.
    pub fn call_count(&self) -> usize {
        self.calls.lock().len()
    }

    /// Get the number of requests currently in-flight (executing).
    ///
    /// This is useful for testing cancellation - if a request is aborted,
    /// the in-flight count will decrease.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }
}

impl Default for MockHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HttpClient for MockHttpClient {
    async fn execute(&self, request: &RequestData, api_key: &str) -> Result<HttpResponse> {
        // Increment in-flight counter
        self.in_flight.fetch_add(1, Ordering::SeqCst);

        // Guard to ensure we decrement even if cancelled/panicked
        let in_flight = self.in_flight.clone();
        let _guard = InFlightGuard { in_flight };

        // Record this call
        self.calls.lock().push(MockCall {
            method: request.method.clone(),
            endpoint: request.endpoint.clone(),
            path: request.path.clone(),
            body: request.body.clone(),
            api_key: api_key.to_string(),
            batch_metadata: request.batch_metadata.clone(),
        });

        // Look up the response
        let key = format!("{} {}", request.method, request.path);
        let mock_response = {
            let mut responses = self.responses.lock();
            if let Some(response_queue) = responses.get_mut(&key) {
                if !response_queue.is_empty() {
                    Some(response_queue.remove(0))
                } else {
                    None
                }
            } else {
                None
            }
        };

        match mock_response {
            Some(MockResponse::Immediate(response)) => response,
            Some(MockResponse::Triggered { response, trigger }) => {
                // Wait for the trigger signal before returning the response
                let rx = {
                    let mut trigger_guard = trigger.lock();
                    trigger_guard.take()
                };

                if let Some(rx) = rx {
                    // Wait for trigger (ignore the result - we proceed either way)
                    let _ = rx.await;
                }

                response
            }
            None => {
                // No response configured - return a default error
                Err(crate::error::FusilladeError::Other(anyhow::anyhow!(
                    "No mock response configured for {} {}",
                    request.method,
                    request.path
                )))
            }
        }
    }
}

/// Guard that decrements the in-flight counter when dropped.
/// This ensures the counter is decremented even if the task is cancelled or panics.
struct InFlightGuard {
    in_flight: Arc<AtomicUsize>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::RequestId;

    #[test]
    fn upload_completes_only_after_transport_releases_final_chunk() {
        use http_body::Body as _;

        let progress = UploadProgress::new(3);
        let mut body = std::pin::pin!(ProgressBody {
            remaining: bytes::Bytes::from_static(b"abc"),
            progress: progress.clone(),
            chunk_bytes: DEFAULT_UPLOAD_CHUNK_BYTES,
        });
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());

        let frame = match body.as_mut().poll_frame(&mut context) {
            std::task::Poll::Ready(Some(Ok(frame))) => frame.into_data().unwrap(),
            other => panic!("expected a data frame, got {other:?}"),
        };

        assert_eq!(progress.sent_bytes(), 0);
        drop(frame);
        assert_eq!(progress.sent_bytes(), 3);
    }

    #[test]
    fn configurable_upload_chunk_size_controls_progress_frames() {
        use http_body::Body as _;

        let progress = UploadProgress::new(6);
        let mut body = std::pin::pin!(ProgressBody {
            remaining: bytes::Bytes::from_static(b"abcdef"),
            progress: progress.clone(),
            chunk_bytes: 3,
        });
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());

        let first = match body.as_mut().poll_frame(&mut context) {
            std::task::Poll::Ready(Some(Ok(frame))) => frame.into_data().unwrap(),
            other => panic!("expected first data frame, got {other:?}"),
        };
        assert_eq!(first.len(), 3);
        drop(first);
        assert_eq!(progress.sent_bytes(), 3);

        let second = match body.as_mut().poll_frame(&mut context) {
            std::task::Poll::Ready(Some(Ok(frame))) => frame.into_data().unwrap(),
            other => panic!("expected second data frame, got {other:?}"),
        };
        assert_eq!(second.len(), 3);
        drop(second);
        assert_eq!(progress.sent_bytes(), 6);
    }

    #[test]
    fn upload_watchdog_client_configuration_has_defaults_and_overrides() {
        let client = ReqwestHttpClient::new(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            vec![],
        );
        assert_eq!(client.upload_chunk_bytes, 64 * 1024);
        assert_eq!(client.upload_stall_poll, Duration::from_millis(100));

        let client = client
            .with_upload_chunk_bytes(8 * 1024)
            .with_upload_stall_poll_interval(Duration::from_millis(25));
        assert_eq!(client.upload_chunk_bytes, 8 * 1024);
        assert_eq!(client.upload_stall_poll, Duration::from_millis(25));
    }

    #[test]
    #[should_panic(expected = "upload chunk size must be greater than zero")]
    fn zero_upload_chunk_size_is_rejected() {
        let _ = ReqwestHttpClient::new(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            vec![],
        )
        .with_upload_chunk_bytes(0);
    }

    #[test]
    #[should_panic(expected = "upload stall poll interval must be greater than zero")]
    fn zero_upload_stall_poll_interval_is_rejected() {
        let _ = ReqwestHttpClient::new(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            vec![],
        )
        .with_upload_stall_poll_interval(Duration::ZERO);
    }

    #[tokio::test]
    async fn configurable_upload_stall_poll_controls_first_check() {
        let result = tokio::time::timeout(
            Duration::from_millis(250),
            race_upload_stall(
                std::future::pending::<()>(),
                Some(UploadProgress::new(1)),
                Duration::from_millis(1),
                Duration::from_secs(5),
                "http://example.test",
            ),
        )
        .await;

        assert!(
            result.is_err(),
            "watchdog checked before the configured poll interval"
        );
    }

    #[tokio::test]
    async fn upload_watchdog_aborts_when_progress_stops() {
        let stall_timeout = Duration::from_millis(250);
        let progress = UploadProgress::new(1);

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            race_upload_stall(
                std::future::pending::<()>(),
                Some(progress),
                stall_timeout,
                DEFAULT_UPLOAD_STALL_POLL,
                "http://example.test",
            ),
        )
        .await
        .expect("watchdog did not enforce the stall timeout")
        .unwrap_err();

        assert!(matches!(
            result,
            crate::error::FusilladeError::UploadStallTimeout(_)
        ));
    }

    #[tokio::test]
    async fn upload_watchdog_allows_progress_across_multiple_stall_windows() {
        const PROGRESS_STEPS: u64 = 24;
        let stall_timeout = Duration::from_millis(250);
        let progress = UploadProgress::new(PROGRESS_STEPS);
        let watchdog_progress = progress.clone();
        let (send_complete_tx, send_complete_rx) = tokio::sync::oneshot::channel();
        let watchdog = tokio::spawn(async move {
            race_upload_stall(
                async move { send_complete_rx.await.unwrap() },
                Some(watchdog_progress),
                stall_timeout,
                DEFAULT_UPLOAD_STALL_POLL,
                "http://example.test",
            )
            .await
        });

        let started = std::time::Instant::now();
        let mut pacing = tokio::time::interval(Duration::from_millis(25));
        pacing.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        for _ in 0..PROGRESS_STEPS {
            pacing.tick().await;
            progress.record(1);
            assert!(
                !watchdog.is_finished(),
                "watchdog aborted despite continuing upload progress"
            );
        }
        assert!(
            started.elapsed() > stall_timeout * 2,
            "test did not span multiple complete stall windows"
        );

        send_complete_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), watchdog)
            .await
            .expect("watchdog did not finish after upload and send completion")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn test_mock_client_basic() {
        let mock = MockHttpClient::new();
        mock.add_response(
            "POST /test",
            Ok(HttpResponse {
                status: 200,
                body: "success".to_string(),
            }),
        );

        let request = RequestData {
            id: RequestId::from(uuid::Uuid::new_v4()),
            batch_id: Some(crate::batch::BatchId::from(uuid::Uuid::new_v4())),
            template_id: crate::batch::TemplateId::from(uuid::Uuid::new_v4()),
            custom_id: None,
            endpoint: "https://api.example.com".to_string(),
            method: "POST".to_string(),
            path: "/test".to_string(),
            body: "{}".to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            created_by: String::new(),
            batch_metadata: std::collections::HashMap::new(),
        };

        let response = mock.execute(&request, "test-key").await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "success");

        // Verify call was recorded
        let calls = mock.get_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, "POST");
        assert_eq!(calls[0].path, "/test");
        assert_eq!(calls[0].api_key, "test-key");
    }

    #[tokio::test]
    async fn test_mock_client_multiple_responses() {
        let mock = MockHttpClient::new();
        mock.add_response(
            "GET /status",
            Ok(HttpResponse {
                status: 200,
                body: "first".to_string(),
            }),
        );
        mock.add_response(
            "GET /status",
            Ok(HttpResponse {
                status: 200,
                body: "second".to_string(),
            }),
        );

        let request = RequestData {
            id: RequestId::from(uuid::Uuid::new_v4()),
            batch_id: Some(crate::batch::BatchId::from(uuid::Uuid::new_v4())),
            template_id: crate::batch::TemplateId::from(uuid::Uuid::new_v4()),
            custom_id: None,
            endpoint: "https://api.example.com".to_string(),
            method: "GET".to_string(),
            path: "/status".to_string(),
            body: "".to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            created_by: String::new(),
            batch_metadata: std::collections::HashMap::new(),
        };

        let response1 = mock.execute(&request, "key").await.unwrap();
        assert_eq!(response1.body, "first");

        let response2 = mock.execute(&request, "key").await.unwrap();
        assert_eq!(response2.body, "second");

        assert_eq!(mock.call_count(), 2);
    }

    #[tokio::test]
    async fn test_mock_client_no_response() {
        let mock = MockHttpClient::new();

        let request = RequestData {
            id: RequestId::from(uuid::Uuid::new_v4()),
            batch_id: Some(crate::batch::BatchId::from(uuid::Uuid::new_v4())),
            template_id: crate::batch::TemplateId::from(uuid::Uuid::new_v4()),
            custom_id: None,
            endpoint: "https://api.example.com".to_string(),
            method: "POST".to_string(),
            path: "/unknown".to_string(),
            body: "{}".to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            created_by: String::new(),
            batch_metadata: std::collections::HashMap::new(),
        };

        let result = mock.execute(&request, "key").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_mock_client_with_trigger() {
        let mock = MockHttpClient::new();

        let trigger = mock.add_response_with_trigger(
            "POST /test",
            Ok(HttpResponse {
                status: 200,
                body: "triggered".to_string(),
            }),
        );

        let request = RequestData {
            id: RequestId::from(uuid::Uuid::new_v4()),
            batch_id: Some(crate::batch::BatchId::from(uuid::Uuid::new_v4())),
            template_id: crate::batch::TemplateId::from(uuid::Uuid::new_v4()),
            custom_id: None,
            endpoint: "https://api.example.com".to_string(),
            method: "POST".to_string(),
            path: "/test".to_string(),
            body: "{}".to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            created_by: String::new(),
            batch_metadata: std::collections::HashMap::new(),
        };

        // Spawn the request execution (it will block waiting for trigger)
        let mock_clone = mock.clone();
        let handle = tokio::spawn(async move { mock_clone.execute(&request, "key").await });

        // Give it a moment to start executing
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Verify it hasn't completed yet
        assert!(!handle.is_finished());

        // Now trigger the response
        trigger.send(()).unwrap();

        // Wait for completion
        let response = handle.await.unwrap().unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "triggered");
    }

    #[tokio::test]
    async fn test_mock_client_records_batch_metadata() {
        let mock = MockHttpClient::new();
        mock.add_response(
            "POST /test",
            Ok(HttpResponse {
                status: 200,
                body: "success".to_string(),
            }),
        );

        let mut batch_metadata = std::collections::HashMap::new();
        batch_metadata.insert("id".to_string(), "batch-123".to_string());
        batch_metadata.insert(
            "endpoint".to_string(),
            "https://api.example.com".to_string(),
        );
        batch_metadata.insert("created_at".to_string(), "2025-12-19T12:00:00Z".to_string());
        batch_metadata.insert("completion_window".to_string(), "2s".to_string());

        let request = RequestData {
            id: RequestId::from(uuid::Uuid::new_v4()),
            batch_id: Some(crate::batch::BatchId::from(uuid::Uuid::new_v4())),
            template_id: crate::batch::TemplateId::from(uuid::Uuid::new_v4()),
            custom_id: None,
            endpoint: "https://api.example.com".to_string(),
            method: "POST".to_string(),
            path: "/test".to_string(),
            body: r#"{"key":"value"}"#.to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            created_by: String::new(),
            batch_metadata: batch_metadata.clone(),
        };

        let response = mock.execute(&request, "test-key").await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "success");

        // Verify batch metadata was recorded
        let calls = mock.get_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].batch_metadata.len(), 4);
        assert_eq!(
            calls[0].batch_metadata.get("id"),
            Some(&"batch-123".to_string())
        );
        assert_eq!(
            calls[0].batch_metadata.get("endpoint"),
            Some(&"https://api.example.com".to_string())
        );
        assert_eq!(
            calls[0].batch_metadata.get("created_at"),
            Some(&"2025-12-19T12:00:00Z".to_string())
        );
        assert_eq!(
            calls[0].batch_metadata.get("completion_window"),
            Some(&"2s".to_string())
        );
    }

    #[tokio::test]
    async fn test_reqwest_client_sets_batch_metadata_headers() {
        use axum::{Router, extract::Request, http::StatusCode, routing::post};

        // Create a test server that captures headers
        let app = Router::new().route(
            "/test",
            post(|request: Request| async move {
                let headers = request.headers();

                // Verify batch metadata headers are present and correct
                assert_eq!(
                    headers
                        .get("x-fusillade-batch-id")
                        .and_then(|h| h.to_str().ok()),
                    Some("batch-456"),
                    "Missing or incorrect x-fusillade-batch-id header"
                );
                assert_eq!(
                    headers
                        .get("x-fusillade-batch-endpoint")
                        .and_then(|h| h.to_str().ok()),
                    Some("/v1/completions"),
                    "Missing or incorrect x-fusillade-batch-endpoint header"
                );
                assert_eq!(
                    headers
                        .get("x-fusillade-batch-created-at")
                        .and_then(|h| h.to_str().ok()),
                    Some("2025-12-19T13:00:00Z"),
                    "Missing or incorrect x-fusillade-batch-created-at header"
                );
                assert_eq!(
                    headers
                        .get("x-fusillade-batch-completion-window")
                        .and_then(|h| h.to_str().ok()),
                    Some("24h"),
                    "Missing or incorrect x-fusillade-batch-completion-window header"
                );

                // Also verify standard headers
                assert_eq!(
                    headers.get("authorization").and_then(|h| h.to_str().ok()),
                    Some("Bearer test-api-key"),
                    "Missing or incorrect authorization header"
                );
                assert!(
                    headers.get("x-fusillade-request-id").is_some(),
                    "Missing x-fusillade-request-id header"
                );

                (StatusCode::OK, r#"{"result":"ok"}"#)
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // Give server time to start
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Create request with batch metadata
        let mut batch_metadata = std::collections::HashMap::new();
        batch_metadata.insert("id".to_string(), "batch-456".to_string());
        batch_metadata.insert("endpoint".to_string(), "/v1/completions".to_string());
        batch_metadata.insert("created_at".to_string(), "2025-12-19T13:00:00Z".to_string());
        batch_metadata.insert("completion_window".to_string(), "24h".to_string());

        let request = RequestData {
            id: RequestId::from(uuid::Uuid::new_v4()),
            batch_id: Some(crate::batch::BatchId::from(uuid::Uuid::new_v4())),
            template_id: crate::batch::TemplateId::from(uuid::Uuid::new_v4()),
            custom_id: None,
            endpoint: format!("http://{}", addr),
            method: "POST".to_string(),
            path: "/test".to_string(),
            body: r#"{"prompt":"test"}"#.to_string(),
            model: "test-model".to_string(),
            api_key: "test-api-key".to_string(),
            created_by: String::new(),
            batch_metadata,
        };

        // Use real HTTP client
        let client = ReqwestHttpClient::default();
        let response = client.execute(&request, "test-api-key").await.unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(response.body, r#"{"result":"ok"}"#);
    }

    #[tokio::test]
    async fn test_read_timeout_on_stalled_body() {
        use axum::{Router, http::StatusCode, routing::post};

        // Server sends one SSE event, then stalls forever
        let app = Router::new().route(
            "/test",
            post(|| async {
                use futures::StreamExt;
                let stream = futures::stream::once(async {
                    Ok::<_, std::convert::Infallible>(
                        "data: {\"chunk\":1}\n\n".to_string().into_bytes(),
                    )
                })
                .chain(futures::stream::pending());

                let body = axum::body::Body::from_stream(stream);
                (StatusCode::OK, body)
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // Give server time to start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let request = RequestData {
            id: RequestId::from(uuid::Uuid::new_v4()),
            batch_id: Some(crate::batch::BatchId::from(uuid::Uuid::new_v4())),
            template_id: crate::batch::TemplateId::from(uuid::Uuid::new_v4()),
            custom_id: None,
            endpoint: format!("http://{}", addr),
            method: "POST".to_string(),
            path: "/test".to_string(),
            body: "{}".to_string(),
            model: "test-model".to_string(),
            api_key: "".to_string(),
            created_by: String::new(),
            batch_metadata: std::collections::HashMap::new(),
        };

        let timeout = Duration::from_millis(200);
        let client = ReqwestHttpClient::new(
            timeout,
            timeout,
            ONE_DAY_DURATION,
            vec!["/test".to_string()],
        );
        let result = client.execute(&request, "").await;
        let err = result.expect_err("Expected TokensTimeout for stalled body");

        match err {
            crate::error::FusilladeError::TokensTimeout(msg) => {
                assert!(msg.contains("SSE stream stalled"));
            }
            other => panic!("Expected TokensTimeout, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_read_timeout_on_stalled_headers() {
        // Server accepts connection but never sends headers
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                // Hold the connection open but never respond
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                drop(socket);
            }
        });

        let request = RequestData {
            id: RequestId::from(uuid::Uuid::new_v4()),
            batch_id: Some(crate::batch::BatchId::from(uuid::Uuid::new_v4())),
            template_id: crate::batch::TemplateId::from(uuid::Uuid::new_v4()),
            custom_id: None,
            endpoint: format!("http://{}", addr),
            method: "POST".to_string(),
            path: "/test".to_string(),
            body: "{}".to_string(),
            model: "test-model".to_string(),
            api_key: "".to_string(),
            created_by: String::new(),
            batch_metadata: std::collections::HashMap::new(),
        };

        let timeout = Duration::from_millis(200);
        let client = ReqwestHttpClient::new(
            timeout,
            timeout,
            ONE_DAY_DURATION,
            vec!["/test".to_string()],
        );
        let result = client.execute(&request, "").await;
        let err = result.expect_err("Expected FirstChunkTimeout for stalled headers");

        match err {
            crate::error::FusilladeError::FirstChunkTimeout(msg) => {
                assert!(msg.contains("No response headers from"));
            }
            other => panic!("Expected FirstChunkTimeout, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_body_timeout_on_slow_drip() {
        use axum::{Router, http::StatusCode, routing::post};
        use futures::StreamExt;

        // Server sends one SSE event every 50ms — never trips the 200ms chunk timeout,
        // but exceeds the 300ms body timeout.
        let app = Router::new().route(
            "/test",
            post(|| async {
                let stream = futures::stream::unfold(0u32, |i| async move {
                    if i >= 20 {
                        return None;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    Some((
                        Ok::<_, std::convert::Infallible>(
                            format!("data: {{\"chunk\":{i}}}\n\n").into_bytes(),
                        ),
                        i + 1,
                    ))
                })
                .boxed();
                let body = axum::body::Body::from_stream(stream);
                (StatusCode::OK, body)
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let request = RequestData {
            id: RequestId::from(uuid::Uuid::new_v4()),
            batch_id: Some(crate::batch::BatchId::from(uuid::Uuid::new_v4())),
            template_id: crate::batch::TemplateId::from(uuid::Uuid::new_v4()),
            custom_id: None,
            endpoint: format!("http://{}", addr),
            method: "POST".to_string(),
            path: "/test".to_string(),
            body: "{}".to_string(),
            model: "test-model".to_string(),
            api_key: "".to_string(),
            created_by: String::new(),
            batch_metadata: std::collections::HashMap::new(),
        };

        // chunk_timeout=200ms (never trips), body_timeout=300ms (trips after ~6 chunks)
        let client = ReqwestHttpClient::new(
            ONE_DAY_DURATION,
            Duration::from_millis(200),
            Duration::from_millis(300),
            vec!["/test".to_string()],
        );
        let result = client.execute(&request, "").await;
        let err = result.expect_err("Expected BodyTimeout for slow-drip response");

        match err {
            crate::error::FusilladeError::BodyTimeout(msg) => {
                assert!(msg.contains("Total body read from"));
            }
            other => panic!("Expected BodyTimeout, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_custom_id_with_newline_is_not_retriable() {
        use crate::request::types::FailureReason;

        let request = RequestData {
            id: RequestId::from(uuid::Uuid::new_v4()),
            batch_id: Some(crate::batch::BatchId::from(uuid::Uuid::new_v4())),
            template_id: crate::batch::TemplateId::from(uuid::Uuid::new_v4()),
            custom_id: Some("invalid\ncustom_id".to_string()), // Contains newline
            endpoint: "https://api.example.com".to_string(),
            method: "POST".to_string(),
            path: "/test".to_string(),
            body: "{}".to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            created_by: String::new(),
            batch_metadata: std::collections::HashMap::new(),
        };

        let client = ReqwestHttpClient::default();
        let result = client.execute(&request, "test-key").await;
        let err = result.expect_err("Expected builder error for invalid header value");

        // Verify it's a builder error and map to FailureReason (same logic as transitions.rs)
        let reason = match err {
            crate::error::FusilladeError::HttpRequestBuilder(error) => {
                FailureReason::RequestBuilderError { error }
            }
            _ => panic!("Expected HttpClient builder error, got: {:?}", err),
        };

        assert!(
            !reason.is_retriable(),
            "Builder errors should not be retriable"
        );
    }

    #[tokio::test]
    async fn test_streaming_reassembles_sse_into_json() {
        use axum::{Router, http::StatusCode, routing::post};

        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                let sse = concat!(
                    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n",
                    "data: [DONE]\n\n",
                );
                (
                    StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    sse,
                )
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let request = RequestData {
            id: RequestId::from(uuid::Uuid::new_v4()),
            batch_id: Some(crate::batch::BatchId::from(uuid::Uuid::new_v4())),
            template_id: crate::batch::TemplateId::from(uuid::Uuid::new_v4()),
            custom_id: None,
            endpoint: format!("http://{}", addr),
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            body: r#"{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}"#.to_string(),
            model: "gpt-4".to_string(),
            api_key: "".to_string(),
            created_by: String::new(),
            batch_metadata: std::collections::HashMap::new(),
        };

        let client = ReqwestHttpClient::new(
            ONE_DAY_DURATION,
            ONE_DAY_DURATION,
            ONE_DAY_DURATION,
            vec!["/v1/chat/completions".to_string()],
        );
        let response = client.execute(&request, "").await.unwrap();
        assert_eq!(response.status, 200);

        let body: serde_json::Value =
            serde_json::from_str(&response.body).expect("reassembled body should be valid JSON");
        assert_eq!(body["object"], "chat.completion");
        assert_eq!(body["choices"][0]["message"]["content"], "Hello world");
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
        assert_eq!(body["usage"]["total_tokens"], 7);
    }

    #[tokio::test]
    async fn test_streaming_error_status_skips_reassembly() {
        use axum::{Router, http::StatusCode, routing::post};

        // An error-status SSE response whose chunks, if reassembled, would
        // synthesize a successful-looking `chat.completion`. The `status < 400`
        // guard must bypass reassembly and return the raw payload so the real
        // 5xx drives retry classification (rather than a misleading or
        // degenerate `{"choices":[],"usage":null}` completion).
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                let sse = concat!(
                    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n",
                );
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [("content-type", "text/event-stream")],
                    sse,
                )
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let request = RequestData {
            id: RequestId::from(uuid::Uuid::new_v4()),
            batch_id: Some(crate::batch::BatchId::from(uuid::Uuid::new_v4())),
            template_id: crate::batch::TemplateId::from(uuid::Uuid::new_v4()),
            custom_id: None,
            endpoint: format!("http://{}", addr),
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            body: r#"{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}"#.to_string(),
            model: "gpt-4".to_string(),
            api_key: "".to_string(),
            created_by: String::new(),
            batch_metadata: std::collections::HashMap::new(),
        };

        // Default client has openai reassembly enabled.
        let client = ReqwestHttpClient::new(
            ONE_DAY_DURATION,
            ONE_DAY_DURATION,
            ONE_DAY_DURATION,
            vec!["/v1/chat/completions".to_string()],
        );
        let response = client.execute(&request, "").await.unwrap();

        // Real HTTP status preserved, so retry classification sees a 5xx.
        assert_eq!(response.status, 500);

        // Body is the raw SSE payload, not reassembled: the chunk-level
        // `chat.completion.chunk` objects survive verbatim, and the raw
        // multi-chunk body does not parse as one reassembled JSON object.
        assert!(
            response.body.contains("chat.completion.chunk"),
            "error-status body should be raw chunks, got: {}",
            response.body
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(&response.body).is_err(),
            "raw multi-chunk body should not parse as a single reassembled object, got: {}",
            response.body
        );
    }

    #[tokio::test]
    async fn test_streaming_json_error_preserves_body() {
        use axum::{Json, Router, http::StatusCode, routing::post};

        let expected = serde_json::json!({
            "error": {
                "code": "unsupported_parameter",
                "message": "Unsupported parameter 'chat_template_kwargs.enable_thinking'; use 'reasoning_effort'.",
                "param": "chat_template_kwargs.enable_thinking",
                "type": "invalid_request_error"
            }
        });
        let response_body = expected.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let body = response_body.clone();
                async move { (StatusCode::BAD_REQUEST, Json(body)) }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let request = RequestData {
            id: RequestId::from(uuid::Uuid::new_v4()),
            batch_id: Some(crate::batch::BatchId::from(uuid::Uuid::new_v4())),
            template_id: crate::batch::TemplateId::from(uuid::Uuid::new_v4()),
            custom_id: None,
            endpoint: format!("http://{}", addr),
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            body: r#"{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}"#.to_string(),
            model: "gpt-4".to_string(),
            api_key: "".to_string(),
            created_by: String::new(),
            batch_metadata: std::collections::HashMap::new(),
        };

        let client = ReqwestHttpClient::new(
            ONE_DAY_DURATION,
            ONE_DAY_DURATION,
            ONE_DAY_DURATION,
            vec!["/v1/chat/completions".to_string()],
        );
        let response = client.execute(&request, "").await.unwrap();

        assert_eq!(response.status, 400);
        let body: serde_json::Value = serde_json::from_str(&response.body)
            .expect("JSON provider error body should be preserved");
        assert_eq!(body, expected);
    }

    /// Captures every event's `data` field into a shared Vec. Used by
    /// the streaming callback tests below. `StreamEvent` itself is
    /// borrowed from fusillade's per-chunk parse buffer, so the test
    /// pulls out the owned fields it actually wants to assert on.
    struct CapturingCallback {
        data: Arc<Mutex<Vec<String>>>,
    }

    impl StreamEventCallback for CapturingCallback {
        fn on_event(&self, event: &StreamEvent<'_>) {
            self.data.lock().push(event.data.to_string());
        }
    }

    #[tokio::test]
    async fn test_streaming_callback_fires_per_sse_event() {
        use axum::{Router, http::StatusCode, routing::post};

        // Same SSE payload as `test_streaming_reassembles_sse_into_json`
        // so we get a known 3-event stream (two delta chunks + [DONE]).
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                let sse = concat!(
                    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n",
                    "data: [DONE]\n\n",
                );
                (
                    StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    sse,
                )
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let request = RequestData {
            id: RequestId::from(uuid::Uuid::new_v4()),
            batch_id: Some(crate::batch::BatchId::from(uuid::Uuid::new_v4())),
            template_id: crate::batch::TemplateId::from(uuid::Uuid::new_v4()),
            custom_id: None,
            endpoint: format!("http://{}", addr),
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            body: r#"{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}"#.to_string(),
            model: "gpt-4".to_string(),
            api_key: "".to_string(),
            created_by: String::new(),
            batch_metadata: std::collections::HashMap::new(),
        };

        let client = ReqwestHttpClient::new(
            ONE_DAY_DURATION,
            ONE_DAY_DURATION,
            ONE_DAY_DURATION,
            vec!["/v1/chat/completions".to_string()],
        );

        let captured = Arc::new(Mutex::new(Vec::new()));
        let callback: Arc<dyn StreamEventCallback> = Arc::new(CapturingCallback {
            data: captured.clone(),
        });
        let response = client
            .execute_with_event_callback(&request, "", Some(callback))
            .await
            .unwrap();
        assert_eq!(response.status, 200);

        // Reassembly is unchanged by the callback path.
        let body: serde_json::Value =
            serde_json::from_str(&response.body).expect("reassembled body should be valid JSON");
        assert_eq!(body["choices"][0]["message"]["content"], "Hello world");

        // Callback fired in arrival order for every SSE event, including
        // the terminal `[DONE]` marker (fusillade hands it through so
        // consumers can detect end-of-stream without re-parsing).
        let data = captured.lock().clone();
        assert_eq!(data.len(), 3, "got events: {:?}", data);
        assert!(data[0].contains("\"content\":\"Hello\""));
        assert!(data[1].contains("\"content\":\" world\""));
        assert_eq!(data[2], "[DONE]");
    }

    #[tokio::test]
    async fn test_callback_not_invoked_for_non_streaming_request() {
        use axum::{Router, http::StatusCode, routing::post};

        // Path is *not* in the streamable_endpoints list, so the client
        // takes the non-streaming branch. The callback must not fire.
        let app = Router::new().route(
            "/test",
            post(|| async { (StatusCode::OK, r#"{"ok":true}"#) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let request = RequestData {
            id: RequestId::from(uuid::Uuid::new_v4()),
            batch_id: Some(crate::batch::BatchId::from(uuid::Uuid::new_v4())),
            template_id: crate::batch::TemplateId::from(uuid::Uuid::new_v4()),
            custom_id: None,
            endpoint: format!("http://{}", addr),
            method: "POST".to_string(),
            path: "/test".to_string(),
            body: "{}".to_string(),
            model: "test-model".to_string(),
            api_key: "".to_string(),
            created_by: String::new(),
            batch_metadata: std::collections::HashMap::new(),
        };

        let client = ReqwestHttpClient::default(); // no streamable_endpoints

        let captured = Arc::new(Mutex::new(Vec::new()));
        let callback: Arc<dyn StreamEventCallback> = Arc::new(CapturingCallback {
            data: captured.clone(),
        });
        client
            .execute_with_event_callback(&request, "", Some(callback))
            .await
            .unwrap();

        assert!(
            captured.lock().is_empty(),
            "callback fired on a non-streaming response: {:?}",
            captured.lock()
        );
    }
}
