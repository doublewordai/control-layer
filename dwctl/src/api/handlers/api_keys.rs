//! HTTP handlers for API key management endpoints.

use crate::api::models::api_keys::ListApiKeysQuery;
use crate::{
    AppState,
    api::models::{
        api_keys::{ApiKeyCreate, ApiKeyInfoResponse, ApiKeyResponse},
        pagination::PaginatedResponse,
        users::CurrentUser,
    },
    auth::permissions::{
        can_create_all_resources, can_create_own_resource, can_delete_all_resources, can_delete_own_resource, can_read_all_resources,
        can_read_own_resource, is_org_member,
    },
    db::handlers::{Repository, api_keys::ApiKeyFilter, api_keys::ApiKeys},
    db::models::api_keys::ApiKeyCreateDBRequest,
    errors::{Error, Result},
    types::{ApiKeyId, Operation, Permission, Resource, UserIdOrCurrent},
};
use sqlx_pool_router::PoolProvider;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};
use sqlx::Acquire;

/// Create an API key for the current user or a specified user.
/// This returns `ApiKeyResponse`, which contains the actual API key.
///
/// This should be the only time that the API key is returned in a response.
#[utoipa::path(
    post,
    path = "/users/{user_id}/api-keys",
    tag = "api_keys",
    summary = "Create API key",
    description = "Create an API key for the current user or a specified user",
    params(
        ("user_id" = String, Path, description = "User ID (UUID) or 'current' for current user"),
    ),
    responses(
        (status = 201, description = "API key created successfully", body = ApiKeyResponse),
        (status = 400, description = "Bad request - invalid API key data"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - can only manage own API keys unless admin"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn create_user_api_key<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(user_id): Path<UserIdOrCurrent>,
    current_user: CurrentUser,
    Json(data): Json<ApiKeyCreate>,
) -> Result<(StatusCode, Json<ApiKeyResponse>)> {
    // Validate input data
    if data.name.trim().is_empty() {
        return Err(Error::BadRequest {
            message: "API key name cannot be empty".to_string(),
        });
    }

    let target_user_id = match user_id {
        UserIdOrCurrent::Current(_) => current_user.id,
        UserIdOrCurrent::Id(uuid) => uuid,
    };

    // Check permissions: CreateAll, CreateOwn, or org membership
    let can_create_all = can_create_all_resources(&current_user, Resource::ApiKeys);
    let can_create_own = can_create_own_resource(&current_user, Resource::ApiKeys, target_user_id);

    if !can_create_all && !can_create_own {
        let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
        let member = is_org_member(&current_user, target_user_id, &mut conn)
            .await
            .map_err(Error::Database)?;
        if !member {
            return Err(Error::InsufficientPermissions {
                required: Permission::Any(vec![
                    Permission::Allow(Resource::ApiKeys, Operation::CreateAll),
                    Permission::Allow(Resource::ApiKeys, Operation::CreateOwn),
                ]),
                action: Operation::CreateOwn,
                resource: format!("API keys for user {target_user_id}"),
            });
        }
    }

    // Only PlatformManagers can specify member_id to attribute a key to another org member
    if data.member_id.is_some() && !can_create_all {
        return Err(Error::InsufficientPermissions {
            required: Permission::Granted,
            action: Operation::CreateAll,
            resource: "API keys with member_id (requires PlatformManager)".to_string(),
        });
    }

    // Validate purpose: restrict batch/playground to system use only (purpose defaults to Realtime via serde)
    match &data.purpose {
        crate::db::models::api_keys::ApiKeyPurpose::Batch | crate::db::models::api_keys::ApiKeyPurpose::Playground => {
            return Err(Error::BadRequest {
                message:
                    "Cannot manually create API keys with 'batch' or 'playground' purpose. These are reserved for internal system use."
                        .to_string(),
            });
        }
        _ => {}
    }

    let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;

    // Check if target is an organization by looking at user_type
    let target_is_org = {
        let mut user_repo = crate::db::handlers::Users::new(&mut pool_conn);
        user_repo
            .get_by_id(target_user_id)
            .await?
            .map(|u| u.user_type == "organization")
            .unwrap_or(false)
    };

    // Validate member_id: must be a member of the target org
    if let Some(member_id) = data.member_id {
        if !target_is_org {
            return Err(Error::BadRequest {
                message: "member_id can only be used when creating keys for an organization".to_string(),
            });
        }
        let mut org_repo = crate::db::handlers::Organizations::new(&mut pool_conn);
        let role = org_repo
            .get_user_org_role(member_id, target_user_id)
            .await
            .map_err(Error::Database)?;
        if role.is_none() {
            return Err(Error::BadRequest {
                message: format!("User {member_id} is not a member of organization {target_user_id}"),
            });
        }
    }

    // Determine created_by based on target type:
    // - Organization target: attribute to specified member_id, or current user
    // - Individual target (PM creating on behalf): attribute to the target user
    //   so the key is visible to them
    // - Self: attribute to current user
    let pm_creating_for_other = can_create_all && target_user_id != current_user.id;
    let created_by = if target_is_org {
        data.member_id.unwrap_or(current_user.id)
    } else if pm_creating_for_other {
        target_user_id
    } else {
        current_user.id
    };

    let mut repo = ApiKeys::new(&mut pool_conn);
    let db_request = ApiKeyCreateDBRequest::new(target_user_id, created_by, data);

    let api_key = repo.create(&db_request).await?;
    Ok((StatusCode::CREATED, Json(ApiKeyResponse::from(api_key))))
}

/// List the API keys for the current user or a specified user.
/// This should never contain the actual API key value.
#[utoipa::path(
    get,
    path = "/users/{user_id}/api-keys",
    tag = "api_keys",
    summary = "List API keys",
    description = "List API keys for the current user or a specified user",
    params(
        ("user_id" = String, Path, description = "User ID (UUID) or 'current' for current user"),
        ListApiKeysQuery
    ),
    responses(
        (status = 200, description = "Paginated list of API keys", body = PaginatedResponse<ApiKeyInfoResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - can only view own API keys unless admin"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn list_user_api_keys<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(user_id): Path<UserIdOrCurrent>,
    Query(query): Query<ListApiKeysQuery>,
    // Can't use RequiresPermission here because we need conditional logic for own vs other users
    current_user: CurrentUser,
) -> Result<Json<PaginatedResponse<ApiKeyInfoResponse>>> {
    let target_user_id = match user_id {
        UserIdOrCurrent::Current(_) => current_user.id,
        UserIdOrCurrent::Id(uuid) => uuid,
    };

    // Check permissions: ReadAll, ReadOwn, or org membership
    let can_read_all = can_read_all_resources(&current_user, Resource::ApiKeys);
    let can_read_own = can_read_own_resource(&current_user, Resource::ApiKeys, target_user_id);

    if !can_read_all && !can_read_own {
        let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
        let member = is_org_member(&current_user, target_user_id, &mut conn)
            .await
            .map_err(Error::Database)?;
        if !member {
            return Err(Error::InsufficientPermissions {
                required: Permission::Any(vec![
                    Permission::Allow(Resource::ApiKeys, Operation::ReadAll),
                    Permission::Allow(Resource::ApiKeys, Operation::ReadOwn),
                ]),
                action: Operation::ReadOwn,
                resource: format!("API keys for user {target_user_id}"),
            });
        }
    }

    // PlatformManagers (ReadAll) see all keys for a user; everyone else is scoped to created_by.
    let skip_created_by_filter = can_read_all;

    // Use read replica for this read-only operation
    let mut pool_conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ApiKeys::new(&mut pool_conn);

    // Extract pagination parameters with defaults & validation
    let skip = query.pagination.skip();
    let limit = query.pagination.limit();

    // Scope to keys created by this user, unless they have ReadAll (PlatformManager).
    // PM-created keys on behalf of users have created_by = target_user_id, so the target
    // user can always see them. PMs bypass the filter to see all keys for any user.
    let filter = ApiKeyFilter {
        skip,
        limit,
        user_id: Some(target_user_id),
        created_by: if skip_created_by_filter { None } else { Some(current_user.id) },
    };

    // Get total count and list of items
    let total_count = repo.count(&filter).await?;
    let api_keys = repo.list(&filter).await?;

    let data: Vec<ApiKeyInfoResponse> = api_keys.into_iter().map(ApiKeyInfoResponse::from).collect();

    Ok(Json(PaginatedResponse::new(data, total_count, skip, limit)))
}

/// Get a specific API key for the current user or a specified user.
#[utoipa::path(
    get,
    path = "/users/{user_id}/api-keys/{id}",
    tag = "api_keys",
    summary = "Get API key",
    description = "Get a specific API key for the current user or a specified user",
    params(
        ("user_id" = String, Path, description = "User ID (UUID) or 'current' for current user"),
        ("id" = uuid::Uuid, Path, description = "API key ID to retrieve"),
    ),
    responses(
        (status = 200, description = "API key information", body = ApiKeyInfoResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - can only view own API keys unless admin"),
        (status = 404, description = "API key not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_user_api_key<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((user_id, api_key_id)): Path<(UserIdOrCurrent, ApiKeyId)>,
    // Can't use RequiresPermission here because we need conditional logic for own vs other users
    current_user: CurrentUser,
) -> Result<Json<ApiKeyInfoResponse>> {
    let target_user_id = match user_id {
        UserIdOrCurrent::Current(_) => current_user.id,
        UserIdOrCurrent::Id(uuid) => uuid,
    };

    // Check permissions: ReadAll, ReadOwn, or org membership
    let can_read_all = can_read_all_resources(&current_user, Resource::ApiKeys);
    let can_read_own = can_read_own_resource(&current_user, Resource::ApiKeys, target_user_id);

    if !can_read_all && !can_read_own {
        let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
        let member = is_org_member(&current_user, target_user_id, &mut conn)
            .await
            .map_err(Error::Database)?;
        if !member {
            return Err(Error::InsufficientPermissions {
                required: Permission::Any(vec![
                    Permission::Allow(Resource::ApiKeys, Operation::ReadAll),
                    Permission::Allow(Resource::ApiKeys, Operation::ReadOwn),
                ]),
                action: Operation::ReadOwn,
                resource: format!("API keys for user {target_user_id}"),
            });
        }
    }

    let skip_created_by_filter = can_read_all;

    // Use read replica for this read-only operation
    let mut pool_conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ApiKeys::new(&mut pool_conn);

    // Get the specific API key, verify ownership and created_by visibility
    let api_key = repo
        .get_by_id(api_key_id)
        .await?
        .filter(|key| key.user_id == target_user_id)
        .filter(|key| skip_created_by_filter || key.created_by == current_user.id)
        .ok_or_else(|| Error::NotFound {
            resource: "API key".to_string(),
            id: api_key_id.to_string(),
        })?;

    Ok(Json(ApiKeyInfoResponse::from(api_key)))
}

/// Delete a specific API key for the current user or a specified user.
#[utoipa::path(
    delete,
    path = "/users/{user_id}/api-keys/{id}",
    tag = "api_keys",
    summary = "Delete API key",
    description = "Delete a specific API key for the current user or a specified user",
    params(
        ("user_id" = String, Path, description = "User ID (UUID) or 'current' for current user"),
        ("id" = uuid::Uuid, Path, description = "API key ID to delete"),
    ),
    responses(
        (status = 204, description = "API key deleted successfully"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - can only delete own API keys unless admin"),
        (status = 404, description = "API key not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn delete_user_api_key<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((user_id, api_key_id)): Path<(UserIdOrCurrent, ApiKeyId)>,
    // Can't use RequiresPermission here because we need conditional logic for own vs other users
    current_user: CurrentUser,
) -> Result<StatusCode> {
    let target_user_id = match user_id {
        UserIdOrCurrent::Current(_) => current_user.id,
        UserIdOrCurrent::Id(uuid) => uuid,
    };

    // Check permissions: DeleteAll, DeleteOwn, or org membership
    let can_delete_all = can_delete_all_resources(&current_user, Resource::ApiKeys);
    let can_delete_own = can_delete_own_resource(&current_user, Resource::ApiKeys, target_user_id);

    if !can_delete_all && !can_delete_own {
        let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
        let member = is_org_member(&current_user, target_user_id, &mut conn)
            .await
            .map_err(Error::Database)?;
        if !member {
            return Err(Error::InsufficientPermissions {
                required: Permission::Any(vec![
                    Permission::Allow(Resource::ApiKeys, Operation::DeleteAll),
                    Permission::Allow(Resource::ApiKeys, Operation::DeleteOwn),
                ]),
                action: Operation::DeleteOwn,
                resource: format!("API keys for user {target_user_id}"),
            });
        }
    }

    let skip_created_by_filter = can_delete_all;

    let mut tx = state.db.write().begin().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ApiKeys::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);

    // Check if the API key exists, belongs to the target user, and was created by current user
    repo.get_by_id(api_key_id)
        .await?
        .filter(|key| key.user_id == target_user_id)
        .filter(|key| skip_created_by_filter || key.created_by == current_user.id)
        .ok_or_else(|| Error::NotFound {
            resource: "API key".to_string(),
            id: api_key_id.to_string(),
        })?;

    // Now delete the API key
    repo.delete(api_key_id).await?;
    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use crate::api::models::api_keys::{ApiKeyInfoResponse, ApiKeyResponse};
    use crate::api::models::pagination::PaginatedResponse;
    use crate::api::models::users::Role;
    use crate::test::utils::*;
    use serde_json::json;
    use sqlx::PgPool;

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_api_key_for_self(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let api_key_data = json!({
            "name": "Test API Key",
            "description": "A test API key",
            "purpose": "realtime"
        });

        let response = app
            .post("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .json(&api_key_data)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let api_key: ApiKeyResponse = response.json();
        assert_eq!(api_key.name, "Test API Key");
        assert_eq!(api_key.description, Some("A test API key".to_string()));
        assert!(api_key.key.starts_with("sk-"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_api_key_for_other_user_as_admin(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, regular_user.id, group.id).await;

        let api_key_data = json!({
            "name": "Admin Created Key",
            "description": "Created by admin for user",
            "purpose": "realtime"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", regular_user.id))
            .add_header(&add_auth_headers(&admin_user)[0].0, &add_auth_headers(&admin_user)[0].1)
            .add_header(&add_auth_headers(&admin_user)[1].0, &add_auth_headers(&admin_user)[1].1)
            .json(&api_key_data)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let api_key: ApiKeyResponse = response.json();
        assert_eq!(api_key.name, "Admin Created Key");
        assert!(api_key.key.starts_with("sk-"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_api_key_for_other_user_as_non_admin_forbidden(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        let api_key_data = json!({
            "name": "Forbidden Key",
            "description": "This should not work",
            "purpose": "realtime"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", user2.id))
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .json(&api_key_data)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_user_api_keys(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        let api_key = create_test_api_key_for_user(&pool, user.id).await;

        let response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 1);
        assert_eq!(paginated.total_count, 1);
        assert_eq!(paginated.data[0].name, api_key.name);
    }

    // Add new pagination test for the handler
    #[sqlx::test]
    #[test_log::test]
    async fn test_list_user_api_keys_with_pagination_query_params(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create multiple API keys
        for i in 1..=5 {
            let api_key_data = json!({
                "name": format!("Test API Key {}", i),
                "description": format!("Description for key {}", i),
                "purpose": "realtime"
            });

            app.post("/admin/api/v1/users/current/api-keys")
                .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
                .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
                .json(&api_key_data)
                .await
                .assert_status(axum::http::StatusCode::CREATED);
        }

        // Test with pagination parameters
        let response = app
            .get("/admin/api/v1/users/current/api-keys?skip=1&limit=2")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 2, "Should return exactly 2 items with limit=2");
        assert_eq!(paginated.total_count, 5, "Total count should be 5");
        assert_eq!(paginated.skip, 1);
        assert_eq!(paginated.limit, 2);

        // Test with no pagination parameters (should use defaults)
        let response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 5, "Should return all items with default pagination");
        assert_eq!(paginated.total_count, 5);

        // Test with large limit (should be capped)
        let response = app
            .get("/admin/api/v1/users/current/api-keys?limit=9999")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 5, "Should return all items even with large limit");
        assert_eq!(paginated.total_count, 5);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_user_api_key_for_self(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        let api_key = create_test_api_key_for_user(&pool, user.id).await;

        let response = app
            .delete(&format!("/admin/api/v1/users/current/api-keys/{}", api_key.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::NO_CONTENT);

        // Verify the API key was deleted by trying to list them
        let list_response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        list_response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = list_response.json();
        assert_eq!(paginated.data.len(), 0);
        assert_eq!(paginated.total_count, 0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_user_api_key_for_other_user_as_admin(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, regular_user.id, group.id).await;
        let api_key = create_test_api_key_for_user(&pool, regular_user.id).await;

        let response = app
            .delete(&format!("/admin/api/v1/users/{}/api-keys/{}", regular_user.id, api_key.id))
            .add_header(&add_auth_headers(&admin_user)[0].0, &add_auth_headers(&admin_user)[0].1)
            .add_header(&add_auth_headers(&admin_user)[1].0, &add_auth_headers(&admin_user)[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::NO_CONTENT);

        // Verify the API key was deleted
        let list_response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", regular_user.id))
            .add_header(&add_auth_headers(&admin_user)[0].0, &add_auth_headers(&admin_user)[0].1)
            .add_header(&add_auth_headers(&admin_user)[1].0, &add_auth_headers(&admin_user)[1].1)
            .await;

        list_response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = list_response.json();
        assert_eq!(paginated.data.len(), 0);
        assert_eq!(paginated.total_count, 0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_user_api_key_for_other_user_as_non_admin_forbidden(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user2.id, group.id).await;
        let api_key = create_test_api_key_for_user(&pool, user2.id).await;

        let response = app
            .delete(&format!("/admin/api/v1/users/{}/api-keys/{}", user2.id, api_key.id))
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .await;

        response.assert_status_forbidden();

        // Verify the API key still exists
        let list_response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", user2.id))
            .add_header(&add_auth_headers(&user2)[0].0, &add_auth_headers(&user2)[0].1)
            .add_header(&add_auth_headers(&user2)[1].0, &add_auth_headers(&user2)[1].1)
            .await;

        list_response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = list_response.json();
        assert_eq!(paginated.data.len(), 1);
        assert_eq!(paginated.total_count, 1);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_nonexistent_api_key_returns_not_found(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        let fake_api_key_id = uuid::Uuid::new_v4();

        let response = app
            .delete(&format!("/admin/api/v1/users/current/api-keys/{fake_api_key_id}"))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_api_key_belonging_to_different_user_returns_not_found(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user1.id, group.id).await;
        add_user_to_group(&pool, user2.id, group.id).await;

        // Create API key for user2
        let api_key = create_test_api_key_for_user(&pool, user2.id).await;

        // Try to delete user2's API key as user1 (using current endpoint)
        let response = app
            .delete(&format!("/admin/api/v1/users/current/api-keys/{}", api_key.id))
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .await;

        response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_api_keys_for_other_user_as_admin(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, regular_user.id, group.id).await;

        // Create multiple API keys for the regular user
        let api_key1 = create_test_api_key_for_user(&pool, regular_user.id).await;
        let api_key2 = create_test_api_key_for_user(&pool, regular_user.id).await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", regular_user.id))
            .add_header(&add_auth_headers(&admin_user)[0].0, &add_auth_headers(&admin_user)[0].1)
            .add_header(&add_auth_headers(&admin_user)[1].0, &add_auth_headers(&admin_user)[1].1)
            .await;

        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 2);
        assert_eq!(paginated.total_count, 2);

        // Verify we got the correct API keys
        let returned_ids: Vec<_> = paginated.data.iter().map(|k| k.id).collect();
        assert!(returned_ids.contains(&api_key1.id));
        assert!(returned_ids.contains(&api_key2.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_api_keys_for_other_user_as_non_admin_forbidden(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user2.id, group.id).await;
        create_test_api_key_for_user(&pool, user2.id).await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", user2.id))
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_api_key_for_other_user_as_admin(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, regular_user.id, group.id).await;
        let api_key = create_test_api_key_for_user(&pool, regular_user.id).await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys/{}", regular_user.id, api_key.id))
            .add_header(&add_auth_headers(&admin_user)[0].0, &add_auth_headers(&admin_user)[0].1)
            .add_header(&add_auth_headers(&admin_user)[1].0, &add_auth_headers(&admin_user)[1].1)
            .await;

        response.assert_status_ok();
        let returned_key: ApiKeyInfoResponse = response.json();
        assert_eq!(returned_key.id, api_key.id);
        assert_eq!(returned_key.name, api_key.name);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_api_key_for_other_user_as_non_admin_forbidden(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user2.id, group.id).await;
        let api_key = create_test_api_key_for_user(&pool, user2.id).await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys/{}", user2.id, api_key.id))
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_request_viewer_api_key_permissions(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        // Note: create_test_user automatically adds StandardUser role
        // So request_viewer actually has [RequestViewer, StandardUser] roles
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;
        let other_user = create_test_user(&pool, Role::StandardUser).await;

        // RequestViewer (with StandardUser) CAN create API keys for themselves
        let api_key_data = json!({
            "name": "RequestViewer Key",
            "description": "Should work - StandardUser can manage own keys",
            "purpose": "realtime"
        });

        let response = app
            .post("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&request_viewer)[0].0, &add_auth_headers(&request_viewer)[0].1)
            .add_header(&add_auth_headers(&request_viewer)[1].0, &add_auth_headers(&request_viewer)[1].1)
            .json(&api_key_data)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);

        // RequestViewer (with StandardUser) CAN list their own API keys
        let response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&request_viewer)[0].0, &add_auth_headers(&request_viewer)[0].1)
            .add_header(&add_auth_headers(&request_viewer)[1].0, &add_auth_headers(&request_viewer)[1].1)
            .await;

        response.assert_status_ok();

        // RequestViewer should NOT be able to list other users' API keys (no PlatformManager role)
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", other_user.id))
            .add_header(&add_auth_headers(&request_viewer)[0].0, &add_auth_headers(&request_viewer)[0].1)
            .add_header(&add_auth_headers(&request_viewer)[1].0, &add_auth_headers(&request_viewer)[1].1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_multi_role_user_api_key_permissions(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;

        // Create a user with both StandardUser and RequestViewer roles
        let multi_role_user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::RequestViewer]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, multi_role_user.id, group.id).await;

        // Should be able to create API keys (from StandardUser role)
        let api_key_data = json!({
            "name": "Multi Role Key",
            "description": "Should work due to StandardUser role",
            "purpose": "realtime"
        });

        let response = app
            .post("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&multi_role_user)[0].0, &add_auth_headers(&multi_role_user)[0].1)
            .add_header(&add_auth_headers(&multi_role_user)[1].0, &add_auth_headers(&multi_role_user)[1].1)
            .json(&api_key_data)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);

        // Should be able to list their own API keys (from StandardUser role)
        let response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&multi_role_user)[0].0, &add_auth_headers(&multi_role_user)[0].1)
            .add_header(&add_auth_headers(&multi_role_user)[1].0, &add_auth_headers(&multi_role_user)[1].1)
            .await;

        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 1);
        assert_eq!(paginated.total_count, 1);
        assert_eq!(paginated.data[0].name, "Multi Role Key");

        // Should be able to get specific API key
        let api_key_id = paginated.data[0].id;
        let response = app
            .get(&format!("/admin/api/v1/users/current/api-keys/{api_key_id}"))
            .add_header(&add_auth_headers(&multi_role_user)[0].0, &add_auth_headers(&multi_role_user)[0].1)
            .add_header(&add_auth_headers(&multi_role_user)[1].0, &add_auth_headers(&multi_role_user)[1].1)
            .await;

        response.assert_status_ok();

        // Should be able to delete their own API keys
        let response = app
            .delete(&format!("/admin/api/v1/users/current/api-keys/{api_key_id}"))
            .add_header(&add_auth_headers(&multi_role_user)[0].0, &add_auth_headers(&multi_role_user)[0].1)
            .add_header(&add_auth_headers(&multi_role_user)[1].0, &add_auth_headers(&multi_role_user)[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::NO_CONTENT);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_full_api_key_access(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_user(&pool, Role::PlatformManager).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, standard_user.id, group.id).await;

        // Platform manager should be able to create API keys for other users
        let api_key_data = json!({
            "name": "Manager Created Key",
            "description": "Created by platform manager",
            "purpose": "realtime"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", standard_user.id))
            .add_header(&add_auth_headers(&platform_manager)[0].0, &add_auth_headers(&platform_manager)[0].1)
            .add_header(&add_auth_headers(&platform_manager)[1].0, &add_auth_headers(&platform_manager)[1].1)
            .json(&api_key_data)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);

        // Platform manager should be able to list all users' API keys
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", standard_user.id))
            .add_header(&add_auth_headers(&platform_manager)[0].0, &add_auth_headers(&platform_manager)[0].1)
            .add_header(&add_auth_headers(&platform_manager)[1].0, &add_auth_headers(&platform_manager)[1].1)
            .await;

        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 1);
        assert_eq!(paginated.total_count, 1);

        // Platform manager should be able to get specific API keys for other users
        let api_key_id = paginated.data[0].id;
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys/{}", standard_user.id, api_key_id))
            .add_header(&add_auth_headers(&platform_manager)[0].0, &add_auth_headers(&platform_manager)[0].1)
            .add_header(&add_auth_headers(&platform_manager)[1].0, &add_auth_headers(&platform_manager)[1].1)
            .await;

        response.assert_status_ok();

        // The target user can also see the key created on their behalf
        let response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&standard_user)[0].0, &add_auth_headers(&standard_user)[0].1)
            .add_header(&add_auth_headers(&standard_user)[1].0, &add_auth_headers(&standard_user)[1].1)
            .await;

        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 1);
        assert_eq!(paginated.data[0].name, "Manager Created Key");

        // Platform manager should be able to delete other users' API keys
        let response = app
            .delete(&format!("/admin/api/v1/users/{}/api-keys/{}", standard_user.id, api_key_id))
            .add_header(&add_auth_headers(&platform_manager)[0].0, &add_auth_headers(&platform_manager)[0].1)
            .add_header(&add_auth_headers(&platform_manager)[1].0, &add_auth_headers(&platform_manager)[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::NO_CONTENT);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_api_key_isolation_between_users(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user1.id, group.id).await;
        add_user_to_group(&pool, user2.id, group.id).await;

        // Create API keys for both users
        let api_key1 = create_test_api_key_for_user(&pool, user1.id).await;
        let api_key2 = create_test_api_key_for_user(&pool, user2.id).await;

        // User1 should only see their own API keys
        let response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .await;

        response.assert_status_ok();
        let user1_paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(user1_paginated.data.len(), 1);
        assert_eq!(user1_paginated.total_count, 1);
        assert_eq!(user1_paginated.data[0].id, api_key1.id);

        // User2 should only see their own API keys
        let response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&user2)[0].0, &add_auth_headers(&user2)[0].1)
            .add_header(&add_auth_headers(&user2)[1].0, &add_auth_headers(&user2)[1].1)
            .await;

        response.assert_status_ok();
        let user2_paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(user2_paginated.data.len(), 1);
        assert_eq!(user2_paginated.total_count, 1);
        assert_eq!(user2_paginated.data[0].id, api_key2.id);

        // User1 should not be able to access user2's specific API key
        let response = app
            .get(&format!("/admin/api/v1/users/current/api-keys/{}", api_key2.id))
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .await;

        response.assert_status_not_found(); // 404 because the key doesn't belong to user1
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_error_messages_are_user_friendly(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Try to access another user's API keys - should get user-friendly error
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", user2.id))
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .await;

        response.assert_status_forbidden();
        let body = response.text();

        // Should show generic "Read" not "ReadAll" or "ReadOwn"
        assert!(body.contains("Insufficient permissions to Read"));
        assert!(!body.contains("ReadAll"));
        assert!(!body.contains("ReadOwn"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_specific_api_key_for_self(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        let api_key = create_test_api_key_for_user(&pool, user.id).await;

        let response = app
            .get(&format!("/admin/api/v1/users/current/api-keys/{}", api_key.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let returned_key: ApiKeyInfoResponse = response.json();
        assert_eq!(returned_key.id, api_key.id);
        assert_eq!(returned_key.name, api_key.name);
        assert_eq!(returned_key.description, api_key.description);
        // ApiKeyInfoResponse intentionally does not have a key field (security feature)
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_api_key_creation_returns_key_value_only_once(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create API key - should return the actual key value
        let api_key_data = json!({
            "name": "Test Key for Security",
            "description": "Testing key exposure",
            "purpose": "realtime"
        });

        let create_response = app
            .post("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .json(&api_key_data)
            .await;

        create_response.assert_status(axum::http::StatusCode::CREATED);
        let created_key: ApiKeyResponse = create_response.json();

        // Should have the actual key value
        assert!(created_key.key.starts_with("sk-"));
        assert!(created_key.key.len() > 10);

        // List API keys - should NOT return key values (uses ApiKeyInfoResponse)
        let list_response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        list_response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = list_response.json();
        assert_eq!(paginated.data.len(), 1);
        assert_eq!(paginated.total_count, 1);

        // ApiKeyInfoResponse doesn't have a key field - this is the security feature

        // Get specific API key - should NOT return key value (uses ApiKeyInfoResponse)
        let get_response = app
            .get(&format!("/admin/api/v1/users/current/api-keys/{}", created_key.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        get_response.assert_status_ok();
        let retrieved_key: ApiKeyInfoResponse = get_response.json();

        // ApiKeyInfoResponse doesn't have a key field - this is the security feature
        assert_eq!(retrieved_key.id, created_key.id);
        assert_eq!(retrieved_key.name, created_key.name);
    }

    // ── Organization API key tests ────────────────────────────────────────

    #[sqlx::test]
    #[test_log::test]
    async fn test_org_member_can_create_api_key(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let bob = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;
        add_org_member(&pool, org.id, bob.id, "member").await;

        // Bob (member, not admin) creates a key for the org
        let api_key_data = json!({
            "name": "Bob Org Key",
            "description": "Created by member",
            "purpose": "realtime"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&bob)[0].0, &add_auth_headers(&bob)[0].1)
            .add_header(&add_auth_headers(&bob)[1].0, &add_auth_headers(&bob)[1].1)
            .json(&api_key_data)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let api_key: ApiKeyResponse = response.json();
        assert_eq!(api_key.name, "Bob Org Key");
        assert!(api_key.key.starts_with("sk-"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_non_member_cannot_create_org_api_key(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let outsider = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;

        // Outsider (not a member) tries to create a key for the org
        let api_key_data = json!({
            "name": "Outsider Key",
            "purpose": "realtime"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&outsider)[0].0, &add_auth_headers(&outsider)[0].1)
            .add_header(&add_auth_headers(&outsider)[1].0, &add_auth_headers(&outsider)[1].1)
            .json(&api_key_data)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_org_member_list_only_own_keys(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let bob = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;
        add_org_member(&pool, org.id, bob.id, "member").await;

        // Alice (owner) creates a key for the org
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&alice)[0].0, &add_auth_headers(&alice)[0].1)
            .add_header(&add_auth_headers(&alice)[1].0, &add_auth_headers(&alice)[1].1)
            .json(&json!({"name": "Alice Key", "purpose": "realtime"}))
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);

        // Bob (member) creates a key for the org
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&bob)[0].0, &add_auth_headers(&bob)[0].1)
            .add_header(&add_auth_headers(&bob)[1].0, &add_auth_headers(&bob)[1].1)
            .json(&json!({"name": "Bob Key", "purpose": "realtime"}))
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);

        // Alice lists org keys → should only see her own key
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&alice)[0].0, &add_auth_headers(&alice)[0].1)
            .add_header(&add_auth_headers(&alice)[1].0, &add_auth_headers(&alice)[1].1)
            .await;
        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 1);
        assert_eq!(paginated.data[0].name, "Alice Key");

        // Bob lists org keys → should only see his own key
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&bob)[0].0, &add_auth_headers(&bob)[0].1)
            .add_header(&add_auth_headers(&bob)[1].0, &add_auth_headers(&bob)[1].1)
            .await;
        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 1);
        assert_eq!(paginated.data[0].name, "Bob Key");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_org_member_cannot_get_other_members_key(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let bob = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;
        add_org_member(&pool, org.id, bob.id, "member").await;

        // Bob creates a key
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&bob)[0].0, &add_auth_headers(&bob)[0].1)
            .add_header(&add_auth_headers(&bob)[1].0, &add_auth_headers(&bob)[1].1)
            .json(&json!({"name": "Bob Key", "purpose": "realtime"}))
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);
        let bob_key: ApiKeyResponse = response.json();

        // Alice tries to get Bob's key by ID → should get 404
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys/{}", org.id, bob_key.id))
            .add_header(&add_auth_headers(&alice)[0].0, &add_auth_headers(&alice)[0].1)
            .add_header(&add_auth_headers(&alice)[1].0, &add_auth_headers(&alice)[1].1)
            .await;
        response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_org_member_cannot_delete_other_members_key(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let bob = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;
        add_org_member(&pool, org.id, bob.id, "member").await;

        // Bob creates a key
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&bob)[0].0, &add_auth_headers(&bob)[0].1)
            .add_header(&add_auth_headers(&bob)[1].0, &add_auth_headers(&bob)[1].1)
            .json(&json!({"name": "Bob Key", "purpose": "realtime"}))
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);
        let bob_key: ApiKeyResponse = response.json();

        // Alice tries to delete Bob's key → should get 404
        let response = app
            .delete(&format!("/admin/api/v1/users/{}/api-keys/{}", org.id, bob_key.id))
            .add_header(&add_auth_headers(&alice)[0].0, &add_auth_headers(&alice)[0].1)
            .add_header(&add_auth_headers(&alice)[1].0, &add_auth_headers(&alice)[1].1)
            .await;
        response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_org_member_can_delete_own_key(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let bob = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;
        add_org_member(&pool, org.id, bob.id, "member").await;

        // Bob creates a key
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&bob)[0].0, &add_auth_headers(&bob)[0].1)
            .add_header(&add_auth_headers(&bob)[1].0, &add_auth_headers(&bob)[1].1)
            .json(&json!({"name": "Bob Key", "purpose": "realtime"}))
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);
        let bob_key: ApiKeyResponse = response.json();

        // Bob deletes his own key → should succeed
        let response = app
            .delete(&format!("/admin/api/v1/users/{}/api-keys/{}", org.id, bob_key.id))
            .add_header(&add_auth_headers(&bob)[0].0, &add_auth_headers(&bob)[0].1)
            .add_header(&add_auth_headers(&bob)[1].0, &add_auth_headers(&bob)[1].1)
            .await;
        response.assert_status(axum::http::StatusCode::NO_CONTENT);

        // Verify key is gone from Bob's list
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&bob)[0].0, &add_auth_headers(&bob)[0].1)
            .add_header(&add_auth_headers(&bob)[1].0, &add_auth_headers(&bob)[1].1)
            .await;
        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_sees_all_org_keys(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let bob = create_test_user(&pool, Role::StandardUser).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let org = create_test_org(&pool, alice.id).await;
        add_org_member(&pool, org.id, bob.id, "member").await;

        // Alice creates a key
        app.post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&alice)[0].0, &add_auth_headers(&alice)[0].1)
            .add_header(&add_auth_headers(&alice)[1].0, &add_auth_headers(&alice)[1].1)
            .json(&json!({"name": "Alice Key", "purpose": "realtime"}))
            .await
            .assert_status(axum::http::StatusCode::CREATED);

        // Bob creates a key
        app.post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&bob)[0].0, &add_auth_headers(&bob)[0].1)
            .add_header(&add_auth_headers(&bob)[1].0, &add_auth_headers(&bob)[1].1)
            .json(&json!({"name": "Bob Key", "purpose": "realtime"}))
            .await
            .assert_status(axum::http::StatusCode::CREATED);

        // PlatformManager lists org keys → should see all keys (bypasses created_by filter)
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&pm)[0].0, &add_auth_headers(&pm)[0].1)
            .add_header(&add_auth_headers(&pm)[1].0, &add_auth_headers(&pm)[1].1)
            .await;
        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 2);
    }

    // ── created_by attribution tests ──────────────────────────────────────

    #[sqlx::test]
    #[test_log::test]
    async fn test_org_member_key_created_by_is_member_not_org(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;

        // Alice (org owner) creates a key for the org
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&alice)[0].0, &add_auth_headers(&alice)[0].1)
            .add_header(&add_auth_headers(&alice)[1].0, &add_auth_headers(&alice)[1].1)
            .json(&json!({"name": "Alice Org Key", "purpose": "realtime"}))
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);
        let key: ApiKeyResponse = response.json();

        // created_by should be Alice, not the org
        assert_eq!(key.created_by, alice.id, "created_by should be the member, not the org");
        assert_eq!(key.user_id, org.id, "user_id should be the org");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_pm_org_member_key_created_by_is_pm_not_org(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let org = create_test_org(&pool, pm.id).await;

        // PM who is also an org owner creates a key for the org
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&pm)[0].0, &add_auth_headers(&pm)[0].1)
            .add_header(&add_auth_headers(&pm)[1].0, &add_auth_headers(&pm)[1].1)
            .json(&json!({"name": "PM Org Key", "purpose": "realtime"}))
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);
        let key: ApiKeyResponse = response.json();

        // created_by should be the PM, not the org — this is the original bug regression test
        assert_eq!(key.created_by, pm.id, "created_by should be the PM, not the org");
        assert_eq!(key.user_id, org.id, "user_id should be the org");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_pm_creates_org_key_with_member_id(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;

        // PM creates a key for the org attributed to Alice via member_id
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&pm)[0].0, &add_auth_headers(&pm)[0].1)
            .add_header(&add_auth_headers(&pm)[1].0, &add_auth_headers(&pm)[1].1)
            .json(&json!({"name": "Attributed Key", "purpose": "realtime", "member_id": alice.id}))
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);
        let key: ApiKeyResponse = response.json();

        // created_by should be Alice (the specified member), not the PM
        assert_eq!(key.created_by, alice.id, "created_by should be the specified member_id");
        assert_eq!(key.user_id, org.id, "user_id should be the org");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_pm_member_id_rejected_for_non_org_target(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, alice.id, group.id).await;

        // PM tries to use member_id when creating a key for an individual user
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", alice.id))
            .add_header(&add_auth_headers(&pm)[0].0, &add_auth_headers(&pm)[0].1)
            .add_header(&add_auth_headers(&pm)[1].0, &add_auth_headers(&pm)[1].1)
            .json(&json!({"name": "Bad Key", "purpose": "realtime", "member_id": pm.id}))
            .await;
        response.assert_status_bad_request();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_pm_member_id_rejected_for_non_member(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let outsider = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;

        // PM tries to attribute key to a user who is not an org member
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&pm)[0].0, &add_auth_headers(&pm)[0].1)
            .add_header(&add_auth_headers(&pm)[1].0, &add_auth_headers(&pm)[1].1)
            .json(&json!({"name": "Bad Key", "purpose": "realtime", "member_id": outsider.id}))
            .await;
        response.assert_status_bad_request();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_non_pm_cannot_use_member_id(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let bob = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;
        add_org_member(&pool, org.id, bob.id, "member").await;

        // Alice (org owner, not a PM) tries to use member_id
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&alice)[0].0, &add_auth_headers(&alice)[0].1)
            .add_header(&add_auth_headers(&alice)[1].0, &add_auth_headers(&alice)[1].1)
            .json(&json!({"name": "Bad Key", "purpose": "realtime", "member_id": bob.id}))
            .await;
        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_pm_creates_key_for_individual_user_created_by_is_target(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, alice.id, group.id).await;

        // PM creates a personal key for Alice (not an org)
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", alice.id))
            .add_header(&add_auth_headers(&pm)[0].0, &add_auth_headers(&pm)[0].1)
            .add_header(&add_auth_headers(&pm)[1].0, &add_auth_headers(&pm)[1].1)
            .json(&json!({"name": "PM For Alice", "purpose": "realtime"}))
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);
        let key: ApiKeyResponse = response.json();

        // created_by should be Alice (the target individual) so she can see it
        assert_eq!(key.created_by, alice.id, "created_by should be the target user for individual keys");
        assert_eq!(key.user_id, alice.id, "user_id should be the target user");
    }
}
