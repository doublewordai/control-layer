use sqlx_pool_router::PoolProvider;

use crate::AppState;
use crate::api::models::provider_display_configs::{
    CreateProviderDisplayConfig, ProviderDisplayConfigResponse, UpdateProviderDisplayConfig,
};
use crate::auth::permissions::{RequiresPermission, operation, resource};
use crate::db::handlers::ProviderDisplayConfigs;
use crate::db::models::provider_display_configs::{ProviderDisplayConfigCreateDBRequest, ProviderDisplayConfigUpdateDBRequest};
use crate::errors::{Error, Result};
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
    _: RequiresPermission<resource::Models, operation::ReadOwn>,
) -> Result<Json<Vec<ProviderDisplayConfigResponse>>> {
    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ProviderDisplayConfigs::new(&mut conn);
    let configs = repo.list().await?;
    let known = repo.list_known_providers().await?;

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
