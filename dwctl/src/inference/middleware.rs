//! Axum middleware that routes inference requests based on `service_tier` and `background`.
//!
//! Applied to the onwards router for all inference POST requests
//! (`/v1/responses`, `/v1/chat/completions`, `/v1/embeddings`).
//!
//! ## Routing
//!
//! - `priority` / `default` / `auto` (realtime): proxies via onwards.
//!   Background requests durably admit a `processing` row before returning
//!   202; ordinary requests synthesize their terminal row on completion.
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
use onwards::errors::OnwardsErrorResponse;
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

    let warm_attempt = match warm_path_branch(is_responses_api, is_flex, background, stream_requested, has_tools) {
        WarmPathBranch::Stream => try_warm_path_stream(&state, &request_value, api_key.as_deref(), model).await,
        WarmPathBranch::Blocking => try_warm_path_blocking(&state, &request_value, api_key.as_deref(), model).await,
        WarmPathBranch::Background => try_warm_path_background(&state, &request_value, api_key.as_deref(), model).await,
        WarmPathBranch::FallThrough => Ok(None),
    };
    match warm_attempt {
        Ok(Some(response)) | Err(response) => return response,
        Ok(None) => {}
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
        let Some(key) = api_key.as_deref() else {
            return OnwardsErrorResponse::unauthorized().into_response();
        };
        let Some(metadata) = state.api_key_cache.get(key) else {
            return OnwardsErrorResponse::forbidden().into_response();
        };
        Some(metadata.owner_id.to_string())
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
fn enqueue_error_response() -> Response {
    Response::builder()
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
        .expect("static enqueue error response is valid")
}

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
    let endpoint_for_header = realtime_input.path.clone();

    if background {
        // Background mode: create row synchronously so it exists before
        // we return the 202 (client will poll immediately).
        if let Err(e) = state.requests_writer.admit_realtime(realtime_input).await {
            tracing::error!(error = %e, "Failed to admit realtime tracking row");
            return enqueue_error_response();
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
    if let Err(e) = state.requests_writer.admit_flex(flex_input).await {
        tracing::error!(error = %e, "Failed to create flex row in fusillade");
        return enqueue_error_response();
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
    if let Err(e) = state.requests_writer.admit_flex(flex_input).await {
        tracing::error!(error = %e, "Failed to create flex chat-completions batch in fusillade");
        return enqueue_error_response();
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
        state.requests_writer.clone(),
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
        state.requests_writer.clone(),
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
/// streaming handler. `Ok(None)` means the request is not applicable to
/// this path (currently only a missing bearer key); `Err(response)` is a
/// fatal admission failure and must not fall through to ordinary dispatch.
type WarmPathAttempt = Result<Option<Response>, Response>;

async fn try_warm_path_stream<P: PoolProvider + Clone + Send + Sync + 'static>(
    state: &InferenceMiddlewareState<P>,
    request_value: &serde_json::Value,
    api_key: Option<&str>,
    model: &str,
) -> WarmPathAttempt {
    let Some(api_key) = api_key else {
        return Ok(None);
    };
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
    Ok(Some(sse.into_response()))
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
) -> WarmPathAttempt {
    let Some(api_key) = api_key else {
        return Ok(None);
    };
    let (request_id, resolved, upstream) = warm_path_setup(state, request_value, api_key, model).await?;

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
    Ok(Some((status, Json(body)).into_response()))
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
) -> WarmPathAttempt {
    let Some(api_key) = api_key else {
        return Ok(None);
    };
    let (request_id, resolved, upstream) = warm_path_setup(state, request_value, api_key, model).await?;

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

    Ok(Some((StatusCode::ACCEPTED, Json(response_body)).into_response()))
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
/// A single `/v1/responses` fusillade row is admitted up front through
/// the commit-acknowledging writer (state=`processing`,
/// id=`head_step_uuid`). That row is the response: `record_step`'s head
/// branch reuses it instead of inserting another, and
/// `finalize_head_request` completes it when the loop terminates.
/// Descendant model_call steps still mint their own sub-request rows.
/// The asymmetry with the daemon-driven flex path (which uses
/// `handle_flex` to admit the same shape of row in `pending` state) is
/// just state at insert: warm path doesn't go through the daemon's
/// claim cycle.
async fn warm_path_setup<P: PoolProvider + Clone + Send + Sync + 'static>(
    state: &InferenceMiddlewareState<P>,
    request_value: &serde_json::Value,
    api_key: &str,
    model: &str,
) -> Result<(uuid::Uuid, Arc<crate::inference::tools::ResolvedToolSet>, onwards::UpstreamTarget), Response> {
    let Some(metadata) = state.api_key_cache.get(api_key) else {
        return Err(OnwardsErrorResponse::forbidden().into_response());
    };
    let created_by = metadata.owner_id.to_string();

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
        created_by: Some(created_by.clone()),
        base_url: state.loopback_base_url.clone(),
        resolved_tool_names,
    };
    if let Err(e) = state.response_store.register_pending_with_id(head_step_uuid, pending) {
        tracing::error!(
            error = %e,
            request_id = %head_step_uuid,
            "warm-path: failed to register pending input — aborting warm path",
        );
        return Err(enqueue_error_response());
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
        created_by,
    };
    if let Err(e) = state.requests_writer.admit_realtime(realtime_input).await {
        tracing::error!(
            error = %e,
            request_id = %head_step_uuid,
            "warm-path: failed to create /v1/responses tracking row — aborting warm path",
        );
        state.response_store.unregister_pending(&head_step_uuid.to_string());
        return Err(enqueue_error_response());
    }

    // Endpoint + path are split (not pre-concatenated) so fusillade
    // can match `/v1/chat/completions` against its streamable_endpoints
    // list and pick the streaming branch when the user requested SSE.
    let upstream = onwards::UpstreamTarget {
        endpoint: state.loopback_base_url.clone(),
        path: "/v1/chat/completions".to_string(),
        api_key: Some(api_key.to_string()),
    };

    Ok((head_step_uuid, resolved, upstream))
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
            hidden_batch_key_is_child: true,
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

    struct RealtimeAuthFixture {
        app: axum::Router,
        state: InferenceMiddlewareState<sqlx_pool_router::TestDbPools>,
        fusillade_pool: sqlx::PgPool,
        dispatches: Arc<std::sync::atomic::AtomicUsize>,
        writer_shutdown: tokio_util::sync::CancellationToken,
        writer_task: tokio::task::JoinHandle<()>,
    }

    async fn realtime_auth_fixture(pool: &sqlx::PgPool) -> RealtimeAuthFixture {
        use std::time::Duration;

        use axum::{Router, middleware, routing::post};
        use sqlx_pool_router::TestDbPools;

        let fusillade_pool = crate::test::utils::setup_fusillade_pool(pool).await;
        let fusillade_pools = TestDbPools::new(fusillade_pool.clone()).await.unwrap();
        let request_manager = Arc::new(fusillade_arsenal::PostgresRequestManager::new(fusillade_pools, Default::default()));
        let (requests_writer_task, requests_writer) =
            crate::inference::engine::writer::RequestsWriter::new(request_manager.clone(), 1, Duration::ZERO);
        let response_store = Arc::new(response_store::FusilladeResponseStore::new(
            request_manager.clone(),
            requests_writer.clone(),
        ));
        let api_key_cache = crate::sync::api_key_cache::ApiKeyMetadataCache::empty();
        let state = InferenceMiddlewareState {
            requests_writer,
            request_manager,
            daemon_id: OnwardsDaemonId(Uuid::new_v4()),
            loopback_base_url: "http://127.0.0.1:3001/ai".to_string(),
            dwctl_pool: pool.clone(),
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
            flex_batch_key_resolver: crate::sync::api_key_cache::FlexBatchKeyResolver::new(pool.clone(), api_key_cache),
            onwards_targets: flex_access_targets(&[], HashMap::new(), Vec::new()),
        };
        let dispatches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_counter = dispatches.clone();
        let app = Router::new()
            .route(
                "/v1/responses",
                post(move || {
                    let dispatch_counter = dispatch_counter.clone();
                    async move {
                        dispatch_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        StatusCode::IM_A_TEAPOT
                    }
                }),
            )
            .layer(middleware::from_fn_with_state(state.clone(), inference_middleware));
        let writer_shutdown = tokio_util::sync::CancellationToken::new();
        let writer_task = tokio::spawn(requests_writer_task.run(writer_shutdown.clone()));

        RealtimeAuthFixture {
            app,
            state,
            fusillade_pool,
            dispatches,
            writer_shutdown,
            writer_task,
        }
    }

    async fn response_json(response: Response) -> serde_json::Value {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn responses_burst_flex_request(input: String) -> Request<Body> {
        Request::builder()
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
                    "input": input,
                })
                .to_string(),
            ))
            .unwrap()
    }

    fn responses_burst_get_request(request_id: Uuid) -> Request<Body> {
        Request::builder()
            .uri(format!("/v1/responses/resp_{request_id}"))
            .header("authorization", format!("Bearer {FLEX_TEST_KEY}"))
            .body(Body::empty())
            .unwrap()
    }

    async fn responses_burst_json(response: Response) -> Result<(StatusCode, serde_json::Value), String> {
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .map_err(|error| format!("failed to read response body: {error}"))?;
        let body = serde_json::from_slice(&body).map_err(|error| format!("HTTP {status} returned non-JSON body: {error}"))?;
        Ok((status, body))
    }

    fn responses_burst_created_ids(
        responses: Vec<Result<(StatusCode, serde_json::Value), String>>,
        expected_count: usize,
    ) -> Result<Vec<Uuid>, String> {
        if responses.len() != expected_count {
            return Err(format!("expected {expected_count} create responses, got {}", responses.len()));
        }

        let mut ids = Vec::with_capacity(expected_count);
        let mut unique = HashSet::with_capacity(expected_count);
        for (index, response) in responses.into_iter().enumerate() {
            let (status, body) = response?;
            if status != StatusCode::ACCEPTED {
                return Err(format!("create {index} returned HTTP {status}: {body}"));
            }
            for (field, expected) in [("object", "response"), ("status", "queued"), ("model", FLEX_TEST_MODEL)] {
                if body[field].as_str() != Some(expected) {
                    return Err(format!("create {index} returned unexpected {field}: {body}"));
                }
            }
            if body["background"].as_bool() != Some(true) {
                return Err(format!("create {index} did not preserve background=true: {body}"));
            }
            let response_id = body["id"]
                .as_str()
                .ok_or_else(|| format!("create {index} returned no response id: {body}"))?;
            let raw_id = response_id
                .strip_prefix("resp_")
                .ok_or_else(|| format!("create {index} returned malformed response id: {response_id}"))?;
            let request_id =
                Uuid::parse_str(raw_id).map_err(|error| format!("create {index} returned invalid UUID {response_id}: {error}"))?;
            if !unique.insert(request_id) {
                return Err(format!("create {index} repeated response id {response_id}"));
            }
            ids.push(request_id);
        }
        Ok(ids)
    }

    fn responses_burst_validate_gets(
        responses: Vec<(Uuid, Result<(StatusCode, serde_json::Value), String>)>,
        expected_count: usize,
    ) -> Result<(), String> {
        if responses.len() != expected_count {
            return Err(format!("expected {expected_count} GET responses, got {}", responses.len()));
        }
        for (expected_id, response) in responses {
            let (status, body) = response?;
            if status != StatusCode::OK {
                return Err(format!("GET resp_{expected_id} returned HTTP {status}: {body}"));
            }
            let expected_response_id = format!("resp_{expected_id}");
            if body["id"].as_str() != Some(expected_response_id.as_str())
                || body["object"].as_str() != Some("response")
                || body["status"].as_str() != Some("queued")
                || body["model"].as_str() != Some(FLEX_TEST_MODEL)
                // The durable outbound template intentionally strips admission-only
                // `background`; queued retrieval therefore uses its canonical default.
                || body["background"].as_bool() != Some(false)
            {
                return Err(format!("GET resp_{expected_id} returned the wrong owner-visible object: {body}"));
            }
        }
        Ok(())
    }

    #[sqlx::test]
    async fn background_realtime_missing_bearer_returns_canonical_unauthorized_before_admission(pool: sqlx::PgPool) {
        use std::time::Duration;

        use tower::ServiceExt;

        let fixture = realtime_auth_fixture(&pool).await;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
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

        let response = tokio::time::timeout(Duration::from_secs(5), fixture.app.oneshot(request))
            .await
            .expect("missing bearer must be rejected without waiting for admission")
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response_json(response).await,
            serde_json::json!({
                "error": {
                    "message": "Please supply an authentication token to access this resource",
                    "type": "invalid_request_error",
                    "param": null,
                    "code": "unauthenticated",
                }
            })
        );
        assert_eq!(fixture.dispatches.load(std::sync::atomic::Ordering::SeqCst), 0);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM requests")
            .fetch_one(&fixture.fusillade_pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);

        fixture.writer_shutdown.cancel();
        fixture.writer_task.await.unwrap();
    }

    #[sqlx::test]
    async fn background_realtime_unknown_bearer_returns_canonical_forbidden_before_admission(pool: sqlx::PgPool) {
        use std::time::Duration;

        use tower::ServiceExt;

        let fixture = realtime_auth_fixture(&pool).await;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("authorization", "Bearer sk-unknown")
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

        let response = tokio::time::timeout(Duration::from_secs(5), fixture.app.oneshot(request))
            .await
            .expect("unknown bearer must be rejected without waiting for admission")
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response_json(response).await,
            serde_json::json!({
                "error": {
                    "message": "Forbidden",
                    "type": "invalid_request_error",
                    "param": null,
                    "code": "forbidden",
                }
            })
        );
        assert_eq!(fixture.dispatches.load(std::sync::atomic::Ordering::SeqCst), 0);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM requests")
            .fetch_one(&fixture.fusillade_pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);

        fixture.writer_shutdown.cancel();
        fixture.writer_task.await.unwrap();
    }

    #[sqlx::test]
    async fn warm_path_missing_bearer_falls_through_without_admission(pool: sqlx::PgPool) {
        let fixture = realtime_auth_fixture(&pool).await;
        let request_value = serde_json::json!({
            "model": FLEX_TEST_MODEL,
            "background": false,
            "tools": [{
                "type": "function",
                "name": "client_tool",
                "parameters": {"type": "object"},
            }],
            "input": "hello",
        });

        let attempt = try_warm_path_blocking(&fixture.state, &request_value, None, FLEX_TEST_MODEL).await;

        assert!(matches!(attempt, Ok(None)));
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM requests")
            .fetch_one(&fixture.fusillade_pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);

        fixture.writer_shutdown.cancel();
        fixture.writer_task.await.unwrap();
    }

    #[sqlx::test]
    async fn warm_path_unknown_bearer_returns_canonical_forbidden_before_admission(pool: sqlx::PgPool) {
        use std::time::Duration;

        use tower::ServiceExt;

        let fixture = realtime_auth_fixture(&pool).await;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("authorization", "Bearer sk-unknown")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "model": FLEX_TEST_MODEL,
                    "background": true,
                    "tools": [{
                        "type": "function",
                        "name": "client_tool",
                        "parameters": {"type": "object"},
                    }],
                    "input": "hello",
                })
                .to_string(),
            ))
            .unwrap();

        let response = tokio::time::timeout(Duration::from_secs(5), fixture.app.oneshot(request))
            .await
            .expect("unknown warm-path bearer must be rejected without waiting for admission")
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response_json(response).await,
            serde_json::json!({
                "error": {
                    "message": "Forbidden",
                    "type": "invalid_request_error",
                    "param": null,
                    "code": "forbidden",
                }
            })
        );
        assert_eq!(fixture.dispatches.load(std::sync::atomic::Ordering::SeqCst), 0);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM requests")
            .fetch_one(&fixture.fusillade_pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);

        fixture.writer_shutdown.cancel();
        fixture.writer_task.await.unwrap();
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
        let (requests_writer_task, requests_writer) =
            crate::inference::engine::writer::RequestsWriter::new(request_manager.clone(), 1, Duration::ZERO);
        let response_store = Arc::new(response_store::FusilladeResponseStore::new(
            request_manager.clone(),
            requests_writer.clone(),
        ));

        let main_pool = PgPoolOptions::new()
            .max_connections(1)
            .min_connections(0)
            .acquire_timeout(Duration::from_millis(25))
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
                hidden_batch_key_is_child: true,
            },
        )]));
        let flex_batch_key_resolver = crate::sync::api_key_cache::FlexBatchKeyResolver::new(main_pool.clone(), api_key_cache.clone());
        let onwards_targets = flex_access_targets(&[FLEX_TEST_KEY, hidden_key], HashMap::new(), Vec::new());

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
        let mut request_task = tokio::spawn(app.oneshot(request));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut request_task).await.is_err(),
            "Flex admission must wait for the writer's commit acknowledgement"
        );
        let rows_before_writer: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM requests")
            .fetch_one(&fusillade_pool)
            .await
            .unwrap();
        assert_eq!(rows_before_writer, 0, "the pending row must not be visible before writer commit");

        let writer_shutdown = tokio_util::sync::CancellationToken::new();
        let writer_task = tokio::spawn(requests_writer_task.run(writer_shutdown.clone()));
        let response = tokio::time::timeout(Duration::from_secs(5), request_task)
            .await
            .expect("cached Flex validation must not acquire the unavailable main pool")
            .unwrap()
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let (stored_key, created_by): (String, Option<String>) =
            sqlx::query_as("SELECT t.api_key, r.created_by FROM requests r JOIN request_templates t ON t.id = r.template_id")
                .fetch_one(&fusillade_pool)
                .await
                .unwrap();
        assert_eq!(stored_key, hidden_key);
        assert_eq!(created_by.as_deref(), Some(owner_id.to_string().as_str()));

        writer_shutdown.cancel();
        writer_task.await.unwrap();
    }

    #[sqlx::test]
    async fn responses_burst_survives_low_pools_and_batches_durable_creates(pool: sqlx::PgPool) {
        use std::sync::OnceLock;
        use std::time::Duration;

        use axum::{
            Router, middleware,
            routing::{get, post},
        };
        use futures::future::join_all;
        use sqlx::postgres::PgPoolOptions;
        use tower::ServiceExt;

        const INITIAL_CREATES: usize = 100;
        const MIXED_CREATES: usize = 34;
        const MIXED_GETS: usize = 33;
        const MIXED_COMPLETIONS: usize = 33;
        const COMPLETION_MODEL: &str = "responses-burst-realtime";

        let main_pool = PgPoolOptions::new()
            .max_connections(1)
            .min_connections(0)
            .acquire_timeout(Duration::from_millis(25))
            .connect_with(pool.connect_options().as_ref().clone())
            .await
            .unwrap();

        let migration_pool = crate::test::utils::setup_fusillade_pool(&pool).await;
        let fusillade_pool = PgPoolOptions::new()
            .max_connections(4)
            .min_connections(0)
            .acquire_timeout(Duration::from_millis(50))
            .connect_with(migration_pool.connect_options().as_ref().clone())
            .await
            .unwrap();
        migration_pool.close().await;

        let request_manager = Arc::new(fusillade_arsenal::PostgresRequestManager::new(
            fusillade_pool.clone(),
            Default::default(),
        ));
        let response_step_manager = Arc::new(request_manager.response_step_manager());
        let (requests_writer_task, requests_writer) =
            crate::inference::engine::writer::RequestsWriter::new(request_manager.clone(), INITIAL_CREATES, Duration::from_millis(2));
        let writer_observer = requests_writer.test_observer();
        let response_store = Arc::new(
            response_store::FusilladeResponseStore::new(request_manager.clone(), requests_writer.clone())
                .with_step_manager(response_step_manager.clone()),
        );

        let owner = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        sqlx::query("UPDATE users SET verified = true WHERE id = $1")
            .bind(owner.id)
            .execute(&pool)
            .await
            .expect("mark burst owner verified");
        let presented_key = crate::test::utils::create_test_api_key_for_user(&pool, owner.id).await;
        sqlx::query("UPDATE api_keys SET secret = $1, spend_limit = 10 WHERE id = $2")
            .bind(FLEX_TEST_KEY)
            .bind(presented_key.id)
            .execute(&pool)
            .await
            .expect("install deterministic capped burst key");
        let hidden_key = {
            let mut conn = pool.acquire().await.expect("acquire cap-child setup connection");
            crate::db::handlers::api_keys::ApiKeys::new(&mut conn)
                .get_or_create_child_hidden_key(presented_key.id)
                .await
                .expect("create cap-scoped execution key")
                .0
        };
        let owner_id = owner.id;
        let api_key_cache = crate::sync::api_key_cache::initial_cache(&main_pool)
            .await
            .expect("load capped burst key metadata");
        let cached_key = api_key_cache.get(FLEX_TEST_KEY).expect("presented burst key must be cached");
        assert_eq!(cached_key.owner_id, owner_id);
        assert_eq!(cached_key.hidden_batch_key.as_deref(), Some(hidden_key.as_str()));
        assert!(
            cached_key.hidden_batch_key_is_child,
            "measured hot phase requires an authoritative cap-child cache entry"
        );
        let flex_batch_key_resolver = crate::sync::api_key_cache::FlexBatchKeyResolver::new(main_pool.clone(), api_key_cache.clone());
        let primed_flex_key = flex_batch_key_resolver
            .resolve_hidden_batch_key(FLEX_TEST_KEY)
            .await
            .expect("hidden Flex key setup should succeed")
            .expect("presented Flex key should resolve");
        assert_eq!(primed_flex_key.secret, hidden_key);
        let inference_state = InferenceMiddlewareState {
            requests_writer: requests_writer.clone(),
            request_manager: request_manager.clone(),
            daemon_id: OnwardsDaemonId(Uuid::new_v4()),
            loopback_base_url: "http://127.0.0.1:3001/ai".to_string(),
            dwctl_pool: main_pool.clone(),
            response_store: response_store.clone(),
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
            flex_batch_key_resolver: flex_batch_key_resolver.clone(),
            onwards_targets: flex_access_targets(&[FLEX_TEST_KEY, hidden_key.as_str()], HashMap::new(), Vec::new()),
        };

        let config = crate::test::utils::create_test_config();
        let shared_config = crate::SharedConfig::new(config.clone());
        underway::run_migrations(&pool).await.expect("underway migrations should succeed");
        let task_runner = Arc::new(
            crate::tasks::TaskRunner::new(
                pool.clone(),
                crate::tasks::TaskState {
                    request_manager: request_manager.clone(),
                    dwctl_pool: pool.clone(),
                    config: shared_config.clone(),
                    encryption_key: None,
                    ingest_file_job: Arc::new(OnceLock::new()),
                    activate_batch_job: Arc::new(OnceLock::new()),
                    create_batch_job: Arc::new(OnceLock::new()),
                    cascade_batch_state_job: Arc::new(OnceLock::new()),
                },
                &crate::config::TaskWorkersConfig {
                    create_batch_workers: 0,
                    cascade_batch_state_workers: 0,
                    purge_user_data_workers: 0,
                    response_writer_batch_size: 0,
                    response_writer_max_linger_ms: 0,
                },
            )
            .await
            .expect("task runner should build before measured pool contention"),
        );
        let app_state = crate::AppState::builder()
            .db(main_pool.clone())
            .config(shared_config)
            .request_manager(request_manager.clone())
            .requests_writer(requests_writer.clone())
            .task_runner(task_runner)
            .limiters(crate::limits::Limiters::new(&config.limits))
            .response_store(response_store)
            .response_step_manager(response_step_manager)
            .image_normalizer(Arc::new(crate::image_normalizer::DisabledNormalizer) as Arc<dyn crate::image_normalizer::ImageNormalizer>)
            .api_key_cache(api_key_cache)
            .flex_batch_key_resolver(flex_batch_key_resolver)
            .build();
        let app = Router::new()
            .route("/v1/responses", post(|| async { StatusCode::IM_A_TEAPOT }))
            .route(
                "/v1/responses/{response_id}",
                get(crate::inference::handler::get_response::<sqlx::PgPool>),
            )
            .layer(middleware::from_fn_with_state(inference_state, inference_middleware))
            .with_state(app_state);

        let held_main_connection = main_pool.acquire().await.unwrap();
        let held_fusillade_connection_a = fusillade_pool.acquire().await.unwrap();
        let held_fusillade_connection_b = fusillade_pool.acquire().await.unwrap();

        let phase_a_app = app.clone();
        let mut phase_a_task = tokio::spawn(async move {
            join_all((0..INITIAL_CREATES).map(|index| {
                let app = phase_a_app.clone();
                async move {
                    let response = app
                        .oneshot(responses_burst_flex_request(format!("initial-{index}")))
                        .await
                        .expect("Axum router is infallible");
                    responses_burst_json(response).await
                }
            }))
            .await
        });

        let writer_shutdown = tokio_util::sync::CancellationToken::new();
        let writer_shutdown_task = writer_shutdown.clone();
        let (start_writer, writer_start) = tokio::sync::oneshot::channel();
        let mut writer_task = tokio::spawn(async move {
            if writer_start.await.is_ok() {
                requests_writer_task.run(writer_shutdown_task).await;
            }
        });

        let hot_result: Result<(), String> = tokio::time::timeout(Duration::from_secs(10), async {
            let queue_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            loop {
                if requests_writer.queued_commands() == INITIAL_CREATES {
                    break;
                }
                if phase_a_task.is_finished() {
                    let early = (&mut phase_a_task)
                        .await
                        .map_err(|error| format!("initial HTTP group failed early: {error}"))?;
                    let statuses = early
                        .iter()
                        .map(|response| match response {
                            Ok((status, _)) => status.to_string(),
                            Err(error) => error.clone(),
                        })
                        .collect::<Vec<_>>();
                    return Err(format!("initial HTTP group completed before 100 commands queued: {statuses:?}"));
                }
                if tokio::time::Instant::now() >= queue_deadline {
                    return Err(format!(
                        "timed out with {} of {INITIAL_CREATES} commands queued",
                        requests_writer.queued_commands()
                    ));
                }
                tokio::task::yield_now().await;
            }

            if writer_observer.create_transaction_attempts() != 0 {
                return Err("create transaction started before the writer was released".to_string());
            }
            let rows_before_writer: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fusillade.requests")
                .fetch_one(&pool)
                .await
                .map_err(|error| format!("count rows before writer start: {error}"))?;
            if rows_before_writer != 0 {
                return Err(format!(
                    "{rows_before_writer} rows were visible before durable writer acknowledgement"
                ));
            }
            if phase_a_task.is_finished() {
                return Err("HTTP creates completed before durable writer acknowledgement".to_string());
            }

            start_writer.send(()).map_err(|_| "writer start gate closed".to_string())?;
            let phase_a_responses = (&mut phase_a_task)
                .await
                .map_err(|error| format!("initial HTTP group panicked: {error}"))?;
            let phase_a_ids = responses_burst_created_ids(phase_a_responses, INITIAL_CREATES)?;

            let create_attempts = writer_observer.create_transaction_attempts();
            if create_attempts != 1 || create_attempts >= INITIAL_CREATES {
                return Err(format!("100 prequeued creates used {create_attempts} storage transaction attempts"));
            }

            let phase_a_rows: Vec<(Uuid, String, String, Option<String>, String, Option<Uuid>, Uuid)> = sqlx::query_as(
                "SELECT r.id, r.state, r.service_tier, r.created_by, r.model, \
                            r.batch_id, t.id \
                       FROM fusillade.requests r \
                       JOIN fusillade.request_templates t ON t.id = r.template_id \
                      WHERE r.id = ANY($1)",
            )
            .bind(&phase_a_ids)
            .fetch_all(&pool)
            .await
            .map_err(|error| format!("observe initial durable rows: {error}"))?;
            let expected_phase_a: HashSet<Uuid> = phase_a_ids.iter().copied().collect();
            let stored_phase_a: HashSet<Uuid> = phase_a_rows.iter().map(|row| row.0).collect();
            let template_ids: HashSet<Uuid> = phase_a_rows.iter().map(|row| row.6).collect();
            if phase_a_rows.len() != INITIAL_CREATES || stored_phase_a != expected_phase_a || template_ids.len() != INITIAL_CREATES {
                return Err(format!(
                    "acknowledged create set did not match durable request/template rows: \
                         requests={}, templates={}",
                    phase_a_rows.len(),
                    template_ids.len()
                ));
            }
            let owner = owner_id.to_string();
            if let Some(invalid) = phase_a_rows.iter().find(|row| {
                row.1 != "pending"
                    || row.2 != "flex"
                    || row.3.as_deref() != Some(owner.as_str())
                    || row.4 != FLEX_TEST_MODEL
                    || row.5.is_some()
            }) {
                return Err(format!("initial create row has invalid lifecycle shape: {invalid:?}"));
            }

            let phase_b_responses = join_all(phase_a_ids.iter().copied().map(|request_id| {
                let app = app.clone();
                async move {
                    let response = app
                        .oneshot(responses_burst_get_request(request_id))
                        .await
                        .expect("Axum router is infallible");
                    (request_id, responses_burst_json(response).await)
                }
            }))
            .await;
            responses_burst_validate_gets(phase_b_responses, INITIAL_CREATES)?;

            let mixed_completion_records: Vec<(Uuid, String)> = (0..MIXED_COMPLETIONS)
                .map(|index| {
                    (
                        Uuid::new_v4(),
                        serde_json::json!({
                            "output": format!("mixed-completion-{index}")
                        })
                        .to_string(),
                    )
                })
                .collect();
            let mixed_create_app = app.clone();
            let mixed_get_app = app.clone();
            let mixed_writer = requests_writer.clone();
            let mixed_create_future = join_all((0..MIXED_CREATES).map(|index| {
                let app = mixed_create_app.clone();
                async move {
                    let response = app
                        .oneshot(responses_burst_flex_request(format!("mixed-{index}")))
                        .await
                        .expect("Axum router is infallible");
                    responses_burst_json(response).await
                }
            }));
            let mixed_get_future = join_all(phase_a_ids.iter().take(MIXED_GETS).copied().map(|request_id| {
                let app = mixed_get_app.clone();
                async move {
                    let response = app
                        .oneshot(responses_burst_get_request(request_id))
                        .await
                        .expect("Axum router is infallible");
                    (request_id, responses_burst_json(response).await)
                }
            }));
            let mixed_completion_future = join_all(mixed_completion_records.iter().cloned().map(|(request_id, response_body)| {
                let writer = mixed_writer.clone();
                let owner = owner.clone();
                async move {
                    let started_at = chrono::DateTime::from_timestamp_millis(1_700_000_000_000).unwrap();
                    let completed_at = started_at + chrono::Duration::milliseconds(1);
                    writer
                        .complete_realtime(crate::inference::engine::writer::RawCompletedRequest {
                            request_id,
                            status_code: 200,
                            response_body,
                            request_body: serde_json::json!({
                                "model": COMPLETION_MODEL,
                                "input": format!("completion-{request_id}"),
                            })
                            .to_string(),
                            model: COMPLETION_MODEL.to_string(),
                            endpoint: "/v1/responses".to_string(),
                            api_key: FLEX_TEST_KEY.to_string(),
                            created_by: owner,
                            started_at,
                            completed_at,
                        })
                        .await
                        .map_err(|error| format!("completion {request_id} admission failed: {error}"))
                }
            }));
            let (mixed_create_responses, mixed_get_responses, mixed_completion_results) =
                tokio::join!(mixed_create_future, mixed_get_future, mixed_completion_future,);
            let mixed_create_ids = responses_burst_created_ids(mixed_create_responses, MIXED_CREATES)?;
            responses_burst_validate_gets(mixed_get_responses, MIXED_GETS)?;
            for completion in mixed_completion_results {
                completion?;
            }

            writer_shutdown.cancel();
            tokio::time::timeout(Duration::from_secs(3), &mut writer_task)
                .await
                .map_err(|_| "writer did not drain within 3 seconds".to_string())?
                .map_err(|error| format!("writer task panicked: {error}"))?;

            let mixed_flex_rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
                "SELECT r.id, t.id \
                       FROM fusillade.requests r \
                       JOIN fusillade.request_templates t ON t.id = r.template_id \
                      WHERE r.id = ANY($1)",
            )
            .bind(&mixed_create_ids)
            .fetch_all(&pool)
            .await
            .map_err(|error| format!("observe mixed Flex rows: {error}"))?;
            if mixed_flex_rows.len() != MIXED_CREATES
                || mixed_flex_rows.iter().map(|row| row.0).collect::<HashSet<_>>() != mixed_create_ids.iter().copied().collect()
                || mixed_flex_rows.iter().map(|row| row.1).collect::<HashSet<_>>().len() != MIXED_CREATES
            {
                return Err("mixed Flex acknowledgements did not match durable request/template rows".to_string());
            }

            let completion_ids: Vec<Uuid> = mixed_completion_records.iter().map(|row| row.0).collect();
            let completion_rows: Vec<(Uuid, String, Option<i16>, Option<String>, Option<String>, String)> = sqlx::query_as(
                "SELECT id, state, response_status, response_body, created_by, model \
                       FROM fusillade.requests \
                      WHERE id = ANY($1)",
            )
            .bind(&completion_ids)
            .fetch_all(&pool)
            .await
            .map_err(|error| format!("observe mixed completion rows: {error}"))?;
            let expected_completion_bodies: HashMap<Uuid, &str> =
                mixed_completion_records.iter().map(|(id, body)| (*id, body.as_str())).collect();
            if completion_rows.len() != MIXED_COMPLETIONS {
                return Err(format!(
                    "expected {MIXED_COMPLETIONS} completion rows, got {}",
                    completion_rows.len()
                ));
            }
            if let Some(invalid) = completion_rows.iter().find(|row| {
                row.1 != "completed"
                    || row.2 != Some(200)
                    || row.3.as_deref() != expected_completion_bodies.get(&row.0).copied()
                    || row.4.as_deref() != Some(owner.as_str())
                    || row.5 != COMPLETION_MODEL
            }) {
                return Err(format!("mixed completion row has invalid terminal state: {invalid:?}"));
            }

            let original_ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM fusillade.requests WHERE id = ANY($1)")
                .bind(&phase_a_ids)
                .fetch_all(&pool)
                .await
                .map_err(|error| format!("recheck original rows: {error}"))?;
            if original_ids.iter().copied().collect::<HashSet<_>>() != expected_phase_a {
                return Err("mixed pass changed or removed an original response row".to_string());
            }

            let total_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fusillade.requests")
                .fetch_one(&pool)
                .await
                .map_err(|error| format!("count final response rows: {error}"))?;
            let expected_total = INITIAL_CREATES + MIXED_CREATES + MIXED_COMPLETIONS;
            if total_rows != expected_total as i64 {
                return Err(format!("expected {expected_total} total rows after mixed pass, got {total_rows}"));
            }
            Ok(())
        })
        .await
        .map_err(|_| "responses burst hot phase exceeded 10 seconds".to_string())
        .and_then(|result| result);

        if let Err(error) = hot_result {
            phase_a_task.abort();
            writer_shutdown.cancel();
            if !writer_task.is_finished() {
                let _ = tokio::time::timeout(Duration::from_secs(3), &mut writer_task).await;
            }
            drop(held_fusillade_connection_b);
            drop(held_fusillade_connection_a);
            drop(held_main_connection);
            panic!("{error}");
        }

        drop(held_fusillade_connection_b);
        drop(held_fusillade_connection_a);
        drop(held_main_connection);
    }

    #[sqlx::test]
    async fn warm_background_persists_cached_owner_while_main_pool_is_unavailable(pool: sqlx::PgPool) {
        use std::time::Duration;

        use sqlx::postgres::PgPoolOptions;
        use sqlx_pool_router::TestDbPools;

        let fusillade_pool = crate::test::utils::setup_fusillade_pool(&pool).await;
        let fusillade_pools = TestDbPools::new(fusillade_pool.clone()).await.unwrap();
        let request_manager = Arc::new(fusillade_arsenal::PostgresRequestManager::new(fusillade_pools, Default::default()));
        let (requests_writer_task, requests_writer) =
            crate::inference::engine::writer::RequestsWriter::new(request_manager.clone(), 1, Duration::ZERO);
        let response_store = Arc::new(response_store::FusilladeResponseStore::new(
            request_manager.clone(),
            requests_writer.clone(),
        ));

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
                hidden_batch_key_is_child: true,
            },
        )]));
        let flex_batch_key_resolver = crate::sync::api_key_cache::FlexBatchKeyResolver::new(main_pool.clone(), api_key_cache.clone());
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
        let warm_state = state.clone();
        let warm_request = request_value.clone();
        let mut warm_task =
            tokio::spawn(async move { try_warm_path_background(&warm_state, &warm_request, Some(FLEX_TEST_KEY), FLEX_TEST_MODEL).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut warm_task).await.is_err(),
            "warm-path setup must wait for durable admission before spawning its loop"
        );
        let rows_before_writer: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM requests")
            .fetch_one(&fusillade_pool)
            .await
            .unwrap();
        assert_eq!(rows_before_writer, 0);

        let writer_shutdown = tokio_util::sync::CancellationToken::new();
        let writer_task = tokio::spawn(requests_writer_task.run(writer_shutdown.clone()));
        let response = tokio::time::timeout(Duration::from_secs(5), warm_task)
            .await
            .expect("warm background attribution must not wait on the unavailable main pool")
            .unwrap()
            .expect("warm background setup must retain cached attribution")
            .expect("warm background admission must succeed");

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let stored_created_by: Option<String> = sqlx::query_scalar("SELECT created_by FROM requests")
            .fetch_one(&fusillade_pool)
            .await
            .unwrap();
        assert_eq!(stored_created_by.as_deref(), Some(owner_id.to_string().as_str()));
        writer_shutdown.cancel();
        writer_task.await.unwrap();

        let rows_before_failure: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM requests")
            .fetch_one(&fusillade_pool)
            .await
            .unwrap();
        let failed = match try_warm_path_background(&state, &request_value, Some(FLEX_TEST_KEY), FLEX_TEST_MODEL).await {
            Err(response) => response,
            Ok(_) => panic!("closed writer must be a fatal warm-path admission failure"),
        };
        assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let rows_after_failure: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM requests")
            .fetch_one(&fusillade_pool)
            .await
            .unwrap();
        assert_eq!(rows_after_failure, rows_before_failure);
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
        let (requests_writer_task, requests_writer) =
            crate::inference::engine::writer::RequestsWriter::new(request_manager.clone(), 1, Duration::ZERO);
        let response_store = Arc::new(response_store::FusilladeResponseStore::new(
            request_manager.clone(),
            requests_writer.clone(),
        ));
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
                hidden_batch_key_is_child: true,
            },
        )]));
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
        let dispatches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_counter = dispatches.clone();
        let app = Router::new()
            .route(
                "/v1/responses",
                post(move || {
                    let dispatch_counter = dispatch_counter.clone();
                    async move {
                        dispatch_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        StatusCode::IM_A_TEAPOT
                    }
                }),
            )
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
        let mut request_task = tokio::spawn(app.clone().oneshot(request));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut request_task).await.is_err(),
            "background realtime must wait for durable admission"
        );
        assert_eq!(dispatches.load(std::sync::atomic::Ordering::SeqCst), 0);
        let rows_before_writer: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM requests")
            .fetch_one(&fusillade_pool)
            .await
            .unwrap();
        assert_eq!(rows_before_writer, 0);

        let writer_shutdown = tokio_util::sync::CancellationToken::new();
        let writer_task = tokio::spawn(requests_writer_task.run(writer_shutdown.clone()));
        let response = tokio::time::timeout(Duration::from_secs(5), request_task)
            .await
            .expect("background realtime attribution must not wait on the unavailable main pool")
            .unwrap()
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let stored_created_by: Option<String> = sqlx::query_scalar("SELECT created_by FROM requests")
            .fetch_one(&fusillade_pool)
            .await
            .unwrap();
        assert_eq!(stored_created_by.as_deref(), Some(owner_id.to_string().as_str()));
        tokio::time::timeout(Duration::from_secs(5), async {
            while dispatches.load(std::sync::atomic::Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("successful background admission should dispatch upstream");
        writer_shutdown.cancel();
        writer_task.await.unwrap();

        let closed_writer_background = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("authorization", format!("Bearer {FLEX_TEST_KEY}"))
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "model": FLEX_TEST_MODEL,
                    "background": true,
                    "tools": [],
                    "input": "must fail closed",
                })
                .to_string(),
            ))
            .unwrap();
        let failed = app.clone().oneshot(closed_writer_background).await.unwrap();
        assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
        tokio::task::yield_now().await;
        assert_eq!(
            dispatches.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "writer failure must prevent background upstream dispatch"
        );

        let rows_before_ordinary: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM requests")
            .fetch_one(&fusillade_pool)
            .await
            .unwrap();
        let ordinary_realtime = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("authorization", format!("Bearer {FLEX_TEST_KEY}"))
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "model": FLEX_TEST_MODEL,
                    "background": false,
                    "tools": [],
                    "input": "ordinary realtime bypass",
                })
                .to_string(),
            ))
            .unwrap();
        let ordinary = app.oneshot(ordinary_realtime).await.unwrap();
        assert_eq!(ordinary.status(), StatusCode::IM_A_TEAPOT);
        assert_eq!(dispatches.load(std::sync::atomic::Ordering::SeqCst), 2);
        let rows_after_ordinary: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM requests")
            .fetch_one(&fusillade_pool)
            .await
            .unwrap();
        assert_eq!(
            rows_after_ordinary, rows_before_ordinary,
            "ordinary realtime must not create a lifecycle row before proxying"
        );
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
