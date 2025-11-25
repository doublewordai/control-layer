//! Handlers for the Daemons API.
//!
//! Provides endpoints for monitoring daemon status.

use crate::AppState;
use crate::api::models::daemons::{DaemonResponse, DaemonStats, DaemonStatus, ListDaemonsQuery, ListDaemonsResponse};
use crate::auth::permissions::{RequiresPermission, operation, resource};
use crate::errors::Result;
use axum::{
    Json,
    extract::{Query, State},
};
use fusillade::daemon::AnyDaemonRecord;
use fusillade::manager::DaemonStorage;

/// Helper function to convert fusillade DaemonStats to API DaemonStats
fn to_api_stats(stats: &fusillade::daemon::DaemonStats) -> DaemonStats {
    DaemonStats {
        requests_processed: stats.requests_processed,
        requests_failed: stats.requests_failed,
        requests_in_flight: stats.requests_in_flight,
    }
}

/// Helper function to convert fusillade AnyDaemonRecord to API DaemonResponse
fn to_daemon_response(daemon: AnyDaemonRecord) -> DaemonResponse {
    match daemon {
        AnyDaemonRecord::Initializing(d) => DaemonResponse {
            id: d.data.id.0.to_string(),
            status: DaemonStatus::Initializing,
            hostname: d.data.hostname.clone(),
            pid: d.data.pid,
            version: d.data.version.clone(),
            started_at: d.state.started_at.timestamp(),
            last_heartbeat: None,
            stopped_at: None,
            stats: DaemonStats {
                requests_processed: 0,
                requests_failed: 0,
                requests_in_flight: 0,
            },
            config: d.data.config_snapshot,
        },
        AnyDaemonRecord::Running(d) => DaemonResponse {
            id: d.data.id.0.to_string(),
            status: DaemonStatus::Running,
            hostname: d.data.hostname.clone(),
            pid: d.data.pid,
            version: d.data.version.clone(),
            started_at: d.state.started_at.timestamp(),
            last_heartbeat: Some(d.state.last_heartbeat.timestamp()),
            stopped_at: None,
            stats: to_api_stats(&d.state.stats),
            config: d.data.config_snapshot,
        },
        AnyDaemonRecord::Dead(d) => DaemonResponse {
            id: d.data.id.0.to_string(),
            status: DaemonStatus::Dead,
            hostname: d.data.hostname.clone(),
            pid: d.data.pid,
            version: d.data.version.clone(),
            started_at: d.state.started_at.timestamp(),
            last_heartbeat: None,
            stopped_at: Some(d.state.stopped_at.timestamp()),
            stats: to_api_stats(&d.state.final_stats),
            config: d.data.config_snapshot,
        },
    }
}

/// List all daemons with optional status filtering.
///
/// GET /ai/v1/daemons
pub async fn list_daemons(
    State(state): State<AppState>,
    Query(query): Query<ListDaemonsQuery>,
    _current_user: RequiresPermission<resource::System, operation::ReadAll>,
) -> Result<Json<ListDaemonsResponse>> {
    // Convert API status to fusillade status
    let status_filter = query.status.map(|s| match s {
        DaemonStatus::Initializing => fusillade::daemon::DaemonStatus::Initializing,
        DaemonStatus::Running => fusillade::daemon::DaemonStatus::Running,
        DaemonStatus::Dead => fusillade::daemon::DaemonStatus::Dead,
    });

    // Get daemons from fusillade
    let daemons = state
        .request_manager
        .list_daemons(status_filter)
        .await
        .map_err(|e| crate::errors::Error::Internal {
            operation: format!("list daemons: {}", e),
        })?;

    // Convert to API response
    let daemon_responses: Vec<DaemonResponse> = daemons.into_iter().map(to_daemon_response).collect();

    Ok(Json(ListDaemonsResponse { daemons: daemon_responses }))
}
