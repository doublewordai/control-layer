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
    /// Multi-step warm-path streaming pieces. When the user sends
    /// `stream: true` (and not `background: true`) on `/v1/responses`,
    /// the middleware routes the request through
    /// [`super::streaming::run_inline_streaming`] using these.
    pub response_store: Arc<super::store::FusilladeResponseStore<P>>,
    pub multi_step_tool_executor: Arc<crate::tool_executor::HttpToolExecutor>,
    pub multi_step_http_client: Arc<dyn onwards::client::HttpClient + Send + Sync>,
    pub loop_config: onwards::LoopConfig,
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

    let mut request_value: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "Failed to parse request body in responses middleware");
            return Response::builder().status(StatusCode::BAD_REQUEST).body(Body::empty()).unwrap();
        }
    };

    let model = request_value["model"].as_str().unwrap_or("unknown").to_string();
    let model = model.as_str();
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

    // Inject server-side resolved tools into the request body so the
    // multi-step path's transition function (which reads `body["tools"]`
    // and forwards them to upstream model_call payloads) sees them.
    // Without this, tools registered in the dwctl tool registry never
    // reach the model — it can't issue real tool_calls and the loop
    // runs as a single model_call.
    //
    // The single-step (onwards-routed) path injects tools via the
    // strict-mode handlers; the multi-step path bypasses onwards for
    // model_calls so we have to do it here. Skipped if the user
    // already supplied tools (their list takes precedence).
    //
    // The cross-cutting `tool_injection_middleware` runs *inside* this
    // layer (axum applies later-added layers as outer wrappers, so
    // when responses_middleware fires, tool_injection hasn't yet
    // populated request.extensions::<ResolvedTools>). We do the same
    // DB resolve directly here.
    if request_value.get("tools").is_none()
        && is_responses_api
        && let Some(key) = api_key.as_deref()
        && let Ok(Some(resolved)) = crate::tool_injection::resolve_tools_for_request(&state.dwctl_pool, key, Some(model)).await
    {
        let openai_tools = resolved.to_openai_tools_array();
        if !openai_tools.is_empty() {
            request_value["tools"] = serde_json::Value::Array(openai_tools);
        }
    }

    // Only the Responses API supports service_tier and background.
    // Chat completions and embeddings always use realtime tier.
    let (service_tier, background) = if is_responses_api {
        let tier = resolve_service_tier(request_value["service_tier"].as_str());
        let bg = request_value["background"].as_bool().unwrap_or(false);
        (tier, bg)
    } else {
        (ServiceTier::Realtime, false)
    };

    // Multi-step warm-path dispatch. Always engages for /v1/responses
    // (regardless of stream/background flags) so tool calls actually
    // dispatch — single-step onwards proxying would forward server-side
    // tools to the upstream model but never run them. /v1/responses is
    // the multi-step API; chat completions and embeddings keep their
    // existing single-step proxy path.
    //
    //   stream=true,  background=false → SSE response, loop runs inline
    //   stream=false, background=false → JSON response, loop runs inline
    //   stream=*,     background=true  → 202 + spawned loop, GET /v1/responses/{id} polls
    let stream_requested = is_responses_api && !background && request_value["stream"].as_bool().unwrap_or(false);
    if stream_requested && let Some(resp) = try_warm_path_stream(&state, &request_value, api_key.as_deref(), model).await {
        return resp;
    }
    if is_responses_api
        && !background
        && !stream_requested
        && let Some(resp) = try_warm_path_blocking(&state, &request_value, api_key.as_deref(), model).await
    {
        return resp;
    }
    if is_responses_api
        && background
        && let Some(resp) = try_warm_path_background(&state, &request_value, api_key.as_deref(), model).await
    {
        return resp;
    }

    tracing::debug!(
        model = %model,
        service_tier = %service_tier,
        background = background,
        endpoint = %endpoint,
        "Routing inference request"
    );

    // Generate the request and batch IDs upfront — known before any DB calls
    // or proxying. The batch_id is set as `x-fusillade-batch-id` on the
    // proxied request so analytics_handler can associate the http_analytics
    // row with this batch (otherwise total_cost / token aggregates in the
    // Batches view come back empty for realtime tracking rows).
    let request_id = uuid::Uuid::new_v4();
    let batch_id = uuid::Uuid::new_v4();
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
        batch_id: Some(batch_id),
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
        ServiceTier::Realtime => handle_realtime(&state, batch_input, batch_id, &resp_id, model, background, parts, body_bytes, next).await,
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
    batch_id: uuid::Uuid,
    resp_id: &str,
    model: &str,
    background: bool,
    parts: axum::http::request::Parts,
    body_bytes: bytes::Bytes,
    next: Next,
) -> Response {
    let rm = state.request_manager.clone();

    // batch_id is passed in explicitly (rather than re-extracted from
    // batch_input) so the type system enforces its presence — fusillade's
    // `batch_id` field is `Option<Uuid>` to keep its API friendly for callers
    // that don't need a pre-generated id, but here we always have one.
    let endpoint_for_header = batch_input.endpoint.clone();

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
            batch_id,
            request_id: batch_input.request_id,
            body: batch_input.body,
            model: batch_input.model,
            base_url: batch_input.base_url,
            endpoint: batch_input.endpoint,
            api_key: batch_input.api_key,
        };
        tracing::debug!(
            request_id = %job_input.request_id,
            model = %job_input.model,
            endpoint = %job_input.endpoint,
            "responses_middleware enqueueing create-response job"
        );
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
    // x-fusillade-batch-id wires this realtime tracking row up to its
    // http_analytics row so total_cost / token aggregates show up in the
    // Batches view (analytics_handler reads this header).
    req.headers_mut().insert(
        "x-fusillade-batch-id",
        batch_id.to_string().parse().expect("batch_id is valid header value"),
    );
    req.headers_mut().insert(
        ONWARDS_RESPONSE_ID_HEADER,
        resp_id.parse().expect("response_id is valid header value"),
    );
    // The outlet handler reads these to synthesize the fusillade row if
    // create-response hasn't run yet (race).
    if let Ok(value) = endpoint_for_header.parse() {
        req.headers_mut().insert("x-onwards-endpoint", value);
    }
    if let Ok(value) = model.parse() {
        req.headers_mut().insert("x-onwards-model", value);
    }

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
                    // Extract the real HTTP status from the error's code field,
                    // which is populated by detail_to_response_object from the
                    // upstream response status.
                    response_obj["error"]["code"]
                        .as_u64()
                        .and_then(|c| StatusCode::from_u16(c as u16).ok())
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
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

/// Attempt to dispatch a `/v1/responses` request through the warm-path
/// streaming handler. Returns `Some(response)` if the dispatch
/// succeeded; `None` if the request can't be served via the warm path
/// (no API key, missing tool resolution, etc.) and should fall through
/// to the standard single-step / daemon paths.
async fn try_warm_path_stream<P: PoolProvider + Clone + Send + Sync + 'static>(
    state: &ResponsesMiddlewareState<P>,
    request_value: &serde_json::Value,
    api_key: Option<&str>,
    model: &str,
) -> Option<Response> {
    let api_key = api_key?;
    let (head_step_uuid, resolved, upstream) = warm_path_setup(state, request_value, api_key, model).await?;

    let sse = super::streaming::run_inline_streaming(
        state.response_store.clone(),
        state.multi_step_tool_executor.clone(),
        resolved,
        state.multi_step_http_client.clone(),
        upstream,
        state.loop_config,
        head_step_uuid.to_string(),
        model.to_string(),
    );
    Some(sse.into_response())
}

/// Warm-path blocking handler for `/v1/responses` with
/// `stream:false, background:false`. Same multi-step machinery as
/// `try_warm_path_stream`, but accumulates the loop's output and
/// returns a single JSON response instead of an SSE stream — so tools
/// dispatch correctly even when the user opted out of streaming.
async fn try_warm_path_blocking<P: PoolProvider + Clone + Send + Sync + 'static>(
    state: &ResponsesMiddlewareState<P>,
    request_value: &serde_json::Value,
    api_key: Option<&str>,
    model: &str,
) -> Option<Response> {
    let api_key = api_key?;
    let (request_id, resolved, upstream) = match warm_path_setup(state, request_value, api_key, model).await {
        Some(s) => s,
        None => return None,
    };

    let result = super::streaming::run_inline_blocking(
        state.response_store.clone(),
        state.multi_step_tool_executor.clone(),
        resolved,
        state.multi_step_http_client.clone(),
        upstream,
        state.loop_config,
        request_id.to_string(),
        model.to_string(),
    )
    .await;

    let (status, body) = match result {
        Ok(json) => (StatusCode::OK, json),
        Err(err_payload) => (StatusCode::BAD_GATEWAY, serde_json::json!({"error": err_payload})),
    };
    Some((status, Json(body)).into_response())
}

/// Warm-path background handler for `/v1/responses` with
/// `background:true`. Spawns the multi-step loop in a background
/// task and returns a 202 with the in_progress response shape — the
/// caller polls `GET /v1/responses/{id}` for the terminal state.
async fn try_warm_path_background<P: PoolProvider + Clone + Send + Sync + 'static>(
    state: &ResponsesMiddlewareState<P>,
    request_value: &serde_json::Value,
    api_key: Option<&str>,
    model: &str,
) -> Option<Response> {
    let api_key = api_key?;
    let (request_id, resolved, upstream) = match warm_path_setup(state, request_value, api_key, model).await {
        Some(s) => s,
        None => return None,
    };

    let resp_id = format!("resp_{request_id}");
    let response_body = serde_json::json!({
        "id": resp_id,
        "object": "response",
        "status": "in_progress",
        "model": model,
        "background": true,
        "output": [],
    });

    let response_store = state.response_store.clone();
    let tool_executor = state.multi_step_tool_executor.clone();
    let http_client = state.multi_step_http_client.clone();
    let loop_config = state.loop_config;
    let model_str = model.to_string();
    let request_id_str = request_id.to_string();
    tokio::spawn(async move {
        let _ = super::streaming::run_inline_blocking(
            response_store,
            tool_executor,
            resolved,
            http_client,
            upstream,
            loop_config,
            request_id_str,
            model_str,
        )
        .await;
    });

    Some((StatusCode::ACCEPTED, Json(response_body)).into_response())
}

/// Shared setup for the three warm paths: register the per-response
/// context in the side-channel so the bridge's `next_action_for` /
/// `record_step` can re-parse the original body and stamp api_key /
/// created_by / base_url on per-step sub-request rows; resolve
/// per-request tools; build the upstream target.
///
/// Returns the head-step UUID — the caller surfaces it to the user as
/// `resp_<id>` and threads its string form into `run_response_loop` as
/// the loop's `request_id` parameter. Crucially, **no parent
/// `/v1/responses` fusillade row is created**: the response identity is
/// purely the head step, and per-step sub-request rows are minted
/// inside `record_step` for each model_call. This is the key shape
/// change vs the pre-16.8 bridge — it's what lets the dashboard
/// listing query show one row per response (the head's sub-request)
/// with real analytics, instead of a parent row with zero usage.
async fn warm_path_setup<P: PoolProvider + Clone + Send + Sync + 'static>(
    state: &ResponsesMiddlewareState<P>,
    request_value: &serde_json::Value,
    api_key: &str,
    model: &str,
) -> Option<(uuid::Uuid, Arc<crate::tool_executor::ResolvedToolSet>, onwards::UpstreamTarget)> {
    let created_by = response_store::lookup_created_by(&state.dwctl_pool, Some(api_key)).await;

    let resolved = match crate::tool_injection::resolve_tools_for_request(&state.dwctl_pool, api_key, Some(model)).await {
        Ok(Some(set)) => Arc::new(set),
        Ok(None) => Arc::new(crate::tool_executor::ResolvedToolSet::new(
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
        )),
        Err(e) => {
            tracing::warn!(error = %e, "warm-path: tool resolution failed; running with no tools");
            Arc::new(crate::tool_executor::ResolvedToolSet::new(
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
            ))
        }
    };

    // The transition function uses these names to decide which
    // tool_calls returned by the model can be auto-dispatched and
    // which must be passed through to the client as `function_call`
    // output items. Any tool the user supplies in their request body
    // that isn't registered in `tool_sources` ends up outside this set
    // and gets the client-side passthrough treatment — without this,
    // HttpToolExecutor would try to dispatch the unknown name and the
    // step would fail with `Tool not found`.
    let resolved_tool_names = resolved.tools.keys().cloned().collect();

    let pending = response_store::PendingResponseInput {
        body: request_value.to_string(),
        api_key: Some(api_key.to_string()),
        created_by,
        base_url: state.loopback_base_url.clone(),
        resolved_tool_names,
    };
    let head_step_uuid = state.response_store.register_pending(pending);

    let upstream = onwards::UpstreamTarget {
        url: format!("{}/v1/chat/completions", state.loopback_base_url),
        api_key: Some(api_key.to_string()),
    };

    Some((head_step_uuid, resolved, upstream))
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
