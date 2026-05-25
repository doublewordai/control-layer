//! HTTP handlers for support request endpoints.

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use sqlx_pool_router::PoolProvider;
use utoipa::ToSchema;

use crate::{
    AppState,
    api::models::users::CurrentUser,
    email_jobs::SendEmailInput,
    errors::{Error, Result},
};

#[derive(Debug, Deserialize, ToSchema)]
pub struct SupportRequest {
    /// Subject line for the support request
    pub subject: String,
    /// Message body
    pub message: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SupportResponse {
    /// Whether the support request was accepted for delivery. Accepted does
    /// not mean delivered — the actual send runs asynchronously via the
    /// `send-email` worker, which retries transient provider failures.
    pub sent: bool,
}

/// Submit a support request via email.
///
/// Enqueues the send rather than awaiting it inline so the caller doesn't
/// see a 5xx when the upstream provider is unavailable or rate-limiting.
/// The actual delivery runs in the `send-email` worker with retry on
/// transient errors.
#[utoipa::path(
    post,
    path = "/support/requests",
    request_body = SupportRequest,
    responses(
        (status = 200, description = "Support request accepted for delivery", body = SupportResponse),
        (status = 400, description = "Subject or message missing"),
        (status = 500, description = "Failed to enqueue support request"),
    ),
    security(("BearerAuth" = []), ("CookieAuth" = []), ("X-Doubleword-User" = [])),
)]
#[tracing::instrument(skip_all)]
pub async fn submit_support_request<P: PoolProvider>(
    State(state): State<AppState<P>>,
    current_user: CurrentUser,
    Json(request): Json<SupportRequest>,
) -> Result<Json<SupportResponse>> {
    let subject = request.subject.trim();
    let message = request.message.trim();
    let config = state.current_config();

    if subject.is_empty() || message.is_empty() {
        return Err(Error::BadRequest {
            message: "Subject and message are required".to_string(),
        });
    }

    state
        .task_runner
        .send_email_job
        .enqueue(&SendEmailInput::SupportRequest {
            support_email: config.support_email.clone(),
            user_email: current_user.email.clone(),
            user_name: current_user.display_name.clone(),
            subject: subject.to_string(),
            message: message.to_string(),
        })
        .await
        .map_err(|e| Error::Internal {
            operation: format!("enqueue support request: {e}"),
        })?;

    Ok(Json(SupportResponse { sent: true }))
}
