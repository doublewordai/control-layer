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

use std::sync::Arc;

use axum::{
    Json,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use fusillade::{PostgresRequestManager, ReqwestHttpClient};
use sqlx_pool_router::PoolProvider;

use super::jobs::CreateResponseInput;
use super::store::{self as response_store, ONWARDS_RESPONSE_ID_HEADER, OnwardsDaemonId};

/// State for the responses middleware.
#[derive(Clone)]
pub struct ResponsesMiddlewareState<P: PoolProvider + Clone = sqlx_pool_router::DbPools> {
    pub request_manager: Arc<PostgresRequestManager<P, ReqwestHttpClient>>,
    pub daemon_id: OnwardsDaemonId,
    pub create_response_job: Arc<underway::Job<CreateResponseInput, crate::tasks::TaskState>>,
    /// Base URL for loopback requests (e.g., "http://0.0.0.0:3001/ai").
    /// Flex batches are routed back through dwctl so onwards handles the
    /// responses→chat completions conversion.
    pub loopback_base_url: String,
    /// dwctl database pool for model access validation.
    pub dwctl_pool: sqlx::PgPool,
}

/// Middleware that routes inference requests based on service_tier and background.
#[tracing::instrument(skip_all)]
pub async fn responses_middleware<P: PoolProvider + Clone + Send + Sync + 'static>(
    State(state): State<ResponsesMiddlewareState<P>>,
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
            return Response::builder().status(StatusCode::BAD_REQUEST).body(Body::empty()).unwrap();
        }
    };

    let request_value: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "Failed to parse request body in responses middleware");
            return Response::builder().status(StatusCode::BAD_REQUEST).body(Body::empty()).unwrap();
        }
    };

    let model = request_value["model"].as_str().unwrap_or("unknown");
    let nested_path = parts.uri.path();
    let is_responses_api = nested_path.ends_with("/responses");
    // The router is nested at /ai/v1, so the path here is e.g. "/responses".
    // Prepend /v1 for the full API path used by the loopback and fusillade templates.
    let endpoint = format!("/v1{nested_path}");

    // Extract bearer token for auth check and batch attribution
    let api_key = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());

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
            handle_realtime(
                &state,
                &request_value,
                model,
                &endpoint,
                background,
                api_key.as_deref(),
                parts,
                body_bytes,
                next,
            )
            .await
        }
        ServiceTier::Flex => handle_flex(&state, &request_value, model, &endpoint, background, api_key.as_deref(), &state.loopback_base_url).await,
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
/// Enqueues a CreateResponseJob (validates API key, creates fusillade row) and
/// proxies via onwards. The job runs in parallel with the proxy.
/// With `background=true`, returns 202 immediately and spawns the proxy as a background task.
#[allow(clippy::too_many_arguments)]
async fn handle_realtime<P: PoolProvider + Clone + Send + Sync + 'static>(
    state: &ResponsesMiddlewareState<P>,
    request_value: &serde_json::Value,
    model: &str,
    endpoint: &str,
    background: bool,
    api_key: Option<&str>,
    parts: axum::http::request::Parts,
    body_bytes: bytes::Bytes,
    next: Next,
) -> Response {
    // Generate response ID upfront (needed for both paths)
    let resp_id = format!("resp_{}", uuid::Uuid::new_v4());

    // Enqueue the create-response job (validates key, creates fusillade row).
    // This runs in the background via underway — doesn't block the proxy.
    let enqueue_result = state
        .create_response_job
        .enqueue(&CreateResponseInput {
            response_id: resp_id.clone(),
            request_body: request_value.to_string(),
            model: model.to_string(),
            endpoint: endpoint.to_string(),
            api_key: api_key.map(|s| s.to_string()),
            daemon_id: state.daemon_id.0,
        })
        .await;

    if let Err(e) = enqueue_result {
        tracing::warn!(error = %e, "Failed to enqueue create-response job, proceeding without tracking");
    }

    // Attach the response ID header
    let mut req = Request::from_parts(parts, Body::from(body_bytes));
    req.headers_mut().insert(
        ONWARDS_RESPONSE_ID_HEADER,
        resp_id.parse().expect("response_id is valid header value"),
    );

    if background {
        // Return 202 immediately, proxy in background
        let response_body = serde_json::json!({
            "id": resp_id,
            "object": "response",
            "status": "in_progress",
            "model": model,
            "background": true,
            "output": [],
        });

        tokio::spawn(async move {
            let response = next.run(req).await;
            // Consume the body so outlet's tee-stream captures all chunks
            let (_parts, body) = response.into_parts();
            let _ = axum::body::to_bytes(body, usize::MAX).await;
        });

        (StatusCode::ACCEPTED, Json(response_body)).into_response()
    } else {
        // Blocking: proxy via onwards, then patch response ID
        let resp = next.run(req).await;
        patch_response_id(resp, &resp_id).await
    }
}

/// Handle a flex request by creating a batch of 1 in fusillade (1h completion window).
///
/// The fusillade daemon will pick it up and process it at async pricing.
/// With `background=false`, holds the connection and polls until complete (Phase 3).
/// With `background=true`, returns 202 immediately.
async fn handle_flex<P: PoolProvider + Clone + Send + Sync + 'static>(
    state: &ResponsesMiddlewareState<P>,
    request_value: &serde_json::Value,
    model: &str,
    endpoint: &str,
    background: bool,
    api_key: Option<&str>,
    loopback_base_url: &str,
) -> Response {
    // Validate model access before creating the batch
    if let Some(key) = api_key {
        if let Err(msg) = crate::error_enrichment::validate_api_key_model_access(
            state.dwctl_pool.clone(), key, model,
        ).await {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::from(serde_json::json!({"error": {"message": msg, "type": "invalid_request_error"}}).to_string()))
                .unwrap();
        }
    }

    let result = response_store::create_batch_of_1(&state.request_manager, request_value, model, loopback_base_url, endpoint, "1h", api_key).await;

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
        // If the client disconnects, the future is dropped but the batch remains
        // in fusillade — the daemon still processes it and the client can poll
        // GET /v1/responses/{id} later.
        tracing::debug!(response_id = %response_id, "Blocking flex — polling until daemon completes");

        let poll_interval = std::time::Duration::from_millis(500);
        let timeout = std::time::Duration::from_secs(3600); // 1h matches completion_window

        match response_store::poll_until_complete(&state.request_manager, &response_id, poll_interval, timeout).await {
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

/// Patch the `id` field in a JSON response body to use our fusillade ID.
/// If the body can't be parsed or is streaming, returns the response unchanged.
async fn patch_response_id(response: Response, fusillade_id: &str) -> Response {
    let (parts, body) = response.into_parts();

    // Only patch JSON responses (not streaming SSE)
    let is_json = parts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("application/json"));

    if !is_json {
        return Response::from_parts(parts, body);
    }

    match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => {
            if let Ok(mut json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if json.get("id").is_some() {
                    json["id"] = serde_json::Value::String(fusillade_id.to_string());
                }
                let patched = serde_json::to_vec(&json).unwrap_or_else(|_| bytes.to_vec());
                Response::from_parts(parts, Body::from(patched))
            } else {
                Response::from_parts(parts, Body::from(bytes))
            }
        }
        Err(_) => Response::from_parts(parts, Body::empty()),
    }
}

/// Check if a request should be intercepted by this middleware.
pub(crate) fn should_intercept(method: &axum::http::Method, path: &str) -> bool {
    method == axum::http::Method::POST
        && (path.ends_with("/responses") || path.ends_with("/chat/completions") || path.ends_with("/embeddings"))
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
        assert!(should_intercept(&axum::http::Method::POST, "/v1/chat/completions"));
    }

    #[test]
    fn test_should_intercept_embeddings() {
        assert!(should_intercept(&axum::http::Method::POST, "/v1/embeddings"));
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
