//! Platform-manager reads of serving deals: an organisation's settings, overlays and
//! prices in one response, and the organisations with an overlay on a model. Customers
//! never see any of this; their only surface is the model suffix.

use axum::Json;
use axum::extract::{Path, State};
use sqlx_pool_router::PoolProvider;

use crate::AppState;
use crate::api::models::serving::{OrganizationCacheTariffResponse, OrganizationServingResponse, OverlayResponse, OverlayRow};
use crate::api::models::tariffs::TariffResponse;
use crate::auth::permissions::{RequiresPermission, operation, resource};
use crate::db::handlers::repository::Repository;
use crate::db::handlers::{Tariffs, Users};
use crate::errors::{Error, Result};
use crate::types::{DeploymentId, UserId};

const OVERLAY_COLUMNS: &str = r#"
    SELECT mo.user_id AS organization_id, u.username AS organization_name,
           mo.deployed_model_id, dm.alias, mo.default_serving_class, mo.targets,
           mo.self_hosted_only, mo.provisioning_source, mo.updated_at
    FROM model_overlays mo
    JOIN users u ON u.id = mo.user_id
    JOIN deployed_models dm ON dm.id = mo.deployed_model_id
    WHERE dm.deleted = FALSE AND u.is_deleted = FALSE"#;

#[utoipa::path(
    get,
    path = "/organizations/{id}/serving",
    tag = "organizations",
    summary = "An organisation's serving settings, overlays and prices (platform managers)",
    params(("id" = uuid::Uuid, Path, description = "Organisation ID")),
    responses(
        (status = 200, description = "The organisation's serving deal", body = OrganizationServingResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden (platform managers only)"),
        (status = 404, description = "Organisation not found"),
    ),
    security(("BearerAuth" = []), ("CookieAuth" = []), ("X-Doubleword-User" = []))
)]
#[tracing::instrument(skip_all)]
pub async fn get_organization_serving<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(id): Path<UserId>,
    _user: RequiresPermission<resource::Organizations, operation::ReadAll>,
) -> Result<Json<OrganizationServingResponse>> {
    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let org = Users::new(&mut conn).get_by_id(id).await?.ok_or_else(|| Error::NotFound {
        resource: "Organization".to_string(),
        id: id.to_string(),
    })?;
    if org.user_type != "organization" {
        return Err(Error::NotFound {
            resource: "Organization".to_string(),
            id: id.to_string(),
        });
    }

    let overlays: Vec<OverlayRow> = sqlx::query_as(&format!("{OVERLAY_COLUMNS} AND mo.user_id = $1 ORDER BY dm.alias"))
        .bind(id)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| Error::Database(e.into()))?;

    let tariffs = Tariffs::new(&mut conn)
        .list_current_by_account(id)
        .await?
        .into_iter()
        .map(TariffResponse::from)
        .collect();

    #[derive(sqlx::FromRow)]
    struct CacheRow {
        deployed_model_id: DeploymentId,
        alias: String,
        write_multiplier_5m: rust_decimal::Decimal,
        write_multiplier_1h: rust_decimal::Decimal,
        write_multiplier_24h: rust_decimal::Decimal,
        read_multiplier: rust_decimal::Decimal,
        min_prefix_tokens: i32,
        valid_from: chrono::DateTime<chrono::Utc>,
    }
    let cache_rows: Vec<CacheRow> = sqlx::query_as(
        r#"SELECT mct.deployed_model_id, dm.alias, mct.write_multiplier_5m, mct.write_multiplier_1h,
                  mct.write_multiplier_24h, mct.read_multiplier, mct.min_prefix_tokens, mct.valid_from
           FROM model_cache_tariffs mct
           JOIN deployed_models dm ON dm.id = mct.deployed_model_id
           WHERE mct.user_id = $1 AND mct.valid_until IS NULL AND dm.deleted = FALSE
           ORDER BY dm.alias"#,
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| Error::Database(e.into()))?;

    Ok(Json(OrganizationServingResponse {
        organization_id: id,
        granted_serving_classes: org.granted_serving_classes,
        default_serving_class: org.default_serving_class,
        self_hosted_only: org.self_hosted_only,
        overlays: overlays.into_iter().map(OverlayResponse::from).collect(),
        tariffs,
        cache_tariffs: cache_rows
            .into_iter()
            .map(|r| OrganizationCacheTariffResponse {
                deployed_model_id: r.deployed_model_id,
                alias: r.alias,
                write_multiplier_5m: r.write_multiplier_5m,
                write_multiplier_1h: r.write_multiplier_1h,
                write_multiplier_24h: r.write_multiplier_24h,
                read_multiplier: r.read_multiplier,
                min_prefix_tokens: r.min_prefix_tokens,
                valid_from: r.valid_from,
            })
            .collect(),
    }))
}

#[utoipa::path(
    get,
    path = "/models/{id}/overlays",
    tag = "models",
    summary = "Organisations with a serving overlay on a model (platform managers)",
    params(("id" = uuid::Uuid, Path, description = "Deployment ID")),
    responses(
        (status = 200, description = "One entry per organisation with an overlay on the model", body = Vec<OverlayResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden (platform managers only)"),
        (status = 404, description = "Model not found"),
    ),
    security(("BearerAuth" = []), ("CookieAuth" = []), ("X-Doubleword-User" = []))
)]
#[tracing::instrument(skip_all)]
pub async fn list_model_overlays<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(id): Path<DeploymentId>,
    _user: RequiresPermission<resource::Models, operation::ReadAll>,
) -> Result<Json<Vec<OverlayResponse>>> {
    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM deployed_models WHERE id = $1 AND deleted = FALSE)")
        .bind(id)
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| Error::Database(e.into()))?;
    if !exists {
        return Err(Error::NotFound {
            resource: "Model".to_string(),
            id: id.to_string(),
        });
    }
    let overlays: Vec<OverlayRow> = sqlx::query_as(&format!("{OVERLAY_COLUMNS} AND mo.deployed_model_id = $1 ORDER BY u.username"))
        .bind(id)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| Error::Database(e.into()))?;
    Ok(Json(overlays.into_iter().map(OverlayResponse::from).collect()))
}

#[cfg(test)]
mod tests {
    use crate::api::models::users::Role;
    use crate::test::utils::{
        add_auth_headers, create_test_admin_user, create_test_app, create_test_endpoint, create_test_model, create_test_org,
        create_test_user,
    };
    use serde_json::{Value, json};
    use sqlx::PgPool;

    async fn get(server: &axum_test::TestServer, path: &str, user: &crate::api::models::users::UserResponse) -> axum_test::TestResponse {
        let headers = add_auth_headers(user);
        server
            .get(path)
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await
    }

    #[sqlx::test]
    #[test_log::test]
    async fn customers_see_their_effective_price_and_operators_see_every_scope(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
        let deal_holder = create_test_user(&pool, Role::StandardUser).await;
        let other = create_test_user(&pool, Role::StandardUser).await;
        let endpoint = create_test_endpoint(&pool, "ep", admin.id).await;
        let model_id = create_test_model(&pool, "m", "priced/model", endpoint, admin.id).await;
        // Everyone can see the model; only the deal holder has their own price on it.
        sqlx::query(
            "INSERT INTO deployment_groups (deployment_id, group_id, granted_by) VALUES ($1, '00000000-0000-0000-0000-000000000000', $2)",
        )
        .bind(model_id)
        .bind(admin.id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO model_tariffs (deployed_model_id, name, input_price_per_token, output_price_per_token, api_key_purpose, completion_window, user_id)
             VALUES ($1, 'general', 0.00000100, 0.00000200, 'realtime', NULL, NULL),
                    ($1, 'general-24h', 0.00000050, 0.00000100, 'batch', '24h', NULL),
                    ($1, 'deal', 0.00000010, 0.00000020, 'realtime', NULL, $2)",
        )
        .bind(model_id)
        .bind(deal_holder.id)
        .execute(&pool)
        .await
        .unwrap();

        let path = format!("/admin/api/v1/models/{model_id}?include=pricing");
        let tariffs = |body: Value| -> Vec<(String, Option<String>)> {
            body["tariffs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| {
                    (
                        t["name"].as_str().unwrap().to_string(),
                        t["organization_id"].as_str().map(str::to_string),
                    )
                })
                .collect()
        };

        // The deal holder pays their own realtime price and the general batch price.
        let resp = get(&server, &path, &deal_holder).await;
        resp.assert_status_ok();
        let mut seen = tariffs(resp.json());
        seen.sort();
        assert_eq!(
            seen,
            vec![
                ("deal".to_string(), Some(deal_holder.id.to_string())),
                ("general-24h".to_string(), None)
            ]
        );

        // Anyone else pays the general prices and never sees the deal.
        let resp = get(&server, &path, &other).await;
        resp.assert_status_ok();
        let mut seen = tariffs(resp.json());
        seen.sort();
        assert_eq!(seen, vec![("general".to_string(), None), ("general-24h".to_string(), None)]);

        // A platform manager sees every scope, organisation rows marked.
        let resp = get(&server, &path, &admin).await;
        resp.assert_status_ok();
        let mut seen = tariffs(resp.json());
        seen.sort();
        assert_eq!(
            seen,
            vec![
                ("deal".to_string(), Some(deal_holder.id.to_string())),
                ("general".to_string(), None),
                ("general-24h".to_string(), None)
            ]
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn serving_views_are_platform_manager_only(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
        let member = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, member.id).await;
        let endpoint = create_test_endpoint(&pool, "ep", admin.id).await;
        let model_id = create_test_model(&pool, "m", "overlaid/model", endpoint, admin.id).await;
        sqlx::query(
            "INSERT INTO model_overlays (user_id, deployed_model_id, default_serving_class, provisioning_source) VALUES ($1, $2, 'throughput', 'org-overlays:acme.yaml')",
        )
        .bind(org.id)
        .bind(model_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO model_tariffs (deployed_model_id, name, input_price_per_token, output_price_per_token, api_key_purpose, user_id) VALUES ($1, 'deal', 0.00000010, 0.00000020, 'realtime', $2)",
        )
        .bind(model_id)
        .bind(org.id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO model_cache_tariffs (deployed_model_id, write_multiplier_5m, write_multiplier_1h, write_multiplier_24h, read_multiplier, min_prefix_tokens, user_id) VALUES ($1, 1.1, 1.5, 2.0, 0.05, 512, $2)",
        )
        .bind(model_id)
        .bind(org.id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE users SET granted_serving_classes = '{throughput}', self_hosted_only = TRUE WHERE id = $1")
            .bind(org.id)
            .execute(&pool)
            .await
            .unwrap();

        // The organisation's owner is not an operator: neither view is theirs.
        get(&server, &format!("/admin/api/v1/organizations/{}/serving", org.id), &member)
            .await
            .assert_status_forbidden();
        get(&server, &format!("/admin/api/v1/models/{model_id}/overlays"), &member)
            .await
            .assert_status_forbidden();

        let resp = get(&server, &format!("/admin/api/v1/organizations/{}/serving", org.id), &admin).await;
        resp.assert_status_ok();
        let body: Value = resp.json();
        assert_eq!(body["granted_serving_classes"], json!(["throughput"]));
        assert_eq!(body["self_hosted_only"], json!(true));
        assert_eq!(body["overlays"].as_array().unwrap().len(), 1);
        assert_eq!(body["overlays"][0]["alias"], "overlaid/model");
        assert_eq!(body["overlays"][0]["default_serving_class"], "throughput");
        assert_eq!(body["overlays"][0]["provisioning_source"], "org-overlays:acme.yaml");
        assert_eq!(body["tariffs"].as_array().unwrap().len(), 1);
        assert_eq!(body["tariffs"][0]["organization_id"], org.id.to_string());
        assert_eq!(body["cache_tariffs"].as_array().unwrap().len(), 1);
        assert_eq!(body["cache_tariffs"][0]["min_prefix_tokens"], 512);

        let resp = get(&server, &format!("/admin/api/v1/models/{model_id}/overlays"), &admin).await;
        resp.assert_status_ok();
        let body: Value = resp.json();
        assert_eq!(body.as_array().unwrap().len(), 1);
        assert_eq!(body[0]["organization_id"], org.id.to_string());
        assert_eq!(body[0]["organization_name"], org.username);

        // A personal account is not an organisation.
        get(&server, &format!("/admin/api/v1/organizations/{}/serving", member.id), &admin)
            .await
            .assert_status_not_found();
    }
}
