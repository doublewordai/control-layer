use crate::api::models::deployments::DeployedModelResponse;
use crate::api::models::groups::{GroupCreate, GroupResponse, GroupUpdate, ListGroupsQuery};
use crate::api::models::users::{CurrentUser, UserResponse};
use crate::auth::permissions::{can_read_all_resources, can_read_own_resource, operation, resource, RequiresPermission};
use crate::db::handlers::{groups::GroupFilter, Deployments, Groups, Repository, Users};
use crate::db::models::groups::{GroupCreateDBRequest, GroupUpdateDBRequest};
use crate::errors::{Error, Result};
use crate::types::{Operation, Permission, Resource};
use crate::{
    types::{DeploymentId, GroupId, UserId},
    AppState,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use sqlx::Acquire;

#[utoipa::path(
    get,
    path = "/groups",
    tag = "groups",
    summary = "List groups",
    responses(
        (status = 200, description = "List of groups", body = Vec<GroupResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("skip" = Option<i64>, Query, description = "Number of groups to skip"),
        ("limit" = Option<i64>, Query, description = "Maximum number of groups to return"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn list_groups(
    State(state): State<AppState>,
    Query(query): Query<ListGroupsQuery>,
    _: RequiresPermission<resource::Groups, operation::ReadAll>,
) -> Result<Json<Vec<GroupResponse>>> {
    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;

    let groups;
    {
        let mut repo = Groups::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
        let skip = query.skip.unwrap_or(0);
        let limit = query.limit.unwrap_or(100).min(1000);

        groups = repo.list(&GroupFilter::new(skip, limit)).await?;
    }

    // Parse include parameter
    let includes: Vec<&str> = query
        .include
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    let mut response_groups = Vec::new();

    // If includes are requested, fetch relationships efficiently
    if !includes.is_empty() {
        let group_ids: Vec<_> = groups.iter().map(|g| g.id).collect();

        let resolved_users_map = if includes.contains(&"users") {
            // First get the user IDs for each group
            let groups_users_map;
            {
                let mut repo = Groups::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
                groups_users_map = repo.get_groups_users_bulk(&group_ids).await?;
            }
            // Collect all unique user IDs
            let all_user_ids: Vec<UserId> = groups_users_map
                .values()
                .flat_map(|user_ids| user_ids.iter())
                .copied()
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();

            // Fetch all users in bulk
            let users_bulk;
            {
                let mut users_repo = Users::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
                users_bulk = users_repo.get_bulk(all_user_ids).await?;
            }

            // Build a map from group_id to Vec<UserResponse>
            let mut resolved_map = std::collections::HashMap::new();
            for (group_id, user_ids) in groups_users_map {
                let users: Vec<UserResponse> = user_ids
                    .iter()
                    .filter_map(|user_id| users_bulk.get(user_id))
                    .map(|user_db| UserResponse::from(user_db.clone()))
                    .collect();
                resolved_map.insert(group_id, users);
            }
            Some(resolved_map)
        } else {
            None
        };

        let resolved_models_map = if includes.contains(&"models") {
            // First get the deployment IDs for each group
            let groups_deployments_map;
            {
                let mut repo = Groups::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
                groups_deployments_map = repo.get_groups_deployments_bulk(&group_ids).await?;
            }
            // Collect all unique deployment IDs
            let all_deployment_ids: Vec<DeploymentId> = groups_deployments_map
                .values()
                .flat_map(|deployment_ids| deployment_ids.iter())
                .copied()
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();

            // Fetch all deployments in bulk
            let mut deployments_repo = Deployments::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);
            let deployments_bulk = deployments_repo.get_bulk(all_deployment_ids).await?;
            // Build a map from group_id to Vec<DeployedModelResponse>
            let mut resolved_map = std::collections::HashMap::new();
            for (group_id, deployment_ids) in groups_deployments_map {
                let models: Vec<DeployedModelResponse> = deployment_ids
                    .iter()
                    .filter_map(|deployment_id| deployments_bulk.get(deployment_id))
                    .map(|deployment_db| DeployedModelResponse::from(deployment_db.clone()))
                    .collect();
                resolved_map.insert(group_id, models);
            }
            Some(resolved_map)
        } else {
            None
        };

        for group in groups {
            let users = resolved_users_map.as_ref().and_then(|map| map.get(&group.id).cloned());
            let models = resolved_models_map.as_ref().and_then(|map| map.get(&group.id).cloned());

            let response_group = GroupResponse::from(group).with_relationships(users, models);
            response_groups.push(response_group);
        }
    } else {
        // No includes requested, just convert normally
        response_groups = groups.into_iter().map(GroupResponse::from).collect();
    }

    // Commit the transaction to ensure all reads were atomic
    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(Json(response_groups))
}

#[utoipa::path(
    post,
    path = "/groups",
    tag = "groups",
    summary = "Create group",
    request_body = GroupCreate,
    responses(
        (status = 201, description = "Group created successfully", body = GroupResponse),
        (status = 400, description = "Invalid request"),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error")
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn create_group(
    State(state): State<AppState>,
    current_user: RequiresPermission<resource::Groups, operation::CreateAll>,
    Json(create): Json<GroupCreate>,
) -> Result<(StatusCode, Json<GroupResponse>)> {
    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Groups::new(&mut pool_conn);
    let request = GroupCreateDBRequest::new(current_user.id, create);

    let group = repo.create(&request).await?;
    Ok((StatusCode::CREATED, Json(GroupResponse::from(group))))
}

#[utoipa::path(
    get,
    path = "/groups/{group_id}",
    tag = "groups",
    summary = "Get group",
    responses(
        (status = 200, description = "Group details", body = GroupResponse),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Group not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("group_id" = uuid::Uuid, Path, description = "Group ID")
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_group(
    State(state): State<AppState>,
    Path(group_id): Path<GroupId>,
    _: RequiresPermission<resource::Groups, operation::ReadAll>,
) -> Result<Json<GroupResponse>> {
    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Groups::new(&mut pool_conn);

    match repo.get_by_id(group_id).await? {
        Some(group) => Ok(Json(GroupResponse::from(group))),
        None => Err(Error::NotFound {
            resource: "Group".to_string(),
            id: group_id.to_string(),
        }),
    }
}

#[utoipa::path(
    patch,
    path = "/groups/{group_id}",
    tag = "groups",
    summary = "Update group",
    request_body = GroupUpdate,
    responses(
        (status = 200, description = "Group updated successfully", body = GroupResponse),
        (status = 400, description = "Invalid request"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Group not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("group_id" = uuid::Uuid, Path, description = "Group ID")
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn update_group(
    State(state): State<AppState>,
    Path(group_id): Path<GroupId>,
    _: RequiresPermission<resource::Groups, operation::UpdateAll>,
    Json(update): Json<GroupUpdate>,
) -> Result<Json<GroupResponse>> {
    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Groups::new(&mut pool_conn);
    let request = GroupUpdateDBRequest::from(update);

    let group = repo.update(group_id, &request).await?;
    Ok(Json(GroupResponse::from(group)))
}

#[utoipa::path(
    delete,
    path = "/groups/{group_id}",
    tag = "groups",
    summary = "Delete group",
    responses(
        (status = 204, description = "Group deleted successfully"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Group not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("group_id" = uuid::Uuid, Path, description = "Group ID")
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn delete_group(
    State(state): State<AppState>,
    Path(group_id): Path<GroupId>,
    _: RequiresPermission<resource::Groups, operation::DeleteAll>,
) -> Result<StatusCode> {
    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Groups::new(&mut pool_conn);

    if repo.delete(group_id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Error::NotFound {
            resource: "Group".to_string(),
            id: group_id.to_string(),
        })
    }
}

#[utoipa::path(
    post,
    path = "/groups/{group_id}/users/{user_id}",
    tag = "groups",
    summary = "Add user to group",
    responses(
        (status = 204, description = "User added to group successfully"),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("group_id" = uuid::Uuid, Path, description = "Group ID"),
        ("user_id" = uuid::Uuid, Path, description = "User ID")
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn add_user_to_group(
    State(state): State<AppState>,
    Path((group_id, user_id)): Path<(GroupId, UserId)>,
    _: RequiresPermission<resource::Groups, operation::UpdateAll>,
) -> Result<StatusCode> {
    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Groups::new(&mut pool_conn);
    repo.add_user_to_group(user_id, group_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/groups/{group_id}/users/{user_id}",
    tag = "groups",
    summary = "Remove user from group",
    responses(
        (status = 204, description = "User removed from group successfully"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Relationship not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("group_id" = uuid::Uuid, Path, description = "Group ID"),
        ("user_id" = uuid::Uuid, Path, description = "User ID")
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn remove_user_from_group(
    State(state): State<AppState>,
    Path((group_id, user_id)): Path<(GroupId, UserId)>,
    _: RequiresPermission<resource::Groups, operation::UpdateAll>,
) -> Result<StatusCode> {
    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Groups::new(&mut pool_conn);
    repo.remove_user_from_group(user_id, group_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/users/{user_id}/groups/{group_id}",
    tag = "groups",
    summary = "Add group to user",
    responses(
        (status = 204, description = "User added to group successfully"),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("user_id" = uuid::Uuid, Path, description = "User ID"),
        ("group_id" = uuid::Uuid, Path, description = "Group ID")
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn add_group_to_user(
    State(state): State<AppState>,
    Path((user_id, group_id)): Path<(UserId, GroupId)>,
    _: RequiresPermission<resource::Users, operation::UpdateAll>,
) -> Result<StatusCode> {
    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Groups::new(&mut pool_conn);
    repo.add_user_to_group(user_id, group_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/users/{user_id}/groups/{group_id}",
    tag = "groups",
    summary = "Remove group from user",
    responses(
        (status = 204, description = "User removed from group successfully"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Relationship not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("user_id" = uuid::Uuid, Path, description = "User ID"),
        ("group_id" = uuid::Uuid, Path, description = "Group ID")
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn remove_group_from_user(
    State(state): State<AppState>,
    Path((user_id, group_id)): Path<(UserId, GroupId)>,
    _: RequiresPermission<resource::Users, operation::UpdateAll>,
) -> Result<StatusCode> {
    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Groups::new(&mut pool_conn);
    repo.remove_user_from_group(user_id, group_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/groups/{group_id}/users",
    tag = "groups",
    summary = "Get group users",
    responses(
        (status = 200, description = "List of users in group", body = Vec<String>),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Group not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("group_id" = uuid::Uuid, Path, description = "Group ID")
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_group_users(
    State(state): State<AppState>,
    Path(group_id): Path<GroupId>,
    _: RequiresPermission<resource::Users, operation::ReadAll>,
) -> Result<Json<Vec<UserId>>> {
    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Groups::new(&mut pool_conn);

    Ok(Json(repo.get_group_users(group_id).await?))
}

#[utoipa::path(
    get,
    path = "/users/{user_id}/groups",
    tag = "groups",
    summary = "Get user groups",
    responses(
        (status = 200, description = "List of groups for user", body = Vec<GroupResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "User not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("user_id" = uuid::Uuid, Path, description = "User ID")
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_user_groups(
    State(state): State<AppState>,
    Path(user_id): Path<UserId>,
    current_user: CurrentUser,
) -> Result<Json<Vec<GroupResponse>>> {
    let can_read_all_users = can_read_all_resources(&current_user, Resource::Users);
    let can_read_own_user = can_read_own_resource(&current_user, Resource::Users, user_id);

    // Allow access if user can either read all users OR read their own user data
    if !can_read_all_users && !can_read_own_user {
        return Err(Error::InsufficientPermissions {
            required: Permission::Any(vec![
                Permission::Allow(Resource::Users, Operation::ReadAll),
                Permission::Allow(Resource::Users, Operation::ReadOwn),
            ]),
            action: Operation::ReadOwn,
            resource: "user groups".to_string(),
        });
    }

    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Groups::new(&mut pool_conn);

    let groups = repo.get_user_groups(user_id).await?;
    Ok(Json(groups.into_iter().map(GroupResponse::from).collect::<Vec<_>>()))
}

// Deployment-group management endpoints

#[utoipa::path(
    post,
    path = "/groups/{group_id}/models/{deployment_id}",
    tag = "models",
    summary = "Grant group access to model",
    responses(
        (status = 204, description = "Group granted access to model successfully"),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("group_id" = uuid::Uuid, Path, description = "Group ID"),
        ("deployment_id" = uuid::Uuid, Path, description = "Deployment ID")
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn add_deployment_to_group(
    State(state): State<AppState>,
    Path((group_id, deployment_id)): Path<(GroupId, DeploymentId)>,
    current_user: RequiresPermission<resource::Groups, operation::UpdateAll>,
) -> Result<StatusCode> {
    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Groups::new(&mut pool_conn);
    repo.add_deployment_to_group(deployment_id, group_id, current_user.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/groups/{group_id}/models/{deployment_id}",
    tag = "models",
    summary = "Revoke group access to model",
    responses(
        (status = 204, description = "Group access to model revoked successfully"),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("group_id" = uuid::Uuid, Path, description = "Group ID"),
        ("deployment_id" = uuid::Uuid, Path, description = "Deployment ID")
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn remove_deployment_from_group(
    State(state): State<AppState>,
    Path((group_id, deployment_id)): Path<(GroupId, DeploymentId)>,
    _: RequiresPermission<resource::Groups, operation::UpdateAll>,
) -> Result<StatusCode> {
    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Groups::new(&mut pool_conn);
    repo.remove_deployment_from_group(deployment_id, group_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/groups/{group_id}/models",
    tag = "groups",
    summary = "Get models accessible by group",
    responses(
        (status = 200, description = "List of models accessible by group", body = Vec<String>),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Group not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("group_id" = uuid::Uuid, Path, description = "Group ID")
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_group_deployments(
    State(state): State<AppState>,
    Path(group_id): Path<GroupId>,
    _: RequiresPermission<resource::Groups, operation::ReadAll>,
) -> Result<Json<Vec<DeploymentId>>> {
    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Groups::new(&mut pool_conn);
    let deployments = repo.get_group_deployments(group_id).await?;
    Ok(Json(deployments))
}

#[utoipa::path(
    get,
    path = "/models/{deployment_id}/groups",
    tag = "models",
    summary = "Get groups with model access",
    responses(
        (status = 200, description = "List of groups with access to model", body = Vec<String>),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Deployment not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("deployment_id" = uuid::Uuid, Path, description = "Deployment ID")
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_deployment_groups(
    State(state): State<AppState>,
    Path(deployment_id): Path<DeploymentId>,
    _: RequiresPermission<resource::Groups, operation::ReadAll>,
) -> Result<Json<Vec<GroupId>>> {
    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Groups::new(&mut pool_conn);
    let groups = repo.get_deployment_groups(deployment_id).await?;
    Ok(Json(groups))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::{
        api::models::{groups::GroupResponse, users::Role},
        db::{
            handlers::{Deployments, Groups, Repository},
            models::{deployments::DeploymentCreateDBRequest, groups::GroupCreateDBRequest},
        },
        test_utils::*,
        types::{DeploymentId, GroupId, UserId},
    };
    use axum::http::StatusCode;
    use serde_json::json;
    use sqlx::PgPool;

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_groups_with_pagination(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Create test groups
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);
        for i in 0..5 {
            let group_create = GroupCreateDBRequest {
                name: format!("Test Group {i}"),
                description: Some(format!("Description for group {i}")),
                created_by: user.id,
            };
            group_repo.create(&group_create).await.expect("Failed to create test group");
        }

        // Test with limit
        let response = app
            .get("/admin/api/v1/groups?limit=3")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let groups: Vec<GroupResponse> = response.json();
        assert_eq!(groups.len(), 3);

        // Test with skip and limit
        let response = app
            .get("/admin/api/v1/groups?skip=2&limit=2")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let groups: Vec<GroupResponse> = response.json();
        assert_eq!(groups.len(), 2);

        // Test skip beyond available groups
        let response = app
            .get("/admin/api/v1/groups?skip=1000&limit=10")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let groups: Vec<GroupResponse> = response.json();
        assert!(groups.is_empty());

        // Test default pagination values (no params)
        let response = app
            .get("/admin/api/v1/groups")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let groups: Vec<GroupResponse> = response.json();
        assert_eq!(groups.len(), 6); // Should return all 6 groups (5 test groups + Everyone group)
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_add_user_to_group(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_admin_user(&pool, Role::PlatformManager).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create a group
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);
        let group_create = GroupCreateDBRequest {
            name: "Test Group".to_string(),
            description: Some("Test group for membership".to_string()),
            created_by: user1.id,
        };
        let group = group_repo.create(&group_create).await.expect("Failed to create test group");

        // Add user2 to the group
        let response = app
            .post(&format!("/admin/api/v1/groups/{}/users/{}", group.id, user2.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status(StatusCode::NO_CONTENT);

        // Verify user is in group by getting group users
        let response = app
            .get(&format!("/admin/api/v1/groups/{}/users", group.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status_ok();
        let user_ids: Vec<UserId> = response.json();
        assert!(user_ids.contains(&user2.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_remove_user_from_group(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_admin_user(&pool, Role::PlatformManager).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create a group
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);
        let group_create = GroupCreateDBRequest {
            name: "Test Group".to_string(),
            description: Some("Test group for membership".to_string()),
            created_by: user1.id,
        };
        let group = group_repo.create(&group_create).await.expect("Failed to create test group");

        // Add user2 to the group first
        let response = app
            .post(&format!("/admin/api/v1/groups/{}/users/{}", group.id, user2.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;
        response.assert_status(StatusCode::NO_CONTENT);

        // Remove user2 from the group
        let response = app
            .delete(&format!("/admin/api/v1/groups/{}/users/{}", group.id, user2.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status(StatusCode::NO_CONTENT);

        // Verify user is no longer in group
        let response = app
            .get(&format!("/admin/api/v1/groups/{}/users", group.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status_ok();
        let user_ids: Vec<UserId> = response.json();
        assert!(!user_ids.contains(&user2.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_group_users(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_admin_user(&pool, Role::PlatformManager).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;
        let user3 = create_test_user(&pool, Role::StandardUser).await;

        // Create a group
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);
        let group_create = GroupCreateDBRequest {
            name: "Test Group".to_string(),
            description: Some("Test group for listing users".to_string()),
            created_by: user1.id,
        };
        let group = group_repo.create(&group_create).await.expect("Failed to create test group");

        // Add users to the group
        app.post(&format!("/admin/api/v1/groups/{}/users/{}", group.id, user2.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await
            .assert_status(StatusCode::NO_CONTENT);

        app.post(&format!("/admin/api/v1/groups/{}/users/{}", group.id, user3.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await
            .assert_status(StatusCode::NO_CONTENT);

        // List group users
        let response = app
            .get(&format!("/admin/api/v1/groups/{}/users", group.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status_ok();
        let user_ids: Vec<UserId> = response.json();
        assert_eq!(user_ids.len(), 2);
        assert!(user_ids.contains(&user2.id));
        assert!(user_ids.contains(&user3.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_user_groups(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Create multiple groups
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);
        let mut group_ids = vec![];

        for i in 0..3 {
            let group_create = GroupCreateDBRequest {
                name: format!("Test Group {i}"),
                description: Some(format!("Test group {i} for user membership")),
                created_by: user.id,
            };
            let group = group_repo.create(&group_create).await.expect("Failed to create test group");
            group_ids.push(group.id);

            // Add user to each group
            app.post(&format!("/admin/api/v1/groups/{}/users/{}", group.id, user.id))
                .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
                .await
                .assert_status(StatusCode::NO_CONTENT);
        }

        // List user's groups
        let response = app
            .get(&format!("/admin/api/v1/users/{}/groups", user.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let groups: Vec<GroupResponse> = response.json();
        assert_eq!(groups.len(), 4); // Should return all 4 groups (3 test groups + Everyone group)

        // Verify all groups are present
        for group_id in group_ids {
            assert!(groups.iter().any(|g| g.id == group_id));
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_duplicate_membership_prevention(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_admin_user(&pool, Role::PlatformManager).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create a group
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);
        let group_create = GroupCreateDBRequest {
            name: "Test Group".to_string(),
            description: Some("Test group for duplicate prevention".to_string()),
            created_by: user1.id,
        };
        let group = group_repo.create(&group_create).await.expect("Failed to create test group");

        // Add user2 to the group
        let response = app
            .post(&format!("/admin/api/v1/groups/{}/users/{}", group.id, user2.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;
        response.assert_status(StatusCode::NO_CONTENT);

        // Try to add user2 again - should succeed but not create duplicate
        let response = app
            .post(&format!("/admin/api/v1/groups/{}/users/{}", group.id, user2.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;
        response.assert_status(StatusCode::NO_CONTENT);

        // Verify user is in group only once
        let response = app
            .get(&format!("/admin/api/v1/groups/{}/users", group.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status_ok();
        let user_ids: Vec<UserId> = response.json();
        let user2_count = user_ids.iter().filter(|&id| *id == user2.id).count();
        assert_eq!(user2_count, 1);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_symmetric_group_user_endpoints(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Create a group
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);
        let group_create = GroupCreateDBRequest {
            name: "Test Group".to_string(),
            description: Some("Test symmetric endpoints".to_string()),
            created_by: user.id,
        };
        let group = group_repo.create(&group_create).await.expect("Failed to create test group");

        // Add group to user (using the /users/{id}/groups/{id} endpoint)
        let response = app
            .post(&format!("/admin/api/v1/users/{}/groups/{}", user.id, group.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;
        response.assert_status(StatusCode::NO_CONTENT);

        // Verify via both endpoints
        let response = app
            .get(&format!("/admin/api/v1/groups/{}/users", group.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;
        response.assert_status_ok();
        let user_ids: Vec<UserId> = response.json();
        assert!(user_ids.contains(&user.id));

        let response = app
            .get(&format!("/admin/api/v1/users/{}/groups", user.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;
        response.assert_status_ok();
        let groups: Vec<GroupResponse> = response.json();
        assert!(groups.iter().any(|g| g.id == group.id));

        // Remove using the /users/{id}/groups/{id} endpoint
        let response = app
            .delete(&format!("/admin/api/v1/users/{}/groups/{}", user.id, group.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;
        response.assert_status(StatusCode::NO_CONTENT);

        // Verify removal via both endpoints
        let response = app
            .get(&format!("/admin/api/v1/groups/{}/users", group.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;
        response.assert_status_ok();
        let user_ids: Vec<UserId> = response.json();
        assert!(!user_ids.contains(&user.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_add_deployment_to_group_api(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Create a group
        let group_create = json!({
            "name": "Test Group",
            "description": "Test group for deployment access"
        });

        let response = app
            .post("/admin/api/v1/groups")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&group_create)
            .await;

        response.assert_status(StatusCode::CREATED);
        let group: GroupResponse = response.json();

        // Get a valid endpoint ID
        let endpoint_id = get_test_endpoint_id(&pool).await;

        // Create a deployment
        let mut pool_conn2 = pool.acquire().await.unwrap();
        let mut deployment_repo = Deployments::new(&mut pool_conn2);
        let mut deployment_create = DeploymentCreateDBRequest::builder()
            .created_by(admin_user.id)
            .model_name("test-model".to_string())
            .alias("test-alias".to_string())
            .build();
        deployment_create.hosted_on = endpoint_id;
        let deployment = deployment_repo
            .create(&deployment_create)
            .await
            .expect("Failed to create test deployment");

        // Add deployment to group via API
        let response = app
            .post(&format!("/admin/api/v1/groups/{}/models/{}", group.id, deployment.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status(StatusCode::NO_CONTENT);

        // Verify deployment is in group
        let response = app
            .get(&format!("/admin/api/v1/groups/{}/models", group.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let deployments: Vec<DeploymentId> = response.json();
        assert!(deployments.contains(&deployment.id));

        // Verify group has access to deployment
        let response = app
            .get(&format!("/admin/api/v1/models/{}/groups", deployment.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let groups: Vec<GroupId> = response.json();
        assert!(groups.contains(&group.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_remove_deployment_from_group_api(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Create a group
        let group_create = json!({
            "name": "Test Group",
            "description": "Test group for deployment access"
        });

        let response = app
            .post("/admin/api/v1/groups")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&group_create)
            .await;

        response.assert_status(StatusCode::CREATED);
        let group: GroupResponse = response.json();

        // Get a valid endpoint ID
        let endpoint_id = get_test_endpoint_id(&pool).await;

        // Create a deployment
        let mut pool_conn2 = pool.acquire().await.unwrap();
        let mut deployment_repo = Deployments::new(&mut pool_conn2);
        let mut deployment_create = DeploymentCreateDBRequest::builder()
            .created_by(admin_user.id)
            .model_name("test-model".to_string())
            .alias("test-alias".to_string())
            .build();
        deployment_create.hosted_on = endpoint_id;
        let deployment = deployment_repo
            .create(&deployment_create)
            .await
            .expect("Failed to create test deployment");

        // Add deployment to group first
        let response = app
            .post(&format!("/admin/api/v1/groups/{}/models/{}", group.id, deployment.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status(StatusCode::NO_CONTENT);

        // Remove deployment from group
        let response = app
            .delete(&format!("/admin/api/v1/groups/{}/models/{}", group.id, deployment.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status(StatusCode::NO_CONTENT);

        // Verify deployment is no longer in group
        let response = app
            .get(&format!("/admin/api/v1/groups/{}/models", group.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let deployments: Vec<DeploymentId> = response.json();
        assert!(!deployments.contains(&deployment.id));

        // Verify group no longer has access to deployment
        let response = app
            .get(&format!("/admin/api/v1/models/{}/groups", deployment.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let groups: Vec<GroupId> = response.json();
        assert!(!groups.contains(&group.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_deployment_group_access_control(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;

        // Create a group
        let group_create = json!({
            "name": "Test Group",
            "description": "Test group for deployment access"
        });

        let response = app
            .post("/admin/api/v1/groups")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&group_create)
            .await;

        response.assert_status(StatusCode::CREATED);
        let group: GroupResponse = response.json();

        // Get a valid endpoint ID
        let endpoint_id = get_test_endpoint_id(&pool).await;

        // Create a deployment
        let mut pool_conn2 = pool.acquire().await.unwrap();
        let mut deployment_repo = Deployments::new(&mut pool_conn2);
        let mut deployment_create = DeploymentCreateDBRequest::builder()
            .created_by(admin_user.id)
            .model_name("test-model".to_string())
            .alias("test-alias".to_string())
            .build();
        deployment_create.hosted_on = endpoint_id;
        let deployment = deployment_repo
            .create(&deployment_create)
            .await
            .expect("Failed to create test deployment");

        // Regular user should not be able to add deployment to group
        let response = app
            .post(&format!("/admin/api/v1/groups/{}/models/{}", group.id, deployment.id))
            .add_header(add_auth_headers(&regular_user).0, add_auth_headers(&regular_user).1)
            .await;

        response.assert_status(StatusCode::FORBIDDEN);

        // Regular user should not be able to list group deployments
        let response = app
            .get(&format!("/admin/api/v1/groups/{}/models", group.id))
            .add_header(add_auth_headers(&regular_user).0, add_auth_headers(&regular_user).1)
            .await;

        response.assert_status(StatusCode::FORBIDDEN);

        // Regular user should not be able to list deployment groups
        let response = app
            .get(&format!("/admin/api/v1/models/{}/groups", deployment.id))
            .add_header(add_auth_headers(&regular_user).0, add_auth_headers(&regular_user).1)
            .await;

        response.assert_status(StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_groups_with_include_parameters(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;

        // Create a group
        let group_create = json!({
            "name": "Test Group",
            "description": "Test group for include parameters"
        });

        let response = app
            .post("/admin/api/v1/groups")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&group_create)
            .await;

        response.assert_status(StatusCode::CREATED);
        let group: GroupResponse = response.json();

        // Add user to group
        app.post(&format!("/admin/api/v1/groups/{}/users/{}", group.id, regular_user.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await
            .assert_status(StatusCode::NO_CONTENT);

        // Get a valid endpoint ID
        let endpoint_id = get_test_endpoint_id(&pool).await;

        // Create a deployment and add to group
        let mut pool_conn2 = pool.acquire().await.unwrap();
        let mut deployment_repo = Deployments::new(&mut pool_conn2);
        let mut deployment_create = DeploymentCreateDBRequest::builder()
            .created_by(admin_user.id)
            .model_name("test-model".to_string())
            .alias("test-alias".to_string())
            .build();
        deployment_create.hosted_on = endpoint_id;
        let deployment = deployment_repo
            .create(&deployment_create)
            .await
            .expect("Failed to create test deployment");

        app.post(&format!("/admin/api/v1/groups/{}/models/{}", group.id, deployment.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await
            .assert_status(StatusCode::NO_CONTENT);

        // Test without include parameters - should not include relationships
        let response = app
            .get("/admin/api/v1/groups")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let groups: Vec<GroupResponse> = response.json();
        let found_group = groups.iter().find(|g| g.id == group.id).expect("Group not found");
        assert!(found_group.users.is_none());
        assert!(found_group.models.is_none());

        // Test with include=users - should include users but not models
        let response = app
            .get("/admin/api/v1/groups?include=users")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let groups: Vec<GroupResponse> = response.json();
        let found_group = groups.iter().find(|g| g.id == group.id).expect("Group not found");
        assert!(found_group.users.is_some());
        assert!(found_group.models.is_none());
        let users = found_group.users.as_ref().unwrap().iter().map(|x| x.id).collect::<HashSet<_>>();
        assert!(users.contains(&regular_user.id));

        // Test with include=models - should include models but not users
        let response = app
            .get("/admin/api/v1/groups?include=models")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let groups: Vec<GroupResponse> = response.json();
        let found_group = groups.iter().find(|g| g.id == group.id).expect("Group not found");
        assert!(found_group.users.is_none());
        assert!(found_group.models.is_some());
        let models = found_group.models.as_ref().unwrap().iter().map(|x| x.id).collect::<HashSet<_>>();
        assert!(models.contains(&deployment.id));

        // Test with include=users,models - should include both
        let response = app
            .get("/admin/api/v1/groups?include=users,models")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let groups: Vec<GroupResponse> = response.json();
        let found_group = groups.iter().find(|g| g.id == group.id).expect("Group not found");
        assert!(found_group.users.is_some());
        assert!(found_group.models.is_some());
        let users = found_group.users.as_ref().unwrap().iter().map(|x| x.id).collect::<HashSet<_>>();
        let models = found_group.models.as_ref().unwrap().iter().map(|x| x.id).collect::<HashSet<_>>();
        assert!(users.contains(&regular_user.id));
        assert!(models.contains(&deployment.id));

        // Test with include=users,models and pagination
        let response = app
            .get("/admin/api/v1/groups?include=users,models&limit=10")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let groups: Vec<GroupResponse> = response.json();
        let found_group = groups.iter().find(|g| g.id == group.id).expect("Group not found");
        assert!(found_group.users.is_some());
        assert!(found_group.models.is_some());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_can_see_other_user_groups(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_user(&pool, Role::PlatformManager).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;

        // Create some groups
        let mut pool_conn = pool.clone().acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);

        let group1_create = GroupCreateDBRequest {
            name: "User Group 1".to_string(),
            description: Some("First group for standard user".to_string()),
            created_by: platform_manager.id,
        };
        let group1 = group_repo.create(&group1_create).await.expect("Failed to create test group");

        let group2_create = GroupCreateDBRequest {
            name: "User Group 2".to_string(),
            description: Some("Second group for standard user".to_string()),
            created_by: platform_manager.id,
        };
        let group2 = group_repo.create(&group2_create).await.expect("Failed to create test group");

        // Add standard_user to both groups
        app.post(&format!("/admin/api/v1/groups/{}/users/{}", group1.id, standard_user.id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await
            .assert_status(StatusCode::NO_CONTENT);

        app.post(&format!("/admin/api/v1/groups/{}/users/{}", group2.id, standard_user.id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await
            .assert_status(StatusCode::NO_CONTENT);

        // Platform manager should be able to see other user's groups (this should succeed)
        let response = app
            .get(&format!("/admin/api/v1/users/{}/groups", standard_user.id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;

        response.assert_status_ok(); // Changed from expecting 403 to expecting 200
        let groups: Vec<GroupResponse> = response.json();

        // Should see both groups plus the Everyone group (3 total)
        assert_eq!(groups.len(), 3, "Platform manager should see all user's groups");
        assert!(groups.iter().any(|g| g.id == group1.id), "Should see group1");
        assert!(groups.iter().any(|g| g.id == group2.id), "Should see group2");

        // Test with a different user to ensure it's not just working for one case
        let another_standard_user = create_test_user(&pool, Role::StandardUser).await;

        // Add to only one group
        app.post(&format!("/admin/api/v1/groups/{}/users/{}", group1.id, another_standard_user.id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await
            .assert_status(StatusCode::NO_CONTENT);

        // Platform manager should see only the groups this user is in
        let response = app
            .get(&format!("/admin/api/v1/users/{}/groups", another_standard_user.id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;

        response.assert_status_ok();
        let groups: Vec<GroupResponse> = response.json();

        // Should see group1 plus the Everyone group (2 total)
        assert_eq!(groups.len(), 2, "Platform manager should see user's actual groups");
        assert!(groups.iter().any(|g| g.id == group1.id), "Should see group1");
        assert!(!groups.iter().any(|g| g.id == group2.id), "Should NOT see group2");

        // Verify non-platform-manager users cannot see other users' groups
        let request_viewer_only = create_test_user(&pool, Role::RequestViewer).await; // RequestViewer without PlatformManager role
        let response = app
            .get(&format!("/admin/api/v1/users/{}/groups", standard_user.id))
            .add_header(add_auth_headers(&request_viewer_only).0, add_auth_headers(&request_viewer_only).1)
            .await;

        response.assert_status(StatusCode::FORBIDDEN); // This should still be forbidden

        // Verify standard users can only see their own groups
        let response = app
            .get(&format!("/admin/api/v1/users/{}/groups", standard_user.id))
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1) // standard_user accessing their own groups
            .await;

        response.assert_status_ok(); // Should work - users can see their own groups

        // But standard users cannot see other users' groups
        let response = app
            .get(&format!("/admin/api/v1/users/{}/groups", another_standard_user.id))
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1) // standard_user accessing another_standard_user's groups
            .await;

        response.assert_status(StatusCode::FORBIDDEN); // This should be forbidden
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_multiple_roles_with_platform_manager_can_see_user_groups(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;

        // Create a user with multiple roles including PlatformManager
        let multi_role_user = create_test_user_with_roles(&pool, vec![Role::PlatformManager, Role::RequestViewer]).await;

        // Create a group and add standard user to it
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);

        let group_create = GroupCreateDBRequest {
            name: "Multi Role Test Group".to_string(),
            description: Some("Group for multi-role user test".to_string()),
            created_by: multi_role_user.id,
        };
        let group = group_repo.create(&group_create).await.expect("Failed to create test group");

        app.post(&format!("/admin/api/v1/groups/{}/users/{}", group.id, standard_user.id))
            .add_header(add_auth_headers(&multi_role_user).0, add_auth_headers(&multi_role_user).1)
            .await
            .assert_status(StatusCode::NO_CONTENT);

        // User with PlatformManager role should be able to see other user's groups
        let response = app
            .get(&format!("/admin/api/v1/users/{}/groups", standard_user.id))
            .add_header(add_auth_headers(&multi_role_user).0, add_auth_headers(&multi_role_user).1)
            .await;

        response.assert_status_ok();
        let groups: Vec<GroupResponse> = response.json();

        assert!(groups.len() >= 2, "Should see user's groups including the new group");
        assert!(groups.iter().any(|g| g.id == group.id), "Should see the created group");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_user_group_access_permissions(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Create a group and add user1 to it
        let group_create = json!({
            "name": "Access Test Group",
            "description": "Testing user group access permissions"
        });

        let response = app
            .post("/admin/api/v1/groups")
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .json(&group_create)
            .await;

        response.assert_status(StatusCode::CREATED);
        let group: GroupResponse = response.json();

        app.post(&format!("/admin/api/v1/groups/{}/users/{}", group.id, user1.id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await
            .assert_status(StatusCode::NO_CONTENT);

        // User1 should be able to see their own groups
        let response = app
            .get(&format!("/admin/api/v1/users/{}/groups", user1.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status_ok();
        let user1_groups: Vec<GroupResponse> = response.json();
        assert!(user1_groups.iter().any(|g| g.id == group.id));

        // User1 should NOT be able to see user2's groups
        let response = app
            .get(&format!("/admin/api/v1/users/{}/groups", user2.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status_forbidden();

        // User2 should be able to see their own groups (but shouldn't see the group they're not in)
        let response = app
            .get(&format!("/admin/api/v1/users/{}/groups", user2.id))
            .add_header(add_auth_headers(&user2).0, add_auth_headers(&user2).1)
            .await;

        response.assert_status_ok();
        let user2_groups: Vec<GroupResponse> = response.json();
        assert!(!user2_groups.iter().any(|g| g.id == group.id));

        // Both users should NOT be able to see group membership lists
        let response = app
            .get(&format!("/admin/api/v1/groups/{}/users", group.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status_forbidden();

        let response = app
            .get(&format!("/admin/api/v1/groups/{}/users", group.id))
            .add_header(add_auth_headers(&user2).0, add_auth_headers(&user2).1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_deployment_group_management_permissions(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;

        // Create group and deployment
        let group_create = json!({
            "name": "Deployment Access Group",
            "description": "Testing deployment-group permissions"
        });

        let response = app
            .post("/admin/api/v1/groups")
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .json(&group_create)
            .await;

        response.assert_status(StatusCode::CREATED);
        let group: GroupResponse = response.json();

        let endpoint_id = get_test_endpoint_id(&pool).await;
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut deployment_repo = Deployments::new(&mut pool_conn);
        let deployment_create = DeploymentCreateDBRequest::builder()
            .created_by(platform_manager.id)
            .model_name("perm-test-model".to_string())
            .alias("perm-test-alias".to_string())
            .hosted_on(endpoint_id)
            .build();
        let deployment = deployment_repo.create(&deployment_create).await.unwrap();

        // Only PlatformManager should be able to add deployment to group
        let response = app
            .post(&format!("/admin/api/v1/groups/{}/models/{}", group.id, deployment.id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;

        response.assert_status(StatusCode::NO_CONTENT);

        // StandardUser should NOT be able to add deployment to group
        let response = app
            .post(&format!("/admin/api/v1/groups/{}/models/{}", group.id, deployment.id))
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;

        response.assert_status_forbidden();

        // RequestViewer should NOT be able to add deployment to group
        let response = app
            .post(&format!("/admin/api/v1/groups/{}/models/{}", group.id, deployment.id))
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;

        response.assert_status_forbidden();

        // Only PlatformManager should be able to remove deployment from group
        let response = app
            .delete(&format!("/admin/api/v1/groups/{}/models/{}", group.id, deployment.id))
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;

        response.assert_status_forbidden();

        let response = app
            .delete(&format!("/admin/api/v1/groups/{}/models/{}", group.id, deployment.id))
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;

        response.assert_status_forbidden();

        // Only PlatformManager should be able to list group deployments
        let response = app
            .get(&format!("/admin/api/v1/groups/{}/models", group.id))
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;

        response.assert_status_forbidden();

        let response = app
            .get(&format!("/admin/api/v1/groups/{}/models", group.id))
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;

        response.assert_status_forbidden();

        // Only PlatformManager should be able to list deployment groups
        let response = app
            .get(&format!("/admin/api/v1/models/{}/groups", deployment.id))
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;

        response.assert_status_forbidden();

        let response = app
            .get(&format!("/admin/api/v1/models/{}/groups", deployment.id))
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;

        response.assert_status_forbidden();

        // PlatformManager should be able to do all operations
        let response = app
            .get(&format!("/admin/api/v1/groups/{}/models", group.id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;

        response.assert_status_ok();

        let response = app
            .get(&format!("/admin/api/v1/models/{}/groups", deployment.id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;

        response.assert_status_ok();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_groups_list_permission_filtering(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;
        let _standard_user = create_test_user(&pool, Role::StandardUser).await;

        // Create a group
        let group_create = json!({
            "name": "Permission Filter Test",
            "description": "Testing permission filtering"
        });

        let response = app
            .post("/admin/api/v1/groups")
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .json(&group_create)
            .await;

        response.assert_status(StatusCode::CREATED);

        // RequestViewer should NOT be able to list all groups (no Groups permissions)
        let response = app
            .get("/admin/api/v1/groups")
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;

        response.assert_status_forbidden(); // Changed expectation

        // RequestViewer should NOT be able to get specific group
        let response = app
            .get("/admin/api/v1/groups/00000000-0000-0000-0000-000000000000")
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;

        response.assert_status_forbidden(); // Changed expectation
    }

    // Add a new test for the intended layered role behavior:
    #[sqlx::test]
    #[test_log::test]
    async fn test_layered_roles_platform_manager_plus_request_viewer(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;

        // User with PlatformManager + RequestViewer should have full access
        let full_admin = create_test_user_with_roles(&pool, vec![Role::PlatformManager, Role::RequestViewer]).await;

        // Should be able to create groups (PlatformManager)
        let group_create = json!({
            "name": "Full Admin Group",
            "description": "Created by PlatformManager + RequestViewer"
        });

        let response = app
            .post("/admin/api/v1/groups")
            .add_header(add_auth_headers(&full_admin).0, add_auth_headers(&full_admin).1)
            .json(&group_create)
            .await;

        response.assert_status(StatusCode::CREATED);

        // Should be able to list groups (PlatformManager)
        let response = app
            .get("/admin/api/v1/groups")
            .add_header(add_auth_headers(&full_admin).0, add_auth_headers(&full_admin).1)
            .await;

        response.assert_status_ok();

        // Should be able to see other users' groups (PlatformManager)
        let response = app
            .get(&format!("/admin/api/v1/users/{}/groups", standard_user.id))
            .add_header(add_auth_headers(&full_admin).0, add_auth_headers(&full_admin).1)
            .await;

        response.assert_status_ok();

        // And should have access to request logs (RequestViewer) - but we'd test this in request handler tests
    }
}
