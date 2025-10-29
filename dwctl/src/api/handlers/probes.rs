use crate::api::models::probes::{
    CreateProbe, ProbeStatistics, ProbesQuery, ResultsQuery, StatsQuery, TestProbeRequest, UpdateProbeRequest,
};
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
    let probe = ProbeManager::update_probe(&state.db, id, update).await?;
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
    Json(request): Json<Option<TestProbeRequest>>,
) -> Result<(StatusCode, Json<ProbeResult>), Error> {
    let (http_method, request_path, request_body) = if let Some(req) = request {
        (req.http_method, req.request_path, req.request_body)
    } else {
        (None, None, None)
    };

    let result = ProbeManager::test_probe(&state.db, deployment_id, &state.config, http_method, request_path, request_body).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::models::users::Role,
        db::models::probes::Probe,
        test_utils::{add_auth_headers, create_test_admin_user, create_test_app, create_test_user},
    };
    use sqlx::PgPool;

    async fn setup_test_deployment(pool: &PgPool, user_id: Uuid) -> Uuid {
        let unique_id = Uuid::new_v4();
        let endpoint_name = format!("test-endpoint-{}", unique_id);
        let model_name = format!("test-model-{}", unique_id);

        let endpoint_id = sqlx::query_scalar!(
            "INSERT INTO inference_endpoints (name, url, created_by) VALUES ($1, $2, $3) RETURNING id",
            endpoint_name,
            "http://localhost:8080",
            user_id
        )
        .fetch_one(pool)
        .await
        .unwrap();

        sqlx::query_scalar!(
            "INSERT INTO deployed_models (model_name, alias, type, hosted_on, created_by) VALUES ($1, $2, $3, $4, $5) RETURNING id",
            model_name.clone(),
            model_name,
            "chat" as _,
            endpoint_id,
            user_id
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_probe(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let deployment_id = setup_test_deployment(&pool, user.id).await;

        let payload = serde_json::json!({
            "name": "Test Probe",
            "deployment_id": deployment_id,
            "interval_seconds": 60
        });

        let response = app
            .post("/admin/api/v1/probes")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .json(&payload)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let probe: Probe = response.json();
        assert_eq!(probe.name, "Test Probe");
        assert_eq!(probe.deployment_id, deployment_id);
        assert_eq!(probe.interval_seconds, 60);
        assert!(probe.active);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_probe_unauthorized(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let deployment_id = setup_test_deployment(&pool, user.id).await;

        let payload = serde_json::json!({
            "name": "Test Probe",
            "deployment_id": deployment_id,
            "interval_seconds": 60
        });

        let response = app
            .post("/admin/api/v1/probes")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .json(&payload)
            .await;

        response.assert_status(axum::http::StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_probes(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Create test probes
        let deployment_id1 = setup_test_deployment(&pool, user.id).await;
        let deployment_id2 = setup_test_deployment(&pool, user.id).await;

        ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Probe 1".to_string(),
                deployment_id: deployment_id1,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Probe 2".to_string(),
                deployment_id: deployment_id2,
                interval_seconds: 120,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        let response = app
            .get("/admin/api/v1/probes")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let probes: Vec<Probe> = response.json();
        assert_eq!(probes.len(), 2);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_probe(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let deployment_id = setup_test_deployment(&pool, user.id).await;

        let created = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Test Probe".to_string(),
                deployment_id,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        let response = app
            .get(&format!("/admin/api/v1/probes/{}", created.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let probe: Probe = response.json();
        assert_eq!(probe.id, created.id);
        assert_eq!(probe.name, "Test Probe");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_probe_not_found(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let non_existent_id = Uuid::new_v4();

        let response = app
            .get(&format!("/admin/api/v1/probes/{}", non_existent_id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_probe(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let deployment_id = setup_test_deployment(&pool, user.id).await;

        let created = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Original Name".to_string(),
                deployment_id,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        let payload = serde_json::json!({
            "interval_seconds": 120
        });

        let response = app
            .patch(&format!("/admin/api/v1/probes/{}", created.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .json(&payload)
            .await;

        response.assert_status_ok();
        let probe: Probe = response.json();
        assert_eq!(probe.name, "Original Name"); // Name should not change
        assert_eq!(probe.interval_seconds, 120);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_activate_probe(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let deployment_id = setup_test_deployment(&pool, user.id).await;

        let created = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Test Probe".to_string(),
                deployment_id,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        // First deactivate it
        ProbeManager::deactivate_probe(&pool, created.id).await.unwrap();

        let response = app
            .patch(&format!("/admin/api/v1/probes/{}/activate", created.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let probe: Probe = response.json();
        assert!(probe.active);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_deactivate_probe(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let deployment_id = setup_test_deployment(&pool, user.id).await;

        let created = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Test Probe".to_string(),
                deployment_id,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        let response = app
            .patch(&format!("/admin/api/v1/probes/{}/deactivate", created.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let probe: Probe = response.json();
        assert!(!probe.active);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_probe(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let deployment_id = setup_test_deployment(&pool, user.id).await;

        let created = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Test Probe".to_string(),
                deployment_id,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        let response = app
            .delete(&format!("/admin/api/v1/probes/{}", created.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status(axum::http::StatusCode::NO_CONTENT);

        // Verify probe is deleted
        let get_response = app
            .get(&format!("/admin/api/v1/probes/{}", created.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        get_response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_probe_results(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let deployment_id = setup_test_deployment(&pool, user.id).await;

        let created = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Test Probe".to_string(),
                deployment_id,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        let response = app
            .get(&format!("/admin/api/v1/probes/{}/results", created.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let results: Vec<ProbeResult> = response.json();
        assert!(results.is_empty()); // No results initially
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_statistics(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let deployment_id = setup_test_deployment(&pool, user.id).await;

        let created = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Test Probe".to_string(),
                deployment_id,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        let response = app
            .get(&format!("/admin/api/v1/probes/{}/statistics", created.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let stats: ProbeStatistics = response.json();
        assert_eq!(stats.total_executions, 0);
    }
}
