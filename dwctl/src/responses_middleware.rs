//! Axum middleware that routes inference requests based on `service_tier` and `background`.
//!
//! Applied to the onwards router for all inference POST requests
//! (`/v1/responses`, `/v1/chat/completions`, `/v1/embeddings`).
//!
//! ## Routing
//!
//! - `priority` (realtime): creates a `processing` row, proxies via onwards.
//!   With `background=true`, returns 202 and spawns the proxy as a background task.
//! - `default` / `auto` (async): creates a batch of 1 with 1h completion window.
//!   The fusillade daemon picks it up. With `background=false`, holds the connection
//!   and polls until complete (Phase 3). With `background=true`, returns 202 immediately.
//! - `flex` (batch): same as default but with 24h completion window and batch pricing.

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use sqlx::PgPool;

use crate::response_store::{self, OnwardsDaemonId, ONWARDS_RESPONSE_ID_HEADER};

/// State for the responses middleware.
#[derive(Clone)]
pub struct ResponsesMiddlewareState {
    pub pool: PgPool,
    pub daemon_id: OnwardsDaemonId,
}

/// Middleware that routes inference requests based on service_tier and background.
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

    // Read and parse the request body
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
    let endpoint = parts.uri.path().to_string();
    let service_tier = resolve_service_tier(request_value["service_tier"].as_str());
    let background = request_value["background"].as_bool().unwrap_or(false);

    tracing::debug!(
        model = %model,
        service_tier = %service_tier,
        background = background,
        endpoint = %endpoint,
        "Routing inference request"
    );

    match service_tier {
        ServiceTier::Priority => {
            handle_priority(&state, &request_value, model, &endpoint, background, parts, body_bytes, next).await
        }
        ServiceTier::Async { completion_window } => {
            handle_async(&state, &request_value, model, &endpoint, background, &completion_window).await
        }
    }
}

/// Resolved service tier with its completion window.
enum ServiceTier {
    /// Realtime: direct proxy via onwards.
    Priority,
    /// Async: batch of 1 processed by fusillade daemon.
    Async { completion_window: String },
}

impl std::fmt::Display for ServiceTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceTier::Priority => write!(f, "priority"),
            ServiceTier::Async { completion_window } => write!(f, "async({completion_window})"),
        }
    }
}

/// Map the service_tier string to a resolved tier.
fn resolve_service_tier(tier: Option<&str>) -> ServiceTier {
    match tier {
        Some("priority") => ServiceTier::Priority,
        Some("flex") => ServiceTier::Async {
            completion_window: "24h".to_string(),
        },
        // "default", "auto", None, or unrecognized → async with 1h window
        _ => ServiceTier::Async {
            completion_window: "1h".to_string(),
        },
    }
}

/// Handle a priority (realtime) request.
///
/// Creates a `processing` row and proxies via onwards.
/// With `background=true`, returns 202 immediately and spawns the proxy as a background task.
async fn handle_priority(
    state: &ResponsesMiddlewareState,
    request_value: &serde_json::Value,
    model: &str,
    endpoint: &str,
    background: bool,
    parts: axum::http::request::Parts,
    body_bytes: bytes::Bytes,
    next: Next,
) -> Response {
    // Create the pending fusillade row (processing state, onwards is the daemon).
    // If this fails, proceed without tracking.
    let response_id = match response_store::create_pending(
        &state.pool,
        request_value,
        model,
        endpoint,
        state.daemon_id,
    )
    .await
    {
        Ok(id) => {
            tracing::debug!(response_id = %id, "Created pending response (priority)");
            Some(id)
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to create pending response, proceeding without tracking");
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

    if background {
        // Return 202 immediately, proxy in background
        let resp_id = response_id.unwrap_or_else(|| format!("resp_{}", uuid::Uuid::new_v4()));
        let response_body = serde_json::json!({
            "id": resp_id,
            "object": "response",
            "status": "in_progress",
            "model": model,
            "background": true,
            "output": [],
        });
        tokio::spawn(async move {
            let _response = next.run(req).await;
            // outlet handler will update the row on completion
        });
        (StatusCode::ACCEPTED, Json(response_body)).into_response()
    } else {
        // Blocking: proxy normally, outlet handler writes body on completion
        next.run(req).await
    }
}

/// Handle an async/flex request by creating a batch of 1 in fusillade.
///
/// The fusillade daemon will pick it up and process it.
/// With `background=false`, this would hold the connection and poll (Phase 3 — not yet implemented).
/// With `background=true`, returns 202 immediately.
async fn handle_async(
    state: &ResponsesMiddlewareState,
    request_value: &serde_json::Value,
    model: &str,
    endpoint: &str,
    background: bool,
    completion_window: &str,
) -> Response {
    // Create a batch of 1 in fusillade. The daemon will process it.
    let result = response_store::create_batch_of_1(
        &state.pool,
        request_value,
        model,
        endpoint,
        completion_window,
    )
    .await;

    let (response_id, request_id) = match result {
        Ok(ids) => ids,
        Err(e) => {
            tracing::error!(error = %e, "Failed to create async batch in fusillade");
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from(
                    serde_json::json!({
                        "error": {
                            "message": "Failed to enqueue request",
                            "type": "server_error",
                        }
                    })
                    .to_string(),
                ))
                .unwrap();
        }
    };

    if background {
        // Return 202 with queued status
        let response_body = serde_json::json!({
            "id": response_id,
            "object": "response",
            "status": "queued",
            "model": model,
            "background": true,
            "service_tier": completion_window_to_tier(completion_window),
            "output": [],
        });
        tracing::debug!(
            response_id = %response_id,
            completion_window = %completion_window,
            "Enqueued async request"
        );
        (StatusCode::ACCEPTED, Json(response_body)).into_response()
    } else {
        // Phase 3: hold connection and poll until daemon completes.
        // For now, return 202 with a note that blocking async is not yet supported.
        // TODO: implement polling/LISTEN-NOTIFY for blocking async
        tracing::warn!(
            response_id = %response_id,
            "Blocking async (background=false + non-priority tier) not yet implemented, returning 202"
        );
        let response_body = serde_json::json!({
            "id": response_id,
            "object": "response",
            "status": "queued",
            "model": model,
            "background": false,
            "service_tier": completion_window_to_tier(completion_window),
            "output": [],
        });
        (StatusCode::ACCEPTED, Json(response_body)).into_response()
    }
}

fn completion_window_to_tier(window: &str) -> &str {
    match window {
        "24h" => "flex",
        _ => "default",
    }
}

/// Check if a request should be intercepted by this middleware.
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

    #[test]
    fn test_resolve_service_tier_priority() {
        assert!(matches!(
            resolve_service_tier(Some("priority")),
            ServiceTier::Priority
        ));
    }

    #[test]
    fn test_resolve_service_tier_default() {
        match resolve_service_tier(Some("default")) {
            ServiceTier::Async { completion_window } => assert_eq!(completion_window, "1h"),
            _ => panic!("Expected Async"),
        }
    }

    #[test]
    fn test_resolve_service_tier_auto() {
        match resolve_service_tier(Some("auto")) {
            ServiceTier::Async { completion_window } => assert_eq!(completion_window, "1h"),
            _ => panic!("Expected Async"),
        }
    }

    #[test]
    fn test_resolve_service_tier_flex() {
        match resolve_service_tier(Some("flex")) {
            ServiceTier::Async { completion_window } => assert_eq!(completion_window, "24h"),
            _ => panic!("Expected Async"),
        }
    }

    #[test]
    fn test_resolve_service_tier_none() {
        match resolve_service_tier(None) {
            ServiceTier::Async { completion_window } => assert_eq!(completion_window, "1h"),
            _ => panic!("Expected Async for None"),
        }
    }
}
