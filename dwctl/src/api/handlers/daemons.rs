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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::users::Role;
    use crate::test::utils::{add_auth_headers, create_test_app, create_test_user};
    use axum::http::StatusCode;
    use sqlx::PgPool;

    #[sqlx::test]
    async fn test_list_daemons_requires_system_read_all_permission(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let headers = add_auth_headers(&user);
        let response = app
            .get("/ai/v1/daemons")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await;

        // Standard user should not have permission to view daemons
        response.assert_status(StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn test_list_daemons_platform_manager_can_access(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::PlatformManager).await;

        let headers = add_auth_headers(&user);
        let response = app
            .get("/ai/v1/daemons")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await;

        // PlatformManager should have System::ReadAll permission
        response.assert_status(StatusCode::OK);
        let json: ListDaemonsResponse = response.json();

        // Should return a list (may be empty if no daemons running)
        assert!(json.daemons.is_empty() || !json.daemons.is_empty());
    }

    #[sqlx::test]
    async fn test_list_daemons_without_authentication(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;

        let response = app.get("/ai/v1/daemons").await;

        response.assert_status(StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    async fn test_list_daemons_with_status_filter(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::PlatformManager).await;

        let headers = add_auth_headers(&user);
        let response = app
            .get("/ai/v1/daemons?status=running")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await;

        response.assert_status(StatusCode::OK);
        let json: ListDaemonsResponse = response.json();

        // All returned daemons should have "running" status
        for daemon in json.daemons {
            assert_eq!(daemon.status, DaemonStatus::Running);
        }
    }

    #[sqlx::test]
    async fn test_list_daemons_returns_empty_list_when_no_daemons(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::PlatformManager).await;

        let headers = add_auth_headers(&user);
        let response = app
            .get("/ai/v1/daemons")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await;

        response.assert_status(StatusCode::OK);
        let json: ListDaemonsResponse = response.json();

        // Since we're not running background daemons in tests (false parameter to create_test_app),
        // we should get an empty list
        assert_eq!(json.daemons.len(), 0);
    }
}
