use sqlx_pool_router::PoolProvider;

use crate::AppState;
use crate::api::models::provider_display_configs::{
    CreateProviderDisplayConfig, ProviderDisplayConfigResponse, UpdateProviderDisplayConfig,
};
use crate::auth::permissions::{RequiresPermission, can_read_all_resources, operation, resource};
use crate::db::handlers::ProviderDisplayConfigs;
use crate::db::models::provider_display_configs::{ProviderDisplayConfigCreateDBRequest, ProviderDisplayConfigUpdateDBRequest};
use crate::errors::{Error, Result};
use crate::types::Resource;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use std::collections::{BTreeMap, HashMap};

fn normalize_provider_key(value: &str) -> String {
    value.trim().to_lowercase()
}

fn validate_provider_key(provider_key: &str) -> Result<()> {
    if provider_key.trim().is_empty() {
        return Err(Error::BadRequest {
            message: "provider_key must not be empty".to_string(),
        });
    }
    Ok(())
}

fn validate_icon(icon: Option<&str>) -> Result<()> {
    let Some(icon) = icon.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(());
    };

    let is_builtin = matches!(icon, "anthropic" | "google" | "openai" | "onwards" | "snowflake");
    let is_url = icon.starts_with("https://") || icon.starts_with("/");
    if !is_builtin && !is_url {
        return Err(Error::BadRequest {
            message: "icon must be an https URL, root-relative asset path, or built-in icon key".to_string(),
        });
    }
    Ok(())
}

#[utoipa::path(
    get,
    path = "/provider-display-configs",
    tag = "provider-display-configs",
    summary = "List provider display configs",
    responses(
        (status = 200, description = "Provider display configs", body = Vec<ProviderDisplayConfigResponse>)
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
pub async fn list_provider_display_configs<P: PoolProvider>(
    State(state): State<AppState<P>>,
    current_user: RequiresPermission<resource::Models, operation::ReadOwn>,
) -> Result<Json<Vec<ProviderDisplayConfigResponse>>> {
    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ProviderDisplayConfigs::new(&mut conn);

    let can_read_all = can_read_all_resources(&current_user, Resource::Models);
    let known = if can_read_all {
        repo.list_known_providers().await?
    } else {
        let target_user_id = current_user.active_organization.unwrap_or(current_user.id);
        repo.list_known_providers_for_user(target_user_id).await?
    };

    // For standard users, only return configs that have accessible models
    let configs = if can_read_all {
        repo.list().await?
    } else {
        let known_keys: std::collections::HashSet<_> = known.iter().map(|k| k.provider_key.clone()).collect();
        repo.list()
            .await?
            .into_iter()
            .filter(|c| known_keys.contains(&c.provider_key))
            .collect()
    };

    let config_map: HashMap<_, _> = configs.into_iter().map(|config| (config.provider_key.clone(), config)).collect();
    let known_map: HashMap<_, _> = known
        .into_iter()
        .map(|provider| (provider.provider_key.clone(), provider))
        .collect();

    let mut keys = BTreeMap::new();
    for key in config_map.keys() {
        keys.insert(key.clone(), ());
    }
    for key in known_map.keys() {
        keys.insert(key.clone(), ());
    }

    let mut response = Vec::new();
    for key in keys.into_keys() {
        response.push(ProviderDisplayConfigResponse::from_parts(
            config_map.get(&key).cloned(),
            known_map.get(&key).cloned(),
        ));
    }

    response.sort_by(|a, b| a.display_name.cmp(&b.display_name));

    Ok(Json(response))
}

#[utoipa::path(
    get,
    path = "/provider-display-configs/{provider_key}",
    tag = "provider-display-configs",
    summary = "Get provider display config",
    params(
        ("provider_key" = String, Path)
    ),
    responses(
        (status = 200, description = "Provider display config", body = ProviderDisplayConfigResponse),
        (status = 404, description = "Provider display config not found")
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
pub async fn get_provider_display_config<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(provider_key): Path<String>,
    _: RequiresPermission<resource::Models, operation::ReadOwn>,
) -> Result<Json<ProviderDisplayConfigResponse>> {
    let provider_key = normalize_provider_key(&provider_key);
    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ProviderDisplayConfigs::new(&mut conn);
    let config = repo.get_by_key(&provider_key).await?;
    let known = repo
        .list_known_providers()
        .await?
        .into_iter()
        .find(|provider| provider.provider_key == provider_key);

    match (config, known) {
        (None, None) => Err(Error::NotFound {
            resource: "provider display config".to_string(),
            id: provider_key,
        }),
        (config, known) => Ok(Json(ProviderDisplayConfigResponse::from_parts(config, known))),
    }
}

#[utoipa::path(
    post,
    path = "/provider-display-configs",
    tag = "provider-display-configs",
    summary = "Create provider display config",
    request_body = CreateProviderDisplayConfig,
    responses(
        (status = 201, description = "Provider display config created", body = ProviderDisplayConfigResponse)
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
pub async fn create_provider_display_config<P: PoolProvider>(
    State(state): State<AppState<P>>,
    current_user: RequiresPermission<resource::Models, operation::UpdateAll>,
    Json(create): Json<CreateProviderDisplayConfig>,
) -> Result<(StatusCode, Json<ProviderDisplayConfigResponse>)> {
    let provider_key = normalize_provider_key(&create.provider_key);
    validate_provider_key(&provider_key)?;
    validate_icon(create.icon.as_deref())?;

    let display_name = create
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(create.provider_key.trim())
        .to_string();

    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ProviderDisplayConfigs::new(&mut conn);
    let config = repo
        .create(&ProviderDisplayConfigCreateDBRequest {
            provider_key: provider_key.clone(),
            display_name,
            icon: create.icon.filter(|value| !value.trim().is_empty()),
            created_by: current_user.id,
        })
        .await?;

    let known = repo
        .list_known_providers()
        .await?
        .into_iter()
        .find(|provider| provider.provider_key == provider_key);

    Ok((
        StatusCode::CREATED,
        Json(ProviderDisplayConfigResponse::from_parts(Some(config), known)),
    ))
}

#[utoipa::path(
    patch,
    path = "/provider-display-configs/{provider_key}",
    tag = "provider-display-configs",
    summary = "Update provider display config",
    request_body = UpdateProviderDisplayConfig,
    params(
        ("provider_key" = String, Path)
    ),
    responses(
        (status = 200, description = "Provider display config updated", body = ProviderDisplayConfigResponse)
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
pub async fn update_provider_display_config<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(provider_key): Path<String>,
    _: RequiresPermission<resource::Models, operation::UpdateAll>,
    Json(update): Json<UpdateProviderDisplayConfig>,
) -> Result<Json<ProviderDisplayConfigResponse>> {
    let provider_key = normalize_provider_key(&provider_key);
    validate_provider_key(&provider_key)?;
    validate_icon(update.icon.as_ref().and_then(|value| value.as_deref()))?;

    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ProviderDisplayConfigs::new(&mut conn);
    let config = repo
        .update(
            &provider_key,
            &ProviderDisplayConfigUpdateDBRequest {
                display_name: update.display_name.and_then(|value| {
                    let trimmed = value.trim().to_string();
                    (!trimmed.is_empty()).then_some(trimmed)
                }),
                icon: update.icon.map(|value| {
                    value.and_then(|icon| {
                        let trimmed = icon.trim().to_string();
                        (!trimmed.is_empty()).then_some(trimmed)
                    })
                }),
            },
        )
        .await?;

    let known = repo
        .list_known_providers()
        .await?
        .into_iter()
        .find(|provider| provider.provider_key == provider_key);

    Ok(Json(ProviderDisplayConfigResponse::from_parts(Some(config), known)))
}

#[utoipa::path(
    delete,
    path = "/provider-display-configs/{provider_key}",
    tag = "provider-display-configs",
    summary = "Delete provider display config",
    params(
        ("provider_key" = String, Path)
    ),
    responses(
        (status = 204, description = "Provider display config deleted")
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
pub async fn delete_provider_display_config<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(provider_key): Path<String>,
    _: RequiresPermission<resource::Models, operation::UpdateAll>,
) -> Result<StatusCode> {
    let provider_key = normalize_provider_key(&provider_key);
    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ProviderDisplayConfigs::new(&mut conn);
    let deleted = repo.delete(&provider_key).await?;
    if !deleted {
        return Err(Error::NotFound {
            resource: "provider display config".to_string(),
            id: provider_key,
        });
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use crate::api::models::provider_display_configs::ProviderDisplayConfigResponse;
    use crate::api::models::users::Role;
    use crate::test::utils::*;
    use sqlx::PgPool;

    /// Helper: create a deployed model with provider metadata and return its ID
    async fn create_model_with_provider(pool: &PgPool, alias: &str, provider: &str, created_by: uuid::Uuid) -> uuid::Uuid {
        let endpoint_id = get_test_endpoint_id(pool).await;
        let deployment_id = uuid::Uuid::new_v4();
        sqlx::query!(
            r#"
            INSERT INTO deployed_models (id, model_name, alias, hosted_on, created_by, deleted, metadata)
            VALUES ($1, $2, $3, $4, $5, false, $6)
            "#,
            deployment_id,
            alias,
            alias,
            endpoint_id,
            created_by,
            serde_json::json!({ "provider": provider }),
        )
        .execute(pool)
        .await
        .expect("Failed to create test model with provider");
        deployment_id
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_standard_user_only_sees_accessible_providers(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create models for two different providers
        let anthropic_model = create_model_with_provider(&pool, "claude-3", "Anthropic", admin.id).await;
        let _openai_model = create_model_with_provider(&pool, "gpt-4", "OpenAI", admin.id).await;

        // Create a group and add only the Anthropic model
        let group = create_test_group(&pool).await;
        add_deployment_to_group(&pool, anthropic_model, group.id, admin.id).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Standard user should only see the Anthropic provider
        let headers = add_auth_headers(&user);
        let response = app
            .get("/admin/api/v1/provider-display-configs")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await;

        response.assert_status_ok();
        let providers: Vec<ProviderDisplayConfigResponse> = response.json();
        let provider_keys: Vec<&str> = providers.iter().map(|p| p.provider_key.as_str()).collect();
        assert!(provider_keys.contains(&"anthropic"), "Should see accessible provider");
        assert!(!provider_keys.contains(&"openai"), "Should NOT see inaccessible provider");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_admin_sees_all_providers(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Create models for two different providers (not assigned to any group)
        create_model_with_provider(&pool, "claude-3", "Anthropic", admin.id).await;
        create_model_with_provider(&pool, "gpt-4", "OpenAI", admin.id).await;

        // Admin should see both providers regardless of group membership
        let headers = add_auth_headers(&admin);
        let response = app
            .get("/admin/api/v1/provider-display-configs")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await;

        response.assert_status_ok();
        let providers: Vec<ProviderDisplayConfigResponse> = response.json();
        let provider_keys: Vec<&str> = providers.iter().map(|p| p.provider_key.as_str()).collect();
        assert!(provider_keys.contains(&"anthropic"), "Admin should see Anthropic");
        assert!(provider_keys.contains(&"openai"), "Admin should see OpenAI");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_standard_user_sees_everyone_group_providers(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create a model and add it to the Everyone group (nil UUID)
        let model_id = create_model_with_provider(&pool, "gemini-pro", "Google", admin.id).await;
        let everyone_group_id = uuid::Uuid::nil();
        add_deployment_to_group(&pool, model_id, everyone_group_id, admin.id).await;

        // Standard user should see the provider via the Everyone group
        let headers = add_auth_headers(&user);
        let response = app
            .get("/admin/api/v1/provider-display-configs")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await;

        response.assert_status_ok();
        let providers: Vec<ProviderDisplayConfigResponse> = response.json();
        let provider_keys: Vec<&str> = providers.iter().map(|p| p.provider_key.as_str()).collect();
        assert!(provider_keys.contains(&"google"), "Should see provider from Everyone group");
    }
}
