//! Fusillade-backed implementation of onwards' `ResponseStore` trait
//! and standalone functions for creating/completing response records.
//!
//! All fusillade operations go through the `Storage` trait via `request_manager`.
//! The only raw SQL is the `api_keys` lookup which queries a dwctl-owned table.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use fusillade::{
    BatchInput, CreateSingleRequestBatchInput, CreateStepInput, PostgresRequestManager, PostgresResponseStepManager, RequestId,
    RequestTemplateInput, ReqwestHttpClient, ResponseStep, ResponseStepStore, StepId, StepKind as FusilladeStepKind,
    StepState as FusilladeStepState, Storage,
};
use onwards::{
    ChainStep, MultiStepStore, RecordedStep, ResponseStore, StepDescriptor, StepKind as OnwardsStepKind, StepState as OnwardsStepState,
    StoreError,
};
use sqlx_pool_router::PoolProvider;
use uuid::Uuid;

/// Per-response context the warm path stashes before kicking off the
/// multi-step loop so the bridge's `next_action_for` / `record_step`
/// can build chat-completions payloads + create per-step sub-request
/// fusillade rows without round-tripping a "parent /v1/responses"
/// fusillade row that no longer exists.
///
/// Keyed by `head_step_uuid.to_string()` (the response_id minus its
/// `resp_` prefix), populated by the warm path before the loop starts,
/// removed when the loop returns.
#[derive(Debug, Clone)]
pub struct PendingResponseInput {
    /// Raw user-submitted `/v1/responses` body. Re-parsed by
    /// `next_action_for` on each iteration to extract model, initial
    /// messages, tools, and the `stream` flag.
    pub body: String,
    /// User's API key. Stamped onto each per-step sub-request row's
    /// `api_key` column for downstream attribution + auth.
    pub api_key: Option<String>,
    /// Resolved `created_by` (user id) for the response's batches /
    /// requests rows.
    pub created_by: Option<String>,
    /// Loopback base URL: the per-step model_call sub-request rows'
    /// `base_url` column points at the dwctl loopback so onwards can
    /// pick a target / honor strict mode at fire time.
    pub base_url: String,
    /// Names of server-side tools registered for this request (resolved
    /// from `tool_sources` joined with the user's groups + the deployment).
    /// The transition function uses this to decide which tool_calls
    /// returned by the model can be auto-dispatched server-side and which
    /// must be passed through to the client as `function_call` output items.
    ///
    /// When a tool_call's name is missing from this set, it's treated as a
    /// client-side tool: the loop completes with the model's response, and
    /// `assemble_response` surfaces the call as a `function_call` item per
    /// the OpenAI Responses contract — the client is expected to execute
    /// it and submit the result via a follow-up request.
    pub resolved_tool_names: HashSet<String>,
}

/// Header set by the responses middleware so the outlet handler knows which
/// fusillade row to update with the response body.
pub const ONWARDS_RESPONSE_ID_HEADER: &str = "x-onwards-response-id";

/// A fusillade daemon ID assigned to this onwards instance.
#[derive(Debug, Clone, Copy)]
pub struct OnwardsDaemonId(pub Uuid);

/// ResponseStore implementation backed by fusillade's `Storage` trait.
pub struct FusilladeResponseStore<P: PoolProvider + Clone> {
    request_manager: Arc<PostgresRequestManager<P, ReqwestHttpClient>>,
    /// Step storage for multi-step responses. Optional so existing callers
    /// that only need the legacy single-step `ResponseStore` surface (e.g.
    /// the `previous_response_id` flow) can construct a store without
    /// wiring the multi-step manager.
    step_manager: Option<Arc<PostgresResponseStepManager<P>>>,
    /// In-memory side-channel: response_id (head_step_uuid.to_string())
    /// → original `/v1/responses` body + per-response context. The warm
    /// path inserts before kicking off the loop and removes when the
    /// loop returns. `next_action_for` reads to re-parse the user input
    /// on every iteration; `record_step` reads to stamp api_key +
    /// created_by + base_url on per-step sub-request rows.
    pending_inputs: Arc<RwLock<HashMap<String, PendingResponseInput>>>,
}

impl<P: PoolProvider + Clone> FusilladeResponseStore<P> {
    pub fn new(request_manager: Arc<PostgresRequestManager<P, ReqwestHttpClient>>) -> Self {
        Self {
            request_manager,
            step_manager: None,
            pending_inputs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Wire in the multi-step step manager so the new
    /// [`ResponseStore::record_step`] / `mark_step_processing` /
    /// `complete_step` / `fail_step` / `next_sequence` methods are
    /// backed by real persistence rather than the trait's
    /// "not implemented" default.
    pub fn with_step_manager(mut self, step_manager: Arc<PostgresResponseStepManager<P>>) -> Self {
        self.step_manager = Some(step_manager);
        self
    }

    /// Stash per-response context for the multi-step loop. Called by
    /// the warm path before `run_response_loop` starts. Returns the
    /// generated head step UUID — the warm path uses its string form
    /// as both the user-visible `resp_<id>` value and the loop's
    /// `request_id` parameter.
    ///
    /// On lock-poison, logs at error level and still returns a UUID;
    /// the warm path can't easily fail-fast on this without
    /// restructuring its `Option`-returning helpers, and the
    /// downstream `next_action_for` will surface a "no pending input
    /// registered" error with the same UUID, making the two log
    /// lines correlatable. Daemon callers that *can* propagate
    /// errors should use [`register_pending_with_id`] directly.
    pub fn register_pending(&self, input: PendingResponseInput) -> Uuid {
        let head_step_uuid = Uuid::new_v4();
        if let Err(e) = self.register_pending_with_id(head_step_uuid, input) {
            tracing::error!(
                error = %e,
                request_id = %head_step_uuid,
                "warm-path register_pending continuing after lock-poison; loop will fail downstream",
            );
        }
        head_step_uuid
    }

    /// Variant of [`register_pending`] for callers that already have a
    /// stable response id and need the side-channel keyed by it. The
    /// daemon path uses this: when a fusillade row is claimed for
    /// `/v1/responses`, its `request_id` *is* the head step UUID
    /// (`record_step` reuses `request_id` as the head step's id), so
    /// the loop's `next_action_for(request_id)` lookup must match.
    /// Without this, daemon-driven multi-step requests fail at the
    /// first iteration with "no pending input registered".
    ///
    /// Returns `Err` on lock poisoning so the daemon can fail the
    /// request immediately with the real cause rather than letting
    /// the loop surface a downstream "no pending input registered"
    /// error that doesn't mention the lock at all.
    pub fn register_pending_with_id(&self, head_step_uuid: Uuid, input: PendingResponseInput) -> Result<(), StoreError> {
        let key = head_step_uuid.to_string();
        let mut guard = self
            .pending_inputs
            .write()
            .map_err(|e| StoreError::StorageError(format!("pending_inputs lock poisoned: {e}")))?;
        guard.insert(key, input);
        Ok(())
    }

    /// Remove the side-channel entry for a completed (or failed)
    /// response. Idempotent — safe to call from both Ok and Err arms
    /// of the warm path's run-loop wrapper.
    pub fn unregister_pending(&self, request_id: &str) {
        let key = parse_response_id(request_id)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| request_id.to_string());
        if let Ok(mut guard) = self.pending_inputs.write() {
            guard.remove(&key);
        }
    }

    fn pending_input(&self, request_id: &str) -> Result<PendingResponseInput, StoreError> {
        let key = parse_response_id(request_id)?.to_string();
        self.pending_inputs
            .read()
            .map_err(|_| StoreError::StorageError("pending_inputs lock poisoned".into()))?
            .get(&key)
            .cloned()
            .ok_or_else(|| {
                StoreError::StorageError(format!(
                    "no pending input registered for response {request_id} — warm path didn't stash it (or it was unregistered)"
                ))
            })
    }

    /// Borrow the inner request_manager. Used by the warm-path SSE
    /// handler to call `complete_request` / `fail_request` directly
    /// after running the loop inline.
    pub fn request_manager(&self) -> &PostgresRequestManager<P, ReqwestHttpClient> {
        &self.request_manager
    }

    fn require_step_manager(&self) -> Result<&PostgresResponseStepManager<P>, StoreError> {
        self.step_manager.as_deref().ok_or_else(|| {
            StoreError::StorageError(
                "FusilladeResponseStore was constructed without a step manager — multi-step \
                 orchestration methods require with_step_manager(...) at construction time"
                    .into(),
            )
        })
    }

    /// Retrieve a response by ID. Used by the GET /v1/responses/{id} handler.
    ///
    /// Two retrieval paths:
    ///
    /// * **Multi-step** — the id is a head step's uuid. We look up the
    ///   head step, walk to its sub-request fusillade row, build the
    ///   response envelope from that row (created_at, status, model,
    ///   response_body) and stamp `resp_<head_step_uuid>` as the id.
    ///
    /// * **Single-step** — the id is itself a `fusillade.requests` id
    ///   (a `/v1/chat/completions` or `/v1/embeddings` row created by
    ///   the realtime path). When no head step matches, we fall back
    ///   to the legacy lookup so `/v1/chat/completions` results stay
    ///   retrievable via `GET /v1/responses/{id}` — the API surface
    ///   the dashboard depends on.
    ///
    /// Returns `None` only when neither lookup matches.
    pub async fn get_response(&self, response_id: &str) -> Result<Option<serde_json::Value>, StoreError> {
        let parsed_uuid = parse_response_id(response_id)?;

        // Multi-step path: try head_step first.
        if let Some(step_manager) = self.step_manager.as_deref()
            && let Some(head_step) = step_manager.get_step(StepId(parsed_uuid)).await.map_err(map_fusillade_err)?
        {
            // Head step exists → resolve via its sub-request fusillade
            // row. CHECK constraint guarantees model_call ⇒
            // request_id, so the unwrap below is well-defined for any
            // committed head step.
            let Some(sub_request_id) = head_step.request_id else {
                return Ok(None);
            };
            let detail = match self.request_manager.get_request_detail(sub_request_id).await {
                Ok(d) => d,
                Err(fusillade::FusilladeError::RequestNotFound(_)) => return Ok(None),
                Err(e) => return Err(StoreError::StorageError(format!("fetch head sub-request: {e}"))),
            };
            let mut resp = detail_to_response_object(&detail);
            // Surface the user-facing id (head step uuid), not the
            // internal sub-request uuid.
            resp["id"] = serde_json::Value::String(format!("resp_{parsed_uuid}"));
            return Ok(Some(resp));
        }

        // Single-step fallback: the id is itself a fusillade.requests
        // row (chat completions / embeddings). Used by callers that
        // GET a previously-issued non-multi-step request via the same
        // /v1/responses/{id} endpoint.
        match self.request_manager.get_request_detail(RequestId(parsed_uuid)).await {
            Ok(detail) => Ok(Some(detail_to_response_object(&detail))),
            Err(fusillade::FusilladeError::RequestNotFound(_)) => Ok(None),
            Err(e) => Err(StoreError::StorageError(format!("Failed to fetch request: {e}"))),
        }
    }
}

fn parse_step_id(raw: &str) -> Result<StepId, StoreError> {
    Uuid::parse_str(raw)
        .map(StepId::from)
        .map_err(|_| StoreError::NotFound(raw.to_string()))
}

fn map_step_kind(kind: OnwardsStepKind) -> FusilladeStepKind {
    match kind {
        OnwardsStepKind::ModelCall => FusilladeStepKind::ModelCall,
        OnwardsStepKind::ToolCall => FusilladeStepKind::ToolCall,
    }
}

fn map_kind_back(kind: FusilladeStepKind) -> OnwardsStepKind {
    match kind {
        FusilladeStepKind::ModelCall => OnwardsStepKind::ModelCall,
        FusilladeStepKind::ToolCall => OnwardsStepKind::ToolCall,
    }
}

fn map_state_back(state: FusilladeStepState) -> OnwardsStepState {
    match state {
        FusilladeStepState::Pending => OnwardsStepState::Pending,
        FusilladeStepState::Processing => OnwardsStepState::Processing,
        FusilladeStepState::Completed => OnwardsStepState::Completed,
        FusilladeStepState::Failed => OnwardsStepState::Failed,
        FusilladeStepState::Canceled => OnwardsStepState::Canceled,
    }
}

fn step_to_chain(step: ResponseStep) -> ChainStep {
    ChainStep {
        id: step.id.0.to_string(),
        kind: map_kind_back(step.step_kind),
        state: map_state_back(step.state),
        sequence: step.step_sequence,
        prev_step_id: step.prev_step_id.map(|s| s.0.to_string()),
        parent_step_id: step.parent_step_id.map(|s| s.0.to_string()),
        response_payload: step.response_payload,
        error: step.error,
    }
}

fn map_fusillade_err(e: fusillade::FusilladeError) -> StoreError {
    StoreError::StorageError(format!("fusillade: {e}"))
}

/// Mark a response as failed.
pub async fn fail_response<P: PoolProvider + Clone>(
    request_manager: &PostgresRequestManager<P, ReqwestHttpClient>,
    response_id: &str,
    error: &str,
) -> Result<(), StoreError> {
    let id = parse_response_id(response_id)?;

    // 500 is the catch-all status; specific code paths that have a real
    // upstream HTTP status use lower-level fusillade APIs directly.
    request_manager
        .fail_request(RequestId(id), error, 500)
        .await
        .map_err(|e| StoreError::StorageError(format!("Failed to fail request: {e}")))?;

    Ok(())
}

/// Returns true if a fusillade request with this id already exists.
///
/// Used by `create-response` to skip work when `complete-response` has already
/// raced ahead and inserted the row itself.
pub async fn request_exists<P: PoolProvider + Clone>(
    request_manager: &PostgresRequestManager<P, ReqwestHttpClient>,
    request_id: Uuid,
) -> Result<bool, StoreError> {
    match request_manager.get_request_detail(RequestId(request_id)).await {
        Ok(_) => Ok(true),
        Err(fusillade::FusilladeError::RequestNotFound(_)) => Ok(false),
        Err(e) => Err(StoreError::StorageError(format!("Failed to check request existence: {e}"))),
    }
}

/// Context required to create a fusillade single-request batch.
///
/// Carried by `complete-response` so it can create-then-complete when it
/// races ahead of `create-response`.
pub struct CreateContext<'a> {
    pub batch_id: Uuid,
    pub request_id: Uuid,
    pub request_body: &'a str,
    pub model: &'a str,
    pub endpoint: &'a str,
    pub base_url: &'a str,
    pub api_key: Option<&'a str>,
}

/// Mark a response as completed, creating the row first if it doesn't exist.
///
/// The two-job lifecycle (create-response, complete-response) can race —
/// they're enqueued within ~50ms of each other and run on independent
/// underway queues. Worse, underway's heartbeat-based reclamation can
/// produce zombie attempts: the original worker keeps running after
/// underway has marked the attempt failed and started a fresh one, so
/// two attempts may modify the same row concurrently.
///
/// This helper tolerates all of those orderings:
///  - missing row → synthesize, then complete
///  - row already in `completed`/`failed`/`canceled` → idempotent success
///    (some other writer — typically a zombie attempt or a duplicate
///    enqueue — has already done our work)
///  - row in `processing` → straight UPDATE
pub async fn complete_response_idempotent<P: PoolProvider + Clone>(
    request_manager: &PostgresRequestManager<P, ReqwestHttpClient>,
    dwctl_pool: &sqlx::PgPool,
    response_id: &str,
    response_body: &str,
    status_code: u16,
    create_ctx: CreateContext<'_>,
) -> Result<(), StoreError> {
    let id = parse_response_id(response_id)?;

    // Fast path: row exists in `processing` — UPDATE matches and we're done.
    match request_manager.complete_request(RequestId(id), response_body, status_code).await {
        Ok(()) => return Ok(()),
        Err(fusillade::FusilladeError::RequestStateConflict { current_state, .. }) if is_terminal(&current_state) => {
            tracing::info!(
                response_id = %response_id,
                final_state = %current_state,
                "complete-response: row already in terminal state — idempotent success"
            );
            return Ok(());
        }
        Err(fusillade::FusilladeError::RequestStateConflict { current_state, .. }) => {
            // Row exists in some non-terminal, non-processing state (e.g.
            // 'pending' or 'claimed'). This shouldn't happen for the realtime
            // path, but it's not our place to force-complete. Bubble up.
            return Err(StoreError::StorageError(format!(
                "Row exists for response {response_id} in unexpected state '{current_state}'"
            )));
        }
        Err(fusillade::FusilladeError::RequestNotFound(_)) => {} // synthesize below
        Err(e) => return Err(StoreError::StorageError(format!("Failed to complete request: {e}"))),
    }

    // Row doesn't exist — synthesize it. create-response may race us; if it
    // wins between our failed UPDATE and our INSERT, the INSERT hits a PK
    // conflict and the retry UPDATE below sorts it out.
    tracing::info!(
        response_id = %response_id,
        model = %create_ctx.model,
        endpoint = %create_ctx.endpoint,
        "complete-response synthesizing row (create-response hasn't run yet)"
    );
    if create_ctx.endpoint.is_empty() {
        // Empty endpoint means an upstream header is missing; better to fail
        // loudly than silently insert a row the /responses lookup can't find.
        return Err(StoreError::StorageError(
            "Cannot synthesize request row: empty endpoint in CreateContext (x-onwards-endpoint header missing upstream)".into(),
        ));
    }
    let created_by = lookup_created_by(dwctl_pool, create_ctx.api_key).await;
    let batch_input = fusillade::CreateSingleRequestBatchInput {
        batch_id: Some(create_ctx.batch_id),
        request_id: create_ctx.request_id,
        body: create_ctx.request_body.to_string(),
        model: create_ctx.model.to_string(),
        base_url: create_ctx.base_url.to_string(),
        endpoint: create_ctx.endpoint.to_string(),
        completion_window: "0s".to_string(),
        initial_state: "processing".to_string(),
        api_key: create_ctx.api_key.map(String::from),
        created_by,
    };
    match request_manager.create_single_request_batch(batch_input).await {
        Ok(_) => tracing::info!(
            response_id = %response_id,
            "Synthetic create from complete-response succeeded — row now exists in 'processing'"
        ),
        Err(e) => tracing::info!(
            response_id = %response_id,
            error = %e,
            "Synthetic create from complete-response failed (likely create-response won the race) — proceeding to UPDATE"
        ),
    }

    // Retry the UPDATE. Same idempotency rules as the fast path: another
    // writer may have raced ahead to a terminal state in the window between
    // our first UPDATE and this retry.
    match request_manager.complete_request(RequestId(id), response_body, status_code).await {
        Ok(()) => {
            tracing::info!(response_id = %response_id, "Second-attempt UPDATE succeeded — row now 'completed'");
            Ok(())
        }
        Err(fusillade::FusilladeError::RequestStateConflict { current_state, .. }) if is_terminal(&current_state) => {
            tracing::info!(
                response_id = %response_id,
                final_state = %current_state,
                "complete-response: row already terminal after synthesis — idempotent success"
            );
            Ok(())
        }
        Err(e) => {
            tracing::warn!(response_id = %response_id, error = %e, "Second-attempt UPDATE failed");
            Err(StoreError::StorageError(format!("Failed to complete after create: {e}")))
        }
    }
}

/// True when the row has reached a state where re-completing it would be a
/// no-op — there's nothing left for us to do.
fn is_terminal(state: &str) -> bool {
    matches!(state, "completed" | "failed" | "canceled")
}

/// Poll a fusillade request until it reaches a terminal state (completed/failed/canceled).
pub async fn poll_until_complete<P: PoolProvider + Clone>(
    request_manager: &PostgresRequestManager<P, ReqwestHttpClient>,
    response_id: &str,
    poll_interval: std::time::Duration,
    timeout: std::time::Duration,
) -> Result<serde_json::Value, StoreError> {
    let id = parse_response_id(response_id)?;
    let start = std::time::Instant::now();

    loop {
        match request_manager.get_request_detail(RequestId(id)).await {
            Ok(detail) => match detail.status.as_str() {
                "completed" | "failed" | "canceled" => {
                    return Ok(detail_to_response_object(&detail));
                }
                _ => {}
            },
            Err(fusillade::FusilladeError::RequestNotFound(_)) => {}
            Err(e) => {
                return Err(StoreError::StorageError(format!("Failed to poll request: {e}")));
            }
        }

        if start.elapsed() >= timeout {
            return Err(StoreError::StorageError(format!(
                "Timeout waiting for request {response_id} to complete after {:?}",
                timeout
            )));
        }

        tokio::time::sleep(poll_interval).await;
    }
}

/// Look up the user ID from an API key for batch/response attribution.
///
/// Returns `Some(user_id)` if the key is found, `None` otherwise.
pub async fn lookup_created_by(pool: &sqlx::PgPool, api_key: Option<&str>) -> Option<String> {
    let key = api_key?;
    match sqlx::query("SELECT user_id FROM public.api_keys WHERE secret = $1 AND is_deleted = false LIMIT 1")
        .bind(key)
        .fetch_optional(pool)
        .await
    {
        Ok(Some(row)) => {
            use sqlx::Row;
            let user_id: Uuid = row.get("user_id");
            Some(user_id.to_string())
        }
        Ok(None) => {
            tracing::warn!(key_prefix = &key[..8.min(key.len())], "API key not found for attribution");
            None
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to look up API key for attribution");
            None
        }
    }
}

/// Create a batch of 1 in fusillade for async/flex processing.
///
/// Uses fusillade's `create_file` + `create_batch` methods.
/// The fusillade daemon will pick up the pending request and process it.
///
/// Returns `(response_id, request_id)` where response_id is `resp_<uuid>`.
pub async fn create_batch_of_1<P: PoolProvider + Clone>(
    request_manager: &PostgresRequestManager<P, ReqwestHttpClient>,
    request: &serde_json::Value,
    model: &str,
    base_url: &str,
    path: &str,
    completion_window: &str,
    api_key: Option<&str>,
) -> Result<(String, Uuid), StoreError> {
    let pool = request_manager.pool();
    let body = request.to_string();

    let created_by = lookup_created_by(pool, api_key).await.unwrap_or_default();

    let template = RequestTemplateInput {
        custom_id: None,
        endpoint: base_url.to_string(),
        method: "POST".to_string(),
        path: path.to_string(),
        body,
        model: model.to_string(),
        api_key: String::new(),
    };

    let file_id = request_manager
        .create_file("responses_api_single".into(), None, vec![template])
        .await
        .map_err(|e| StoreError::StorageError(format!("Failed to create file: {e}")))?;

    let batch = request_manager
        .create_batch(BatchInput {
            file_id,
            endpoint: path.to_string(),
            completion_window: completion_window.to_string(),
            metadata: None,
            created_by: if created_by.is_empty() { None } else { Some(created_by) },
            api_key_id: None,
            api_key: api_key.map(|s| s.to_string()),
            total_requests: Some(1),
        })
        .await
        .map_err(|e| StoreError::StorageError(format!("Failed to create batch: {e}")))?;

    let requests = request_manager
        .get_batch_requests(batch.id)
        .await
        .map_err(|e| StoreError::StorageError(format!("Failed to get batch requests: {e}")))?;

    let request_id = requests
        .first()
        .map(|r| *r.id())
        .ok_or_else(|| StoreError::StorageError("Batch created with no requests".into()))?;

    let response_id = format!("resp_{request_id}");
    tracing::debug!(
        response_id = %response_id,
        batch_id = %batch.id,
        completion_window = %completion_window,
        "Created batch of 1 for async processing"
    );

    Ok((response_id, request_id))
}

/// Extract error type and message from an upstream response body and status code.
///
/// Tries to parse the body as an OpenAI error envelope (`{"error": {"message": ...}}`).
/// Falls back to the raw body text with a status-appropriate error type.
fn extract_upstream_error(status: u16, body: &str) -> (&'static str, String) {
    if let Some(message) = parse_openai_error(body) {
        return (status_to_error_type(status), message);
    }
    (status_to_error_type(status), body.to_string())
}

/// Extract the error type and message from a fusillade error string.
///
/// The error column may contain:
/// 1. A serialized `FailureReason` JSON envelope
///    (e.g. `{"type":"NonRetriableHttpStatus","details":{"status":403,"body":"{...}"}}`)
/// 2. A legacy "Upstream returned {status}: {body}" string
/// 3. A raw OpenAI error envelope
/// 4. Plain text
///
/// When the body is an OpenAI-compatible error envelope, unwrap it so callers
/// see the upstream error directly. Falls back to "server_error" with the raw
/// string for any other format.
///
/// Returns `(error_type, message, status_code)` where status_code is extracted
/// from the FailureReason envelope or legacy prefix when available.
fn parse_failure_error(err: &str) -> (&'static str, String, Option<u16>) {
    // Try to parse as FailureReason envelope
    if let Ok(reason) = serde_json::from_str::<serde_json::Value>(err)
        && let Some(details) = reason.get("details")
    {
        let status = details.get("status").and_then(|s| s.as_u64()).and_then(|s| u16::try_from(s).ok());
        let error_type = status_to_error_type(status.unwrap_or(500));
        if let Some(body) = details.get("body").and_then(|b| b.as_str()) {
            if let Some(message) = parse_openai_error(body) {
                return (error_type, message, status);
            }
            return (error_type, body.to_string(), status);
        }
    }

    // Try legacy "Upstream returned {status}: {body}" format
    if let Some(rest) = err.strip_prefix("Upstream returned ")
        && let Some(colon_pos) = rest.find(": ")
        && let Ok(status) = rest[..colon_pos].parse::<u16>()
    {
        let body = &rest[colon_pos + 2..];
        if let Some(message) = parse_openai_error(body) {
            return (status_to_error_type(status), message, Some(status));
        }
        return (status_to_error_type(status), body.to_string(), Some(status));
    }

    // Not a FailureReason envelope — try as raw OpenAI error
    if let Some(message) = parse_openai_error(err) {
        return ("server_error", message, None);
    }

    ("server_error", err.to_string(), None)
}

/// Try to extract the message from an OpenAI-compatible error body:
/// `{"error": {"message": "...", "type": "...", ...}}`
fn parse_openai_error(body: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    let error = parsed.get("error")?;
    let message = error.get("message")?.as_str()?;
    Some(message.to_string())
}

/// Map an HTTP status code to an Open Responses API error type.
fn status_to_error_type(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        402 => "insufficient_credits",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        _ => "server_error",
    }
}

/// Parse a response ID like "resp_<uuid>" into a UUID.
fn parse_response_id(response_id: &str) -> Result<Uuid, StoreError> {
    let uuid_str = response_id.strip_prefix("resp_").unwrap_or(response_id);
    Uuid::parse_str(uuid_str).map_err(|e| StoreError::NotFound(format!("Invalid response ID: {e}")))
}

/// Map a fusillade request state to an Open Responses API status.
fn state_to_status(state: &str) -> &'static str {
    match state {
        "pending" => "queued",
        "claimed" | "processing" => "in_progress",
        "completed" => "completed",
        "failed" => "failed",
        "canceled" => "cancelled",
        _ => "failed",
    }
}

/// Convert a `RequestDetail` into an Open Responses API Response object.
fn detail_to_response_object(detail: &fusillade::RequestDetail) -> serde_json::Value {
    let status = state_to_status(&detail.status);

    // Derive background from the stored request body if available.
    let background = detail
        .body
        .as_deref()
        .and_then(|b| serde_json::from_str::<serde_json::Value>(b).ok())
        .and_then(|v| v.get("background")?.as_bool())
        .unwrap_or(false);

    let mut resp = serde_json::json!({
        "id": format!("resp_{}", detail.id),
        "object": "response",
        "created_at": detail.created_at.timestamp(),
        "status": status,
        "model": detail.model,
        "background": background,
        "output": [],
    });

    if status == "completed" {
        let response_status = match detail.response_status {
            Some(s) => u16::try_from(s).unwrap_or(500),
            None => 200,
        };
        let is_error_response = response_status >= 400;

        if is_error_response {
            // Non-2xx responses stored via complete_request preserve the real
            // upstream status and body. Surface the error to callers instead
            // of treating it as a successful completion.
            resp["status"] = serde_json::json!("failed");
            let (error_type, message) = if let Some(ref body) = detail.response_body {
                extract_upstream_error(response_status, body)
            } else {
                (
                    status_to_error_type(response_status),
                    format!("Upstream returned {response_status}"),
                )
            };
            resp["error"] = serde_json::json!({
                "type": error_type,
                "code": response_status,
                "message": message,
            });
        } else if let Some(ref body) = detail.response_body
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body)
        {
            if let Some(output) = parsed.get("output") {
                resp["output"] = output.clone();
            }
            if let Some(usage) = parsed.get("usage") {
                resp["usage"] = usage.clone();
            }
            // ChatCompletion format (batch results)
            if parsed.get("choices").is_some() {
                resp["output"] = serde_json::json!([{
                    "type": "message",
                    "role": "assistant",
                    "content": parsed
                }]);
            }
        }
        resp["completed_at"] = serde_json::json!(detail.completed_at.map(|t| t.timestamp()));
    }

    if status == "failed"
        && let Some(ref err) = detail.error
    {
        // Legacy path: errors stored via fail_request have the error in the
        // `error` column. Try to parse structured FailureReason to extract the
        // real error body instead of showing raw serialized JSON.
        let (error_type, message, status_code) = parse_failure_error(err);
        let mut error_obj = serde_json::json!({
            "type": error_type,
            "message": message,
        });
        if let Some(code) = status_code {
            error_obj["code"] = serde_json::json!(code);
        }
        resp["error"] = error_obj;
    }

    resp
}

#[async_trait]
impl<P: PoolProvider + Clone + Send + Sync + 'static> ResponseStore for FusilladeResponseStore<P> {
    async fn store(&self, response: &serde_json::Value) -> Result<String, StoreError> {
        let id = response.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        Ok(id)
    }

    async fn get_context(&self, response_id: &str) -> Result<Option<serde_json::Value>, StoreError> {
        self.get_response(response_id).await
    }
}

// MultiStepStore implementation: storage primitives + chain walk backed
// by fusillade's PostgresResponseStepManager, plus the Open Responses
// transition function (next_action_for) and assembly (assemble_response)
// living in the sibling `transition` and `assembly` modules.
//
// Identity model after the response_steps re-anchoring (fusillade 16.8):
//   * `request_id` from onwards == "resp_<head_step_uuid>" (or just
//     <head_step_uuid>) — the head step's id is the response identity.
//   * No parent /v1/responses fusillade row exists. Every model_call
//     step (including the head) gets its own per-step sub-request
//     fusillade row created synchronously inside `record_step`.
//   * `parent_step_id` is the chain identifier — NULL on the head,
//     head's id on every descendant. `scope_parent` from onwards is
//     not threaded through to fusillade's parent_step_id (it would
//     break listing's anti-join); sub-agent recursion, when wired,
//     will be modeled via `prev_step_id` branching instead.
#[async_trait]
impl<P: PoolProvider + Clone + Send + Sync + 'static> MultiStepStore for FusilladeResponseStore<P> {
    async fn next_action_for(&self, request_id: &str, scope_parent: Option<&str>) -> Result<onwards::NextAction, StoreError> {
        // The original /v1/responses body lives in the side-channel
        // populated by the warm path. There is no longer a parent
        // fusillade row to fetch it from.
        let pending = self.pending_input(request_id)?;
        let parsed = super::transition::parse_parent_request(&pending.body).map_err(StoreError::StorageError)?;
        let chain = <Self as MultiStepStore>::list_chain(self, request_id, scope_parent).await?;
        Ok(super::transition::decide_next_action(&parsed, &chain, &pending.resolved_tool_names))
    }

    async fn record_step(
        &self,
        request_id: &str,
        scope_parent: Option<&str>,
        prev_step: Option<&str>,
        descriptor: &StepDescriptor,
    ) -> Result<RecordedStep, StoreError> {
        let step_manager = self.require_step_manager()?;
        let head_step_uuid = parse_response_id(request_id)?;
        let prev_step_id = prev_step.map(parse_step_id).transpose()?;

        // Chain-walk to figure out: is this the very first step (the
        // head)? and what sequence number to assign?
        //
        // Idempotency under crash recovery is the caller's
        // responsibility — fusillade dropped the chain-uniqueness
        // constraint because parallel tool_calls share
        // (parent, prev, kind) by design.
        let chain = step_manager.list_chain(StepId(head_step_uuid)).await.map_err(map_fusillade_err)?;

        // The very first record_step on a top-level chain (no
        // scope_parent, empty chain) becomes the head. We pre-allocate
        // its id from the request_id so the warm path's "resp_<id>"
        // matches the head step row that's about to be inserted.
        let is_head = chain.is_empty() && scope_parent.is_none();
        let new_step_id = if is_head { Some(head_step_uuid) } else { None };
        // Every non-head step shares parent_step_id = head id. This is
        // what makes the listing-query anti-join work and what the
        // chain_walk index is keyed on.
        let parent_step_id = if is_head { None } else { Some(StepId(head_step_uuid)) };
        let sequence = chain.iter().map(|s| s.step_sequence).max().unwrap_or(0) + 1;

        // model_call steps require a sub-request fusillade row (CHECK
        // constraint: model_call ⇒ request_id IS NOT NULL). Create it
        // synchronously so the FK is satisfied at the response_steps
        // insert. tool_call steps have request_id = NULL — analytics
        // for them live in tool_call_analytics (keyed on
        // response_step_id).
        let req_id = match descriptor.kind {
            OnwardsStepKind::ModelCall => Some(RequestId(self.create_sub_request_row(request_id, descriptor).await?)),
            OnwardsStepKind::ToolCall => None,
        };

        let id = step_manager
            .create_step(CreateStepInput {
                id: new_step_id,
                request_id: req_id,
                prev_step_id,
                parent_step_id,
                step_kind: map_step_kind(descriptor.kind),
                step_sequence: sequence,
                request_payload: descriptor.request_payload.clone(),
            })
            .await
            .map_err(map_fusillade_err)?;

        Ok(RecordedStep {
            id: id.0.to_string(),
            sequence,
        })
    }

    async fn mark_step_processing(&self, step_id: &str) -> Result<(), StoreError> {
        let step_manager = self.require_step_manager()?;
        step_manager
            .mark_step_processing(parse_step_id(step_id)?)
            .await
            .map_err(map_fusillade_err)
    }

    async fn complete_step(&self, step_id: &str, payload: &serde_json::Value) -> Result<(), StoreError> {
        let step_manager = self.require_step_manager()?;
        step_manager
            .complete_step(parse_step_id(step_id)?, payload.clone())
            .await
            .map_err(map_fusillade_err)
    }

    async fn fail_step(&self, step_id: &str, error: &serde_json::Value) -> Result<(), StoreError> {
        let step_manager = self.require_step_manager()?;
        step_manager
            .fail_step(parse_step_id(step_id)?, error.clone())
            .await
            .map_err(map_fusillade_err)
    }

    async fn list_chain(&self, request_id: &str, _scope_parent: Option<&str>) -> Result<Vec<ChainStep>, StoreError> {
        // `scope_parent` is intentionally ignored: in the new schema,
        // every step in a response (including any future sub-agent
        // descendants) shares parent_step_id = head id, so a single
        // chain walk covers what list_scope used to serve. When
        // sub-agent dispatch is wired in the loop, scope filtering
        // will move to a client-side prev_step_id traversal of the
        // returned chain rather than a different storage call.
        let step_manager = self.require_step_manager()?;
        let head_step_uuid = parse_response_id(request_id)?;

        let steps = step_manager.list_chain(StepId(head_step_uuid)).await.map_err(map_fusillade_err)?;

        Ok(steps.into_iter().map(step_to_chain).collect())
    }

    async fn assemble_response(&self, request_id: &str) -> Result<serde_json::Value, StoreError> {
        let chain = <Self as MultiStepStore>::list_chain(self, request_id, None).await?;
        Ok(super::assembly::assemble_from_chain(request_id, &chain))
    }
}

impl<P: PoolProvider + Clone + Send + Sync + 'static> FusilladeResponseStore<P> {
    /// Mark the head step's sub-request fusillade row as completed
    /// (status 200) or failed (any other status) with the assembled
    /// response (or error payload) as the row's `response_body`.
    ///
    /// This is what makes the dashboard's responses listing show the
    /// response as completed and surfaces a useful response_body for
    /// retrieval. The head step's sub-request row is the one row in
    /// `requests` that survives the listing-query anti-join (its
    /// `response_step.parent_step_id` is NULL); every other model_call
    /// row in the chain is filtered out. Status_code 200 → `complete_request`,
    /// otherwise `fail_request`.
    ///
    /// No-op if the head step or its sub-request row can't be found
    /// (e.g., the loop crashed before the first record_step) — the
    /// caller has already handled that path via the assembled fail
    /// payload returned from the loop.
    pub async fn finalize_head_request(&self, request_id: &str, status_code: u16, body: serde_json::Value) -> Result<(), StoreError> {
        let step_manager = self.require_step_manager()?;
        let head_step_uuid = parse_response_id(request_id)?;
        let head_step = step_manager.get_step(StepId(head_step_uuid)).await.map_err(map_fusillade_err)?;
        let Some(head_step) = head_step else {
            return Ok(());
        };
        let Some(sub_request_id) = head_step.request_id else {
            return Ok(());
        };
        let body_str = serde_json::to_string(&body).map_err(|e| StoreError::StorageError(format!("serialize finalized body: {e}")))?;
        if status_code == 200 {
            self.request_manager
                .complete_request(sub_request_id, &body_str, status_code)
                .await
                .map_err(|e| StoreError::StorageError(format!("complete head sub-request row: {e}")))
        } else {
            self.request_manager
                .fail_request(sub_request_id, &body_str, status_code)
                .await
                .map_err(|e| StoreError::StorageError(format!("fail head sub-request row: {e}")))
        }
    }

    /// Create a fusillade `requests` row for a single model_call step.
    /// Returns the new row's id, which the caller writes into the
    /// `response_steps.request_id` column to wire the 1:1 link.
    ///
    /// Uses `create_single_request_batch` to satisfy the file/template/
    /// batch FK chain in one round-trip. The batch is single-use (one
    /// request per batch); the listing-query anti-join hides every
    /// non-head sub-request row.
    async fn create_sub_request_row(&self, request_id: &str, descriptor: &StepDescriptor) -> Result<Uuid, StoreError> {
        let pending = self.pending_input(request_id)?;
        let model = descriptor
            .request_payload
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let body = serde_json::to_string(&descriptor.request_payload)
            .map_err(|e| StoreError::StorageError(format!("serialize step request_payload: {e}")))?;

        let sub_request_id = Uuid::new_v4();
        let input = CreateSingleRequestBatchInput {
            batch_id: None,
            request_id: sub_request_id,
            body,
            model,
            base_url: pending.base_url,
            // The actual upstream HTTP fire happens via onwards' loopback
            // to /v1/chat/completions. Storing that as the row's endpoint
            // makes analytics + the responses-listing dashboard show the
            // right URL for each step.
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "0s".to_string(),
            // Initial state is `processing` because this row is "owned"
            // by the warm-path loop running in this onwards instance —
            // the daemon shouldn't claim it. The loop will UPDATE it via
            // complete_request / fail_request when the step terminates.
            initial_state: "processing".to_string(),
            api_key: pending.api_key,
            created_by: pending.created_by,
        };
        self.request_manager
            .create_single_request_batch(input)
            .await
            .map_err(|e| StoreError::StorageError(format!("create sub-request row: {e}")))?;
        Ok(sub_request_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_response_id_with_prefix() {
        let uuid = Uuid::new_v4();
        let id = format!("resp_{uuid}");
        let parsed = parse_response_id(&id).unwrap();
        assert_eq!(parsed, uuid);
    }

    #[test]
    fn test_parse_response_id_without_prefix() {
        let uuid = Uuid::new_v4();
        let parsed = parse_response_id(&uuid.to_string()).unwrap();
        assert_eq!(parsed, uuid);
    }

    #[test]
    fn test_parse_response_id_invalid() {
        let result = parse_response_id("not-a-uuid");
        assert!(result.is_err());
        assert!(matches!(result, Err(StoreError::NotFound(_))));
    }

    #[test]
    fn test_state_to_status_mapping() {
        assert_eq!(state_to_status("pending"), "queued");
        assert_eq!(state_to_status("claimed"), "in_progress");
        assert_eq!(state_to_status("processing"), "in_progress");
        assert_eq!(state_to_status("completed"), "completed");
        assert_eq!(state_to_status("failed"), "failed");
        assert_eq!(state_to_status("canceled"), "cancelled");
        assert_eq!(state_to_status("unknown"), "failed");
    }

    #[test]
    fn test_store_extracts_id_from_response() {
        let response = serde_json::json!({
            "id": "resp_12345678-1234-1234-1234-123456789abc",
            "status": "completed",
        });
        let id = response.get("id").and_then(|v| v.as_str()).unwrap_or("");
        assert_eq!(id, "resp_12345678-1234-1234-1234-123456789abc");
    }

    #[test]
    fn test_store_handles_missing_id() {
        let response = serde_json::json!({"status": "completed"});
        let id = response.get("id").and_then(|v| v.as_str()).unwrap_or("");
        assert_eq!(id, "");
    }

    #[test]
    fn test_extract_upstream_error_openai_format() {
        let body = r#"{"error":{"message":"Forbidden","type":"invalid_request_error","param":null,"code":"forbidden"}}"#;
        let (error_type, message) = extract_upstream_error(403, body);
        assert_eq!(error_type, "permission_error");
        assert_eq!(message, "Forbidden");
    }

    #[test]
    fn test_extract_upstream_error_rate_limit() {
        let body = r#"{"error":{"message":"Rate limit exceeded","type":"rate_limit_error","code":"rate_limit"}}"#;
        let (error_type, message) = extract_upstream_error(429, body);
        assert_eq!(error_type, "rate_limit_error");
        assert_eq!(message, "Rate limit exceeded");
    }

    #[test]
    fn test_extract_upstream_error_plain_text() {
        let (error_type, message) = extract_upstream_error(402, "Account balance too low");
        assert_eq!(error_type, "insufficient_credits");
        assert_eq!(message, "Account balance too low");
    }

    #[test]
    fn test_extract_upstream_error_server_error() {
        let body = r#"{"error":{"message":"Internal error"}}"#;
        let (error_type, message) = extract_upstream_error(500, body);
        assert_eq!(error_type, "server_error");
        assert_eq!(message, "Internal error");
    }

    #[test]
    fn test_parse_failure_error_legacy_format() {
        // Legacy FailureReason envelope with OpenAI body
        let err = r#"{"type":"NonRetriableHttpStatus","details":{"status":403,"body":"{\"error\":{\"message\":\"Forbidden\",\"type\":\"invalid_request_error\",\"param\":null,\"code\":\"forbidden\"}}"}}"#;
        let (error_type, message, status_code) = parse_failure_error(err);
        assert_eq!(error_type, "permission_error");
        assert_eq!(message, "Forbidden");
        assert_eq!(status_code, Some(403));
    }

    #[test]
    fn test_parse_failure_error_plain_string() {
        let (error_type, message, status_code) = parse_failure_error("some unknown error");
        assert_eq!(error_type, "server_error");
        assert_eq!(message, "some unknown error");
        assert_eq!(status_code, None);
    }

    #[test]
    fn test_parse_failure_error_legacy_upstream_returned_format() {
        // Legacy format: "Upstream returned {status}: {body}"
        let err =
            r#"Upstream returned 403: {"error":{"message":"Forbidden","type":"invalid_request_error","param":null,"code":"forbidden"}}"#;
        let (error_type, message, status_code) = parse_failure_error(err);
        assert_eq!(error_type, "permission_error");
        assert_eq!(message, "Forbidden");
        assert_eq!(status_code, Some(403));
    }

    #[test]
    fn test_status_to_error_type_mapping() {
        assert_eq!(status_to_error_type(400), "invalid_request_error");
        assert_eq!(status_to_error_type(401), "authentication_error");
        assert_eq!(status_to_error_type(402), "insufficient_credits");
        assert_eq!(status_to_error_type(403), "permission_error");
        assert_eq!(status_to_error_type(404), "not_found_error");
        assert_eq!(status_to_error_type(429), "rate_limit_error");
        assert_eq!(status_to_error_type(500), "server_error");
        assert_eq!(status_to_error_type(503), "server_error");
    }
}
