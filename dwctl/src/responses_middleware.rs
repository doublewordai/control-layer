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
    let is_responses_api = endpoint.ends_with("/responses");

    // Only the Responses API supports service_tier and background.
    // Chat completions and embeddings always use realtime tier.
    let (service_tier, background) = if is_responses_api {
        let tier = resolve_service_tier(request_value["service_tier"].as_str());
        let bg = request_value["background"].as_bool().unwrap_or(false);
        (tier, bg)
    } else {
        (ServiceTier::Realtime, false)
    };

    tracing::debug!(
        model = %model,
        service_tier = %service_tier,
        background = background,
        endpoint = %endpoint,
        "Routing inference request"
    );

    match service_tier {
        ServiceTier::Realtime => {
            handle_realtime(&state, &request_value, model, &endpoint, background, parts, body_bytes, next).await
        }
        ServiceTier::Flex => {
            handle_flex(&state, &request_value, model, &endpoint, background).await
        }
    }
}

/// Resolved service tier.
enum ServiceTier {
    /// Realtime: direct proxy via onwards.
    Realtime,
    /// Flex: batch of 1 with 1h completion window, processed by fusillade daemon.
    Flex,
}

impl std::fmt::Display for ServiceTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceTier::Realtime => write!(f, "realtime"),
            ServiceTier::Flex => write!(f, "flex"),
        }
    }
}

/// Map the service_tier string to a resolved tier.
/// Only "flex" gets async processing. Everything else is realtime.
fn resolve_service_tier(tier: Option<&str>) -> ServiceTier {
    match tier {
        Some("flex") => ServiceTier::Flex,
        // "priority", "default", "auto", None → realtime
        _ => ServiceTier::Realtime,
    }
}

/// Handle a realtime request (priority/default/auto).
///
/// Creates a `processing` row and proxies via onwards.
/// With `background=true`, returns 202 immediately and spawns the proxy as a background task.
async fn handle_realtime(
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

/// Handle a flex request by creating a batch of 1 in fusillade (1h completion window).
///
/// The fusillade daemon will pick it up and process it at async pricing.
/// With `background=false`, holds the connection and polls until complete (Phase 3).
/// With `background=true`, returns 202 immediately.
async fn handle_flex(
    state: &ResponsesMiddlewareState,
    request_value: &serde_json::Value,
    model: &str,
    endpoint: &str,
    background: bool,
) -> Response {
    let result = response_store::create_batch_of_1(
        &state.pool,
        request_value,
        model,
        endpoint,
        "1h",
    )
    .await;

    let (response_id, _request_id) = match result {
        Ok(ids) => ids,
        Err(e) => {
            tracing::error!(error = %e, "Failed to create flex batch in fusillade");
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
        let response_body = serde_json::json!({
            "id": response_id,
            "object": "response",
            "status": "queued",
            "model": model,
            "background": true,
            "service_tier": "flex",
            "output": [],
        });
        tracing::debug!(response_id = %response_id, "Enqueued flex request");
        (StatusCode::ACCEPTED, Json(response_body)).into_response()
    } else {
        // Blocking flex: hold the connection and poll until the daemon completes.
        tracing::debug!(response_id = %response_id, "Blocking flex — polling until daemon completes");

        let poll_interval = std::time::Duration::from_millis(500);
        let timeout = std::time::Duration::from_secs(3600); // 1h matches completion_window

        match response_store::poll_until_complete(
            &state.pool,
            &response_id,
            poll_interval,
            timeout,
        )
        .await
        {
            Ok(response_obj) => {
                let status_code = if response_obj["status"].as_str() == Some("completed") {
                    StatusCode::OK
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                };
                (status_code, Json(response_obj)).into_response()
            }
            Err(e) => {
                tracing::error!(error = %e, response_id = %response_id, "Blocking flex poll failed");
                let response_body = serde_json::json!({
                    "error": {
                        "message": format!("Request timed out: {e}"),
                        "type": "server_error",
                    }
                });
                (StatusCode::GATEWAY_TIMEOUT, Json(response_body)).into_response()
            }
        }
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
    fn test_resolve_service_tier_priority_is_realtime() {
        assert!(matches!(resolve_service_tier(Some("priority")), ServiceTier::Realtime));
    }

    #[test]
    fn test_resolve_service_tier_default_is_realtime() {
        assert!(matches!(resolve_service_tier(Some("default")), ServiceTier::Realtime));
    }

    #[test]
    fn test_resolve_service_tier_auto_is_realtime() {
        assert!(matches!(resolve_service_tier(Some("auto")), ServiceTier::Realtime));
    }

    #[test]
    fn test_resolve_service_tier_none_is_realtime() {
        assert!(matches!(resolve_service_tier(None), ServiceTier::Realtime));
    }

    #[test]
    fn test_resolve_service_tier_flex() {
        assert!(matches!(resolve_service_tier(Some("flex")), ServiceTier::Flex));
    }
}
