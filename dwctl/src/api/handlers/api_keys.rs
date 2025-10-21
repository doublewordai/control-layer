use crate::api::models::api_keys::ListApiKeysQuery;
use crate::{
    api::models::{
        api_keys::{ApiKeyCreate, ApiKeyInfoResponse, ApiKeyResponse},
        users::CurrentUser,
    },
    auth::permissions::{
        can_create_all_resources, can_create_own_resource, can_delete_all_resources, can_delete_own_resource, can_read_all_resources,
        can_read_own_resource,
    },
    db::handlers::{api_keys::ApiKeyFilter, api_keys::ApiKeys, Repository},
    db::models::api_keys::ApiKeyCreateDBRequest,
    errors::{Error, Result},
    types::{ApiKeyId, Operation, Permission, Resource, UserIdOrCurrent},
    AppState,
};
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
        ("X-Doubleword-User" = [])
    )
)]
pub async fn create_user_api_key(
    State(state): State<AppState>,
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
        UserIdOrCurrent::Current(_) => {
            // For /current, verify they have permission to create their own API keys
            if !can_create_own_resource(&current_user, Resource::ApiKeys, current_user.id) {
                return Err(Error::InsufficientPermissions {
                    required: Permission::Allow(Resource::ApiKeys, Operation::CreateOwn),
                    action: Operation::CreateOwn,
                    resource: "API keys for current user".to_string(),
                });
            }
            current_user.id
        }
        UserIdOrCurrent::Id(uuid) => {
            let can_create_all_api_keys = can_create_all_resources(&current_user, Resource::ApiKeys);
            let can_create_own_api_keys = can_create_own_resource(&current_user, Resource::ApiKeys, uuid);

            // Allow creation if user can create all API keys OR create their own API keys
            if !can_create_all_api_keys && !can_create_own_api_keys {
                return Err(Error::InsufficientPermissions {
                    required: Permission::Any(vec![
                        Permission::Allow(Resource::ApiKeys, Operation::CreateAll),
                        Permission::Allow(Resource::ApiKeys, Operation::CreateOwn),
                    ]),
                    action: Operation::CreateOwn,
                    resource: format!("API keys for user {uuid}"),
                });
            }
            uuid
        }
    };

    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ApiKeys::new(&mut pool_conn);
    let db_request = ApiKeyCreateDBRequest::new(target_user_id, data);

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
        (status = 200, description = "List of API keys", body = [ApiKeyInfoResponse]),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - can only view own API keys unless admin"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn list_user_api_keys(
    State(state): State<AppState>,
    Path(user_id): Path<UserIdOrCurrent>,
    Query(query): Query<ListApiKeysQuery>,
    // Can't use RequiresPermission here because we need conditional logic for own vs other users
    current_user: CurrentUser,
) -> Result<Json<Vec<ApiKeyInfoResponse>>> {
    let target_user_id = match user_id {
        UserIdOrCurrent::Current(_) => {
            // Even for /current, verify they have permission to read their own API keys
            if !can_read_own_resource(&current_user, Resource::ApiKeys, current_user.id) {
                return Err(Error::InsufficientPermissions {
                    required: Permission::Allow(Resource::ApiKeys, Operation::ReadOwn),
                    action: Operation::ReadOwn,
                    resource: "API keys for current user".to_string(),
                });
            }
            current_user.id
        }
        UserIdOrCurrent::Id(uuid) => {
            let can_read_all_api_keys = can_read_all_resources(&current_user, Resource::ApiKeys);
            let can_read_own_api_keys = can_read_own_resource(&current_user, Resource::ApiKeys, uuid);

            // Allow access if user can read all API keys OR read their own API keys
            if !can_read_all_api_keys && !can_read_own_api_keys {
                return Err(Error::InsufficientPermissions {
                    required: Permission::Any(vec![
                        Permission::Allow(Resource::ApiKeys, Operation::ReadAll),
                        Permission::Allow(Resource::ApiKeys, Operation::ReadOwn),
                    ]),
                    action: Operation::ReadOwn,
                    resource: format!("API keys for user {uuid}"),
                });
            }
            uuid
        }
    };

    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ApiKeys::new(&mut pool_conn);

    // Extract pagination parameters with defaults & validation
    let skip = query.skip.unwrap_or(0);
    let limit = query.limit.unwrap_or(100).min(1000);

    let filter = ApiKeyFilter {
        skip,
        limit,
        user_id: Some(target_user_id),
    };

    let api_keys = repo.list(&filter).await?;
    Ok(Json(api_keys.into_iter().map(ApiKeyInfoResponse::from).collect()))
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
        ("X-Doubleword-User" = [])
    )
)]
pub async fn get_user_api_key(
    State(state): State<AppState>,
    Path((user_id, api_key_id)): Path<(UserIdOrCurrent, ApiKeyId)>,
    // Can't use RequiresPermission here because we need conditional logic for own vs other users
    current_user: CurrentUser,
) -> Result<Json<ApiKeyInfoResponse>> {
    let target_user_id = match user_id {
        UserIdOrCurrent::Current(_) => {
            // Even for /current, verify they have permission to read their own API keys
            if !can_read_own_resource(&current_user, Resource::ApiKeys, current_user.id) {
                return Err(Error::InsufficientPermissions {
                    required: Permission::Allow(Resource::ApiKeys, Operation::ReadOwn),
                    action: Operation::ReadOwn,
                    resource: "API keys for current user".to_string(),
                });
            }
            current_user.id
        }
        UserIdOrCurrent::Id(uuid) => {
            let can_read_all_api_keys = can_read_all_resources(&current_user, Resource::ApiKeys);
            let can_read_own_api_keys = can_read_own_resource(&current_user, Resource::ApiKeys, uuid);

            // Allow access if user can read all API keys OR read their own API keys
            if !can_read_all_api_keys && !can_read_own_api_keys {
                return Err(Error::InsufficientPermissions {
                    required: Permission::Any(vec![
                        Permission::Allow(Resource::ApiKeys, Operation::ReadAll),
                        Permission::Allow(Resource::ApiKeys, Operation::ReadOwn),
                    ]),
                    action: Operation::ReadOwn,
                    resource: format!("API keys for user {uuid}"),
                });
            }
            uuid
        }
    };

    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ApiKeys::new(&mut pool_conn);

    // Get the specific API key and verify ownership
    let api_key = repo
        .get_by_id(api_key_id)
        .await?
        .filter(|key| key.user_id == target_user_id)
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
        ("X-Doubleword-User" = [])
    )
)]
pub async fn delete_user_api_key(
    State(state): State<AppState>,
    Path((user_id, api_key_id)): Path<(UserIdOrCurrent, ApiKeyId)>,
    // Can't use RequiresPermission here because we need conditional logic for own vs other users
    current_user: CurrentUser,
) -> Result<StatusCode> {
    let target_user_id = match user_id {
        UserIdOrCurrent::Current(_) => {
            // Even for /current, verify they have permission to delete their own API keys
            if !can_delete_own_resource(&current_user, Resource::ApiKeys, current_user.id) {
                return Err(Error::InsufficientPermissions {
                    required: Permission::Allow(Resource::ApiKeys, Operation::DeleteOwn),
                    action: Operation::DeleteOwn,
                    resource: "API keys for current user".to_string(),
                });
            }
            current_user.id
        }
        UserIdOrCurrent::Id(uuid) => {
            let can_delete_all_api_keys = can_delete_all_resources(&current_user, Resource::ApiKeys);
            let can_delete_own_api_keys = can_delete_own_resource(&current_user, Resource::ApiKeys, uuid);

            // Allow deletion if user can delete all API keys OR delete their own API keys
            if !can_delete_all_api_keys && !can_delete_own_api_keys {
                return Err(Error::InsufficientPermissions {
                    required: Permission::Any(vec![
                        Permission::Allow(Resource::ApiKeys, Operation::DeleteAll),
                        Permission::Allow(Resource::ApiKeys, Operation::DeleteOwn),
                    ]),
                    action: Operation::DeleteOwn,
                    resource: format!("API keys for user {uuid}"),
                });
            }
            uuid
        }
    };

    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ApiKeys::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);

    // Check if the API key exists and belongs to the target user before deleting
    repo.get_by_id(api_key_id)
        .await?
        .filter(|key| key.user_id == target_user_id)
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
    use crate::api::models::users::Role;
    use crate::test_utils::*;
    use serde_json::json;
    use sqlx::PgPool;

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_api_key_for_self(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let api_key_data = json!({
            "name": "Test API Key",
            "description": "A test API key"
        });

        let response = app
            .post("/admin/api/v1/users/current/api-keys")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
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
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, regular_user.id, group.id).await;

        let api_key_data = json!({
            "name": "Admin Created Key",
            "description": "Created by admin for user"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", regular_user.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
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
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        let api_key_data = json!({
            "name": "Forbidden Key",
            "description": "This should not work"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", user2.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .json(&api_key_data)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_user_api_keys(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        create_test_api_key_for_user(&pool, user.id).await;

        let response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let api_keys: Vec<ApiKeyInfoResponse> = response.json();
        assert_eq!(api_keys.len(), 1);
        assert_eq!(api_keys[0].name, "Test API Key");
    }

    // Add new pagination test for the handler
    #[sqlx::test]
    #[test_log::test]
    async fn test_list_user_api_keys_with_pagination_query_params(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create multiple API keys
        for i in 1..=5 {
            let api_key_data = json!({
                "name": format!("Test API Key {}", i),
                "description": format!("Description for key {}", i)
            });

            app.post("/admin/api/v1/users/current/api-keys")
                .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
                .json(&api_key_data)
                .await
                .assert_status(axum::http::StatusCode::CREATED);
        }

        // Test with pagination parameters
        let response = app
            .get("/admin/api/v1/users/current/api-keys?skip=1&limit=2")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let api_keys: Vec<ApiKeyInfoResponse> = response.json();
        assert_eq!(api_keys.len(), 2, "Should return exactly 2 items with limit=2");

        // Test with no pagination parameters (should use defaults)
        let response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let api_keys: Vec<ApiKeyInfoResponse> = response.json();
        assert_eq!(api_keys.len(), 5, "Should return all items with default pagination");

        // Test with large limit (should be capped)
        let response = app
            .get("/admin/api/v1/users/current/api-keys?limit=9999")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let api_keys: Vec<ApiKeyInfoResponse> = response.json();
        assert_eq!(api_keys.len(), 5, "Should return all items even with large limit");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_user_api_key_for_self(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        let api_key = create_test_api_key_for_user(&pool, user.id).await;

        let response = app
            .delete(&format!("/admin/api/v1/users/current/api-keys/{}", api_key.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status(axum::http::StatusCode::NO_CONTENT);

        // Verify the API key was deleted by trying to list them
        let list_response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        list_response.assert_status_ok();
        let api_keys: Vec<ApiKeyInfoResponse> = list_response.json();
        assert_eq!(api_keys.len(), 0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_user_api_key_for_other_user_as_admin(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, regular_user.id, group.id).await;
        let api_key = create_test_api_key_for_user(&pool, regular_user.id).await;

        let response = app
            .delete(&format!("/admin/api/v1/users/{}/api-keys/{}", regular_user.id, api_key.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status(axum::http::StatusCode::NO_CONTENT);

        // Verify the API key was deleted
        let list_response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", regular_user.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        list_response.assert_status_ok();
        let api_keys: Vec<ApiKeyInfoResponse> = list_response.json();
        assert_eq!(api_keys.len(), 0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_user_api_key_for_other_user_as_non_admin_forbidden(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user2.id, group.id).await;
        let api_key = create_test_api_key_for_user(&pool, user2.id).await;

        let response = app
            .delete(&format!("/admin/api/v1/users/{}/api-keys/{}", user2.id, api_key.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status_forbidden();

        // Verify the API key still exists
        let list_response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", user2.id))
            .add_header(add_auth_headers(&user2).0, add_auth_headers(&user2).1)
            .await;

        list_response.assert_status_ok();
        let api_keys: Vec<ApiKeyInfoResponse> = list_response.json();
        assert_eq!(api_keys.len(), 1);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_nonexistent_api_key_returns_not_found(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        let fake_api_key_id = uuid::Uuid::new_v4();

        let response = app
            .delete(&format!("/admin/api/v1/users/current/api-keys/{fake_api_key_id}"))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_api_key_belonging_to_different_user_returns_not_found(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
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
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_api_keys_for_other_user_as_admin(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, regular_user.id, group.id).await;

        // Create multiple API keys for the regular user
        let api_key1 = create_test_api_key_for_user(&pool, regular_user.id).await;
        let api_key2 = create_test_api_key_for_user(&pool, regular_user.id).await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", regular_user.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let api_keys: Vec<ApiKeyInfoResponse> = response.json();
        assert_eq!(api_keys.len(), 2);

        // Verify we got the correct API keys
        let returned_ids: Vec<_> = api_keys.iter().map(|k| k.id).collect();
        assert!(returned_ids.contains(&api_key1.id));
        assert!(returned_ids.contains(&api_key2.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_api_keys_for_other_user_as_non_admin_forbidden(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user2.id, group.id).await;
        create_test_api_key_for_user(&pool, user2.id).await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", user2.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_api_key_for_other_user_as_admin(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, regular_user.id, group.id).await;
        let api_key = create_test_api_key_for_user(&pool, regular_user.id).await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys/{}", regular_user.id, api_key.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let returned_key: ApiKeyInfoResponse = response.json();
        assert_eq!(returned_key.id, api_key.id);
        assert_eq!(returned_key.name, api_key.name);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_api_key_for_other_user_as_non_admin_forbidden(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user2.id, group.id).await;
        let api_key = create_test_api_key_for_user(&pool, user2.id).await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys/{}", user2.id, api_key.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_request_viewer_cannot_manage_api_keys(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;

        // RequestViewer should not be able to create API keys for themselves
        let api_key_data = json!({
            "name": "RequestViewer Key",
            "description": "Should not work"
        });

        let response = app
            .post("/admin/api/v1/users/current/api-keys")
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .json(&api_key_data)
            .await;

        response.assert_status_forbidden();

        // RequestViewer should not be able to list their own API keys
        let response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;

        response.assert_status_forbidden();

        // RequestViewer should not be able to list other users' API keys
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", standard_user.id))
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_multi_role_user_api_key_permissions(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;

        // Create a user with both StandardUser and RequestViewer roles
        let multi_role_user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::RequestViewer]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, multi_role_user.id, group.id).await;

        // Should be able to create API keys (from StandardUser role)
        let api_key_data = json!({
            "name": "Multi Role Key",
            "description": "Should work due to StandardUser role"
        });

        let response = app
            .post("/admin/api/v1/users/current/api-keys")
            .add_header(add_auth_headers(&multi_role_user).0, add_auth_headers(&multi_role_user).1)
            .json(&api_key_data)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);

        // Should be able to list their own API keys (from StandardUser role)
        let response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(add_auth_headers(&multi_role_user).0, add_auth_headers(&multi_role_user).1)
            .await;

        response.assert_status_ok();
        let api_keys: Vec<ApiKeyInfoResponse> = response.json();
        assert_eq!(api_keys.len(), 1);
        assert_eq!(api_keys[0].name, "Multi Role Key");

        // Should be able to get specific API key
        let api_key_id = api_keys[0].id;
        let response = app
            .get(&format!("/admin/api/v1/users/current/api-keys/{api_key_id}"))
            .add_header(add_auth_headers(&multi_role_user).0, add_auth_headers(&multi_role_user).1)
            .await;

        response.assert_status_ok();

        // Should be able to delete their own API keys
        let response = app
            .delete(&format!("/admin/api/v1/users/current/api-keys/{api_key_id}"))
            .add_header(add_auth_headers(&multi_role_user).0, add_auth_headers(&multi_role_user).1)
            .await;

        response.assert_status(axum::http::StatusCode::NO_CONTENT);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_full_api_key_access(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_user(&pool, Role::PlatformManager).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, standard_user.id, group.id).await;

        // Platform manager should be able to create API keys for other users
        let api_key_data = json!({
            "name": "Manager Created Key",
            "description": "Created by platform manager"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", standard_user.id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .json(&api_key_data)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);

        // Platform manager should be able to list all users' API keys
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", standard_user.id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;

        response.assert_status_ok();
        let api_keys: Vec<ApiKeyInfoResponse> = response.json();
        assert_eq!(api_keys.len(), 1);

        // Platform manager should be able to get specific API keys for other users
        let api_key_id = api_keys[0].id;
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys/{}", standard_user.id, api_key_id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;

        response.assert_status_ok();

        // Platform manager should be able to delete other users' API keys
        let response = app
            .delete(&format!("/admin/api/v1/users/{}/api-keys/{}", standard_user.id, api_key_id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;

        response.assert_status(axum::http::StatusCode::NO_CONTENT);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_api_key_isolation_between_users(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
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
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status_ok();
        let user1_keys: Vec<ApiKeyInfoResponse> = response.json();
        assert_eq!(user1_keys.len(), 1);
        assert_eq!(user1_keys[0].id, api_key1.id);

        // User2 should only see their own API keys
        let response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(add_auth_headers(&user2).0, add_auth_headers(&user2).1)
            .await;

        response.assert_status_ok();
        let user2_keys: Vec<ApiKeyInfoResponse> = response.json();
        assert_eq!(user2_keys.len(), 1);
        assert_eq!(user2_keys[0].id, api_key2.id);

        // User1 should not be able to access user2's specific API key
        let response = app
            .get(&format!("/admin/api/v1/users/current/api-keys/{}", api_key2.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status_not_found(); // 404 because the key doesn't belong to user1
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_error_messages_are_user_friendly(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Try to access another user's API keys - should get user-friendly error
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", user2.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
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
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        let api_key = create_test_api_key_for_user(&pool, user.id).await;

        let response = app
            .get(&format!("/admin/api/v1/users/current/api-keys/{}", api_key.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
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
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create API key - should return the actual key value
        let api_key_data = json!({
            "name": "Test Key for Security",
            "description": "Testing key exposure"
        });

        let create_response = app
            .post("/admin/api/v1/users/current/api-keys")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
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
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        list_response.assert_status_ok();
        let listed_keys: Vec<ApiKeyInfoResponse> = list_response.json();
        assert_eq!(listed_keys.len(), 1);

        // ApiKeyInfoResponse doesn't have a key field - this is the security feature

        // Get specific API key - should NOT return key value (uses ApiKeyInfoResponse)
        let get_response = app
            .get(&format!("/admin/api/v1/users/current/api-keys/{}", created_key.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        get_response.assert_status_ok();
        let retrieved_key: ApiKeyInfoResponse = get_response.json();

        // ApiKeyInfoResponse doesn't have a key field - this is the security feature
        assert_eq!(retrieved_key.id, created_key.id);
        assert_eq!(retrieved_key.name, created_key.name);
    }
}
