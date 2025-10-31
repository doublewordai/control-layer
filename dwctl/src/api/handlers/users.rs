use crate::{
    api::models::{
        groups::GroupResponse,
        users::{CurrentUser, GetUserQuery, ListUsersQuery, UserCreate, UserResponse, UserUpdate},
    },
    auth::permissions::{self as permissions, can_read_all_resources, can_read_own_resource, operation, resource, RequiresPermission},
    db::{
        handlers::{users::UserFilter, Credits, Groups, Repository, Users},
        models::users::{UserCreateDBRequest, UserUpdateDBRequest},
    },
    errors::{Error, Result},
    types::{GroupId, Operation, Permission, Resource, UserId, UserIdOrCurrent},
    AppState,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};
use rust_decimal::prelude::ToPrimitive;
use tracing::error;

// GET /user - List users (admin only)
#[utoipa::path(
    get,
    path = "/users",
    tag = "users",
    summary = "List users",
    description = "List all users (admin only)",
    params(
        ("skip" = Option<i64>, Query, description = "Number of users to skip"),
        ("limit" = Option<i64>, Query, description = "Maximum number of users to return"),
        ("include" = Option<String>, Query, description = "Comma-separated list of related entities to include (e.g., 'groups')"),
    ),
    responses(
        (status = 200, description = "List of users", body = [UserResponse]),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn list_users(
    State(state): State<AppState>,
    Query(query): Query<ListUsersQuery>,
    current_user: CurrentUser,
    _: RequiresPermission<resource::Users, operation::ReadAll>,
) -> Result<Json<Vec<UserResponse>>> {
    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;
    let skip = query.skip.unwrap_or(0);
    let limit = query.limit.unwrap_or(100).min(1000);

    let users;
    {
        let mut repo = Users::new(&mut tx);
        users = repo.list(&UserFilter::new(skip, limit)).await?;
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

    let mut response_users = Vec::new();

    // Create a list of user IDs for bulk fetching related data
    let user_ids: Vec<_> = users.iter().map(|u| u.id).collect();

    // Check if user has permission to view billing data (Credits ReadAll)
    let can_view_billing = permissions::has_permission(&current_user, Resource::Credits, Operation::ReadAll);

    // If includes billing AND user has permission, create balances_map
    let balances_map = if includes.contains(&"billing") && can_view_billing {
        let mut credits_repo = Credits::new(&mut tx);
        Some(credits_repo.get_users_balances_bulk(&user_ids).await?)
    } else {
        None
    };

    // If includes groups, make groups_map and user_groups_map
    let (groups_map, user_groups_map) = if includes.contains(&"groups") {
        let mut groups_repo = Groups::new(&mut tx);
        let user_groups_map = groups_repo.get_users_groups_bulk(&user_ids).await?;
        // Collect all unique group IDs that we need to fetch
        let all_group_ids: Vec<GroupId> = user_groups_map
            .values()
            .flatten()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        // Fetch only the specific groups we need in bulk
        let groups_map = groups_repo.get_bulk(all_group_ids).await?;
        (Some(groups_map), Some(user_groups_map))
    } else {
        (None, None)
    };

    // Iterate through and enrich users
    for user in users {
        let mut response_user = UserResponse::from(user);
        // If includes groups
        if let Some(groups_map) = &groups_map {
            if let Some(user_groups_map) = &user_groups_map {
                let group_ids = user_groups_map.get(&response_user.id).cloned().unwrap_or_default();
                let groups: Vec<GroupResponse> = group_ids
                    .iter()
                    .filter_map(|group_id| groups_map.get(group_id))
                    .cloned()
                    .map(|group| group.into())
                    .collect();
                response_user = response_user.with_groups(groups);
            }
        }

        // If includes billing
        if let Some(balances_map) = &balances_map {
            let balance = balances_map.get(&response_user.id).cloned().unwrap_or(0.0);
            response_user = response_user.with_credit_balance(balance);
        }

        response_users.push(response_user);
    }

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;
    Ok(Json(response_users))
}

// GET /users/{user_id} - Get specific user (admin only) or current user
#[utoipa::path(
    get,
    path = "/users/{user_id}",
    tag = "users",
    summary = "Get user",
    description = "Get a specific user by ID or current user",
    params(
        ("user_id" = String, Path, description = "User ID (UUID) or 'current' for current user"),
        ("includes" = Option<String>, Query, description = "Data to include, currently only 'billing' is supported"),
    ),
    responses(
        (status = 200, description = "User information", body = UserResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - can only view own user data unless admin"),
        (status = 404, description = "User not found"),
        (status = 500, description = "Internal server error")
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn get_user(
    State(state): State<AppState>,
    Path(user_id): Path<UserIdOrCurrent>,
    Query(query): Query<GetUserQuery>,
    // Can't use RequiresPermission here because we need conditional logic for own vs other users
    current_user: CurrentUser,
) -> Result<Json<UserResponse>> {
    let target_user_id = match user_id {
        UserIdOrCurrent::Current(_) => {
            // Even for /current, verify they have permission to read their own user data
            if !can_read_own_resource(&current_user, Resource::Users, current_user.id) {
                return Err(Error::InsufficientPermissions {
                    required: Permission::Allow(Resource::Users, Operation::ReadOwn),
                    action: Operation::ReadOwn,
                    resource: "current user data".to_string(),
                });
            }
            current_user.id
        }
        UserIdOrCurrent::Id(uuid) => {
            let can_read_all_users = can_read_all_resources(&current_user, Resource::Users);
            let can_read_own_user = can_read_own_resource(&current_user, Resource::Users, uuid);

            // Allow access if user can read all users OR read their own user data
            if !can_read_all_users && !can_read_own_user {
                return Err(Error::InsufficientPermissions {
                    required: Permission::Any(vec![
                        Permission::Allow(Resource::Users, Operation::ReadAll),
                        Permission::Allow(Resource::Users, Operation::ReadOwn),
                    ]),
                    action: Operation::ReadAll,
                    resource: format!("user data for user {uuid}"),
                });
            }
            uuid
        }
    };

    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Users::new(&mut pool_conn);

    let user = repo.get_by_id(target_user_id).await?.ok_or_else(|| Error::NotFound {
        resource: "User".to_string(),
        id: target_user_id.to_string(),
    })?;

    let mut response = UserResponse::from(user);

    // Include groups if requested and permitted
    // Permitted if:
    //     1. You have ReadAll on Credits
    //     2. You are requesting your own data and have ReadOwn on Credits
    if query.include.as_deref().is_some_and(|includes| includes.contains("billing"))
        && (permissions::has_permission(&current_user, Resource::Credits, Operation::ReadAll)
            || (target_user_id == current_user.id && permissions::has_permission(&current_user, Resource::Credits, Operation::ReadOwn)))
    {
        let mut credits_repo = Credits::new(&mut pool_conn);
        let balance = credits_repo.get_user_balance(target_user_id).await?.to_f64().unwrap_or_else(|| {
            error!("Failed to convert balance to f64 for user_id {}", target_user_id);
            0.0
        });
        response = response.with_credit_balance(balance);
    }

    Ok(Json(response))
}

// POST /users - Create user (admin only)
#[utoipa::path(
    post,
    path = "/users",
    tag = "users",
    summary = "Create user",
    description = "Create a new user (admin only)",
    responses(
        (status = 201, description = "User created successfully", body = UserResponse),
        (status = 400, description = "Bad request - invalid user data"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn create_user(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Users, operation::CreateAll>,
    Json(user_data): Json<UserCreate>,
) -> Result<(StatusCode, Json<UserResponse>)> {
    // Check admin role

    let mut conn = state.db.acquire().await.expect("Failed to acquire database connection");
    let mut repo = Users::new(&mut conn);
    let db_request = UserCreateDBRequest::from(user_data);

    let user = repo.create(&db_request).await?;
    Ok((StatusCode::CREATED, Json(UserResponse::from(user))))
}

// PATCH /user/{user_id} - Update user (admin only)
#[utoipa::path(
    patch,
    path = "/users/{user_id}",
    tag = "users",
    summary = "Update user",
    description = "Update an existing user (admin only)",
    params(
        ("user_id" = uuid::Uuid, Path, description = "User ID to update"),
    ),
    responses(
        (status = 200, description = "User updated successfully", body = UserResponse),
        (status = 400, description = "Bad request - invalid user data"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "User not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn update_user(
    State(state): State<AppState>,
    Path(user_id): Path<UserId>,
    _: RequiresPermission<resource::Users, operation::UpdateAll>,
    Json(user_data): Json<UserUpdate>,
) -> Result<Json<UserResponse>> {
    // Check admin role
    let mut conn = state.db.acquire().await.expect("Failed to acquire database connection");

    let mut repo = Users::new(&mut conn);
    let db_request = UserUpdateDBRequest::new(user_data);

    let user = repo.update(user_id, &db_request).await?;
    Ok(Json(UserResponse::from(user)))
}

// DELETE /user/{user_id} - Delete user (admin only)
#[utoipa::path(
    delete,
    path = "/users/{user_id}",
    tag = "users",
    summary = "Delete user",
    description = "Delete a user (admin only)",
    params(
        ("user_id" = uuid::Uuid, Path, description = "User ID to delete"),
    ),
    responses(
        (status = 204, description = "User deleted successfully"),
        (status = 400, description = "Bad request - cannot delete yourself"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "User not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn delete_user(
    State(state): State<AppState>,
    Path(user_id): Path<UserId>,
    current_user: RequiresPermission<resource::Users, operation::DeleteAll>,
) -> Result<StatusCode> {
    // Prevent self-deletion
    if user_id == current_user.id {
        return Err(Error::BadRequest {
            message: "You cannot delete your own account".to_string(),
        });
    }
    let mut conn = state.db.acquire().await.expect("Failed to acquire database connection");
    let mut repo = Users::new(&mut conn);

    match repo.delete(user_id).await? {
        true => Ok(StatusCode::NO_CONTENT),
        false => Err(Error::NotFound {
            resource: "User".to_string(),
            id: user_id.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::users::Role;
    use crate::db::handlers::{Credits, Groups, Repository};
    use crate::db::models::credits::CreditTransactionType;
    use crate::db::models::{credits::CreditTransactionCreateDBRequest, groups::GroupCreateDBRequest};
    use crate::test_utils::*;
    use rust_decimal::Decimal;
    use serde_json::json;
    use sqlx::PgPool;
    use std::collections::HashSet;
    use std::str::FromStr;
    use uuid::Uuid;

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_current_user_info(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let response = app
            .get("/admin/api/v1/users/current")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let current_user: UserResponse = response.json();
        assert_eq!(current_user.id, user.id);
        assert_eq!(current_user.email, user.email);
        assert_eq!(current_user.roles, user.roles);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_users_as_admin(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        let response = app
            .get("/admin/api/v1/users")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let users: Vec<UserResponse> = response.json();
        assert!(!users.is_empty());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_users_as_non_admin_forbidden(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let response = app
            .get("/admin/api/v1/users")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_user_as_admin(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        let new_user = json!({
            "username": "newuser",
            "email": "newuser@example.com",
            "display_name": "New User",
            "avatar_url": null,
            "roles": ["StandardUser"]
        });

        let response = app
            .post("/admin/api/v1/users")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&new_user)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let created_user: UserResponse = response.json();
        assert_eq!(created_user.username, "newuser");
        assert_eq!(created_user.email, "newuser@example.com");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_unauthenticated_request(pool: PgPool) {
        let (app, _) = create_test_app(pool, false).await;

        let response = app.get("/admin/api/v1/users/current").await;
        response.assert_status_unauthorized();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_users_with_pagination(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Create additional test users
        for _ in 0..5 {
            create_test_user(&pool, Role::StandardUser).await;
        }

        // Test with limit
        let response = app
            .get("/admin/api/v1/users?limit=3")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let users: Vec<UserResponse> = response.json();
        assert_eq!(users.len(), 3);

        // Test with skip and limit
        let response = app
            .get("/admin/api/v1/users?skip=2&limit=2")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let users: Vec<UserResponse> = response.json();
        assert!(users.len() <= 2);

        // Test skip beyond available users
        let response = app
            .get("/admin/api/v1/users?skip=1000&limit=10")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let users: Vec<UserResponse> = response.json();
        assert!(users.is_empty());

        // Test maximum limit enforcement
        let response = app
            .get("/admin/api/v1/users?limit=2000")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let users: Vec<UserResponse> = response.json();
        assert!(users.len() <= 1000); // Should be capped at 1000
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_other_user_as_admin(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}", regular_user.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let user_response: UserResponse = response.json();
        assert_eq!(user_response.id, regular_user.id);
        assert_eq!(user_response.email, regular_user.email);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_other_user_as_non_admin_forbidden(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}", user2.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_user_not_found(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let nonexistent_id = uuid::Uuid::new_v4();

        let response = app
            .get(&format!("/admin/api/v1/users/{nonexistent_id}"))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_users_with_groups_include(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;

        // Create a group and add the regular user to it
        let mut conn = pool.acquire().await.expect("Failed to acquire database connection");
        let mut group_repo = Groups::new(&mut conn);
        let group_create = GroupCreateDBRequest {
            name: "Test Group".to_string(),
            description: Some("Test group for user include".to_string()),
            created_by: admin_user.id,
        };
        let group = group_repo.create(&group_create).await.expect("Failed to create test group");
        group_repo
            .add_user_to_group(regular_user.id, group.id)
            .await
            .expect("Failed to add user to group");

        // Test without include parameter - should not include groups
        let response = app
            .get("/admin/api/v1/users")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let users: Vec<UserResponse> = response.json();
        let found_user = users.iter().find(|u| u.id == regular_user.id).expect("User not found");
        assert!(found_user.groups.is_none());

        // Test with include=groups - should include groups
        let response = app
            .get("/admin/api/v1/users?include=groups")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let users: Vec<UserResponse> = response.json();
        let found_user = users.iter().find(|u| u.id == regular_user.id).expect("User not found");
        assert!(found_user.groups.is_some());
        let groups = found_user.groups.as_ref().unwrap().iter().map(|x| x.id).collect::<HashSet<_>>();
        assert!(groups.contains(&group.id));

        // Test with include=groups and pagination
        let response = app
            .get("/admin/api/v1/users?include=groups&limit=10")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let users: Vec<UserResponse> = response.json();
        let found_user = users.iter().find(|u| u.id == regular_user.id).expect("User not found");
        assert!(found_user.groups.is_some());
        let groups = found_user.groups.as_ref().unwrap().iter().map(|x| x.id).collect::<HashSet<_>>();
        assert!(groups.contains(&group.id));

        // Test with include=invalid - should ignore invalid includes
        let response = app
            .get("/admin/api/v1/users?include=invalid,groups")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let users: Vec<UserResponse> = response.json();
        let found_user = users.iter().find(|u| u.id == regular_user.id).expect("User not found");
        assert!(found_user.groups.is_some());
        let groups = found_user.groups.as_ref().unwrap().iter().map(|x| x.id).collect::<HashSet<_>>();
        assert!(groups.contains(&group.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_users_with_billing_include(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;

        // Add credits to the regular user
        create_initial_credit_transaction(&pool, regular_user.id, "250.0").await;

        // Test without include parameter - should not include billing
        let response = app
            .get("/admin/api/v1/users")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let users: Vec<UserResponse> = response.json();
        let found_user = users.iter().find(|u| u.id == regular_user.id).expect("User not found");
        assert!(found_user.credit_balance.is_none());

        // Test with include=billing - should include credit balance
        let response = app
            .get("/admin/api/v1/users?include=billing")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let users: Vec<UserResponse> = response.json();
        let found_user = users.iter().find(|u| u.id == regular_user.id).expect("User not found");
        assert!(found_user.credit_balance.is_some());
        assert_eq!(found_user.credit_balance.unwrap(), 250.0);

        // Test with include=billing and pagination
        let response = app
            .get("/admin/api/v1/users?include=billing&limit=10")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let users: Vec<UserResponse> = response.json();
        let found_user = users.iter().find(|u| u.id == regular_user.id).expect("User not found");
        assert!(found_user.credit_balance.is_some());
        assert_eq!(found_user.credit_balance.unwrap(), 250.0);

        // Test with include=invalid,billing - should ignore invalid and include billing
        let response = app
            .get("/admin/api/v1/users?include=invalid,billing")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let users: Vec<UserResponse> = response.json();
        let found_user = users.iter().find(|u| u.id == regular_user.id).expect("User not found");
        assert!(found_user.credit_balance.is_some());
        assert_eq!(found_user.credit_balance.unwrap(), 250.0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_users_with_groups_and_billing_include(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;

        // Create a group and add the regular user to it
        let mut conn = pool.acquire().await.expect("Failed to acquire database connection");
        let mut group_repo = Groups::new(&mut conn);
        let group_create = GroupCreateDBRequest {
            name: "Test Group".to_string(),
            description: Some("Test group for combined include".to_string()),
            created_by: admin_user.id,
        };
        let group = group_repo.create(&group_create).await.expect("Failed to create test group");
        group_repo
            .add_user_to_group(regular_user.id, group.id)
            .await
            .expect("Failed to add user to group");

        // Add credits to the regular user
        create_initial_credit_transaction(&pool, regular_user.id, "500.0").await;

        // Test with include=groups,billing - should include both
        let response = app
            .get("/admin/api/v1/users?include=groups,billing")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let users: Vec<UserResponse> = response.json();
        let found_user = users.iter().find(|u| u.id == regular_user.id).expect("User not found");

        // Verify groups are included
        assert!(found_user.groups.is_some());
        let groups = found_user.groups.as_ref().unwrap().iter().map(|x| x.id).collect::<HashSet<_>>();
        assert!(groups.contains(&group.id));

        // Verify billing is included
        assert!(found_user.credit_balance.is_some());
        assert_eq!(found_user.credit_balance.unwrap(), 500.0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_users_billing_with_zero_balance(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;

        // Don't add any credits - user should have zero balance

        // Test with include=billing - should show 0.0 for users with no transactions
        let response = app
            .get("/admin/api/v1/users?include=billing")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let users: Vec<UserResponse> = response.json();
        let found_user = users.iter().find(|u| u.id == regular_user.id).expect("User not found");
        assert!(found_user.credit_balance.is_some());
        assert_eq!(found_user.credit_balance.unwrap(), 0.0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_users_billing_with_multiple_transactions(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;
        let user3 = create_test_user(&pool, Role::StandardUser).await;

        // Add different credit amounts to different users
        create_initial_credit_transaction(&pool, user1.id, "100.0").await;
        create_initial_credit_transaction(&pool, user2.id, "200.0").await;
        create_initial_credit_transaction(&pool, user3.id, "300.0").await;

        // Add a second transaction to user1
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits_repo = Credits::new(&mut conn);
        let request = CreditTransactionCreateDBRequest {
            user_id: user1.id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("50.0").unwrap(),
            description: Some("Additional grant".to_string()),
        };
        credits_repo
            .create_transaction(&request)
            .await
            .expect("Failed to create transaction");

        // Test with include=billing - should show latest balance for all users
        let response = app
            .get("/admin/api/v1/users?include=billing")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let users: Vec<UserResponse> = response.json();

        let found_user1 = users.iter().find(|u| u.id == user1.id).expect("User1 not found");
        assert_eq!(found_user1.credit_balance.unwrap(), 150.0); // 100 + 50

        let found_user2 = users.iter().find(|u| u.id == user2.id).expect("User2 not found");
        assert_eq!(found_user2.credit_balance.unwrap(), 200.0);

        let found_user3 = users.iter().find(|u| u.id == user3.id).expect("User3 not found");
        assert_eq!(found_user3.credit_balance.unwrap(), 300.0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_users_billing_manager_can_view_billing(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_admin_user(&pool, Role::BillingManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;

        // Add credits to the regular user
        create_initial_credit_transaction(&pool, regular_user.id, "350.0").await;

        // Test with include=billing - BillingManager should see balances
        let response = app
            .get("/admin/api/v1/users?include=billing")
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .await;

        response.assert_status_ok();
        let users: Vec<UserResponse> = response.json();
        let found_user = users.iter().find(|u| u.id == regular_user.id).expect("User not found");
        assert!(found_user.credit_balance.is_some());
        assert_eq!(found_user.credit_balance.unwrap(), 350.0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_user_as_admin(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;

        let update_data = json!({
            "display_name": "Updated Display Name",
            "avatar_url": "https://example.com/new-avatar.jpg"
        });

        let response = app
            .patch(&format!("/admin/api/v1/users/{}", regular_user.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&update_data)
            .await;

        response.assert_status_ok();
        let updated_user: UserResponse = response.json();
        assert_eq!(updated_user.id, regular_user.id);
        assert_eq!(updated_user.display_name.as_deref(), Some("Updated Display Name"));
        assert_eq!(updated_user.avatar_url.as_deref(), Some("https://example.com/new-avatar.jpg"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_user_as_non_admin_forbidden(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        let update_data = json!({
            "display_name": "Should Not Work"
        });

        let response = app
            .patch(&format!("/admin/api/v1/users/{}", user2.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .json(&update_data)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_nonexistent_user(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let nonexistent_id = uuid::Uuid::new_v4();

        let update_data = json!({
            "display_name": "Should Not Work"
        });

        let response = app
            .patch(&format!("/admin/api/v1/users/{nonexistent_id}"))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&update_data)
            .await;

        response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_user_as_admin(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;

        let response = app
            .delete(&format!("/admin/api/v1/users/{}", regular_user.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status(axum::http::StatusCode::NO_CONTENT);

        // Verify user is deleted by trying to get it
        let get_response = app
            .get(&format!("/admin/api/v1/users/{}", regular_user.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        get_response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_user_as_non_admin_forbidden(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        let response = app
            .delete(&format!("/admin/api/v1/users/{}", user2.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_nonexistent_user(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let nonexistent_id = uuid::Uuid::new_v4();

        let response = app
            .delete(&format!("/admin/api/v1/users/{nonexistent_id}"))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_self_forbidden(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        let response = app
            .delete(&format!("/admin/api/v1/users/{}", admin_user.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_bad_request();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_standard_user_permissions(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;
        let other_user = create_test_user(&pool, Role::StandardUser).await;

        // StandardUser should be able to get their own info
        let response = app
            .get("/admin/api/v1/users/current")
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;
        response.assert_status_ok();

        let response = app
            .get(&format!("/admin/api/v1/users/{}", standard_user.id))
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;
        response.assert_status_ok();

        // StandardUser should NOT be able to list all users (no ReadAll permission)
        let response = app
            .get("/admin/api/v1/users")
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;
        response.assert_status_forbidden();

        // StandardUser should NOT be able to get other users (no ReadAll permission)
        let response = app
            .get(&format!("/admin/api/v1/users/{}", other_user.id))
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;
        response.assert_status_forbidden();

        // StandardUser should NOT be able to create users
        let new_user = json!({
            "username": "should_not_work",
            "email": "shouldnotwork@example.com",
            "roles": ["StandardUser"]
        });

        let response = app
            .post("/admin/api/v1/users")
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .json(&new_user)
            .await;
        response.assert_status_forbidden();

        // StandardUser should NOT be able to update other users
        let update_data = json!({"display_name": "Should Not Work"});
        let response = app
            .patch(&format!("/admin/api/v1/users/{}", other_user.id))
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .json(&update_data)
            .await;
        response.assert_status_forbidden();

        // StandardUser should NOT be able to delete users
        let response = app
            .delete(&format!("/admin/api/v1/users/{}", other_user.id))
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;
        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_request_viewer_permissions(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;
        let other_user = create_test_user(&pool, Role::StandardUser).await;

        // RequestViewer should be able to get their own info (has ReadOwn for Users)
        let response = app
            .get("/admin/api/v1/users/current")
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;
        response.assert_status_ok();

        // RequestViewer should NOT be able to list all users (no ReadAll for Users)
        let response = app
            .get("/admin/api/v1/users")
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;
        response.assert_status_forbidden();

        // RequestViewer should NOT be able to get other users (no ReadAll for Users)
        let response = app
            .get(&format!("/admin/api/v1/users/{}", other_user.id))
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;
        response.assert_status_forbidden();

        // RequestViewer should NOT be able to create, update, or delete users
        let new_user = json!({
            "username": "should_not_work",
            "email": "shouldnotwork@example.com",
            "roles": ["StandardUser"]
        });

        let response = app
            .post("/admin/api/v1/users")
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .json(&new_user)
            .await;
        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_user_permissions(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_user(&pool, Role::PlatformManager).await; // Non-admin PlatformManager
        let standard_user = create_test_user(&pool, Role::StandardUser).await;

        // PlatformManager should be able to list all users
        let response = app
            .get("/admin/api/v1/users")
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;
        response.assert_status_ok();

        // PlatformManager should be able to get any user
        let response = app
            .get(&format!("/admin/api/v1/users/{}", standard_user.id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;
        response.assert_status_ok();

        // PlatformManager should be able to create users
        let new_user = json!({
            "username": "created_by_pm",
            "email": "createdbypm@example.com",
            "roles": ["StandardUser"]
        });

        let response = app
            .post("/admin/api/v1/users")
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .json(&new_user)
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);
        let created_user: UserResponse = response.json();

        // PlatformManager should be able to update users
        let update_data = json!({"display_name": "Updated by PM"});
        let response = app
            .patch(&format!("/admin/api/v1/users/{}", created_user.id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .json(&update_data)
            .await;
        response.assert_status_ok();

        // PlatformManager should be able to delete users
        let response = app
            .delete(&format!("/admin/api/v1/users/{}", created_user.id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;
        response.assert_status(axum::http::StatusCode::NO_CONTENT);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_multi_role_user_permissions(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;

        // User with StandardUser + RequestViewer should have additive permissions
        let multi_role_user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::RequestViewer]).await;
        let other_user = create_test_user(&pool, Role::StandardUser).await;

        // Should be able to get their own info (both roles have ReadOwn for Users)
        let response = app
            .get("/admin/api/v1/users/current")
            .add_header(add_auth_headers(&multi_role_user).0, add_auth_headers(&multi_role_user).1)
            .await;
        response.assert_status_ok();

        // Should NOT be able to list all users (neither role has ReadAll for Users)
        let response = app
            .get("/admin/api/v1/users")
            .add_header(add_auth_headers(&multi_role_user).0, add_auth_headers(&multi_role_user).1)
            .await;
        response.assert_status_forbidden();

        // Should NOT be able to get other users (neither role has ReadAll for Users)
        let response = app
            .get(&format!("/admin/api/v1/users/{}", other_user.id))
            .add_header(add_auth_headers(&multi_role_user).0, add_auth_headers(&multi_role_user).1)
            .await;
        response.assert_status_forbidden();

        // Should NOT be able to create users (neither role has CreateAll)
        let new_user = json!({
            "username": "should_not_work",
            "email": "shouldnotwork@example.com",
            "roles": ["StandardUser"]
        });

        let response = app
            .post("/admin/api/v1/users")
            .add_header(add_auth_headers(&multi_role_user).0, add_auth_headers(&multi_role_user).1)
            .json(&new_user)
            .await;
        response.assert_status_forbidden();

        // User with PlatformManager + RequestViewer should have full user management
        let full_admin = create_test_user_with_roles(&pool, vec![Role::PlatformManager, Role::RequestViewer]).await;

        let response = app
            .get("/admin/api/v1/users")
            .add_header(add_auth_headers(&full_admin).0, add_auth_headers(&full_admin).1)
            .await;
        response.assert_status_ok();

        let response = app
            .post("/admin/api/v1/users")
            .add_header(add_auth_headers(&full_admin).0, add_auth_headers(&full_admin).1)
            .json(&new_user)
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_user_access_isolation(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;
        let user3 = create_test_user(&pool, Role::RequestViewer).await;

        // Test that users can only access their own data
        let users = vec![&user1, &user2, &user3];
        let targets = vec![&user1, &user2, &user3];

        for user in &users {
            for target in &targets {
                let response = app
                    .get(&format!("/admin/api/v1/users/{}", target.id))
                    .add_header(add_auth_headers(user).0, add_auth_headers(user).1)
                    .await;

                if user.id == target.id {
                    // Users should be able to access their own data
                    response.assert_status_ok();
                    let user_response: UserResponse = response.json();
                    assert_eq!(user_response.id, target.id);
                } else {
                    // Users should NOT be able to access other users' data
                    response.assert_status_forbidden();
                }
            }
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_role_layering_user_access(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;

        // Test different role combinations for user access
        let role_tests = vec![
            (vec![Role::StandardUser], false, true, false, "StandardUser only"),
            (vec![Role::RequestViewer], false, true, false, "RequestViewer only"),
            (vec![Role::PlatformManager], true, true, true, "PlatformManager only"),
            (
                vec![Role::StandardUser, Role::RequestViewer],
                false,
                true,
                false,
                "StandardUser + RequestViewer",
            ),
            (
                vec![Role::PlatformManager, Role::RequestViewer],
                true,
                true,
                true,
                "PlatformManager + RequestViewer",
            ),
            (
                vec![Role::PlatformManager, Role::StandardUser],
                true,
                true,
                true,
                "PlatformManager + StandardUser",
            ),
        ];

        for (roles, can_list_users, can_read_own, can_manage_users, _description) in role_tests {
            let user = create_test_user_with_roles(&pool, roles).await;

            // Test list users access
            let response = app
                .get("/admin/api/v1/users")
                .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
                .await;

            if can_list_users {
                response.assert_status_ok();
            } else {
                response.assert_status_forbidden();
            }

            // Test read own user access
            let response = app
                .get("/admin/api/v1/users/current")
                .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
                .await;

            if can_read_own {
                response.assert_status_ok();
            } else {
                response.assert_status_forbidden();
            }

            // Test user creation access
            let new_user = json!({
                "username": format!("test_user_{}", uuid::Uuid::new_v4()),
                "email": format!("test{}@example.com", uuid::Uuid::new_v4()),
                "roles": ["StandardUser"]
            });

            let response = app
                .post("/admin/api/v1/users")
                .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
                .json(&new_user)
                .await;

            if can_manage_users {
                response.assert_status(axum::http::StatusCode::CREATED);
                // Clean up created user
                let created_user: UserResponse = response.json();
                app.delete(&format!("/admin/api/v1/users/{}", created_user.id))
                    .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
                    .await
                    .assert_status(axum::http::StatusCode::NO_CONTENT);
            } else {
                response.assert_status_forbidden();
            }
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_admin_bypass_vs_role_permissions(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;

        // Test that admin users bypass role restrictions
        let admin_user = create_test_admin_user(&pool, Role::RequestViewer).await; // Admin with minimal role
        let non_admin_pm = create_test_user(&pool, Role::PlatformManager).await; // Non-admin with powerful role

        // Both should be able to list users, but for different reasons:
        // - Admin bypasses permission checks entirely
        // - PlatformManager has ReadAll permission for Users

        let admin_response = app
            .get("/admin/api/v1/users")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;
        admin_response.assert_status_ok();

        let pm_response = app
            .get("/admin/api/v1/users")
            .add_header(add_auth_headers(&non_admin_pm).0, add_auth_headers(&non_admin_pm).1)
            .await;
        pm_response.assert_status_ok();

        // Create a StandardUser who should not be able to list users
        let standard_user = create_test_user(&pool, Role::StandardUser).await;
        let standard_response = app
            .get("/admin/api/v1/users")
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;
        standard_response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_user_roles_backend_protection(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::PlatformManager]).await;

        // Try to update user roles to only RequestViewer (omitting StandardUser)
        let update_data = json!({
            "roles": ["RequestViewer"] // Intentionally omitting StandardUser
        });

        let response = app
            .patch(&format!("/admin/api/v1/users/{}", regular_user.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&update_data)
            .await;

        response.assert_status_ok();
        let updated_user: UserResponse = response.json();

        // Backend should have automatically added StandardUser role
        assert_eq!(updated_user.roles.len(), 2);
        assert!(updated_user.roles.contains(&Role::StandardUser));
        assert!(updated_user.roles.contains(&Role::RequestViewer));
        assert!(!updated_user.roles.contains(&Role::PlatformManager));

        // Try to update with empty roles array
        let update_data = json!({
            "roles": []
        });

        let response = app
            .patch(&format!("/admin/api/v1/users/{}", regular_user.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&update_data)
            .await;

        response.assert_status_ok();
        let updated_user: UserResponse = response.json();

        // Backend should have automatically added StandardUser role
        assert_eq!(updated_user.roles.len(), 1);
        assert!(updated_user.roles.contains(&Role::StandardUser));
    }

    async fn create_initial_credit_transaction(pool: &PgPool, user_id: UserId, amount: &str) -> Uuid {
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits_repo = Credits::new(&mut conn);

        let amount_decimal = Decimal::from_str(amount).expect("Invalid decimal amount");
        let request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: amount_decimal,
            description: Some("Initial credit grant".to_string()),
        };

        credits_repo
            .create_transaction(&request)
            .await
            .expect("Failed to create transaction")
            .id
    }

    // Test: GET /users/{user_id}/balance returns own balance for standard user
    #[sqlx::test]
    #[test_log::test]
    async fn test_get_own_balance_as_standard_user(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Add some credits
        create_initial_credit_transaction(&pool, user.id, "150.0").await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}?include=billing", user.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let balance: UserResponse = response.json();
        assert_eq!(balance.id, user.id);
        assert_eq!(balance.credit_balance, Some(150.0));
    }

    // Test: GET /users/current/balance works for standard user with billing info
    #[sqlx::test]
    #[test_log::test]
    async fn test_get_current_user_balance(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Add some credits
        create_initial_credit_transaction(&pool, user.id, "125.0").await;

        let response = app
            .get("/admin/api/v1/users/current?include=billing")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let balance: UserResponse = response.json();
        assert_eq!(balance.id, user.id);
        assert_eq!(balance.credit_balance, Some(125.0));
    }

    // Test: GET /users/{other_user_id} returns 403 for standard user asking for billing
    #[sqlx::test]
    #[test_log::test]
    async fn test_get_other_user_balance_forbidden(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}?include=billing", user2.id))
            .add_header(add_auth_headers(&user1).0, add_auth_headers(&user1).1)
            .await;

        response.assert_status_forbidden();
    }

    // Test: GET /users/{user_id} works for own balance as RequestViewer
    #[sqlx::test]
    #[test_log::test]
    async fn test_get_own_balance_as_request_viewer(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::RequestViewer).await;

        // Add some credits
        create_initial_credit_transaction(&pool, user.id, "150.0").await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}?include=billing", user.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let balance: UserResponse = response.json();
        assert_eq!(balance.id, user.id);
        assert_eq!(balance.credit_balance, Some(150.0));
    }

    // Test: GET /users/current works for RequestViewer with billing info
    #[sqlx::test]
    #[test_log::test]
    async fn test_get_current_user_balance_request_viewer(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::RequestViewer).await;

        // Add some credits
        create_initial_credit_transaction(&pool, user.id, "125.0").await;

        let response = app
            .get("/admin/api/v1/users/current?include=billing")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let balance: UserResponse = response.json();
        assert_eq!(balance.id, user.id);
        assert_eq!(balance.credit_balance, Some(125.0));
    }

    // Test: GET /users/current works for PlatformManager with billing info
    #[sqlx::test]
    #[test_log::test]
    async fn test_get_current_user_balance_platform_manager(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::PlatformManager).await;

        // Add some credits
        create_initial_credit_transaction(&pool, user.id, "125.0").await;

        let response = app
            .get("/admin/api/v1/users/current?include=billing")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let balance: UserResponse = response.json();
        assert_eq!(balance.id, user.id);
        assert_eq!(balance.credit_balance, Some(125.0));
    }

    // Test: PlatformManager can view any user's balance (has ReadAll permission)
    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_can_view_any_balance(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_user(&pool, Role::PlatformManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Add some credits to user
        create_initial_credit_transaction(&pool, user.id, "300.0").await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}?include=billing", user.id))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;

        response.assert_status_ok();
        let balance: UserResponse = response.json();
        assert_eq!(balance.id, user.id);
        assert_eq!(balance.credit_balance, Some(300.0));
    }

    // Test: BillingManager can view any user's balance
    #[sqlx::test]
    #[test_log::test]
    async fn test_billing_manager_can_view_any_balance(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let billing_manager = create_test_user(&pool, Role::BillingManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Add some credits to user
        create_initial_credit_transaction(&pool, user.id, "300.0").await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}?include=billing", user.id))
            .add_header(add_auth_headers(&billing_manager).0, add_auth_headers(&billing_manager).1)
            .await;

        response.assert_status_ok();
        let balance: UserResponse = response.json();
        assert_eq!(balance.id, user.id);
        assert_eq!(balance.credit_balance, Some(300.0));
    }
}
