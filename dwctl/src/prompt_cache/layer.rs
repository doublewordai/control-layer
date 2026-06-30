//! The dwctl-owned cache tower layer — the integration point.
//!
//! Wrapping the (cache-agnostic) onwards router, on each cacheable request it:
//!   1. reads the body, extracts the virtual model + bearer token,
//!   2. **forks** [`Classifier::classify`] (in parallel with the upstream call),
//!   3. strips `cache_control` markers + forces `include_usage`, forwards to onwards,
//!   4. joins classify under a deadline, **injects** the `CacheStats` into the usage,
//!   5. on a 2xx, **commits** the `PendingWrite` to the index (off the response path).
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

use super::classifier::{Classifier, ClassifyOutcome, ClassifyRequest};
use super::index::TierPolicy;
use super::inject::{CommitGate, inject_cache_stats_into_response, strip_cache_control};
use super::metrics as cache_metrics;
use super::parse::{ParseError, validate_markers};

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
            "param": "messages[].content[].cache_control",
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

    // Join classify under the deadline (≈never waits — it raced the slower generation).
    // Time out against `&mut handle` (a JoinHandle is Unpin → itself a Future) so the handle
    // survives an elapsed timeout and we can `abort()` it: a *dropped* JoinHandle detaches the
    // task to run on, so under a tokenizer/DB stall a moved-in handle would leak an orphan
    // classify task per request. Aborting cancels it at its next await instead.
    let outcome = match classify_handle {
        Some(mut handle) => match tokio::time::timeout(state.deadline, &mut handle).await {
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
                // JoinError is a panic OR a cancellation (e.g. runtime shutdown); only the
                // former is a bug, so don't fold cancellations into the "panicked" series.
                cache_metrics::record_classify(if e.is_panic() { "panicked" } else { "error" });
                warn!(error = %e, "cache classify task failed");
                ClassifyOutcome::inactive()
            }
            Err(_) => {
                cache_metrics::record_classify("deadline_exceeded");
                handle.abort(); // deadline — best-effort, reconciliation backstops; don't leak the task
                ClassifyOutcome::inactive()
            }
        },
        None => ClassifyOutcome::inactive(),
    };

    // Request-level cache outcome across ALL traffic (incl. inactive). No model label:
    // `inactive` covers unknown/typo models (raw client input) → unbounded cardinality;
    // per-model volumes live on record_token_volumes (enabled models only).
    let outcome_label = if !outcome.active {
        "inactive"
    } else if outcome.stats.read > 0 && outcome.stats.creation_total() > 0 {
        "read_and_create"
    } else if outcome.stats.read > 0 {
        "read"
    } else if outcome.stats.creation_total() > 0 {
        "create_only"
    } else {
        "zero_active"
    };
    cache_metrics::record_request_outcome(outcome_label);

    // Disabled model (or a degraded classify) → leave the response untouched. Enabled
    // models always get the cache_* fields (zeros when this prompt cached nothing), so
    // the cohort has one uniform response shape.
    if !outcome.active {
        return response;
    }

    // Classified token volumes (model-labelled) for the usage dashboards. Guarded on a
    // non-empty model like the other cache metrics, to avoid a stray model="" series.
    if !model_label.is_empty() {
        cache_metrics::record_token_volumes(
            &model_label,
            outcome.stats.read,
            outcome.stats.creation_5m,
            outcome.stats.creation_1h,
            outcome.stats.creation_24h,
        );
    }

    let (response, gate) = inject_cache_stats_into_response(response, &outcome.stats).await;

    // Commit the write/refresh only when the request actually succeeded for billing —
    // the same signal billing uses, NOT a bare HTTP 200 (a streamed call is 200 the
    // moment it opens; a mid-stream error bills zero and must not seed the cache). Off
    // the response path either way.
    if !outcome.pending.is_empty() {
        let classifier = state.classifier.clone();
        let pending = outcome.pending;
        match gate {
            CommitGate::Ready(true) => spawn_commit(classifier, pending),
            CommitGate::Ready(false) => cache_metrics::record_commit_vetoed("non_2xx"),
            CommitGate::Deferred(rx) => {
                tokio::spawn(async move {
                    if rx.await.unwrap_or(false) {
                        commit_with_deadline(&classifier, &pending).await;
                    } else {
                        cache_metrics::record_commit_vetoed("stream_aborted");
                    }
                });
            }
        }
    }

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
