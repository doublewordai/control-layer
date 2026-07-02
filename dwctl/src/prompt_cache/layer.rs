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
//! correlation id, no trait injected into onwards. Failures degrade to "no caching"
//! (the response is forwarded untouched); the commit is success-gated.
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
use super::inject::{inject_into_response_nonstreaming, scan_inject_sse, strip_cache_control};
use super::metrics as cache_metrics;
use super::parse::{ParseError, validate_markers};
use super::sse::SseBufferedStream;
use super::stats::CacheStats;

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
    pub fn new(classifier: Classifier, body_limit: usize) -> Self {
        Self {
            classifier,
            // Mirrors onwards' old `DEFAULT_CLASSIFY_DEADLINE`; only bites on an
            // index/tokenizer outage (classify normally finishes during generation).
            deadline: Duration::from_secs(5),
            body_limit,
        }
    }
}

/// v1: only chat-completions (the parser handles that body shape). Responses + others
/// pass straight through (tool-Responses per-step caching is a fast-follow).
fn is_cacheable(req: &Request) -> bool {
    req.method() == Method::POST && req.uri().path().ends_with("/chat/completions")
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
    let parsed_body = serde_json::from_slice::<serde_json::Value>(&body_bytes).ok();

    // Reject disallowed/malformed cache_control markers synchronously, before forking + forwarding
    // — a 400 like a bad parameter, NOT a silent no-cache, so the client learns immediately and
    // isn't billed full price thinking it cached. Cheap: walks the already-parsed Value, no hashing.
    if let Some(body) = &parsed_body
        && let Err(e) = validate_markers(body, state.classifier.tier_policy())
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
                })
                .await
        })
    });

    // Sanitise the outbound body: strip markers + ensure include_usage (no-op → keep).
    // `had_markers` is whether the client actually sent cache_control (the adoption signal,
    // recorded for all traffic) — NOT whether the body changed, since a stream gets
    // include_usage injected even with no markers. Re-frame: set Content-Length and drop any
    // stale Transfer-Encoding (sending both is invalid HTTP). `from(u64)` is the numeric ctor.
    let (stripped, had_markers) = strip_cache_control(&body_bytes);
    cache_metrics::record_marker_request(had_markers);
    let forward = stripped.unwrap_or(body_bytes);
    parts.headers.remove(header::TRANSFER_ENCODING);
    parts
        .headers
        .insert(header::CONTENT_LENGTH, axum::http::HeaderValue::from(forward.len() as u64));
    let response = next.run(Request::from_parts(parts, Body::from(forward))).await;

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
        return defer_classify_into_stream(response, handle, state.deadline, model_label, state.classifier.clone());
    }

    let outcome = join_classify(&mut handle, state.deadline, &model_label).await;
    if !outcome.active {
        // Disabled model (or a degraded classify) → leave the response untouched.
        return response;
    }
    let (response, billing_ok) = inject_into_response_nonstreaming(response, &outcome.stats).await;
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
fn defer_classify_into_stream(
    response: Response,
    handle: tokio::task::JoinHandle<CacheResult<ClassifyOutcome>>,
    deadline: Duration,
    model_label: String,
    classifier: Classifier,
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
            let probe = scan_inject_sse(&chunk, &CacheStats::default(), true);
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
            // Inject into the (single) usage frame, but only for an active (cache-enabled) request.
            let out = if !edited && probe.saw_usage && outcome.as_ref().is_some_and(|o| o.active) {
                let stats = outcome.as_ref().map(|o| o.stats).unwrap_or_default();
                let scan = scan_inject_sse(&chunk, &stats, false);
                // Only mark done once it *actually* rewrote — a (rare) reserialize failure
                // shouldn't permanently disable injection for a later usage frame.
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
    use crate::prompt_cache::{
        CacheIndex, IndexScope, ModelConfigResolver, PostgresIndex, PrincipalResolver, TokenizerClient, parse_chat_completions,
    };
    use crate::test::utils::{create_test_api_key_for_user, create_test_endpoint, create_test_model, create_test_user};
    use axum::middleware::from_fn_with_state;
    use axum::routing::post;
    use axum::{Json, Router};
    use sqlx::PgPool;
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ALIAS: &str = "layer-model";
    const TOK_VER: &str = "sha256:lv1";

    fn all_tiers() -> TierPolicy {
        TierPolicy::from_config(&["5m".to_string(), "1h".to_string(), "24h".to_string()], "5m")
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
            Arc::new(PostgresIndex::new(pool.clone())),
            all_tiers(),
        );
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_upstream))
            .layer(from_fn_with_state(CacheLayerState::new(classifier, usize::MAX), cache_middleware));
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
        let hash = parse_chat_completions(&serde_json::to_vec(&body()).unwrap(), &all_tiers())
            .unwrap()
            .cumulative_hashes[0]
            .clone();
        let idx = PostgresIndex::new(pool.clone());
        let mut committed = false;
        for _ in 0..100 {
            if !idx.lookup(&scope, std::slice::from_ref(&hash)).await.unwrap().is_empty() {
                committed = true;
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(committed, "the write should have committed after a 2xx");

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
            Arc::new(PostgresIndex::new(pool.clone())),
            all_tiers(),
        );
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_upstream_streaming))
            .layer(from_fn_with_state(CacheLayerState::new(classifier, usize::MAX), cache_middleware));
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
        let hash = parse_chat_completions(&serde_json::to_vec(&body_streaming()).unwrap(), &all_tiers())
            .unwrap()
            .cumulative_hashes[0]
            .clone();
        let idx = PostgresIndex::new(pool.clone());
        let mut committed = false;
        for _ in 0..100 {
            if !idx.lookup(&scope, std::slice::from_ref(&hash)).await.unwrap().is_empty() {
                committed = true;
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(committed, "streaming write commits after a clean usage frame");

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
            Arc::new(PostgresIndex::new(pool.clone())),
            all_tiers(),
        );
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_upstream_streaming_error))
            .layer(from_fn_with_state(CacheLayerState::new(classifier, usize::MAX), cache_middleware));
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
        let hash = parse_chat_completions(&serde_json::to_vec(&body_streaming()).unwrap(), &all_tiers())
            .unwrap()
            .cumulative_hashes[0]
            .clone();
        let idx = PostgresIndex::new(pool.clone());
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
            Arc::new(PostgresIndex::new(pool.clone())),
            all_tiers(),
        );
        let app = Router::new()
            .route("/v1/embeddings", post(mock_upstream))
            .layer(from_fn_with_state(CacheLayerState::new(classifier, usize::MAX), cache_middleware));
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
            Arc::new(PostgresIndex::new(pool.clone())),
            TierPolicy::from_config(&["5m".to_string()], "5m"),
        );
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_upstream)) // must NOT be reached
            .layer(from_fn_with_state(CacheLayerState::new(classifier, usize::MAX), cache_middleware));
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
}
