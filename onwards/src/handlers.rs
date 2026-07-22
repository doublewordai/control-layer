//! HTTP request handlers for the proxy server
//!
//! This module contains the main Axum handlers that process incoming requests,
//! route them to appropriate targets, and handle authentication and rate limiting.

use crate::AppState;
use crate::auth;
use crate::client::HttpClient;
use crate::errors::{ErrorResponseBody, OnwardsErrorResponse};
use crate::models::ListModelResponse;
use crate::sse::SseBufferedStream;
use crate::target::{ConcurrencyGuard, RoutingAction, Target};
use axum::{
    Json,
    extract::Request,
    extract::State,
    http::{
        HeaderMap, HeaderName, HeaderValue, StatusCode, Uri,
        header::{CONTENT_LENGTH, TRANSFER_ENCODING},
    },
    response::{IntoResponse, Response},
};
use opentelemetry::propagation::{Extractor, Injector, TextMapPropagator};
use serde_json::map::Entry;
use tracing::{Instrument, debug, error, instrument, trace, warn};

/// Adapter to extract W3C trace context from an axum HeaderMap.
struct HeaderExtractor<'a>(&'a HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

/// Adapter to inject W3C trace context into an axum HeaderMap.
struct HeaderInjector<'a>(&'a mut HeaderMap);

impl Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(name), Ok(val)) = (key.parse::<HeaderName>(), HeaderValue::from_str(&value)) {
            self.0.insert(name, val);
        }
    }
}

/// Record HTTP response status code on the current span
fn record_response_status(status_code: u16) {
    tracing::Span::current().record("http.response.status_code", status_code);
}

/// Detect a provider error envelope embedded in an HTTP 200 body.
///
/// Some upstreams return `200 OK` with the real error in
/// the response body instead of using the HTTP status line, e.g.
/// `{"error":{"code":429,"message":"Provider returned error"}}`. The error may
/// be the whole body or sit alongside otherwise-valid fields
/// (`{"id":...,"choices":[],"error":{...}}`). When the embedded `error.code` is
/// a numeric HTTP status in the 4xx/5xx range, return it so the caller can treat
/// the "200" as that status.
///
/// Returns `None` for normal responses — no `error` object, or a non-numeric /
/// out-of-range code (e.g. string codes like `"rate_limit_exceeded"`,
/// which always arrive with a correct HTTP status anyway) — so well-formed
/// completions are never disturbed.
fn embedded_error_status(body: &serde_json::Value) -> Option<u16> {
    let code = body.get("error")?.get("code")?;
    let status = code
        .as_u64()
        .or_else(|| code.as_str().and_then(|s| s.parse::<u64>().ok()))?;
    let status = u16::try_from(status).ok()?;
    (400..600).contains(&status).then_some(status)
}

/// Classification of a single SSE event for the streaming embedded-error peek.
#[derive(Debug, PartialEq, Eq)]
enum SseEventKind {
    /// A `data:` frame carrying an embedded provider error — the contained
    /// `error.code`, per [`embedded_error_status`].
    Error(u16),
    /// A `data:` frame carrying normal content (or the `[DONE]` sentinel).
    Data,
    /// No `data:` field: an SSE comment / keep-alive (e.g. `: keep-alive`), which
    /// the caller peeks past to reach the first real frame.
    Comment,
}

/// Classify a single SSE event (a complete frame already reassembled by
/// [`SseBufferedStream`]).
///
/// Some providers open a `200 OK` stream and send the error as the
/// first `data:` frame (`data: {"error":{"code":429,...}}`) rather than content.
/// Per the SSE spec an event's `data:` lines are concatenated with `\n` (with at
/// most one space after the colon stripped) and parsed once — so compact,
/// multi-line, and non-spaced (`data:{...}`) framing are all handled. A frame
/// with no `data:` field is a comment/keep-alive.
fn classify_sse_event(chunk: &[u8]) -> SseEventKind {
    let Ok(text) = std::str::from_utf8(chunk) else {
        // Non-UTF-8 can't be an error envelope we understand — forward as-is.
        return SseEventKind::Data;
    };
    let mut data = String::new();
    let mut has_data = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            has_data = true;
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if !has_data {
        return SseEventKind::Comment;
    }
    if data.trim() == "[DONE]" {
        return SseEventKind::Data;
    }
    match serde_json::from_str::<serde_json::Value>(data.trim())
        .ok()
        .as_ref()
        .and_then(embedded_error_status)
    {
        Some(status) => SseEventKind::Error(status),
        None => SseEventKind::Data,
    }
}

/// Internal enum to distinguish timeout from network error in upstream requests
enum UpstreamOutcome {
    Timeout,
    Error(Box<dyn std::error::Error + Send + Sync>),
}

/// RAII guard that decrements the inflight gauges on drop.
///
/// Decrements the global `onwards_requests_inflight` always, and the per-model
/// `onwards_model_inflight{model=…}` once the model has been resolved (see
/// [`InflightGuard::set_model`]). Because the guard rides the response body via
/// [`GuardedStream`], both gauges stay incremented for the full lifetime of a
/// streaming response and only drop when the stream finishes — so
/// `onwards_model_inflight` reflects *active streams per model*, not requests
/// accepted. A downstream autoscaler (scouter) polls this on the Dynamo gateway
/// to gate graceful scale-down: a model being drained is only scaled to zero
/// once its in-flight count hits zero (no active streams to cut).
struct InflightGuard {
    /// Resolved model name, set once `extract_model_from_request` succeeds.
    /// `None` for requests that error before model resolution (e.g. an oversized
    /// body or an unparseable model), which therefore only count toward the
    /// global gauge.
    model: Option<String>,
}

impl InflightGuard {
    /// Associate the resolved model with this in-flight request and increment the
    /// per-model gauge. Must be called **after** the model is validated against the
    /// configured targets (only known models get a series), so the gauge's label
    /// set is bounded by configured models — an arbitrary/unknown model from a
    /// request body never registers a series (that would be an unbounded-cardinality
    /// vector, made worse by our deliberate no-eviction policy). Called at most once
    /// per guard; the matching decrement happens in `Drop`. On the Dynamo gateway
    /// the requested model is the served model, so this label matches what the
    /// autoscaler scales and what `/v1/models` lists.
    fn set_model(&mut self, model: &str) {
        debug_assert!(
            self.model.is_none(),
            "set_model must be called at most once per guard (else the per-model gauge leaks)"
        );
        let model = model.to_string();
        metrics::gauge!("onwards_model_inflight", "model" => model.clone()).increment(1.0);
        self.model = Some(model);
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        metrics::gauge!("onwards_requests_inflight").decrement(1.0);
        if let Some(model) = self.model.take() {
            metrics::gauge!("onwards_model_inflight", "model" => model).decrement(1.0);
        }
    }
}

/// A stream wrapper that keeps a [`ConcurrencyGuard`] and [`InflightGuard`] alive
/// until the stream is fully consumed or dropped. This ensures the active connection
/// count and inflight gauge are decremented when the response body finishes, not
/// when the handler returns — critical for streaming responses where the body
/// outlives the handler.
struct GuardedStream<S> {
    inner: S,
    _guard: ConcurrencyGuard,
    _inflight_guard: InflightGuard,
}

impl<S, E> futures_util::Stream for GuardedStream<S>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
{
    type Item = Result<bytes::Bytes, E>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

/// Stores the original model name requested by the client
///
/// This is used to rewrite the model field in responses when `onwards_model` is configured
#[derive(Clone, Debug)]
struct OriginalModel(String);

/// Response extension carrying the resolved trust level of the provider that handled the request.
/// Set by `target_message_handler` so strict-mode handlers can read it without a second pool lookup.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedTrust(pub(crate) bool);

/// Response extension identifying the upstream provider that produced this response.
///
/// Present whenever an upstream produced the response that reached the client —
/// including passed-through non-2xx upstream statuses. Set after final
/// load-balancer selection (i.e. after any fallback/retry), so it names the
/// provider whose response was returned; its presence means "an upstream
/// answered", not "the request succeeded" — check the status code for that.
/// Absent when no upstream produced a response (auth/validation rejections,
/// exhausted fallbacks, gateway-generated errors). For streaming tool loops the
/// response is returned before follow-up iterations run, so it names the
/// provider of the initial iteration. Integrators (e.g. request-logging
/// middleware) can read it to attribute traffic to a concrete upstream without
/// re-deriving the routing decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServedBy {
    /// Full URL of the upstream target that served the request.
    pub url: String,
    /// The upstream model name after any `onwards_model` rewrite, when configured.
    pub onwards_model: Option<String>,
}

/// Resolve whether W3C trace context headers should be propagated to an
/// upstream provider. The per-provider `propagate_trace_context` overrides;
/// when unset, defaults to the resolved trusted value (per-provider `trusted`
/// falling back to the pool-level setting). Extracted as a pure function for
/// unit testing.
fn resolve_trace_propagation(target: &Target, pool_trusted: bool) -> bool {
    target
        .propagate_trace_context
        .unwrap_or_else(|| target.trusted.unwrap_or(pool_trusted))
}

/// Remove W3C trace-context headers (`traceparent`, `tracestate`) from the
/// outbound header set. Used when trace propagation is disabled for an
/// upstream, so an inbound trace context from the caller is not forwarded to
/// (and re-emitted by) an untrusted third party. Extracted as a pure
/// function for unit testing.
fn withhold_trace_context(headers: &mut HeaderMap) {
    headers.remove("traceparent");
    headers.remove("tracestate");
}

/// Filters and modifies headers before forwarding to upstream
///
/// This function implements RFC 7230 compliant proxy behavior by:
/// - Removing hop-by-hop headers (connection, keep-alive, etc.)
/// - Stripping authentication headers to prevent credential leakage
/// - Removing browser-specific context headers (sec-*, origin, referer)
/// - Adding upstream authentication if configured
/// - Adding X-Forwarded-* headers for transparency
fn filter_headers_for_upstream(headers: &mut HeaderMap, target: &Target) {
    // Headers to remove: hop-by-hop (RFC 7230), auth, browser context, and routing headers
    const HEADERS_TO_STRIP: &[&str] = &[
        // RFC 7230 hop-by-hop headers (MUST remove per spec)
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "upgrade",
        // Authentication headers (prevent credential leakage to downstream)
        "authorization",
        "x-api-key",
        "api-key",
        // Browser security context headers (meaningless to upstream APIs)
        "sec-fetch-site",
        "sec-fetch-mode",
        "sec-fetch-dest",
        "sec-fetch-user",
        // Browser context leakage
        "origin",
        "referer",
        // Client state (security)
        "cookie",
        // HTTP caching (irrelevant since we buffer full responses)
        "if-modified-since",
        "if-none-match",
        "if-match",
        "if-unmodified-since",
        "if-range",
        // Our routing headers (already consumed)
        "model-override",
    ];

    for header in HEADERS_TO_STRIP {
        headers.remove(*header);
    }

    // Remove all sec-ch-ua* headers (Chrome User-Agent Client Hints)
    // These have many variants (sec-ch-ua, sec-ch-ua-mobile, sec-ch-ua-platform, etc.)
    let sec_ch_ua_headers: Vec<_> = headers
        .keys()
        .filter(|name| name.as_str().starts_with("sec-ch-ua"))
        .cloned()
        .collect();
    for header in sec_ch_ua_headers {
        headers.remove(header);
    }

    // Add Authorization header if target requires authentication to upstream
    if let Some(key) = &target.onwards_key {
        let header_name_str = target
            .upstream_auth_header_name
            .as_deref()
            .unwrap_or("Authorization");
        let header_name = HeaderName::from_bytes(header_name_str.as_bytes()).unwrap();
        let prefix = target
            .upstream_auth_header_prefix
            .as_deref()
            .unwrap_or("Bearer ");
        let header_value = format!("{}{}", prefix, key);
        debug!(
            "Adding {} header for upstream {}",
            header_name_str, target.url
        );
        headers.insert(header_name, header_value.parse().unwrap());
    } else {
        debug!(
            "No upstream authentication configured for target {}",
            target.url
        );
    }

    // Add X-Forwarded headers for transparency (preserve original request context)
    // Note: We don't have access to the original client IP in this handler,
    // so we only set X-Forwarded-Proto for now
    headers.insert("x-forwarded-proto", "https".parse().unwrap());
}

/// The main handler responsible for forwarding requests to targets
/// TODO(fergus): Better error messages beyond raw status codes.
pub async fn target_message_handler<T: HttpClient>(
    State(state): State<AppState<T>>,
    mut req: axum::extract::Request,
) -> Result<Response, OnwardsErrorResponse> {
    // Create the tracing span BEFORE entering it so that set_parent() correctly
    // updates the OTel parent context. With #[instrument], the OTel span is started
    // on the first enter() which happens before the function body runs — making
    // set_parent() inside the body too late to change the trace ID.
    // Here we create the span, optionally set the parent, then run the body via
    // .instrument(span) which enters it for the first time with the correct context.
    let span = tracing::info_span!(
        "onwards.request",
        otel.name = "onwards.request",
        gen_ai.request.model = tracing::field::Empty,
        http.response.status_code = tracing::field::Empty,
    );

    // Extract W3C trace context (traceparent + tracestate) from inbound headers
    // BEFORE the span is first entered. This stitches cross-service traces (e.g.
    // remote onwards receiving requests from local onwards).
    if req.headers().contains_key("traceparent") {
        let propagator = opentelemetry_sdk::propagation::TraceContextPropagator::new();
        let parent_ctx = propagator.extract(&HeaderExtractor(req.headers()));
        use tracing_opentelemetry::OpenTelemetrySpanExt;
        let _ = span.set_parent(parent_ctx);
    }

    async move {

    // Track inflight requests for observability. The guard is moved into GuardedStream
    // on the success path so the gauge stays incremented for the full lifetime of
    // streaming response bodies.
    metrics::gauge!("onwards_requests_inflight").increment(1.0);
    let mut inflight_guard = Some(InflightGuard { model: None });

    // Extract the request body. TODO(fergus): make this step conditional: its not necessary if we
    // extract the model from the header.
    let mut body_bytes =
        match axum::body::to_bytes(std::mem::take(req.body_mut()), state.body_limit).await {
            Ok(bytes) => bytes,
            // to_bytes only fails when the body exceeds the limit (or the
            // connection drops mid-body); report it as an explicit 413 rather
            // than an opaque 500.
            Err(_) => return Err(OnwardsErrorResponse::payload_too_large(state.body_limit)),
        };

    // Apply body transformation if provided
    if let Some(ref transform_fn) = state.body_transform_fn {
        let path = req.uri().path();
        if let Some(transformed_body) = transform_fn(path, req.headers(), &body_bytes) {
            debug!("Applied body transformation for path: {}", path);
            body_bytes = transformed_body;
        }
    }

    // Log incoming request metadata for debugging.
    // ZDR: never log the request body or headers — bodies carry prompt content
    // and headers carry auth credentials. Only method, path, and body length.
    trace!(
        method = %req.method(),
        uri = %req.uri(),
        body_len = body_bytes.len(),
        "Incoming request"
    );

    // Extract the model using the shared function
    let model_name = match crate::extract_model_from_request(req.headers(), &body_bytes) {
        Some(model) => model,
        None => {
            record_response_status(400);
            return Err(OnwardsErrorResponse::bad_request(
                "Could not parse onwards model from request. 'model' parameter must be supplied in either the body or in the Model-Override header.",
                Some("model"),
            ));
        }
    };

    // Record model in span for trace correlation
    tracing::Span::current().record("gen_ai.request.model", &model_name);

    // Store original model in request extensions for response sanitization
    req.extensions_mut()
        .insert(OriginalModel(model_name.clone()));

    trace!("Received request for model: {}", model_name);
    trace!(
        "Available targets: {:?}",
        state
            .targets
            .targets
            .iter()
            .map(|entry| entry.key().clone())
            .collect::<Vec<_>>()
    );

    let mut pool = match state.targets.targets.get(&model_name) {
        Some(pool) => {
            // Now that the model is known to be a configured target, tag the
            // in-flight guard so `onwards_model_inflight{model=…}` tracks this
            // request for its whole lifetime (the guard moves into GuardedStream on
            // the streaming success path and decrements when the body finishes).
            // Gating on a known model bounds the gauge's label set — an unknown
            // model 404s above without ever creating a series.
            if let Some(guard) = inflight_guard.as_mut() {
                guard.set_model(&model_name);
            }
            pool.clone()
        }
        None => {
            debug!("No target found for model: {}", model_name);
            record_response_status(404);
            return Err(OnwardsErrorResponse::model_not_found(model_name.as_str()));
        }
    };

    let canonical_request_path = req.uri().path().to_string();

    // Extract bearer token for authentication and rate limiting
    let bearer_token = req
        .headers()
        .get("authorization")
        .and_then(|auth_header| auth_header.to_str().ok())
        .and_then(|auth_value| auth_value.strip_prefix("Bearer "));

    // Validate API key using pool-level keys
    if let Some(keys) = pool.keys() {
        match bearer_token {
            Some(token) => {
                trace!("Validating bearer token against pool keys");
                if auth::validate_bearer_token(keys, token) {
                    debug!("Bearer token validation successful");
                } else {
                    debug!("Bearer token validation failed - token not in key set");
                    record_response_status(403);
                    return Err(OnwardsErrorResponse::forbidden());
                }
            }
            None => {
                debug!("No bearer token found in authorization header");
                record_response_status(401);
                return Err(OnwardsErrorResponse::unauthorized());
            }
        }
    } else {
        debug!(
            "Pool '{}' has no keys configured - allowing request",
            model_name
        );
    }

    // Evaluate routing rules against key labels (after auth, before rate limiting).
    // Rules on the pool are matched against the authenticated key's labels.
    // Note: routing rules are NOT re-evaluated on the redirect target pool.
    if !pool.routing_rules().is_empty()
        && let Some(token) = bearer_token
    {
            let labels = state
                .targets
                .key_labels
                .get(token)
                .map(|r| r.value().clone())
                .unwrap_or_default();

            // Clone the action to release the borrow on pool before potentially reassigning it
            let action = pool.evaluate_routing_rules(&labels).cloned();

            if let Some(action) = action {
                match action {
                    RoutingAction::Deny => {
                        debug!(
                            "Routing rule denied request for model '{}' with labels {:?}",
                            model_name, labels
                        );
                        record_response_status(403);
                        return Err(OnwardsErrorResponse::forbidden());
                    }
                    RoutingAction::Redirect {
                        target: ref redirect_alias,
                    } => {
                        debug!(
                            "Routing rule redirecting from '{}' to '{}' with labels {:?}",
                            model_name, redirect_alias, labels
                        );
                        pool = match state.targets.targets.get(redirect_alias) {
                            Some(p) => p.clone(),
                            None => {
                                debug!("Redirect target '{}' not found", redirect_alias);
                                return Err(OnwardsErrorResponse::bad_gateway());
                            }
                        };
                        if pool.is_empty() {
                            debug!("Redirect target pool '{}' has no providers", redirect_alias);
                            return Err(OnwardsErrorResponse::bad_gateway());
                        }
                    }
                }
            }
        // If no bearer token, no labels to match — rules are skipped (allow by default)
    }

    let canonical_reasoning = if let Some(reasoning) = req
        .extensions()
        .get::<crate::reasoning::CanonicalReasoningRequest>()
    {
        Some(reasoning.clone())
    } else if !body_bytes.is_empty()
        && crate::reasoning::uses_reasoning_contract(&canonical_request_path)
    {
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).map_err(|_| {
            OnwardsErrorResponse::bad_request("Request body must be valid JSON.", None)
        })?;
        crate::reasoning::validate_canonical_reasoning(&canonical_request_path, &body).map_err(
            |error| OnwardsErrorResponse::reasoning(&error),
        )?
    } else {
        None
    };

    if let Some(reasoning) = canonical_reasoning.as_ref() {
        for provider in pool.providers() {
            if let Some(config) = provider.target.reasoning_translation.as_ref() {
                config
                    .validate_request(&canonical_request_path, reasoning)
                    .map_err(|error| OnwardsErrorResponse::reasoning(&error))?;
            }
        }
    }

    // Check if pool has no providers (e.g., composite model with no enabled components).
    // This runs after routing rules so that redirects get a chance to replace the pool.
    if pool.is_empty() {
        debug!("Pool for model '{}' has no providers", model_name);
        return Err(OnwardsErrorResponse::service_unavailable());
    }

    // Check pool-level rate limit before selecting a provider
    {
        let _rate_limit_span = tracing::info_span!("onwards.rate_limit_check", otel.name = "onwards.rate_limit_check", model = %model_name).entered();
        if let Some(limiter) = pool.pool_limiter()
            && limiter.check().is_err()
        {
            debug!("Pool-level rate limit exceeded for model: {}", model_name);
            record_response_status(429);
            return Err(OnwardsErrorResponse::rate_limited());
        }

        // Check per-key rate limits if bearer token is present
        if let Some(token) = bearer_token
            && let Some(limiter) = state.targets.key_rate_limiters.get(token)
            && limiter.check().is_err()
        {
            debug!("Per-key rate limit exceeded for token: {}", token);
            record_response_status(429);
            return Err(OnwardsErrorResponse::rate_limited());
        }
    }

    // Acquire pool-level concurrency permit
    let (_pool_concurrency_guard, _key_concurrency_guard) = {
        let _concurrency_span = tracing::info_span!("onwards.acquire_concurrency", otel.name = "onwards.acquire_concurrency", model = %model_name).entered();
        let pool_guard = if let Some(limiter) = pool.pool_concurrency_limiter() {
            match limiter.try_acquire() {
                Some(guard) => Some(guard),
                None => {
                    debug!(
                        "Pool-level concurrency limit exceeded for model: {}",
                        model_name
                    );
                    return Err(OnwardsErrorResponse::concurrency_limited());
                }
            }
        } else {
            None
        };

        // Acquire per-key concurrency permit
        let key_guard = if let Some(token) = bearer_token {
            if let Some(limiter) = state.targets.key_concurrency_limiters.get(token) {
                match limiter.try_acquire() {
                    Some(guard) => Some(guard),
                    None => {
                        debug!("Per-key concurrency limit exceeded for token: {}", token);
                        return Err(OnwardsErrorResponse::concurrency_limited());
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
        (pool_guard, key_guard)
    };

    // Extract path info once (used for each provider attempt)
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or(req.uri().path())
        .to_string();

    // Prepare original headers and method for potential retries
    let original_headers = req.headers().clone();
    let method = req.method().clone();

    // Track last error for fallback scenarios
    let mut last_error: Option<OnwardsErrorResponse> = None;

    // Iterate through providers (with fallback support).
    // select_iter() uses weighted least connections: picks the provider with the
    // lowest active_connections/weight ratio, skipping providers at their
    // concurrency limit. The returned guard tracks the active connection.
    let mut any_attempted = false;
    let mut attempt_number: u32 = 0;
    let mut total_backoff_ms: u64 = 0;
    let pool_max_attempts = pool.fallback_max_attempts();
    for (_idx, target, connection_guard) in pool.select_iter() {
        any_attempted = true;
        attempt_number += 1;

        let attempt_span = tracing::info_span!(
            "onwards.provider_attempt",
            otel.name = "onwards.provider_attempt",
            attempt = attempt_number,
            provider.url = %target.url,
            provider.model = target.onwards_model.as_deref().unwrap_or(""),
            provider.timeout_secs = target.request_timeout_secs,
            http.response.status_code = tracing::field::Empty,
            onwards.fallback = tracing::field::Empty,
        );

        // The loop body is wrapped in an instrumented async block so that
        // attempt_span is the "current" span for all logging / field recording,
        // without using Span::enter() which is thread-local and unsafe across
        // .await points (it leaked spans onto tokio worker threads, causing
        // ever-growing nested span chains in logs).
        //
        // Because `continue` / `return` cannot cross the async-block boundary
        // we return an enum: Continue (try next provider) or Done (propagate
        // the final result out of the for-loop).
        enum LoopAction<T> {
            Continue(Option<OnwardsErrorResponse>),
            Done(T),
        }

        // Extract original_model before the async block so we don't capture &req
        // (Request<Body> is !Sync, which would make the future !Send)
        let original_model = req
            .extensions()
            .get::<OriginalModel>()
            .map(|m| m.0.to_string());

        let action = async {

        // Check provider-level rate limit (skip to next if configured for rate limit fallback)
        if let Some(ref limiter) = target.limiter
            && limiter.check().is_err()
        {
            debug!("Provider rate limited: {:?}", target.url);
            tracing::Span::current().record("onwards.fallback", "rate_limited");
            if pool.should_fallback_on_rate_limit() {
                debug!("Fallback on rate limit enabled, trying next provider");
                return LoopAction::Continue(Some(OnwardsErrorResponse::rate_limited()));
            } else {
                return LoopAction::Done(Err(OnwardsErrorResponse::rate_limited()));
            }
        }

        // Clone response headers for later use
        let response_headers = target.response_headers.clone();

        // Prepare body for this attempt (may need model rewrite)
        let mut attempt_body = body_bytes.clone();

        // Rewrite model field if configured
        if let Some(ref rewrite) = target.onwards_model
            && !attempt_body.is_empty()
        {
            debug!("Rewriting model key to: {}", rewrite);
            let error = OnwardsErrorResponse::bad_request(
                "Could not parse onwards model from request. 'model' parameter must be supplied in either the body or in the Model-Override header.",
                Some("model"),
            );
            let mut body_serialized: serde_json::Value = match serde_json::from_slice(&attempt_body)
            {
                Ok(value) => value,
                Err(_) => return LoopAction::Done(Err(error.clone())),
            };
            let entry = match body_serialized.as_object_mut() {
                Some(obj) => obj.entry("model"),
                None => return LoopAction::Done(Err(error.clone())),
            };
            match entry {
                Entry::Occupied(mut entry) => {
                    entry.insert(serde_json::Value::String(rewrite.clone()));
                }
                Entry::Vacant(_entry) => {
                    return LoopAction::Done(Err(error.clone()));
                }
            }
            attempt_body = match serde_json::to_vec(&body_serialized) {
                Ok(bytes) => axum::body::Bytes::from(bytes),
                Err(_) => return LoopAction::Done(Err(OnwardsErrorResponse::internal())),
            };
        }

        if canonical_reasoning.is_some()
            && let Some(reasoning_translation) = target.reasoning_translation.as_ref()
            && !attempt_body.is_empty()
        {
            let mut body: serde_json::Value = match serde_json::from_slice(&attempt_body) {
                Ok(body) => body,
                Err(_) => {
                    return LoopAction::Done(Err(OnwardsErrorResponse::bad_request(
                        "Request body must be valid JSON.",
                        None,
                    )))
                }
            };
            if let Some(reasoning) = canonical_reasoning.as_ref()
                && let Err(error) = reasoning_translation.apply_request(
                    &canonical_request_path,
                    reasoning,
                    &mut body,
                )
            {
                return LoopAction::Done(Err(OnwardsErrorResponse::reasoning(&error)));
            }
            attempt_body = match serde_json::to_vec(&body) {
                Ok(bytes) => axum::body::Bytes::from(bytes),
                Err(_) => return LoopAction::Done(Err(OnwardsErrorResponse::internal())),
            };
        }

        // Build the upstream URI for this target
        let request_path = path_and_query.strip_prefix('/').unwrap_or(&path_and_query);
        let target_path = target.url.path().trim_end_matches('/');

        let path_to_join = if !target_path.is_empty() && target_path != "/" {
            let target_path_no_slash = &target_path[1..];
            if let Some(rest) = request_path.strip_prefix(target_path_no_slash) {
                if rest.is_empty() || rest.starts_with('/') {
                    rest.strip_prefix('/').unwrap_or(rest)
                } else {
                    request_path
                }
            } else {
                request_path
            }
        } else {
            request_path
        };

        let upstream_uri = match target.url.join(path_to_join) {
            Ok(url) => url.to_string(),
            Err(_) => return LoopAction::Done(Err(OnwardsErrorResponse::internal())),
        };
        let upstream_uri_parsed = match Uri::try_from(&upstream_uri) {
            Ok(uri) => uri,
            Err(_) => {
                error!("Invalid URI: {}", upstream_uri);
                return LoopAction::Done(Err(OnwardsErrorResponse::internal()));
            }
        };

        // Build request for this provider
        let mut attempt_headers = original_headers.clone();

        // Update host header
        if let Some(host) = upstream_uri_parsed.host() {
            let host_value = if let Some(port) = upstream_uri_parsed.port_u16() {
                format!("{host}:{port}")
            } else {
                host.to_string()
            };
            attempt_headers.insert("host", host_value.parse().unwrap());
        }

        // Set Content-Length and remove Transfer-Encoding
        attempt_headers.insert(
            CONTENT_LENGTH,
            attempt_body
                .len()
                .to_string()
                .parse()
                .expect("Content-Length should be valid"),
        );
        attempt_headers.remove(TRANSFER_ENCODING);

        // Filter headers for upstream forwarding
        filter_headers_for_upstream(&mut attempt_headers, target);

        // Apply W3C trace-context policy for this upstream, gated on the
        // per-target propagate_trace_context flag (defaults to the resolved
        // trusted value).
        //
        // - Enabled: inject the current span context, propagating the trace to
        //   upstreams that participate in our distributed tracing fabric
        //   (typically self-hosted, marked trusted). inject_context overwrites
        //   any inbound traceparent/tracestate.
        // - Disabled: strip any *inbound* traceparent/tracestate so they are
        //   not forwarded to an untrusted third party. These headers are not
        //   in HEADERS_TO_STRIP (they're handled here, next to the inject
        //   decision), so without this branch an inbound trace context from
        //   the caller would pass straight through, defeating the opt-out and
        //   letting the third party re-emit our trace IDs on its own outbound
        //   calls.
        if resolve_trace_propagation(target, pool.is_trusted()) {
            use tracing_opentelemetry::OpenTelemetrySpanExt;
            let ctx = tracing::Span::current().context();
            let propagator = opentelemetry_sdk::propagation::TraceContextPropagator::new();
            propagator.inject_context(&ctx, &mut HeaderInjector(&mut attempt_headers));
        } else {
            withhold_trace_context(&mut attempt_headers);
        }

        // Build the request
        let attempt_req = axum::extract::Request::builder()
            .method(method.clone())
            .uri(upstream_uri_parsed)
            .body(axum::body::Body::from(attempt_body))
            .unwrap();
        let (mut parts, body) = attempt_req.into_parts();
        parts.headers = attempt_headers;
        let attempt_req = axum::extract::Request::from_parts(parts, body);

        trace!(
            "Outgoing request to provider:\n  URI: {}",
            upstream_uri
        );

        // Make the request with optional timeout
        // Note: Timeout only applies to receiving response headers, not reading the full body.
        // For streaming responses, the body may take much longer than the timeout.
        let upstream_span = tracing::info_span!(
            "onwards.upstream_request",
            otel.name = "onwards.upstream_request",
            otel.kind = "Client",
            http.request.method = %method,
            url.full = %upstream_uri,
            http.response.status_code = tracing::field::Empty,
        );
        let request_result = async {
            if let Some(timeout_secs) = target.request_timeout_secs {
                let timeout_duration = std::time::Duration::from_secs(timeout_secs);
                match tokio::time::timeout(timeout_duration, state.http_client.request(attempt_req))
                    .await
                {
                    Err(_) => {
                        // Timeout occurred
                        debug!(
                            "Request to {} timed out after {:?}",
                            upstream_uri, timeout_duration
                        );
                        Err(UpstreamOutcome::Timeout)
                    }
                    Ok(result) => result.map_err(UpstreamOutcome::Error),
                }
            } else {
                // No timeout configured
                state.http_client.request(attempt_req).await.map_err(UpstreamOutcome::Error)
            }
        }
        .instrument(upstream_span.clone())
        .await;

        // Handle request errors
        let mut response = match request_result {
            Err(UpstreamOutcome::Timeout) => {
                upstream_span.record("http.response.status_code", 504_u16);
                tracing::Span::current().record("onwards.fallback", "timeout");
                if pool.fallback_enabled() {
                    return LoopAction::Continue(Some(OnwardsErrorResponse::gateway_timeout()));
                } else {
                    record_response_status(504);
                    return LoopAction::Done(Err(OnwardsErrorResponse::gateway_timeout()));
                }
            }
            Err(UpstreamOutcome::Error(e)) => {
                error!(
                    "Error forwarding request to target url {}: {}",
                    upstream_uri, e
                );
                tracing::Span::current().record("onwards.fallback", "network_error");
                // Only continue to next provider if fallback is enabled
                if pool.fallback_enabled() {
                    return LoopAction::Continue(Some(OnwardsErrorResponse::bad_gateway()));
                } else {
                    return LoopAction::Done(Err(OnwardsErrorResponse::bad_gateway()));
                }
            }
            Ok(response) => response,
        };

        let status = response.status().as_u16();
        upstream_span.record("http.response.status_code", status);
        tracing::Span::current().record("http.response.status_code", status);

        // Check if we should fallback based on status code
        if pool.should_fallback_on_status(status) {
            debug!(
                "Provider returned fallback status {}, trying next: {:?}",
                status, target.url
            );
            tracing::Span::current().record("onwards.fallback", "status_fallback");
            return LoopAction::Continue(Some(OnwardsErrorResponse::bad_gateway()));
        }

        // Sanitize error responses when sanitize_response is enabled.
        // Replace upstream error bodies with generic messages to prevent
        // information leakage (provider names, URLs, internal model names).
        if target.sanitize_response && !(200..300).contains(&status) {
            // Drain the original error body (buffering up to 64 KiB) so we can
            // record its length, but do NOT log its content. ZDR: provider error
            // bodies can echo prompt/response content, so only the status,
            // upstream, and buffered length are safe. `body_len` below is that
            // buffered size — 0 if the read errored or the body exceeded 64 KiB.
            let error_body = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .ok();

            error!(
                status = status,
                upstream = %target.url,
                body_len = error_body.as_ref().map(|b| b.len()).unwrap_or(0),
                "Upstream provider returned error, sanitizing before forwarding to client"
            );

            let sanitized_error = if (400..500).contains(&status) {
                OnwardsErrorResponse::builder()
                    .body(ErrorResponseBody {
                        message: "The upstream provider rejected the request.".to_string(),
                        r#type: "invalid_request_error".to_string(),
                        param: None,
                        code: "upstream_error".to_string(),
                    })
                    .status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST))
                    .build()
            } else {
                OnwardsErrorResponse::builder()
                    .body(ErrorResponseBody {
                        message: "An internal error occurred. Please try again later.".to_string(),
                        r#type: "internal_error".to_string(),
                        param: None,
                        code: "internal_error".to_string(),
                    })
                    .status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY))
                    .build()
            };

            record_response_status(status);
            return LoopAction::Done(Err(sanitized_error));
        }

        // Check content type for SSE handling
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let is_sse = content_type.contains("text/event-stream");

        // Some upstreams return HTTP 200 while embedding the
        // real error in the body — `{"error":{"code":429,...}}` for a unary
        // response, or as the first `data:` frame of an SSE stream — instead of
        // using the status line. The status-based fallback (`should_fallback_on_status`
        // above) and the rate-limit retry machinery only ever see the `200`, so a
        // retryable upstream error (e.g. a 429 rate limit) is silently bypassed and
        // a transient throttle surfaces as a user-visible failure.
        //
        // Detect such an envelope and feed its embedded status into the SAME
        // fallback/retry decision as a real HTTP status. Only 2xx responses are
        // scanned — a non-2xx already carries a correct status and is handled by
        // the status-based fallback above.
        //
        // Gated to strict_mode: the only mode where the body is *already* buffered
        // (unary) or routed through `SseBufferedStream` (streaming) by the strict
        // sanitizer downstream. So this adds no new buffering and no change to
        // streaming behaviour — it just inspects the body a little earlier so a
        // retry stays possible. Pure-passthrough / sanitize-without-transform
        // targets are deliberately left untouched, to avoid forcing buffering (and
        // SSE re-framing) onto streams that would otherwise pass straight through.
        // The motivating upstreams run in strict mode.
        // Classification of a 2xx response that should be treated as an upstream
        // failure and fed into the retry loop. `Clean` forwards `response` as-is.
        enum Scan2xx {
            Clean,
            /// Provider embedded an error status in the 2xx body
            /// (`{"error":{"code":N}}`).
            Embedded(u16),
            /// 2xx whose body *terminated* empty before any content frame — e.g.
            /// an upstream `200 OK` with an empty body. Treated as a retryable
            /// 502. Keyed on stream termination, never a time budget, so a
            /// valid-but-slow stream is forwarded (Clean), not retried.
            EmptyBody,
        }
        let scan: Scan2xx = if (200..300).contains(&status)
            && state.targets.strict_mode
        {
            if is_sse {
                // Peek the leading SSE events to find the first *real* frame,
                // skipping any keep-alive/comment frames a provider may send
                // first. Such providers put the error in that first frame (before
                // any token), so this is enough to tell an error stream from a normal
                // one. Holding the `200` headers until the first frame costs
                // nothing in time-to-first-*token* (onwards must receive it to
                // forward it anyway). The whole peek is bounded by a short fixed
                // budget — NOT the request timeout, which governs header receipt —
                // so a `200`-then-idle stream isn't withheld from the client; on
                // timeout we forward whatever we have, unmodified (no retry).
                use futures_util::StreamExt;
                const SSE_PEEK_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
                const SSE_PEEK_MAX_EVENTS: usize = 4;

                let (parts, body) = response.into_parts();
                let mut events = SseBufferedStream::new(body.into_data_stream());
                // `peeked` / `embedded` live outside the peek future so a partial
                // peek survives a budget timeout (consumed frames are not lost).
                let mut peeked = Vec::new();
                let mut embedded = None;
                // `saw_data` = a real content frame arrived (definitely not empty).
                // `stream_ended` = the stream closed (`None`) or errored (`Err`)
                // before any content — a *terminal* empty, safe to retry.
                let mut saw_data = false;
                let mut stream_ended = false;
                let peek = async {
                    for _ in 0..SSE_PEEK_MAX_EVENTS {
                        match events.next().await {
                            Some(Ok(chunk)) => match classify_sse_event(&chunk) {
                                SseEventKind::Error(status) => {
                                    embedded = Some(status);
                                    peeked.push(Ok(chunk));
                                    break;
                                }
                                // First real data frame is normal content — stop.
                                SseEventKind::Data => {
                                    saw_data = true;
                                    peeked.push(Ok(chunk));
                                    break;
                                }
                                // Keep-alive / comment — keep looking.
                                SseEventKind::Comment => peeked.push(Ok(chunk)),
                            },
                            Some(Err(e)) => {
                                stream_ended = true;
                                peeked.push(Err(e));
                                break;
                            }
                            None => {
                                stream_ended = true;
                                break;
                            }
                        }
                    }
                };
                let timed_out = tokio::time::timeout(SSE_PEEK_BUDGET, peek).await.is_err();
                if timed_out {
                    debug!("Timed out peeking SSE lead frames; forwarding stream unmodified");
                }
                // Forward the consumed frames followed by the remainder, so a clean
                // (or slow) stream is intact; on a retry path below this `response`
                // is dropped when we return.
                let rest = futures_util::stream::iter(peeked).chain(events);
                response = Response::from_parts(parts, axum::body::Body::from_stream(rest));
                if let Some(status) = embedded {
                    Scan2xx::Embedded(status)
                } else if !timed_out && stream_ended && !saw_data {
                    // Stream closed/errored before any content frame: nothing was
                    // forwarded, so retrying is safe. A *timeout* (stream still open,
                    // just slow to first token) deliberately falls through to `Clean`
                    // — that's the valid-but-slow case we must never retry.
                    Scan2xx::EmptyBody
                } else {
                    Scan2xx::Clean
                }
            } else {
                let (parts, body) = response.into_parts();
                let buffered = match axum::body::to_bytes(body, usize::MAX).await {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        error!("Failed to buffer response body for embedded-error scan: {}", e);
                        return LoopAction::Done(Err(OnwardsErrorResponse::internal()));
                    }
                };
                // A 2xx whose body terminated empty is the unary form of the same
                // upstream failure (`bytes_read: 0`, deserialize EOF downstream).
                // `to_bytes` already awaited end-of-body, so empty is terminal —
                // there is no "not yet" — and nothing was forwarded, so retry.
                if buffered.is_empty() {
                    response = Response::from_parts(parts, axum::body::Body::empty());
                    Scan2xx::EmptyBody
                } else {
                    // Cheap guard: only parse when an `error` key could be present, so
                    // normal completions/embeddings aren't deserialized an extra time.
                    const ERROR_KEY: &[u8] = b"\"error\"";
                    let embedded = if buffered.windows(ERROR_KEY.len()).any(|w| w == ERROR_KEY) {
                        serde_json::from_slice::<serde_json::Value>(&buffered)
                            .ok()
                            .as_ref()
                            .and_then(embedded_error_status)
                    } else {
                        None
                    };
                    // Re-attach the buffered body so downstream sees an intact response.
                    let len = buffered.len();
                    response = Response::from_parts(parts, axum::body::Body::from(buffered));
                    response.headers_mut().remove(TRANSFER_ENCODING);
                    response
                        .headers_mut()
                        .insert(CONTENT_LENGTH, HeaderValue::from(len));
                    match embedded {
                        Some(status) => Scan2xx::Embedded(status),
                        None => Scan2xx::Clean,
                    }
                }
            }
        } else {
            Scan2xx::Clean
        };

        match scan {
            Scan2xx::Embedded(embedded) => {
                warn!(
                    http_status = status,
                    embedded_status = embedded,
                    upstream = %target.url,
                    "Upstream returned 2xx with an embedded provider error; treating it as status {embedded}"
                );
                upstream_span.record("http.response.status_code", embedded);
                tracing::Span::current().record("http.response.status_code", embedded);

                let retryable = pool.should_fallback_on_status(embedded)
                    || (embedded == 429 && pool.should_fallback_on_rate_limit());
                if retryable {
                    tracing::Span::current().record("onwards.fallback", "embedded_error");
                    // Retry internally. If every attempt / provider fallback is
                    // exhausted the caller gets a sanitized 503 — never the
                    // upstream's rate limit (see the non-retryable arm below).
                    return LoopAction::Continue(Some(OnwardsErrorResponse::service_unavailable()));
                }

                record_response_status(embedded);
                // Don't leak an upstream rate limit: a 429 — and *any* 5xx, including
                // non-retryable ones like 501/505 — collapses to a generic 503. This is
                // deliberately more opaque than the non-embedded error path: a
                // 200-with-error body is already anomalous, so we hide the specifics.
                // Genuine client errors (other 4xx) are surfaced, sanitized, so the
                // caller can fix the request.
                let err = if embedded == 429 || embedded >= 500 {
                    OnwardsErrorResponse::service_unavailable()
                } else {
                    OnwardsErrorResponse::builder()
                        .body(ErrorResponseBody {
                            message: "The upstream provider rejected the request.".to_string(),
                            r#type: "invalid_request_error".to_string(),
                            param: None,
                            code: "upstream_error".to_string(),
                        })
                        .status(StatusCode::from_u16(embedded).unwrap_or(StatusCode::BAD_REQUEST))
                        .build()
                };
                return LoopAction::Done(Err(err));
            }
            Scan2xx::EmptyBody => {
                warn!(
                    http_status = status,
                    upstream = %target.url,
                    "Upstream returned 2xx with an empty body before any content; treating it as 502"
                );
                upstream_span.record("http.response.status_code", 502_u16);
                tracing::Span::current().record("http.response.status_code", 502_u16);

                if pool.should_fallback_on_status(502) {
                    tracing::Span::current().record("onwards.fallback", "empty_body");
                    // Retry internally; on exhaustion the loop's final-error handling
                    // surfaces the sanitized 503 carried below.
                    return LoopAction::Continue(Some(OnwardsErrorResponse::service_unavailable()));
                }

                // Not configured to retry 502s: collapse the anomalous empty 200 to a
                // generic 503 rather than forwarding a bodyless success to the client.
                record_response_status(503);
                return LoopAction::Done(Err(OnwardsErrorResponse::service_unavailable()));
            }
            Scan2xx::Clean => {}
        }

        // Determine if SSE buffering is needed for non-strict sanitization
        // Note: Strict mode handlers apply their own buffering before their sanitizers,
        // so we skip buffering here to avoid double-wrapping
        let needs_sse_buffering = !state.targets.strict_mode
            && state.response_transform_fn.is_some()
            && target.sanitize_response
            && (200..300).contains(&status);

        // Wrap SSE streams with buffering to ensure complete events (delimited by \n\n).
        // This prevents incomplete JSON from reaching sanitization logic.
        // Providers may send partial chunks that split events across network packets.
        if is_sse && needs_sse_buffering {
            debug!("Wrapping SSE response with buffered stream for non-strict sanitization");
            let (parts, body) = response.into_parts();
            let byte_stream = body.into_data_stream();
            let buffered = SseBufferedStream::new(byte_stream);
            let new_body = axum::body::Body::from_stream(buffered);
            response = Response::from_parts(parts, new_body);
        }

        // Apply response transformation if configured
        // Per-target opt-in via sanitize_response flag, only for 2xx responses
        // Skip if strict mode is enabled - strict handlers do their own sanitization
        if let Some(ref transform_fn) = state.response_transform_fn
            && target.sanitize_response
            && (200..300).contains(&status)
            && !state.targets.strict_mode
        {
            debug!(
                "Attempting response sanitization for status {}, path {}",
                status, path_and_query
            );

            // original_model extracted before the async block (to avoid capturing &req)

            if is_sse {
                // Streaming SSE response - transform chunk-by-chunk
                // Note: stream is already buffered above
                debug!("Applying streaming sanitization");

                let sanitizer = crate::response_sanitizer::ResponseSanitizer {
                    original_model: original_model.clone(),
                };

                use futures_util::StreamExt;

                let body_stream =
                    http_body_util::BodyExt::into_data_stream(std::mem::take(response.body_mut()));

                let transformed_stream = body_stream.map(move |chunk_result| {
                    match chunk_result {
                        Ok(chunk) => {
                            // Sanitize this chunk
                            match sanitizer.sanitize_streaming(&chunk) {
                                Ok(Some(sanitized)) => Ok::<_, std::io::Error>(sanitized),
                                Ok(None) => Ok(chunk),
                                Err(e) => {
                                    tracing::error!("Failed to sanitize streaming chunk: {}", e);
                                    Ok(chunk) // Pass through on error
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Stream error: {}", e);
                            Err(std::io::Error::other(e))
                        }
                    }
                });

                *response.body_mut() = axum::body::Body::from_stream(transformed_stream);
            } else {
                // Non-streaming response - buffer and transform
                debug!("Applying non-streaming sanitization");

                let response_body = match
                    axum::body::to_bytes(std::mem::take(response.body_mut()), usize::MAX)
                        .await
                {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        error!("Failed to buffer response body: {}", e);
                        return LoopAction::Done(Err(OnwardsErrorResponse::internal()));
                    }
                };

                // ZDR: log only the buffered length, never the response body content.
                debug!(
                    "Response body buffered: {} bytes, content-type: {}",
                    response_body.len(),
                    content_type
                );

                // Apply transformation
                match transform_fn(
                    &path_and_query,
                    response.headers(),
                    &response_body,
                    original_model.as_deref(),
                ) {
                    Ok(Some(transformed_body)) => {
                        // Update response with sanitized body
                        let content_length = transformed_body.len();
                        // ZDR: log only the lengths, never the sanitized body content.
                        debug!(
                            "Sanitization successful: {} bytes -> {} bytes",
                            response_body.len(),
                            content_length
                        );
                        *response.body_mut() = axum::body::Body::from(transformed_body);

                        // Remove transfer-encoding since we're setting content-length
                        response.headers_mut().remove(TRANSFER_ENCODING);
                        response
                            .headers_mut()
                            .insert(CONTENT_LENGTH, HeaderValue::from(content_length));
                    }
                    Ok(None) => {
                        // No transformation applied, restore original body
                        debug!(
                            "Sanitization returned None, restoring original {} bytes",
                            response_body.len()
                        );
                        let content_length = response_body.len();
                        *response.body_mut() = axum::body::Body::from(response_body);

                        // Ensure proper headers even when not transforming
                        response.headers_mut().remove(TRANSFER_ENCODING);
                        response
                            .headers_mut()
                            .insert(CONTENT_LENGTH, HeaderValue::from(content_length));
                    }
                    Err(e) => {
                        error!("Response sanitization failed: {}", e);
                        return LoopAction::Done(Err(OnwardsErrorResponse::internal()));
                    }
                }
            }
        }

        // Override the response `id` field for /responses and /chat/completions
        // requests when the caller supplied a response ID via the configured
        // header. Both response bodies expose a top-level `id` we can rewrite.
        if let Some(ref header_name) = state.response_id_header
            && crate::response_id::path_supports_id_override(&path_and_query)
            && (200..300).contains(&status)
        {
            if let Some(override_id) =
                crate::response_id::extract_override_id(&original_headers, header_name)
            {
                crate::response_id::patch_response_body_id(&mut response, override_id).await;
            }
        }

        // Add custom response headers
        if let Some(headers) = response_headers {
            for (key, value) in headers.iter() {
                if let (Ok(header_name), Ok(header_value)) =
                    (key.parse::<HeaderName>(), value.parse::<HeaderValue>())
                {
                    response.headers_mut().insert(header_name, header_value);
                }
            }
            trace!(
                model = %model_name,
                headers = ?headers,
                "Added custom response headers"
            );
        }

        record_response_status(response.status().as_u16());
        debug!(
            "Returning response with status {}, content-length: {:?}, strict_mode: {}",
            response.status(),
            response.headers().get(CONTENT_LENGTH),
            state.targets.strict_mode
        );
        let resolved_trust = target.trusted.unwrap_or_else(|| pool.is_trusted());
        response
            .extensions_mut()
            .insert(ResolvedTrust(resolved_trust));
        response.extensions_mut().insert(ServedBy {
            url: target.url.to_string(),
            onwards_model: target.onwards_model.clone(),
        });

        // Attach the connection guard and inflight guard to the response body so both
        // are decremented when the body stream completes, not when the handler returns.
        // Critical for streaming responses where the body outlives the handler.
        let (parts, body) = response.into_parts();
        let guarded = GuardedStream {
            inner: body.into_data_stream(),
            _guard: connection_guard,
            _inflight_guard: inflight_guard.take().expect("inflight_guard taken once on success path"),
        };
        let response = Response::from_parts(parts, axum::body::Body::from_stream(guarded));

        LoopAction::Done(Ok(response))

        }.instrument(attempt_span).await;

        match action {
            LoopAction::Continue(err) => {
                last_error = err;
                // Sleep here — *after* the current connection_guard has gone
                // out of scope but *before* select_iter().next() grabs the
                // next one — so we don't pin a concurrency slot while waiting.
                // Skip if this was the last attempt the iterator would yield.
                if (attempt_number as usize) < pool_max_attempts
                    && let Some(backoff) = pool.fallback().and_then(|f| f.backoff.as_ref())
                {
                    // `attempt_number` is the count of the attempt that just
                    // failed (1-indexed), which matches `BackoffConfig::delay`'s
                    // retry-index contract: `delay(N)` is the wait that goes
                    // *before* attempt N+1. Bind it locally so the relationship
                    // is obvious if the loop counter is ever refactored.
                    let retry_index = attempt_number;
                    let delay = backoff.delay(retry_index);
                    let delay_ms = delay.as_millis() as u64;
                    let next_total = total_backoff_ms.saturating_add(delay_ms);
                    if let Some(max_total) = pool.fallback().and_then(|f| f.max_total_backoff_ms)
                        && next_total > max_total
                    {
                        debug!(
                            used_ms = total_backoff_ms,
                            next_ms = delay_ms,
                            cap_ms = max_total,
                            "Retry backoff budget exhausted, giving up"
                        );
                        metrics::counter!("onwards_retry_budget_exhausted_total").increment(1);
                        break;
                    }
                    total_backoff_ms = next_total;
                    metrics::counter!("onwards_retries_total").increment(1);
                    debug!(
                        retry = retry_index,
                        delay_ms, "Backing off before next provider attempt"
                    );
                    tokio::time::sleep(delay).await;
                }
                continue;
            }
            LoopAction::Done(result) => return result,
        }
    }

    // All providers exhausted — distinguish "no providers found" from
    // "all providers at concurrency capacity"
    if any_attempted {
        // We tried at least one provider but all failed
        let final_error = last_error
            .unwrap_or_else(|| OnwardsErrorResponse::model_not_found(model_name.as_str()));
        record_response_status(final_error.status.as_u16());
        Err(final_error)
    } else if !pool.is_empty() {
        // Pool has providers but select_iter() yielded nothing — all at capacity
        record_response_status(429);
        Err(OnwardsErrorResponse::concurrency_limited())
    } else {
        // Empty pool (shouldn't normally happen, targets resolved earlier)
        let err = OnwardsErrorResponse::model_not_found(model_name.as_str());
        record_response_status(err.status.as_u16());
        Err(err)
    }
    }
    .instrument(span)
    .await
}

#[instrument(skip(state, req))]
pub async fn models<T: HttpClient>(
    State(state): State<AppState<T>>,
    req: Request,
) -> impl IntoResponse {
    // Extract bearer token from Authorization header
    let bearer_token = req
        .headers()
        .get("authorization")
        .and_then(|auth_header| auth_header.to_str().ok())
        .and_then(|auth_value| auth_value.strip_prefix("Bearer "));

    // Filter targets based on bearer token permissions using pool-level keys
    let accessible_models: Vec<String> = state
        .targets
        .targets
        .iter()
        .filter(|entry| {
            let pool = entry.value();

            // If pool has no keys configured, it's publicly accessible
            let Some(keys) = pool.keys() else {
                return true;
            };

            // If pool has keys but no bearer token provided, deny access
            let Some(token) = bearer_token else {
                return false;
            };

            // Validate bearer token against pool's keys
            auth::validate_bearer_token(keys, token)
        })
        .map(|entry| entry.key().clone())
        .collect();

    // Create filtered response
    Json(ListModelResponse::from_model_names(&accessible_models))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_error_status_detects_provider_envelope() {
        // Whole-body error envelope (the 200-with-error pattern).
        let body =
            serde_json::json!({"error": {"code": 429, "message": "Provider returned error"}});
        assert_eq!(embedded_error_status(&body), Some(429));

        // Error alongside otherwise-valid fields (reassembled-stream shape).
        let body = serde_json::json!({
            "id": "gen-x", "object": "chat.completion.chunk", "choices": [],
            "error": {"code": 503, "message": "upstream"}
        });
        assert_eq!(embedded_error_status(&body), Some(503));

        // Stringified numeric code.
        assert_eq!(
            embedded_error_status(&serde_json::json!({"error": {"code": "502"}})),
            Some(502)
        );
    }

    #[test]
    fn embedded_error_status_ignores_normal_and_ambiguous_bodies() {
        // A well-formed completion must never be disturbed.
        let ok = serde_json::json!({
            "id": "chatcmpl-1", "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}}]
        });
        assert_eq!(embedded_error_status(&ok), None);

        // No code / null code / non-numeric (string) code / out-of-range code.
        assert_eq!(
            embedded_error_status(&serde_json::json!({"error": {"message": "x"}})),
            None
        );
        assert_eq!(
            embedded_error_status(&serde_json::json!({"error": {"code": null}})),
            None
        );
        assert_eq!(
            embedded_error_status(&serde_json::json!({"error": {"code": "rate_limit_exceeded"}})),
            None
        );
        assert_eq!(
            embedded_error_status(&serde_json::json!({"error": {"code": 200}})),
            None
        );
        assert_eq!(
            embedded_error_status(&serde_json::json!({"error": {"code": 999}})),
            None
        );
    }

    #[test]
    fn classify_sse_event_distinguishes_error_data_and_comment() {
        use SseEventKind::{Comment, Data, Error};

        // A provider's first-frame error on a 200 stream.
        assert_eq!(
            classify_sse_event(b"data: {\"error\":{\"code\":429,\"message\":\"x\"}}\n\n"),
            Error(429)
        );
        // Error alongside otherwise-valid chunk fields (provider shape).
        assert_eq!(
            classify_sse_event(b"data: {\"id\":\"g\",\"choices\":[],\"error\":{\"code\":502}}\n\n"),
            Error(502)
        );
        // Tolerant of `data:` with no space after the colon.
        assert_eq!(
            classify_sse_event(b"data:{\"error\":{\"code\":503}}\n\n"),
            Error(503)
        );
        // Multi-line `data:` fields are concatenated with `\n` before parsing
        // (SSE spec): the two `data:` lines here join to the valid JSON
        // `{"error":{"code":429}}` (the embedded `\n` is JSON whitespace).
        assert_eq!(
            classify_sse_event(b"data: {\"error\":\ndata: {\"code\":429}}\n\n"),
            Error(429)
        );

        // Normal content and the [DONE] sentinel are data frames, not errors.
        assert_eq!(
            classify_sse_event(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n"),
            Data
        );
        assert_eq!(classify_sse_event(b"data: [DONE]\n\n"), Data);

        // Comment / keep-alive frames carry no `data:` field.
        assert_eq!(classify_sse_event(b": keep-alive\n\n"), Comment);
        assert_eq!(classify_sse_event(b": ping\n\n"), Comment);

        // Non-UTF-8 is treated as opaque data (forwarded, never flagged).
        assert_eq!(classify_sse_event(&[0xFF, 0xFE]), Data);
    }

    #[test]
    fn test_filter_headers_strips_hop_by_hop_headers() {
        let mut headers = HeaderMap::new();

        // Add hop-by-hop headers that should be removed
        headers.insert("connection", "keep-alive".parse().unwrap());
        headers.insert("keep-alive", "timeout=5".parse().unwrap());
        headers.insert("proxy-authenticate", "Basic".parse().unwrap());
        headers.insert("proxy-authorization", "Basic abc123".parse().unwrap());
        headers.insert("te", "trailers".parse().unwrap());
        headers.insert("trailer", "Expires".parse().unwrap());
        headers.insert("upgrade", "HTTP/2.0".parse().unwrap());

        // Add a safe header that should be kept
        headers.insert("content-type", "application/json".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Verify hop-by-hop headers were removed
        assert!(!headers.contains_key("connection"));
        assert!(!headers.contains_key("keep-alive"));
        assert!(!headers.contains_key("proxy-authenticate"));
        assert!(!headers.contains_key("proxy-authorization"));
        assert!(!headers.contains_key("te"));
        assert!(!headers.contains_key("trailer"));
        assert!(!headers.contains_key("upgrade"));

        // Verify safe header was kept
        assert!(headers.contains_key("content-type"));
    }

    #[test]
    fn test_filter_headers_strips_auth_headers() {
        let mut headers = HeaderMap::new();

        // Add auth headers that should be removed
        headers.insert("authorization", "Bearer client-token".parse().unwrap());
        headers.insert("x-api-key", "client-api-key".parse().unwrap());
        headers.insert("api-key", "another-key".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Verify all auth headers were removed (client credentials stripped)
        assert!(!headers.contains_key("authorization"));
        assert!(!headers.contains_key("x-api-key"));
        assert!(!headers.contains_key("api-key"));
    }

    #[test]
    fn test_filter_headers_strips_browser_security_headers() {
        let mut headers = HeaderMap::new();

        // Add browser security headers
        headers.insert("sec-fetch-site", "cross-site".parse().unwrap());
        headers.insert("sec-fetch-mode", "cors".parse().unwrap());
        headers.insert("sec-fetch-dest", "empty".parse().unwrap());
        headers.insert("sec-fetch-user", "?1".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Verify all sec-fetch-* headers were removed
        assert!(!headers.contains_key("sec-fetch-site"));
        assert!(!headers.contains_key("sec-fetch-mode"));
        assert!(!headers.contains_key("sec-fetch-dest"));
        assert!(!headers.contains_key("sec-fetch-user"));
    }

    #[test]
    fn test_filter_headers_strips_all_sec_ch_ua_variants() {
        let mut headers = HeaderMap::new();

        // Add various sec-ch-ua headers
        headers.insert("sec-ch-ua", "\"Chrome\";v=\"120\"".parse().unwrap());
        headers.insert("sec-ch-ua-mobile", "?0".parse().unwrap());
        headers.insert("sec-ch-ua-platform", "\"macOS\"".parse().unwrap());
        headers.insert("sec-ch-ua-arch", "\"arm64\"".parse().unwrap());
        headers.insert("sec-ch-ua-model", "\"\"".parse().unwrap());

        // Add a safe header
        headers.insert("user-agent", "Mozilla/5.0...".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Verify all sec-ch-ua* headers were removed
        assert!(!headers.contains_key("sec-ch-ua"));
        assert!(!headers.contains_key("sec-ch-ua-mobile"));
        assert!(!headers.contains_key("sec-ch-ua-platform"));
        assert!(!headers.contains_key("sec-ch-ua-arch"));
        assert!(!headers.contains_key("sec-ch-ua-model"));

        // Verify user-agent was kept
        assert!(headers.contains_key("user-agent"));
    }

    #[test]
    fn test_filter_headers_strips_browser_context_headers() {
        let mut headers = HeaderMap::new();

        headers.insert("origin", "http://localhost:3000".parse().unwrap());
        headers.insert("referer", "http://localhost:3000/chat".parse().unwrap());
        headers.insert("cookie", "session=abc123".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        assert!(!headers.contains_key("origin"));
        assert!(!headers.contains_key("referer"));
        assert!(!headers.contains_key("cookie"));
    }

    #[test]
    fn test_filter_headers_strips_caching_headers() {
        let mut headers = HeaderMap::new();

        headers.insert(
            "if-modified-since",
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        headers.insert("if-none-match", "\"abc123\"".parse().unwrap());
        headers.insert("if-match", "\"xyz789\"".parse().unwrap());
        headers.insert(
            "if-unmodified-since",
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        headers.insert("if-range", "\"abc123\"".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        assert!(!headers.contains_key("if-modified-since"));
        assert!(!headers.contains_key("if-none-match"));
        assert!(!headers.contains_key("if-match"));
        assert!(!headers.contains_key("if-unmodified-since"));
        assert!(!headers.contains_key("if-range"));
    }

    #[test]
    fn test_filter_headers_strips_model_override_header() {
        let mut headers = HeaderMap::new();

        headers.insert("model-override", "gpt-4".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // model-override should be removed (already consumed for routing)
        assert!(!headers.contains_key("model-override"));
        // content-type should be kept
        assert!(headers.contains_key("content-type"));
    }

    #[test]
    fn test_filter_headers_keeps_safe_headers() {
        let mut headers = HeaderMap::new();

        // Add headers that should be kept
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("accept", "application/json".parse().unwrap());
        headers.insert("user-agent", "MyClient/1.0".parse().unwrap());
        headers.insert("accept-language", "en-US,en;q=0.9".parse().unwrap());
        headers.insert("accept-encoding", "gzip, deflate, br".parse().unwrap());
        headers.insert("x-stainless-lang", "js".parse().unwrap());
        headers.insert("x-stainless-os", "macOS".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // All these headers should be kept
        assert!(headers.contains_key("content-type"));
        assert!(headers.contains_key("accept"));
        assert!(headers.contains_key("user-agent"));
        assert!(headers.contains_key("accept-language"));
        assert!(headers.contains_key("accept-encoding"));
        assert!(headers.contains_key("x-stainless-lang"));
        assert!(headers.contains_key("x-stainless-os"));
    }

    #[test]
    fn test_filter_headers_adds_authorization_when_onwards_key_present() {
        let mut headers = HeaderMap::new();

        // Add client authorization that should be stripped
        headers.insert("authorization", "Bearer client-token".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .onwards_key("sk-upstream-key".to_string())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Client auth should be removed and replaced with onwards_key
        assert!(headers.contains_key("authorization"));
        assert_eq!(
            headers.get("authorization").unwrap().to_str().unwrap(),
            "Bearer sk-upstream-key"
        );
    }

    #[test]
    fn test_filter_headers_no_authorization_when_onwards_key_absent() {
        let mut headers = HeaderMap::new();

        // Add client authorization
        headers.insert("authorization", "Bearer client-token".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            // No onwards_key configured
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Client auth should be removed and NOT replaced (no onwards_key)
        assert!(!headers.contains_key("authorization"));
    }

    #[test]
    fn test_filter_headers_custom_auth_header_name() {
        let mut headers = HeaderMap::new();

        // Add client authorization that should be stripped
        headers.insert("authorization", "Bearer client-token".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .onwards_key("my-api-key-123".to_string())
            .upstream_auth_header_name("X-API-Key".to_string())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Client auth should be removed
        assert!(!headers.contains_key("authorization"));

        // Custom header should be added with default Bearer prefix
        assert!(headers.contains_key("x-api-key"));
        assert_eq!(
            headers.get("x-api-key").unwrap().to_str().unwrap(),
            "Bearer my-api-key-123"
        );
    }

    #[test]
    fn test_filter_headers_custom_auth_header_prefix() {
        let mut headers = HeaderMap::new();

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .onwards_key("token-xyz".to_string())
            .upstream_auth_header_prefix("ApiKey ".to_string())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Should use custom prefix with default Authorization header
        assert!(headers.contains_key("authorization"));
        assert_eq!(
            headers.get("authorization").unwrap().to_str().unwrap(),
            "ApiKey token-xyz"
        );
    }

    #[test]
    fn test_filter_headers_empty_auth_header_prefix() {
        let mut headers = HeaderMap::new();

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .onwards_key("plain-api-key-456".to_string())
            .upstream_auth_header_prefix("".to_string())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Should use empty prefix (just the key value)
        assert!(headers.contains_key("authorization"));
        assert_eq!(
            headers.get("authorization").unwrap().to_str().unwrap(),
            "plain-api-key-456"
        );
    }

    #[test]
    fn test_filter_headers_custom_header_name_and_prefix() {
        let mut headers = HeaderMap::new();

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .onwards_key("secret-key".to_string())
            .upstream_auth_header_name("X-Custom-Auth".to_string())
            .upstream_auth_header_prefix("Token ".to_string())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Should use both custom header name and custom prefix
        assert!(!headers.contains_key("authorization"));
        assert!(headers.contains_key("x-custom-auth"));
        assert_eq!(
            headers.get("x-custom-auth").unwrap().to_str().unwrap(),
            "Token secret-key"
        );
    }

    #[test]
    fn test_filter_headers_adds_x_forwarded_proto() {
        let mut headers = HeaderMap::new();

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        assert!(headers.contains_key("x-forwarded-proto"));
        assert_eq!(
            headers.get("x-forwarded-proto").unwrap().to_str().unwrap(),
            "https"
        );
    }

    #[test]
    fn test_filter_headers_comprehensive_browser_request() {
        // Simulate a real browser request with all the problematic headers
        let mut headers = HeaderMap::new();

        // Hop-by-hop
        headers.insert("connection", "keep-alive".parse().unwrap());

        // Auth (should be stripped)
        headers.insert("authorization", "Bearer client-secret".parse().unwrap());

        // Browser security
        headers.insert("sec-fetch-site", "same-origin".parse().unwrap());
        headers.insert("sec-fetch-mode", "cors".parse().unwrap());
        headers.insert("sec-fetch-dest", "empty".parse().unwrap());
        headers.insert("sec-ch-ua", "\"Chrome\";v=\"120\"".parse().unwrap());
        headers.insert("sec-ch-ua-mobile", "?0".parse().unwrap());
        headers.insert("sec-ch-ua-platform", "\"macOS\"".parse().unwrap());

        // Browser context
        headers.insert("origin", "http://localhost:5173".parse().unwrap());
        headers.insert(
            "referer",
            "http://localhost:5173/playground".parse().unwrap(),
        );
        headers.insert("cookie", "session=xyz; token=abc".parse().unwrap());

        // Caching
        headers.insert("if-none-match", "\"abc123\"".parse().unwrap());

        // Safe headers that should be kept
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("accept", "application/json".parse().unwrap());
        headers.insert("user-agent", "Mozilla/5.0...".parse().unwrap());

        let target = Target::builder()
            .url("https://api.anthropic.com".parse().unwrap())
            .onwards_key("sk-ant-upstream-key".to_string())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Verify all problematic headers were removed
        assert!(!headers.contains_key("connection"));
        assert!(!headers.contains_key("sec-fetch-site"));
        assert!(!headers.contains_key("sec-fetch-mode"));
        assert!(!headers.contains_key("sec-fetch-dest"));
        assert!(!headers.contains_key("sec-ch-ua"));
        assert!(!headers.contains_key("sec-ch-ua-mobile"));
        assert!(!headers.contains_key("sec-ch-ua-platform"));
        assert!(!headers.contains_key("origin"));
        assert!(!headers.contains_key("referer"));
        assert!(!headers.contains_key("cookie"));
        assert!(!headers.contains_key("if-none-match"));

        // Verify safe headers were kept
        assert!(headers.contains_key("content-type"));
        assert!(headers.contains_key("accept"));
        assert!(headers.contains_key("user-agent"));

        // Verify Authorization was replaced with upstream key
        assert_eq!(
            headers.get("authorization").unwrap().to_str().unwrap(),
            "Bearer sk-ant-upstream-key"
        );

        // Verify X-Forwarded-Proto was added
        assert_eq!(
            headers.get("x-forwarded-proto").unwrap().to_str().unwrap(),
            "https"
        );
    }

    #[test]
    fn test_filter_headers_preserves_provider_specific_headers() {
        let mut headers = HeaderMap::new();

        // Provider-specific headers that should be kept
        headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
        headers.insert("openai-organization", "org-123".parse().unwrap());
        headers.insert("x-stainless-lang", "js".parse().unwrap());
        headers.insert("x-stainless-runtime", "browser:chrome".parse().unwrap());

        let target = Target::builder()
            .url("https://api.anthropic.com".parse().unwrap())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // All provider-specific headers should be kept
        assert!(headers.contains_key("anthropic-version"));
        assert!(headers.contains_key("openai-organization"));
        assert!(headers.contains_key("x-stainless-lang"));
        assert!(headers.contains_key("x-stainless-runtime"));
    }

    #[test]
    fn test_path_stripping_with_duplicate_prefix() {
        // Test the logic: target URL has "/v1", request path has "/v1/chat/completions"
        // Should result in "https://api.openai.com/v1/chat/completions" not "/v1/v1/..."

        let target_url = url::Url::parse("https://api.openai.com/v1/").unwrap();
        let target_path = target_url.path().trim_end_matches('/'); // "/v1"
        let request_path = "v1/chat/completions"; // already stripped leading /

        let path_to_join = if !target_path.is_empty() && target_path != "/" {
            let target_path_no_slash = &target_path[1..];
            if let Some(rest) = request_path.strip_prefix(target_path_no_slash) {
                if rest.is_empty() || rest.starts_with('/') {
                    rest.strip_prefix('/').unwrap_or(rest)
                } else {
                    request_path
                }
            } else {
                request_path
            }
        } else {
            request_path
        };

        let result = target_url.join(path_to_join).unwrap();
        assert_eq!(
            result.as_str(),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn test_path_stripping_without_duplicate() {
        // Test: target URL has no path, request path has "/v1/chat/completions"
        // Should result in "https://api.openai.com/v1/chat/completions"

        let target_url = url::Url::parse("https://api.openai.com/").unwrap();
        let target_path = target_url.path().trim_end_matches('/'); // "/"
        let request_path = "v1/chat/completions";

        let path_to_join = if !target_path.is_empty() && target_path != "/" {
            let target_path_no_slash = &target_path[1..];
            if let Some(rest) = request_path.strip_prefix(target_path_no_slash) {
                if rest.is_empty() || rest.starts_with('/') {
                    rest.strip_prefix('/').unwrap_or(rest)
                } else {
                    request_path
                }
            } else {
                request_path
            }
        } else {
            request_path
        };

        let result = target_url.join(path_to_join).unwrap();
        assert_eq!(
            result.as_str(),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn test_path_stripping_with_actual_duplicate_paths() {
        // Edge case: API actually has /v1/v1/something
        // Target has no path, request has "v1/v1/something"

        let target_url = url::Url::parse("https://api.example.com/").unwrap();
        let target_path = target_url.path().trim_end_matches('/');
        let request_path = "v1/v1/something";

        let path_to_join = if !target_path.is_empty() && target_path != "/" {
            let target_path_no_slash = &target_path[1..];
            if let Some(rest) = request_path.strip_prefix(target_path_no_slash) {
                if rest.is_empty() || rest.starts_with('/') {
                    rest.strip_prefix('/').unwrap_or(rest)
                } else {
                    request_path
                }
            } else {
                request_path
            }
        } else {
            request_path
        };

        let result = target_url.join(path_to_join).unwrap();
        assert_eq!(result.as_str(), "https://api.example.com/v1/v1/something");
    }

    #[test]
    fn test_path_stripping_with_different_prefix() {
        // Test: target has "/v2", request has "/v1/chat/completions"
        // Should not strip, result in "https://api.example.com/v2/v1/chat/completions"

        let target_url = url::Url::parse("https://api.example.com/v2/").unwrap();
        let target_path = target_url.path().trim_end_matches('/'); // "/v2"
        let request_path = "v1/chat/completions";

        let path_to_join = if !target_path.is_empty() && target_path != "/" {
            let target_path_no_slash = &target_path[1..];
            if let Some(rest) = request_path.strip_prefix(target_path_no_slash) {
                if rest.is_empty() || rest.starts_with('/') {
                    rest.strip_prefix('/').unwrap_or(rest)
                } else {
                    request_path
                }
            } else {
                request_path
            }
        } else {
            request_path
        };

        let result = target_url.join(path_to_join).unwrap();
        assert_eq!(
            result.as_str(),
            "https://api.example.com/v2/v1/chat/completions"
        );
    }

    #[test]
    fn test_path_stripping_with_query_params() {
        // Test: path with query parameters should work correctly

        let target_url = url::Url::parse("https://api.openai.com/v1/").unwrap();
        let target_path = target_url.path().trim_end_matches('/'); // "/v1"
        let request_path = "v1/chat/completions?stream=true";

        let path_to_join = if !target_path.is_empty() && target_path != "/" {
            let target_path_no_slash = &target_path[1..];
            if let Some(rest) = request_path.strip_prefix(target_path_no_slash) {
                if rest.is_empty() || rest.starts_with('/') {
                    rest.strip_prefix('/').unwrap_or(rest)
                } else {
                    request_path
                }
            } else {
                request_path
            }
        } else {
            request_path
        };

        let result = target_url.join(path_to_join).unwrap();
        assert_eq!(
            result.as_str(),
            "https://api.openai.com/v1/chat/completions?stream=true"
        );
    }

    #[test]
    fn test_path_stripping_false_positive() {
        // Test: path starts with target prefix but no slash after (e.g., "v1x")
        // Should NOT strip since it's not a real path match

        let target_url = url::Url::parse("https://api.example.com/v1/").unwrap();
        let target_path = target_url.path().trim_end_matches('/'); // "/v1"
        let request_path = "v1x/something";

        let path_to_join = if !target_path.is_empty() && target_path != "/" {
            let target_path_no_slash = &target_path[1..];
            if let Some(rest) = request_path.strip_prefix(target_path_no_slash) {
                if rest.is_empty() || rest.starts_with('/') {
                    rest.strip_prefix('/').unwrap_or(rest)
                } else {
                    request_path
                }
            } else {
                request_path
            }
        } else {
            request_path
        };

        let result = target_url.join(path_to_join).unwrap();
        // Should NOT strip "v1" since "v1x" is not the same as "v1/"
        assert_eq!(result.as_str(), "https://api.example.com/v1/v1x/something");
    }

    // Timeout behavior tests
    // These tests document the expected behavior but can't easily test the actual timeout
    // functionality without a mock HTTP client. They serve as documentation and type-checking.

    #[test]
    fn test_timeout_config_can_be_set() {
        // Test that timeout can be configured on target
        use crate::target::Target;

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .request_timeout_secs(30)
            .build();

        assert_eq!(target.request_timeout_secs, Some(30));
    }

    #[test]
    fn test_timeout_defaults_to_none() {
        // Test that timeout defaults to None (unlimited)
        use crate::target::Target;

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        assert_eq!(target.request_timeout_secs, None);
    }

    #[test]
    fn test_gateway_timeout_error_response() {
        // Test that gateway_timeout returns 504 status
        let error = OnwardsErrorResponse::gateway_timeout();
        let response = error.into_response();

        assert_eq!(response.status().as_u16(), 504);
    }

    #[tokio::test]
    async fn test_gateway_timeout_error_body() {
        // Test that gateway_timeout has appropriate error message
        use http_body_util::BodyExt;

        let error = OnwardsErrorResponse::gateway_timeout();
        let response = error.into_response();

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();

        // Should contain error structure with message and code fields
        // ErrorResponseBody has: message, type, param, code (no outer "error" wrapper)
        assert!(body_str.contains("message"));
        assert!(body_str.contains("code"));
        assert!(body_str.contains("gateway_timeout"));
        assert!(body_str.contains("took too long"));
    }

    // Mock HttpClient that delays responses to test timeout behavior
    #[derive(Debug, Clone)]
    struct DelayedMockClient {
        delay: std::time::Duration,
        response_status: u16,
    }

    impl DelayedMockClient {
        fn new(delay: std::time::Duration, response_status: u16) -> Self {
            Self {
                delay,
                response_status,
            }
        }
    }

    #[async_trait::async_trait]
    impl HttpClient for DelayedMockClient {
        async fn request(
            &self,
            _req: axum::extract::Request,
        ) -> Result<axum::response::Response, Box<dyn std::error::Error + Send + Sync>> {
            tokio::time::sleep(self.delay).await;
            Ok(axum::response::Response::builder()
                .status(self.response_status)
                .body(axum::body::Body::empty())
                .unwrap())
        }
    }

    #[tokio::test]
    async fn test_mock_client_timeout_fires() {
        // Test that tokio::time::timeout correctly fires when the mock client delays
        // This validates the test infrastructure, not the handler behavior
        use crate::target::Target;

        // Create a target with 1 second timeout
        let target = Target::builder()
            .url("https://api.example.com/".parse().unwrap())
            .request_timeout_secs(1)
            .build();

        let pool = target.into_pool();

        // Mock client that delays 2 seconds (longer than timeout)
        let mock_client = DelayedMockClient::new(std::time::Duration::from_secs(2), 200);

        let state = AppState {
            targets: crate::target::Targets {
                targets: std::sync::Arc::new(dashmap::DashMap::new()),
                key_rate_limiters: std::sync::Arc::new(dashmap::DashMap::new()),
                key_concurrency_limiters: std::sync::Arc::new(dashmap::DashMap::new()),
                key_labels: std::sync::Arc::new(dashmap::DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            },
            http_client: mock_client,
            body_transform_fn: None,
            response_transform_fn: None,
            streaming_header: None,
            response_id_header: None,
            tool_executor: std::sync::Arc::new(crate::NoOpToolExecutor),
            response_store: std::sync::Arc::new(crate::NoOpResponseStore),
            body_limit: crate::DEFAULT_BODY_LIMIT,
        };

        // Create a simple POST request
        let req = axum::extract::Request::builder()
            .uri("/v1/chat/completions")
            .method("POST")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(r#"{"model":"gpt-4","messages":[]}"#))
            .unwrap();

        // Test the timeout logic directly (not the full handler)
        let target = pool.first_target().unwrap();
        let timeout_secs = target.request_timeout_secs.unwrap();
        let timeout_duration = std::time::Duration::from_secs(timeout_secs);

        let result = tokio::time::timeout(timeout_duration, state.http_client.request(req)).await;

        // Should timeout (Err from tokio::time::timeout)
        assert!(result.is_err(), "Expected timeout but request completed");
    }

    #[test]
    fn test_pool_with_fallback_enabled() {
        // Test that pool configuration correctly enables fallback with multiple providers
        // This validates pool setup, not actual retry behavior
        use crate::load_balancer::{Provider, ProviderPool};
        use crate::target::{FallbackConfig, LoadBalanceStrategy, Target};

        // Create two targets
        let target1 = Target::builder()
            .url("https://provider1.example.com/".parse().unwrap())
            .request_timeout_secs(1)
            .build();

        let target2 = Target::builder()
            .url("https://provider2.example.com/".parse().unwrap())
            .request_timeout_secs(1)
            .build();

        let providers = vec![Provider::new(target1, 1), Provider::new(target2, 1)];

        let fallback_config = Some(FallbackConfig {
            enabled: true,
            on_status: vec![],
            on_rate_limit: false,
            ..Default::default()
        });

        let pool = ProviderPool::with_config(
            providers,
            None,
            None,
            None,
            fallback_config,
            LoadBalanceStrategy::Priority,
            false,
            Vec::new(),
        );

        assert!(pool.fallback_enabled(), "Fallback should be enabled");
        assert_eq!(pool.len(), 2, "Pool should have 2 providers");
    }

    /// Helper: build a minimal target with the requested trace flags.
    fn target_with_trace_flags(
        trusted: Option<bool>,
        propagate_trace_context: Option<bool>,
    ) -> Target {
        Target {
            url: "https://api.example.com/".parse().unwrap(),
            keys: None,
            onwards_key: None,
            onwards_model: None,
            limiter: None,
            upstream_auth_header_name: None,
            upstream_auth_header_prefix: None,
            response_headers: None,
            sanitize_response: false,
            open_responses: None,
            request_timeout_secs: None,
            trusted,
            propagate_trace_context,
            reasoning_translation: None,
        }
    }

    #[test]
    fn resolve_trace_propagation_explicit_true_overrides_untrusted_pool() {
        let target = target_with_trace_flags(Some(false), Some(true));
        assert!(resolve_trace_propagation(&target, false));
    }

    #[test]
    fn resolve_trace_propagation_explicit_false_overrides_trusted_pool() {
        let target = target_with_trace_flags(Some(true), Some(false));
        assert!(!resolve_trace_propagation(&target, true));
    }

    #[test]
    fn resolve_trace_propagation_inherits_from_target_trusted_when_unset() {
        let target = target_with_trace_flags(Some(true), None);
        assert!(resolve_trace_propagation(&target, false));

        let target = target_with_trace_flags(Some(false), None);
        assert!(!resolve_trace_propagation(&target, true));
    }

    #[test]
    fn resolve_trace_propagation_inherits_from_pool_when_target_unset() {
        let target = target_with_trace_flags(None, None);
        assert!(resolve_trace_propagation(&target, true));
        assert!(!resolve_trace_propagation(&target, false));
    }

    #[test]
    fn withhold_trace_context_strips_inbound_trace_headers() {
        // When propagation is disabled for an upstream, an inbound trace
        // context (e.g. from the caller) must not be forwarded.
        let mut headers = HeaderMap::new();
        headers.insert(
            "traceparent",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
                .parse()
                .unwrap(),
        );
        headers.insert("tracestate", "vendor=value".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());

        withhold_trace_context(&mut headers);

        assert!(
            !headers.contains_key("traceparent"),
            "traceparent must be stripped"
        );
        assert!(
            !headers.contains_key("tracestate"),
            "tracestate must be stripped"
        );
        // Unrelated headers are left untouched.
        assert!(
            headers.contains_key("content-type"),
            "non-trace headers must be preserved"
        );
    }

    #[test]
    fn withhold_trace_context_is_noop_when_absent() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        withhold_trace_context(&mut headers);
        assert_eq!(headers.len(), 1);
    }
}
