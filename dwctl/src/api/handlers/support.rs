//! HTTP handlers for support request endpoints.

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use sqlx_pool_router::PoolProvider;
use utoipa::ToSchema;

use crate::{
    AppState,
    api::models::users::CurrentUser,
    email::EmailService,
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
    /// Whether the support request was sent successfully
    pub sent: bool,
}

/// Submit a support request via email
#[utoipa::path(
    post,
    path = "/support/requests",
    request_body = SupportRequest,
    responses(
        (status = 200, description = "Support request sent", body = SupportResponse),
        (status = 500, description = "Failed to send support request"),
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

    let email_service = EmailService::new(&config)?;
    email_service
        .send_support_request(
            &config.support_email,
            &current_user.email,
            current_user.display_name.as_deref(),
            subject,
            message,
        )
        .await?;

    Ok(Json(SupportResponse { sent: true }))
}
