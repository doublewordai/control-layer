//! Handlers that serve the OpenAPI specs and Scalar doc UIs.
//!
//! Both surfaces require authentication. The Admin surface additionally
//! requires an admin-level identity (PlatformManager role, the admin
//! user, or a `platform`-purpose API key) — the spec describes the full
//! internal management API and would otherwise hand attackers a map of
//! the privileged surface.

use axum::{
    Json,
    response::{Html, IntoResponse, Response},
};
use utoipa::OpenApi;
use utoipa_scalar::{Scalar, Servable};

use crate::{
    api::models::users::CurrentUser,
    auth::permissions::{RequiresPermission, operation, resource},
    openapi::{AdminApiDoc, AiApiDoc},
};

/// Serve the Admin OpenAPI spec as JSON.
///
/// `RequiresPermission<System, ReadAll>` is the admin-or-PlatformManager
/// gate: `is_admin` bypasses, PlatformManager is granted (System is not
/// Requests), and StandardUser / RequestViewer are denied.
#[tracing::instrument(skip_all)]
pub async fn admin_openapi_json(_: RequiresPermission<resource::System, operation::ReadAll>) -> Json<utoipa::openapi::OpenApi> {
    Json(AdminApiDoc::openapi())
}

/// Serve the Scalar UI for the Admin OpenAPI spec.
#[tracing::instrument(skip_all)]
pub async fn admin_openapi_docs(_: RequiresPermission<resource::System, operation::ReadAll>) -> Response {
    let scalar = Scalar::with_url("/admin/openapi.json", AdminApiDoc::openapi());
    Html(scalar.to_html()).into_response()
}

/// Serve the AI OpenAPI spec as JSON. Any authenticated identity may read it.
#[tracing::instrument(skip_all)]
pub async fn ai_openapi_json(_: CurrentUser) -> Json<utoipa::openapi::OpenApi> {
    Json(AiApiDoc::openapi())
}

/// Serve the Scalar UI for the AI OpenAPI spec.
#[tracing::instrument(skip_all)]
pub async fn ai_openapi_docs(_: CurrentUser) -> Response {
    let scalar = Scalar::with_url("/ai/openapi.json", AiApiDoc::openapi());
    Html(scalar.to_html()).into_response()
}
