//! Axum middleware that creates pending response records in fusillade before
//! onwards proxies the request.
//!
//! Applied to the onwards router for all inference POST requests
//! (`/v1/responses`, `/v1/chat/completions`, `/v1/embeddings`). Sets the
//! `X-Onwards-Response-Id` header so the `FusilladeOutletHandler` knows which
//! row to update with the captured response body.

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::Response,
};
use sqlx::PgPool;

use crate::response_store::{self, OnwardsDaemonId, ONWARDS_RESPONSE_ID_HEADER};

/// State for the responses middleware.
#[derive(Clone)]
pub struct ResponsesMiddlewareState {
    pub pool: PgPool,
    pub daemon_id: OnwardsDaemonId,
}

/// Middleware that creates a pending fusillade row for `/v1/responses` POST requests.
///
/// Skips requests that are already tracked by fusillade's batch daemon
/// (identified by the `X-Fusillade-Request-Id` header).
#[tracing::instrument(skip_all)]
pub async fn responses_middleware(
    State(state): State<ResponsesMiddlewareState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    // Only intercept POST requests to inference endpoints.
    if !should_intercept(req.method(), req.uri().path()) {
        return next.run(req).await;
    }

    // Skip if this is a fusillade daemon request (already tracked)
    if req.headers().get("x-fusillade-request-id").is_some() {
        return next.run(req).await;
    }

    // Read and parse the request body to extract model
    let (parts, body) = req.into_parts();
    let body_bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!(error = %e, "Failed to read request body in responses middleware");
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::empty())
                .unwrap();
        }
    };

    let request_value: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "Failed to parse request body in responses middleware");
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::empty())
                .unwrap();
        }
    };

    let model = request_value["model"].as_str().unwrap_or("unknown");
    let endpoint = parts.uri.path();

    // Create the pending fusillade rows. If this fails (e.g., fusillade not configured),
    // proceed without tracking — the request still proxies, just won't be retrievable.
    let response_id = match response_store::create_pending(
        &state.pool,
        &request_value,
        model,
        endpoint,
        state.daemon_id,
    )
    .await
    {
        Ok(id) => {
            tracing::debug!(response_id = %id, model = %model, "Created pending response");
            Some(id)
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to create pending response in fusillade, proceeding without tracking");
            None
        }
    };

    // Reconstruct the request, attaching the response ID header if tracking succeeded
    let mut req = Request::from_parts(parts, Body::from(body_bytes));
    if let Some(ref id) = response_id {
        req.headers_mut().insert(
            ONWARDS_RESPONSE_ID_HEADER,
            id.parse().expect("response_id is valid header value"),
        );
    }

    next.run(req).await
}

/// Check if a request should be intercepted by this middleware.
/// Exported for testing.
pub(crate) fn should_intercept(method: &axum::http::Method, path: &str) -> bool {
    method == axum::http::Method::POST
        && (path.ends_with("/responses")
            || path.ends_with("/chat/completions")
            || path.ends_with("/embeddings"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_intercept_responses() {
        assert!(should_intercept(&axum::http::Method::POST, "/v1/responses"));
        assert!(should_intercept(&axum::http::Method::POST, "/responses"));
    }

    #[test]
    fn test_should_intercept_chat_completions() {
        assert!(should_intercept(
            &axum::http::Method::POST,
            "/v1/chat/completions"
        ));
        assert!(should_intercept(
            &axum::http::Method::POST,
            "/chat/completions"
        ));
    }

    #[test]
    fn test_should_intercept_embeddings() {
        assert!(should_intercept(
            &axum::http::Method::POST,
            "/v1/embeddings"
        ));
    }

    #[test]
    fn test_should_not_intercept_get() {
        assert!(!should_intercept(&axum::http::Method::GET, "/v1/responses"));
        assert!(!should_intercept(
            &axum::http::Method::GET,
            "/v1/chat/completions"
        ));
    }

    #[test]
    fn test_should_not_intercept_models() {
        assert!(!should_intercept(&axum::http::Method::GET, "/v1/models"));
        assert!(!should_intercept(&axum::http::Method::POST, "/v1/models"));
    }

    #[test]
    fn test_should_not_intercept_batches() {
        assert!(!should_intercept(&axum::http::Method::POST, "/v1/batches"));
    }

    #[test]
    fn test_should_not_intercept_files() {
        assert!(!should_intercept(&axum::http::Method::POST, "/v1/files"));
    }
}
