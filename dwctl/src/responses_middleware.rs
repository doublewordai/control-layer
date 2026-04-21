//! Axum middleware that creates pending response records in fusillade before
//! onwards proxies the request.
//!
//! Applied to the onwards router for `/v1/responses` requests only. Sets the
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
    // Only intercept POST requests to /responses (the path within the /ai/v1 nest)
    let is_responses_post =
        req.method() == axum::http::Method::POST && req.uri().path().ends_with("/responses");

    if !is_responses_post {
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

    // Create the pending fusillade rows
    let response_id = match response_store::create_pending(
        &state.pool,
        &request_value,
        model,
        "/v1/responses",
        state.daemon_id,
    )
    .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "Failed to create pending response in fusillade");
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .unwrap();
        }
    };

    tracing::debug!(response_id = %response_id, model = %model, "Created pending response");

    // Reconstruct the request with the response ID header
    let mut req = Request::from_parts(parts, Body::from(body_bytes));
    req.headers_mut().insert(
        ONWARDS_RESPONSE_ID_HEADER,
        response_id.parse().expect("response_id is valid header value"),
    );

    next.run(req).await
}
