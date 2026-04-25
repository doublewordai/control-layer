//! Underway jobs for Open Responses API lifecycle management.
//!
//! Two jobs handle the response lifecycle. Both are idempotent and tolerate
//! the create/complete race in either direction:
//!
//! - `CreateResponseJob`: validates API key, no-ops if the row already exists
//!   (complete-response won the race), otherwise creates the realtime tracking
//!   batch in fusillade.
//! - `CompleteResponseJob`: updates the row to `completed`. If the row doesn't
//!   exist yet (create-response hasn't run), synthesizes it first using the
//!   request context carried in the job payload.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::store::{self as response_store};

// ---------------------------------------------------------------------------
// CreateResponse job
// ---------------------------------------------------------------------------

/// Input for the create-response background job.
///
/// Used by the realtime non-background path so the middleware can return a
/// response without blocking on the fusillade batch insert. The job creates
/// a single-request batch in `"processing"` state — the outlet handler's
/// `complete-response` job then transitions it to `"completed"`.
#[derive(Debug, Serialize, Deserialize)]
pub struct CreateResponseInput {
    /// Pre-generated request UUID; becomes the fusillade request's primary key.
    pub request_id: Uuid,
    /// The full request body as JSON string.
    pub body: String,
    /// Model name from the request.
    pub model: String,
    /// Loopback base URL so the fusillade daemon can reach dwctl if needed.
    pub base_url: String,
    /// Request endpoint (e.g., `/v1/responses`, `/v1/chat/completions`).
    pub endpoint: String,
    /// Bearer token from the Authorization header — used for attribution.
    pub api_key: Option<String>,
}

/// Build the underway job for creating response records.
pub async fn build_create_response_job<P: sqlx_pool_router::PoolProvider + Clone + Send + Sync + 'static>(
    pool: sqlx::PgPool,
    state: crate::tasks::TaskState<P>,
) -> anyhow::Result<underway::Job<CreateResponseInput, crate::tasks::TaskState<P>>> {
    use underway::Job;
    use underway::job::To;
    use underway::task::Error as TaskError;

    Job::<CreateResponseInput, _>::builder()
        .state(state)
        .step(|cx, input: CreateResponseInput| async move {
            // Validate API key and model access before creating the request
            if let Some(ref key) = input.api_key {
                if let Err(msg) =
                    crate::error_enrichment::validate_api_key_model_access(cx.state.dwctl_pool.clone(), key, &input.model).await
                {
                    tracing::debug!(
                        request_id = %input.request_id,
                        error = %msg,
                        "Skipping response creation — model access denied"
                    );
                    return To::done();
                }
            } else {
                tracing::debug!(
                    request_id = %input.request_id,
                    "Skipping response creation — no API key"
                );
                return To::done();
            }

            // Idempotency: complete-response can race ahead and create the row
            // itself. If the request already exists, we have nothing to do.
            match response_store::request_exists(&cx.state.request_manager, input.request_id).await {
                Ok(true) => {
                    tracing::debug!(
                        request_id = %input.request_id,
                        "Skipping response creation — row already exists (complete-response won the race)"
                    );
                    return To::done();
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::error!(
                        request_id = %input.request_id,
                        error = %e,
                        "Failed to check for existing request before create"
                    );
                    return Err(TaskError::Retryable(e.to_string()));
                }
            }

            // Resolve attribution from the API key.
            let created_by = response_store::lookup_created_by(&cx.state.dwctl_pool, input.api_key.as_deref()).await;

            tracing::debug!(
                request_id = %input.request_id,
                model = %input.model,
                endpoint = %input.endpoint,
                "create-response inserting fusillade row"
            );

            let batch_input = fusillade::CreateSingleRequestBatchInput {
                request_id: input.request_id,
                body: input.body,
                model: input.model.clone(),
                base_url: input.base_url,
                endpoint: input.endpoint,
                // Realtime tracking batch: completion window 0s, row pre-marked
                // "processing" so the fusillade daemon won't claim it — onwards
                // is already proxying it.
                completion_window: "0s".to_string(),
                initial_state: "processing".to_string(),
                api_key: input.api_key,
                created_by,
            };

            if let Err(e) = fusillade::Storage::create_single_request_batch(&*cx.state.request_manager, batch_input).await {
                // The pre-check is best-effort — complete-response can insert
                // the row in the TOCTOU window between request_exists and this
                // INSERT. Re-check; if it now exists we lost the race, our
                // work is done, no need to retry and no need to log loudly.
                if let Ok(true) = response_store::request_exists(&cx.state.request_manager, input.request_id).await {
                    tracing::debug!(
                        request_id = %input.request_id,
                        "create-response lost race after pre-check — row now exists, done"
                    );
                    return To::done();
                }
                tracing::error!(
                    request_id = %input.request_id,
                    error = %e,
                    "Failed to create realtime tracking batch"
                );
                return Err(TaskError::Retryable(e.to_string()));
            }

            tracing::debug!(
                request_id = %input.request_id,
                model = %input.model,
                "Created realtime tracking batch in fusillade"
            );

            To::done()
        })
        .name("create-response")
        .pool(pool)
        .build()
        .await
        .map_err(Into::into)
}

// ---------------------------------------------------------------------------
// CompleteResponse job
// ---------------------------------------------------------------------------

/// Input for the complete-response background job.
///
/// Enqueued by the FusilladeOutletHandler after outlet captures the response
/// body. Carries enough context to create the fusillade row from scratch in
/// case create-response hasn't run yet — see
/// [`super::store::complete_response_idempotent`].
#[derive(Debug, Serialize, Deserialize)]
pub struct CompleteResponseInput {
    /// Response ID (e.g., `resp_<uuid>`)
    pub response_id: String,
    /// HTTP status code from the upstream response
    pub status_code: u16,
    /// Response body as string (may be large for non-streaming responses)
    pub response_body: String,

    // Fields below are used only when create-response hasn't run yet and
    // we need to synthesize the row ourselves.
    /// Pre-generated request UUID (matches `response_id` minus prefix).
    pub request_id: Uuid,
    /// Original request body (JSON string).
    pub request_body: String,
    /// Model name from the request.
    pub model: String,
    /// Request endpoint (e.g., `/v1/responses`, `/v1/chat/completions`).
    pub endpoint: String,
    /// Loopback base URL (only used by the daemon for non-realtime; pass
    /// empty string when not relevant).
    pub base_url: String,
    /// Bearer token from the Authorization header — used for attribution.
    pub api_key: Option<String>,
}

/// Build the underway job for completing response records.
pub async fn build_complete_response_job<P: sqlx_pool_router::PoolProvider + Clone + Send + Sync + 'static>(
    pool: sqlx::PgPool,
    state: crate::tasks::TaskState<P>,
) -> anyhow::Result<underway::Job<CompleteResponseInput, crate::tasks::TaskState<P>>> {
    use underway::Job;
    use underway::job::To;
    use underway::task::Error as TaskError;

    Job::<CompleteResponseInput, _>::builder()
        .state(state)
        .step(|cx, input: CompleteResponseInput| async move {
            if (200..300).contains(&input.status_code) {
                let create_ctx = response_store::CreateContext {
                    request_id: input.request_id,
                    request_body: &input.request_body,
                    model: &input.model,
                    endpoint: &input.endpoint,
                    base_url: &input.base_url,
                    api_key: input.api_key.as_deref(),
                };
                if let Err(e) = response_store::complete_response_idempotent(
                    &cx.state.request_manager,
                    &cx.state.dwctl_pool,
                    &input.response_id,
                    &input.response_body,
                    input.status_code,
                    create_ctx,
                )
                .await
                {
                    tracing::error!(
                        response_id = %input.response_id,
                        error = %e,
                        "Failed to complete response in fusillade"
                    );
                    return Err(TaskError::Retryable(e.to_string()));
                }

                tracing::debug!(
                    response_id = %input.response_id,
                    status_code = input.status_code,
                    body_size = input.response_body.len(),
                    "Response completed in fusillade"
                );
            } else {
                let error_msg = format!("Upstream returned {}: {}", input.status_code, input.response_body);
                if let Err(e) = response_store::fail_response(&cx.state.request_manager, &input.response_id, &error_msg).await {
                    tracing::error!(
                        response_id = %input.response_id,
                        error = %e,
                        "Failed to mark response as failed in fusillade"
                    );
                    return Err(TaskError::Retryable(e.to_string()));
                }

                tracing::debug!(
                    response_id = %input.response_id,
                    status_code = input.status_code,
                    "Response marked as failed in fusillade"
                );
            }

            To::done()
        })
        .name("complete-response")
        .pool(pool)
        .build()
        .await
        .map_err(Into::into)
}
