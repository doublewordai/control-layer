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
use crate::db::{errors::DbError, handlers::api_keys::ApiKeys, models::api_keys::ApiKeyPurpose};
use crate::image_normalizer::ImageNormalizer;

/// State for the inference middleware.
#[derive(Clone)]
pub struct InferenceMiddlewareState<P: PoolProvider + Clone = sqlx_pool_router::DbPools> {
    pub request_manager: Arc<PostgresRequestManager<P>>,
    pub daemon_id: OnwardsDaemonId,
    /// Base URL for loopback requests (e.g., "http://127.0.0.1:3001/ai").
    /// Flex batches are routed back through dwctl so onwards handles the
    /// responses→chat completions conversion.
    pub loopback_base_url: String,
    /// dwctl database pool for model access validation.
    pub dwctl_pool: sqlx::PgPool,
    /// Fusillade-backed response store. Used by the control plane here for
    /// `previous_response_id` hydration, and by `GET /v1/responses/{id}`.
    pub response_store: Arc<super::store::FusilladeResponseStore<P>>,
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
    /// Per-key ZDR policy map (api key secret to the owning account's
    /// `zero_data_retention` flag), kept fresh by [`crate::sync::zdr_keys`].
    /// Read by [`super::zdr::is_zdr_request`] on the submit path. Defaults to
    /// empty (every key reads as non-ZDR) when the sync is not wired.
    pub zdr_key_cache: crate::sync::zdr_keys::ZdrKeyCache,
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

    // `previous_response_id` hydration (Responses control plane). Inline the prior
    // turn's output items ahead of the current input, in the Responses domain,
    // BEFORE the edge translation (one layer inside this one) converts the request
    // to Chat Completions. Runs here rather than in the translator so the pure
    // translator stays stateless. Gated on the field's presence so the common path
    // pays nothing. The hydrated body flows to every downstream path because it is
    // re-serialised from `request_value` below.
    if is_responses_api && request_value.get("previous_response_id").is_some() {
        use crate::inference::translation::responses::hydrate::{HydrationError, hydrate_previous_response};
        if let Err(e) = hydrate_previous_response(&*state.response_store, &mut request_value).await {
            let (status, message) = match e {
                HydrationError::NotFound(id) => (StatusCode::BAD_REQUEST, format!("previous response not found: {id}")),
                HydrationError::Internal(msg) => {
                    tracing::error!(error = %msg, "responses hydration failed");
                    (StatusCode::INTERNAL_SERVER_ERROR, "failed to load previous response".to_string())
                }
            };
            let err_type = if status == StatusCode::BAD_REQUEST {
                "invalid_request_error"
            } else {
                "server_error"
            };
            return Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"error": {"message": message, "type": err_type}}).to_string(),
                ))
                .unwrap();
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

    // Resolve created_by upfront for background realtime responses (row must
    // exist before returning 202). Flex uses the key owner's hidden batch key
    // below, so resolving it there also supplies the attribution target.
    let created_by = if background && matches!(service_tier, ServiceTier::Realtime) {
        response_store::lookup_created_by(&state.dwctl_pool, api_key.as_deref()).await
    } else {
        None
    };
    let flex_batch_key = if matches!(service_tier, ServiceTier::Flex) {
        match resolve_flex_batch_api_key(&state.dwctl_pool, api_key.as_deref()).await {
            Ok(Some(key)) => Some(key),
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
        }
    } else {
        None
    };

    // Bound how much an unverified creditor can queue via flex. Flex requests are
    // persisted and dispatched later (they don't pass through onwards' rate
    // limiter), so without this an unverified user could enqueue unbounded
    // volume. The creditor id and verified flag ride along on the hidden
    // batch-key resolution above (`key_owner_id` is `api_keys.user_id`), so this
    // costs no extra query. `flex_batch_key` is `Some` only for the flex tier,
    // and its resolution already failed closed (403/500) on lookup errors above,
    // so an unresolved creditor never reaches enforcement. No-op for verified
    // creditors or a disabled cap.
    if let Some(key) = flex_batch_key.as_ref()
        && let Err(err) = crate::api::handlers::unverified_volume::enforce_unverified_volume_limit(
            &*state.request_manager,
            state.unverified_requests_per_completion_hour,
            key.key_owner_id,
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
            let zdr = crate::inference::zdr::is_zdr_request(&state.zdr_key_cache, api_key.as_deref());

            // Tag realtime traffic with the priority hint (`nvext.agent_hints.
            // priority = 0`) so the backend scheduler ranks it above deadline-
            // prioritised batch work, which fusillade stamps with large
            // *negative* priorities (`-batch_expires_at`). Without this, an
            // unprioritised realtime request can be starved behind the batch
            // queue on backends that don't default a missing priority above
            // those negatives.
            attach_realtime_priority(&mut request_value);

            // Forward the *mutated* body downstream. The live upstream call in
            // `handle_realtime` is built from `body_bytes`, so rebuild those
            // bytes from `request_value` — otherwise the hint only reaches the
            // stored tracking row below and never the scheduler.
            let body_bytes = bytes::Bytes::from(request_value.to_string());

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
            let zdr = crate::inference::zdr::is_zdr_request(&state.zdr_key_cache, api_key.as_deref());
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
                .map(|key| key.key_owner_id.to_string())
                .or_else(|| created_by.clone())
                .unwrap_or_default();
            // INVARIANT: the daemon dispatches this job back through the loopback
            // (`endpoint` = `.../ai`), so it re-enters the FULL dwctl stack,
            // including the edge translation layer. That is load-bearing for
            // Responses: this middleware runs OUTER to translation, so `flex_body`
            // and `path` are still the RAW `/responses` request here (untranslated).
            // Translation converts it on the daemon's loopback in both directions
            // (request -> chat for the model call, chat -> Responses for the stored
            // result), which is what lets `GET /v1/responses/{id}` return a
            // Responses object. Do not "optimise" the loopback to hit onwards
            // directly - that would bypass translation and break Responses flex.
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

#[derive(Debug, Clone)]
struct FlexBatchApiKey {
    secret: String,
    key_owner_id: uuid::Uuid,
    /// The key owner's `users.verified` flag (organizations are `users` rows
    /// too). Rides along on this lookup so the unverified upload-volume cap on
    /// the flex path needs no extra query. `false` when no user row matches.
    verified: bool,
}

async fn resolve_flex_batch_api_key(pool: &sqlx::PgPool, api_key: Option<&str>) -> Result<Option<FlexBatchApiKey>, DbError> {
    let Some(api_key) = api_key else {
        return Ok(None);
    };

    let row = sqlx::query(
        r#"
        SELECT ak.user_id, ak.created_by, u.verified
        FROM api_keys ak
        LEFT JOIN users u ON u.id = ak.user_id
        WHERE ak.secret = $1 AND ak.is_deleted = false
        LIMIT 1
        "#,
    )
    .bind(api_key)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };
    let key_owner_id: uuid::Uuid = sqlx::Row::get(&row, "user_id");
    let created_by: uuid::Uuid = sqlx::Row::get(&row, "created_by");
    // NULL when the LEFT JOIN found no user row; a missing row counts as
    // unverified, matching `Users::is_verified`.
    let verified: bool = sqlx::Row::try_get(&row, "verified").ok().flatten().unwrap_or(false);

    let mut conn = pool.acquire().await?;
    let mut api_keys_repo = ApiKeys::new(&mut conn);
    let secret = api_keys_repo
        .get_or_create_hidden_key(key_owner_id, ApiKeyPurpose::Batch, created_by)
        .await?;

    Ok(Some(FlexBatchApiKey {
        secret,
        key_owner_id,
        verified,
    }))
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

/// Attach the realtime priority hint (`nvext.agent_hints.priority = 0`)
/// to an inference request body. All realtime traffic carries this hint
/// so downstream scheduling can distinguish it from async/flex work.
///
/// Sets only `nvext.agent_hints`, preserving any other `nvext` fields the
/// client sent, and coerces `nvext` to an object if it arrived as some
/// other type (rather than panicking on `serde_json`'s indexing). A no-op
/// if the body isn't a JSON object.
fn attach_realtime_priority(request_value: &mut serde_json::Value) {
    if let Some(obj) = request_value.as_object_mut() {
        let nvext = obj.entry("nvext").or_insert_with(|| serde_json::json!({}));
        if !nvext.is_object() {
            *nvext = serde_json::json!({});
        }
        if let Some(nvext) = nvext.as_object_mut() {
            nvext.insert("agent_hints".to_string(), serde_json::json!({ "priority": 0 }));
        }
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
    // The realtime priority hint rewrites the body upstream of here, so the
    // Content-Length inherited from the client request in `parts` is stale.
    // Set it to the actual forwarded length so onwards/the backend don't
    // truncate or reject the request.
    let content_length = body_bytes.len();
    let mut req = Request::from_parts(parts, Body::from(body_bytes));
    req.headers_mut().insert(
        axum::http::header::CONTENT_LENGTH,
        axum::http::HeaderValue::from(content_length as u64),
    );
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
///
/// `/messages` is included because this middleware now runs OUTER to the edge
/// translation (so it sees the raw foreign path). Anthropic `/messages` still
/// needs a tracking row and realtime/flex routing, exactly like `/chat/completions`
/// it would otherwise be translated into; translation happens on the layer just
/// inside this one.
pub(crate) fn should_intercept(method: &axum::http::Method, path: &str) -> bool {
    method == axum::http::Method::POST
        && (path.ends_with("/responses")
            || path.ends_with("/chat/completions")
            || path.ends_with("/messages")
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

    #[sqlx::test]
    async fn resolve_flex_batch_key_uses_hidden_batch_key_for_key_owner(pool: sqlx::PgPool) {
        use crate::api::models::{api_keys::ApiKeyCreate, users::Role};
        use crate::db::{
            handlers::{Repository, api_keys::ApiKeys},
            models::api_keys::{ApiKeyCreateDBRequest, ApiKeyPurpose},
        };

        let user = crate::test::utils::create_test_user(&pool, Role::BatchAPIUser).await;
        let org = crate::test::utils::create_test_org(&pool, user.id).await;
        let realtime_key = {
            let mut conn = pool.acquire().await.expect("acquire connection");
            let mut repo = ApiKeys::new(&mut conn);
            repo.create(&ApiKeyCreateDBRequest::new(
                org.id,
                user.id,
                ApiKeyCreate {
                    name: "Org realtime key".to_string(),
                    description: None,
                    purpose: ApiKeyPurpose::Realtime,
                    requests_per_second: None,
                    burst_size: None,
                    member_id: None,
                },
            ))
            .await
            .expect("create realtime key")
            .secret
        };

        let flex_key = resolve_flex_batch_api_key(&pool, Some(&realtime_key))
            .await
            .expect("resolve flex key")
            .expect("known realtime key should resolve");

        assert_ne!(flex_key.secret, realtime_key);
        assert_eq!(flex_key.key_owner_id, org.id);

        let row = sqlx::query(
            r#"
            SELECT user_id, created_by, purpose, hidden
            FROM api_keys
            WHERE secret = $1
            "#,
        )
        .bind(&flex_key.secret)
        .fetch_one(&pool)
        .await
        .expect("hidden key row");

        let row_user_id: uuid::Uuid = sqlx::Row::get(&row, "user_id");
        let row_created_by: uuid::Uuid = sqlx::Row::get(&row, "created_by");
        let row_purpose: String = sqlx::Row::get(&row, "purpose");
        let row_hidden: bool = sqlx::Row::get(&row, "hidden");

        assert_eq!(row_user_id, org.id);
        assert_eq!(row_created_by, user.id);
        assert_eq!(row_purpose, "batch");
        assert!(row_hidden);
    }

    /// The realtime priority hint every realtime request must carry.
    fn priority(v: &serde_json::Value) -> Option<&serde_json::Value> {
        v.get("nvext")?.get("agent_hints")?.get("priority")
    }

    #[test]
    fn attach_realtime_priority_sets_zero_on_bare_body() {
        let mut body = serde_json::json!({ "model": "gpt-4", "input": "hi" });
        attach_realtime_priority(&mut body);
        assert_eq!(priority(&body), Some(&serde_json::json!(0)));
    }

    #[test]
    fn attach_realtime_priority_covers_representative_realtime_bodies() {
        // Every shape of realtime traffic must come out with priority 0:
        // responses, chat completions, embeddings; with and without an
        // existing (unrelated) nvext, and with pre-existing tools.
        let bodies = [
            serde_json::json!({ "model": "m", "input": "hi" }),
            serde_json::json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }] }),
            serde_json::json!({ "model": "m", "input": ["a", "b"] }),
            serde_json::json!({ "model": "m", "tools": [{ "type": "function" }] }),
            serde_json::json!({ "model": "m", "nvext": { "guided_json": { "x": 1 } } }),
            serde_json::json!({}),
        ];
        for body in bodies {
            let mut body = body;
            attach_realtime_priority(&mut body);
            assert_eq!(priority(&body), Some(&serde_json::json!(0)), "missing priority for body: {body}");
        }
    }

    #[test]
    fn attach_realtime_priority_preserves_other_nvext_fields() {
        let mut body = serde_json::json!({
            "model": "m",
            "nvext": { "guided_json": { "type": "object" }, "agent_hints": { "other": true } },
        });
        attach_realtime_priority(&mut body);
        // Sibling nvext field is untouched...
        assert_eq!(body["nvext"]["guided_json"], serde_json::json!({ "type": "object" }));
        // ...and agent_hints is replaced with exactly the priority hint.
        assert_eq!(body["nvext"]["agent_hints"], serde_json::json!({ "priority": 0 }));
    }

    #[test]
    fn attach_realtime_priority_replaces_non_object_agent_hints() {
        let mut body = serde_json::json!({
            "model": "m",
            "nvext": { "guided_json": { "x": 1 }, "agent_hints": "surprise" },
        });
        attach_realtime_priority(&mut body);
        assert_eq!(body["nvext"]["guided_json"], serde_json::json!({ "x": 1 }));
        assert_eq!(body["nvext"]["agent_hints"], serde_json::json!({ "priority": 0 }));
    }

    #[test]
    fn attach_realtime_priority_coerces_non_object_nvext() {
        // A hostile/odd client sending a non-object nvext must not panic
        // and must still end up with priority 0.
        let mut body = serde_json::json!({ "model": "m", "nvext": "surprise" });
        attach_realtime_priority(&mut body);
        assert_eq!(priority(&body), Some(&serde_json::json!(0)));
    }

    #[test]
    fn attach_realtime_priority_noop_on_non_object_body() {
        // Non-object bodies aren't valid inference requests; the helper
        // leaves them untouched rather than panicking.
        let mut body = serde_json::json!("not a request");
        attach_realtime_priority(&mut body);
        assert_eq!(body, serde_json::json!("not a request"));
    }
}
