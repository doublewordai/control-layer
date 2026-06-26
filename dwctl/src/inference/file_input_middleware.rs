//! Body-rewriting middleware that walks incoming `/responses` requests and
//! normalises every `input_file` content part via [`crate::file_input`]:
//! `file_url`s are fetched through the hardened fetcher and inlined as base64
//! `file_data`, and unresolvable references (a bare dwctl `file_id`) are
//! rejected with a clear 4xx instead of being silently dropped downstream.
//!
//! Modelled on [`super::image_normalizer_middleware`]: read the body once,
//! mutate the JSON in place, restore the body via `Body::from(...)`.
//!
//! The middleware is a no-op for:
//! - non-`/responses` paths (only the Responses API carries `input_file`),
//! - the feature being disabled,
//! - requests with no body or a non-JSON body,
//! - bodies that contain no rewritable `input_file` parts.
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::Value;
use std::sync::Arc;
use tracing::{debug, warn};

use crate::file_input::normalize_input_files;
use crate::image_normalizer::NormalizeError;
use crate::image_normalizer::fetcher::ImageFetcher;

/// Shared state threaded through the middleware.
#[derive(Clone)]
pub struct FileInputMiddlewareState {
    /// Whether `input_file` `file_url` fetch-and-inline is enabled
    /// (`config.file_input.enabled`). When false the middleware passes traffic
    /// through untouched - onwards then returns a clear error for an
    /// unconvertible `file_url` in strict mode rather than dropping it.
    pub enabled: bool,
    /// Hardened fetcher with the document MIME allow-list applied.
    pub fetcher: Arc<ImageFetcher>,
}

/// Axum middleware function. Applies to the onwards router.
pub async fn file_input_middleware(State(state): State<FileInputMiddlewareState>, mut request: Request<Body>, next: Next) -> Response {
    if !state.enabled {
        return next.run(request).await;
    }

    // `input_file` is a Responses-API concept; nothing else carries it.
    if !path_accepts_files(request.uri().path()) {
        return next.run(request).await;
    }

    // Read the body once.
    let body_bytes = match axum::body::to_bytes(std::mem::take(request.body_mut()), usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "Failed to read request body in file_input middleware");
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

    // Fast path: if the raw body never mentions `input_file` there is nothing
    // to rewrite or reject, so skip the JSON parse entirely and forward the
    // bytes untouched. This keeps the common (no document input) case as cheap
    // as the unavoidable body buffer.
    if !contains_input_file(&body_bytes) {
        *request.body_mut() = Body::from(body_bytes);
        return next.run(request).await;
    }

    // Non-JSON bodies pass through (we only act on JSON request bodies).
    let mut body_value: Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(_) => {
            *request.body_mut() = Body::from(body_bytes);
            return next.run(request).await;
        }
    };

    let fetcher = state.fetcher.clone();
    let result = normalize_input_files(&mut body_value, |url| {
        let fetcher = fetcher.clone();
        async move {
            fetcher
                .fetch(&url)
                .await
                .map(|fetched| (fetched.mime, fetched.bytes))
                .map_err(NormalizeError::from)
        }
    })
    .await;

    let substituted = match result {
        Ok(n) => n,
        Err(e) => {
            warn!(error = %e, "input_file normalisation failed");
            return file_input_error_response(e);
        }
    };

    if substituted == 0 {
        // No parts touched - restore the original bytes verbatim to avoid any
        // whitespace / key-order drift from a JSON round-trip.
        *request.body_mut() = Body::from(body_bytes);
    } else {
        debug!(substituted, "file_input normaliser inlined file_url bytes");
        let new_bytes = match serde_json::to_vec(&body_value) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "Failed to re-serialise body after input_file normalisation");
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
        let len = new_bytes.len();
        request.headers_mut().insert(
            axum::http::header::CONTENT_LENGTH,
            len.to_string().parse().expect("digit string is a valid header value"),
        );
        *request.body_mut() = Body::from(new_bytes);
    }

    next.run(request).await
}

/// Map a [`NormalizeError`] from `input_file` handling to an HTTP response.
/// File-specific codes/messages so the document case is self-diagnosing (the
/// shared `NormalizeError` Display text is image-oriented, so we build our own
/// message from the inner detail).
fn file_input_error_response(err: NormalizeError) -> Response {
    let (status, code, message) = match err {
        NormalizeError::BadInput(m) => (StatusCode::BAD_REQUEST, "input_file_rejected", m),
        NormalizeError::Unfetchable(m) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "input_file_url_unfetchable",
            format!(
                "the file URL could not be retrieved: {m}; ensure it is publicly accessible and \
                 does not require authentication"
            ),
        ),
        NormalizeError::Transient(m) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "input_file_fetch_transient",
            format!("transient failure fetching the file URL: {m}"),
        ),
        NormalizeError::FetchFailed(m) => (
            StatusCode::BAD_GATEWAY,
            "input_file_fetch_failed",
            format!("failed to fetch the file URL: {m}"),
        ),
        NormalizeError::StoreFailed(m) => (StatusCode::SERVICE_UNAVAILABLE, "input_file_store_failed", m),
        NormalizeError::NotFound => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "input_file_not_found",
            "file reference could not be resolved".to_string(),
        ),
    };
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": "invalid_request_error",
            "code": code,
        }
    });
    (status, axum::Json(body)).into_response()
}

/// Only the Responses API carries `input_file`. Matches both `/ai/v1`-nested
/// (production) and bare `/responses` (tests / strict mode) paths.
fn path_accepts_files(path: &str) -> bool {
    path.ends_with("/responses")
}

/// Cheap pre-check on the raw body so we only pay for a JSON parse when an
/// `input_file` content part might be present. A false positive (the substring
/// appears elsewhere, e.g. in user text) just falls through to the normal walk,
/// which is a no-op; a false negative is impossible because every `input_file`
/// part carries the literal `"input_file"` type discriminator.
fn contains_input_file(body: &[u8]) -> bool {
    // `input_file` is 10 bytes; scan for the discriminator substring.
    body.windows(b"input_file".len()).any(|w| w == b"input_file")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_normalizer::config::FetcherConfig;
    use axum::{Router, body::to_bytes, http::Method, middleware, routing::post};
    use serde_json::json;
    use tower::ServiceExt;

    fn build_router(enabled: bool) -> Router {
        let state = FileInputMiddlewareState {
            enabled,
            fetcher: Arc::new(ImageFetcher::new(FetcherConfig {
                allowed_mime: vec!["application/pdf".to_string()],
                ..FetcherConfig::default()
            })),
        };
        let inner = post(|body: axum::body::Bytes| async move { (StatusCode::OK, body) });
        Router::new()
            .route("/responses", inner.clone())
            .route("/chat/completions", inner)
            .layer(middleware::from_fn_with_state(state, file_input_middleware))
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

    fn input_file_body(part: Value) -> Value {
        json!({
            "model": "doc-qa",
            "input": [{ "role": "user", "content": [{ "type": "input_text", "text": "hi" }, part] }]
        })
    }

    #[tokio::test]
    async fn responses_without_input_file_passes_through_verbatim() {
        // No input_file present: the fast path forwards the body untouched.
        let body = json!({
            "model": "doc-qa",
            "input": [{ "role": "user", "content": [{ "type": "input_text", "text": "hi" }] }]
        });
        let (status, echoed) = post_json(build_router(true), "/responses", body.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(echoed, body);
    }

    #[tokio::test]
    async fn rejects_bare_file_id_with_400() {
        let (status, body) = post_json(
            build_router(true),
            "/responses",
            input_file_body(json!({ "type": "input_file", "file_id": "file-abc" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "input_file_rejected");
    }

    #[tokio::test]
    async fn rejects_link_local_file_url_with_400() {
        let (status, body) = post_json(
            build_router(true),
            "/responses",
            input_file_body(json!({
                "type": "input_file",
                "file_url": "http://169.254.169.254/latest/meta-data/"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "input_file_rejected");
    }

    #[tokio::test]
    async fn passes_inline_file_data_through_unchanged() {
        let body = input_file_body(json!({
            "type": "input_file",
            "file_data": "data:application/pdf;base64,QUJD"
        }));
        let (status, echoed) = post_json(build_router(true), "/responses", body.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(echoed, body);
    }

    #[tokio::test]
    async fn disabled_passes_file_url_through_unchanged() {
        let body = input_file_body(json!({
            "type": "input_file",
            "file_url": "http://169.254.169.254/latest/meta-data/"
        }));
        let (status, echoed) = post_json(build_router(false), "/responses", body.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(echoed, body, "disabled middleware must not touch the body");
    }

    #[tokio::test]
    async fn ignores_non_responses_paths() {
        let body = input_file_body(json!({ "type": "input_file", "file_id": "file-abc" }));
        // Same body on /chat/completions must not be inspected or rejected.
        let (status, _echoed) = post_json(build_router(true), "/chat/completions", body).await;
        assert_eq!(status, StatusCode::OK);
    }
}
