//! Body-rewriting middleware that walks incoming `/chat/completions` and
//! `/responses` requests, hands every HTTP(S) `image_url` to the
//! [`ImageNormalizer`] for fetch + store, and substitutes the URL in the
//! body with a fresh short-lived signed URL before forwarding to onwards.
//!
//! Modelled directly on the existing
//! [`tool_injection_middleware`](crate::tool_injection::tool_injection_middleware)
//! pattern: read the body once via `axum::body::to_bytes`, mutate the JSON
//! in place, restore the body via `Body::from(...)`.
//!
//! The middleware is a no-op for:
//! - non-AI paths (we filter on `/chat/completions` and `/responses` suffixes),
//! - requests with no body or with a non-JSON body,
//! - bodies that contain no normalisable image URLs.
//!
//! When [`ImageNormalizerConfig::enabled`] is false the configured
//! [`ImageNormalizer`] is a [`DisabledNormalizer`] which surfaces an error
//! from every call — but because the walker only invokes the normaliser
//! when it actually sees a URL to substitute, requests without any image
//! URLs continue to flow unchanged.
//!
//! Error handling:
//!
//! - `NormalizeError::BadInput` → 400 (the URL is unacceptable: bad scheme,
//!   denied IP, MIME mismatch, oversized).
//! - `NormalizeError::Transient` → 503 (retried internally and still
//!   failing; client can retry).
//! - `NormalizeError::FetchFailed` → 502 (non-retryable upstream error).
//! - `NormalizeError::StoreFailed` / other → 500.
//!
//! The middleware never falls through to passing the original URL on a
//! normalisation failure — that would defeat the purpose during degraded
//! states.
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

use crate::image_normalizer::{ImageInput, ImageNormalizer, Mode, NormalizeError, walker};
use sqlx::PgPool;

/// Shared state threaded through the middleware.
#[derive(Clone)]
pub struct ImageNormalizerMiddlewareState {
    pub normalizer: Arc<dyn ImageNormalizer>,
    /// TTL applied to signed URLs handed to upstream providers from this
    /// (realtime) path. Copied from `ImageNormalizerConfig::signing.realtime_ttl()`.
    pub realtime_ttl: Duration,
    /// Optional DB pool used (a) to look up the caller's user_id from the
    /// bearer-token API key for `image_access` bookkeeping, and (b) by the
    /// per-user-mode lookup once the opt-in flag is wired through.
    /// `None` disables both (useful in tests).
    pub pool: Option<PgPool>,
}

/// Extract the Bearer token from `Authorization`, case-insensitive.
fn extract_bearer_token(request: &Request<Body>) -> Option<String> {
    let auth = request.headers().get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let auth = auth.trim();
    if auth.len() > 7 && auth[..7].eq_ignore_ascii_case("bearer ") {
        Some(auth[7..].to_string())
    } else {
        None
    }
}

/// Resolve `bearer` → `(user_id, image_normalization_enabled)`. Returns
/// `None` if the key isn't known. Best-effort; errors are logged and
/// swallowed.
async fn resolve_caller_for_normalizer(pool: &PgPool, bearer: &str) -> Option<(uuid::Uuid, bool)> {
    match sqlx::query!(
        r#"
        SELECT u.id AS "id!", u.image_normalization_enabled AS "image_normalization_enabled!"
        FROM api_keys ak
        INNER JOIN users u ON u.id = ak.user_id
        WHERE ak.secret = $1 AND ak.is_deleted = FALSE AND u.is_deleted = FALSE
        LIMIT 1
        "#,
        bearer
    )
    .fetch_optional(pool)
    .await
    {
        Ok(Some(row)) => Some((row.id, row.image_normalization_enabled)),
        Ok(None) => None,
        Err(e) => {
            warn!(error = %e, "failed to resolve bearer token for image normaliser (non-fatal)");
            None
        }
    }
}

/// Axum middleware function. Applies to the onwards router, runs in front
/// of `tool_injection_middleware` (i.e. configured as an outer Tower layer
/// so the request reaches it first).
pub async fn image_normalizer_middleware(
    State(state): State<ImageNormalizerMiddlewareState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    // Only act on paths that can carry image inputs.
    if !path_accepts_images(request.uri().path()) {
        return next.run(request).await;
    }

    // Read the body once.
    let body_bytes = match axum::body::to_bytes(std::mem::take(request.body_mut()), usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "Failed to read request body in image_normalizer middleware");
            return (StatusCode::BAD_REQUEST, "Failed to read request body").into_response();
        }
    };

    // Parse as JSON. Non-JSON bodies pass through (some clients send
    // streaming chunks via different paths; we only modify JSON bodies).
    let mut body_value: Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(_) => {
            // Restore body and proceed; not a JSON request we can act on.
            *request.body_mut() = Body::from(body_bytes);
            return next.run(request).await;
        }
    };

    // Resolve the caller's user_id (for image_access bookkeeping) and the
    // per-user opt-in flag (which selects walker mode).
    let caller = match (state.pool.as_ref(), extract_bearer_token(&request)) {
        (Some(pool), Some(bearer)) => resolve_caller_for_normalizer(pool, &bearer).await,
        _ => None,
    };
    let mode = match caller {
        Some((_uid, true)) => Mode::All,
        _ => Mode::HttpOnly,
    };

    let normalizer = state.normalizer.clone();
    let realtime_ttl = state.realtime_ttl;
    let pool_for_access = state.pool.clone();
    let user_id_for_access = caller.map(|(uid, _)| uid);
    let substitute = move |url: String| {
        let normalizer = normalizer.clone();
        let pool_for_access = pool_for_access.clone();
        let is_data_uri = url.starts_with("data:");
        async move {
            let input = if is_data_uri { ImageInput::DataUri(url) } else { ImageInput::HttpUrl(url) };
            let token = normalizer.ingest(input).await?;
            let signed = normalizer.sign(token, realtime_ttl).await?;
            // Best-effort image_access bookkeeping: fire-and-forget so
            // the request path isn't blocked. The (mime, bytes_len)
            // metadata are not needed here; the dashboard endpoint reads
            // them from the store directly.
            if let (Some(pool), Some(user_id)) = (pool_for_access, user_id_for_access) {
                tokio::spawn(async move {
                    crate::api::handlers::images::record_image_access(&pool, user_id, token, "", 0).await;
                });
            }
            Ok::<String, NormalizeError>(signed.url)
        }
    };

    let substituted = match walker::substitute_with(&mut body_value, mode, substitute).await {
        Ok(n) => n,
        Err(e) => {
            warn!(error = %e, "image normalisation failed");
            return normalize_error_response(e);
        }
    };

    if substituted == 0 {
        // No URLs touched — restore the original bytes verbatim to avoid
        // any whitespace / key-order drift from a JSON round-trip.
        *request.body_mut() = Body::from(body_bytes);
    } else {
        debug!(substituted, "image normaliser replaced URLs in request body");
        let new_bytes = match serde_json::to_vec(&body_value) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "Failed to re-serialise body after image normalisation");
                return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to re-serialise request body").into_response();
            }
        };
        // Keep Content-Length consistent with the new body so any
        // downstream layer that reads it sees the post-substitution size.
        let len = new_bytes.len();
        request.headers_mut().insert(
            axum::http::header::CONTENT_LENGTH,
            len.to_string().parse().expect("digit string is a valid header value"),
        );
        *request.body_mut() = Body::from(new_bytes);
    }

    next.run(request).await
}

/// Map a [`NormalizeError`] to an HTTP response. Body shape is a small
/// JSON object so OpenAI-compatible clients can surface it cleanly.
fn normalize_error_response(err: NormalizeError) -> Response {
    let (status, code) = match &err {
        NormalizeError::BadInput(_) => (StatusCode::BAD_REQUEST, "image_url_rejected"),
        NormalizeError::Transient(_) => (StatusCode::SERVICE_UNAVAILABLE, "image_fetch_transient"),
        NormalizeError::FetchFailed(_) => (StatusCode::BAD_GATEWAY, "image_fetch_failed"),
        NormalizeError::StoreFailed(_) => (StatusCode::INTERNAL_SERVER_ERROR, "image_store_failed"),
        NormalizeError::NotFound => (StatusCode::INTERNAL_SERVER_ERROR, "image_token_not_found"),
    };
    let body = serde_json::json!({
        "error": {
            "message": err.to_string(),
            "type": "invalid_request_error",
            "code": code,
        }
    });
    (status, axum::Json(body)).into_response()
}

/// True for paths that may carry image inputs we want to normalise.
/// Matches when nested under `/ai/v1` (production) and when used directly
/// (tests / strict mode).
fn path_accepts_images(path: &str) -> bool {
    path.ends_with("/chat/completions") || path.ends_with("/responses")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_normalizer::{DefaultImageNormalizer, MemoryStore, config::FetcherConfig};
    use axum::{Router, body::to_bytes, http::Method, middleware, routing::post};
    use serde_json::json;
    use std::sync::Arc;
    use tower::ServiceExt;

    /// 1×1 transparent PNG, base64-encoded as a data URI; used as the
    /// fetcher payload for fake upstreams in tests. Smaller alternatives
    /// would suffice.
    const TINY_PNG_DATA_URI: &str =
        "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";

    fn build_router(state: ImageNormalizerMiddlewareState) -> Router {
        // A trivial inner handler that echoes the body it received back to
        // the caller. We assert on this echo to verify the substitution
        // happened (or didn't).
        let inner = post(|body: axum::body::Bytes| async move { (StatusCode::OK, body) });
        Router::new()
            .route("/chat/completions", inner.clone())
            .route("/responses", inner.clone())
            .route("/embeddings", inner)
            .layer(middleware::from_fn_with_state(state, image_normalizer_middleware))
    }

    fn state_for_tests() -> ImageNormalizerMiddlewareState {
        let store = Arc::new(MemoryStore::new().with_base_url("http://test.local/dw-img"));
        let normalizer = Arc::new(DefaultImageNormalizer::new(FetcherConfig::default(), store));
        ImageNormalizerMiddlewareState {
            normalizer,
            realtime_ttl: Duration::from_secs(900),
            pool: None,
        }
    }

    async fn post_json(router: Router, path: &str, body: Value) -> (StatusCode, Value) {
        let resp = router
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
    }

    #[tokio::test]
    async fn passes_through_when_no_image_urls_present() {
        let router = build_router(state_for_tests());
        let (status, echoed) = post_json(
            router,
            "/chat/completions",
            json!({ "model": "vision", "messages": [ { "role": "user", "content": "hi" } ] }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(echoed["messages"][0]["content"], "hi");
    }

    #[tokio::test]
    async fn passes_through_when_only_data_uri_in_http_only_mode() {
        // HttpOnly mode is the default for the realtime middleware; data:
        // URIs must pass through untouched.
        let router = build_router(state_for_tests());
        let (status, echoed) = post_json(
            router,
            "/chat/completions",
            json!({
                "model": "vision",
                "messages": [{
                    "role": "user",
                    "content": [{ "type": "image_url", "image_url": { "url": TINY_PNG_DATA_URI } }]
                }]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            echoed["messages"][0]["content"][0]["image_url"]["url"],
            TINY_PNG_DATA_URI,
            "data: URI should pass through HttpOnly mode",
        );
    }

    #[tokio::test]
    async fn rejects_http_url_to_link_local_with_400() {
        // The IP deny-list inside the fetcher blocks 169.254/16. Asserts
        // the middleware surfaces this as a 400, not a 500.
        let router = build_router(state_for_tests());
        let (status, body) = post_json(
            router,
            "/chat/completions",
            json!({
                "model": "vision",
                "messages": [{
                    "role": "user",
                    "content": [{
                        "type": "image_url",
                        "image_url": { "url": "http://169.254.169.254/latest/meta-data/" }
                    }]
                }]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "image_url_rejected");
    }

    #[tokio::test]
    async fn does_not_touch_unrelated_paths() {
        // /embeddings doesn't carry image_url, so the middleware shouldn't
        // even peek at the body. Verify by sending invalid JSON — the
        // middleware should pass it through and the inner handler will
        // echo whatever bytes we sent.
        let router = build_router(state_for_tests());
        let resp = router
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/embeddings")
                    .header("content-type", "application/json")
                    .body(Body::from("not json at all"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&bytes[..], b"not json at all");
    }

    #[tokio::test]
    async fn non_json_body_on_image_path_passes_through() {
        // Some clients send streaming chunks etc. that aren't a complete
        // JSON body; we must not error in that case.
        let router = build_router(state_for_tests());
        let resp = router
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from("garbage"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&bytes[..], b"garbage");
    }
}
