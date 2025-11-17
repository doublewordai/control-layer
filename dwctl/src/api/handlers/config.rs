//! HTTP handlers for configuration retrieval endpoints.

use axum::{extract::State, response::IntoResponse, Json};
use serde::Serialize;

use crate::{api::models::users::CurrentUser, AppState};

/// Configuration response with computed fields
#[derive(Debug, Clone, Serialize)]
pub struct ConfigResponse {
    pub region: String,
    pub organization: String,
    pub registration_enabled: bool,
    pub payment_enabled: bool,
}

#[utoipa::path(
    delete,
    path = "/config",
    tag = "config",
    summary = "Get config",
    description = "Get current app configuration",
    responses(
        (status = 200, description = "Got metadata"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_config(State(state): State<AppState>, _user: CurrentUser) -> impl IntoResponse {
    let metadata = &state.config.metadata;

    let response = ConfigResponse {
        region: metadata.region.clone(),
        organization: metadata.organization.clone(),
        // Compute registration_enabled based on native auth configuration
        registration_enabled: state.config.auth.native.enabled && state.config.auth.native.allow_registration,
        // Compute payment_enabled based on whether payment_processor is configured
        payment_enabled: state.config.payment_processor.is_some(),
    };

    Json(response)
}
