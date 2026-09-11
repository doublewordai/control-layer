//! Body-rewriting middleware that walks incoming `/chat/completions` and
//! `/responses` requests, hands every HTTP(S) / `data:` `image_url` to the
//! [`ImageNormalizer`] for fetch + store, and substitutes the URL in the
//! body with a fresh short-lived signed URL before forwarding to onwards.
//!
//! It also signs `dw-img://` tokens. Those are what the flex enqueue and
//! `/v1/files` ingest paths store, so they arrive here on the daemon's
//! dispatch loopback. Signing them in this layer — which sits BELOW the
//! prompt-cache layer — is deliberate: the cache hashes the stable
//! content-addressed token, not the per-attempt signed URL, so a
//! byte-identical image keeps a prefix chain intact across calls. The bearer
//! says who is calling: a dispatch carries the daemon's hidden batch key
//! (never exposed to clients), and its tokens are signed on trust (our own
//! ingest put them in the stored body, under that request's principal) with
//! the dispatch TTL. Any other key is a client re-sending a request it
//! downloaded: each token is authorised against `image_access` (the
//! submitting user, or anyone acting in the organization it was submitted
//! under) and signed with the realtime TTL; anyone else's token gets a 403,
//! and a lookup failure a 503.
//!
//! Pattern: read the body once via `axum::body::to_bytes`, mutate the JSON
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
//! - `NormalizeError::Unfetchable` → 422 (the origin returned a non-408/429
//!   4xx — the user's URL is forbidden/gated/missing; their bad input, not our
//!   failure). 408/429 are classified as transient and retried instead.
//! - `NormalizeError::Transient` → 503 (retried internally and still
//!   failing; client can retry).
//! - `NormalizeError::FetchFailed` → 502 (non-retryable upstream error: a
//!   transport-level failure on our side, not a clean origin 4xx).
//! - `NormalizeError::StoreFailed` → 503 (the content store was briefly
//!   unreachable — a transient dependency failure the client can retry,
//!   not an internal bug).
//! - `NormalizeError::Forbidden` → 403 (a `dw-img://` token the caller's
//!   user/organization never submitted, or a caller we cannot attribute).
//! - `NormalizeError::NotFound` / other → 500 (internal inconsistency).
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

use crate::image_normalizer::{ImageInput, ImageNormalizer, ImageToken, Mode, NormalizeError, TokenParseError, walker};
use sqlx::PgPool;

/// Shared state threaded through the middleware.
#[derive(Clone)]
pub struct ImageNormalizerMiddlewareState {
    /// Whether image normalisation is enabled (`config.image_normalizer.enabled`).
    /// When false, the middleware is a pure pass-through — the configured
    /// normaliser is a `DisabledNormalizer` that errors on every call, so we
    /// must NOT invoke it (otherwise image requests would fail when the
    /// feature is off).
    pub enabled: bool,
    pub normalizer: Arc<dyn ImageNormalizer>,
    /// TTL applied to signed URLs handed to upstream providers from this
    /// (realtime) path. Copied from `ImageNormalizerConfig::signing.realtime_ttl()`.
    pub realtime_ttl: Duration,
    /// TTL applied when signing a `dw-img://` token on a daemon dispatch
    /// loopback: the dispatch TTL, long enough to outlive one full processing
    /// attempt. (A client re-sending its own tokens gets `realtime_ttl`.)
    /// Copied from `ImageNormalizerConfig::signing.dispatch_ttl(..)`.
    pub token_ttl: Duration,
    /// Optional DB pool used (a) to look up the caller's user_id from the
    /// bearer-token API key for `image_access` bookkeeping, and (b) by the
    /// per-user-mode lookup once the opt-in flag is wired through.
    /// `None` disables both (useful in tests).
    pub pool: Option<sqlx_pool_router::DynPools>,
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

/// Axum middleware function. Applies to the onwards router.
pub async fn image_normalizer_middleware(
    State(state): State<ImageNormalizerMiddlewareState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    // Disabled feature → pass through untouched. The normaliser is a
    // `DisabledNormalizer` in this case and would error on any image input,
    // so we must short-circuit before touching the body.
    if !state.enabled {
        return next.run(request).await;
    }

    // Only act on paths that can carry image inputs.
    if !path_accepts_images(request.uri().path()) {
        return next.run(request).await;
    }

    // Read the body once.
    let body_bytes = match axum::body::to_bytes(std::mem::take(request.body_mut()), usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "Failed to read request body in image_normalizer middleware");
            // Match the structured error shape used elsewhere so
            // OpenAI-compatible clients can parse it.
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

    // Image normalisation is a system-wide setting (controlled by
    // `config.image_normalizer.enabled` at startup). When the middleware
    // is wired in, every image input — HTTP(S) URL or `data:` URI — gets
    // normalised through the content-addressed store, and every `dw-img://`
    // token (a daemon loopback carrying what enqueue / ingest stored) gets
    // signed.
    let mode = Mode::AllAndTokens;

    // Who is calling, from the bearer key. Two things come out of one lookup:
    // the attribution (acting user + owning org — `image_access` bookkeeping
    // for fetched/decoded images, best-effort; the authorisation for a
    // client-sent `dw-img://` token), and whether the key is the daemon's
    // hidden batch key, i.e. this is a dispatch loopback. The lookup error is
    // kept: a database blip must become a retryable 503, not a 403.
    let caller_lookup: Result<Option<crate::api::handlers::images::ResolvedCaller>, String> =
        match (state.pool.as_ref(), extract_bearer_token(&request)) {
            (Some(pool), Some(bearer)) => crate::api::handlers::images::try_resolve_caller(&pool.write(), &bearer)
                .await
                .map_err(|e| e.to_string()),
            _ => Ok(None),
        };
    let attribution_for_access = caller_lookup.as_ref().ok().copied().flatten().map(|c| c.attribution);

    let normalizer = state.normalizer.clone();
    let realtime_ttl = state.realtime_ttl;
    let token_ttl = state.token_ttl;
    let pool_for_access = state.pool.clone();
    let substitute = move |url: String| {
        let normalizer = normalizer.clone();
        let pool_for_access = pool_for_access.clone();
        let caller_lookup = caller_lookup.clone();
        let is_data_uri = url.starts_with("data:");
        async move {
            // `dw-img://` token: sign it, no ingest — the bytes are already in
            // the store.
            if ImageToken::looks_like_token(&url) {
                let token: ImageToken = url
                    .parse()
                    .map_err(|e: TokenParseError| NormalizeError::BadInput(format!("invalid dw-img token: {e}")))?;
                // Unattributable callers are refused rather than trusted: a
                // token names bytes, and signing it hands out a URL to them.
                let Some(pool) = pool_for_access.as_ref() else {
                    return Err(NormalizeError::Forbidden);
                };
                let caller = match caller_lookup {
                    Ok(Some(c)) => c,
                    Ok(None) => return Err(NormalizeError::Forbidden),
                    Err(e) => return Err(NormalizeError::Transient(format!("caller lookup failed: {e}"))),
                };
                if caller.is_daemon_dispatch {
                    // The daemon's own dispatch of a body our ingest stored
                    // under this principal: the stored body is the
                    // authorisation record, so sign on trust with the dispatch
                    // TTL. No `image_access` dependency, so neither a
                    // bookkeeping gap nor a lookup blip can fail queued work.
                    let signed = normalizer.sign(token, token_ttl).await?;
                    return Ok::<String, NormalizeError>(signed.url);
                }
                // A client re-sending a request it downloaded: authorise
                // against `image_access` (the submitting user, or anyone acting
                // in the organization it was submitted under) and sign with the
                // realtime TTL — this is an ordinary request, not a dispatch.
                match crate::api::handlers::images::is_token_accessible(&pool.write(), &caller.attribution, token).await {
                    Ok(true) => {}
                    Ok(false) => return Err(NormalizeError::Forbidden),
                    Err(e) => return Err(NormalizeError::Transient(format!("image access lookup failed: {e}"))),
                }
                let signed = normalizer.sign(token, realtime_ttl).await?;
                return Ok::<String, NormalizeError>(signed.url);
            }
            // Pass through URLs that already point at our own normalised
            // store (a client echoing back a URL we signed). Re-ingesting
            // and re-signing would waste a round-trip re-fetching an image we
            // already host and clobber the original TTL with the realtime one.
            if !is_data_uri && normalizer.owns_url(&url) {
                return Ok::<String, NormalizeError>(url);
            }
            let input = if is_data_uri {
                ImageInput::DataUri(url)
            } else {
                ImageInput::HttpUrl(url)
            };
            let ingested = normalizer.ingest(input).await?;
            let signed = normalizer.sign(ingested.token, realtime_ttl).await?;
            // Best-effort image_access bookkeeping: fire-and-forget so
            // the realtime request path isn't blocked. Records real
            // (mime, bytes_len) captured from the ingest result rather
            // than empty placeholders — useful for any future
            // dedup-stats or storage-accounting query.
            if let (Some(pool), Some(attribution)) = (pool_for_access, attribution_for_access) {
                let mime = ingested.mime.clone();
                let bytes_len = ingested.bytes_len;
                let token = ingested.token;
                tokio::spawn(async move {
                    crate::api::handlers::images::record_image_access(&pool.write(), attribution, token, &mime, bytes_len).await;
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
                let body = serde_json::json!({
                    "error": {
                        "message": format!("failed to re-serialise request body: {e}"),
                        "type": "internal_error",
                        "code": "body_reserialize_failed",
                    }
                });
                return (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(body)).into_response();
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

/// Normalise every image input in `body` to a `dw-img://` token (the
/// content-addressed reference), ingesting the bytes into the store. Used by
/// the **Flex** enqueue path: the request is persisted with tokens, and when
/// the daemon's dispatch loops back through the edge, [`image_normalizer_middleware`]
/// turns each token into a fresh signed URL — below the prompt-cache layer, so
/// the cache keys on the token — and the provider never receives the raw
/// image, matching the `/v1/files` batch path. (The realtime path above
/// substitutes signed URLs directly instead, since it forwards immediately.)
///
/// Records `image_access` for every ingested image when `access_pool` is
/// supplied (then `attribution` is required). Returns the number of
/// substitutions made.
pub(crate) async fn normalize_value_to_tokens(
    body: &mut Value,
    normalizer: &Arc<dyn ImageNormalizer>,
    access_pool: Option<PgPool>,
    attribution: Option<crate::api::handlers::images::ImageAttribution>,
) -> Result<usize, NormalizeError> {
    let normalizer = normalizer.clone();
    let substitute = move |url: String| {
        let normalizer = normalizer.clone();
        let access_pool = access_pool.clone();
        let is_data_uri = url.starts_with("data:");
        async move {
            let input = if is_data_uri {
                ImageInput::DataUri(url)
            } else {
                ImageInput::HttpUrl(url)
            };
            let ingested = normalizer.ingest(input).await?;
            // AWAITED and REQUIRED, not fire-and-forget: this row is what
            // authorises the customer re-sending this request's tokens later
            // (the daemon's own dispatch is trusted without it). A submission
            // whose bookkeeping cannot be written — or whose caller cannot be
            // attributed, which a just-authenticated key never is — fails
            // retryably (503) rather than persisting tokens nobody may re-send.
            if let Some(pool) = access_pool {
                let Some(attribution) = attribution else {
                    return Err(NormalizeError::Transient("image attribution unavailable".to_string()));
                };
                crate::api::handlers::images::try_record_image_access(
                    &pool,
                    attribution,
                    ingested.token,
                    &ingested.mime,
                    ingested.bytes_len,
                )
                .await
                .map_err(|e| NormalizeError::StoreFailed(format!("image_access bookkeeping: {e}")))?;
            }
            Ok::<String, NormalizeError>(ingested.token.to_dw_img_uri())
        }
    };
    walker::substitute_with(body, Mode::All, substitute).await
}

/// Map a [`NormalizeError`] to an HTTP response. Body shape is a small
/// JSON object so OpenAI-compatible clients can surface it cleanly.
pub(crate) fn normalize_error_response(err: NormalizeError) -> Response {
    let (status, code) = match &err {
        NormalizeError::BadInput(_) => (StatusCode::BAD_REQUEST, "image_url_rejected"),
        NormalizeError::Unfetchable(_) => (StatusCode::UNPROCESSABLE_ENTITY, "image_url_unfetchable"),
        NormalizeError::Transient(_) => (StatusCode::SERVICE_UNAVAILABLE, "image_fetch_transient"),
        NormalizeError::FetchFailed(_) => (StatusCode::BAD_GATEWAY, "image_fetch_failed"),
        NormalizeError::StoreFailed(_) => (StatusCode::SERVICE_UNAVAILABLE, "image_store_failed"),
        NormalizeError::NotFound => (StatusCode::INTERNAL_SERVER_ERROR, "image_token_not_found"),
        NormalizeError::Forbidden => (StatusCode::FORBIDDEN, "image_token_forbidden"),
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
    use crate::image_normalizer::{DefaultImageNormalizer, DisabledNormalizer, ImageNormalizer, MemoryStore, config::FetcherConfig};
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
            enabled: true,
            normalizer,
            realtime_ttl: Duration::from_secs(900),
            token_ttl: Duration::from_secs(1800),
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
    async fn substitutes_data_uri_with_signed_url_in_all_mode() {
        // The realtime middleware runs `Mode::All` in production. Data
        // URIs should be substituted with a signed URL pointing at the
        // store (a local `http://test.local/dw-img/<hex>` URL when the
        // backing store is `MemoryStore`).
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
        let substituted = echoed["messages"][0]["content"][0]["image_url"]["url"]
            .as_str()
            .expect("image_url.url should still be a string");
        assert_ne!(
            substituted, TINY_PNG_DATA_URI,
            "data: URI should be substituted, not passed through"
        );
        assert!(
            substituted.starts_with("http://test.local/dw-img/"),
            "expected MemoryStore-backed signed URL, got: {substituted}",
        );
    }

    #[tokio::test]
    async fn passes_through_url_already_in_our_store_unchanged() {
        // Regression: a request whose image_url already points at our own
        // normalised store (e.g. signed upstream by the batch dispatch path
        // with the long dispatch TTL) must be forwarded UNCHANGED. Re-ingesting
        // + re-signing here would clobber the upstream TTL with the shorter
        // realtime TTL — the root cause of batch image fetches 403-ing on
        // expired URLs. With MemoryStore, our own URLs start with the base_url.
        let already_ours = "http://test.local/dw-img/abcdef0123456789?expires=9999999999";
        let router = build_router(state_for_tests());
        let (status, echoed) = post_json(
            router,
            "/chat/completions",
            json!({
                "model": "vision",
                "messages": [{
                    "role": "user",
                    "content": [{ "type": "image_url", "image_url": { "url": already_ours } }]
                }]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let forwarded = echoed["messages"][0]["content"][0]["image_url"]["url"]
            .as_str()
            .expect("image_url.url should still be a string");
        assert_eq!(
            forwarded, already_ours,
            "a URL already in our store must pass through unchanged (no re-sign / TTL clobber)"
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

    #[tokio::test]
    async fn disabled_passes_image_request_through_unchanged() {
        // With the feature off, the configured normaliser is a
        // `DisabledNormalizer` that errors on any call. The middleware must
        // short-circuit so image requests still flow to the provider.
        let state = ImageNormalizerMiddlewareState {
            enabled: false,
            normalizer: Arc::new(DisabledNormalizer),
            realtime_ttl: Duration::from_secs(900),
            token_ttl: Duration::from_secs(1800),
            pool: None,
        };
        let router = build_router(state);
        let body = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": TINY_PNG_DATA_URI}}
            ]}]
        });
        let (status, echoed) = post_json(router, "/chat/completions", body.clone()).await;
        assert_eq!(status, StatusCode::OK);
        // Body passed through verbatim — no error, no substitution.
        assert_eq!(echoed, body);
    }

    #[tokio::test]
    async fn normalize_value_to_tokens_replaces_image_with_token() {
        // The Flex enqueue path substitutes images with dw-img:// tokens (not
        // signed URLs) so the daemon JIT-signs them at dispatch.
        let store = Arc::new(MemoryStore::new());
        let normalizer: Arc<dyn ImageNormalizer> = Arc::new(DefaultImageNormalizer::new(FetcherConfig::default(), store));
        let mut body = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "hi"},
                {"type": "image_url", "image_url": {"url": TINY_PNG_DATA_URI}}
            ]}]
        });
        let n = normalize_value_to_tokens(&mut body, &normalizer, None, None).await.unwrap();
        assert_eq!(n, 1);
        let url = body["messages"][0]["content"][1]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("dw-img://"), "expected a dw-img token, got {url}");
        assert!(!url.contains("base64"), "raw base64 must be replaced");
    }

    // ---- `dw-img://` token signing ----
    //
    // Two callers, two rules, told apart by the bearer: the daemon's hidden
    // batch key marks a dispatch, which is trusted (its body was stored by our
    // own ingest) and signed with the dispatch TTL; any other key is a client
    // re-sending a request it downloaded, authorised per token against
    // `image_access` and signed with the realtime TTL.

    async fn post_json_as(router: Router, bearer: Option<&str>, body: Value) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method(Method::POST)
            .uri("/chat/completions")
            .header("content-type", "application/json");
        if let Some(b) = bearer {
            req = req.header("authorization", format!("Bearer {b}"));
        }
        let resp = router
            .oneshot(req.body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn body_with_token(token: crate::image_normalizer::ImageToken) -> Value {
        json!({
            "model": "vision",
            "messages": [{ "role": "user", "content": [
                { "type": "text", "text": "what is this?" },
                { "type": "image_url", "image_url": { "url": token.to_dw_img_uri() } }
            ]}]
        })
    }

    fn state_with_pool(pool: &sqlx::PgPool) -> ImageNormalizerMiddlewareState {
        let mut state = state_for_tests();
        state.pool = Some(sqlx_pool_router::DynPools::new(pool.clone()));
        state
    }

    /// The signed URL the echo returned for the image block, and the TTL it
    /// was signed with (the MemoryStore encodes `expires=<unix>`).
    fn signed_url_and_ttl(echoed: &Value) -> (String, i64) {
        let url = echoed["messages"][0]["content"][1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .to_string();
        let expires: i64 = url.split("expires=").nth(1).expect("expires param").parse().expect("unix ts");
        (url, expires - chrono::Utc::now().timestamp())
    }

    /// An org-scoped API key: `user_id` = the org (the billing principal),
    /// `created_by` = the acting member — the same shape as the hidden batch
    /// key the daemon dispatches with.
    async fn create_org_key_for_member(pool: &sqlx::PgPool, org: crate::types::UserId, member: crate::types::UserId) -> String {
        use crate::api::models::api_keys::ApiKeyCreate;
        use crate::db::handlers::api_keys::ApiKeys;
        use crate::db::handlers::repository::Repository;
        use crate::db::models::api_keys::{ApiKeyCreateDBRequest, ApiKeyPurpose};

        let mut conn = pool.acquire().await.expect("acquire");
        let mut repo = ApiKeys::new(&mut conn);
        let request = ApiKeyCreateDBRequest::new(
            org,
            member,
            ApiKeyCreate {
                name: format!("org key {}", uuid::Uuid::new_v4().simple()),
                description: None,
                purpose: ApiKeyPurpose::Realtime,
                requests_per_second: None,
                burst_size: None,
                member_id: None,
                spend_limit: None,
                spend_limit_interval: None,
            },
        );
        repo.create(&request).await.expect("create org key").secret
    }

    /// The key the daemon dispatches with: the user's hidden `batch` key, as
    /// flex enqueue / batch creation store it on the request.
    async fn hidden_batch_key_for(pool: &sqlx::PgPool, user: crate::types::UserId) -> String {
        use crate::db::handlers::api_keys::ApiKeys;
        use crate::db::models::api_keys::ApiKeyPurpose;

        let mut conn = pool.acquire().await.expect("acquire");
        let mut repo = ApiKeys::new(&mut conn);
        let (secret, _) = repo
            .get_or_create_hidden_key_with_id(user, ApiKeyPurpose::Batch, user)
            .await
            .expect("hidden batch key");
        secret
    }

    /// Put an image in the store WITHOUT any `image_access` row — the state a
    /// stored body's token is in if the bookkeeping never happened.
    async fn ingest_unrecorded(state: &ImageNormalizerMiddlewareState) -> crate::image_normalizer::ImageToken {
        state
            .normalizer
            .ingest(ImageInput::DataUri(TINY_PNG_DATA_URI.to_string()))
            .await
            .expect("ingest")
            .token
    }

    /// Ingest an image the way flex enqueue does (bytes into the store, an
    /// `image_access` row for the submitting key) and return the token.
    async fn ingest_for_key(
        pool: &sqlx::PgPool,
        state: &ImageNormalizerMiddlewareState,
        key_secret: &str,
    ) -> crate::image_normalizer::ImageToken {
        let ingested = state
            .normalizer
            .ingest(ImageInput::DataUri(TINY_PNG_DATA_URI.to_string()))
            .await
            .expect("ingest");
        let attribution = crate::api::handlers::images::resolve_image_attribution(pool, key_secret)
            .await
            .expect("key resolves");
        crate::api::handlers::images::record_image_access(pool, attribution, ingested.token, &ingested.mime, ingested.bytes_len).await;
        ingested.token
    }

    /// The fix for flex prompt caching with images: a daemon loopback carrying
    /// the token that enqueue stored gets a signed URL from THIS layer (below
    /// the prompt cache). The bearer is the hidden batch key, so it is trusted
    /// — no `image_access` row exists here — and signed with the dispatch TTL:
    /// neither a bookkeeping gap nor a database blip can fail a queued request.
    #[sqlx::test]
    async fn a_daemon_dispatch_signs_its_tokens_on_trust_with_the_dispatch_ttl(pool: sqlx::PgPool) {
        use crate::api::models::users::Role;
        use crate::test::utils::create_test_user;

        let user = create_test_user(&pool, Role::StandardUser).await;
        let batch_key = hidden_batch_key_for(&pool, user.id).await;
        let state = state_with_pool(&pool);
        let token = ingest_unrecorded(&state).await;

        let (status, echoed) = post_json_as(build_router(state), Some(&batch_key), body_with_token(token)).await;

        assert_eq!(status, StatusCode::OK, "{echoed}");
        let (url, ttl) = signed_url_and_ttl(&echoed);
        assert!(url.contains(&token.to_hex()), "{url}");
        assert!((1800 - 60..=1800).contains(&ttl), "dispatch TTL expected, got {ttl}s");
        assert_eq!(echoed["messages"][0]["content"][0]["text"], "what is this?");
    }

    /// A client re-sending a request it downloaded: its own token is signed,
    /// but authorised against `image_access` and with the REALTIME TTL — this
    /// is an ordinary request, not a dispatch.
    #[sqlx::test]
    async fn a_client_resending_its_own_token_is_signed_with_the_realtime_ttl(pool: sqlx::PgPool) {
        use crate::api::models::users::Role;
        use crate::test::utils::{create_test_api_key_for_user, create_test_user};

        let user = create_test_user(&pool, Role::StandardUser).await;
        let key = create_test_api_key_for_user(&pool, user.id).await;
        let state = state_with_pool(&pool);
        let token = ingest_for_key(&pool, &state, &key.secret).await;

        let (status, echoed) = post_json_as(build_router(state), Some(&key.secret), body_with_token(token)).await;

        assert_eq!(status, StatusCode::OK, "{echoed}");
        let (url, ttl) = signed_url_and_ttl(&echoed);
        assert!(url.contains(&token.to_hex()), "{url}");
        assert!((900 - 60..=900).contains(&ttl), "realtime TTL expected, got {ttl}s");
    }

    /// Ownership is by principal, not by human: an image submitted under an
    /// organization key by one member is re-sendable by any member of that
    /// organization.
    #[sqlx::test]
    async fn a_client_may_resend_a_token_another_member_of_its_organization_submitted(pool: sqlx::PgPool) {
        use crate::api::models::users::Role;
        use crate::test::utils::{add_org_member, create_test_org, create_test_user};

        let alice = create_test_user(&pool, Role::StandardUser).await;
        let bob = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;
        add_org_member(&pool, org.id, bob.id, "member").await;
        let alice_org_key = create_org_key_for_member(&pool, org.id, alice.id).await;
        let bob_org_key = create_org_key_for_member(&pool, org.id, bob.id).await;
        let state = state_with_pool(&pool);
        let token = ingest_for_key(&pool, &state, &alice_org_key).await;

        let (status, echoed) = post_json_as(build_router(state), Some(&bob_org_key), body_with_token(token)).await;

        assert_eq!(status, StatusCode::OK, "{echoed}");
        let (url, _) = signed_url_and_ttl(&echoed);
        assert!(url.contains(&token.to_hex()), "{url}");
    }

    /// A token names bytes; signing it hands out a URL to them. A different
    /// principal presenting someone else's token is refused, not served.
    #[sqlx::test]
    async fn a_client_is_refused_a_token_it_never_submitted(pool: sqlx::PgPool) {
        use crate::api::models::users::Role;
        use crate::test::utils::{create_test_api_key_for_user, create_test_user};

        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_key = create_test_api_key_for_user(&pool, owner.id).await;
        let other = create_test_user(&pool, Role::StandardUser).await;
        let other_key = create_test_api_key_for_user(&pool, other.id).await;
        let state = state_with_pool(&pool);
        let token = ingest_for_key(&pool, &state, &owner_key.secret).await;

        let (status, body) = post_json_as(build_router(state), Some(&other_key.secret), body_with_token(token)).await;

        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["error"]["code"], "image_token_forbidden");
    }

    /// No attributable caller (no bearer at all) means no owner to check
    /// against, so the token is refused rather than signed on trust.
    #[sqlx::test]
    async fn an_unattributable_client_is_refused_a_token(pool: sqlx::PgPool) {
        use crate::api::models::users::Role;
        use crate::test::utils::{create_test_api_key_for_user, create_test_user};

        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_key = create_test_api_key_for_user(&pool, owner.id).await;
        let state = state_with_pool(&pool);
        let token = ingest_for_key(&pool, &state, &owner_key.secret).await;

        let (status, body) = post_json_as(build_router(state), None, body_with_token(token)).await;

        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }

    #[test]
    fn error_response_maps_each_variant_to_the_right_status() {
        // Locks the realtime error contract. In particular a store outage is
        // 503 (transient dependency, retryable), NOT 500 — it is not a bug in
        // our code, and 503 lets retry-aware clients back off and retry.
        let cases = [
            (NormalizeError::BadInput("x".into()), StatusCode::BAD_REQUEST),
            // A 4xx from the origin (e.g. a 403 on a gated URL) is the user's
            // bad input — 422, not a 502 gateway error.
            (
                NormalizeError::Unfetchable("origin 403 Forbidden".into()),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (NormalizeError::Transient("x".into()), StatusCode::SERVICE_UNAVAILABLE),
            (NormalizeError::FetchFailed("x".into()), StatusCode::BAD_GATEWAY),
            (NormalizeError::StoreFailed("x".into()), StatusCode::SERVICE_UNAVAILABLE),
            (NormalizeError::NotFound, StatusCode::INTERNAL_SERVER_ERROR),
            // A token the caller never submitted is refused, not "not found":
            // the bytes may well exist, they just aren't theirs.
            (NormalizeError::Forbidden, StatusCode::FORBIDDEN),
        ];
        for (err, expected) in cases {
            assert_eq!(normalize_error_response(err).status(), expected);
        }
    }
}
