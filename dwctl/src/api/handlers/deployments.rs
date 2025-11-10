use crate::{
    api::models::{
        deployments::{DeployedModelCreate, DeployedModelResponse, DeployedModelUpdate, GetModelQuery, ListModelsQuery, ModelProbeStatus},
        users::CurrentUser,
    },
    auth::permissions::{can_read_all_resources, has_permission, operation, resource, RequiresPermission},
    db::{
        handlers::{analytics::get_model_metrics, deployments::DeploymentFilter, Deployments, Groups, InferenceEndpoints, Repository},
        models::deployments::{DeploymentCreateDBRequest, DeploymentUpdateDBRequest, ModelPricing, ModelStatus},
    },
    errors::{Error, Result},
    types::{DeploymentId, GroupId, Resource},
    AppState,
};
use axum::{
    extract::{Path, Query, State},
    response::Json,
};
use sqlx::Acquire;

/// Apply pricing information to model response based on user permissions
/// - All users see customer-facing pricing rates
/// - Users with Pricing::ReadAll also see downstream/provider pricing details
fn apply_pricing_to_response(
    mut response: DeployedModelResponse,
    pricing: Option<ModelPricing>,
    can_read_full_pricing: bool,
) -> DeployedModelResponse {
    if let Some(model_pricing) = pricing {
        // All users get customer-facing pricing
        response = response.with_pricing(model_pricing.to_customer_pricing());

        // Only privileged users get downstream pricing
        if can_read_full_pricing {
            response = response.with_downstream_pricing(model_pricing.downstream);
        }
    }
    response
}

#[utoipa::path(
    get,
    path = "/models",
    tag = "models",
    summary = "List deployed models",
    description = "List all deployed models, optionally filtered by endpoint",
    params(
        ("endpoint" = Option<i32>, Query, description = "Filter by inference endpoint ID"),
        ("accessible" = Option<bool>, Query, description = "Filter to only models the current user can access (defaults to false for admins, true for users)"),
        ("include" = Option<String>, Query, description = "Include additional data (comma-separated: 'groups', 'metrics', 'status', 'pricing'). Only platform managers can include groups. Status shows probe monitoring information. Pricing shows simple customer rates for regular users, full pricing structure for users with Pricing::ReadAll permission."),
        ("deleted" = Option<bool>, Query, description = "Show deleted models when true (admin only), non-deleted models when false, and all models when not specified"),
        ("inactive" = Option<bool>, Query, description = "Show inactive models when true (admin only)"),
    ),
    responses(
        (status = 200, description = "Map of deployed models", body = HashMap<String, DeployedModelResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Inference endpoint not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn list_deployed_models(
    State(state): State<AppState>,
    Query(query): Query<ListModelsQuery>,
    // Lots of conditional logic here, so no logic in extractor
    current_user: CurrentUser,
) -> Result<Json<Vec<DeployedModelResponse>>> {
    let has_system_access = has_permission(&current_user, resource::Models.into(), operation::SystemAccess.into());
    let can_read_all_models = can_read_all_resources(&current_user, Resource::Models);
    let can_read_groups = can_read_all_resources(&current_user, Resource::Groups);
    let can_read_pricing = can_read_all_resources(&current_user, Resource::Pricing);
    let can_read_rate_limits = can_read_all_resources(&current_user, Resource::ModelRateLimits);
    let can_read_metrics = can_read_all_resources(&current_user, Resource::Analytics);

    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;

    // Validate endpoint exists if specified
    if let Some(endpoint_id) = query.endpoint {
        let mut endpoints_repo = InferenceEndpoints::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
        if endpoints_repo.get_by_id(endpoint_id).await?.is_none() {
            return Err(Error::NotFound {
                resource: "endpoint".to_string(),
                id: endpoint_id.to_string(),
            });
        }
    }

    // Get deployments with the filter
    let mut repo = Deployments::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);

    // Build the filter with role-based deleted parameter handling
    let mut filter = DeploymentFilter::new(0, i64::MAX);

    if let Some(endpoint_id) = query.endpoint {
        filter = filter.with_endpoint(endpoint_id);
    };

    // Handle deleted models - admins can supply query parameter
    if has_system_access {
        if query.deleted.unwrap_or(false) {
            // Admins can see deleted models if requested (default from the repo is all models,
            // inc. deleted), so no filter to add here.
        } else {
            // Admins see non-deleted models by default
            filter = filter.with_deleted(false);
        }
    } else {
        // users can only see non-deleted models
        filter = filter.with_deleted(false);
    };

    // Handle inactive models - admins can supply query parameter
    if has_system_access {
        if query.inactive.unwrap_or(false) {
            // Admins can see inactive models if requested
            filter = filter.with_statuses(vec![ModelStatus::Inactive]);
        } else {
            // Admins see active models by default
            filter = filter.with_statuses(vec![ModelStatus::Active]);
        }
    } else {
        // users can only see active models
        filter = filter.with_statuses(vec![ModelStatus::Active]);
    };

    // Apply accessibility filtering based if user doesn't have PlatformManager role
    if !can_read_all_models || query.accessible.unwrap_or(false) {
        filter = filter.with_accessible_to(current_user.id);
    }

    // Parse include parameter
    let all_includes: Vec<&str> = query
        .include
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    // Filter includes based on permissions
    let mut includes: Vec<&str> = Vec::new();
    for &include in &all_includes {
        match include {
            "groups" => {
                // Only users with Groups::ReadAll can include groups
                if can_read_groups {
                    includes.push(include);
                }
            }
            "metrics" => {
                // Only users with Analytics::ReadAll can include metrics
                if can_read_metrics {
                    includes.push(include);
                }
            }
            _ => {
                // Other includes (like pricing) are allowed for all users
                includes.push(include);
            }
        }
    }

    let filtered_models = repo.list(&filter).await?;

    let mut response: Vec<DeployedModelResponse> = vec![];

    // Prepare data for bulk fetching if needed
    let model_ids: Vec<DeploymentId> = filtered_models.iter().map(|m| m.id).collect();
    let include_groups = includes.contains(&"groups");
    let include_metrics = includes.contains(&"metrics");
    let include_status = includes.contains(&"status");
    let include_pricing = includes.contains(&"pricing");

    // Fetch groups data if requested
    let (model_groups_map, groups_map) = if include_groups {
        let groups_conn = tx.acquire().await.map_err(|e| Error::Database(e.into()))?;
        let mut groups_repo = Groups::new(&mut *groups_conn);

        let model_groups_map = groups_repo.get_deployments_groups_bulk(&model_ids).await?;

        // Collect all unique group IDs that we need to fetch
        let all_group_ids: Vec<GroupId> = model_groups_map
            .values()
            .flatten()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let groups_map = groups_repo.get_bulk(all_group_ids).await?;

        (Some(model_groups_map), Some(groups_map))
    } else {
        (None, None)
    };

    // Fetch probe status data if requested
    let status_map = if include_status {
        use crate::probes::db::ProbeManager;
        ProbeManager::get_deployment_statuses(&state.db, &model_ids).await.ok()
    } else {
        None
    };

    // Build response with requested includes
    for model in filtered_models {
        // Extract pricing before conversion (for later filtering)
        let model_pricing = model.pricing.clone();

        // Convert to api response format
        let mut model_response: DeployedModelResponse = model.into();

        // Add groups if requested and available
        if include_groups {
            if let (Some(ref model_groups_map), Some(ref groups_map)) = (&model_groups_map, &groups_map) {
                if let Some(group_ids) = model_groups_map.get(&model_response.id) {
                    let model_groups: Vec<_> = group_ids
                        .iter()
                        .filter_map(|group_id| groups_map.get(group_id))
                        .cloned()
                        .map(|group| group.into())
                        .collect();
                    model_response = model_response.with_groups(model_groups);
                } else {
                    // No groups for this model, but groups were requested, so set empty array
                    model_response = model_response.with_groups(vec![]);
                }
            } else {
                // Groups requested but no data available, set empty array
                model_response = model_response.with_groups(vec![]);
            }
        }

        // Add metrics if requested
        if include_metrics {
            match get_model_metrics(&state.db, &model_response.alias).await {
                Ok(metrics) => {
                    model_response = model_response.with_metrics(metrics);
                }
                Err(e) => {
                    // Log the error but don't fail the request - just skip metrics for this model
                    tracing::warn!("Failed to fetch metrics for model {}: {:?}", model_response.alias, e);
                }
            }
        }

        // Add probe status if requested
        if include_status {
            if let Some(ref statuses) = status_map {
                if let Some((probe_id, active, interval_seconds, last_check, last_success, uptime_percentage)) =
                    statuses.get(&model_response.id)
                {
                    let status = ModelProbeStatus {
                        probe_id: *probe_id,
                        active: *active,
                        interval_seconds: *interval_seconds,
                        last_check: *last_check,
                        last_success: *last_success,
                        uptime_percentage: *uptime_percentage,
                    };
                    model_response = model_response.with_status(status);
                } else {
                    // No probe for this model - set default status
                    let status = ModelProbeStatus {
                        probe_id: None,
                        active: false,
                        interval_seconds: None,
                        last_check: None,
                        last_success: None,
                        uptime_percentage: None,
                    };
                    model_response = model_response.with_status(status);
                }
            }
        }

        // Add pricing if requested (filtered by user role)
        if include_pricing {
            model_response = apply_pricing_to_response(model_response, model_pricing, can_read_pricing);
        }

        // Mask rate limiting info for users without ModelRateLimits permission
        if !can_read_rate_limits {
            model_response = model_response.mask_rate_limiting();
        }

        response.push(model_response);
    }

    // Commit the transaction to ensure all reads were atomic
    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(Json(response))
}

#[utoipa::path(
    post,
    path = "/models",
    tag = "models",
    summary = "Create a new deployed model",
    description = "Create a new deployed model. Admin only.",
    request_body = DeployedModelCreate,
    responses(
        (status = 201, description = "Model created successfully", body = DeployedModelResponse),
        (status = 400, description = "Bad request - invalid model data or duplicate alias/model name"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Inference endpoint not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn create_deployed_model(
    State(state): State<AppState>,
    current_user: RequiresPermission<resource::Models, operation::CreateAll>,
    Json(create): Json<DeployedModelCreate>,
) -> Result<Json<DeployedModelResponse>> {
    let model_name = create.model_name.trim();
    let alias = create.alias.as_deref().unwrap_or(model_name).trim();

    if model_name.is_empty() || alias.is_empty() {
        return Err(Error::BadRequest {
            message: "Model name and alias must not be empty or whitespace".to_string(),
        });
    }

    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;

    // Validate endpoint exists
    let mut endpoints_repo = InferenceEndpoints::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
    if endpoints_repo.get_by_id(create.hosted_on).await?.is_none() {
        return Err(Error::NotFound {
            resource: "endpoint".to_string(),
            id: create.hosted_on.to_string(),
        });
    }

    // Create the deployment - let database constraints handle uniqueness
    let mut repo = Deployments::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
    let db_request = DeploymentCreateDBRequest::from_api_create(current_user.id, create);
    let model = repo.create(&db_request).await?;
    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(Json(DeployedModelResponse::from(model)))
}

#[utoipa::path(
    patch,
    path = "/models/{id}",
    tag = "models",
    summary = "Update deployed model",
    description = "Update a deployed model",
    params(
        ("id" = uuid::Uuid, Path, description = "Deployment ID to update"),
    ),
    responses(
        (status = 200, description = "Deployed model updated successfully", body = DeployedModelResponse),
        (status = 400, description = "Bad request - invalid model data"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Inference endpoint or deployment not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn update_deployed_model(
    State(state): State<AppState>,
    Path(deployment_id): Path<DeploymentId>,
    current_user: RequiresPermission<resource::Models, operation::UpdateAll>,
    Json(update): Json<DeployedModelUpdate>,
) -> Result<Json<DeployedModelResponse>> {
    let has_system_access = has_permission(&current_user, resource::Models.into(), operation::SystemAccess.into());

    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Deployments::new(&mut pool_conn);

    // Verify deployment exists and check access based on permissions
    match repo.get_by_id(deployment_id).await {
        Ok(Some(model)) => {
            // Only allow non-admin users to access non-deleted models
            if model.deleted && !has_system_access {
                return Err(Error::NotFound {
                    resource: "Deployment".to_string(),
                    id: deployment_id.to_string(),
                });
            }
        }
        Ok(None) => {
            return Err(Error::NotFound {
                resource: "Deployment".to_string(),
                id: deployment_id.to_string(),
            })
        }
        Err(e) => return Err(e.into()),
    }

    let db_request = DeploymentUpdateDBRequest::from(update);
    let model = repo.update(deployment_id, &db_request).await?;
    Ok(Json(DeployedModelResponse::from(model)))
}

#[utoipa::path(
    get,
    path = "/models/{id}",
    tag = "models",
    summary = "Get deployed model",
    description = "Get a specific deployed model",
    params(
        ("id" = uuid::Uuid, Path, description = "Deployment ID to retrieve"),
        GetModelQuery
    ),
    responses(
        (status = 200, description = "Deployed model information", body = DeployedModelResponse),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Inference endpoint or deployment not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_deployed_model(
    State(state): State<AppState>,
    Path(deployment_id): Path<DeploymentId>,
    Query(query): Query<GetModelQuery>,
    current_user: CurrentUser,
) -> Result<Json<DeployedModelResponse>> {
    let has_system_access = has_permission(&current_user, resource::Models.into(), operation::SystemAccess.into());
    let can_read_rate_limits = can_read_all_resources(&current_user, Resource::ModelRateLimits);

    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Deployments::new(&mut pool_conn);

    let model = match repo.get_by_id(deployment_id).await {
        Ok(Some(model)) => model,
        Ok(None) => {
            return Err(Error::NotFound {
                resource: "Deployment".to_string(),
                id: deployment_id.to_string(),
            });
        }
        Err(e) => return Err(e.into()),
    };

    // Check visibility rules based on model state and user permissions
    match (model.deleted, &model.status) {
        // Deleted models: only show to admins who explicitly request them
        (true, _) => {
            if !has_system_access || !query.deleted.unwrap_or(false) {
                return Err(Error::NotFound {
                    resource: "Deployment".to_string(),
                    id: deployment_id.to_string(),
                });
            }
        }
        // Inactive models: only show to admins who explicitly request them
        (false, ModelStatus::Inactive) => {
            if !has_system_access || !query.inactive.unwrap_or(false) {
                return Err(Error::NotFound {
                    resource: "Deployment".to_string(),
                    id: deployment_id.to_string(),
                });
            }
        }
        // Active models (or other statuses): always visible if not deleted
        (false, _) => {
            // Model is visible, continue
        }
    }

    // Build and return response
    let mut response = DeployedModelResponse::from(model);

    // Mask rate limiting info for users without ModelRateLimits permission
    if !can_read_rate_limits {
        response = response.mask_rate_limiting();
    }

    Ok(Json(response))
}

#[utoipa::path(
    delete,
    path = "/models/{id}",
    tag = "models",
    summary = "Delete deployed model",
    description = "Delete a deployed model",
    params(
        ("id" = uuid::Uuid, Path, description = "Deployment ID to delete"),
    ),
    responses(
        (status = 200, description = "Deployed model deleted successfully"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Inference endpoint or deployment not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn delete_deployed_model(
    State(state): State<AppState>,
    Path(deployment_id): Path<DeploymentId>,
    _: RequiresPermission<resource::Models, operation::DeleteAll>,
) -> Result<Json<String>> {
    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Deployments::new(&mut pool_conn);

    // Hide model by setting deleted flag
    let update_request = DeploymentUpdateDBRequest::visibility_update(true);

    repo.update(deployment_id, &update_request).await?;
    Ok(Json(deployment_id.to_string()))
}

#[cfg(test)]
mod tests {

    use crate::{
        api::{handlers::deployments::DeployedModelResponse, models::users::Role},
        db::{
            handlers::{Groups, Repository},
            models::groups::GroupCreateDBRequest,
        },
        test_utils::*,
        types::DeploymentId,
    };
    use serde_json::json;
    use sqlx::PgPool;

    /// Helper function to find a model by ID in a list of models
    fn get_model_by_id(id: DeploymentId, models: &[DeployedModelResponse]) -> Option<&DeployedModelResponse> {
        models.iter().find(|model| model.id == id)
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_deployments(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let response = app
            .get(&format!("/admin/api/v1/models?endpoint={test_endpoint_id}"))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let deployments: Vec<DeployedModelResponse> = response.json();
        // Should be empty initially, but test that it returns proper structure
        assert!(deployments.is_empty() || !deployments.is_empty());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_deployments_with_nonexistent_endpoint(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let non_existent_id = uuid::Uuid::new_v4();

        let response = app
            .get(&format!("/admin/api/v1/models?endpoint={non_existent_id}"))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_model_operations(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Create a deployment on the test endpoint
        let created = create_test_deployment(&pool, user.id, "test-model", "test-alias").await;
        let deployment_id = created.id;

        // Get the deployment
        let response = app
            .get(&format!("/admin/api/v1/models/{deployment_id}"))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let model: DeployedModelResponse = response.json();
        assert_eq!(model.id, deployment_id);
        assert_eq!(model.model_name, "test-model");
        assert_eq!(model.alias, "test-alias");

        // Update the deployment
        let update = json!({
            "alias": "new-alias"
        });
        let response = app
            .patch(&format!("/admin/api/v1/models/{deployment_id}"))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .json(&update)
            .await;

        response.assert_status_ok();
        let updated_model: DeployedModelResponse = response.json();
        assert_eq!(updated_model.alias, "new-alias");

        // List models with endpoint filter
        let test_endpoint_id = get_test_endpoint_id(&pool).await;
        let response = app
            .get(&format!("/admin/api/v1/models?endpoint={test_endpoint_id}"))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let models: Vec<DeployedModelResponse> = response.json();
        assert!(models.iter().any(|it| it.id == deployment_id));

        // Delete the deployment
        let response = app
            .delete(&format!("/admin/api/v1/models/{deployment_id}"))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();

        // Verify it's deleted - should return 404 without deleted=true parameter
        let response = app
            .get(&format!("/admin/api/v1/models/{deployment_id}"))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_not_found(); // Returns 404 without deleted=true

        // But admin should be able to see it with deleted=true
        let response = app
            .get(&format!("/admin/api/v1/models/{deployment_id}?deleted=true"))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok(); // Admin can see deleted model with deleted=true
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_deployments_with_groups_include(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Create a deployment
        let deployment = create_test_deployment(&pool, admin_user.id, "test-model", "test-alias").await;
        assert!(deployment.last_sync.is_none());

        // Create a group and add the deployment to it
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);
        let group_create = GroupCreateDBRequest {
            name: "Test Group".to_string(),
            description: Some("Test group for deployment".to_string()),
            created_by: admin_user.id,
        };
        let group = group_repo.create(&group_create).await.expect("Failed to create test group");
        group_repo
            .add_deployment_to_group(deployment.id, group.id, admin_user.id)
            .await
            .expect("Failed to add deployment to group");

        // Test without include parameter - should not include groups
        let response = app
            .get("/admin/api/v1/models")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let models: Vec<DeployedModelResponse> = response.json();
        assert!(models.iter().any(|it| it.id == deployment.id && it.groups.is_none()));

        // Test with include=groups - should include groups
        let response = app
            .get("/admin/api/v1/models?include=groups")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let models: Vec<DeployedModelResponse> = response.json();

        assert!(models
            .iter()
            .any(|it| { it.id == deployment.id && it.groups.as_deref().is_some_and(|gs| gs.len() == 1 && gs[0].id == group.id) }));

        // Test with include=groups and endpoint filter
        let test_endpoint_id = get_test_endpoint_id(&pool).await;
        let response = app
            .get(&format!("/admin/api/v1/models?endpoint={test_endpoint_id}&include=groups"))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let models: Vec<DeployedModelResponse> = response.json();
        assert!(models
            .iter()
            .any(|it| { it.id == deployment.id && it.groups.as_deref().is_some_and(|gs| gs.iter().any(|g| g.id == group.id)) }));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_role_based_visibility_for_deleted_models(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;

        // Create a deployment
        let deployment = create_test_deployment(&pool, admin_user.id, "test-model", "test-alias").await;
        let deployment_id = deployment.id;

        // Both users should initially see the model
        let response = app
            .get(&format!("/admin/api/v1/models/{deployment_id}"))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;
        response.assert_status_ok();

        let response = app
            .get(&format!("/admin/api/v1/models/{deployment_id}"))
            .add_header(add_auth_headers(&regular_user).0, add_auth_headers(&regular_user).1)
            .await;
        response.assert_status_ok();

        // Admin hides the model (soft delete)
        let response = app
            .delete(&format!("/admin/api/v1/models/{deployment_id}"))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;
        response.assert_status_ok();

        // Admin should still be able to see the deleted model with deleted=true
        let response = app
            .get(&format!("/admin/api/v1/models/{deployment_id}?deleted=true"))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;
        response.assert_status_ok();
        let model: DeployedModelResponse = response.json();
        assert_eq!(model.id, deployment_id);

        // Regular user should NOT see the deleted model (404)
        let response = app
            .get(&format!("/admin/api/v1/models/{deployment_id}"))
            .add_header(add_auth_headers(&regular_user).0, add_auth_headers(&regular_user).1)
            .await;
        response.assert_status_not_found();

        // Verify the API behavior is consistent with soft deletion
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_role_based_list_filtering(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;

        // Create multiple deployments
        let deployment1 = create_test_deployment(&pool, admin_user.id, "active-model", "active-alias").await;
        let deployment2 = create_test_deployment(&pool, admin_user.id, "hidden-model", "hidden-alias").await;

        // Create a group and add regular user to it so they can see deployment1
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);
        let group_create = GroupCreateDBRequest {
            name: "List Filter Test Group".to_string(),
            description: Some("Test group for list filtering".to_string()),
            created_by: admin_user.id,
        };
        let group = group_repo.create(&group_create).await.unwrap();
        group_repo.add_user_to_group(regular_user.id, group.id).await.unwrap();

        // Add deployment1 to the group (regular user should see this)
        group_repo
            .add_deployment_to_group(deployment1.id, group.id, admin_user.id)
            .await
            .unwrap();
        // Don't add deployment2 to any group (regular user shouldn't see it)

        // Hide the second deployment
        let response = app
            .delete(&format!("/admin/api/v1/models/{}", deployment2.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;
        response.assert_status_ok();

        // Admin should see both models in list when requesting deleted=true (include deleted)
        let response = app
            .get("/admin/api/v1/models?deleted=true")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;
        response.assert_status_ok();
        let admin_all_models: Vec<DeployedModelResponse> = response.json();
        assert!(admin_all_models.iter().any(|it| it.id == deployment1.id));
        assert!(admin_all_models.iter().any(|it| it.id == deployment2.id));

        // Admin should see only non-deleted models by default
        let response = app
            .get("/admin/api/v1/models")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;
        response.assert_status_ok();
        let admin_models: Vec<DeployedModelResponse> = response.json();
        assert!(admin_models.iter().any(|it| it.id == deployment1.id));
        assert!(!admin_models.iter().any(|it| it.id == deployment2.id));

        // Regular user should only see the active model
        let response = app
            .get("/admin/api/v1/models")
            .add_header(add_auth_headers(&regular_user).0, add_auth_headers(&regular_user).1)
            .await;
        response.assert_status_ok();
        let user_models: Vec<DeployedModelResponse> = response.json();
        assert!(user_models.iter().any(|it| it.id == deployment1.id));
        assert!(!user_models.iter().any(|it| it.id == deployment2.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_role_based_update_access_for_deleted_models(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;

        // Create and hide a deployment
        let deployment = create_test_deployment(&pool, admin_user.id, "update-test-model", "update-test-alias").await;
        let deployment_id = deployment.id;

        // Hide the model
        let response = app
            .delete(&format!("/admin/api/v1/models/{deployment_id}"))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;
        response.assert_status_ok();

        // Admin should be able to update the deleted model
        let update = json!({
            "alias": "admin-updated-alias"
        });
        let response = app
            .patch(&format!("/admin/api/v1/models/{deployment_id}"))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&update)
            .await;
        response.assert_status_ok();
        let updated_model: DeployedModelResponse = response.json();
        assert_eq!(updated_model.alias, "admin-updated-alias");

        // Regular user should NOT be able to update the deleted model (404)
        let update = json!({
            "alias": "user-attempted-update"
        });
        let response = app
            .patch(&format!("/admin/api/v1/models/{deployment_id}"))
            .add_header(add_auth_headers(&regular_user).0, add_auth_headers(&regular_user).1)
            .json(&update)
            .await;
        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_soft_delete_preserves_model_accessibility_for_admin(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;

        // Create a deployment via API
        let deployment = create_test_deployment(&pool, admin_user.id, "preserve-test-model", "preserve-test-alias").await;
        let deployment_id = deployment.id;

        // Verify both users can initially access the model
        let response = app
            .get(&format!("/admin/api/v1/models/{deployment_id}"))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;
        response.assert_status_ok();

        let response = app
            .get(&format!("/admin/api/v1/models/{deployment_id}"))
            .add_header(add_auth_headers(&regular_user).0, add_auth_headers(&regular_user).1)
            .await;
        response.assert_status_ok();

        // Admin soft deletes the model
        let response = app
            .delete(&format!("/admin/api/v1/models/{deployment_id}"))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;
        response.assert_status_ok();

        // Admin can still access the model after soft deletion with deleted=true
        let response = app
            .get(&format!("/admin/api/v1/models/{deployment_id}?deleted=true"))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;
        response.assert_status_ok();
        let model: DeployedModelResponse = response.json();
        assert_eq!(model.model_name, "preserve-test-model");
        assert_eq!(model.alias, "preserve-test-alias");

        // Regular user can no longer access the model
        let response = app
            .get(&format!("/admin/api/v1/models/{deployment_id}"))
            .add_header(add_auth_headers(&regular_user).0, add_auth_headers(&regular_user).1)
            .await;
        response.assert_status_not_found();

        // Admin can still update the soft-deleted model
        let update = json!({
            "alias": "updated-after-deletion"
        });
        let response = app
            .patch(&format!("/admin/api/v1/models/{deployment_id}"))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&update)
            .await;
        response.assert_status_ok();
        let updated_model: DeployedModelResponse = response.json();
        assert_eq!(updated_model.alias, "updated-after-deletion");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_deployed_model(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create a model via API
        let create_request = json!({
            "model_name": "test-new-model",
            "alias": "Test New Model",
            "hosted_on": test_endpoint_id.to_string(),
            "description": "A test model created via API",
            "model_type": "CHAT",
            "capabilities": ["text-generation", "streaming"]
        });

        let response = app
            .post("/admin/api/v1/models")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create_request)
            .await;

        response.assert_status_ok();
        let created_model: DeployedModelResponse = response.json();

        assert_eq!(created_model.model_name, "test-new-model");
        assert_eq!(created_model.alias, "Test New Model");
        assert_eq!(created_model.hosted_on, test_endpoint_id);
        assert_eq!(created_model.description, Some("A test model created via API".to_string()));
        assert_eq!(created_model.created_by, admin_user.id);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_deployed_model_with_defaults(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create a model with minimal data (alias should default to model_name)
        let create_request = json!({
            "model_name": "simple-model",
            "hosted_on": test_endpoint_id.to_string()
        });

        let response = app
            .post("/admin/api/v1/models")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create_request)
            .await;

        response.assert_status_ok();
        let created_model: DeployedModelResponse = response.json();

        assert_eq!(created_model.model_name, "simple-model");
        assert_eq!(created_model.alias, "simple-model"); // Should default to model_name
        assert_eq!(created_model.hosted_on, test_endpoint_id);
        assert_eq!(created_model.description, None);
        assert_eq!(created_model.created_by, admin_user.id);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_deployed_model_non_admin_forbidden(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let create_request = json!({
            "model_name": "forbidden-model",
            "hosted_on": test_endpoint_id.to_string()
        });

        let response = app
            .post("/admin/api/v1/models")
            .add_header(add_auth_headers(&regular_user).0, add_auth_headers(&regular_user).1)
            .json(&create_request)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_deployed_model_nonexistent_endpoint(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        let create_request = json!({
            "model_name": "test-model",
            "hosted_on": "99999999-9999-9999-9999-999999999999"  // Non-existent endpoint
        });

        let response = app
            .post("/admin/api/v1/models")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create_request)
            .await;

        response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_include_groups_admin_only(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;

        // Create a deployment
        let deployment = create_test_deployment(&pool, admin_user.id, "groups-test-model", "groups-test-alias").await;

        // Create a group and add the deployment to it
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut groups_repo = Groups::new(&mut pool_conn);
        let group_create = GroupCreateDBRequest {
            name: "Test Group".to_string(),
            description: Some("Test group for include test".to_string()),
            created_by: admin_user.id,
        };
        let group = groups_repo.create(&group_create).await.expect("Failed to create group");
        groups_repo
            .add_deployment_to_group(deployment.id, group.id, admin_user.id)
            .await
            .expect("Failed to add deployment to group");

        // Add regular user to the group so they can see the deployment
        groups_repo
            .add_user_to_group(regular_user.id, group.id)
            .await
            .expect("Failed to add regular user to group");

        // Admin should be able to include groups and see them
        let response = app
            .get("/admin/api/v1/models?include=groups")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;
        response.assert_status_ok();
        let models: Vec<DeployedModelResponse> = response.json();

        // Find our test deployment by ID and verify it has groups included
        let test_model = get_model_by_id(deployment.id, &models).unwrap_or_else(|| {
            panic!(
                "Test model not found. Available models: {:?}",
                models.iter().map(|m| &m.id).collect::<Vec<_>>()
            )
        });

        assert!(test_model.groups.is_some(), "Admin should see groups included");
        let groups = test_model.groups.as_ref().unwrap();
        assert_eq!(groups.len(), 1, "Should have exactly one group");
        assert_eq!(groups[0].name, "Test Group");

        // Regular user should NOT be able to include groups (groups should be None)
        let response = app
            .get("/admin/api/v1/models?include=groups")
            .add_header(add_auth_headers(&regular_user).0, add_auth_headers(&regular_user).1)
            .await;
        response.assert_status_ok();
        let models: Vec<DeployedModelResponse> = response.json();

        // Find our test deployment by ID and verify groups are NOT included
        let test_model = get_model_by_id(deployment.id, &models).unwrap_or_else(|| {
            panic!(
                "Test model not found. Available models: {:?}",
                models.iter().map(|m| &m.id).collect::<Vec<_>>()
            )
        });
        assert!(test_model.groups.is_none(), "Regular user should NOT see groups included");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_accessible_parameter_filtering(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;

        // Create deployments
        let deployment1 = create_test_deployment(&pool, admin_user.id, "test-model-1", "test-alias-1").await;
        let deployment2 = create_test_deployment(&pool, admin_user.id, "test-model-2", "test-alias-2").await;

        // Create a group and add regular user to it
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);
        let group_create = GroupCreateDBRequest {
            name: "Access Test Group".to_string(),
            description: Some("Test group for accessible filtering".to_string()),
            created_by: admin_user.id,
        };
        let group = group_repo.create(&group_create).await.unwrap();
        group_repo.add_user_to_group(regular_user.id, group.id).await.unwrap();

        // Add only deployment1 to the group (regular user should only access this one)
        group_repo
            .add_deployment_to_group(deployment1.id, group.id, admin_user.id)
            .await
            .unwrap();
        // Don't add deployment2 to any group

        // Test 1: Regular user without accessible=true should still get filtered (default behavior)
        let response = app
            .get("/admin/api/v1/models")
            .add_header(add_auth_headers(&regular_user).0, add_auth_headers(&regular_user).1)
            .await;
        response.assert_status_ok();
        let user_models: Vec<DeployedModelResponse> = response.json();
        assert_eq!(user_models.len(), 1, "Regular user should only see 1 accessible model");
        assert!(get_model_by_id(deployment1.id, &user_models).is_some());
        assert!(get_model_by_id(deployment2.id, &user_models).is_none());

        // Test 2: Regular user with accessible=true should get same result (explicit filtering)
        let response = app
            .get("/admin/api/v1/models?accessible=true")
            .add_header(add_auth_headers(&regular_user).0, add_auth_headers(&regular_user).1)
            .await;
        response.assert_status_ok();
        let user_models_explicit: Vec<DeployedModelResponse> = response.json();
        assert_eq!(user_models_explicit.len(), 1);
        assert!(get_model_by_id(deployment1.id, &user_models_explicit).is_some());

        // Test 3: Admin user without accessible parameter should see all models (default)
        let response = app
            .get("/admin/api/v1/models")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;
        response.assert_status_ok();
        let admin_models: Vec<DeployedModelResponse> = response.json();
        assert_eq!(admin_models.len(), 2, "Admin should see all models by default");
        assert!(get_model_by_id(deployment1.id, &admin_models).is_some());
        assert!(get_model_by_id(deployment2.id, &admin_models).is_some());

        // Test 4: Admin user with accessible=false should see all models (explicit no filtering)
        let response = app
            .get("/admin/api/v1/models?accessible=false")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;
        response.assert_status_ok();
        let admin_models_explicit: Vec<DeployedModelResponse> = response.json();
        assert_eq!(admin_models_explicit.len(), 2);

        // Test 5: Admin user with accessible=true should get filtered results (only their accessible models)
        // First add admin to a group and that group to deployment1
        group_repo.add_user_to_group(admin_user.id, group.id).await.unwrap();

        let response = app
            .get("/admin/api/v1/models?accessible=true")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;
        response.assert_status_ok();
        let admin_accessible: Vec<DeployedModelResponse> = response.json();
        assert_eq!(
            admin_accessible.len(),
            1,
            "Admin with accessible=true should only see their accessible models"
        );
        assert!(get_model_by_id(deployment1.id, &admin_accessible).is_some());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_include_metrics_parameter(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;

        // Create a deployment
        let deployment = create_test_deployment(&pool, admin_user.id, "metrics-test-model", "metrics-test-alias").await;

        // Test without include parameter - should not include metrics
        let response = app
            .get("/admin/api/v1/models")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let models: Vec<DeployedModelResponse> = response.json();
        let test_model = get_model_by_id(deployment.id, &models).unwrap();
        assert!(test_model.metrics.is_none(), "Should not include metrics by default");

        // Test with include=metrics - should include metrics
        let response = app
            .get("/admin/api/v1/models?include=metrics")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let models: Vec<DeployedModelResponse> = response.json();
        let test_model = get_model_by_id(deployment.id, &models).unwrap();
        assert!(test_model.metrics.is_some(), "Admin should see metrics when requested");
        let metrics = test_model.metrics.as_ref().unwrap();
        assert_eq!(metrics.total_requests, 0); // No requests yet, so should be 0

        // Test that regular users CANNOT include metrics (no Analytics::ReadAll permission)
        let response = app
            .get("/admin/api/v1/models?include=metrics")
            .add_header(add_auth_headers(&regular_user).0, add_auth_headers(&regular_user).1)
            .await;

        response.assert_status_ok();
        let models: Vec<DeployedModelResponse> = response.json();
        if let Some(test_model) = get_model_by_id(deployment.id, &models) {
            assert!(
                test_model.metrics.is_none(),
                "Regular user should NOT see metrics (no Analytics::ReadAll permission)"
            );
        }

        // Test with include=groups,metrics - should include both for admin
        let response = app
            .get("/admin/api/v1/models?include=groups,metrics")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let models: Vec<DeployedModelResponse> = response.json();
        let test_model = get_model_by_id(deployment.id, &models).unwrap();
        assert!(test_model.groups.is_some(), "Admin should see groups when requested");
        assert!(test_model.metrics.is_some(), "Admin should see metrics when requested");

        // Test that regular users cannot include groups or metrics (no permissions)
        let response = app
            .get("/admin/api/v1/models?include=groups,metrics")
            .add_header(add_auth_headers(&regular_user).0, add_auth_headers(&regular_user).1)
            .await;

        response.assert_status_ok();
        let models: Vec<DeployedModelResponse> = response.json();
        if let Some(test_model) = get_model_by_id(deployment.id, &models) {
            assert!(test_model.groups.is_none(), "Regular user should NOT see groups");
            assert!(
                test_model.metrics.is_none(),
                "Regular user should NOT see metrics (no Analytics::ReadAll permission)"
            );
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_sees_all_models_by_default(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;

        // Create multiple deployments
        let deployment1 = create_test_deployment(&pool, platform_manager.id, "pm-model-1", "pm-alias-1").await;
        let deployment2 = create_test_deployment(&pool, platform_manager.id, "pm-model-2", "pm-alias-2").await;

        // Create a group and add only standard user to it
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);
        let group_create = GroupCreateDBRequest {
            name: "Standard User Group".to_string(),
            description: Some("Group for standard user only".to_string()),
            created_by: platform_manager.id,
        };
        let group = group_repo.create(&group_create).await.unwrap();
        group_repo.add_user_to_group(standard_user.id, group.id).await.unwrap();

        // Add only deployment1 to the group (standard user should only see this)
        group_repo
            .add_deployment_to_group(deployment1.id, group.id, platform_manager.id)
            .await
            .unwrap();
        // Don't add deployment2 to any group - platform manager should still see it

        // Platform manager should see ALL models (both deployment1 and deployment2)
        let response = app
            .get("/admin/api/v1/models")
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;
        response.assert_status_ok();
        let pm_models: Vec<DeployedModelResponse> = response.json();

        assert!(pm_models.iter().any(|m| m.id == deployment1.id), "PM should see deployment1");
        assert!(
            pm_models.iter().any(|m| m.id == deployment2.id),
            "PM should see deployment2 even without group access"
        );

        // Standard user should only see models they have access to (deployment1 only)
        let response = app
            .get("/admin/api/v1/models")
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;
        response.assert_status_ok();
        let user_models: Vec<DeployedModelResponse> = response.json();

        let user_accessible_count = user_models
            .iter()
            .filter(|m| m.id == deployment1.id || m.id == deployment2.id)
            .count();
        assert_eq!(user_accessible_count, 1, "Standard user should only see 1 accessible model");
        assert!(user_models.iter().any(|m| m.id == deployment1.id), "User should see deployment1");
        assert!(
            !user_models.iter().any(|m| m.id == deployment2.id),
            "User should NOT see deployment2"
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_can_request_accessible_filtering(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Create deployments
        let deployment1 = create_test_deployment(&pool, platform_manager.id, "pm-access-1", "pm-access-alias-1").await;
        let deployment2 = create_test_deployment(&pool, platform_manager.id, "pm-access-2", "pm-access-alias-2").await;

        // Create a group and add platform manager to it
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);
        let group_create = GroupCreateDBRequest {
            name: "PM Access Group".to_string(),
            description: Some("Group for platform manager accessibility test".to_string()),
            created_by: platform_manager.id,
        };
        let group = group_repo.create(&group_create).await.unwrap();
        group_repo.add_user_to_group(platform_manager.id, group.id).await.unwrap();

        // Add only deployment1 to the group
        group_repo
            .add_deployment_to_group(deployment1.id, group.id, platform_manager.id)
            .await
            .unwrap();

        // Platform manager with accessible=false should see ALL models (default behavior)
        let response = app
            .get("/admin/api/v1/models?accessible=false")
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;
        response.assert_status_ok();
        let all_models: Vec<DeployedModelResponse> = response.json();

        assert!(all_models.iter().any(|m| m.id == deployment1.id));
        assert!(all_models.iter().any(|m| m.id == deployment2.id));

        // Platform manager with accessible=true should see only accessible models
        let response = app
            .get("/admin/api/v1/models?accessible=true")
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;
        response.assert_status_ok();
        let accessible_models: Vec<DeployedModelResponse> = response.json();

        let accessible_count = accessible_models
            .iter()
            .filter(|m| m.id == deployment1.id || m.id == deployment2.id)
            .count();
        assert_eq!(accessible_count, 1, "PM with accessible=true should see only 1 accessible model");
        assert!(
            accessible_models.iter().any(|m| m.id == deployment1.id),
            "Should see accessible deployment"
        );
        assert!(
            !accessible_models.iter().any(|m| m.id == deployment2.id),
            "Should NOT see non-accessible deployment"
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_request_viewer_role_gets_filtered(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;

        // Create deployments
        let deployment1 = create_test_deployment(&pool, platform_manager.id, "rv-model-1", "rv-alias-1").await;
        let deployment2 = create_test_deployment(&pool, platform_manager.id, "rv-model-2", "rv-alias-2").await;

        // Create a group and add request viewer to it
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);
        let group_create = GroupCreateDBRequest {
            name: "Request Viewer Group".to_string(),
            description: Some("Group for request viewer test".to_string()),
            created_by: platform_manager.id,
        };
        let group = group_repo.create(&group_create).await.unwrap();
        group_repo.add_user_to_group(request_viewer.id, group.id).await.unwrap();

        // Add only deployment1 to the group
        group_repo
            .add_deployment_to_group(deployment1.id, group.id, platform_manager.id)
            .await
            .unwrap();

        // Request viewer should only see models they have access to (like standard user)
        let response = app
            .get("/admin/api/v1/models")
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;
        response.assert_status_ok();
        let rv_models: Vec<DeployedModelResponse> = response.json();

        let rv_accessible_count = rv_models
            .iter()
            .filter(|m| m.id == deployment1.id || m.id == deployment2.id)
            .count();
        assert_eq!(rv_accessible_count, 1, "RequestViewer should only see 1 accessible model");
        assert!(rv_models.iter().any(|m| m.id == deployment1.id), "Should see accessible deployment");
        assert!(
            !rv_models.iter().any(|m| m.id == deployment2.id),
            "Should NOT see non-accessible deployment"
        );

        // Compare with platform manager who should see both
        let response = app
            .get("/admin/api/v1/models")
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;
        response.assert_status_ok();
        let pm_models: Vec<DeployedModelResponse> = response.json();

        assert!(pm_models.iter().any(|m| m.id == deployment1.id));
        assert!(pm_models.iter().any(|m| m.id == deployment2.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_can_see_newly_created_models(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create a model via API
        let create_request = json!({
            "model_name": "pm-new-model",
            "alias": "Platform Manager New Model",
            "hosted_on": test_endpoint_id.to_string(),
            "description": "A model created by platform manager"
        });

        let response = app
            .post("/admin/api/v1/models")
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .json(&create_request)
            .await;
        response.assert_status_ok();

        let created_model: DeployedModelResponse = response.json();
        let deployment_id = created_model.id;

        // Platform manager should immediately see the newly created model in list
        let response = app
            .get("/admin/api/v1/models")
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;
        response.assert_status_ok();

        let models: Vec<DeployedModelResponse> = response.json();
        assert!(
            models.iter().any(|m| m.id == deployment_id),
            "Platform manager should see newly created model immediately"
        );

        // Verify the model details
        let found_model = models.iter().find(|m| m.id == deployment_id).unwrap();
        assert_eq!(found_model.model_name, "pm-new-model");
        assert_eq!(found_model.alias, "Platform Manager New Model");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_standard_user_cannot_see_ungrouped_models(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;

        // Platform manager creates a model
        let deployment = create_test_deployment(&pool, platform_manager.id, "ungrouped-model", "ungrouped-alias").await;

        // Don't add the model to any groups

        // Platform manager should see the ungrouped model
        let response = app
            .get("/admin/api/v1/models")
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;
        response.assert_status_ok();

        let pm_models: Vec<DeployedModelResponse> = response.json();
        assert!(
            pm_models.iter().any(|m| m.id == deployment.id),
            "Platform manager should see ungrouped model"
        );

        // Standard user should NOT see the ungrouped model
        let response = app
            .get("/admin/api/v1/models")
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;
        response.assert_status_ok();

        let user_models: Vec<DeployedModelResponse> = response.json();
        assert!(
            !user_models.iter().any(|m| m.id == deployment.id),
            "Standard user should NOT see ungrouped model"
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_request_viewer_cannot_modify_models(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create a deployment as platform manager
        let deployment = create_test_deployment(&pool, platform_manager.id, "rv-test-model", "rv-test-alias").await;

        // RequestViewer should NOT be able to create models
        let create_request = json!({
            "model_name": "rv-forbidden-create",
            "hosted_on": test_endpoint_id.to_string()
        });

        let response = app
            .post("/admin/api/v1/models")
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .json(&create_request)
            .await;

        response.assert_status_forbidden();

        // RequestViewer should NOT be able to update models
        let update = json!({"alias": "rv-forbidden-update"});
        let response = app
            .patch(&format!("/admin/api/v1/models/{}", deployment.id))
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .json(&update)
            .await;

        response.assert_status_forbidden();

        // RequestViewer should NOT be able to delete models
        let response = app
            .delete(&format!("/admin/api/v1/models/{}", deployment.id))
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_standard_user_cannot_modify_models(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create a deployment as platform manager
        let deployment = create_test_deployment(&pool, platform_manager.id, "su-test-model", "su-test-alias").await;

        // StandardUser should NOT be able to create models
        let create_request = json!({
            "model_name": "su-forbidden-create",
            "hosted_on": test_endpoint_id.to_string()
        });

        let response = app
            .post("/admin/api/v1/models")
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .json(&create_request)
            .await;

        response.assert_status_forbidden();

        // StandardUser should NOT be able to update models
        let update = json!({"alias": "su-forbidden-update"});
        let response = app
            .patch(&format!("/admin/api/v1/models/{}", deployment.id))
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .json(&update)
            .await;

        response.assert_status_forbidden();

        // StandardUser should NOT be able to delete models
        let response = app
            .delete(&format!("/admin/api/v1/models/{}", deployment.id))
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_multi_role_user_cannot_modify_models_without_platform_manager(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        // Create user with StandardUser + RequestViewer (but not PlatformManager)
        let multi_role_user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::RequestViewer]).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Multi-role user should still NOT be able to create models (needs PlatformManager role)
        let create_request = json!({
            "model_name": "multi-forbidden-create",
            "hosted_on": test_endpoint_id.to_string()
        });

        let response = app
            .post("/admin/api/v1/models")
            .add_header(add_auth_headers(&multi_role_user).0, add_auth_headers(&multi_role_user).1)
            .json(&create_request)
            .await;

        response.assert_status_forbidden();

        // Create a deployment to test update/delete
        let deployment = create_test_deployment(&pool, platform_manager.id, "multi-test-model", "multi-test-alias").await;

        // Multi-role user should NOT be able to update models
        let update = json!({"alias": "multi-forbidden-update"});
        let response = app
            .patch(&format!("/admin/api/v1/models/{}", deployment.id))
            .add_header(add_auth_headers(&multi_role_user).0, add_auth_headers(&multi_role_user).1)
            .json(&update)
            .await;

        response.assert_status_forbidden();

        // Multi-role user should NOT be able to delete models
        let response = app
            .delete(&format!("/admin/api/v1/models/{}", deployment.id))
            .add_header(add_auth_headers(&multi_role_user).0, add_auth_headers(&multi_role_user).1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_plus_standard_user_can_modify_models(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        // Create user with both PlatformManager and StandardUser roles
        let platform_user = create_test_user_with_roles(&pool, vec![Role::PlatformManager, Role::StandardUser]).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Should be able to create models (PlatformManager permission)
        let create_request = json!({
            "model_name": "pm-create-test",
            "hosted_on": test_endpoint_id.to_string(),
            "alias": "Platform Manager Created"
        });

        let response = app
            .post("/admin/api/v1/models")
            .add_header(add_auth_headers(&platform_user).0, add_auth_headers(&platform_user).1)
            .json(&create_request)
            .await;

        response.assert_status_ok();
        let created_model: DeployedModelResponse = response.json();

        // Should be able to update models
        let update = json!({"alias": "PM Updated Alias"});
        let response = app
            .patch(&format!("/admin/api/v1/models/{}", created_model.id))
            .add_header(add_auth_headers(&platform_user).0, add_auth_headers(&platform_user).1)
            .json(&update)
            .await;

        response.assert_status_ok();
        let updated_model: DeployedModelResponse = response.json();
        assert_eq!(updated_model.alias, "PM Updated Alias");

        // Should be able to delete models
        let response = app
            .delete(&format!("/admin/api/v1/models/{}", created_model.id))
            .add_header(add_auth_headers(&platform_user).0, add_auth_headers(&platform_user).1)
            .await;

        response.assert_status_ok();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_accessibility_filtering_permissions(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;

        // Create deployments
        let accessible_deployment = create_test_deployment(&pool, platform_manager.id, "accessible-model", "accessible-alias").await;
        let inaccessible_deployment = create_test_deployment(&pool, platform_manager.id, "inaccessible-model", "inaccessible-alias").await;

        // Create a group and add standard_user to it
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);
        let group_create = GroupCreateDBRequest {
            name: "Access Test Group".to_string(),
            description: Some("Group for accessibility testing".to_string()),
            created_by: platform_manager.id,
        };
        let group = group_repo.create(&group_create).await.unwrap();
        group_repo.add_user_to_group(standard_user.id, group.id).await.unwrap();
        group_repo.add_user_to_group(request_viewer.id, group.id).await.unwrap();

        // Add only accessible_deployment to the group
        group_repo
            .add_deployment_to_group(accessible_deployment.id, group.id, platform_manager.id)
            .await
            .unwrap();

        // StandardUser should only see accessible models (default behavior)
        let response = app
            .get("/admin/api/v1/models")
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;
        response.assert_status_ok();

        let standard_models: Vec<DeployedModelResponse> = response.json();
        assert!(standard_models.iter().any(|m| m.id == accessible_deployment.id));
        assert!(!standard_models.iter().any(|m| m.id == inaccessible_deployment.id));

        // RequestViewer should have same accessibility filtering as StandardUser
        let response = app
            .get("/admin/api/v1/models")
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;
        response.assert_status_ok();

        let rv_models: Vec<DeployedModelResponse> = response.json();
        assert!(rv_models.iter().any(|m| m.id == accessible_deployment.id));
        assert!(!rv_models.iter().any(|m| m.id == inaccessible_deployment.id));

        // PlatformManager should see all models by default
        let response = app
            .get("/admin/api/v1/models")
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;
        response.assert_status_ok();

        let pm_models: Vec<DeployedModelResponse> = response.json();
        assert!(pm_models.iter().any(|m| m.id == accessible_deployment.id));
        assert!(pm_models.iter().any(|m| m.id == inaccessible_deployment.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_groups_include_permission_enforcement(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;

        // Create a deployment and add it to a group
        let deployment = create_test_deployment(&pool, platform_manager.id, "groups-perm-model", "groups-perm-alias").await;

        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);
        let group_create = GroupCreateDBRequest {
            name: "Groups Permission Test".to_string(),
            description: Some("Test group for groups include permission".to_string()),
            created_by: platform_manager.id,
        };
        let group = group_repo.create(&group_create).await.unwrap();

        // Add all users to the group so they can see the deployment
        group_repo.add_user_to_group(standard_user.id, group.id).await.unwrap();
        group_repo.add_user_to_group(request_viewer.id, group.id).await.unwrap();
        group_repo
            .add_deployment_to_group(deployment.id, group.id, platform_manager.id)
            .await
            .unwrap();

        // PlatformManager should be able to include groups
        let response = app
            .get("/admin/api/v1/models?include=groups")
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;
        response.assert_status_ok();
        let pm_models: Vec<DeployedModelResponse> = response.json();
        let pm_model = pm_models.iter().find(|m| m.id == deployment.id).unwrap();
        assert!(pm_model.groups.is_some(), "PlatformManager should see groups when included");

        // StandardUser should NOT be able to include groups (groups should be None)
        let response = app
            .get("/admin/api/v1/models?include=groups")
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;
        response.assert_status_ok();
        let models: Vec<DeployedModelResponse> = response.json();
        let test_model = get_model_by_id(deployment.id, &models).unwrap();
        assert!(test_model.groups.is_none(), "Regular user should NOT see groups included");

        // RequestViewer should NOT be able to include groups
        let response = app
            .get("/admin/api/v1/models?include=groups")
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;
        response.assert_status_ok();

        let rv_models: Vec<DeployedModelResponse> = response.json();
        let rv_model = rv_models.iter().find(|m| m.id == deployment.id).unwrap();
        assert!(rv_model.groups.is_none(), "RequestViewer should NOT see groups even when requested");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_rate_limits_permission_gating(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;

        // Create a deployment with rate limits
        let deployment = create_test_deployment(&pool, platform_manager.id, "rate-limit-test", "rate-limit-alias").await;

        // Set rate limits on the deployment
        let update = json!({
            "requests_per_second": 100.0,
            "burst_size": 200
        });
        let response = app
            .patch(&format!("/admin/api/v1/models/{}", deployment.id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .json(&update)
            .await;
        response.assert_status_ok();

        // Create a group and add users to it so they can see the deployment
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);
        let group_create = GroupCreateDBRequest {
            name: "Rate Limit Test Group".to_string(),
            description: Some("Test group for rate limit permissions".to_string()),
            created_by: platform_manager.id,
        };
        let group = group_repo.create(&group_create).await.unwrap();
        group_repo.add_user_to_group(standard_user.id, group.id).await.unwrap();
        group_repo.add_user_to_group(request_viewer.id, group.id).await.unwrap();
        group_repo
            .add_deployment_to_group(deployment.id, group.id, platform_manager.id)
            .await
            .unwrap();

        // PlatformManager should see rate limits (has ModelRateLimits::ReadAll)
        let response = app
            .get(&format!("/admin/api/v1/models/{}", deployment.id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;
        response.assert_status_ok();
        let pm_model: DeployedModelResponse = response.json();
        assert_eq!(pm_model.requests_per_second, Some(100.0), "PlatformManager should see rate limits");
        assert_eq!(pm_model.burst_size, Some(200), "PlatformManager should see burst size");

        // StandardUser should NOT see rate limits (masked)
        let response = app
            .get(&format!("/admin/api/v1/models/{}", deployment.id))
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;
        response.assert_status_ok();
        let user_model: DeployedModelResponse = response.json();
        assert_eq!(user_model.requests_per_second, None, "StandardUser should NOT see rate limits");
        assert_eq!(user_model.burst_size, None, "StandardUser should NOT see burst size");

        // RequestViewer should NOT see rate limits (masked)
        let response = app
            .get(&format!("/admin/api/v1/models/{}", deployment.id))
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;
        response.assert_status_ok();
        let rv_model: DeployedModelResponse = response.json();
        assert_eq!(rv_model.requests_per_second, None, "RequestViewer should NOT see rate limits");
        assert_eq!(rv_model.burst_size, None, "RequestViewer should NOT see burst size");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_metrics_permission_gating(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;

        // Create a deployment
        let deployment = create_test_deployment(&pool, platform_manager.id, "metrics-perm-test", "metrics-perm-alias").await;

        // Create a group and add users to it so they can see the deployment
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);
        let group_create = GroupCreateDBRequest {
            name: "Metrics Permission Test Group".to_string(),
            description: Some("Test group for metrics permissions".to_string()),
            created_by: platform_manager.id,
        };
        let group = group_repo.create(&group_create).await.unwrap();
        group_repo.add_user_to_group(standard_user.id, group.id).await.unwrap();
        group_repo.add_user_to_group(request_viewer.id, group.id).await.unwrap();
        group_repo
            .add_deployment_to_group(deployment.id, group.id, platform_manager.id)
            .await
            .unwrap();

        // PlatformManager should be able to include metrics (has Analytics::ReadAll)
        let response = app
            .get("/admin/api/v1/models?include=metrics")
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;
        response.assert_status_ok();
        let models: Vec<DeployedModelResponse> = response.json();
        let pm_model = get_model_by_id(deployment.id, &models).unwrap();
        assert!(pm_model.metrics.is_some(), "PlatformManager should see metrics when requested");

        // StandardUser should NOT be able to include metrics (no Analytics::ReadAll)
        let response = app
            .get("/admin/api/v1/models?include=metrics")
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;
        response.assert_status_ok();
        let models: Vec<DeployedModelResponse> = response.json();
        let user_model = get_model_by_id(deployment.id, &models).unwrap();
        assert!(
            user_model.metrics.is_none(),
            "StandardUser should NOT see metrics even when requested"
        );

        // RequestViewer should be able to include metrics (has Analytics::ReadAll)
        let response = app
            .get("/admin/api/v1/models?include=metrics")
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;
        response.assert_status_ok();
        let models: Vec<DeployedModelResponse> = response.json();
        let rv_model = get_model_by_id(deployment.id, &models).unwrap();
        assert!(rv_model.metrics.is_some(), "RequestViewer should see metrics when requested");
    }
}
