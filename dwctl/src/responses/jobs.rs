//! Underway jobs for Open Responses API lifecycle management.
//!
//! Two jobs handle the response lifecycle:
//! - `CreateResponseJob`: validates API key, creates the realtime tracking batch in fusillade
//! - `CompleteResponseJob`: updates the request with response body/status or marks it failed

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

            // Resolve attribution from the API key.
            let created_by = response_store::lookup_created_by(&cx.state.dwctl_pool, input.api_key.as_deref()).await;

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
/// Enqueued by the FusilladeOutletHandler after outlet captures the response body.
#[derive(Debug, Serialize, Deserialize)]
pub struct CompleteResponseInput {
    /// Response ID (e.g., `resp_<uuid>`)
    pub response_id: String,
    /// HTTP status code from the upstream response
    pub status_code: u16,
    /// Response body as string (may be large for non-streaming responses)
    pub response_body: String,
}

/// Build the underway job for completing response records.
pub async fn build_complete_response_job<P: sqlx_pool_router::PoolProvider + Clone + Send + Sync + 'static>(
    pool: sqlx::PgPool,
    state: crate::tasks::TaskState<P>,
) -> anyhow::Result<underway::Job<CompleteResponseInput, crate::tasks::TaskState<P>>> {
    use underway::Job;
    use underway::job::To;
    use underway::task::{Error as TaskError, RetryPolicy};

    // Tight retries to ride out the create-response vs complete-response race.
    // Both jobs are enqueued within ~50ms of each other; if complete-response
    // wins, the fusillade row isn't there yet and we need to retry quickly.
    // Default is 5 attempts at 1s/2s/4s/8s — way too slow for realtime flows.
    let retry_policy = RetryPolicy::builder().max_attempts(10).initial_interval_ms(100).build();

    Job::<CompleteResponseInput, _>::builder()
        .state(state)
        .step(|cx, input: CompleteResponseInput| async move {
            if (200..300).contains(&input.status_code) {
                if let Err(e) = response_store::complete_response(
                    &cx.state.request_manager,
                    &input.response_id,
                    &input.response_body,
                    input.status_code,
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
        .retry_policy(retry_policy)
        .name("complete-response")
        .pool(pool)
        .build()
        .await
        .map_err(Into::into)
}
