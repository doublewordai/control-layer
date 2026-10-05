use axum::Json;
use axum::extract::{Path, State};
use sqlx_pool_router::PoolProvider;

use crate::AppState;
use crate::api::models::realtime_inflight_limits::RealtimeInflightLimitsResponse;
use crate::auth::permissions::{RequiresPermission, operation, resource};
use crate::db::handlers::Deployments;
use crate::db::handlers::realtime_inflight_limits::RealtimeInflightLimits;
use crate::db::handlers::repository::Repository;
use crate::errors::{Error, Result};
use crate::types::DeploymentId;

#[utoipa::path(
    get,
    path = "/models/{id}/realtime-inflight-limits",
    tag = "models",
    summary = "A virtual model's realtime in-flight default and the per-account limits set in account limit files",
    params(("id" = uuid::Uuid, Path, description = "Virtual model ID")),
    responses(
        (status = 200, description = "The default and every per-account limit", body = RealtimeInflightLimitsResponse),
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
    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let default_limit = match Deployments::new(&mut conn).get_by_id(id).await? {
        Some(model) if model.is_composite && !model.deleted => model.realtime_inflight_limit,
        _ => {
            return Err(Error::NotFound {
                resource: "Virtual model".to_string(),
                id: id.to_string(),
            });
        }
    };
    let overrides = RealtimeInflightLimits::new(&mut conn).list(id).await?;
    Ok(Json(RealtimeInflightLimitsResponse {
        deployed_model_id: id,
        default_limit,
        overrides: overrides.into_iter().map(Into::into).collect(),
    }))
}

#[cfg(test)]
mod tests {
    use crate::api::models::deployments::DeployedModelResponse;
    use crate::api::models::realtime_inflight_limits::RealtimeInflightLimitsResponse;
    use crate::api::models::users::{Role, UserResponse};
    use crate::test::utils::{
        add_auth_headers, create_test_admin_user, create_test_app, create_test_deployment, create_test_org, create_test_user,
    };
    use axum_test::{TestResponse, TestServer};
    use serde_json::json;
    use sqlx::PgPool;

    async fn get(server: &TestServer, path: &str, user: &UserResponse) -> TestResponse {
        let headers = add_auth_headers(user);
        server
            .get(path)
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await
    }

    async fn create_virtual_model(server: &TestServer, manager: &UserResponse, alias: &str) -> DeployedModelResponse {
        let headers = add_auth_headers(manager);
        let response = server
            .post("/admin/api/v1/models")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .json(&json!({ "type": "composite", "model_name": alias, "alias": alias }))
            .await;
        response.assert_status_ok();
        response.json()
    }

    #[dwctl_test_macros::test]
    #[test_log::test]
    async fn platform_managers_see_the_default_and_every_account_limit(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        let org = create_test_org(&pool, manager.id).await;
        let model = create_virtual_model(&server, &manager, "inflight/virtual").await;
        sqlx::query("INSERT INTO realtime_inflight_limit_overrides (deployed_model_id, user_id, inflight_limit) VALUES ($1, $2, 250)")
            .bind(model.id)
            .bind(org.id)
            .execute(&pool)
            .await
            .unwrap();

        let listed: RealtimeInflightLimitsResponse = get(
            &server,
            &format!("/admin/api/v1/models/{}/realtime-inflight-limits", model.id),
            &manager,
        )
        .await
        .json();
        assert_eq!(listed.default_limit, 14);
        assert_eq!(listed.overrides.len(), 1);
        assert_eq!(listed.overrides[0].account_id, org.id);
        assert_eq!(listed.overrides[0].limit, 250);

        let customer = create_test_user(&pool, Role::StandardUser).await;
        get(
            &server,
            &format!("/admin/api/v1/models/{}/realtime-inflight-limits", model.id),
            &customer,
        )
        .await
        .assert_status_forbidden();

        let standard_model = create_test_deployment(&pool, manager.id, "standard-model", "standard-model").await;
        get(
            &server,
            &format!("/admin/api/v1/models/{}/realtime-inflight-limits", standard_model.id),
            &manager,
        )
        .await
        .assert_status_not_found();
    }
}
