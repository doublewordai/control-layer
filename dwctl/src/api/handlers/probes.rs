use crate::api::models::probes::{CreateProbe, ProbeStatistics, ProbesQuery, ResultsQuery, StatsQuery, UpdateProbeRequest};
use crate::auth::permissions::{operation, resource, RequiresPermission};
use crate::db::models::probes::{Probe, ProbeResult};
use crate::errors::Error;
use crate::probes::db::ProbeManager;
use crate::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use uuid::Uuid;

#[utoipa::path(
    post,
    path = "/probes",
    tag = "probes",
    summary = "Create a new probe",
    description = "Create a new probe to monitor a deployed model. The probe is automatically activated and starts executing on its configured interval.",
    request_body = CreateProbe,
    responses(
        (status = 201, description = "Probe created successfully", body = Probe),
        (status = 400, description = "Bad request - invalid probe data"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Deployment not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn create_probe(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::CreateAll>,
    Json(probe): Json<CreateProbe>,
) -> Result<(StatusCode, Json<Probe>), Error> {
    let created = ProbeManager::create_probe(&state.db, probe).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

#[utoipa::path(
    get,
    path = "/probes",
    tag = "probes",
    summary = "List all probes",
    description = "List all probes, optionally filtered by status",
    params(
        ProbesQuery
    ),
    responses(
        (status = 200, description = "List of probes", body = Vec<Probe>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn list_probes(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::ReadAll>,
    Query(query): Query<ProbesQuery>,
) -> Result<Json<Vec<Probe>>, Error> {
    let probes = match query.status.as_deref() {
        Some("active") => ProbeManager::list_active_probes(&state.db).await?,
        _ => ProbeManager::list_probes(&state.db).await?,
    };
    Ok(Json(probes))
}

#[utoipa::path(
    get,
    path = "/probes/{id}",
    tag = "probes",
    summary = "Get a specific probe",
    description = "Get detailed information about a specific probe by ID",
    params(
        ("id" = uuid::Uuid, Path, description = "Probe ID to retrieve"),
    ),
    responses(
        (status = 200, description = "Probe details", body = Probe),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Probe not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn get_probe(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::ReadAll>,
    Path(id): Path<Uuid>,
) -> Result<Json<Probe>, Error> {
    let probe = ProbeManager::get_probe(&state.db, id).await?;
    Ok(Json(probe))
}

#[utoipa::path(
    delete,
    path = "/probes/{id}",
    tag = "probes",
    summary = "Delete a probe",
    description = "Delete a probe and stop its scheduler",
    params(
        ("id" = uuid::Uuid, Path, description = "Probe ID to delete"),
    ),
    responses(
        (status = 204, description = "Probe deleted successfully"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Probe not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn delete_probe(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::DeleteAll>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Error> {
    ProbeManager::delete_probe(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    patch,
    path = "/probes/{id}/activate",
    tag = "probes",
    summary = "Activate a probe",
    description = "Activate a probe and start its scheduler to begin executing at its configured interval",
    params(
        ("id" = uuid::Uuid, Path, description = "Probe ID to activate"),
    ),
    responses(
        (status = 200, description = "Probe activated successfully", body = Probe),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Probe not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn activate_probe(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::UpdateAll>,
    Path(id): Path<Uuid>,
) -> Result<Json<Probe>, Error> {
    let probe = ProbeManager::activate_probe(&state.db, id).await?;
    Ok(Json(probe))
}

#[utoipa::path(
    patch,
    path = "/probes/{id}/deactivate",
    tag = "probes",
    summary = "Deactivate a probe",
    description = "Deactivate a probe and stop its scheduler to stop executing",
    params(
        ("id" = uuid::Uuid, Path, description = "Probe ID to deactivate"),
    ),
    responses(
        (status = 200, description = "Probe deactivated successfully", body = Probe),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Probe not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn deactivate_probe(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::UpdateAll>,
    Path(id): Path<Uuid>,
) -> Result<Json<Probe>, Error> {
    let probe = ProbeManager::deactivate_probe(&state.db, id).await?;
    Ok(Json(probe))
}

#[utoipa::path(
    patch,
    path = "/probes/{id}",
    tag = "probes",
    summary = "Update a probe",
    description = "Update probe configuration such as execution interval",
    params(
        ("id" = uuid::Uuid, Path, description = "Probe ID to update"),
    ),
    request_body = UpdateProbeRequest,
    responses(
        (status = 200, description = "Probe updated successfully", body = Probe),
        (status = 400, description = "Bad request - invalid update data"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Probe not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn update_probe(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::UpdateAll>,
    Path(id): Path<Uuid>,
    Json(update): Json<UpdateProbeRequest>,
) -> Result<Json<Probe>, Error> {
    let probe = ProbeManager::update_probe(&state.db, id, update.interval_seconds).await?;
    Ok(Json(probe))
}

#[utoipa::path(
    post,
    path = "/probes/{id}/execute",
    tag = "probes",
    summary = "Execute a probe immediately",
    description = "Manually trigger a probe execution without waiting for the scheduled interval",
    params(
        ("id" = uuid::Uuid, Path, description = "Probe ID to execute"),
    ),
    responses(
        (status = 201, description = "Probe executed successfully", body = ProbeResult),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Probe not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn execute_probe(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::UpdateAll>,
    Path(id): Path<Uuid>,
) -> Result<(StatusCode, Json<ProbeResult>), Error> {
    let result = ProbeManager::execute_probe(&state.db, id, &state.config).await?;
    Ok((StatusCode::CREATED, Json(result)))
}

#[utoipa::path(
    post,
    path = "/probes/test/{deployment_id}",
    tag = "probes",
    summary = "Test a probe configuration",
    description = "Test a probe configuration for a deployment without creating an actual probe",
    params(
        ("deployment_id" = uuid::Uuid, Path, description = "Deployment ID to test probe against"),
    ),
    responses(
        (status = 200, description = "Probe test executed successfully", body = ProbeResult),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Deployment not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn test_probe(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::ReadAll>,
    Path(deployment_id): Path<Uuid>,
) -> Result<(StatusCode, Json<ProbeResult>), Error> {
    let result = ProbeManager::test_probe(&state.db, deployment_id, &state.config).await?;
    Ok((StatusCode::OK, Json(result)))
}

#[utoipa::path(
    get,
    path = "/probes/{id}/results",
    tag = "probes",
    summary = "Get probe execution results",
    description = "Retrieve historical execution results for a probe, optionally filtered by time range and limited",
    params(
        ("id" = uuid::Uuid, Path, description = "Probe ID to get results for"),
        ResultsQuery
    ),
    responses(
        (status = 200, description = "List of probe execution results", body = Vec<ProbeResult>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Probe not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn get_probe_results(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::ReadAll>,
    Path(id): Path<Uuid>,
    Query(query): Query<ResultsQuery>,
) -> Result<Json<Vec<ProbeResult>>, Error> {
    let results = ProbeManager::get_probe_results(&state.db, id, query.start_time, query.end_time, query.limit).await?;
    Ok(Json(results))
}

#[utoipa::path(
    get,
    path = "/probes/{id}/statistics",
    tag = "probes",
    summary = "Get probe statistics",
    description = "Get aggregated statistics for a probe including success rates, response times, and percentiles",
    params(
        ("id" = uuid::Uuid, Path, description = "Probe ID to get statistics for"),
        StatsQuery
    ),
    responses(
        (status = 200, description = "Probe statistics", body = ProbeStatistics),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Probe not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn get_statistics(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::ReadAll>,
    Path(id): Path<Uuid>,
    Query(query): Query<StatsQuery>,
) -> Result<Json<ProbeStatistics>, Error> {
    let stats = ProbeManager::get_statistics(&state.db, id, query.start_time, query.end_time).await?;
    Ok(Json(stats))
}
