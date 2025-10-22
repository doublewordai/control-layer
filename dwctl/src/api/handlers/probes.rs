use crate::auth::permissions::{operation, resource, RequiresPermission};
use crate::errors::Error;
use crate::probes::models::{CreateProbe, Probe, ProbeResult, ProbeStatistics};
use crate::probes::db::ProbeManager;
use crate::AppState;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

// Query parameters for filtering probes
#[derive(Deserialize)]
pub struct ProbesQuery {
    status: Option<String>,
}

// Query parameters for filtering results
#[derive(Deserialize)]
pub struct ResultsQuery {
    start_time: Option<DateTime<Utc>>,
    end_time: Option<DateTime<Utc>>,
    limit: Option<i64>,
}

#[derive(Deserialize)]
pub struct StatsQuery {
    start_time: Option<DateTime<Utc>>,
    end_time: Option<DateTime<Utc>>,
}

// POST /probes - Create a new probe
pub async fn create_probe(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::CreateAll>,
    Json(probe): Json<CreateProbe>,
) -> Result<(StatusCode, Json<Probe>), Error> {
    let probe_manager = state.probe_manager.as_ref()
        .ok_or_else(|| Error::Internal {
            operation: "Probe manager not initialized".to_string(),
        })?;

    let created = probe_manager.create_probe(&state.db, probe).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

// GET /probes - List all probes (optionally filtered by ?status=active)
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

// GET /probes/:id - Get a specific probe
pub async fn get_probe(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::ReadAll>,
    Path(id): Path<Uuid>,
) -> Result<Json<Probe>, Error> {
    let probe = ProbeManager::get_probe(&state.db, id).await?;
    Ok(Json(probe))
}

// DELETE /probes/:id - Delete a probe
pub async fn delete_probe(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::DeleteAll>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Error> {
    let probe_manager = state.probe_manager.as_ref()
        .ok_or_else(|| Error::Internal {
            operation: "Probe manager not initialized".to_string(),
        })?;

    probe_manager.delete_probe(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// PATCH /probes/:id/activate - Activate a probe
pub async fn activate_probe(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::UpdateAll>,
    Path(id): Path<Uuid>,
) -> Result<Json<Probe>, Error> {
    let probe_manager = state.probe_manager.as_ref()
        .ok_or_else(|| Error::Internal {
            operation: "Probe manager not initialized".to_string(),
        })?;

    let probe = probe_manager.activate_probe(&state.db, id).await?;
    Ok(Json(probe))
}

// PATCH /probes/:id/deactivate - Deactivate a probe
pub async fn deactivate_probe(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::UpdateAll>,
    Path(id): Path<Uuid>,
) -> Result<Json<Probe>, Error> {
    let probe_manager = state.probe_manager.as_ref()
        .ok_or_else(|| Error::Internal {
            operation: "Probe manager not initialized".to_string(),
        })?;

    let probe = probe_manager.deactivate_probe(&state.db, id).await?;
    Ok(Json(probe))
}

// PATCH /probes/:id - Update a probe
#[derive(Debug, Deserialize)]
pub struct UpdateProbeRequest {
    interval_seconds: Option<i32>,
}

pub async fn update_probe(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::UpdateAll>,
    Path(id): Path<Uuid>,
    Json(update): Json<UpdateProbeRequest>,
) -> Result<Json<Probe>, Error> {
    let probe_manager = state.probe_manager.as_ref()
        .ok_or_else(|| Error::Internal {
            operation: "Probe manager not initialized".to_string(),
        })?;

    let probe = probe_manager.update_probe(&state.db, id, update.interval_seconds).await?;
    Ok(Json(probe))
}

// POST /probes/:id/execute - Execute a probe immediately
pub async fn execute_probe(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::UpdateAll>,
    Path(id): Path<Uuid>,
) -> Result<(StatusCode, Json<ProbeResult>), Error> {
    let result = ProbeManager::execute_probe(&state.db, id).await?;
    Ok((StatusCode::CREATED, Json(result)))
}

// POST /probes/test/:deployment_id - Test a probe configuration without creating it
pub async fn test_probe(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::ReadAll>,
    Path(deployment_id): Path<Uuid>,
) -> Result<(StatusCode, Json<ProbeResult>), Error> {
    let result = ProbeManager::test_probe(&state.db, deployment_id).await?;
    Ok((StatusCode::OK, Json(result)))
}

// GET /probes/:id/results - Get probe results
pub async fn get_probe_results(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::ReadAll>,
    Path(id): Path<Uuid>,
    Query(query): Query<ResultsQuery>,
) -> Result<Json<Vec<ProbeResult>>, Error> {
    let results = ProbeManager::get_probe_results(
        &state.db,
        id,
        query.start_time,
        query.end_time,
        query.limit,
    )
    .await?;
    Ok(Json(results))
}

// GET /probes/:id/statistics - Get probe statistics
pub async fn get_statistics(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Probes, operation::ReadAll>,
    Path(id): Path<Uuid>,
    Query(query): Query<StatsQuery>,
) -> Result<Json<ProbeStatistics>, Error> {
    let stats = ProbeManager::get_statistics(&state.db, id, query.start_time, query.end_time).await?;
    Ok(Json(stats))
}
