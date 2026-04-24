//! Axum middleware that routes inference requests based on `service_tier` and `background`.
//!
//! Applied to the onwards router for all inference POST requests
//! (`/v1/responses`, `/v1/chat/completions`, `/v1/embeddings`).
//!
//! ## Routing
//!
//! - `priority` / `default` / `auto` (realtime): creates a batch of 1 with
//!   `completion_window=0s` in `processing` state, proxies via onwards.
//!   With `background=true`, returns 202 and spawns the proxy as a background task.
//! - `flex` (async): creates a batch of 1 with `completion_window=1h` in
//!   `pending` state. The fusillade daemon picks it up. With `background=false`,
//!   holds the connection and polls until complete. With `background=true`,
//!   returns 202 immediately.

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
    /// Base URL for loopback requests (e.g., "http://127.0.0.1:3001/ai").
    /// Flex batches are routed back through dwctl so onwards handles the
    /// responses→chat completions conversion.
    pub loopback_base_url: String,
    /// dwctl database pool for model access validation.
    pub dwctl_pool: sqlx::PgPool,
    /// Underway job used to create the realtime tracking batch asynchronously
    /// on the blocking (non-background) path.
    pub create_response_job: Arc<underway::Job<CreateResponseInput, crate::tasks::TaskState<P>>>,
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

    // Generate the request ID upfront — known before any DB calls or proxying.
    let request_id = uuid::Uuid::new_v4();
    let resp_id = format!("resp_{request_id}");
    let completion_window = match service_tier {
        ServiceTier::Flex => "1h",
        ServiceTier::Realtime => "0s",
    };

    // Validate API key for flex requests (realtime is validated by onwards).
    // Flex requests bypass onwards entirely — the daemon processes them later —
    // so we must enforce auth here.
    if matches!(service_tier, ServiceTier::Flex) {
        match api_key.as_deref() {
            None => {
                return Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"error": {"message": "API key required", "type": "invalid_request_error"}}).to_string(),
                    ))
                    .unwrap();
            }
            Some(key) => {
                if let Err(msg) = crate::error_enrichment::validate_api_key_model_access(state.dwctl_pool.clone(), key, model).await {
                    return Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({"error": {"message": msg, "type": "invalid_request_error"}}).to_string(),
                        ))
                        .unwrap();
                }
            }
        }
    }

    // Resolve created_by upfront for background/flex (row must exist before
    // returning 202). For realtime non-background, defer to the background task.
    let needs_sync_attribution = background || matches!(service_tier, ServiceTier::Flex);
    let created_by = if needs_sync_attribution {
        response_store::lookup_created_by(&state.dwctl_pool, api_key.as_deref()).await
    } else {
        None
    };

    // Build the batch input (shared by both tiers).
    let initial_state = match service_tier {
        ServiceTier::Realtime => "processing", // Daemon won't claim; outlet handler completes
        ServiceTier::Flex => "pending",        // Daemon claims and processes
    };

    let batch_input = fusillade::CreateSingleRequestBatchInput {
        request_id,
        body: request_value.to_string(),
        model: model.to_string(),
        base_url: state.loopback_base_url.clone(),
        endpoint: endpoint.clone(),
        completion_window: completion_window.to_string(),
        initial_state: initial_state.to_string(),
        api_key: api_key.clone(),
        created_by,
    };

    match service_tier {
        ServiceTier::Realtime => handle_realtime(&state, batch_input, &resp_id, model, background, parts, body_bytes, next).await,
        ServiceTier::Flex => handle_flex(&state, batch_input, &resp_id, model, background).await,
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
/// For `background=true`: creates the batch row synchronously (so the client
/// can immediately poll by ID), then spawns the proxy in the background.
/// For `background=false`: fires off batch creation in the background and
/// proxies immediately — the outlet handler completes the row.
#[allow(clippy::too_many_arguments)]
async fn handle_realtime<P: PoolProvider + Clone + Send + Sync + 'static>(
    state: &ResponsesMiddlewareState<P>,
    batch_input: fusillade::CreateSingleRequestBatchInput,
    resp_id: &str,
    model: &str,
    background: bool,
    parts: axum::http::request::Parts,
    body_bytes: bytes::Bytes,
    next: Next,
) -> Response {
    let rm = state.request_manager.clone();

    if background {
        // Background mode: create batch synchronously so the row exists
        // before we return the 202 (client will poll immediately).
        // created_by was resolved upfront by the caller.
        if let Err(e) = fusillade::Storage::create_single_request_batch(&*rm, batch_input).await {
            tracing::warn!(error = %e, "Failed to create realtime tracking batch");
        }
    } else {
        // Blocking mode: enqueue the create-response underway job so a crash
        // between proxying and the DB insert still leaves a retryable record.
        // The job resolves attribution and calls create_single_request_batch.
        let job_input = CreateResponseInput {
            request_id: batch_input.request_id,
            body: batch_input.body,
            model: batch_input.model,
            base_url: batch_input.base_url,
            endpoint: batch_input.endpoint,
            api_key: batch_input.api_key,
        };
        if let Err(e) = state.create_response_job.enqueue(&job_input).await {
            tracing::warn!(error = %e, "Failed to enqueue create-response job");
        }
    }

    // Attach the response ID as x-fusillade-request-id so that onwards uses
    // it as the response object's `id` field (configured via response_id_header).
    // Strip the "resp_" prefix — onwards re-adds it.
    let raw_id = resp_id.strip_prefix("resp_").unwrap_or(resp_id);
    let mut req = Request::from_parts(parts, Body::from(body_bytes));
    req.headers_mut()
        .insert("x-fusillade-request-id", raw_id.parse().expect("response_id is valid header value"));
    req.headers_mut().insert(
        ONWARDS_RESPONSE_ID_HEADER,
        resp_id.parse().expect("response_id is valid header value"),
    );

    if background {
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
            let (_parts, body) = response.into_parts();
            let _ = axum::body::to_bytes(body, usize::MAX).await;
        });

        (StatusCode::ACCEPTED, Json(response_body)).into_response()
    } else {
        next.run(req).await
    }
}

/// Handle a flex request by creating a batch in fusillade.
///
/// The fusillade daemon picks up the pending request and processes it.
/// With `background=false`, holds the connection and polls until complete.
/// With `background=true`, returns 202 immediately.
async fn handle_flex<P: PoolProvider + Clone + Send + Sync + 'static>(
    state: &ResponsesMiddlewareState<P>,
    batch_input: fusillade::CreateSingleRequestBatchInput,
    resp_id: &str,
    model: &str,
    background: bool,
) -> Response {
    // Flex needs the batch created synchronously (daemon must find the row).
    if let Err(e) = fusillade::Storage::create_single_request_batch(&*state.request_manager, batch_input).await {
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

    if background {
        let response_body = serde_json::json!({
            "id": resp_id,
            "object": "response",
            "status": "queued",
            "model": model,
            "background": true,
            "service_tier": "flex",
            "output": [],
        });
        tracing::debug!(response_id = %resp_id, "Enqueued flex request");
        (StatusCode::ACCEPTED, Json(response_body)).into_response()
    } else {
        tracing::debug!(response_id = %resp_id, "Blocking flex — polling until daemon completes");

        let poll_interval = std::time::Duration::from_millis(500);
        let timeout = std::time::Duration::from_secs(3600);

        match response_store::poll_until_complete(&state.request_manager, resp_id, poll_interval, timeout).await {
            Ok(response_obj) => {
                let status_code = if response_obj["status"].as_str() == Some("completed") {
                    StatusCode::OK
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                };
                (status_code, Json(response_obj)).into_response()
            }
            Err(e) => {
                tracing::error!(error = %e, response_id = %resp_id, "Blocking flex poll failed");
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
