//! HTTP handlers for composite model endpoints.
//!
//! Composite models are virtual models that distribute requests across multiple
//! underlying deployed models based on configurable weights.

use crate::{
    AppState,
    api::models::{
        composite_models::{
            CompositeModelComponentDefinition, CompositeModelComponentResponse, CompositeModelComponentUpdate, CompositeModelCreate,
            CompositeModelResponse, CompositeModelUpdate, GetCompositeModelQuery, ListCompositeModelsQuery,
        },
        groups::GroupResponse,
        pagination::PaginatedResponse,
        users::CurrentUser,
    },
    auth::permissions::{RequiresPermission, can_read_all_resources, operation, resource},
    db::{
        handlers::{CompositeModels, Deployments, Groups, Repository, composite_models::CompositeModelFilter},
        models::composite_models::{
            CompositeModelComponentCreateDBRequest, CompositeModelCreateDBRequest, CompositeModelGroupCreateDBRequest,
            CompositeModelUpdateDBRequest,
        },
    },
    errors::{Error, Result},
    types::{CompositeModelId, DeploymentId, GroupId, Resource},
};
use axum::{
    extract::{Path, Query, State},
    response::Json,
};
use sqlx::Acquire;
use std::collections::HashMap;

/// Enrich composite model responses with components and groups
async fn enrich_composite_models(
    state: &AppState,
    models: Vec<CompositeModelResponse>,
    include_components: bool,
    include_groups: bool,
    can_read_rate_limits: bool,
    can_read_users: bool,
) -> Result<Vec<CompositeModelResponse>> {
    if models.is_empty() {
        return Ok(models);
    }

    let model_ids: Vec<CompositeModelId> = models.iter().map(|m| m.id).collect();

    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;

    // Fetch components if requested
    let components_map: HashMap<CompositeModelId, Vec<CompositeModelComponentResponse>> = if include_components {
        let mut composite_repo = CompositeModels::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
        let raw_components = composite_repo.get_components_bulk(&model_ids).await?;

        // Get deployment aliases for enrichment
        let deployment_ids: Vec<DeploymentId> = raw_components.values().flatten().map(|c| c.deployed_model_id).collect();

        let mut deployments_repo = Deployments::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
        let deployments = deployments_repo.get_bulk(deployment_ids).await?;

        raw_components
            .into_iter()
            .map(|(composite_id, components)| {
                let enriched: Vec<CompositeModelComponentResponse> = components
                    .into_iter()
                    .map(|c| CompositeModelComponentResponse {
                        deployed_model_id: c.deployed_model_id,
                        deployed_model_alias: deployments.get(&c.deployed_model_id).map(|d| d.alias.clone()),
                        weight: c.weight,
                        enabled: c.enabled,
                    })
                    .collect();
                (composite_id, enriched)
            })
            .collect()
    } else {
        HashMap::new()
    };

    // Fetch groups if requested
    let groups_map: HashMap<CompositeModelId, Vec<GroupResponse>> = if include_groups {
        let mut composite_repo = CompositeModels::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
        let raw_groups = composite_repo.get_groups_bulk(&model_ids).await?;

        // Get group details
        let group_ids: Vec<GroupId> = raw_groups.values().flatten().copied().collect();

        let mut groups_repo = Groups::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
        let groups = groups_repo.get_bulk(group_ids).await?;

        raw_groups
            .into_iter()
            .map(|(composite_id, group_ids)| {
                let enriched: Vec<GroupResponse> = group_ids
                    .into_iter()
                    .filter_map(|gid| groups.get(&gid).map(|g| GroupResponse::from(g.clone())))
                    .collect();
                (composite_id, enriched)
            })
            .collect()
    } else {
        HashMap::new()
    };

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    // Apply enrichments and masking
    let enriched_models: Vec<CompositeModelResponse> = models
        .into_iter()
        .map(|mut model| {
            if let Some(components) = components_map.get(&model.id) {
                model = model.with_components(components.clone());
            }
            if let Some(groups) = groups_map.get(&model.id) {
                model = model.with_groups(groups.clone());
            }
            if !can_read_rate_limits {
                model = model.mask_rate_limiting().mask_capacity();
            }
            if !can_read_users {
                model = model.mask_created_by();
            }
            model
        })
        .collect();

    Ok(enriched_models)
}

#[utoipa::path(
    get,
    path = "/composite-models",
    tag = "composite-models",
    summary = "List composite models",
    description = "List all composite models",
    params(
        ("accessible" = Option<bool>, Query, description = "Filter to only models the current user can access"),
        ("include" = Option<String>, Query, description = "Include additional data (comma-separated: 'groups', 'components')"),
        ("search" = Option<String>, Query, description = "Search query to filter by alias or description"),
        ("limit" = Option<i64>, Query, description = "Maximum number of items to return (default: 10, max: 100)"),
        ("skip" = Option<i64>, Query, description = "Number of items to skip (default: 0)"),
    ),
    responses(
        (status = 200, description = "Paginated list of composite models", body = PaginatedResponse<CompositeModelResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn list_composite_models(
    State(state): State<AppState>,
    Query(query): Query<ListCompositeModelsQuery>,
    current_user: CurrentUser,
) -> Result<Json<PaginatedResponse<CompositeModelResponse>>> {
    let can_read_all = can_read_all_resources(&current_user, Resource::CompositeModels);
    let can_read_groups = can_read_all_resources(&current_user, Resource::Groups);
    let can_read_rate_limits = can_read_all_resources(&current_user, Resource::ModelRateLimits);
    let can_read_users = can_read_all_resources(&current_user, Resource::Users);

    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = CompositeModels::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);

    let (skip, limit) = query.pagination.params();
    let mut filter = CompositeModelFilter::new(skip, limit);

    // Apply accessibility filtering
    if !can_read_all || query.accessible.unwrap_or(false) {
        filter = filter.with_accessible_to(current_user.id);
    }

    // Apply search filter
    if let Some(search) = query.search.as_ref()
        && !search.trim().is_empty()
    {
        filter = filter.with_search(search.trim().to_string());
    }

    // Parse includes
    let all_includes: Vec<&str> = query
        .include
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    let include_components = all_includes.contains(&"components");
    let include_groups = all_includes.contains(&"groups") && can_read_groups;

    let total_count = repo.count(&filter).await?;
    let models = repo.list(&filter).await?;

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    let responses: Vec<CompositeModelResponse> = models.into_iter().map(CompositeModelResponse::from).collect();

    let enriched = enrich_composite_models(
        &state,
        responses,
        include_components,
        include_groups,
        can_read_rate_limits,
        can_read_users,
    )
    .await?;

    Ok(Json(PaginatedResponse::new(enriched, total_count, skip, limit)))
}

#[utoipa::path(
    get,
    path = "/composite-models/{id}",
    tag = "composite-models",
    summary = "Get a composite model",
    description = "Get a composite model by ID",
    params(
        ("id" = uuid::Uuid, Path, description = "Composite model ID"),
        ("include" = Option<String>, Query, description = "Include additional data (comma-separated: 'groups', 'components')"),
    ),
    responses(
        (status = 200, description = "Composite model", body = CompositeModelResponse),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Composite model not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_composite_model(
    State(state): State<AppState>,
    Path(id): Path<CompositeModelId>,
    Query(query): Query<GetCompositeModelQuery>,
    current_user: CurrentUser,
) -> Result<Json<CompositeModelResponse>> {
    let can_read_groups = can_read_all_resources(&current_user, Resource::Groups);
    let can_read_rate_limits = can_read_all_resources(&current_user, Resource::ModelRateLimits);
    let can_read_users = can_read_all_resources(&current_user, Resource::Users);

    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = CompositeModels::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);

    let model = repo.get_by_id(id).await?.ok_or_else(|| Error::NotFound {
        resource: "composite-model".to_string(),
        id: id.to_string(),
    })?;

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    // Parse includes
    let all_includes: Vec<&str> = query
        .include
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    let include_components = all_includes.contains(&"components");
    let include_groups = all_includes.contains(&"groups") && can_read_groups;

    let response = CompositeModelResponse::from(model);
    let enriched = enrich_composite_models(
        &state,
        vec![response],
        include_components,
        include_groups,
        can_read_rate_limits,
        can_read_users,
    )
    .await?;

    Ok(Json(enriched.into_iter().next().unwrap()))
}

#[utoipa::path(
    post,
    path = "/composite-models",
    tag = "composite-models",
    summary = "Create a composite model",
    description = "Create a new composite model. Admin only.",
    request_body = CompositeModelCreate,
    responses(
        (status = 201, description = "Composite model created", body = CompositeModelResponse),
        (status = 400, description = "Bad request - invalid data or duplicate alias"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn create_composite_model(
    State(state): State<AppState>,
    current_user: RequiresPermission<resource::CompositeModels, operation::CreateAll>,
    Json(create): Json<CompositeModelCreate>,
) -> Result<Json<CompositeModelResponse>> {
    let alias = create.alias.trim();
    if alias.is_empty() {
        return Err(Error::BadRequest {
            message: "Alias must not be empty".to_string(),
        });
    }

    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;

    // Create the composite model
    let mut repo = CompositeModels::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);

    // Extract fallback configuration
    let (fallback_enabled, fallback_on_rate_limit, fallback_on_status) = create
        .fallback
        .map_or((None, None, None), |f| (Some(f.enabled), Some(f.on_rate_limit), Some(f.on_status)));

    let db_request = CompositeModelCreateDBRequest::builder()
        .created_by(current_user.id)
        .alias(alias.to_string())
        .maybe_description(create.description)
        .maybe_model_type(create.model_type)
        .maybe_requests_per_second(create.requests_per_second)
        .maybe_burst_size(create.burst_size)
        .maybe_capacity(create.capacity)
        .maybe_batch_capacity(create.batch_capacity)
        .lb_strategy(create.lb_strategy)
        .maybe_fallback_enabled(fallback_enabled)
        .maybe_fallback_on_rate_limit(fallback_on_rate_limit)
        .maybe_fallback_on_status(fallback_on_status)
        .build();

    let model = repo.create(&db_request).await?;

    // Add components if provided
    if let Some(components) = create.components {
        for component in components {
            validate_component_weight(component.weight)?;
            let component_request = CompositeModelComponentCreateDBRequest {
                composite_model_id: model.id,
                deployed_model_id: component.deployed_model_id,
                weight: component.weight,
                enabled: component.enabled,
            };
            repo.add_component(&component_request).await?;
        }
    }

    // Add groups if provided
    if let Some(group_ids) = create.groups {
        for group_id in group_ids {
            let group_request = CompositeModelGroupCreateDBRequest {
                composite_model_id: model.id,
                group_id,
                granted_by: Some(current_user.id),
            };
            repo.add_group(&group_request).await?;
        }
    }

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(Json(CompositeModelResponse::from(model)))
}

#[utoipa::path(
    patch,
    path = "/composite-models/{id}",
    tag = "composite-models",
    summary = "Update a composite model",
    description = "Update a composite model. Admin only.",
    params(
        ("id" = uuid::Uuid, Path, description = "Composite model ID"),
    ),
    request_body = CompositeModelUpdate,
    responses(
        (status = 200, description = "Composite model updated", body = CompositeModelResponse),
        (status = 400, description = "Bad request - invalid data"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Composite model not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn update_composite_model(
    State(state): State<AppState>,
    Path(id): Path<CompositeModelId>,
    current_user: RequiresPermission<resource::CompositeModels, operation::UpdateAll>,
    Json(update): Json<CompositeModelUpdate>,
) -> Result<Json<CompositeModelResponse>> {
    if let Some(alias) = &update.alias
        && alias.trim().is_empty()
    {
        return Err(Error::BadRequest {
            message: "Alias must not be empty".to_string(),
        });
    }

    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = CompositeModels::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);

    // Check model exists
    if repo.get_by_id(id).await?.is_none() {
        return Err(Error::NotFound {
            resource: "composite-model".to_string(),
            id: id.to_string(),
        });
    }

    // Extract fallback configuration
    let (fallback_enabled, fallback_on_rate_limit, fallback_on_status) = update
        .fallback
        .map_or((None, None, None), |f| (Some(f.enabled), Some(f.on_rate_limit), Some(f.on_status)));

    // Update the model
    let db_request = CompositeModelUpdateDBRequest::builder()
        .maybe_alias(update.alias)
        .maybe_description(update.description)
        .maybe_model_type(update.model_type)
        .maybe_requests_per_second(update.requests_per_second)
        .maybe_burst_size(update.burst_size)
        .maybe_capacity(update.capacity)
        .maybe_batch_capacity(update.batch_capacity)
        .maybe_lb_strategy(update.lb_strategy)
        .maybe_fallback_enabled(fallback_enabled)
        .maybe_fallback_on_rate_limit(fallback_on_rate_limit)
        .maybe_fallback_on_status(fallback_on_status)
        .build();

    let model = repo.update(id, &db_request).await?;

    // Update components if provided (replaces all)
    if let Some(components) = update.components {
        let component_tuples: Vec<(DeploymentId, i32, bool)> = components
            .into_iter()
            .map(|c| {
                validate_component_weight(c.weight)?;
                Ok((c.deployed_model_id, c.weight, c.enabled))
            })
            .collect::<Result<Vec<_>>>()?;

        repo.set_components(id, component_tuples).await?;
    }

    // Update groups if provided (replaces all)
    if let Some(group_ids) = update.groups {
        repo.set_groups(id, group_ids, current_user.id).await?;
    }

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(Json(CompositeModelResponse::from(model)))
}

#[utoipa::path(
    delete,
    path = "/composite-models/{id}",
    tag = "composite-models",
    summary = "Delete a composite model",
    description = "Delete a composite model. Admin only.",
    params(
        ("id" = uuid::Uuid, Path, description = "Composite model ID"),
    ),
    responses(
        (status = 204, description = "Composite model deleted"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Composite model not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn delete_composite_model(
    State(state): State<AppState>,
    Path(id): Path<CompositeModelId>,
    _current_user: RequiresPermission<resource::CompositeModels, operation::DeleteAll>,
) -> Result<()> {
    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = CompositeModels::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);

    let deleted = repo.delete(id).await?;
    if !deleted {
        return Err(Error::NotFound {
            resource: "composite-model".to_string(),
            id: id.to_string(),
        });
    }

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(())
}

// ===== Component Management =====

#[utoipa::path(
    get,
    path = "/composite-models/{id}/components",
    tag = "composite-models",
    summary = "Get components of a composite model",
    description = "Get all components (underlying deployed models) of a composite model",
    params(
        ("id" = uuid::Uuid, Path, description = "Composite model ID"),
    ),
    responses(
        (status = 200, description = "List of components", body = Vec<CompositeModelComponentResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Composite model not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_composite_model_components(
    State(state): State<AppState>,
    Path(id): Path<CompositeModelId>,
    _current_user: CurrentUser,
) -> Result<Json<Vec<CompositeModelComponentResponse>>> {
    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = CompositeModels::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);

    // Verify model exists
    if repo.get_by_id(id).await?.is_none() {
        return Err(Error::NotFound {
            resource: "composite-model".to_string(),
            id: id.to_string(),
        });
    }

    let components = repo.get_components(id).await?;

    // Get deployment aliases for enrichment
    let deployment_ids: Vec<DeploymentId> = components.iter().map(|c| c.deployed_model_id).collect();

    let mut deployments_repo = Deployments::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
    let deployments = deployments_repo.get_bulk(deployment_ids).await?;

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    let response: Vec<CompositeModelComponentResponse> = components
        .into_iter()
        .map(|c| CompositeModelComponentResponse {
            deployed_model_id: c.deployed_model_id,
            deployed_model_alias: deployments.get(&c.deployed_model_id).map(|d| d.alias.clone()),
            weight: c.weight,
            enabled: c.enabled,
        })
        .collect();

    Ok(Json(response))
}

#[utoipa::path(
    post,
    path = "/composite-models/{id}/components/{deployment_id}",
    tag = "composite-models",
    summary = "Add a component to a composite model",
    description = "Add a deployed model as a component to a composite model. Admin only.",
    params(
        ("id" = uuid::Uuid, Path, description = "Composite model ID"),
        ("deployment_id" = uuid::Uuid, Path, description = "Deployed model ID to add as component"),
    ),
    request_body = CompositeModelComponentDefinition,
    responses(
        (status = 200, description = "Component added"),
        (status = 400, description = "Bad request - invalid weight"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Composite model or deployment not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn add_composite_model_component(
    State(state): State<AppState>,
    Path((id, deployment_id)): Path<(CompositeModelId, DeploymentId)>,
    _current_user: RequiresPermission<resource::CompositeModels, operation::UpdateAll>,
    Json(component): Json<CompositeModelComponentDefinition>,
) -> Result<()> {
    validate_component_weight(component.weight)?;

    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;

    // Verify composite model exists
    {
        let mut repo = CompositeModels::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
        if repo.get_by_id(id).await?.is_none() {
            return Err(Error::NotFound {
                resource: "composite-model".to_string(),
                id: id.to_string(),
            });
        }
    }

    // Verify deployment exists
    {
        let mut deployments_repo = Deployments::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
        if deployments_repo.get_by_id(deployment_id).await?.is_none() {
            return Err(Error::NotFound {
                resource: "deployment".to_string(),
                id: deployment_id.to_string(),
            });
        }
    }

    let request = CompositeModelComponentCreateDBRequest {
        composite_model_id: id,
        deployed_model_id: deployment_id,
        weight: component.weight,
        enabled: component.enabled,
    };

    {
        let mut repo = CompositeModels::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
        repo.add_component(&request).await?;
    }

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(())
}

#[utoipa::path(
    patch,
    path = "/composite-models/{id}/components/{deployment_id}",
    tag = "composite-models",
    summary = "Update a component of a composite model",
    description = "Update the weight or enabled status of a component. Admin only.",
    params(
        ("id" = uuid::Uuid, Path, description = "Composite model ID"),
        ("deployment_id" = uuid::Uuid, Path, description = "Deployed model ID"),
    ),
    request_body = CompositeModelComponentUpdate,
    responses(
        (status = 200, description = "Component updated"),
        (status = 400, description = "Bad request - invalid weight"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Composite model or component not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn update_composite_model_component(
    State(state): State<AppState>,
    Path((id, deployment_id)): Path<(CompositeModelId, DeploymentId)>,
    _current_user: RequiresPermission<resource::CompositeModels, operation::UpdateAll>,
    Json(update): Json<CompositeModelComponentUpdate>,
) -> Result<()> {
    if let Some(weight) = update.weight {
        validate_component_weight(weight)?;
    }

    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = CompositeModels::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);

    repo.update_component(id, deployment_id, update.weight, update.enabled).await?;

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(())
}

#[utoipa::path(
    delete,
    path = "/composite-models/{id}/components/{deployment_id}",
    tag = "composite-models",
    summary = "Remove a component from a composite model",
    description = "Remove a deployed model from a composite model. Admin only.",
    params(
        ("id" = uuid::Uuid, Path, description = "Composite model ID"),
        ("deployment_id" = uuid::Uuid, Path, description = "Deployed model ID to remove"),
    ),
    responses(
        (status = 204, description = "Component removed"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Component not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn remove_composite_model_component(
    State(state): State<AppState>,
    Path((id, deployment_id)): Path<(CompositeModelId, DeploymentId)>,
    _current_user: RequiresPermission<resource::CompositeModels, operation::UpdateAll>,
) -> Result<()> {
    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = CompositeModels::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);

    let removed = repo.remove_component(id, deployment_id).await?;
    if !removed {
        return Err(Error::NotFound {
            resource: "component".to_string(),
            id: format!("{}:{}", id, deployment_id),
        });
    }

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(())
}

// ===== Group Management =====

#[utoipa::path(
    get,
    path = "/composite-models/{id}/groups",
    tag = "composite-models",
    summary = "Get groups with access to a composite model",
    description = "Get all groups that have access to a composite model",
    params(
        ("id" = uuid::Uuid, Path, description = "Composite model ID"),
    ),
    responses(
        (status = 200, description = "List of groups", body = Vec<GroupResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Composite model not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_composite_model_groups(
    State(state): State<AppState>,
    Path(id): Path<CompositeModelId>,
    _current_user: RequiresPermission<resource::Groups, operation::ReadAll>,
) -> Result<Json<Vec<GroupResponse>>> {
    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;

    let mut composite_repo = CompositeModels::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
    if composite_repo.get_by_id(id).await?.is_none() {
        return Err(Error::NotFound {
            resource: "composite-model".to_string(),
            id: id.to_string(),
        });
    }

    let group_ids = composite_repo.get_groups(id).await?;

    let mut groups_repo = Groups::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
    let groups = groups_repo.get_bulk(group_ids).await?;

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    let response: Vec<GroupResponse> = groups.into_values().map(GroupResponse::from).collect();

    Ok(Json(response))
}

#[utoipa::path(
    post,
    path = "/composite-models/{id}/groups/{group_id}",
    tag = "composite-models",
    summary = "Add group access to a composite model",
    description = "Grant a group access to a composite model. Admin only.",
    params(
        ("id" = uuid::Uuid, Path, description = "Composite model ID"),
        ("group_id" = uuid::Uuid, Path, description = "Group ID to grant access"),
    ),
    responses(
        (status = 200, description = "Group access granted"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Composite model or group not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn add_composite_model_group(
    State(state): State<AppState>,
    Path((id, group_id)): Path<(CompositeModelId, GroupId)>,
    current_user: RequiresPermission<resource::CompositeModels, operation::UpdateAll>,
) -> Result<()> {
    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;

    // Verify composite model exists
    {
        let mut composite_repo = CompositeModels::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
        if composite_repo.get_by_id(id).await?.is_none() {
            return Err(Error::NotFound {
                resource: "composite-model".to_string(),
                id: id.to_string(),
            });
        }
    }

    // Verify group exists
    {
        let mut groups_repo = Groups::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
        if groups_repo.get_by_id(group_id).await?.is_none() {
            return Err(Error::NotFound {
                resource: "group".to_string(),
                id: group_id.to_string(),
            });
        }
    }

    let request = CompositeModelGroupCreateDBRequest {
        composite_model_id: id,
        group_id,
        granted_by: Some(current_user.id),
    };

    {
        let mut composite_repo = CompositeModels::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
        composite_repo.add_group(&request).await?;
    }

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(())
}

#[utoipa::path(
    delete,
    path = "/composite-models/{id}/groups/{group_id}",
    tag = "composite-models",
    summary = "Remove group access from a composite model",
    description = "Revoke a group's access to a composite model. Admin only.",
    params(
        ("id" = uuid::Uuid, Path, description = "Composite model ID"),
        ("group_id" = uuid::Uuid, Path, description = "Group ID to revoke access"),
    ),
    responses(
        (status = 204, description = "Group access revoked"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Group access not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn remove_composite_model_group(
    State(state): State<AppState>,
    Path((id, group_id)): Path<(CompositeModelId, GroupId)>,
    _current_user: RequiresPermission<resource::CompositeModels, operation::UpdateAll>,
) -> Result<()> {
    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = CompositeModels::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);

    let removed = repo.remove_group(id, group_id).await?;
    if !removed {
        return Err(Error::NotFound {
            resource: "group-access".to_string(),
            id: format!("{}:{}", id, group_id),
        });
    }

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(())
}

fn validate_component_weight(weight: i32) -> Result<()> {
    if !(1..=100).contains(&weight) {
        return Err(Error::BadRequest {
            message: "Weight must be between 1 and 100".to_string(),
        });
    }
    Ok(())
}
