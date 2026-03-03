//! HTTP handlers for organization management endpoints.

use crate::{
    AppState,
    api::models::{
        organizations::{
            AddMemberRequest, ListOrganizationsQuery, OrganizationCreate, OrganizationMemberResponse, OrganizationResponse,
            OrganizationUpdate, SetActiveOrganizationRequest, SetActiveOrganizationResponse, UpdateMemberRoleRequest,
        },
        pagination::PaginatedResponse,
        users::{CurrentUser, UserResponse},
    },
    auth::permissions::{can_manage_org_resource, can_read_all_resources, can_read_own_resource},
    db::handlers::{Organizations, Repository, Users, organizations::OrganizationFilter},
    db::models::organizations::{OrganizationCreateDBRequest, OrganizationUpdateDBRequest},
    errors::{Error, Result},
    types::{Operation, Permission, Resource, UserId, UserIdOrCurrent},
};
use sqlx_pool_router::PoolProvider;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};

const VALID_ROLES: [&str; 3] = ["owner", "admin", "member"];

fn validate_role(role: &str) -> Result<()> {
    if !VALID_ROLES.contains(&role) {
        return Err(Error::BadRequest {
            message: format!("Invalid role '{}'. Must be one of: owner, admin, member", role),
        });
    }
    Ok(())
}

/// Create a new organization. The current user becomes the owner.
#[utoipa::path(
    post,
    path = "/organizations",
    tag = "organizations",
    summary = "Create organization",
    description = "Create a new organization. The authenticated user becomes the owner.",
    responses(
        (status = 201, description = "Organization created", body = OrganizationResponse),
        (status = 400, description = "Bad request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn create_organization<P: PoolProvider>(
    State(state): State<AppState<P>>,
    current_user: CurrentUser,
    Json(data): Json<OrganizationCreate>,
) -> Result<(StatusCode, Json<OrganizationResponse>)> {
    if !crate::auth::permissions::has_permission(&current_user, Resource::Organizations, Operation::CreateOwn) {
        return Err(Error::InsufficientPermissions {
            required: Permission::Allow(Resource::Organizations, Operation::CreateOwn),
            action: Operation::CreateOwn,
            resource: "Organizations".to_string(),
        });
    }

    if data.name.trim().is_empty() {
        return Err(Error::BadRequest {
            message: "Organization name cannot be empty".to_string(),
        });
    }

    let db_request = OrganizationCreateDBRequest {
        name: data.name,
        email: data.email,
        display_name: data.display_name,
        avatar_url: None,
        created_by: current_user.id,
    };

    let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Organizations::new(&mut pool_conn);
    let org = repo.create(&db_request).await?;

    let response = OrganizationResponse::from_user(UserResponse::from(org)).with_member_count(1);

    Ok((StatusCode::CREATED, Json(response)))
}

/// List organizations. Platform managers see all; standard users see their own.
#[utoipa::path(
    get,
    path = "/organizations",
    tag = "organizations",
    summary = "List organizations",
    description = "List organizations. Platform managers see all organizations; standard users see only those they belong to.",
    params(ListOrganizationsQuery),
    responses(
        (status = 200, description = "List of organizations", body = PaginatedResponse<OrganizationResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn list_organizations<P: PoolProvider>(
    State(state): State<AppState<P>>,
    current_user: CurrentUser,
    Query(query): Query<ListOrganizationsQuery>,
) -> Result<Json<PaginatedResponse<OrganizationResponse>>> {
    let can_all = can_read_all_resources(&current_user, Resource::Organizations);

    if can_all {
        // Platform managers: list all organizations
        let skip = query.pagination.skip();
        let limit = query.pagination.limit();
        let filter = OrganizationFilter::new(skip, limit);
        let filter = if let Some(search) = query.search {
            filter.with_search(search)
        } else {
            filter
        };

        let mut pool_conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
        let mut repo = Organizations::new(&mut pool_conn);
        let orgs = repo.list(&filter).await?;
        let total_count = repo.count(&filter).await?;

        let data = orgs
            .into_iter()
            .map(|o| OrganizationResponse::from_user(UserResponse::from(o)))
            .collect();

        Ok(Json(PaginatedResponse {
            data,
            total_count,
            skip,
            limit,
        }))
    } else {
        // Standard users: list only their organizations
        let mut pool_conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
        let mut repo = Organizations::new(&mut pool_conn);
        let memberships = repo.list_user_organizations(current_user.id).await?;

        // Fetch org details for each membership
        let mut users_repo = Users::new(&mut pool_conn);
        let org_ids: Vec<UserId> = memberships.iter().map(|m| m.organization_id).collect();
        let org_map = users_repo.get_bulk(org_ids).await?;

        let data: Vec<OrganizationResponse> = memberships
            .iter()
            .filter_map(|m| {
                org_map
                    .get(&m.organization_id)
                    .map(|o| OrganizationResponse::from_user(UserResponse::from(o.clone())))
            })
            .collect();

        let total_count = data.len() as i64;

        Ok(Json(PaginatedResponse {
            data,
            total_count,
            skip: 0,
            limit: total_count,
        }))
    }
}

/// Get organization details. Must be a member or platform manager.
#[utoipa::path(
    get,
    path = "/organizations/{id}",
    tag = "organizations",
    summary = "Get organization",
    description = "Get organization details. Requires membership or platform manager access.",
    params(
        ("id" = String, Path, description = "Organization ID (UUID)"),
    ),
    responses(
        (status = 200, description = "Organization details", body = OrganizationResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_organization<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(id): Path<UserId>,
    current_user: CurrentUser,
) -> Result<Json<OrganizationResponse>> {
    let can_all = can_read_all_resources(&current_user, Resource::Organizations);

    let mut pool_conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;

    if !can_all {
        let mut repo = Organizations::new(&mut pool_conn);
        let role = repo.get_user_org_role(current_user.id, id).await?;
        if role.is_none() {
            return Err(Error::NotFound {
                resource: "Organization".to_string(),
                id: id.to_string(),
            });
        }
    }

    let mut users_repo = Users::new(&mut pool_conn);
    let org = users_repo.get_by_id(id).await?.ok_or_else(|| Error::NotFound {
        resource: "Organization".to_string(),
        id: id.to_string(),
    })?;

    if org.user_type != "organization" {
        return Err(Error::NotFound {
            resource: "Organization".to_string(),
            id: id.to_string(),
        });
    }

    let mut org_repo = Organizations::new(&mut pool_conn);
    let members = org_repo.list_members(id).await?;

    let response = OrganizationResponse::from_user(UserResponse::from(org)).with_member_count(members.len() as i64);

    Ok(Json(response))
}

/// Update an organization. Must be an owner or admin of the org, or platform manager.
#[utoipa::path(
    patch,
    path = "/organizations/{id}",
    tag = "organizations",
    summary = "Update organization",
    description = "Update organization details. Requires owner/admin role or platform manager access.",
    params(
        ("id" = String, Path, description = "Organization ID (UUID)"),
    ),
    responses(
        (status = 200, description = "Organization updated", body = OrganizationResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn update_organization<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(id): Path<UserId>,
    current_user: CurrentUser,
    Json(data): Json<OrganizationUpdate>,
) -> Result<Json<OrganizationResponse>> {
    let can_all = crate::auth::permissions::has_permission(&current_user, Resource::Organizations, Operation::UpdateAll);

    let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;

    if !can_all {
        let can_org = can_manage_org_resource(&current_user, id, &mut pool_conn).await?;
        if !can_org {
            return Err(Error::InsufficientPermissions {
                required: Permission::Allow(Resource::Organizations, Operation::UpdateOwn),
                action: Operation::UpdateOwn,
                resource: format!("Organization {id}"),
            });
        }
    }

    let db_request = OrganizationUpdateDBRequest {
        display_name: data.display_name,
        avatar_url: None,
        email: data.email,
    };

    let mut repo = Organizations::new(&mut pool_conn);
    let org = repo.update(id, &db_request).await?;

    Ok(Json(OrganizationResponse::from_user(UserResponse::from(org))))
}

/// Delete an organization. Platform managers only.
#[utoipa::path(
    delete,
    path = "/organizations/{id}",
    tag = "organizations",
    summary = "Delete organization",
    description = "Soft-delete an organization. Platform managers only.",
    params(
        ("id" = String, Path, description = "Organization ID (UUID)"),
    ),
    responses(
        (status = 204, description = "Organization deleted"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn delete_organization<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(id): Path<UserId>,
    current_user: CurrentUser,
) -> Result<StatusCode> {
    if !crate::auth::permissions::has_permission(&current_user, Resource::Organizations, Operation::DeleteAll) {
        return Err(Error::InsufficientPermissions {
            required: Permission::Allow(Resource::Organizations, Operation::DeleteAll),
            action: Operation::DeleteAll,
            resource: format!("Organization {id}"),
        });
    }

    let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Organizations::new(&mut pool_conn);

    let deleted = repo.delete(id).await?;
    if !deleted {
        return Err(Error::NotFound {
            resource: "Organization".to_string(),
            id: id.to_string(),
        });
    }

    Ok(StatusCode::NO_CONTENT)
}

/// List members of an organization
#[utoipa::path(
    get,
    path = "/organizations/{id}/members",
    tag = "organizations",
    summary = "List organization members",
    description = "List all members of an organization. Requires membership or platform manager access.",
    params(
        ("id" = String, Path, description = "Organization ID (UUID)"),
    ),
    responses(
        (status = 200, description = "List of members", body = Vec<OrganizationMemberResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn list_members<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(id): Path<UserId>,
    current_user: CurrentUser,
) -> Result<Json<Vec<OrganizationMemberResponse>>> {
    let can_all = can_read_all_resources(&current_user, Resource::Organizations);

    let mut pool_conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;

    if !can_all {
        let mut repo = Organizations::new(&mut pool_conn);
        let role = repo.get_user_org_role(current_user.id, id).await?;
        if role.is_none() {
            return Err(Error::NotFound {
                resource: "Organization".to_string(),
                id: id.to_string(),
            });
        }
    }

    let mut repo = Organizations::new(&mut pool_conn);
    let memberships = repo.list_members(id).await?;

    // Fetch user details for each member
    let user_ids: Vec<UserId> = memberships.iter().map(|m| m.user_id).collect();
    let mut users_repo = Users::new(&mut pool_conn);
    let user_map = users_repo.get_bulk(user_ids).await?;

    let members: Vec<OrganizationMemberResponse> = memberships
        .iter()
        .filter_map(|m| {
            user_map.get(&m.user_id).map(|u| OrganizationMemberResponse {
                user: UserResponse::from(u.clone()),
                role: m.role.clone(),
                created_at: m.created_at,
            })
        })
        .collect();

    Ok(Json(members))
}

/// Add a member to an organization
#[utoipa::path(
    post,
    path = "/organizations/{id}/members",
    tag = "organizations",
    summary = "Add organization member",
    description = "Add a user as a member of an organization. Requires owner/admin role or platform manager access.",
    params(
        ("id" = String, Path, description = "Organization ID (UUID)"),
    ),
    responses(
        (status = 201, description = "Member added", body = OrganizationMemberResponse),
        (status = 400, description = "Bad request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn add_member<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(id): Path<UserId>,
    current_user: CurrentUser,
    Json(data): Json<AddMemberRequest>,
) -> Result<(StatusCode, Json<OrganizationMemberResponse>)> {
    let can_all = crate::auth::permissions::has_permission(&current_user, Resource::Organizations, Operation::UpdateAll);

    let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;

    if !can_all {
        let can_org = can_manage_org_resource(&current_user, id, &mut pool_conn).await?;
        if !can_org {
            return Err(Error::InsufficientPermissions {
                required: Permission::Allow(Resource::Organizations, Operation::UpdateOwn),
                action: Operation::UpdateOwn,
                resource: format!("Organization {id} members"),
            });
        }
    }

    let role = data.role.as_deref().unwrap_or("member");
    validate_role(role)?;

    let mut repo = Organizations::new(&mut pool_conn);
    let membership = repo.add_member(id, data.user_id, role).await?;

    // Fetch user details for response
    let mut users_repo = Users::new(&mut pool_conn);
    let user = users_repo.get_by_id(data.user_id).await?.ok_or_else(|| Error::NotFound {
        resource: "User".to_string(),
        id: data.user_id.to_string(),
    })?;

    Ok((
        StatusCode::CREATED,
        Json(OrganizationMemberResponse {
            user: UserResponse::from(user),
            role: membership.role,
            created_at: membership.created_at,
        }),
    ))
}

/// Update a member's role in an organization
#[utoipa::path(
    patch,
    path = "/organizations/{id}/members/{user_id}",
    tag = "organizations",
    summary = "Update member role",
    description = "Update a member's role in an organization. Requires owner/admin role or platform manager access.",
    params(
        ("id" = String, Path, description = "Organization ID (UUID)"),
        ("user_id" = String, Path, description = "Member user ID (UUID)"),
    ),
    responses(
        (status = 200, description = "Role updated", body = OrganizationMemberResponse),
        (status = 400, description = "Bad request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn update_member_role<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((id, user_id)): Path<(UserId, UserId)>,
    current_user: CurrentUser,
    Json(data): Json<UpdateMemberRoleRequest>,
) -> Result<Json<OrganizationMemberResponse>> {
    let can_all = crate::auth::permissions::has_permission(&current_user, Resource::Organizations, Operation::UpdateAll);

    let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;

    if !can_all {
        let can_org = can_manage_org_resource(&current_user, id, &mut pool_conn).await?;
        if !can_org {
            return Err(Error::InsufficientPermissions {
                required: Permission::Allow(Resource::Organizations, Operation::UpdateOwn),
                action: Operation::UpdateOwn,
                resource: format!("Organization {id} members"),
            });
        }
    }

    validate_role(&data.role)?;

    let mut repo = Organizations::new(&mut pool_conn);
    let membership = repo.update_member_role(id, user_id, &data.role).await?;

    // Fetch user details for response
    let mut users_repo = Users::new(&mut pool_conn);
    let user = users_repo.get_by_id(user_id).await?.ok_or_else(|| Error::NotFound {
        resource: "User".to_string(),
        id: user_id.to_string(),
    })?;

    Ok(Json(OrganizationMemberResponse {
        user: UserResponse::from(user),
        role: membership.role,
        created_at: membership.created_at,
    }))
}

/// Remove a member from an organization
#[utoipa::path(
    delete,
    path = "/organizations/{id}/members/{user_id}",
    tag = "organizations",
    summary = "Remove organization member",
    description = "Remove a member from an organization. Requires owner/admin role or platform manager access.",
    params(
        ("id" = String, Path, description = "Organization ID (UUID)"),
        ("user_id" = String, Path, description = "Member user ID (UUID)"),
    ),
    responses(
        (status = 204, description = "Member removed"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn remove_member<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((id, user_id)): Path<(UserId, UserId)>,
    current_user: CurrentUser,
) -> Result<StatusCode> {
    let can_all = crate::auth::permissions::has_permission(&current_user, Resource::Organizations, Operation::UpdateAll);

    let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;

    if !can_all {
        let can_org = can_manage_org_resource(&current_user, id, &mut pool_conn).await?;
        if !can_org {
            return Err(Error::InsufficientPermissions {
                required: Permission::Allow(Resource::Organizations, Operation::UpdateOwn),
                action: Operation::UpdateOwn,
                resource: format!("Organization {id} members"),
            });
        }
    }

    let mut repo = Organizations::new(&mut pool_conn);
    let removed = repo.remove_member(id, user_id).await?;
    if !removed {
        return Err(Error::NotFound {
            resource: "Organization membership".to_string(),
            id: format!("{user_id} in organization {id}"),
        });
    }

    Ok(StatusCode::NO_CONTENT)
}

/// List organizations a user belongs to
#[utoipa::path(
    get,
    path = "/users/{user_id}/organizations",
    tag = "organizations",
    summary = "List user's organizations",
    description = "List organizations that a user belongs to.",
    params(
        ("user_id" = String, Path, description = "User ID (UUID) or 'current' for current user"),
    ),
    responses(
        (status = 200, description = "List of organizations", body = Vec<crate::api::models::organizations::OrganizationSummary>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn list_user_organizations<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(user_id): Path<UserIdOrCurrent>,
    current_user: CurrentUser,
) -> Result<Json<Vec<crate::api::models::organizations::OrganizationSummary>>> {
    let target_user_id = match user_id {
        UserIdOrCurrent::Current(_) => current_user.id,
        UserIdOrCurrent::Id(uuid) => uuid,
    };

    let can_all = can_read_all_resources(&current_user, Resource::Users);
    let can_own = can_read_own_resource(&current_user, Resource::Users, target_user_id);
    if !can_all && !can_own {
        return Err(Error::InsufficientPermissions {
            required: Permission::Allow(Resource::Users, Operation::ReadOwn),
            action: Operation::ReadOwn,
            resource: format!("Organizations for user {target_user_id}"),
        });
    }

    let mut pool_conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Organizations::new(&mut pool_conn);
    let memberships = repo.list_user_organizations(target_user_id).await?;

    // Fetch org details
    let org_ids: Vec<UserId> = memberships.iter().map(|m| m.organization_id).collect();
    let mut users_repo = Users::new(&mut pool_conn);
    let org_map = users_repo.get_bulk(org_ids).await?;

    let summaries: Vec<crate::api::models::organizations::OrganizationSummary> = memberships
        .iter()
        .filter_map(|m| {
            org_map
                .get(&m.organization_id)
                .map(|o| crate::api::models::organizations::OrganizationSummary {
                    id: o.id,
                    name: o.username.clone(),
                    role: m.role.clone(),
                })
        })
        .collect();

    Ok(Json(summaries))
}

/// Set or clear the active organization context via HttpOnly cookie
#[utoipa::path(
    post,
    path = "/session/organization",
    tag = "organizations",
    summary = "Set active organization",
    description = "Set or clear the active organization context. Sets an HttpOnly cookie.",
    responses(
        (status = 200, description = "Organization context updated", body = SetActiveOrganizationResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn set_active_organization<P: PoolProvider>(
    State(state): State<AppState<P>>,
    current_user: CurrentUser,
    Json(data): Json<SetActiveOrganizationRequest>,
) -> Result<Response> {
    // If organization_id is provided, verify membership
    if let Some(org_id) = data.organization_id {
        let mut pool_conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
        let can_org = can_manage_org_resource(&current_user, org_id, &mut pool_conn).await?;
        if !can_org {
            // Also allow regular members (not just owner/admin)
            let mut repo = Organizations::new(&mut pool_conn);
            let role = repo.get_user_org_role(current_user.id, org_id).await?;
            if role.is_none() {
                return Err(Error::InsufficientPermissions {
                    required: Permission::Allow(Resource::Organizations, Operation::ReadOwn),
                    action: Operation::ReadOwn,
                    resource: format!("Organization {org_id}"),
                });
            }
        }
    }

    let cookie = match data.organization_id {
        Some(org_id) => format!(
            "dw-organization-id={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
            org_id,
            30 * 24 * 3600 // 30 days
        ),
        None => "dw-organization-id=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0".to_string(),
    };

    let body = SetActiveOrganizationResponse {
        active_organization_id: data.organization_id,
    };

    Ok(([(axum::http::header::SET_COOKIE, cookie)], Json(body)).into_response())
}
