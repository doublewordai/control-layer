//! The dwctl-owned cache tower layer (design §0) — the integration point.
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
use axum::http::{Method, header};
use axum::middleware::Next;
use axum::response::Response;
use tracing::warn;

use super::classifier::{Classifier, ClassifyOutcome, ClassifyRequest};
use super::inject::{CommitGate, inject_cache_stats_into_response, strip_cache_control};

/// State for [`cache_middleware`]. Added to the stack only when caching is enabled.
#[derive(Clone)]
pub struct CacheLayerState {
    pub classifier: Classifier,
    pub deadline: Duration,
}

impl CacheLayerState {
    pub fn new(classifier: Classifier) -> Self {
        Self {
            classifier,
            // Mirrors onwards' old `DEFAULT_CLASSIFY_DEADLINE`; only bites on an
            // index/tokenizer outage (classify normally finishes during generation).
            deadline: Duration::from_secs(5),
        }
    }
}

/// v1: only chat-completions (the parser handles that body shape). Responses + others
/// pass straight through (tool-Responses per-step caching is a fast-follow, §0).
fn is_cacheable(req: &Request) -> bool {
    req.method() == Method::POST && req.uri().path().ends_with("/chat/completions")
}

pub async fn cache_middleware(State(state): State<CacheLayerState>, request: Request, next: Next) -> Response {
    if !is_cacheable(&request) {
        return next.run(request).await;
    }

    let (mut parts, body) = request.into_parts();
    let body_bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => {
            // Can't read the body — forward an empty one (degraded; onwards will 4xx).
            return next.run(Request::from_parts(parts, Body::empty())).await;
        }
    };

    let virtual_model = serde_json::from_slice::<serde_json::Value>(&body_bytes)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from));
    let api_key = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(String::from);

    // Fork classify, parallel with the upstream call. Owns its inputs so the task is
    // `'static`; this is the one body clone (the §19 parse-once work would remove it).
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
    let forward = strip_cache_control(&body_bytes).unwrap_or(body_bytes);
    parts
        .headers
        .insert(header::CONTENT_LENGTH, axum::http::HeaderValue::from(forward.len()));
    let response = next.run(Request::from_parts(parts, Body::from(forward))).await;

    // Join classify under the deadline (≈never waits — it raced the slower generation).
    let outcome = match classify_handle {
        Some(handle) => match tokio::time::timeout(state.deadline, handle).await {
            Ok(Ok(Ok(result))) => result,
            Ok(Ok(Err(e))) => {
                warn!(error = %e, "cache classify failed — billing un-cached");
                ClassifyOutcome::inactive()
            }
            Ok(Err(e)) => {
                warn!(error = %e, "cache classify task panicked");
                ClassifyOutcome::inactive()
            }
            Err(_) => ClassifyOutcome::inactive(), // deadline — best-effort, §11 backstops
        },
        None => ClassifyOutcome::inactive(),
    };

    // Disabled model (or a degraded classify) → leave the response untouched. Enabled
    // models always get the cache_* fields (zeros when this prompt cached nothing), so
    // the cohort has one uniform response shape (§0.2).
    if !outcome.active {
        return response;
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
            CommitGate::Ready(false) => {}
            CommitGate::Deferred(rx) => {
                tokio::spawn(async move {
                    if rx.await.unwrap_or(false)
                        && let Err(e) = classifier.commit(&pending).await
                    {
                        warn!(error = %e, "cache index commit failed");
                    }
                });
            }
        }
    }

    response
}

/// Spawn the success-gated index commit off the response path.
fn spawn_commit(classifier: Classifier, pending: super::stats::PendingWrite) {
    tokio::spawn(async move {
        if let Err(e) = classifier.commit(&pending).await {
            warn!(error = %e, "cache index commit failed");
        }
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
        sqlx::query!("UPDATE deployed_models SET cache_pricing_enabled = true WHERE id = $1", id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query!(
            r#"INSERT INTO model_cache_tariffs (deployed_model_id, ttl_tier, write_multiplier, min_prefix_tokens)
               VALUES ($1, '1h', 2.0, 1024)"#,
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
        );
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_upstream))
            .layer(from_fn_with_state(CacheLayerState::new(classifier), cache_middleware));
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
            org_id: user.id,
            virtual_model: ALIAS.into(),
            tokenizer_version: TOK_VER.into(),
        };
        let hash = parse_chat_completions(&serde_json::to_vec(&body()).unwrap())
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
        );
        let app = Router::new()
            .route("/v1/embeddings", post(mock_upstream))
            .layer(from_fn_with_state(CacheLayerState::new(classifier), cache_middleware));
        let server = axum_test::TestServer::new(app).unwrap();
        let r = server
            .post("/v1/embeddings")
            .json(&serde_json::json!({"model": "x", "input": "hi"}))
            .await;
        let v: serde_json::Value = r.json();
        assert!(v["usage"].get("cache_read_input_tokens").is_none());
    }
}
