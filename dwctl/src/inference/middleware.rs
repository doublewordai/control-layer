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
use fusillade_arsenal::PostgresRequestManager;
use sqlx_pool_router::PoolProvider;

use super::image_normalizer_middleware::{normalize_error_response, normalize_value_to_tokens};
use super::store::{self as response_store, ONWARDS_RESPONSE_ID_HEADER, OnwardsDaemonId};
use super::streaming::{ReplayFrame, flex_stream_response};
use crate::image_normalizer::ImageNormalizer;
use crate::sync::api_key_cache::ApiKeyMetadata;

#[derive(Debug, thiserror::Error)]
enum FlexAccessError {
    #[error("Invalid API key")]
    InvalidApiKey,
    #[error("API keys with purpose '{purpose}' cannot be used for inference requests.")]
    NonInferencePurpose { purpose: &'static str },
    #[error("You do not have access to '{model}'. Please contact your administrator to request access.")]
    ModelAccess { model: String },
    #[error("Batch access to '{model}' is blocked by a routing rule. Please contact your administrator to request access.")]
    BatchRoutingDenied { model: String },
}

fn api_key_purpose_name(purpose: &crate::db::models::api_keys::ApiKeyPurpose) -> &'static str {
    use crate::db::models::api_keys::ApiKeyPurpose;

    match purpose {
        ApiKeyPurpose::Platform => "platform",
        ApiKeyPurpose::Realtime => "realtime",
        ApiKeyPurpose::Batch => "batch",
        ApiKeyPurpose::Playground => "playground",
    }
}

fn flex_api_key_metadata(
    cache: &crate::sync::api_key_cache::ApiKeyMetadataCache,
    presented_key: &str,
) -> Result<ApiKeyMetadata, FlexAccessError> {
    cache.get(presented_key).ok_or(FlexAccessError::InvalidApiKey)
}

fn key_is_in_model_pool(targets: &onwards::target::Targets, presented_key: &str, model: &str) -> bool {
    targets.targets.get(model).is_some_and(|pool| {
        pool.keys()
            .is_none_or(|keys| onwards::auth::validate_bearer_token(keys, presented_key))
    })
}

async fn wait_for_model_key(targets: &onwards::target::Targets, key: &str, model: &str, timeout: std::time::Duration) -> bool {
    if key_is_in_model_pool(targets, key, model) {
        return true;
    }
    if timeout.is_zero() {
        return false;
    }

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        if key_is_in_model_pool(targets, key, model) {
            return true;
        }
    }
}

fn validate_flex_access(
    targets: &onwards::target::Targets,
    metadata: &ApiKeyMetadata,
    presented_key: &str,
    model: &str,
) -> Result<(), FlexAccessError> {
    let purpose = api_key_purpose_name(&metadata.purpose);
    if metadata.owner_id != uuid::Uuid::nil() && !crate::db::models::api_keys::is_inference_purpose(purpose) {
        return Err(FlexAccessError::NonInferencePurpose { purpose });
    }

    let pool = targets
        .targets
        .get(model)
        .ok_or_else(|| FlexAccessError::ModelAccess { model: model.to_string() })?;
    if pool
        .keys()
        .is_some_and(|keys| !onwards::auth::validate_bearer_token(keys, presented_key))
    {
        return Err(FlexAccessError::ModelAccess { model: model.to_string() });
    }

    let mut labels = targets
        .key_labels
        .get(presented_key)
        .map(|labels| labels.value().clone())
        .unwrap_or_default();
    labels.insert("purpose".to_string(), "batch".to_string());
    if matches!(pool.evaluate_routing_rules(&labels), Some(onwards::target::RoutingAction::Deny)) {
        return Err(FlexAccessError::BatchRoutingDenied { model: model.to_string() });
    }

    Ok(())
}

/// State for the inference middleware.
#[derive(Clone)]
pub struct InferenceMiddlewareState<P: PoolProvider + Clone = sqlx_pool_router::DbPools> {
    /// Commit-acknowledging response lifecycle writer. Task 5 routes create
    /// admissions through this singleton before upstream dispatch.
    pub requests_writer: super::engine::RequestsWriterHandle,
    pub request_manager: Arc<PostgresRequestManager<P>>,
    pub daemon_id: OnwardsDaemonId,
    /// Base URL for loopback requests (e.g., "http://127.0.0.1:3001/ai").
    /// Flex batches are routed back through dwctl so onwards handles the
    /// responses→chat completions conversion.
    pub loopback_base_url: String,
    /// dwctl database pool for model access validation.
    pub dwctl_pool: sqlx::PgPool,
    /// Multi-step warm-path streaming pieces. When the user sends
    /// `stream: true` (and not `background: true`) on `/v1/responses`,
    /// the middleware routes the request through
    /// [`super::streaming::run_inline_streaming`] using these.
    pub response_store: Arc<super::store::FusilladeResponseStore<P>>,
    pub multi_step_tool_executor: Arc<crate::inference::tools::HttpToolExecutor>,
    pub multi_step_http_client: Arc<fusillade::ReqwestHttpClient>,
    pub loop_config: onwards::LoopConfig,
    /// Image-input normaliser (content-addressed store). On the **Flex**
    /// path the request is persisted and dispatched later by the daemon, so
    /// images are normalised to `dw-img://` tokens here (when enabled) — the
    /// daemon JIT-signs them at dispatch, keeping the raw image off the wire
    /// to the provider, matching the `/v1/files` batch path. (Realtime is
    /// normalised by the separate image-normaliser layer instead.)
    pub image_normalizer: Arc<dyn ImageNormalizer>,
    /// Whether image normalisation is enabled (`config.image_normalizer.enabled`).
    pub image_normalizer_enabled: bool,
    /// Upload-volume cap for unverified creditors, in requests per hour
    /// of completion window (`config.batches.unverified_requests_per_completion_hour`).
    /// 0 disables the cap.
    pub unverified_requests_per_completion_hour: usize,
    /// Completion window that flex/async requests map to
    /// (`config.batches.async_requests.completion_window`, e.g. "1h"). The
    /// unverified cap is measured over a rolling window of this length.
    pub flex_completion_window: String,
    /// Encrypted key custody for ZDR flex bodies. `None` disables ZDR.
    pub keystore: Option<crate::keystore::Keystore>,
    /// Per-key response hot-path metadata, kept fresh by
    /// [`crate::sync::api_key_cache`].
    pub api_key_cache: crate::sync::api_key_cache::ApiKeyMetadataCache,
    /// Shared cold-path resolver for a presented key's hidden batch identity.
    pub flex_batch_key_resolver: crate::sync::api_key_cache::FlexBatchKeyResolver,
    /// Live routing snapshot used by onwards for authentication and routing.
    pub onwards_targets: onwards::target::Targets,
}

/// Middleware that routes inference requests based on service_tier and background.
#[tracing::instrument(skip_all)]
pub async fn inference_middleware<P: PoolProvider + Clone + Send + Sync + 'static>(
    State(state): State<InferenceMiddlewareState<P>>,
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
            tracing::error!(error = %e, "Failed to read request body in inference middleware");
            return Response::builder().status(StatusCode::BAD_REQUEST).body(Body::empty()).unwrap();
        }
    };

    let mut request_value: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "Failed to parse request body in inference middleware");
            return Response::builder().status(StatusCode::BAD_REQUEST).body(Body::empty()).unwrap();
        }
    };

    let model = request_value["model"].as_str().unwrap_or("unknown").to_string();
    let model = model.as_str();
    let nested_path = parts.uri.path();
    let is_responses_api = nested_path.ends_with("/responses");
    let is_chat_completions_api = nested_path.ends_with("/chat/completions");
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
    // when inference_middleware fires, tool_injection hasn't yet
    // populated request.extensions::<ResolvedTools>). We do the same
    // DB resolve directly here.
    if request_value.get("tools").is_none()
        && is_responses_api
        && let Some(key) = api_key.as_deref()
        && let Ok(Some(resolved)) = crate::inference::tools::resolve_tools_for_request(&state.dwctl_pool, key, Some(model)).await
    {
        let openai_tools = resolved.to_openai_tools_array();
        if !openai_tools.is_empty() {
            request_value["tools"] = serde_json::Value::Array(openai_tools);
        }
    }

    // Parse `service_tier` and `background` from the body.
    //
    // Both `/v1/responses` and `/v1/chat/completions` support
    // `service_tier:"flex"` — they route to different handlers because
    // their response shapes differ:
    //   - `/v1/responses` → `handle_flex` (Responses-API output shape,
    //     supports `background=true` for fire-and-poll). With
    //     `stream:true` (and not `background`) →
    //     `handle_responses_flex_streaming`, which replays the finished
    //     object as a `response.*` SSE event sequence.
    //   - `/v1/chat/completions` → `handle_chat_completion_flex` (blocks;
    //     OpenAI chat-completions surface has no `background` field). With
    //     `stream:true` → `handle_chat_completion_flex_streaming`, which
    //     replays the finished completion as a `chat.completion.chunk` SSE
    //     stream.
    //
    // Both streaming paths share `run_flex_stream` (enqueue → poll daemon
    // → render); only the terminal-result rendering differs.
    //
    // `/v1/embeddings` doesn't have a flex handler yet — its response
    // shape isn't a chat completion either, and the
    // `detail_to_response_object` path is wrong for it. Flex on
    // embeddings is silently downgraded to realtime with a warning so
    // the fallback is at least observable.
    let requested_tier = resolve_service_tier(request_value["service_tier"].as_str());
    let background = if is_responses_api {
        request_value["background"].as_bool().unwrap_or(false)
    } else {
        false
    };
    let service_tier = if matches!(requested_tier, ServiceTier::Flex) && !is_responses_api && !is_chat_completions_api {
        tracing::warn!(
            endpoint = %nested_path,
            "service_tier:'flex' is not yet supported on this endpoint; falling back to realtime."
        );
        ServiceTier::Realtime
    } else {
        requested_tier
    };
    let is_flex = matches!(service_tier, ServiceTier::Flex);

    // The warm path / multi-step loop only earns its keep when the
    // request actually has tools to dispatch. Tool-free `/v1/responses`
    // can be served by onwards' native single-step proxying (which
    // rewrites /v1/responses → /v1/chat/completions on the wire and
    // back), producing one tracking row via the standard outlet path
    // instead of going through `record_step` / `response_steps` /
    // `finalize_head_request`. We compute `has_tools` after tool
    // injection so server-side-resolved tools also count.
    let has_tools = request_value.get("tools").and_then(|v| v.as_array()).is_some_and(|a| !a.is_empty());

    // Multi-step warm-path dispatch. Engages for tool-using
    // `/v1/responses` realtime requests (priority / default / auto) so
    // tool calls actually dispatch — single-step onwards proxying
    // would forward server-side tools to the upstream model but never
    // run them. Tool-free requests don't need this and fall through to
    // `handle_realtime` like chat-completions.
    // Flex requests skip the warm path entirely so they can reach
    // `handle_flex` and be queued for the daemon (1h SLA, batch
    // pricing). The daemon's `DwctlRequestProcessor` runs the same
    // multi-step loop async when tools are present.
    //
    //   stream=true,  background=false, !flex, tools → SSE response, loop runs inline
    //   stream=false, background=false, !flex, tools → JSON response, loop runs inline
    //   stream=*,     background=true,  !flex, tools → 202 + spawned loop, GET /v1/responses/{id} polls
    //   no tools, !flex                              → falls through to handle_realtime below
    //   any flex                                     → falls through to handle_flex below
    let stream_requested = is_responses_api && !background && request_value["stream"].as_bool().unwrap_or(false);

    // ZDR + server-side tools (responses API) runs the multi-step tool loop —
    // inline on the realtime warm path, async in the flex daemon — which
    // scatters plaintext into response_steps / sub-request rows / per-step
    // outlet logs that ZDR cannot cover. Reject at submit for both tiers, keyed
    // on per-key policy alone (a keystore is irrelevant to whether we can
    // safely serve the request).
    if is_responses_api && has_tools && crate::inference::zdr::is_zdr_request(&state.api_key_cache, api_key.as_deref()) {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"error": {"message": "Zero-data-retention is not supported for requests that use server-side tools.", "type": "invalid_request_error"}}).to_string(),
            ))
            .unwrap();
    }

    match warm_path_branch(is_responses_api, is_flex, background, stream_requested, has_tools) {
        WarmPathBranch::Stream => {
            if let Some(resp) = try_warm_path_stream(&state, &request_value, api_key.as_deref(), model).await {
                return resp;
            }
        }
        WarmPathBranch::Blocking => {
            if let Some(resp) = try_warm_path_blocking(&state, &request_value, api_key.as_deref(), model).await {
                return resp;
            }
        }
        WarmPathBranch::Background => {
            if let Some(resp) = try_warm_path_background(&state, &request_value, api_key.as_deref(), model).await {
                return resp;
            }
        }
        WarmPathBranch::FallThrough => {}
    }

    tracing::debug!(
        model = %model,
        service_tier = %service_tier,
        background = background,
        endpoint = %endpoint,
        "Routing inference request"
    );

    // Generate the request ID upfront — known before any DB calls or
    // proxying. Used as `x-fusillade-request-id` on the proxied request so
    // the outlet handler can locate the row to update.
    let request_id = uuid::Uuid::new_v4();
    let resp_id = format!("resp_{request_id}");

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
                let metadata = match flex_api_key_metadata(&state.api_key_cache, key) {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        return Response::builder()
                            .status(StatusCode::FORBIDDEN)
                            .header("content-type", "application/json")
                            .body(Body::from(
                                serde_json::json!({"error": {"message": error.to_string(), "type": "invalid_request_error"}}).to_string(),
                            ))
                            .unwrap();
                    }
                };
                if let Err(error) = validate_flex_access(&state.onwards_targets, &metadata, key, model) {
                    return Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({"error": {"message": error.to_string(), "type": "invalid_request_error"}}).to_string(),
                        ))
                        .unwrap();
                }
            }
        }
    }

    // Resolve created_by upfront for background realtime responses (row must
    // exist before returning 202). Flex uses the key owner's hidden batch key
    // below, so resolving it there also supplies the attribution target.
    let created_by = if background && matches!(service_tier, ServiceTier::Realtime) {
        api_key
            .as_deref()
            .and_then(|key| state.api_key_cache.get(key))
            .map(|metadata| metadata.owner_id.to_string())
    } else {
        None
    };
    let flex_batch_key_was_cold = matches!(service_tier, ServiceTier::Flex)
        && api_key
            .as_deref()
            .and_then(|key| state.api_key_cache.get(key))
            .is_some_and(|metadata| metadata.hidden_batch_key.is_none());
    let flex_batch_key = if matches!(service_tier, ServiceTier::Flex) {
        match api_key.as_deref() {
            Some(key) => match state.flex_batch_key_resolver.resolve_hidden_batch_key(key).await {
                Ok(Some(resolved)) => {
                    let wait = if flex_batch_key_was_cold {
                        std::time::Duration::from_secs(1)
                    } else {
                        std::time::Duration::ZERO
                    };
                    if !wait_for_model_key(&state.onwards_targets, &resolved.secret, model, wait).await {
                        tracing::warn!(model, "Flex hidden batch key is absent from the live routing snapshot");
                        return Response::builder()
                            .status(StatusCode::SERVICE_UNAVAILABLE)
                            .header("content-type", "application/json")
                            .header("retry-after", "1")
                            .body(Body::from(
                                serde_json::json!({
                                    "error": {
                                        "message": "Flex routing configuration is not ready. Please retry.",
                                        "type": "server_error",
                                        "code": 503,
                                    }
                                })
                                .to_string(),
                            ))
                            .unwrap();
                    }
                    Some(resolved)
                }
                Ok(None) => {
                    tracing::warn!("Flex API key disappeared before hidden batch-key resolution");
                    return Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({"error": {"message": "Invalid API key", "type": "invalid_request_error"}}).to_string(),
                        ))
                        .unwrap();
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to resolve flex hidden batch key");
                    return Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "error": {
                                    "message": "Failed to prepare flex request",
                                    "type": "server_error",
                                    "code": 500,
                                }
                            })
                            .to_string(),
                        ))
                        .unwrap();
                }
            },
            None => None,
        }
    } else {
        None
    };

    // Bound how much an unverified creditor can queue via flex. Flex requests are
    // persisted and dispatched later (they don't pass through onwards' rate
    // limiter), so without this an unverified user could enqueue unbounded
    // volume. The creditor id and verified flag ride along on the hidden
    // batch-key resolution above (`owner_id` is `api_keys.user_id`), so this
    // costs no extra query. `flex_batch_key` is `Some` only for the flex tier,
    // and its resolution already failed closed (403/500) on lookup errors above,
    // so an unresolved creditor never reaches enforcement. No-op for verified
    // creditors or a disabled cap.
    if let Some(key) = flex_batch_key.as_ref()
        && let Err(err) = crate::api::handlers::unverified_volume::enforce_unverified_volume_limit(
            &*state.request_manager,
            state.unverified_requests_per_completion_hour,
            key.owner_id,
            key.verified,
            &state.flex_completion_window,
            1,
            crate::api::handlers::unverified_volume::SubmissionKind::Flex,
        )
        .await
    {
        return Response::builder()
            .status(err.status_code())
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"error": {"message": err.user_message(), "type": "invalid_request_error"}}).to_string(),
            ))
            .unwrap();
    }

    match service_tier {
        ServiceTier::Realtime => {
            // Realtime ZDR is non-persistence (no encryption): decided per-key
            // from the same policy map as flex, but without a keystore since we
            // suppress the stored copies rather than encrypt them.
            let zdr = crate::inference::zdr::is_zdr_request(&state.api_key_cache, api_key.as_deref());
            let realtime_input = fusillade::CreateRealtimeInput {
                request_id,
                // ZDR: never persist the request body to `request_templates`
                // (background path). The live upstream call runs from
                // `body_bytes` below, independent of this stored copy.
                body: if zdr { String::new() } else { request_value.to_string() },
                model: model.to_string(),
                endpoint: state.loopback_base_url.clone(),
                method: "POST".to_string(),
                path: endpoint.clone(),
                api_key: api_key.clone().unwrap_or_default(),
                created_by: created_by.unwrap_or_default(),
            };
            handle_realtime(&state, realtime_input, &resp_id, model, background, zdr, parts, body_bytes, next).await
        }
        ServiceTier::Flex => {
            // ZDR is decided once here at submit (per-key policy); dispatch and
            // retrieve key off the stored body's sentinel instead of re-checking.
            // The tool-using case is already rejected above, before the warm-path
            // branch, for both tiers.
            let zdr = crate::inference::zdr::is_zdr_request(&state.api_key_cache, api_key.as_deref());
            // Flex encrypts the body at rest, which requires a configured
            // keystore. A ZDR account whose request cannot be encrypted must fail
            // loudly - never silently fall back to plaintext, and bail before we
            // even ingest its images below. Missing keystore is a server
            // misconfiguration, hence 500.
            if zdr && state.keystore.is_none() {
                tracing::error!("ZDR flex request but no keystore configured; refusing to store plaintext");
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"error": {"message": "Zero-data-retention is enabled for this account but the server is not configured to store data securely; refusing to process the request.", "type": "server_error"}}).to_string(),
                    ))
                    .unwrap();
            }
            // Flex is persisted now and dispatched later by the daemon, so —
            // unlike realtime — it does NOT pass through the image-normaliser
            // layer. Normalise image inputs to `dw-img://` tokens here so the
            // daemon's dispatch-time JIT signing hands the provider a signed
            // URL rather than the raw image/URL (closing the same exposure the
            // realtime and `/v1/files` paths already close). No-op when the
            // feature is disabled.
            if state.image_normalizer_enabled {
                // Attribute the image to the acting human + owning org (for org
                // keys), mirroring how CurrentUser is derived, so the console's
                // org-scoped image-view authorization lines up.
                let attribution = match api_key.as_deref() {
                    Some(key) => crate::api::handlers::images::resolve_image_attribution(&state.dwctl_pool, key).await,
                    None => None,
                };
                let access_pool = Some(state.dwctl_pool.clone());
                match normalize_value_to_tokens(&mut request_value, &state.image_normalizer, access_pool, attribution).await {
                    Ok(n) => {
                        if n > 0 {
                            tracing::debug!(substituted = n, "flex image normalisation replaced image inputs with tokens");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "flex image normalisation failed");
                        return normalize_error_response(e);
                    }
                }
            }
            // Did the client ask for a streamed flex response? Flex is
            // daemon-processed, so there is no live token feed — we honour
            // `stream:true` by blocking on the daemon and replaying the
            // finished result as SSE (see `run_flex_stream`). The daemon
            // itself must therefore make a *non*-streaming upstream call, so
            // strip `stream`/`stream_options` from the daemon-bound body.
            //
            // Chat completions has no `background` field, so any `stream` on
            // it replays. Responses only replays when not `background`
            // (background returns 202 immediately and the client polls
            // `GET /v1/responses/{id}`). Realtime never reaches this arm.
            let flex_stream = if is_chat_completions_api {
                request_value["stream"].as_bool().unwrap_or(false)
            } else if is_responses_api {
                !background && request_value["stream"].as_bool().unwrap_or(false)
            } else {
                false
            };
            // `stream_options.include_usage` is the chat-completions opt-in for
            // a trailing usage chunk; the Responses surface carries usage in
            // the completed object instead, so it's chat-only.
            let flex_stream_include_usage =
                flex_stream && is_chat_completions_api && request_value["stream_options"]["include_usage"].as_bool().unwrap_or(false);
            if flex_stream && let Some(obj) = request_value.as_object_mut() {
                obj.remove("stream");
                obj.remove("stream_options");
            }

            // For ZDR, encrypt the body and store the per-request keys; any
            // failure fails the request rather than falling back to plaintext.
            let flex_body = if zdr {
                let zdr_store_error = || {
                    Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({"error": {"message": "Failed to securely store zero-data-retention request.", "type": "server_error"}}).to_string(),
                        ))
                        .unwrap()
                };
                let Some(keystore) = state.keystore.as_ref() else {
                    tracing::error!("ZDR enabled but keystore missing; refusing to store plaintext");
                    return zdr_store_error();
                };
                match crate::inference::zdr::prepare_flex_submit(keystore, &request_id, &mut request_value).await {
                    Ok(body) => body,
                    Err(e) => {
                        tracing::error!(error = %e, "ZDR submit failed; refusing to store plaintext");
                        return zdr_store_error();
                    }
                }
            } else {
                request_value.to_string()
            };

            let flex_api_key = flex_batch_key
                .as_ref()
                .map(|key| key.secret.clone())
                .unwrap_or_else(|| api_key.clone().unwrap_or_default());
            let flex_created_by = flex_batch_key
                .as_ref()
                .map(|key| key.owner_id.to_string())
                .or_else(|| created_by.clone())
                .unwrap_or_default();
            let flex_input = fusillade::CreateFlexInput {
                request_id,
                body: flex_body,
                model: model.to_string(),
                endpoint: state.loopback_base_url.clone(),
                method: "POST".to_string(),
                path: endpoint.clone(),
                api_key: flex_api_key,
                created_by: flex_created_by,
            };

            match (is_chat_completions_api, flex_stream) {
                (true, true) => handle_chat_completion_flex_streaming(&state, flex_input, request_id, flex_stream_include_usage).await,
                (true, false) => handle_chat_completion_flex(&state, flex_input, request_id).await,
                // Responses (embeddings flex was downgraded to realtime above).
                (false, true) => handle_responses_flex_streaming(&state, flex_input, request_id).await,
                (false, false) => handle_flex(&state, flex_input, &resp_id, model, background).await,
            }
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

/// Which warm-path branch (if any) should handle a request, given
/// the orthogonal flags `(is_responses_api, is_flex, background,
/// stream_requested)`. `FallThrough` means the request continues to
/// the realtime / flex dispatch below — that's how flex
/// `/v1/responses` reaches `handle_flex` and how chat completions /
/// embeddings always reach `handle_realtime`.
///
/// Extracted as a pure function so the routing decision is testable
/// without standing up the full middleware state.
///
/// Check order matters for readability rather than correctness — both
/// short-circuits return `FallThrough`, so reordering them produces
/// the same answer — but reading flex-first makes the bug-this-PR-
/// fixes connection explicit: flex must never reach the warm path.
/// `is_responses_api` second documents that warm path is exclusive
/// to `/v1/responses`. Stream/background tail dispatch is the only
/// real branching.
#[derive(Debug, PartialEq, Eq)]
enum WarmPathBranch {
    Stream,
    Blocking,
    Background,
    FallThrough,
}

fn warm_path_branch(is_responses_api: bool, is_flex: bool, background: bool, stream_requested: bool, has_tools: bool) -> WarmPathBranch {
    // Flex must reach `handle_flex` to land in fusillade-pending
    // state for the daemon. Engaging warm-path for flex would defeat
    // the tier (the loop runs inline, billed as realtime).
    if is_flex {
        return WarmPathBranch::FallThrough;
    }
    // Warm path is /v1/responses-only — chat completions and
    // embeddings stay on the single-step proxy path.
    if !is_responses_api {
        return WarmPathBranch::FallThrough;
    }
    // Tool-free /v1/responses doesn't need the multi-step loop —
    // there are no tool_calls to dispatch. Fall through so onwards'
    // single-step /v1/responses → /v1/chat/completions proxy handles
    // it, producing one tracking row via the standard outlet path.
    if !has_tools {
        return WarmPathBranch::FallThrough;
    }
    if stream_requested {
        WarmPathBranch::Stream
    } else if background {
        WarmPathBranch::Background
    } else {
        WarmPathBranch::Blocking
    }
}

/// Handle a realtime request (priority/default/auto).
///
/// For `background=true`: creates the request row synchronously so the client
/// can immediately poll by ID, then spawns the proxy in the background.
///
/// For `background=false`: no DB write up front. The client is holding the
/// HTTP connection and cannot poll, so the row only needs to appear at
/// completion time. The outlet handler sends a completion record to the
/// in-process `RequestsWriter`, which inserts the row directly in
/// `completed` state via the batched persist path.
#[allow(clippy::too_many_arguments)]
async fn handle_realtime<P: PoolProvider + Clone + Send + Sync + 'static>(
    state: &InferenceMiddlewareState<P>,
    realtime_input: fusillade::CreateRealtimeInput,
    resp_id: &str,
    model: &str,
    background: bool,
    zdr: bool,
    parts: axum::http::request::Parts,
    body_bytes: bytes::Bytes,
    next: Next,
) -> Response {
    let rm = state.request_manager.clone();

    let endpoint_for_header = realtime_input.path.clone();

    if background {
        // Background mode: create row synchronously so it exists before
        // we return the 202 (client will poll immediately).
        if let Err(e) = fusillade::Storage::create_realtime(&*rm, realtime_input).await {
            tracing::warn!(error = %e, "Failed to create realtime tracking row");
        }
    }
    // Non-background realtime: no pre-write. Row appears at completion via
    // the outlet handler -> RequestsWriter path (handle_response on success,
    // handle_abandoned on mid-flight client disconnect). Behaviour change from
    // the underway era: if neither outlet hook fires (process panic mid-request,
    // SIGKILL between proxy and handler), no row is ever recorded for this
    // request. Previously the create-response job enqueued up front and would
    // synthesise a 'processing' row even in those cases.

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
    // The outlet handler reads these to synthesize the fusillade row if
    // create-response hasn't run yet (race).
    if let Ok(value) = endpoint_for_header.parse() {
        req.headers_mut().insert("x-onwards-endpoint", value);
    }
    if let Ok(value) = model.parse() {
        req.headers_mut().insert("x-onwards-model", value);
    }
    // ZDR realtime: mark the dispatch so outlet's `ZdrBodyScrubber` blanks the
    // `http_requests`/`http_responses` bodies and `FusilladeOutletHandler`
    // suppresses the request/response body it would persist. Reuses the flex
    // marker header/channel; it is harmless if it reaches the provider, exactly
    // like flex's other `x-fusillade-batch-*` metadata headers.
    if zdr {
        req.headers_mut().insert(
            crate::inference::zdr::ZDR_MARKER_HEADER,
            "1".parse().expect("static ZDR marker is a valid header value"),
        );
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

/// Handle a flex request by creating a pending request row in fusillade.
///
/// The fusillade daemon picks up the pending request and processes it.
/// With `background=false`, holds the connection and polls until complete.
/// With `background=true`, returns 202 immediately.
async fn handle_flex<P: PoolProvider + Clone + Send + Sync + 'static>(
    state: &InferenceMiddlewareState<P>,
    flex_input: fusillade::CreateFlexInput,
    resp_id: &str,
    model: &str,
    background: bool,
) -> Response {
    // Flex needs the row created synchronously (daemon must find it).
    if let Err(e) = fusillade::Storage::create_flex(&*state.request_manager, flex_input).await {
        tracing::error!(error = %e, "Failed to create flex row in fusillade");
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "error": {
                        "message": "Failed to enqueue request",
                        "type": "server_error",
                        "code": 500,
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

        match response_store::poll_until_complete(&state.request_manager, resp_id, poll_interval, timeout, state.keystore.as_ref()).await {
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

/// Handle a flex `/v1/chat/completions` request.
///
/// Always blocks: chat completions has no `background` field in the OpenAI surface,
/// so we hold the connection until the daemon finishes (or we hit the 1h timeout).
/// On success the upstream `chat.completion` body is returned verbatim. On failure
/// the OpenAI chat-completions error envelope is returned with the upstream HTTP
/// status surfaced.
async fn handle_chat_completion_flex<P: PoolProvider + Clone + Send + Sync + 'static>(
    state: &InferenceMiddlewareState<P>,
    flex_input: fusillade::CreateFlexInput,
    request_id: uuid::Uuid,
) -> Response {
    if let Err(e) = fusillade::Storage::create_flex(&*state.request_manager, flex_input).await {
        tracing::error!(error = %e, "Failed to create flex chat-completions batch in fusillade");
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "error": {
                        "message": "Failed to enqueue request",
                        "type": "server_error",
                        "code": 500,
                    }
                })
                .to_string(),
            ))
            .unwrap();
    }

    let poll_interval = std::time::Duration::from_millis(500);
    let timeout = std::time::Duration::from_secs(3600);

    match response_store::poll_until_terminal(&state.request_manager, request_id, poll_interval, timeout, state.keystore.as_ref()).await {
        Ok(detail) => {
            let (status, body) = response_store::detail_to_chat_completion_object(&detail);
            let status_code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            tracing::debug!(request_id = %request_id, %status_code, "Flex chat-completions terminal");
            (status_code, Json(body)).into_response()
        }
        Err(e) => {
            // poll_until_terminal can fail for two reasons: the 1h timeout
            // fires (504-shaped), or the polling query itself errors out
            // (500-shaped — DB/connection issues). The current poller
            // returns a single error type without distinguishing, so we
            // log the underlying error and use 504 as the surfaced status
            // since the timeout path is the dominant case in practice. If
            // the poller grows a typed error variant for timeout vs.
            // storage errors, this map can be tightened.
            tracing::error!(error = %e, request_id = %request_id, "Blocking flex chat-completions poll failed");
            (
                StatusCode::GATEWAY_TIMEOUT,
                Json(serde_json::json!({
                    "error": {
                        "message": format!("Request timed out: {e}"),
                        "type": "server_error",
                        "code": 504,
                    }
                })),
            )
                .into_response()
        }
    }
}

/// Handle a streaming flex `/v1/chat/completions` request.
///
/// Respond-first (see [`flex_stream_response`]): returns `200
/// text/event-stream` immediately and fills the stream once the daemon
/// finishes the (non-streaming, since the caller stripped `stream`) upstream
/// call. The finished `chat.completion` is replayed as a
/// `chat.completion.chunk` stream terminated by `data: [DONE]`; a non-2xx
/// result is emitted as an in-stream `data: {"error": …}` frame (still
/// followed by `[DONE]`). `include_usage` mirrors the client's
/// `stream_options.include_usage`.
async fn handle_chat_completion_flex_streaming<P: PoolProvider + Clone + Send + Sync + 'static>(
    state: &InferenceMiddlewareState<P>,
    flex_input: fusillade::CreateFlexInput,
    request_id: uuid::Uuid,
    include_usage: bool,
) -> Response {
    flex_stream_response(
        state.request_manager.clone(),
        flex_input,
        request_id,
        true,
        state.keystore.clone(),
        move |result| match result {
            Ok(detail) => {
                let (status, body) = response_store::detail_to_chat_completion_object(detail);
                if status < 400 {
                    response_store::chat_completion_to_stream_chunks(&body, include_usage)
                        .into_iter()
                        .map(ReplayFrame::unnamed)
                        .collect()
                } else {
                    // `body` is already the chat-completions error envelope
                    // ({"error": …}); emit it verbatim as one in-stream frame.
                    vec![ReplayFrame::unnamed(body)]
                }
            }
            Err(msg) => vec![ReplayFrame::unnamed(serde_json::json!({
                "error": { "message": format!("Request failed: {msg}"), "type": "server_error", "code": 504 }
            }))],
        },
    )
    .await
}

/// Handle a streaming flex `/v1/responses` request (`stream:true`, not
/// `background`).
///
/// Symmetric with [`handle_chat_completion_flex_streaming`]: respond-first,
/// then replay the finished Responses object as the `response.*` SSE event
/// sequence the live warm path also emits — `response.created` →
/// `response.completed` on success, `response.created` → `response.failed` on
/// error. There is no `[DONE]` sentinel on this surface; the terminal event
/// is the terminator.
async fn handle_responses_flex_streaming<P: PoolProvider + Clone + Send + Sync + 'static>(
    state: &InferenceMiddlewareState<P>,
    flex_input: fusillade::CreateFlexInput,
    request_id: uuid::Uuid,
) -> Response {
    flex_stream_response(
        state.request_manager.clone(),
        flex_input,
        request_id,
        false,
        state.keystore.clone(),
        move |result| match result {
            Ok(detail) => {
                let response = response_store::detail_to_response_object(detail);
                if response["status"] == "completed" {
                    response_store::response_object_to_stream_events(&response)
                        .into_iter()
                        .map(|(event, data)| ReplayFrame::named(event, data))
                        .collect()
                } else {
                    // Failure path: emit the created stub then `response.failed`
                    // carrying the response object (which holds status + error),
                    // mirroring the live loop's terminal `Failed` event.
                    let stub = serde_json::json!({
                        "id": response.get("id").cloned().unwrap_or(serde_json::Value::Null),
                        "object": "response",
                        "status": "in_progress",
                    });
                    vec![
                        ReplayFrame::named("response.created", stub),
                        ReplayFrame::named("response.failed", response),
                    ]
                }
            }
            Err(msg) => {
                // Poll itself failed (timeout / DB error). Emit the same
                // `response.created` → `response.failed` lifecycle as the
                // detail-failure branch above so clients tracking the standard
                // Responses lifecycle don't see a `failed` with no `created`.
                let stub = serde_json::json!({
                    "id": serde_json::Value::Null,
                    "object": "response",
                    "status": "in_progress",
                });
                let err = serde_json::json!({
                    "error": { "message": format!("Request failed: {msg}"), "type": "server_error", "code": 504 }
                });
                vec![
                    ReplayFrame::named("response.created", stub),
                    ReplayFrame::named("response.failed", err),
                ]
            }
        },
    )
    .await
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
    state: &InferenceMiddlewareState<P>,
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
    state: &InferenceMiddlewareState<P>,
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
    state: &InferenceMiddlewareState<P>,
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
/// the loop's `request_id` parameter.
///
/// A single `/v1/responses` fusillade row is created up front via
/// `create_realtime` (state=`processing`, id=`head_step_uuid`). That
/// row is the response: `record_step`'s head branch reuses it instead
/// of inserting another, and `finalize_head_request` completes it when
/// the loop terminates. Descendant model_call steps still mint their
/// own sub-request rows. The asymmetry with the daemon-driven flex
/// path (which uses `handle_flex` to create the same shape of row in
/// `pending` state) is just state at insert: warm path doesn't go
/// through the daemon's claim cycle.
async fn warm_path_setup<P: PoolProvider + Clone + Send + Sync + 'static>(
    state: &InferenceMiddlewareState<P>,
    request_value: &serde_json::Value,
    api_key: &str,
    model: &str,
) -> Option<(uuid::Uuid, Arc<crate::inference::tools::ResolvedToolSet>, onwards::UpstreamTarget)> {
    let created_by = state.api_key_cache.get(api_key).map(|metadata| metadata.owner_id.to_string());

    let resolved = match crate::inference::tools::resolve_tools_for_request(&state.dwctl_pool, api_key, Some(model)).await {
        Ok(Some(set)) => Arc::new(set),
        Ok(None) => Arc::new(crate::inference::tools::ResolvedToolSet::new(
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
        )),
        Err(e) => {
            tracing::warn!(error = %e, "warm-path: tool resolution failed; running with no tools");
            Arc::new(crate::inference::tools::ResolvedToolSet::new(
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

    // Allocate the head step UUID up front so the side-channel entry
    // and the fusillade row share the same id — `record_step`'s head
    // branch reuses this row instead of creating a separate sub-request.
    let head_step_uuid = uuid::Uuid::new_v4();

    let pending = response_store::PendingResponseInput {
        body: request_value.to_string(),
        api_key: Some(api_key.to_string()),
        created_by: created_by.clone(),
        base_url: state.loopback_base_url.clone(),
        resolved_tool_names,
    };
    if let Err(e) = state.response_store.register_pending_with_id(head_step_uuid, pending) {
        tracing::error!(
            error = %e,
            request_id = %head_step_uuid,
            "warm-path: failed to register pending input — aborting warm path",
        );
        return None;
    }

    // Create the /v1/responses row up front in `processing` state so
    // it's not claimable by the daemon (warm path owns its lifecycle)
    // and so `record_step`'s head branch can attach to it via id.
    let realtime_input = fusillade::CreateRealtimeInput {
        request_id: head_step_uuid,
        body: request_value.to_string(),
        model: model.to_string(),
        endpoint: state.loopback_base_url.clone(),
        method: "POST".to_string(),
        path: "/v1/responses".to_string(),
        api_key: api_key.to_string(),
        created_by: created_by.unwrap_or_default(),
    };
    if let Err(e) = fusillade::Storage::create_realtime(&*state.request_manager, realtime_input).await {
        tracing::error!(
            error = %e,
            request_id = %head_step_uuid,
            "warm-path: failed to create /v1/responses tracking row — aborting warm path",
        );
        state.response_store.unregister_pending(&head_step_uuid.to_string());
        return None;
    }

    // Endpoint + path are split (not pre-concatenated) so fusillade
    // can match `/v1/chat/completions` against its streamable_endpoints
    // list and pick the streaming branch when the user requested SSE.
    let upstream = onwards::UpstreamTarget {
        endpoint: state.loopback_base_url.clone(),
        path: "/v1/chat/completions".to_string(),
        api_key: Some(api_key.to_string()),
    };

    Some((head_step_uuid, resolved, upstream))
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use dashmap::DashMap;
    use onwards::{
        auth::ConstantTimeString,
        load_balancer::ProviderPool,
        target::{LoadBalanceStrategy, RoutingAction, RoutingRule, Targets},
    };
    use uuid::Uuid;

    use super::*;
    use crate::{db::models::api_keys::ApiKeyPurpose, sync::api_key_cache::ApiKeyMetadata};

    const FLEX_TEST_KEY: &str = "sk-flex-presented";
    const FLEX_TEST_MODEL: &str = "test-model";

    fn flex_access_metadata(owner_id: Uuid, purpose: ApiKeyPurpose) -> ApiKeyMetadata {
        ApiKeyMetadata {
            owner_id,
            created_by: Uuid::new_v4(),
            purpose,
            verified: true,
            zero_data_retention: false,
            hidden_batch_key: Some("sk-flex-hidden".to_string()),
        }
    }

    fn flex_access_targets(keys: &[&str], labels: HashMap<String, String>, rules: Vec<RoutingRule>) -> Targets {
        let keys = keys
            .iter()
            .map(|key| ConstantTimeString::from((*key).to_string()))
            .collect::<HashSet<_>>();
        let pool = ProviderPool::with_config(
            Vec::new(),
            Some(keys),
            None,
            None,
            None,
            LoadBalanceStrategy::default(),
            false,
            rules,
        );
        let targets = Targets {
            targets: Arc::new(DashMap::new()),
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };
        targets.targets.insert(FLEX_TEST_MODEL.to_string(), pool);
        targets.key_labels.insert(FLEX_TEST_KEY.to_string(), labels);
        targets
    }

    #[test]
    fn flex_access_unknown_key_uses_invalid_api_key_error() {
        let cache = crate::sync::api_key_cache::ApiKeyMetadataCache::empty();

        let error = flex_api_key_metadata(&cache, "sk-unknown").unwrap_err();

        assert_eq!(error.to_string(), "Invalid API key");
    }

    #[test]
    fn flex_access_rejects_non_system_platform_key() {
        let targets = flex_access_targets(&[FLEX_TEST_KEY], HashMap::new(), Vec::new());
        let metadata = flex_access_metadata(Uuid::new_v4(), ApiKeyPurpose::Platform);

        let error = validate_flex_access(&targets, &metadata, FLEX_TEST_KEY, FLEX_TEST_MODEL).unwrap_err();

        assert_eq!(
            error.to_string(),
            "API keys with purpose 'platform' cannot be used for inference requests."
        );
    }

    #[test]
    fn flex_access_allows_system_platform_key() {
        let targets = flex_access_targets(&[FLEX_TEST_KEY], HashMap::new(), Vec::new());
        let metadata = flex_access_metadata(Uuid::nil(), ApiKeyPurpose::Platform);

        assert!(validate_flex_access(&targets, &metadata, FLEX_TEST_KEY, FLEX_TEST_MODEL).is_ok());
    }

    #[test]
    fn flex_access_rejects_key_absent_from_model_pool() {
        let targets = flex_access_targets(&["sk-other"], HashMap::new(), Vec::new());
        let metadata = flex_access_metadata(Uuid::new_v4(), ApiKeyPurpose::Realtime);

        let error = validate_flex_access(&targets, &metadata, FLEX_TEST_KEY, FLEX_TEST_MODEL).unwrap_err();

        assert_eq!(
            error.to_string(),
            "You do not have access to 'test-model'. Please contact your administrator to request access."
        );
    }

    #[test]
    fn flex_access_rejects_batch_routing_deny() {
        let rules = vec![RoutingRule {
            match_labels: HashMap::from([
                ("purpose".to_string(), "batch".to_string()),
                ("tenant".to_string(), "test".to_string()),
            ]),
            action: RoutingAction::Deny,
        }];
        let targets = flex_access_targets(
            &[FLEX_TEST_KEY],
            HashMap::from([
                ("purpose".to_string(), "realtime".to_string()),
                ("tenant".to_string(), "test".to_string()),
            ]),
            rules,
        );
        let metadata = flex_access_metadata(Uuid::new_v4(), ApiKeyPurpose::Realtime);

        let error = validate_flex_access(&targets, &metadata, FLEX_TEST_KEY, FLEX_TEST_MODEL).unwrap_err();

        assert_eq!(
            error.to_string(),
            "Batch access to 'test-model' is blocked by a routing rule. Please contact your administrator to request access."
        );
    }

    #[test]
    fn flex_access_ignores_realtime_routing_deny() {
        let rules = vec![RoutingRule {
            match_labels: HashMap::from([("purpose".to_string(), "realtime".to_string())]),
            action: RoutingAction::Deny,
        }];
        let targets = flex_access_targets(&[FLEX_TEST_KEY], HashMap::new(), rules);
        let metadata = flex_access_metadata(Uuid::new_v4(), ApiKeyPurpose::Realtime);

        assert!(validate_flex_access(&targets, &metadata, FLEX_TEST_KEY, FLEX_TEST_MODEL).is_ok());
    }

    #[test]
    fn flex_access_allows_key_in_model_pool() {
        let targets = flex_access_targets(&[FLEX_TEST_KEY], HashMap::new(), Vec::new());
        let metadata = flex_access_metadata(Uuid::new_v4(), ApiKeyPurpose::Realtime);

        assert!(validate_flex_access(&targets, &metadata, FLEX_TEST_KEY, FLEX_TEST_MODEL).is_ok());
    }

    #[tokio::test]
    async fn flex_access_waits_for_hidden_key_in_live_snapshot() {
        let targets = flex_access_targets(&[FLEX_TEST_KEY], HashMap::new(), Vec::new());
        let updated_targets = flex_access_targets(&[FLEX_TEST_KEY, "sk-flex-hidden"], HashMap::new(), Vec::new());
        let updated_pool = updated_targets.targets.get(FLEX_TEST_MODEL).unwrap().clone();
        let targets_for_update = targets.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            targets_for_update.targets.insert(FLEX_TEST_MODEL.to_string(), updated_pool);
        });

        assert!(wait_for_model_key(&targets, "sk-flex-hidden", FLEX_TEST_MODEL, std::time::Duration::from_millis(200),).await);
    }

    #[tokio::test]
    async fn flex_access_hidden_key_wait_is_bounded() {
        let targets = flex_access_targets(&[FLEX_TEST_KEY], HashMap::new(), Vec::new());

        assert!(!wait_for_model_key(&targets, "sk-flex-hidden", FLEX_TEST_MODEL, std::time::Duration::from_millis(25),).await);
    }

    #[tokio::test(start_paused = true)]
    async fn flex_access_rejects_hidden_key_that_appears_after_deadline() {
        let targets = flex_access_targets(&[FLEX_TEST_KEY], HashMap::new(), Vec::new());
        let updated_targets = flex_access_targets(&[FLEX_TEST_KEY, "sk-flex-hidden"], HashMap::new(), Vec::new());
        let updated_pool = updated_targets.targets.get(FLEX_TEST_MODEL).unwrap().clone();
        let waiter_targets = targets.clone();
        let waiter = tokio::spawn(async move {
            wait_for_model_key(
                &waiter_targets,
                "sk-flex-hidden",
                FLEX_TEST_MODEL,
                std::time::Duration::from_secs(1),
            )
            .await
        });
        tokio::task::yield_now().await;

        tokio::time::advance(std::time::Duration::from_millis(1_001)).await;
        targets.targets.insert(FLEX_TEST_MODEL.to_string(), updated_pool);

        assert!(!waiter.await.unwrap());
    }

    #[sqlx::test]
    async fn flex_access_hot_path_enqueues_while_main_pool_is_unavailable(pool: sqlx::PgPool) {
        use std::time::Duration;

        use axum::{Router, middleware, routing::post};
        use sqlx::postgres::PgPoolOptions;
        use sqlx_pool_router::TestDbPools;
        use tower::ServiceExt;

        let fusillade_pool = crate::test::utils::setup_fusillade_pool(&pool).await;
        let fusillade_pools = TestDbPools::new(fusillade_pool.clone()).await.unwrap();
        let request_manager = Arc::new(fusillade_arsenal::PostgresRequestManager::new(fusillade_pools, Default::default()));
        let response_store = Arc::new(response_store::FusilladeResponseStore::new(request_manager.clone()));

        let main_pool = PgPoolOptions::new()
            .max_connections(1)
            .min_connections(0)
            .connect_with(pool.connect_options().as_ref().clone())
            .await
            .unwrap();

        let owner_id = Uuid::new_v4();
        let hidden_key = "sk-flex-hidden";
        let api_key_cache = crate::sync::api_key_cache::ApiKeyMetadataCache::empty();
        api_key_cache.replace(HashMap::from([(
            FLEX_TEST_KEY.to_string(),
            ApiKeyMetadata {
                owner_id,
                created_by: owner_id,
                purpose: ApiKeyPurpose::Realtime,
                verified: true,
                zero_data_retention: false,
                hidden_batch_key: Some(hidden_key.to_string()),
            },
        )]));
        let flex_batch_key_resolver = crate::sync::api_key_cache::FlexBatchKeyResolver::new(main_pool.clone(), api_key_cache.clone());
        let onwards_targets = flex_access_targets(&[FLEX_TEST_KEY, hidden_key], HashMap::new(), Vec::new());

        let (_requests_writer_task, requests_writer) =
            crate::inference::engine::writer::RequestsWriter::new(request_manager.clone(), 1, Duration::ZERO);
        let state = InferenceMiddlewareState {
            requests_writer,
            request_manager,
            daemon_id: OnwardsDaemonId(Uuid::new_v4()),
            loopback_base_url: "http://127.0.0.1:3001/ai".to_string(),
            dwctl_pool: main_pool.clone(),
            response_store,
            multi_step_tool_executor: Arc::new(crate::inference::tools::HttpToolExecutor::new(reqwest::Client::new(), None)),
            multi_step_http_client: Arc::new(fusillade::ReqwestHttpClient::new(
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Vec::new(),
            )),
            loop_config: onwards::LoopConfig::default(),
            image_normalizer: Arc::new(crate::image_normalizer::DisabledNormalizer),
            image_normalizer_enabled: false,
            unverified_requests_per_completion_hour: 0,
            flex_completion_window: "1h".to_string(),
            keystore: None,
            api_key_cache,
            flex_batch_key_resolver,
            onwards_targets,
        };

        let app = Router::new()
            .route("/v1/responses", post(|| async { StatusCode::IM_A_TEAPOT }))
            .layer(middleware::from_fn_with_state(state, inference_middleware));
        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("authorization", format!("Bearer {FLEX_TEST_KEY}"))
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "model": FLEX_TEST_MODEL,
                    "service_tier": "flex",
                    "background": true,
                    "tools": [],
                    "input": "hello",
                })
                .to_string(),
            ))
            .unwrap();

        let _held_connection = main_pool.acquire().await.unwrap();
        let response = tokio::time::timeout(Duration::from_millis(250), app.oneshot(request))
            .await
            .expect("cached Flex validation must not acquire the unavailable main pool")
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let (stored_key, created_by): (String, Option<String>) =
            sqlx::query_as("SELECT t.api_key, r.created_by FROM requests r JOIN request_templates t ON t.id = r.template_id")
                .fetch_one(&fusillade_pool)
                .await
                .unwrap();
        assert_eq!(stored_key, hidden_key);
        assert_eq!(created_by.as_deref(), Some(owner_id.to_string().as_str()));
    }

    #[sqlx::test]
    async fn warm_background_persists_cached_owner_while_main_pool_is_unavailable(pool: sqlx::PgPool) {
        use std::time::Duration;

        use sqlx::postgres::PgPoolOptions;
        use sqlx_pool_router::TestDbPools;

        let fusillade_pool = crate::test::utils::setup_fusillade_pool(&pool).await;
        let fusillade_pools = TestDbPools::new(fusillade_pool.clone()).await.unwrap();
        let request_manager = Arc::new(fusillade_arsenal::PostgresRequestManager::new(fusillade_pools, Default::default()));
        let response_store = Arc::new(response_store::FusilladeResponseStore::new(request_manager.clone()));

        let main_pool = PgPoolOptions::new()
            .max_connections(1)
            .min_connections(0)
            .acquire_timeout(Duration::from_millis(25))
            .connect_with(pool.connect_options().as_ref().clone())
            .await
            .unwrap();

        let owner_id = Uuid::new_v4();
        let created_by = Uuid::new_v4();
        let api_key_cache = crate::sync::api_key_cache::ApiKeyMetadataCache::empty();
        api_key_cache.replace(HashMap::from([(
            FLEX_TEST_KEY.to_string(),
            ApiKeyMetadata {
                owner_id,
                created_by,
                purpose: ApiKeyPurpose::Realtime,
                verified: true,
                zero_data_retention: false,
                hidden_batch_key: Some("sk-flex-hidden".to_string()),
            },
        )]));
        let flex_batch_key_resolver = crate::sync::api_key_cache::FlexBatchKeyResolver::new(main_pool.clone(), api_key_cache.clone());
        let (_requests_writer_task, requests_writer) =
            crate::inference::engine::writer::RequestsWriter::new(request_manager.clone(), 1, Duration::ZERO);
        let state = InferenceMiddlewareState {
            requests_writer,
            request_manager,
            daemon_id: OnwardsDaemonId(Uuid::new_v4()),
            loopback_base_url: "http://127.0.0.1:3001/ai".to_string(),
            dwctl_pool: main_pool.clone(),
            response_store,
            multi_step_tool_executor: Arc::new(crate::inference::tools::HttpToolExecutor::new(reqwest::Client::new(), None)),
            multi_step_http_client: Arc::new(fusillade::ReqwestHttpClient::new(
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Vec::new(),
            )),
            loop_config: onwards::LoopConfig::default(),
            image_normalizer: Arc::new(crate::image_normalizer::DisabledNormalizer),
            image_normalizer_enabled: false,
            unverified_requests_per_completion_hour: 0,
            flex_completion_window: "1h".to_string(),
            keystore: None,
            api_key_cache,
            flex_batch_key_resolver,
            onwards_targets: flex_access_targets(&[FLEX_TEST_KEY], HashMap::new(), Vec::new()),
        };
        let request_value = serde_json::json!({
            "model": FLEX_TEST_MODEL,
            "background": true,
            "tools": [{
                "type": "function",
                "name": "client_tool",
                "description": "client-side test tool",
                "parameters": {"type": "object"},
            }],
            "input": "hello",
        });

        let _held_connection = main_pool.acquire().await.unwrap();
        let response = tokio::time::timeout(
            Duration::from_millis(250),
            try_warm_path_background(&state, &request_value, Some(FLEX_TEST_KEY), FLEX_TEST_MODEL),
        )
        .await
        .expect("warm background attribution must not wait on the unavailable main pool")
        .expect("warm background setup must retain cached attribution");

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let stored_created_by: Option<String> = sqlx::query_scalar("SELECT created_by FROM requests")
            .fetch_one(&fusillade_pool)
            .await
            .unwrap();
        assert_eq!(stored_created_by.as_deref(), Some(owner_id.to_string().as_str()));
    }

    #[sqlx::test]
    async fn background_realtime_persists_cached_owner_while_main_pool_is_unavailable(pool: sqlx::PgPool) {
        use std::time::Duration;

        use axum::{Router, middleware, routing::post};
        use sqlx::postgres::PgPoolOptions;
        use sqlx_pool_router::TestDbPools;
        use tower::ServiceExt;

        let fusillade_pool = crate::test::utils::setup_fusillade_pool(&pool).await;
        let fusillade_pools = TestDbPools::new(fusillade_pool.clone()).await.unwrap();
        let request_manager = Arc::new(fusillade_arsenal::PostgresRequestManager::new(fusillade_pools, Default::default()));
        let response_store = Arc::new(response_store::FusilladeResponseStore::new(request_manager.clone()));
        let main_pool = PgPoolOptions::new()
            .max_connections(1)
            .min_connections(0)
            .connect_with(pool.connect_options().as_ref().clone())
            .await
            .unwrap();

        let owner_id = Uuid::new_v4();
        let created_by = Uuid::new_v4();
        let api_key_cache = crate::sync::api_key_cache::ApiKeyMetadataCache::empty();
        api_key_cache.replace(HashMap::from([(
            FLEX_TEST_KEY.to_string(),
            ApiKeyMetadata {
                owner_id,
                created_by,
                purpose: ApiKeyPurpose::Realtime,
                verified: true,
                zero_data_retention: false,
                hidden_batch_key: Some("sk-flex-hidden".to_string()),
            },
        )]));
        let (_requests_writer_task, requests_writer) =
            crate::inference::engine::writer::RequestsWriter::new(request_manager.clone(), 1, Duration::ZERO);
        let state = InferenceMiddlewareState {
            requests_writer,
            request_manager,
            daemon_id: OnwardsDaemonId(Uuid::new_v4()),
            loopback_base_url: "http://127.0.0.1:3001/ai".to_string(),
            dwctl_pool: main_pool.clone(),
            response_store,
            multi_step_tool_executor: Arc::new(crate::inference::tools::HttpToolExecutor::new(reqwest::Client::new(), None)),
            multi_step_http_client: Arc::new(fusillade::ReqwestHttpClient::new(
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Vec::new(),
            )),
            loop_config: onwards::LoopConfig::default(),
            image_normalizer: Arc::new(crate::image_normalizer::DisabledNormalizer),
            image_normalizer_enabled: false,
            unverified_requests_per_completion_hour: 0,
            flex_completion_window: "1h".to_string(),
            keystore: None,
            api_key_cache: api_key_cache.clone(),
            flex_batch_key_resolver: crate::sync::api_key_cache::FlexBatchKeyResolver::new(main_pool.clone(), api_key_cache),
            onwards_targets: flex_access_targets(&[FLEX_TEST_KEY], HashMap::new(), Vec::new()),
        };
        let app = Router::new()
            .route("/v1/responses", post(|| async { StatusCode::IM_A_TEAPOT }))
            .layer(middleware::from_fn_with_state(state, inference_middleware));
        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("authorization", format!("Bearer {FLEX_TEST_KEY}"))
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "model": FLEX_TEST_MODEL,
                    "background": true,
                    "tools": [],
                    "input": "hello",
                })
                .to_string(),
            ))
            .unwrap();

        let _held_connection = main_pool.acquire().await.unwrap();
        let response = tokio::time::timeout(Duration::from_millis(250), app.oneshot(request))
            .await
            .expect("background realtime attribution must not wait on the unavailable main pool")
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let stored_created_by: Option<String> = sqlx::query_scalar("SELECT created_by FROM requests")
            .fetch_one(&fusillade_pool)
            .await
            .unwrap();
        assert_eq!(stored_created_by.as_deref(), Some(owner_id.to_string().as_str()));
    }

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

    // Routing-decision tests for `warm_path_branch`. The whole point
    // of this PR is that flex `/v1/responses` must skip the warm
    // path and fall through to `handle_flex`; these tests pin that
    // contract so a future refactor that re-engages warm-path for
    // flex can't silently regress the routing back to the bug.

    #[test]
    fn warm_path_branch_flex_responses_falls_through_to_handle_flex() {
        // The bug being fixed: flex /v1/responses used to engage warm
        // path regardless of tier and run the loop inline at realtime
        // cost. After the fix, every flex case must fall through.
        for &background in &[false, true] {
            for &stream in &[false, true] {
                for &has_tools in &[false, true] {
                    assert_eq!(
                        warm_path_branch(true, true, background, stream, has_tools),
                        WarmPathBranch::FallThrough,
                        "flex /v1/responses must fall through (background={background}, stream={stream}, has_tools={has_tools})"
                    );
                }
            }
        }
    }

    #[test]
    fn warm_path_branch_realtime_responses_with_tools_picks_correct_warm_branch() {
        // Realtime /v1/responses with tools keeps the existing
        // warm-path behavior: stream → SSE, background → spawned
        // task, neither → blocking JSON.
        assert_eq!(warm_path_branch(true, false, false, true, true), WarmPathBranch::Stream);
        assert_eq!(warm_path_branch(true, false, true, false, true), WarmPathBranch::Background);
        assert_eq!(warm_path_branch(true, false, false, false, true), WarmPathBranch::Blocking);
    }

    #[test]
    fn warm_path_branch_realtime_responses_without_tools_falls_through() {
        // Without tools the multi-step loop has nothing to dispatch.
        // Fall through so onwards' single-step /v1/responses proxy
        // handles it — produces one tracking row via the standard
        // outlet path instead of record_step / response_steps.
        for &background in &[false, true] {
            for &stream in &[false, true] {
                assert_eq!(
                    warm_path_branch(true, false, background, stream, false),
                    WarmPathBranch::FallThrough,
                    "tool-free realtime /v1/responses must fall through (background={background}, stream={stream})"
                );
            }
        }
    }

    #[test]
    fn warm_path_branch_chat_completions_always_falls_through() {
        // Warm path is /v1/responses-only. Chat completions and
        // embeddings never engage it regardless of tier — they go
        // through the single-step proxy.
        for &has_tools in &[false, true] {
            assert_eq!(warm_path_branch(false, false, false, false, has_tools), WarmPathBranch::FallThrough);
            assert_eq!(warm_path_branch(false, true, false, false, has_tools), WarmPathBranch::FallThrough);
            assert_eq!(warm_path_branch(false, false, false, true, has_tools), WarmPathBranch::FallThrough);
        }
    }
}
