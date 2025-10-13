use axum::{extract::State, response::IntoResponse, Json};

use crate::{api::models::users::CurrentUser, AppState};

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
        ("X-Doubleword-User" = [])
    )
)]
pub async fn get_config(State(state): State<AppState>, _user: CurrentUser) -> impl IntoResponse {
    let mut metadata = state.config.metadata.clone();

    // Set registration_enabled based on native auth configuration
    metadata.registration_enabled = state.config.auth.native.enabled && state.config.auth.native.allow_registration;

    Json(metadata)
}
