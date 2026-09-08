//! HTTP client abstraction for making requests.
//!
//! This module defines the `HttpClient` trait to abstract HTTP request execution,
//! enabling testability with mock implementations.

use crate::error::Result;
pub use crate::request::HttpResponse;
use crate::request::RequestData;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use opentelemetry::trace::TraceContextExt;
use std::sync::Arc;
use std::time::Duration;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Trait for executing HTTP requests.
///
/// This abstraction allows for different implementations (production vs. testing)
/// and makes the daemon processing logic testable without making real HTTP calls.
///
/// # Example
/// ```ignore
/// let client = ReqwestHttpClient::new(Duration::from_secs(300), Duration::from_secs(30), Duration::from_secs(600));
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
}

// ============================================================================
// Production Implementation using reqwest
// ============================================================================

/// Production HTTP client using reqwest.
///
/// This implementation makes real HTTP requests to external endpoints, and
/// always reads a plain response body.
///
/// Requests to a streamable endpoint are still served as a stream upstream,
/// because streaming is how the provider is made to report the token usage this
/// bills from. Deciding which those are, forcing the stream, reading it and
/// reassembling it all happen in the layer in front of this client, which knows
/// a request came from here by the `X-Fusillade-Request-Id` header stamped on
/// every one. This client is not party to any of it.
///
/// Timeouts are configured at construction time. `first_chunk_timeout +
/// body_timeout` is applied as a single overall reqwest timeout covering the
/// whole request. The stream-shaped budgets are enforced where the stream is
/// read, and one of them firing arrives here as an ordinary 504, retried like
/// any other.
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
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            first_chunk_timeout,
            chunk_timeout,
            body_timeout,
            upload_stall_timeout: DEFAULT_UPLOAD_STALL_TIMEOUT,
            upload_chunk_bytes: DEFAULT_UPLOAD_CHUNK_BYTES,
            upload_stall_poll: DEFAULT_UPLOAD_STALL_POLL,
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
}

impl Default for ReqwestHttpClient {
    fn default() -> Self {
        Self::new(ONE_DAY_DURATION, ONE_DAY_DURATION, ONE_DAY_DURATION)
    }
}

/// Long but finite fallback timeout (24 hours) used when no explicit timeout is configured.
const ONE_DAY_DURATION: Duration = Duration::from_secs(86_400);

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
    #[tracing::instrument(name = "fusillade.execute", skip(self, request, api_key), fields(
        otel.name = %format!("{} {}", request.method, request.path),
    ))]
    async fn execute(&self, request: &RequestData, api_key: &str) -> Result<HttpResponse> {
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

        self.execute_non_streaming(request, req, &url, upload).await
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

/// Submission timestamp of a request, parsed from the `created_at` batch
/// metadata field (RFC3339; the claim queries populate it for both batch and
/// batchless rows — the batch's creation for batch rows, the row's own for
/// batchless).
pub(crate) fn submission_time(request: &RequestData) -> Option<DateTime<Utc>> {
    request
        .batch_metadata
        .get("created_at")?
        .parse::<DateTime<Utc>>()
        .ok()
}

/// Record time-to-first-token measured from submission (`created_at`) — the
/// quantity behind the async-tier "starts within a minute" SLO, spanning queue
/// wait + claim + dispatch + prefill in one measurement (quantiles of separate
/// pickup/dispatch histograms cannot be summed).
///
/// Recorded when an attempt's response starts with a 2xx: at headers for
/// non-streaming (engines send the whole body at once, so this is close to
/// time-to-last-token — conservative in the right direction), at the first SSE
/// event for streaming (headers can arrive while the request is still queued
/// upstream). A 2xx-opening attempt that later fails mid-stream and is retried
/// contributes an extra sample; that is rare enough to document rather than
/// thread response-start timestamps through `HttpResponse`.
fn record_submission_ttft(request: &RequestData, status: u16) {
    if let Some((seconds, completion_window)) = submission_ttft_sample(request, status, Utc::now())
    {
        metrics::histogram!(
            "fusillade_request_time_to_first_token_seconds",
            "model" => request.model.clone(),
            "completion_window" => completion_window,
        )
        .record(seconds);
    }
}

/// The pure decision behind [`record_submission_ttft`]: `Some((seconds,
/// completion_window))` when a sample should be recorded — the response opened
/// 2xx, the row carries a parseable `created_at`, and the clock didn't run
/// backwards.
fn submission_ttft_sample(
    request: &RequestData,
    status: u16,
    now: DateTime<Utc>,
) -> Option<(f64, String)> {
    if !(200..300).contains(&status) {
        return None;
    }
    let created_at = submission_time(request)?;
    let elapsed_ms = (now - created_at).num_milliseconds();
    if elapsed_ms < 0 {
        return None;
    }
    let completion_window = request
        .batch_metadata
        .get("completion_window")
        .cloned()
        .unwrap_or_default();
    Some((elapsed_ms as f64 / 1000.0, completion_window))
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

        record_submission_ttft(request, status);
        let body = response.text().await.map_err(map_reqwest_error)?;

        tracing::debug!(
            request_id = %request.id,
            status = status,
            response_len = body.len(),
            "HTTP request completed"
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

    fn ttft_test_request(metadata: &[(&str, &str)]) -> RequestData {
        RequestData {
            id: RequestId::from(uuid::Uuid::new_v4()),
            batch_id: None,
            template_id: crate::batch::TemplateId::from(uuid::Uuid::new_v4()),
            custom_id: None,
            endpoint: "https://api.example.com".to_string(),
            method: "POST".to_string(),
            path: "/test".to_string(),
            body: "{}".to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            created_by: String::new(),
            batch_metadata: metadata
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn submission_ttft_sample_gates_and_measures() {
        let now: DateTime<Utc> = "2026-07-23T12:00:30Z".parse().unwrap();
        let with_meta = ttft_test_request(&[
            ("created_at", "2026-07-23T12:00:00.000000Z"),
            ("completion_window", "1h"),
        ]);

        // Valid: 2xx + parseable created_at 30s ago.
        let (seconds, window) =
            submission_ttft_sample(&with_meta, 200, now).expect("should record");
        assert!((seconds - 30.0).abs() < 0.001, "elapsed = {seconds}");
        assert_eq!(window, "1h");

        // Non-2xx openings never record.
        assert_eq!(submission_ttft_sample(&with_meta, 429, now), None);
        assert_eq!(submission_ttft_sample(&with_meta, 500, now), None);

        // Missing or unparseable created_at → no sample rather than a junk one.
        assert_eq!(
            submission_ttft_sample(&ttft_test_request(&[]), 200, now),
            None
        );
        assert_eq!(
            submission_ttft_sample(
                &ttft_test_request(&[("created_at", "not a time")]),
                200,
                now
            ),
            None
        );

        // Clock skew (created_at in the future) → no sample.
        let future = ttft_test_request(&[("created_at", "2026-07-23T12:05:00Z")]);
        assert_eq!(submission_ttft_sample(&future, 200, now), None);

        // Missing window falls back to empty label, still records.
        let no_window = ttft_test_request(&[("created_at", "2026-07-23T12:00:00Z")]);
        let (_, window) = submission_ttft_sample(&no_window, 200, now).unwrap();
        assert_eq!(window, "");
    }

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
                // The submitter's client, which dwctl reads back into
                // `http_analytics.user_agent`: this dispatch is the only place a batch's
                // caller is recoverable, since the client below sends no User-Agent of its
                // own. Pinned here because the underscore-to-hyphen rewrite happens in this
                // function, so the key stored at creation and the header dwctl looks for
                // are only equal by this line.
                assert_eq!(
                    headers
                        .get("x-fusillade-batch-dw-user-agent")
                        .and_then(|h| h.to_str().ok()),
                    Some("claude-cli/1.2.3"),
                    "Missing or incorrect x-fusillade-batch-dw-user-agent header"
                );
                assert!(
                    headers.get("user-agent").is_none(),
                    "the dispatching client must not add a User-Agent of its own — if it \
                     ever does, analytics must keep preferring the batch header"
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
        // Underscored at rest (it is a metadata key), hyphenated on the wire.
        batch_metadata.insert("dw_user_agent".to_string(), "claude-cli/1.2.3".to_string());

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
}
