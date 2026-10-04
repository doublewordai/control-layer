use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use sqlx::PgConnection;
use sqlx_pool_router::PoolProvider;

use crate::AppState;
use crate::api::models::realtime_inflight_limits::{
    ClearRealtimeInflightOverride, RealtimeInflightLimitsResponse, RealtimeInflightOverrideResponse, SetRealtimeInflightOverride,
};
use crate::auth::permissions::{RequiresPermission, operation, resource};
use crate::db::handlers::Deployments;
use crate::db::handlers::realtime_inflight_limits::RealtimeInflightLimits;
use crate::db::handlers::repository::Repository;
use crate::errors::{Error, Result};
use crate::types::{DeploymentId, UserId};

async fn virtual_model_default_limit(conn: &mut PgConnection, id: DeploymentId) -> Result<i32> {
    match Deployments::new(conn).get_by_id(id).await? {
        Some(model) if model.is_composite && !model.deleted => Ok(model.realtime_inflight_limit),
        _ => Err(Error::NotFound {
            resource: "Virtual model".to_string(),
            id: id.to_string(),
        }),
    }
}

#[utoipa::path(
    get,
    path = "/models/{id}/realtime-inflight-limits",
    tag = "models",
    summary = "A virtual model's realtime in-flight default and current per-account overrides",
    params(("id" = uuid::Uuid, Path, description = "Virtual model ID")),
    responses(
        (status = 200, description = "The default and every current override", body = RealtimeInflightLimitsResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Virtual model not found"),
    ),
    security(("BearerAuth" = []), ("CookieAuth" = []), ("X-Doubleword-User" = []))
)]
#[tracing::instrument(skip_all)]
pub async fn list_realtime_inflight_limits<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(id): Path<DeploymentId>,
    _: RequiresPermission<resource::ModelRateLimits, operation::ReadAll>,
) -> Result<Json<RealtimeInflightLimitsResponse>> {
    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let default_limit = virtual_model_default_limit(&mut conn, id).await?;
    let overrides = RealtimeInflightLimits::new(&mut conn).list_current(id).await?;
    Ok(Json(RealtimeInflightLimitsResponse {
        deployed_model_id: id,
        default_limit,
        overrides: overrides.into_iter().map(Into::into).collect(),
    }))
}

#[utoipa::path(
    get,
    path = "/models/{id}/realtime-inflight-limits/{account_id}/history",
    tag = "models",
    summary = "Every override an account has had on a virtual model, newest first",
    params(
        ("id" = uuid::Uuid, Path, description = "Virtual model ID"),
        ("account_id" = uuid::Uuid, Path, description = "Account ID: a user, or an organisation for organisation keys"),
    ),
    responses(
        (status = 200, description = "The account's override history", body = Vec<RealtimeInflightOverrideResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Virtual model not found"),
    ),
    security(("BearerAuth" = []), ("CookieAuth" = []), ("X-Doubleword-User" = []))
)]
#[tracing::instrument(skip_all)]
pub async fn get_realtime_inflight_override_history<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((id, account_id)): Path<(DeploymentId, UserId)>,
    _: RequiresPermission<resource::ModelRateLimits, operation::ReadAll>,
) -> Result<Json<Vec<RealtimeInflightOverrideResponse>>> {
    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    virtual_model_default_limit(&mut conn, id).await?;
    let history = RealtimeInflightLimits::new(&mut conn).history(id, account_id).await?;
    Ok(Json(history.into_iter().map(Into::into).collect()))
}

#[utoipa::path(
    put,
    path = "/models/{id}/realtime-inflight-limits/{account_id}",
    tag = "models",
    summary = "Set an account's realtime in-flight limit on a virtual model",
    params(
        ("id" = uuid::Uuid, Path, description = "Virtual model ID"),
        ("account_id" = uuid::Uuid, Path, description = "Account ID: a user, or an organisation for organisation keys"),
    ),
    request_body = SetRealtimeInflightOverride,
    responses(
        (status = 200, description = "The override now in force", body = RealtimeInflightOverrideResponse),
        (status = 400, description = "Invalid limit or empty reason, or the account does not exist"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Virtual model not found"),
    ),
    security(("BearerAuth" = []), ("CookieAuth" = []), ("X-Doubleword-User" = []))
)]
#[tracing::instrument(skip_all)]
pub async fn set_realtime_inflight_override<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((id, account_id)): Path<(DeploymentId, UserId)>,
    user: RequiresPermission<resource::ModelRateLimits, operation::UpdateAll>,
    Json(body): Json<SetRealtimeInflightOverride>,
) -> Result<Json<RealtimeInflightOverrideResponse>> {
    let mut tx = state.db.write().begin().await.map_err(|e| Error::Database(e.into()))?;
    virtual_model_default_limit(&mut tx, id).await?;
    let row = RealtimeInflightLimits::new(&mut tx)
        .replace(id, account_id, Some(body.limit), &body.reason, user.id)
        .await?;
    tx.commit().await.map_err(|e| Error::Database(e.into()))?;
    Ok(Json(row.into()))
}

#[utoipa::path(
    delete,
    path = "/models/{id}/realtime-inflight-limits/{account_id}",
    tag = "models",
    summary = "Return an account to the virtual model's default realtime in-flight limit",
    params(
        ("id" = uuid::Uuid, Path, description = "Virtual model ID"),
        ("account_id" = uuid::Uuid, Path, description = "Account ID: a user, or an organisation for organisation keys"),
    ),
    request_body = ClearRealtimeInflightOverride,
    responses(
        (status = 204, description = "The account now uses the default"),
        (status = 400, description = "Empty reason, or the account does not exist"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Virtual model not found"),
    ),
    security(("BearerAuth" = []), ("CookieAuth" = []), ("X-Doubleword-User" = []))
)]
#[tracing::instrument(skip_all)]
pub async fn clear_realtime_inflight_override<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((id, account_id)): Path<(DeploymentId, UserId)>,
    user: RequiresPermission<resource::ModelRateLimits, operation::UpdateAll>,
    Json(body): Json<ClearRealtimeInflightOverride>,
) -> Result<StatusCode> {
    let mut tx = state.db.write().begin().await.map_err(|e| Error::Database(e.into()))?;
    virtual_model_default_limit(&mut tx, id).await?;
    RealtimeInflightLimits::new(&mut tx)
        .replace(id, account_id, None, &body.reason, user.id)
        .await?;
    tx.commit().await.map_err(|e| Error::Database(e.into()))?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use crate::api::models::deployments::DeployedModelResponse;
    use crate::api::models::realtime_inflight_limits::{RealtimeInflightLimitsResponse, RealtimeInflightOverrideResponse};
    use crate::api::models::users::{Role, UserResponse};
    use crate::test::utils::{
        add_auth_headers, create_test_admin_user, create_test_app, create_test_deployment, create_test_org, create_test_user,
    };
    use axum_test::{TestResponse, TestServer};
    use serde_json::{Value, json};
    use sqlx::PgPool;

    async fn send(server: &TestServer, method: &str, path: &str, user: &UserResponse, body: Option<Value>) -> TestResponse {
        let headers = add_auth_headers(user);
        let request = match method {
            "GET" => server.get(path),
            "PUT" => server.put(path),
            "POST" => server.post(path),
            _ => server.delete(path),
        }
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1);
        match body {
            Some(body) => request.json(&body).await,
            None => request.await,
        }
    }

    async fn create_virtual_model(server: &TestServer, manager: &UserResponse, alias: &str) -> DeployedModelResponse {
        let response = send(
            server,
            "POST",
            "/admin/api/v1/models",
            manager,
            Some(json!({ "type": "composite", "model_name": alias, "alias": alias })),
        )
        .await;
        response.assert_status_ok();
        response.json()
    }

    #[dwctl_test_macros::test]
    #[test_log::test]
    async fn an_override_reaches_the_gateway_and_clearing_it_keeps_the_history(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        let org = create_test_org(&pool, manager.id).await;
        let model = create_virtual_model(&server, &manager, "inflight/virtual").await;
        let base = format!("/admin/api/v1/models/{}/realtime-inflight-limits", model.id);
        let account = format!("{base}/{}", org.id);

        let response = send(
            &server,
            "PUT",
            &account,
            &manager,
            Some(json!({ "limit": 250, "reason": "contracted burst" })),
        )
        .await;
        response.assert_status_ok();
        let set: RealtimeInflightOverrideResponse = response.json();
        assert_eq!(set.limit, Some(250));
        assert_eq!(set.set_by, manager.id);

        let listed: RealtimeInflightLimitsResponse = send(&server, "GET", &base, &manager, None).await.json();
        assert_eq!(listed.default_limit, 100);
        assert_eq!(listed.overrides.len(), 1);
        assert_eq!(listed.overrides[0].account_id, org.id);
        assert_eq!(listed.overrides[0].limit, Some(250));

        let targets = crate::sync::onwards_config::load_targets_from_db(&pool, &[], false).await.unwrap();
        let limits = targets
            .targets
            .get("inflight/virtual")
            .and_then(|pools| pools.default_pool().inflight_limits().cloned())
            .expect("a virtual model always carries its in-flight limits");
        assert_eq!(limits.default, 100);
        assert_eq!(limits.for_account(&org.id.to_string()), 250);
        assert_eq!(limits.for_account(&manager.id.to_string()), 100);

        send(&server, "DELETE", &account, &manager, Some(json!({ "reason": "contract ended" })))
            .await
            .assert_status(axum::http::StatusCode::NO_CONTENT);

        let listed: RealtimeInflightLimitsResponse = send(&server, "GET", &base, &manager, None).await.json();
        assert!(listed.overrides.is_empty());

        let history: Vec<RealtimeInflightOverrideResponse> =
            send(&server, "GET", &format!("{account}/history"), &manager, None).await.json();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].limit, None);
        assert_eq!(history[0].reason, "contract ended");
        assert_eq!(history[0].valid_until, None);
        assert_eq!(history[1].limit, Some(250));
        assert_eq!(history[1].valid_until, Some(history[0].valid_from));
    }

    #[dwctl_test_macros::test]
    #[test_log::test]
    async fn only_platform_managers_can_change_overrides(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        let customer = create_test_user(&pool, Role::StandardUser).await;
        let model = create_virtual_model(&server, &manager, "inflight/guarded").await;
        let account = format!("/admin/api/v1/models/{}/realtime-inflight-limits/{}", model.id, customer.id);

        send(
            &server,
            "PUT",
            &account,
            &customer,
            Some(json!({ "limit": 1000, "reason": "please" })),
        )
        .await
        .assert_status_forbidden();
        send(
            &server,
            "GET",
            &format!("/admin/api/v1/models/{}/realtime-inflight-limits", model.id),
            &customer,
            None,
        )
        .await
        .assert_status_forbidden();
    }

    #[dwctl_test_macros::test]
    #[test_log::test]
    async fn overrides_need_a_virtual_model_a_positive_limit_and_a_reason(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        let customer = create_test_user(&pool, Role::StandardUser).await;
        let virtual_model = create_virtual_model(&server, &manager, "inflight/validated").await;
        let standard_model = create_test_deployment(&pool, manager.id, "standard-model", "standard-model").await;

        send(
            &server,
            "PUT",
            &format!(
                "/admin/api/v1/models/{}/realtime-inflight-limits/{}",
                standard_model.id, customer.id
            ),
            &manager,
            Some(json!({ "limit": 10, "reason": "component" })),
        )
        .await
        .assert_status_not_found();

        let account = format!("/admin/api/v1/models/{}/realtime-inflight-limits/{}", virtual_model.id, customer.id);
        send(&server, "PUT", &account, &manager, Some(json!({ "limit": 0, "reason": "zero" })))
            .await
            .assert_status_bad_request();
        send(&server, "PUT", &account, &manager, Some(json!({ "limit": 10, "reason": "  " })))
            .await
            .assert_status_bad_request();
    }
}
