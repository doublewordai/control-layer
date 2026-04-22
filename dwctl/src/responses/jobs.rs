//! Underway jobs for Open Responses API lifecycle management.
//!
//! Two jobs handle the full request lifecycle:
//! - `CreateResponseJob`: validates API key, creates fusillade template + request rows
//! - `CompleteResponseJob`: updates the request with response body/status or marks it failed

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::store::{self as response_store, OnwardsDaemonId};

// ---------------------------------------------------------------------------
// CreateResponse job
// ---------------------------------------------------------------------------

/// Input for the create-response background job.
///
/// Enqueued by the responses middleware. The job validates the API key,
/// then creates the fusillade template + request rows.
#[derive(Debug, Serialize, Deserialize)]
pub struct CreateResponseInput {
    /// Pre-generated response ID (e.g., `resp_<uuid>`)
    pub response_id: String,
    /// The full request body as JSON string
    pub request_body: String,
    /// Model name from the request
    pub model: String,
    /// Request endpoint (e.g., `/v1/responses`, `/v1/chat/completions`)
    pub endpoint: String,
    /// Bearer token from the Authorization header
    pub api_key: Option<String>,
    /// Onwards daemon ID for the processing state
    pub daemon_id: Uuid,
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
            // Validate the API key exists (lightweight auth gate)
            if let Some(ref key) = input.api_key {
                let key_exists = sqlx::query("SELECT 1 FROM public.api_keys WHERE secret = $1 AND is_deleted = false LIMIT 1")
                    .bind(key)
                    .fetch_optional(&cx.state.dwctl_pool)
                    .await
                    .map_err(|e| TaskError::Retryable(format!("API key lookup failed: {e}")))?
                    .is_some();

                if !key_exists {
                    tracing::debug!(
                        response_id = %input.response_id,
                        "Skipping response creation — invalid API key"
                    );
                    return To::done();
                }
            } else {
                tracing::debug!(
                    response_id = %input.response_id,
                    "Skipping response creation — no API key"
                );
                return To::done();
            }

            // Parse the request body back to JSON
            let request_value: serde_json::Value =
                serde_json::from_str(&input.request_body).map_err(|e| TaskError::Fatal(format!("Failed to parse request body: {e}")))?;

            // Create the daemon request via fusillade's Storage trait
            if let Err(e) = response_store::create_pending_with_id(
                &cx.state.request_manager,
                &input.response_id,
                &request_value,
                &input.model,
                &input.endpoint,
                OnwardsDaemonId(input.daemon_id),
            )
            .await
            {
                tracing::error!(
                    response_id = %input.response_id,
                    error = %e,
                    "Failed to create response record in fusillade"
                );
                return Err(TaskError::Retryable(e.to_string()));
            }

            tracing::debug!(
                response_id = %input.response_id,
                model = %input.model,
                "Created response record in fusillade"
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
    use underway::task::Error as TaskError;

    Job::<CompleteResponseInput, _>::builder()
        .state(state)
        .step(|cx, input: CompleteResponseInput| async move {
            if (200..300).contains(&input.status_code) {
                if let Err(e) =
                    response_store::complete_response(&cx.state.request_manager, &input.response_id, &input.response_body, input.status_code).await
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
