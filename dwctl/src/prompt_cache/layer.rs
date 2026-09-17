//! The dwctl-owned cache tower layer — the integration point.
//!
//! Wrapping the (cache-agnostic) onwards router, on each cacheable request it:
//!   1. reads the body, validates + strips `cache_control` markers, extracts the virtual model,
//!   2. **forks** [`Classifier::classify`] (in parallel with the upstream call),
//!   3. forces `include_usage`, forwards to onwards,
//!   4. **injects** the `CacheStats` into the response usage — joining classify inline for a
//!      buffered (non-streaming) body, or **deferring** the join into the SSE stream's terminal
//!      usage frame for a stream, so the first token is never held by classify,
//!   5. on a billing-success completion, **commits** the `PendingWrite` to the index (off path).
//!
//! Everything lives in one scope, so the pending write is a local value — no
//! correlation id, no trait injected into onwards. Failures degrade to "no caching"; the
//! commit is success-gated.
//!
//! Inactive requests (cache-disabled model, degraded classify) are NOT forwarded untouched:
//! their usage still gets provider-written cache fields scrubbed
//! ([`super::inject::scrub_provider_cache_fields`]). This layer is the sole writer of
//! customer-visible cache accounting — an upstream's own `cached_tokens` (e.g. OpenRouter
//! implicit caching on a model without cache pricing) must never reach a customer we billed
//! at full price. Onwards itself stays a faithful pass-through (the standalone gateway in
//! front of dynamo must forward engine cache stats for internal capture); the scrub decision
//! lives here, next to billing.
//!
//! On the ACTIVE (tariffed) path the paradigm is chosen by ARMING — `had_markers`, the
//! pre-flight signal that the request carried cache markers (block, top-level automatic,
//! or the `cacheBreakpoint` query param) — never by cache values. An UNARMED request
//! bills the upstream's own reported cache hit — engine-cache passthrough (see
//! [`super::inject`]'s `splice_cache_fields`) — so a tariff row alone buys implicit,
//! best-effort caching for marker-less clients. (Billing clamps the read multiplier to
//! 1 for engine-sourced reads — a >1 multiplier is a misconfiguration, and an unmarked
//! customer must never pay above list price for a cache hit.) An ARMED request is wholly explicit:
//! deterministic module numbers, zeros included, engine report ignored. Strictly one
//! paradigm per request, so a customer's cache numbers are always explainable from
//! their own markers.
//!
//! Placed **inner to outlet** in the stack so the analytics/billing capture sees the
//! injected cache fields.

use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tracing::warn;

use futures::StreamExt;
use http_body_util::BodyExt;

use super::classifier::{Classifier, ClassifyOutcome, ClassifyRequest};
use super::index::{CacheResult, TierPolicy};
use super::inject::{
    UpstreamCachedTokens, UsageEdit, inject_into_response_nonstreaming, scan_edit_sse, scrub_response_nonstreaming, strip_cache_control,
};
use super::metrics as cache_metrics;
use super::parse::{ParseError, validate_markers};
use super::query::{self, Inject, InvalidBreakpointValue};
use super::sse::SseBufferedStream;
use super::stats::CacheBilling;

/// Bound on the index commit (off the response path). A slow/hung DB can't leak the
/// spawned task or hold a pool connection indefinitely; a miss just drops the write
/// (best-effort — a reconciliation pass backstops it). Generous vs the classify deadline
/// because it's off-path and is real DB work, not a race against generation.
const COMMIT_DEADLINE: Duration = Duration::from_secs(30);

/// State for [`cache_middleware`]. Added to the stack only when caching is enabled.
#[derive(Clone)]
pub struct CacheLayerState {
    pub classifier: Classifier,
    pub deadline: Duration,
    /// Max bytes to buffer when reading a cacheable request body. Set to the *same* limit
    /// the onwards router enforces (`limits.requests.max_body_size`) so this layer is never
    /// more restrictive than the entry point — a request onwards would accept is buffered,
    /// one it would reject degrades here too. Bounds memory (defence-in-depth vs a DoS).
    pub body_limit: usize,
}

impl CacheLayerState {
    /// `deadline` comes from `cache.classify_deadline_secs` (default 5s — onwards' old
    /// `DEFAULT_CLASSIFY_DEADLINE`); it only bites on an index/tokenizer outage (classify
    /// normally finishes during generation). Latency-insensitive deployments (the
    /// fusillade-batch pod, where the join is inline post-response with no first-token
    /// pressure) set it higher so index retries have room to land.
    pub fn new(classifier: Classifier, body_limit: usize, deadline: Duration) -> Self {
        Self {
            classifier,
            deadline,
            body_limit,
        }
    }
}

/// Chat completions AND plain completions (`/chat/completions` ends with `/completions`,
/// so one suffix covers both). Responses + others pass straight through — Responses
/// arrives here already translated to chat-completions, so it is covered upstream of
/// this check.
///
/// Plain `/completions` cannot create module cache entries because the chat parser does
/// not interpret its `prompt` field (string, string-array, or token-array alike), so it
/// finds zero breakpoints (a top-level automatic marker no-ops on a blockless body per
/// the Anthropic rule — no 400). Unmarked requests on a tariffed model therefore take
/// the engine-cache passthrough, and the scrub applies either way: before this layer
/// covered `/completions`, the upstream's own `cached_tokens` leaked to customers
/// unbilled. A body `cache_control` marker still arms the request (deterministic zeros —
/// the one-paradigm rule); `cacheBreakpoint` is stripped and ignored on this route, so
/// it cannot suppress implicit billing.
fn is_cacheable(req: &Request) -> bool {
    // Mirrors `onwards::RequestClass::from_path`: trailing slashes trimmed, and the same
    // deliberate suffix breadth. The scrub is a leak-guard, so this layer must cover
    // every path shape onwards can serve a completions-style response on — matching
    // narrower than the serving surface would reopen the provider-stat leak there.
    req.method() == Method::POST && req.uri().path().trim_end_matches('/').ends_with("/completions")
}

/// Turn a synchronous marker-validation failure into the structured 400 the rest of the stack
/// uses (same shape as the body-read error) — the request is rejected like a bad parameter, not
/// silently un-cached. A disabled-tier message names the tiers that ARE available so the client
/// can adjust.
fn marker_rejection_response(e: &ParseError, policy: &TierPolicy) -> Response {
    let reason = match e {
        ParseError::DisabledTier(_) => Some("tier_disabled"),
        ParseError::InvalidTtl(_) => Some("invalid_ttl"),
        ParseError::UnsupportedType(_) => Some("unsupported_type"),
        ParseError::TooManyBreakpoints => Some("too_many_breakpoints"),
        ParseError::MalformedCacheControl => Some("malformed_cache_control"),
        ParseError::AutomaticTtlConflict => Some("automatic_ttl_conflict"),
        ParseError::NoAutomaticSlot => Some("automatic_no_slot"),
        // validate_markers takes an already-parsed Value, so a JSON error can't reach here.
        ParseError::Json(_) => None,
    };
    if let Some(r) = reason {
        cache_metrics::record_markers_rejected(r);
    }
    let message = match e {
        ParseError::DisabledTier(_) => format!("{e}; available tiers: {}", policy.enabled_strs().join(", ")),
        _ => e.to_string(),
    };
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": "invalid_request_error",
            "code": "invalid_cache_control",
            "param": "cache_control",
        }
    });
    (StatusCode::BAD_REQUEST, axum::Json(body)).into_response()
}

/// Structured 400 for an unrecognized `cacheBreakpoint` query-param value — strict, like the
/// marker rejections above: a typo must not silently disable caching while the caller believes
/// it's on. Names the supported value so the client can adjust.
fn query_rejection_response(e: &InvalidBreakpointValue) -> Response {
    cache_metrics::record_query_breakpoint("invalid");
    let body = serde_json::json!({
        "error": {
            "message": format!(
                "invalid {} value {:?}; supported values: \"lastUserMessage\"",
                query::CACHE_BREAKPOINT_PARAM, e.0
            ),
            "type": "invalid_request_error",
            "code": "invalid_cache_breakpoint",
            "param": query::CACHE_BREAKPOINT_PARAM,
        }
    });
    (StatusCode::BAD_REQUEST, axum::Json(body)).into_response()
}

pub async fn cache_middleware(State(state): State<CacheLayerState>, request: Request, next: Next) -> Response {
    if !is_cacheable(&request) {
        return next.run(request).await;
    }

    let (mut parts, body) = request.into_parts();
    // Bounded by the same limit onwards enforces (never more restrictive than the entry).
    let body_bytes = match axum::body::to_bytes(body, state.body_limit).await {
        Ok(b) => b,
        Err(e) => {
            // Can't read the body within the limit. Return the structured 400 the rest of the
            // stack uses (matches image_normalizer_middleware) rather than forwarding an empty
            // body: an empty forward would surface to the client as a confusing JSON-parse 4xx
            // from onwards instead of a clear body-read error.
            warn!(error = %e, "Failed to read request body in cache middleware");
            cache_metrics::record_body_read_failed();
            let body = serde_json::json!({
                "error": {
                    "message": format!("failed to read request body: {e}"),
                    "type": "invalid_request_error",
                    "code": "body_read_failed",
                }
            });
            return (StatusCode::BAD_REQUEST, axum::Json(body)).into_response();
        }
    };

    // Parse the body once; reused both for marker validation and to extract `model` (no extra
    // deserialization). `None` if the body isn't JSON — onwards will surface that as a 400.
    let mut parsed_body = serde_json::from_slice::<serde_json::Value>(&body_bytes).ok();

    // `?cacheBreakpoint=lastUserMessage` (see `super::query`): translate the query param into the
    // top-level automatic-caching marker BEFORE validation, so everything downstream — the 400s
    // below, classify's hashing, the outbound marker strip — behaves exactly as if the client had
    // sent the body field. The param itself is stripped from the URI either way: onwards forwards
    // `path_and_query` verbatim, and it must not leak upstream. The re-serialize below is the one
    // extra cost, and only on param-carrying requests (whose marker means the outbound sanitiser
    // was going to rewrite the body anyway).
    let mut body_bytes = body_bytes;
    // The param is a Chat Completions feature (`super::query`'s contract): on any other
    // cacheable path — plain /completions has no blocks for the marker to bind to — it is
    // stripped from the URI (it must never leak upstream) and otherwise ignored, so it
    // cannot arm the request and suppress implicit billing.
    if !parts.uri.path().trim_end_matches('/').ends_with("/chat/completions") {
        if query::breakpoint_marker(parts.uri.query()).is_ok_and(|m| m.is_some()) || query::breakpoint_marker(parts.uri.query()).is_err() {
            parts.uri = query::strip_param(&parts.uri);
            cache_metrics::record_query_breakpoint("non_chat_ignored");
        }
    } else {
        match query::breakpoint_marker(parts.uri.query()) {
            Ok(None) => {}
            Ok(Some(marker)) => {
                parts.uri = query::strip_param(&parts.uri);
                let outcome = match parsed_body.as_mut() {
                    Some(body) => match query::inject_marker(body, marker) {
                        Inject::Applied => match serde_json::to_vec(body) {
                            Ok(b) => {
                                body_bytes = b.into();
                                "applied"
                            }
                            // Serializing a `Value` we just parsed can't realistically fail; if
                            // it ever does, forward the original body un-injected (no caching)
                            // rather than failing the request.
                            Err(_) => "reserialize_failed",
                        },
                        Inject::BodyFieldWins => "body_field_wins",
                        Inject::NotAnObject => "not_an_object",
                    },
                    // Unparseable JSON: nothing to inject into; onwards will 400 the body itself.
                    None => "not_json",
                };
                cache_metrics::record_query_breakpoint(outcome);
            }
            Err(e) => return query_rejection_response(&e),
        }
    }

    // Reject disallowed/malformed cache_control markers synchronously, before forking + forwarding
    // — a 400 like a bad parameter, NOT a silent no-cache, so the client learns immediately and
    // isn't billed full price thinking it cached. Cheap: walks the already-parsed Value, no hashing.
    if let Some(body) = &parsed_body
        && let Err(e) = validate_markers(body, state.classifier.tier_policy(), state.classifier.telemetry_policy())
    {
        return marker_rejection_response(&e, state.classifier.tier_policy());
    }

    let virtual_model = parsed_body
        .as_ref()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from));
    let api_key = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        // Accept both casings and trim stray whitespace, as the rest of the stack does
        // (error_enrichment, image_normalizer) — else a key resolves differently here and
        // silently disables caching for that request.
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
        .map(|t| t.trim().to_string());

    // Bounded metric label (the deployed alias). Cloned before `virtual_model` is moved
    // into the classify task below; empty only for a body with no `model` field.
    let model_label = virtual_model.clone().unwrap_or_default();

    // Fork classify, parallel with the upstream call. Owns its inputs so the task is
    // `'static`; this is the one body clone (a future parse-once refactor would remove it).
    let classify_handle = virtual_model.map(|model| {
        let classifier = state.classifier.clone();
        let body = body_bytes.to_vec();
        tokio::spawn(async move {
            classifier
                .classify(ClassifyRequest {
                    virtual_model: &model,
                    body: &body,
                    api_key: api_key.as_deref(),
                    // Serving always has the bearer token; only historical replay pre-resolves.
                    principal: None,
                })
                .await
        })
    });

    // Sanitise the outbound body: strip markers + ensure include_usage (no-op → keep).
    // `had_markers` is whether the client actually sent cache_control (the adoption signal,
    // recorded for all traffic) — NOT whether the body changed, since a stream gets
    // include_usage injected even with no markers. Re-frame: set Content-Length and drop any
    // stale Transfer-Encoding (sending both is invalid HTTP). `from(u64)` is the numeric ctor.
    let (stripped, had_markers) = strip_cache_control(&body_bytes, state.classifier.telemetry_policy());
    cache_metrics::record_marker_request(had_markers);
    let forward = stripped.unwrap_or(body_bytes);
    parts.headers.remove(header::TRANSFER_ENCODING);
    parts
        .headers
        .insert(header::CONTENT_LENGTH, axum::http::HeaderValue::from(forward.len() as u64));
    let mut response = next.run(Request::from_parts(parts, Body::from(forward))).await;
    // Hand the upstream's own cached-token count to analytics (outlet, outside this
    // layer) before the rewrites below hide it. Inserted now, at head time, because that
    // is when outlet clones the extensions; filled when the usage object is seen.
    let upstream_cached = UpstreamCachedTokens::default();
    response.extensions_mut().insert(upstream_cached.clone());
    let cache_billing = CacheBilling::default();
    response.extensions_mut().insert(cache_billing.clone());

    // Post-response work — resolve classify, inject the stats, commit on success — differs by
    // transport. The split is the whole point of this layer's latency profile:
    //
    // - NON-STREAMING: by the time `next.run` yields a response the upstream round-trip is done and
    //   the full completion generated, so classify (which raced that generation) has almost always
    //   finished — the join here is typically instant. We then buffer the JSON body to edit it.
    // - STREAMING: joining here would hold the *first* token until classify resolves. But the
    //   stats are only needed at the *terminal* usage frame, so we hand the classify handle into
    //   the SSE stream and resolve it there (bounded by the deadline). The first token flows
    //   untouched; at worst only the final frame waits.
    let Some(mut handle) = classify_handle else {
        // No `model` field → classify was never spawned → nothing cacheable.
        cache_metrics::record_request_outcome("inactive");
        return response;
    };

    if is_streaming(&response) {
        return defer_classify_into_stream(
            response,
            handle,
            state.deadline,
            model_label,
            state.classifier.clone(),
            upstream_cached,
            cache_billing,
            had_markers,
        );
    }

    let outcome = join_classify(&mut handle, state.deadline, &model_label).await;
    if !outcome.active {
        // Disabled model (or a degraded classify) → no injection, but the upstream's own cache
        // accounting must still be scrubbed: this module is the only writer of customer-visible
        // cache fields, and a provider-reported `cached_tokens` on a model we bill at full price
        // reads as a discount we didn't give.
        return scrub_response_nonstreaming(response, &upstream_cached).await;
    }
    // Passthrough gate: markers pick the paradigm — no markers means implicit.
    let allow_implicit = !had_markers;
    let (response, billing_ok) = inject_into_response_nonstreaming(response, &outcome.stats, &upstream_cached, allow_implicit).await;
    if !outcome.pending.is_empty() {
        if billing_ok {
            spawn_commit(state.classifier.clone(), outcome.pending);
        } else {
            // billing_ok is false both for a non-billable status and for a 2xx JSON body with no
            // usage object (or unparseable body) — label them apart for diagnosis.
            let reason = if response.status().is_success() { "no_usage" } else { "non_2xx" };
            cache_metrics::record_commit_vetoed(reason);
        }
    }
    response
}

/// Whether a response is a streaming (SSE) chat completion. Media types are case-insensitive and
/// may carry parameters (e.g. `Text/Event-Stream; charset=utf-8`), so match the trimmed base type
/// case-insensitively — a mis-detected SSE would wrongly take the non-streaming path and buffer
/// the whole stream.
fn is_streaming(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|ct| ct.trim().eq_ignore_ascii_case("text/event-stream"))
}

/// Join the spawned classify task under the deadline, recording the classify result, the
/// request-outcome label, and (for an active request) the per-model token volumes. A timeout,
/// task error, or panic resolves to `inactive` (no caching) — never an error to the customer.
/// Used by both transports: joined inline for non-streaming, and lazily at the terminal usage
/// frame for streaming. Borrows `&mut handle` (rather than taking ownership) so the caller can
/// keep it inside [`AbortOnDrop`] across this await — if the client disconnects mid-join, the
/// guard drops with the handle still in it and aborts the task, instead of this future dropping an
/// owned handle and *detaching* it into an orphan. Also times out against the handle so we can
/// `abort()` it on the deadline.
async fn join_classify(
    handle: &mut tokio::task::JoinHandle<CacheResult<ClassifyOutcome>>,
    deadline: Duration,
    model_label: &str,
) -> ClassifyOutcome {
    let outcome = match tokio::time::timeout(deadline, &mut *handle).await {
        Ok(Ok(Ok(result))) => {
            cache_metrics::record_classify("ok");
            result
        }
        Ok(Ok(Err(e))) => {
            cache_metrics::record_classify("error");
            warn!(error = %e, "cache classify failed — billing un-cached");
            ClassifyOutcome::inactive()
        }
        Ok(Err(e)) => {
            // JoinError is a panic OR a cancellation (e.g. runtime shutdown); only the former is
            // a bug, so don't fold cancellations into the "panicked" series.
            cache_metrics::record_classify(if e.is_panic() { "panicked" } else { "error" });
            warn!(error = %e, "cache classify task failed");
            ClassifyOutcome::inactive()
        }
        Err(_) => {
            cache_metrics::record_classify("deadline_exceeded");
            handle.abort(); // best-effort, reconciliation backstops; don't leak the task
            ClassifyOutcome::inactive()
        }
    };

    // Request-level outcome across ALL traffic (incl. inactive). No model label: `inactive` covers
    // unknown/typo models (raw client input) → unbounded cardinality; per-model volumes are below.
    cache_metrics::record_request_outcome(outcome_label(&outcome));
    if outcome.active && !model_label.is_empty() {
        cache_metrics::record_token_volumes(
            model_label,
            outcome.stats.read,
            outcome.stats.creation_5m,
            outcome.stats.creation_1h,
            outcome.stats.creation_24h,
        );
    }
    outcome
}

fn outcome_label(outcome: &ClassifyOutcome) -> &'static str {
    if !outcome.active {
        "inactive"
    } else if outcome.stats.read > 0 && outcome.stats.creation_total() > 0 {
        "read_and_create"
    } else if outcome.stats.read > 0 {
        "read"
    } else if outcome.stats.creation_total() > 0 {
        "create_only"
    } else {
        "zero_active"
    }
}

/// RAII guard for the deferred classify handle: aborts the spawned task on drop. If the client
/// disconnects before the stream reaches the terminal usage frame, the wrapping `async_stream` is
/// dropped — without this, dropping the bare `JoinHandle` would *detach* the (possibly stalled)
/// classify task into an orphan that bypasses the deadline. Aborting cancels it at its next await.
/// `take()` hands the handle to `join_classify` on the normal path, defusing the guard.
struct AbortOnDrop<T>(Option<tokio::task::JoinHandle<T>>);

impl<T> AbortOnDrop<T> {
    fn take(&mut self) -> Option<tokio::task::JoinHandle<T>> {
        self.0.take()
    }

    /// Borrow the handle *without* removing it, so an await on it stays cancellation-safe: the
    /// guard still owns the handle and will `abort()` it on drop. Defuse with [`take`] only once
    /// the await has completed.
    fn as_mut(&mut self) -> Option<&mut tokio::task::JoinHandle<T>> {
        self.0.as_mut()
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            // The guard was never defused via `take()`, so the stream was dropped before classify
            // was joined — a client disconnect ahead of the terminal usage frame. Abort the task so
            // it can't outlive the request, and record the abandonment: without this, classify and
            // request-outcome dashboards silently undercount under high disconnect rates (the join,
            // and its metrics, never run on this path). Cheaper and safer than a detached
            // join-for-metrics, which would re-orphan the very task this guard exists to cancel.
            h.abort();
            cache_metrics::record_classify("abandoned");
            cache_metrics::record_request_outcome("aborted");
        }
    }
}

/// Defer the classify-await into the SSE stream so it never holds the first token. Returns the
/// response immediately; as frames flow it resolves classify lazily at the terminal usage frame
/// (bounded by the deadline — classify has almost always finished during generation), injects the
/// stats there, and commits the index write on a billing-success completion. Every failure path
/// (deadline, classify error, mid-stream error frame, no usage frame, client disconnect) degrades
/// to no caching with the request unharmed.
#[allow(clippy::too_many_arguments)]
fn defer_classify_into_stream(
    response: Response,
    handle: tokio::task::JoinHandle<CacheResult<ClassifyOutcome>>,
    deadline: Duration,
    model_label: String,
    classifier: Classifier,
    upstream_cached: UpstreamCachedTokens,
    cache_billing: CacheBilling,
    had_markers: bool,
) -> Response {
    let (parts, body) = response.into_parts();
    let status_ok = parts.status.is_success();
    // Normalise the body error to io::Error, then re-aggregate provider chunks into complete SSE
    // events so a terminal usage frame split across body chunks isn't missed.
    let body_stream = BodyExt::into_data_stream(body).map(|r| r.map_err(std::io::Error::other));
    let buffered = SseBufferedStream::new(body_stream);

    let stream = async_stream::stream! {
        futures::pin_mut!(buffered);
        // Aborts the classify task if the stream is dropped early (client disconnect) instead of
        // detaching it into an orphan; `take()` defuses it on the normal terminal-frame path.
        let mut handle = AbortOnDrop(Some(handle));
        let mut outcome: Option<ClassifyOutcome> = None;
        let mut edited = false;
        let mut saw_error = false;
        let mut saw_usage = false;

        while let Some(item) = buffered.next().await {
            let chunk = match item {
                Ok(c) => c,
                // A transport error mid-stream is a failure: forward it and veto the write.
                Err(e) => {
                    saw_error = true;
                    yield Err(e);
                    continue;
                }
            };
            // Detect the billing signals on this chunk (no injection yet).
            let probe = scan_edit_sse(&chunk, UsageEdit::Probe);
            if let Some(v) = probe.upstream_cached_tokens {
                upstream_cached.set(v);
            }
            saw_error |= probe.saw_error;
            // The terminal usage frame is the only place the stats are needed: resolve classify now
            // — the single blocking await, on the *last* frame, bounded by the deadline. Borrow the
            // handle from the guard (don't `take()` it) so a disconnect *during* this await still
            // drops the guard → abort + metrics; defuse it only once the join has completed.
            if probe.saw_usage && outcome.is_none() {
                if let Some(h) = handle.as_mut() {
                    outcome = Some(join_classify(h, deadline, &model_label).await);
                }
                handle.take();
            }
            saw_usage |= probe.saw_usage;
            // Edit the (single) usage frame: inject the stats for an active (cache-enabled)
            // request, otherwise scrub the upstream's own cache accounting — this module is the
            // only writer of customer-visible cache fields, and a provider-reported
            // `cached_tokens` on a model we bill at full price reads as a discount we didn't give.
            let out = if !edited && probe.saw_usage {
                let scan = match outcome.as_ref() {
                    Some(o) if o.active => scan_edit_sse(
                        &chunk,
                        UsageEdit::Inject {
                            stats: &o.stats,
                            allow_implicit: !had_markers,
                        },
                    ),
                    // Inactive — and `None` can't happen (the classify join above runs on the
                    // first usage frame), so it degrades to the safe edit.
                    _ => scan_edit_sse(&chunk, UsageEdit::Scrub),
                };
                // Only mark done once it *actually* rewrote — a (rare) reserialize failure (or a
                // scrub with nothing to remove) shouldn't disable editing a later usage frame.
                if let Some(billed) = scan.billing_stats {
                    cache_billing.set(billed);
                }
                edited |= scan.rewritten.is_some();
                scan.rewritten.unwrap_or(chunk)
            } else {
                chunk
            };
            yield Ok(out);
        }

        // Stream drained cleanly. Resolve classify even if no usage frame ever arrived (e.g. an
        // error-only stream) so its metrics are still recorded, then decide the commit. Borrow from
        // the guard across the await (as above) — the consumer can still drop us mid-join here — and
        // defuse only once it completes.
        let outcome = match outcome {
            Some(o) => o,
            None => {
                if let Some(h) = handle.as_mut() {
                    let o = join_classify(h, deadline, &model_label).await;
                    handle.take();
                    o
                } else {
                    ClassifyOutcome::inactive()
                }
            }
        };
        if outcome.active && !outcome.pending.is_empty() {
            if status_ok && !saw_error && saw_usage {
                // Off the response path: the client already has every frame; don't hold the
                // connection open on the DB write.
                spawn_commit(classifier, outcome.pending);
            } else {
                // Distinguish the veto reasons so the metric is diagnosable: a 2xx stream that
                // carried an error frame vs. one that simply never emitted a usage frame are
                // different upstream faults. (A true client disconnect aborts the task before
                // this runs, so it's never labelled here.)
                let reason = if !status_ok {
                    "non_2xx"
                } else if saw_error {
                    "error_frame"
                } else {
                    "no_usage"
                };
                cache_metrics::record_commit_vetoed(reason);
            }
        }
    };

    let mut response = Response::from_parts(parts, Body::from_stream(stream));
    response.headers_mut().remove(header::CONTENT_LENGTH);
    response
}

/// Commit the pending write under [`COMMIT_DEADLINE`], so a slow/hung DB can't leak the
/// task or hold a connection. A timeout or error just drops the write (best-effort).
async fn commit_with_deadline(classifier: &Classifier, pending: &super::stats::PendingWrite) {
    let start = std::time::Instant::now();
    let result = tokio::time::timeout(COMMIT_DEADLINE, classifier.commit(pending)).await;
    cache_metrics::record_commit_duration(start.elapsed().as_secs_f64());
    match result {
        Ok(Ok(())) => cache_metrics::record_commit("ok"),
        Ok(Err(e)) => {
            cache_metrics::record_commit("error");
            warn!(error = %e, "cache index commit failed");
        }
        Err(_) => {
            cache_metrics::record_commit("timeout");
            warn!("cache index commit timed out");
        }
    }
}

/// Spawn the success-gated index commit off the response path.
fn spawn_commit(classifier: Classifier, pending: super::stats::PendingWrite) {
    tokio::spawn(async move {
        commit_with_deadline(&classifier, &pending).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::users::Role;
    use crate::inference::translation::{TranslationRegistry, middleware::translation_middleware, responses::OpenResponses};
    use crate::metrics::errors::component::ANALYTICS_BATCHER;
    use crate::pricing::{CacheMultipliers, TokenCounts, charged_cost};
    use crate::prompt_cache::{
        CacheIndex, IndexScope, ModelConfigResolver, PostgresIndex, PrincipalResolver, TelemetryPolicy, TokenizerClient,
        parse_chat_completions,
    };
    use crate::request_logging::serializers::{extract_cache_tokens, extract_from_last_usage, raw_usage_tokens};
    use crate::test::utils::{create_test_api_key_for_user, create_test_endpoint, create_test_model, create_test_user};
    use axum::http::Extensions;
    use axum::middleware::from_fn_with_state;
    use axum::routing::post;
    use axum::{Json, Router};
    use outlet::ResponseData;
    use rust_decimal::Decimal;
    use sqlx::PgPool;
    use std::sync::{Arc, Mutex};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ALIAS: &str = "layer-model";
    const TOK_VER: &str = "sha256:lv1";

    fn all_tiers() -> TierPolicy {
        TierPolicy::from_config(&["5m".to_string(), "1h".to_string(), "24h".to_string()], "5m")
    }

    /// Poll until `hash` is visible in the index for `scope`, or panic after ~5s.
    ///
    /// The commit runs on a spawned task whose Postgres round-trip needs wall time, not
    /// scheduler turns — a bounded `yield_now` loop is a race that loses on a loaded CI
    /// runner (observed as a flake on an unrelated PR's run).
    async fn await_commit(idx: &PostgresIndex, scope: &IndexScope, hash: &crate::prompt_cache::PrefixHash, what: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if !idx.lookup(scope, std::slice::from_ref(hash)).await.unwrap().is_empty() {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "{what}: write did not commit within 5s");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Stand-in for onwards/upstream: a chat completion with a `usage` object.
    async fn mock_upstream() -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "id": "chatcmpl-1", "object": "chat.completion",
            "choices": [{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
            "usage": {"prompt_tokens": 2000, "completion_tokens": 2, "total_tokens": 2002}
        }))
    }

    fn body() -> serde_json::Value {
        serde_json::json!({
            "model": ALIAS,
            "messages": [
                {"role":"system","content":[{"type":"text","text":"static system","cache_control":{"type":"ephemeral","ttl":"1h"}}]},
                {"role":"user","content":"hi"}
            ]
        })
    }

    #[sqlx::test]
    async fn end_to_end_injects_then_reads(pool: PgPool) {
        let user = create_test_user(&pool, Role::StandardUser).await;
        let key = create_test_api_key_for_user(&pool, user.id).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let id = create_test_model(&pool, "m", ALIAS, endpoint, user.id).await;
        // Presence of a cache-tariff row enables caching for the model.
        sqlx::query!(
            r#"INSERT INTO model_cache_tariffs
                 (deployed_model_id, write_multiplier_5m, write_multiplier_1h, write_multiplier_24h, min_prefix_tokens)
               VALUES ($1, 1.25, 2.0, 2.5, 1024)"#,
            id
        )
        .execute(&pool)
        .await
        .unwrap();

        let tok = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{"alias": ALIAS, "hf_repo": "o/m", "tokenizer_version": TOK_VER}]
            })))
            .mount(&tok)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/tokenize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "virtual_model": ALIAS, "tokenizer_version": TOK_VER,
                "segment_counts": [1500], "cumulative": [1500], "total": 1500
            })))
            .mount(&tok)
            .await;

        let classifier = Classifier::new(
            PrincipalResolver::new(pool.clone()),
            ModelConfigResolver::new(pool.clone()),
            TokenizerClient::new(tok.uri()),
            Arc::new(PostgresIndex::new(pool.clone(), 1)),
            all_tiers(),
            TelemetryPolicy::default(),
            false,
        );
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_upstream))
            .layer(from_fn_with_state(
                CacheLayerState::new(classifier, usize::MAX, Duration::from_secs(5)),
                cache_middleware,
            ));
        let server = axum_test::TestServer::new(app).unwrap();

        // First request: nothing cached yet → all-creation, response carries zeroed read.
        let r1 = server
            .post("/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", key.secret))
            .json(&body())
            .await;
        r1.assert_status_ok();
        let v1: serde_json::Value = r1.json();
        assert_eq!(v1["usage"]["prompt_tokens"], 2000, "upstream total preserved");
        assert_eq!(v1["usage"]["cache_read_input_tokens"], 0);
        assert_eq!(v1["usage"]["cache_creation_input_tokens"], 1500);
        assert_eq!(v1["usage"]["prompt_tokens_details"]["cached_tokens"], 0);

        // The commit is spawned — poll the index until the write lands (no sleep).
        let scope = IndexScope {
            principal_id: user.id,
            virtual_model: ALIAS.into(),
            tokenizer_version: TOK_VER.into(),
        };
        let hash = parse_chat_completions(&serde_json::to_vec(&body()).unwrap(), &all_tiers(), &TelemetryPolicy::default())
            .unwrap()
            .cumulative_hashes[0]
            .clone();
        let idx = PostgresIndex::new(pool.clone(), 1);
        await_commit(&idx, &scope, &hash, "the write should have committed after a 2xx").await;

        // Second identical request → now a read hit on the committed prefix.
        let r2 = server
            .post("/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", key.secret))
            .json(&body())
            .await;
        let v2: serde_json::Value = r2.json();
        assert_eq!(
            v2["usage"]["cache_read_input_tokens"], 1500,
            "second request reads the cached prefix"
        );
        assert_eq!(v2["usage"]["cache_creation_input_tokens"], 0);
    }

    /// Streaming stand-in: an SSE chat completion with a delta, a terminal usage frame, and [DONE].
    async fn mock_upstream_streaming() -> Response {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                   data: {\"choices\":[],\"usage\":{\"prompt_tokens\":2000,\"completion_tokens\":2,\"total_tokens\":2002}}\n\n\
                   data: [DONE]\n\n";
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(sse))
            .unwrap()
    }

    fn body_streaming() -> serde_json::Value {
        serde_json::json!({
            "model": ALIAS,
            "stream": true,
            "messages": [
                {"role":"system","content":[{"type":"text","text":"static system","cache_control":{"type":"ephemeral","ttl":"1h"}}]},
                {"role":"user","content":"hi"}
            ]
        })
    }

    #[sqlx::test]
    async fn streaming_defers_classify_then_injects_and_commits(pool: PgPool) {
        let user = create_test_user(&pool, Role::StandardUser).await;
        let key = create_test_api_key_for_user(&pool, user.id).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let id = create_test_model(&pool, "m", ALIAS, endpoint, user.id).await;
        sqlx::query!(
            r#"INSERT INTO model_cache_tariffs
                 (deployed_model_id, write_multiplier_5m, write_multiplier_1h, write_multiplier_24h, min_prefix_tokens)
               VALUES ($1, 1.25, 2.0, 2.5, 1024)"#,
            id
        )
        .execute(&pool)
        .await
        .unwrap();

        let tok = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{"alias": ALIAS, "hf_repo": "o/m", "tokenizer_version": TOK_VER}]
            })))
            .mount(&tok)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/tokenize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "virtual_model": ALIAS, "tokenizer_version": TOK_VER,
                "segment_counts": [1500], "cumulative": [1500], "total": 1500
            })))
            .mount(&tok)
            .await;

        let classifier = Classifier::new(
            PrincipalResolver::new(pool.clone()),
            ModelConfigResolver::new(pool.clone()),
            TokenizerClient::new(tok.uri()),
            Arc::new(PostgresIndex::new(pool.clone(), 1)),
            all_tiers(),
            TelemetryPolicy::default(),
            false,
        );
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_upstream_streaming))
            .layer(from_fn_with_state(
                CacheLayerState::new(classifier, usize::MAX, Duration::from_secs(5)),
                cache_middleware,
            ));
        let server = axum_test::TestServer::new(app).unwrap();

        // First stream: the deferred classify resolves at the terminal usage frame, which is then
        // edited with the all-creation cache fields (deltas + [DONE] preserved).
        let r1 = server
            .post("/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", key.secret))
            .json(&body_streaming())
            .await;
        r1.assert_status_ok();
        let t1 = r1.text();
        assert!(t1.contains("\"cache_creation_input_tokens\":1500"), "creation injected: {t1}");
        assert!(t1.contains("\"cache_read_input_tokens\":0"), "no read on first sight: {t1}");
        assert!(t1.contains("data: [DONE]"), "DONE preserved: {t1}");
        assert!(t1.contains("\"content\":\"hi\""), "delta preserved: {t1}");

        // The write commits after the stream drains successfully.
        let scope = IndexScope {
            principal_id: user.id,
            virtual_model: ALIAS.into(),
            tokenizer_version: TOK_VER.into(),
        };
        let hash = parse_chat_completions(
            &serde_json::to_vec(&body_streaming()).unwrap(),
            &all_tiers(),
            &TelemetryPolicy::default(),
        )
        .unwrap()
        .cumulative_hashes[0]
            .clone();
        let idx = PostgresIndex::new(pool.clone(), 1);
        await_commit(&idx, &scope, &hash, "streaming write commits after a clean usage frame").await;

        // Second identical stream → a read hit, injected into the terminal frame.
        let r2 = server
            .post("/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", key.secret))
            .json(&body_streaming())
            .await;
        let t2 = r2.text();
        assert!(
            t2.contains("\"cache_read_input_tokens\":1500"),
            "second stream reads the prefix: {t2}"
        );
        assert!(t2.contains("\"cache_creation_input_tokens\":0"), "no creation on a read: {t2}");
    }

    /// Streaming stand-in that fails mid-stream: a delta, then an error frame, and NO usage frame.
    async fn mock_upstream_streaming_error() -> Response {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                   data: {\"error\":{\"message\":\"upstream exploded\"}}\n\n";
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(sse))
            .unwrap()
    }

    #[sqlx::test]
    async fn streaming_error_frame_vetoes_the_write(pool: PgPool) {
        let user = create_test_user(&pool, Role::StandardUser).await;
        let key = create_test_api_key_for_user(&pool, user.id).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let id = create_test_model(&pool, "m", ALIAS, endpoint, user.id).await;
        sqlx::query!(
            r#"INSERT INTO model_cache_tariffs
                 (deployed_model_id, write_multiplier_5m, write_multiplier_1h, write_multiplier_24h, min_prefix_tokens)
               VALUES ($1, 1.25, 2.0, 2.5, 1024)"#,
            id
        )
        .execute(&pool)
        .await
        .unwrap();

        let tok = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{"alias": ALIAS, "hf_repo": "o/m", "tokenizer_version": TOK_VER}]
            })))
            .mount(&tok)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/tokenize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "virtual_model": ALIAS, "tokenizer_version": TOK_VER,
                "segment_counts": [1500], "cumulative": [1500], "total": 1500
            })))
            .mount(&tok)
            .await;

        let classifier = Classifier::new(
            PrincipalResolver::new(pool.clone()),
            ModelConfigResolver::new(pool.clone()),
            TokenizerClient::new(tok.uri()),
            Arc::new(PostgresIndex::new(pool.clone(), 1)),
            all_tiers(),
            TelemetryPolicy::default(),
            false,
        );
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_upstream_streaming_error))
            .layer(from_fn_with_state(
                CacheLayerState::new(classifier, usize::MAX, Duration::from_secs(5)),
                cache_middleware,
            ));
        let server = axum_test::TestServer::new(app).unwrap();

        // Drain the stream: a mid-stream error frame and no usage frame → veto.
        let r = server
            .post("/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", key.secret))
            .json(&body_streaming())
            .await;
        let _ = r.text();

        // An unbilled stream must NOT seed the cache. Give any (erroneously) spawned commit ample
        // chance to land, then assert the index stayed empty.
        let scope = IndexScope {
            principal_id: user.id,
            virtual_model: ALIAS.into(),
            tokenizer_version: TOK_VER.into(),
        };
        let hash = parse_chat_completions(
            &serde_json::to_vec(&body_streaming()).unwrap(),
            &all_tiers(),
            &TelemetryPolicy::default(),
        )
        .unwrap()
        .cumulative_hashes[0]
            .clone();
        let idx = PostgresIndex::new(pool.clone(), 1);
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(
            idx.lookup(&scope, std::slice::from_ref(&hash)).await.unwrap().is_empty(),
            "an unbilled stream (error frame, no usage) must not commit a write"
        );
    }

    #[sqlx::test]
    async fn non_cacheable_path_passes_through(pool: PgPool) {
        // /v1/embeddings is not cacheable → no body editing, no cache fields.
        let classifier = Classifier::new(
            PrincipalResolver::new(pool.clone()),
            ModelConfigResolver::new(pool.clone()),
            TokenizerClient::new("http://127.0.0.1:1"),
            Arc::new(PostgresIndex::new(pool.clone(), 1)),
            all_tiers(),
            TelemetryPolicy::default(),
            false,
        );
        let app = Router::new().route("/v1/embeddings", post(mock_upstream)).layer(from_fn_with_state(
            CacheLayerState::new(classifier, usize::MAX, Duration::from_secs(5)),
            cache_middleware,
        ));
        let server = axum_test::TestServer::new(app).unwrap();
        let r = server
            .post("/v1/embeddings")
            .json(&serde_json::json!({"model": "x", "input": "hi"}))
            .await;
        let v: serde_json::Value = r.json();
        assert!(v["usage"].get("cache_read_input_tokens").is_none());
    }

    #[sqlx::test]
    async fn disabled_tier_marker_rejected_with_400(pool: PgPool) {
        // Policy enables only 5m; a request carrying a 24h marker must be rejected up front
        // (before forwarding) with a clear 400 — not silently un-cached.
        let classifier = Classifier::new(
            PrincipalResolver::new(pool.clone()),
            ModelConfigResolver::new(pool.clone()),
            TokenizerClient::new("http://127.0.0.1:1"),
            Arc::new(PostgresIndex::new(pool.clone(), 1)),
            TierPolicy::from_config(&["5m".to_string()], "5m"),
            TelemetryPolicy::default(),
            false,
        );
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_upstream)) // must NOT be reached
            .layer(from_fn_with_state(
                CacheLayerState::new(classifier, usize::MAX, Duration::from_secs(5)),
                cache_middleware,
            ));
        let server = axum_test::TestServer::new(app).unwrap();

        let r = server
            .post("/v1/chat/completions")
            .add_header("authorization", "Bearer anything")
            .json(&serde_json::json!({
                "model": ALIAS,
                "messages": [{"role": "system", "content": [
                    {"type": "text", "text": "x", "cache_control": {"type": "ephemeral", "ttl": "24h"}}
                ]}]
            }))
            .await;

        r.assert_status(StatusCode::BAD_REQUEST);
        let v: serde_json::Value = r.json();
        assert_eq!(v["error"]["code"], "invalid_cache_control");
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(msg.contains("24h"), "message names the rejected tier: {msg}");
        assert!(msg.contains("available tiers: 5m"), "message names the available tiers: {msg}");
    }

    /// Upstream stand-in that reports ITS OWN cache accounting — the OpenRouter implicit-cache
    /// shape observed in prod on kimi-k3 (provider cached 687 of 985 prompt tokens on a model
    /// with no cache tariff, i.e. billed at full price).
    /// Sees the response the way outlet does: extensions cloned at head time, read after
    /// the body has been consumed.
    type Observed = Arc<std::sync::Mutex<Option<UpstreamCachedTokens>>>;
    async fn observe_upstream_cached(
        axum::extract::State(slot): axum::extract::State<Observed>,
        req: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> Response {
        let response = next.run(req).await;
        *slot.lock().unwrap() = response.extensions().get::<UpstreamCachedTokens>().cloned();
        response
    }

    async fn mock_upstream_with_provider_cache() -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "id": "chatcmpl-1", "object": "chat.completion",
            "choices": [{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
            "usage": {
                "prompt_tokens": 985, "completion_tokens": 2, "total_tokens": 987,
                "prompt_tokens_details": {"cached_tokens": 687},
                "cache_read_input_tokens": 687
            }
        }))
    }

    #[sqlx::test]
    async fn inactive_model_scrubs_provider_cache_fields(pool: PgPool) {
        // No deployed model / cache tariff → classify resolves inactive. The provider's own
        // cache accounting must still be scrubbed: the customer is billed full price, so a
        // forwarded `cached_tokens: 687` would read as a discount we didn't give.
        let user = create_test_user(&pool, Role::StandardUser).await;
        let key = create_test_api_key_for_user(&pool, user.id).await;
        let classifier = Classifier::new(
            PrincipalResolver::new(pool.clone()),
            ModelConfigResolver::new(pool.clone()),
            TokenizerClient::new("http://127.0.0.1:1"),
            Arc::new(PostgresIndex::new(pool.clone(), 1)),
            all_tiers(),
            TelemetryPolicy::default(),
            false,
        );
        let observed: Observed = Default::default();
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_upstream_with_provider_cache))
            .layer(from_fn_with_state(
                CacheLayerState::new(classifier, usize::MAX, Duration::from_secs(5)),
                cache_middleware,
            ))
            .layer(from_fn_with_state(observed.clone(), observe_upstream_cached));
        let server = axum_test::TestServer::new(app).unwrap();

        let r = server
            .post("/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", key.secret))
            .json(&serde_json::json!({"model": ALIAS, "messages": [{"role":"user","content":"hi"}]}))
            .await;
        r.assert_status_ok();
        let v: serde_json::Value = r.json();
        assert_eq!(v["usage"]["prompt_tokens"], 985, "token totals untouched");
        assert_eq!(v["usage"]["prompt_tokens_details"]["cached_tokens"], 0, "provider hit zeroed");
        assert!(
            v["usage"].get("cache_read_input_tokens").is_none(),
            "provider extension field removed: {v}"
        );
        let cell = observed.lock().unwrap().clone().expect("cell inserted at head time");
        assert_eq!(cell.get(), Some(687), "the engine's hit survives for analytics");
    }

    /// Streaming variant of [`mock_upstream_with_provider_cache`].
    async fn mock_upstream_streaming_with_provider_cache() -> Response {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                   data: {\"choices\":[],\"usage\":{\"prompt_tokens\":985,\"completion_tokens\":2,\"total_tokens\":987,\"prompt_tokens_details\":{\"cached_tokens\":687}}}\n\n\
                   data: [DONE]\n\n";
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(sse))
            .unwrap()
    }

    #[sqlx::test]
    async fn inactive_model_scrubs_streaming_terminal_frame(pool: PgPool) {
        let user = create_test_user(&pool, Role::StandardUser).await;
        let key = create_test_api_key_for_user(&pool, user.id).await;
        let classifier = Classifier::new(
            PrincipalResolver::new(pool.clone()),
            ModelConfigResolver::new(pool.clone()),
            TokenizerClient::new("http://127.0.0.1:1"),
            Arc::new(PostgresIndex::new(pool.clone(), 1)),
            all_tiers(),
            TelemetryPolicy::default(),
            false,
        );
        let observed: Observed = Default::default();
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_upstream_streaming_with_provider_cache))
            .layer(from_fn_with_state(
                CacheLayerState::new(classifier, usize::MAX, Duration::from_secs(5)),
                cache_middleware,
            ))
            .layer(from_fn_with_state(observed.clone(), observe_upstream_cached));
        let server = axum_test::TestServer::new(app).unwrap();

        let r = server
            .post("/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", key.secret))
            .json(&serde_json::json!({"model": ALIAS, "stream": true, "messages": [{"role":"user","content":"hi"}]}))
            .await;
        r.assert_status_ok();
        let t = r.text();
        assert!(t.contains("\"cached_tokens\":0"), "provider hit zeroed in terminal frame: {t}");
        assert!(!t.contains("687"), "provider value gone: {t}");
        assert!(t.contains("\"content\":\"hi\""), "delta preserved: {t}");
        assert!(t.contains("data: [DONE]"), "DONE preserved: {t}");
        let cell = observed.lock().unwrap().clone().expect("cell inserted at head time");
        assert_eq!(cell.get(), Some(687), "filled at the terminal frame, after the head was cloned");
    }

    // ---- engine-cache passthrough (implicit caching on tariffed models) ----

    /// Insert the tariff row that cache-activates `ALIAS` for a passthrough test.
    async fn activate_alias(pool: &PgPool, user: uuid::Uuid) {
        let endpoint = create_test_endpoint(pool, "ep", user).await;
        let id = create_test_model(pool, "m", ALIAS, endpoint, user).await;
        sqlx::query!(
            r#"INSERT INTO model_cache_tariffs
                 (deployed_model_id, write_multiplier_5m, write_multiplier_1h, write_multiplier_24h, min_prefix_tokens)
               VALUES ($1, 1.25, 2.0, 2.5, 1024)"#,
            id
        )
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    async fn tariffed_model_passes_engine_cache_through_for_unmarked_requests(pool: PgPool) {
        // A tariff row alone (no markers, tokenizer deliberately unreachable — the model
        // needn't be onboarded to tokenizer-svc) buys implicit caching: the engine's own
        // reported hit becomes the billed, customer-visible read. Creations stay zero —
        // implicit caching has no write concept and no premium.
        let user = create_test_user(&pool, Role::StandardUser).await;
        let key = create_test_api_key_for_user(&pool, user.id).await;
        activate_alias(&pool, user.id).await;
        let classifier = Classifier::new(
            PrincipalResolver::new(pool.clone()),
            ModelConfigResolver::new(pool.clone()),
            TokenizerClient::new("http://127.0.0.1:1"),
            Arc::new(PostgresIndex::new(pool.clone(), 1)),
            all_tiers(),
            TelemetryPolicy::default(),
            false,
        );
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_upstream_with_provider_cache))
            .layer(from_fn_with_state(
                CacheLayerState::new(classifier, usize::MAX, Duration::from_secs(5)),
                cache_middleware,
            ));
        let server = axum_test::TestServer::new(app).unwrap();

        let r = server
            .post("/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", key.secret))
            .json(&serde_json::json!({"model": ALIAS, "messages": [{"role":"user","content":"hi"}]}))
            .await;
        r.assert_status_ok();
        let v: serde_json::Value = r.json();
        assert_eq!(v["usage"]["prompt_tokens"], 985, "token totals untouched");
        assert_eq!(
            v["usage"]["prompt_tokens_details"]["cached_tokens"], 687,
            "engine hit passed through"
        );
        assert_eq!(v["usage"]["cache_read_input_tokens"], 687, "billed read = engine hit");
        assert_eq!(v["usage"]["cache_creation_input_tokens"], 0, "implicit caching never writes");
    }

    #[sqlx::test]
    async fn tariffed_model_passes_engine_cache_through_on_streams(pool: PgPool) {
        let user = create_test_user(&pool, Role::StandardUser).await;
        let key = create_test_api_key_for_user(&pool, user.id).await;
        activate_alias(&pool, user.id).await;
        let classifier = Classifier::new(
            PrincipalResolver::new(pool.clone()),
            ModelConfigResolver::new(pool.clone()),
            TokenizerClient::new("http://127.0.0.1:1"),
            Arc::new(PostgresIndex::new(pool.clone(), 1)),
            all_tiers(),
            TelemetryPolicy::default(),
            false,
        );
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_upstream_streaming_with_provider_cache))
            .layer(from_fn_with_state(
                CacheLayerState::new(classifier, usize::MAX, Duration::from_secs(5)),
                cache_middleware,
            ));
        let server = axum_test::TestServer::new(app).unwrap();

        let r = server
            .post("/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", key.secret))
            .json(&serde_json::json!({"model": ALIAS, "stream": true, "messages": [{"role":"user","content":"hi"}]}))
            .await;
        r.assert_status_ok();
        let t = r.text();
        assert!(t.contains("\"cached_tokens\":687"), "engine hit kept in terminal frame: {t}");
        assert!(t.contains("\"cache_read_input_tokens\":687"), "billed read = engine hit: {t}");
        assert!(t.contains("\"cache_creation_input_tokens\":0"), "no writes on implicit: {t}");
        assert!(t.contains("\"content\":\"hi\""), "delta preserved: {t}");
        assert!(t.contains("data: [DONE]"), "DONE preserved: {t}");
    }

    // ---- plain /completions (implicit-only by construction) ----

    /// Completions-shaped upstream reporting ITS OWN cache hit — the leak shape observed
    /// in prod on /completions before the layer covered the endpoint.
    async fn mock_upstream_completions_with_provider_cache() -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "id": "cmpl-1", "object": "text_completion",
            "model": "m",
            "choices": [{"index":0,"text":" pong","finish_reason":"stop"}],
            "usage": {
                "prompt_tokens": 2008, "completion_tokens": 8, "total_tokens": 2016,
                "prompt_tokens_details": {"cached_tokens": 1792}
            }
        }))
    }

    fn completions_app(pool: &PgPool) -> axum_test::TestServer {
        let classifier = Classifier::new(
            PrincipalResolver::new(pool.clone()),
            ModelConfigResolver::new(pool.clone()),
            TokenizerClient::new("http://127.0.0.1:1"),
            Arc::new(PostgresIndex::new(pool.clone(), 1)),
            all_tiers(),
            TelemetryPolicy::default(),
            false,
        );
        let app = Router::new()
            .route("/v1/completions", post(mock_upstream_completions_with_provider_cache))
            .layer(from_fn_with_state(
                CacheLayerState::new(classifier, usize::MAX, Duration::from_secs(5)),
                cache_middleware,
            ));
        axum_test::TestServer::new(app).unwrap()
    }

    #[sqlx::test]
    async fn tariffed_model_passes_engine_cache_through_on_plain_completions(pool: PgPool) {
        // A string prompt has no blocks for markers to bind to — /completions is
        // implicit-only, and a tariff row alone activates it.
        let user = create_test_user(&pool, Role::StandardUser).await;
        let key = create_test_api_key_for_user(&pool, user.id).await;
        activate_alias(&pool, user.id).await;
        let server = completions_app(&pool);

        let r = server
            .post("/v1/completions")
            .add_header("authorization", format!("Bearer {}", key.secret))
            .json(&serde_json::json!({"model": ALIAS, "prompt": "continue this"}))
            .await;
        r.assert_status_ok();
        let v: serde_json::Value = r.json();
        assert_eq!(v["usage"]["prompt_tokens"], 2008, "token totals untouched");
        assert_eq!(
            v["usage"]["prompt_tokens_details"]["cached_tokens"], 1792,
            "engine hit passed through"
        );
        assert_eq!(v["usage"]["cache_read_input_tokens"], 1792, "billed read = engine hit");
        assert_eq!(v["usage"]["cache_creation_input_tokens"], 0, "implicit never writes");
    }

    #[sqlx::test]
    async fn marked_plain_completions_stay_deterministically_zero(pool: PgPool) {
        // A top-level cache_control on a blockless body arms the request (one paradigm)
        // but the automatic marker no-ops with nothing to bind to — deterministic zeros,
        // engine hit ignored, no 400.
        let user = create_test_user(&pool, Role::StandardUser).await;
        let key = create_test_api_key_for_user(&pool, user.id).await;
        activate_alias(&pool, user.id).await;
        let server = completions_app(&pool);

        let r = server
            .post("/v1/completions")
            .add_header("authorization", format!("Bearer {}", key.secret))
            .json(&serde_json::json!({
                "model": ALIAS, "prompt": "continue this",
                "cache_control": {"type": "ephemeral", "ttl": "1h"}
            }))
            .await;
        r.assert_status_ok();
        let v: serde_json::Value = r.json();
        assert_eq!(v["usage"]["prompt_tokens_details"]["cached_tokens"], 0, "armed → explicit zeros");
        assert_eq!(v["usage"]["cache_read_input_tokens"], 0, "engine hit not billed when armed");
    }

    #[sqlx::test]
    async fn untariffed_plain_completions_scrub_provider_cache_fields(pool: PgPool) {
        // The prod leak this closes: no tariff → inactive → the upstream's own
        // cached_tokens must be zeroed, not shown to a customer billed at full price.
        let user = create_test_user(&pool, Role::StandardUser).await;
        let key = create_test_api_key_for_user(&pool, user.id).await;
        let server = completions_app(&pool);

        let r = server
            .post("/v1/completions")
            .add_header("authorization", format!("Bearer {}", key.secret))
            .json(&serde_json::json!({"model": ALIAS, "prompt": "continue this"}))
            .await;
        r.assert_status_ok();
        let v: serde_json::Value = r.json();
        assert_eq!(v["usage"]["prompt_tokens_details"]["cached_tokens"], 0, "provider hit zeroed");
        assert!(
            v["usage"].get("cache_read_input_tokens").is_none(),
            "no injected fields when inactive"
        );
    }

    #[test]
    fn is_cacheable_matches_the_onwards_completions_surface() {
        let req = |method: Method, path: &str| {
            let mut r = Request::new(Body::empty());
            *r.method_mut() = method;
            *r.uri_mut() = path.parse().unwrap();
            r
        };
        assert!(is_cacheable(&req(Method::POST, "/v1/chat/completions")));
        assert!(is_cacheable(&req(Method::POST, "/v1/completions")));
        // Trailing slash: onwards normalizes it as the same route, so must we.
        assert!(is_cacheable(&req(Method::POST, "/v1/completions/")));
        assert!(is_cacheable(&req(Method::POST, "/v1/chat/completions/")));
        assert!(!is_cacheable(&req(Method::GET, "/v1/completions")));
        assert!(!is_cacheable(&req(Method::POST, "/v1/embeddings")));
    }

    /// Completions upstream that also proves the query param never leaks upstream.
    async fn mock_upstream_completions_asserting_no_query(req: Request) -> Json<serde_json::Value> {
        assert!(req.uri().query().is_none(), "cacheBreakpoint must be stripped before forwarding");
        mock_upstream_completions_with_provider_cache().await
    }

    #[sqlx::test]
    async fn cache_breakpoint_param_is_ignored_on_plain_completions(pool: PgPool) {
        // The param is a Chat Completions feature: on /completions it must be stripped
        // (never forwarded) WITHOUT arming the request, so implicit billing still applies.
        let user = create_test_user(&pool, Role::StandardUser).await;
        let key = create_test_api_key_for_user(&pool, user.id).await;
        activate_alias(&pool, user.id).await;
        let classifier = Classifier::new(
            PrincipalResolver::new(pool.clone()),
            ModelConfigResolver::new(pool.clone()),
            TokenizerClient::new("http://127.0.0.1:1"),
            Arc::new(PostgresIndex::new(pool.clone(), 1)),
            all_tiers(),
            TelemetryPolicy::default(),
            false,
        );
        let app = Router::new()
            .route("/v1/completions", post(mock_upstream_completions_asserting_no_query))
            .layer(from_fn_with_state(
                CacheLayerState::new(classifier, usize::MAX, Duration::from_secs(5)),
                cache_middleware,
            ));
        let server = axum_test::TestServer::new(app).unwrap();

        let r = server
            .post("/v1/completions")
            .add_query_param("cacheBreakpoint", "lastUserMessage")
            .add_header("authorization", format!("Bearer {}", key.secret))
            .json(&serde_json::json!({"model": ALIAS, "prompt": "continue this"}))
            .await;
        r.assert_status_ok();
        let v: serde_json::Value = r.json();
        assert_eq!(v["usage"]["cache_read_input_tokens"], 1792, "param must not suppress implicit");
        assert_eq!(v["usage"]["prompt_tokens_details"]["cached_tokens"], 1792);
    }

    // ---- `?cacheBreakpoint=lastUserMessage` (query-param automatic caching) ----

    /// A body with NO cache_control anywhere — the shape the proxy customer sends.
    fn body_unmarked() -> serde_json::Value {
        serde_json::json!({
            "model": ALIAS,
            "messages": [
                {"role": "system", "content": "static system"},
                {"role": "user", "content": "hi"}
            ]
        })
    }

    /// Tokenizer mocks sized for [`body_unmarked`]'s TWO blocks (the automatic breakpoint's write
    /// span covers the whole request, unlike the single-marked-block mocks above).
    async fn mount_tokenizer_two_segments(tok: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{"alias": ALIAS, "hf_repo": "o/m", "tokenizer_version": TOK_VER}]
            })))
            .mount(tok)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/tokenize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "virtual_model": ALIAS, "tokenizer_version": TOK_VER,
                "segment_counts": [1500, 10], "cumulative": [1500, 1510], "total": 1510
            })))
            .mount(tok)
            .await;
    }

    #[sqlx::test]
    async fn query_param_end_to_end_creates_then_reads(pool: PgPool) {
        query_param_cache_round_trip(pool, "/v1/chat/completions", false).await;
    }

    #[sqlx::test]
    async fn responses_query_param_bills_cache_writes_and_reads(pool: PgPool) {
        query_param_cache_round_trip(pool, "/v1/responses", false).await;
    }

    #[sqlx::test]
    async fn responses_streaming_query_param_bills_cache_writes_and_reads(pool: PgPool) {
        query_param_cache_round_trip(pool, "/v1/responses", true).await;
    }

    type ObservedExtensions = Arc<Mutex<Option<Extensions>>>;

    async fn observe_cache_billing(State(slot): State<ObservedExtensions>, req: Request, next: Next) -> Response {
        let response = next.run(req).await;
        let capture = response
            .extensions()
            .get::<CacheBilling>()
            .expect("billing metadata survives translation");
        if is_streaming(&response) {
            assert!(capture.get().is_none(), "streaming counts arrive after the response head");
        }
        *slot.lock().unwrap() = Some(response.extensions().clone());
        response
    }

    /// Exercise translation -> cache -> upstream -> translation -> billing with
    /// an actual cache-index write followed by a read of the same prefix.
    async fn query_param_cache_round_trip(pool: PgPool, route: &str, streaming: bool) {
        // The customer's whole flow: an unmarked body + the query param behaves exactly like
        // top-level automatic caching — first request writes the full conversation prefix,
        // an identical follow-up reads it.
        let user = create_test_user(&pool, Role::StandardUser).await;
        let key = create_test_api_key_for_user(&pool, user.id).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let id = create_test_model(&pool, "m", ALIAS, endpoint, user.id).await;
        sqlx::query!(
            r#"INSERT INTO model_cache_tariffs
                 (deployed_model_id, write_multiplier_5m, write_multiplier_1h, write_multiplier_24h, min_prefix_tokens)
               VALUES ($1, 1.25, 2.0, 2.5, 1024)"#,
            id
        )
        .execute(&pool)
        .await
        .unwrap();

        let tok = MockServer::start().await;
        mount_tokenizer_two_segments(&tok).await;

        let classifier = Classifier::new(
            PrincipalResolver::new(pool.clone()),
            ModelConfigResolver::new(pool.clone()),
            TokenizerClient::new(tok.uri()),
            Arc::new(PostgresIndex::new(pool.clone(), 1)),
            all_tiers(),
            TelemetryPolicy::default(),
            false,
        );
        let observed: ObservedExtensions = Default::default();
        let app = Router::new()
            .route(route, post(mock_cache_billing_upstream))
            .layer(from_fn_with_state(
                CacheLayerState::new(classifier, usize::MAX, Duration::from_secs(5)),
                cache_middleware,
            ))
            .layer(from_fn_with_state(
                TranslationRegistry::new(vec![Arc::new(OpenResponses::new())]),
                translation_middleware,
            ))
            .layer(from_fn_with_state(observed.clone(), observe_cache_billing));
        let server = axum_test::TestServer::new(app).unwrap();
        let request_body = if route.ends_with("/responses") {
            serde_json::json!({"model": ALIAS, "instructions": "static system", "input": "hi", "stream": streaming})
        } else {
            body_unmarked()
        };

        let r1 = server
            .post(route)
            .add_query_param("cacheBreakpoint", "lastUserMessage")
            .add_header("authorization", format!("Bearer {}", key.secret))
            .json(&request_body)
            .await;
        r1.assert_status_ok();
        // The write leg is EXPLICIT caching in action (creation billed at its premium) — the
        // upstream's 777 engine-cached tokens are deliberately ignored, one paradigm per request.
        // 490 uncached + 1510 * 2 (1h write) + 2 * 3 (output).
        assert_cache_billing(&r1.text(), observed.lock().unwrap().clone().unwrap(), 0, 1510, Decimal::from(3516));

        // The write lands at the LAST block's cumulative hash (markers never enter the hash, so
        // the unmarked body parses to the same hashes).
        let scope = IndexScope {
            principal_id: user.id,
            virtual_model: ALIAS.into(),
            tokenizer_version: TOK_VER.into(),
        };
        let hash = parse_chat_completions(
            &serde_json::to_vec(&body_unmarked()).unwrap(),
            &all_tiers(),
            &TelemetryPolicy::default(),
        )
        .unwrap()
        .cumulative_hashes[1]
            .clone();
        let idx = PostgresIndex::new(pool.clone(), 1);
        await_commit(&idx, &scope, &hash, "the query-param write should commit after a 2xx").await;

        let r2 = server
            .post(route)
            .add_query_param("cacheBreakpoint", "lastUserMessage")
            .add_header("authorization", format!("Bearer {}", key.secret))
            .json(&request_body)
            .await;
        r2.assert_status_ok();
        // The read leg bills the module's smoothed 1510, not the engine's 777 — armed
        // requests are the module's alone.
        // 490 uncached + 1510 * 0.1 (read) + 2 * 3 (output).
        assert_cache_billing(&r2.text(), observed.lock().unwrap().clone().unwrap(), 1510, 0, Decimal::from(647));
    }

    async fn mock_cache_billing_upstream(req: Request) -> Response {
        assert_eq!(req.uri().path(), "/v1/chat/completions");
        assert!(req.uri().query().is_none(), "cache query parameter must not reach upstream");
        let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
        let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(request.get("cache_control").is_none());
        assert!(request.get("input").is_none());
        let usage = serde_json::json!({
            "prompt_tokens": 2000, "completion_tokens": 2, "total_tokens": 2002,
            "prompt_tokens_details": {"cached_tokens": 777}
        });
        if request["stream"] == true {
            let chunk = serde_json::json!({
                "id": "c1", "object": "chat.completion.chunk", "created": 0, "model": ALIAS,
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
                "usage": usage
            });
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                format!("data: {chunk}\n\ndata: [DONE]\n\n"),
            )
                .into_response()
        } else {
            Json(serde_json::json!({
                "id": "c1", "object": "chat.completion", "created": 0, "model": ALIAS,
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
                "usage": usage
            }))
            .into_response()
        }
    }

    fn assert_cache_billing(body: &str, extensions: Extensions, read: i64, creation: i64, expected_cost: Decimal) {
        assert_eq!(extensions.get::<UpstreamCachedTokens>().and_then(|c| c.get()), Some(777));
        let response = ResponseData {
            extensions,
            correlation_id: 1,
            timestamp: std::time::SystemTime::now(),
            status: StatusCode::OK,
            headers: Default::default(),
            body: Some(body.to_string().into()),
            duration: Duration::ZERO,
            duration_to_first_byte: Duration::ZERO,
        };
        let cache = extract_cache_tokens(&response);
        assert_eq!(
            (cache.read, cache.creation_5m, cache.creation_1h, cache.creation_24h),
            (read, 0, creation, 0)
        );
        let displayed = extract_from_last_usage(&response, |usage| {
            usage
                .pointer("/input_tokens_details/cached_tokens")
                .or_else(|| usage.pointer("/prompt_tokens_details/cached_tokens"))
                .and_then(serde_json::Value::as_i64)
        });
        assert_eq!(displayed, Some(read), "displayed reads must match billed reads");
        extract_from_last_usage(&response, |usage| {
            if let Some(details) = usage.get("input_tokens_details") {
                assert_eq!(details["cache_write_tokens"], creation);
                for field in ["cache_read_input_tokens", "cache_creation_input_tokens", "cache_creation"] {
                    assert!(usage.get(field).is_none(), "Responses must not expose {field}");
                }
            } else {
                assert_eq!(usage["cache_read_input_tokens"], read, "Chat Completions stays compatible");
                assert_eq!(usage["cache_creation"]["ephemeral_1h_input_tokens"], creation);
            }
        });
        let tokens = extract_from_last_usage(&response, raw_usage_tokens).expect("response usage");
        let counts = TokenCounts {
            prompt: tokens.prompt,
            completion: tokens.completion,
            cache_read: cache.read,
            cache_creation_5m: cache.creation_5m,
            cache_creation_1h: cache.creation_1h,
            cache_creation_24h: cache.creation_24h,
        };
        let cost = charged_cost(
            &counts,
            Some(ALIAS),
            Some(Decimal::ONE),
            Some(Decimal::from(3)),
            Some(CacheMultipliers::default()),
            ANALYTICS_BATCHER,
        );
        assert_eq!(cost, Some(expected_cost));
    }

    /// Upstream stand-in that echoes what it received (URI query + whether the body still carried
    /// any `cache_control`), so strip behavior is assertable end-to-end.
    async fn mock_upstream_echoing(req: Request) -> Json<serde_json::Value> {
        let (req_parts, body) = req.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let body_has_cache_control = String::from_utf8_lossy(&bytes).contains("cache_control");
        Json(serde_json::json!({
            "id": "chatcmpl-1", "object": "chat.completion",
            "choices": [{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12},
            "echo": {"query": req_parts.uri.query(), "body_has_cache_control": body_has_cache_control}
        }))
    }

    #[sqlx::test]
    async fn query_param_stripped_before_upstream_others_preserved(pool: PgPool) {
        // Neither the param (onwards forwards path_and_query verbatim) nor the injected marker
        // may leak upstream; unrelated query params must survive.
        let classifier = Classifier::new(
            PrincipalResolver::new(pool.clone()),
            ModelConfigResolver::new(pool.clone()),
            TokenizerClient::new("http://127.0.0.1:1"),
            Arc::new(PostgresIndex::new(pool.clone(), 1)),
            all_tiers(),
            TelemetryPolicy::default(),
            false,
        );
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_upstream_echoing))
            .layer(from_fn_with_state(
                CacheLayerState::new(classifier, usize::MAX, Duration::from_secs(5)),
                cache_middleware,
            ));
        let server = axum_test::TestServer::new(app).unwrap();

        let r = server
            .post("/v1/chat/completions")
            .add_query_param("foo", "bar")
            .add_query_param("cacheBreakpoint", "lastUserMessage")
            .json(&body_unmarked())
            .await;
        r.assert_status_ok();
        let v: serde_json::Value = r.json();
        assert_eq!(v["echo"]["query"], "foo=bar", "param stripped, others preserved");
        assert_eq!(v["echo"]["body_has_cache_control"], false, "injected marker stripped from the body");
    }

    #[sqlx::test]
    async fn query_param_invalid_value_rejected_400(pool: PgPool) {
        // Strict: a typo'd value is a 400 up front, never a silent no-cache.
        let classifier = Classifier::new(
            PrincipalResolver::new(pool.clone()),
            ModelConfigResolver::new(pool.clone()),
            TokenizerClient::new("http://127.0.0.1:1"),
            Arc::new(PostgresIndex::new(pool.clone(), 1)),
            all_tiers(),
            TelemetryPolicy::default(),
            false,
        );
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_upstream)) // must NOT be reached
            .layer(from_fn_with_state(
                CacheLayerState::new(classifier, usize::MAX, Duration::from_secs(5)),
                cache_middleware,
            ));
        let server = axum_test::TestServer::new(app).unwrap();

        let r = server
            .post("/v1/chat/completions")
            .add_query_param("cacheBreakpoint", "lastusermessage")
            .json(&body_unmarked())
            .await;
        r.assert_status(StatusCode::BAD_REQUEST);
        let v: serde_json::Value = r.json();
        assert_eq!(v["error"]["code"], "invalid_cache_breakpoint");
        assert_eq!(v["error"]["param"], "cacheBreakpoint");
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(msg.contains("lastusermessage"), "names the rejected value: {msg}");
        assert!(msg.contains("lastUserMessage"), "names the supported value: {msg}");
    }

    #[sqlx::test]
    async fn query_param_composes_with_explicit_markers(pool: PgPool) {
        let classifier = Classifier::new(
            PrincipalResolver::new(pool.clone()),
            ModelConfigResolver::new(pool.clone()),
            TokenizerClient::new("http://127.0.0.1:1"),
            Arc::new(PostgresIndex::new(pool.clone(), 1)),
            all_tiers(),
            TelemetryPolicy::default(),
            false,
        );
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_upstream_echoing))
            .layer(from_fn_with_state(
                CacheLayerState::new(classifier, usize::MAX, Duration::from_secs(5)),
                cache_middleware,
            ));
        let server = axum_test::TestServer::new(app).unwrap();

        // An explicit 5m marker on the LAST block conflicts with the injected 1h automatic
        // marker → the existing automatic-caching 400, exactly as if the client had sent the
        // top-level field itself.
        let conflicting = serde_json::json!({
            "model": ALIAS,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "q", "cache_control": {"type": "ephemeral", "ttl": "5m"}}
            ]}]
        });
        let r = server
            .post("/v1/chat/completions")
            .add_query_param("cacheBreakpoint", "lastUserMessage")
            .json(&conflicting)
            .await;
        r.assert_status(StatusCode::BAD_REQUEST);
        let v: serde_json::Value = r.json();
        assert_eq!(v["error"]["code"], "invalid_cache_control");
        assert!(v["error"]["message"].as_str().unwrap().contains("conflicts"));

        // But a body that EXPLICITLY opted in at the top level wins over the param: top-level 5m
        // + explicit 5m on the last block is the same-ttl no-op → 200. (If the param's 1h had
        // overwritten the body field, this would be the conflict 400 above.)
        let body_wins = serde_json::json!({
            "model": ALIAS,
            "cache_control": {"type": "ephemeral", "ttl": "5m"},
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "q", "cache_control": {"type": "ephemeral", "ttl": "5m"}}
            ]}]
        });
        let r = server
            .post("/v1/chat/completions")
            .add_query_param("cacheBreakpoint", "lastUserMessage")
            .json(&body_wins)
            .await;
        r.assert_status_ok();

        // An earlier (non-last-block) explicit marker simply composes: system layer + moving
        // frontier, two breakpoints, no error.
        let composing = serde_json::json!({
            "model": ALIAS,
            "messages": [
                {"role": "system", "content": [
                    {"type": "text", "text": "sys", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
                ]},
                {"role": "user", "content": "q"}
            ]
        });
        let r = server
            .post("/v1/chat/completions")
            .add_query_param("cacheBreakpoint", "lastUserMessage")
            .json(&composing)
            .await;
        r.assert_status_ok();
    }

    #[sqlx::test]
    async fn query_param_streaming_injects_terminal_frame(pool: PgPool) {
        // Streaming + param: the deferred classify path sees the injected marker and edits the
        // terminal usage frame, same as body-field automatic caching.
        let user = create_test_user(&pool, Role::StandardUser).await;
        let key = create_test_api_key_for_user(&pool, user.id).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let id = create_test_model(&pool, "m", ALIAS, endpoint, user.id).await;
        sqlx::query!(
            r#"INSERT INTO model_cache_tariffs
                 (deployed_model_id, write_multiplier_5m, write_multiplier_1h, write_multiplier_24h, min_prefix_tokens)
               VALUES ($1, 1.25, 2.0, 2.5, 1024)"#,
            id
        )
        .execute(&pool)
        .await
        .unwrap();

        let tok = MockServer::start().await;
        mount_tokenizer_two_segments(&tok).await;

        let classifier = Classifier::new(
            PrincipalResolver::new(pool.clone()),
            ModelConfigResolver::new(pool.clone()),
            TokenizerClient::new(tok.uri()),
            Arc::new(PostgresIndex::new(pool.clone(), 1)),
            all_tiers(),
            TelemetryPolicy::default(),
            false,
        );
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_upstream_streaming))
            .layer(from_fn_with_state(
                CacheLayerState::new(classifier, usize::MAX, Duration::from_secs(5)),
                cache_middleware,
            ));
        let server = axum_test::TestServer::new(app).unwrap();

        let mut body = body_unmarked();
        body["stream"] = serde_json::json!(true);
        let r = server
            .post("/v1/chat/completions")
            .add_query_param("cacheBreakpoint", "lastUserMessage")
            .add_header("authorization", format!("Bearer {}", key.secret))
            .json(&body)
            .await;
        r.assert_status_ok();
        let t = r.text();
        assert!(t.contains("\"cache_creation_input_tokens\":1510"), "creation injected: {t}");
        assert!(t.contains("data: [DONE]"), "DONE preserved: {t}");
    }
}
