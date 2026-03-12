//! HTTP handlers for organization management endpoints.

use crate::{
    AppState,
    api::models::{
        organizations::{
            AddMemberRequest, InviteDetailsResponse, InviteMemberRequest, InviteMemberResponse, ListOrganizationsQuery, OrganizationCreate,
            OrganizationMemberResponse, OrganizationResponse, OrganizationUpdate, SetActiveOrganizationRequest,
            SetActiveOrganizationResponse, UpdateMemberRoleRequest,
        },
        pagination::PaginatedResponse,
        users::{CurrentUser, UserResponse},
    },
    auth::permissions::{can_manage_org_resource, can_read_all_resources, can_read_own_resource},
    db::handlers::{Credits, Organizations, Repository, Users, organizations::OrganizationFilter},
    db::models::organizations::{OrganizationCreateDBRequest, OrganizationUpdateDBRequest},
    email::EmailService,
    errors::{Error, Result},
    types::{Operation, Permission, Resource, UserId, UserIdOrCurrent},
};
use chrono::Duration;
use rust_decimal::prelude::ToPrimitive;
use sha2::{Digest, Sha256};
use sqlx_pool_router::PoolProvider;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::Json,
};

/// Hash a token with SHA-256 for deterministic DB lookup.
/// Since invite tokens are 256 bits of cryptographic randomness,
/// a fast hash is secure enough (no brute-force risk).
fn hash_invite_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

const VALID_ROLES: [&str; 3] = ["owner", "admin", "member"];

fn validate_role(role: &str) -> Result<()> {
    if !VALID_ROLES.contains(&role) {
        return Err(Error::BadRequest {
            message: format!("Invalid role '{}'. Must be one of: owner, admin, member", role),
        });
    }
    Ok(())
}

/// Check that the caller has sufficient privilege to assign the given role.
/// Only owners (or platform managers) can assign the `owner` role.
async fn check_role_assignment_privilege(
    current_user: &CurrentUser,
    org_id: UserId,
    target_role: &str,
    is_platform_manager: bool,
    pool_conn: &mut sqlx::PgConnection,
) -> Result<()> {
    if target_role == "owner" && !is_platform_manager {
        let mut repo = Organizations::new(pool_conn);
        let caller_role = repo.get_user_org_role(current_user.id, org_id).await?;
        if caller_role.as_deref() != Some("owner") {
            return Err(Error::InsufficientPermissions {
                required: Permission::Allow(Resource::Organizations, Operation::UpdateOwn),
                action: Operation::UpdateOwn,
                resource: "Only owners can assign the owner role".to_string(),
            });
        }
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
    if !crate::auth::permissions::has_permission(&current_user, Resource::Organizations, Operation::CreateAll) {
        return Err(Error::InsufficientPermissions {
            required: Permission::Allow(Resource::Organizations, Operation::CreateAll),
            action: Operation::CreateAll,
            resource: "Organizations".to_string(),
        });
    }

    if data.name.trim().is_empty() {
        return Err(Error::BadRequest {
            message: "Organization name cannot be empty".to_string(),
        });
    }

    let owner_id = data.owner_id.unwrap_or(current_user.id);

    let db_request = OrganizationCreateDBRequest {
        name: data.name,
        email: data.email,
        display_name: data.display_name,
        avatar_url: None,
        created_by: owner_id,
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

    let mut response = OrganizationResponse::from_user(UserResponse::from(org)).with_member_count(members.len() as i64);

    // Include credit balance if the user has permission to view billing data
    let can_view_billing = crate::auth::permissions::has_permission(&current_user, Resource::Credits, Operation::ReadAll)
        || crate::auth::permissions::has_permission(&current_user, Resource::Credits, Operation::ReadOwn);
    if can_view_billing {
        let mut credits_repo = Credits::new(&mut pool_conn);
        let balance = credits_repo.get_user_balance(id).await?.to_f64().unwrap_or(0.0);
        response.user = response.user.with_credit_balance(balance);
    }

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

    // Fetch user details for members that have a user_id (excludes pending invites without accounts)
    let user_ids: Vec<UserId> = memberships.iter().filter_map(|m| m.user_id).collect();
    let mut users_repo = Users::new(&mut pool_conn);
    let user_map = users_repo.get_bulk(user_ids).await?;

    let members: Vec<OrganizationMemberResponse> = memberships
        .iter()
        .filter_map(|m| {
            if let Some(uid) = m.user_id {
                // Active member or pending invite for existing user
                user_map.get(&uid).map(|u| OrganizationMemberResponse {
                    id: m.id,
                    user: Some(UserResponse::from(u.clone())),
                    role: m.role.clone(),
                    status: m.status.clone(),
                    created_at: m.created_at,
                    invite_email: m.invite_email.clone(),
                })
            } else {
                // Pending invite for user who hasn't signed up yet
                Some(OrganizationMemberResponse {
                    id: m.id,
                    user: None,
                    role: m.role.clone(),
                    status: m.status.clone(),
                    created_at: m.created_at,
                    invite_email: m.invite_email.clone(),
                })
            }
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

    // Only owners (or platform managers) can assign the owner role
    check_role_assignment_privilege(&current_user, id, role, can_all, &mut pool_conn).await?;

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
            id: membership.id,
            user: Some(UserResponse::from(user)),
            role: membership.role,
            status: membership.status,
            created_at: membership.created_at,
            invite_email: None,
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

    // Use a transaction to prevent TOCTOU race: the owner-count check and role update must be atomic
    let mut tx = state.db.write().begin().await.map_err(|e| Error::Database(e.into()))?;

    if !can_all {
        let can_org = can_manage_org_resource(&current_user, id, &mut tx).await?;
        if !can_org {
            return Err(Error::InsufficientPermissions {
                required: Permission::Allow(Resource::Organizations, Operation::UpdateOwn),
                action: Operation::UpdateOwn,
                resource: format!("Organization {id} members"),
            });
        }
    }

    validate_role(&data.role)?;

    // Only owners (or platform managers) can assign the owner role
    check_role_assignment_privilege(&current_user, id, &data.role, can_all, &mut tx).await?;

    // Prevent demoting the last owner
    let mut repo = Organizations::new(&mut tx);
    if data.role != "owner" {
        let current_role = repo.get_user_org_role(user_id, id).await?;
        if current_role.as_deref() == Some("owner") {
            let members = repo.list_members(id).await?;
            let owner_count = members.iter().filter(|m| m.role == "owner").count();
            if owner_count <= 1 {
                return Err(Error::BadRequest {
                    message: "Cannot demote the last owner. Assign another owner first.".to_string(),
                });
            }
        }
    }

    let membership = repo.update_member_role(id, user_id, &data.role).await?;

    // Fetch user details for response
    let mut users_repo = Users::new(&mut tx);
    let user = users_repo.get_by_id(user_id).await?.ok_or_else(|| Error::NotFound {
        resource: "User".to_string(),
        id: user_id.to_string(),
    })?;

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;
    Ok(Json(OrganizationMemberResponse {
        id: membership.id,
        user: Some(UserResponse::from(user)),
        role: membership.role,
        status: membership.status,
        created_at: membership.created_at,
        invite_email: None,
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

    // Use a transaction to prevent TOCTOU race: the owner-count check and remove must be atomic
    let mut tx = state.db.write().begin().await.map_err(|e| Error::Database(e.into()))?;

    if !can_all {
        let can_org = can_manage_org_resource(&current_user, id, &mut tx).await?;
        if !can_org {
            return Err(Error::InsufficientPermissions {
                required: Permission::Allow(Resource::Organizations, Operation::UpdateOwn),
                action: Operation::UpdateOwn,
                resource: format!("Organization {id} members"),
            });
        }
    }

    let mut repo = Organizations::new(&mut tx);

    // Check if we're removing the last owner
    let target_role = repo.get_user_org_role(user_id, id).await?;
    if let Some(ref role) = target_role
        && role == "owner"
    {
        let members = repo.list_members(id).await?;
        let owner_count = members.iter().filter(|m| m.role == "owner").count();
        if owner_count <= 1 {
            return Err(Error::BadRequest {
                message: "Cannot remove the last owner of an organization. Transfer ownership first.".to_string(),
            });
        }
    }

    let removed = repo.remove_member(id, user_id).await?;
    if !removed {
        return Err(Error::NotFound {
            resource: "Organization membership".to_string(),
            id: format!("{user_id} in organization {id}"),
        });
    }

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;
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

/// Validate and confirm an active organization context.
///
/// Sets a `dw_active_org` cookie so the browser sends it automatically with all
/// subsequent requests.  CLI tools can still use the `X-Organization-Id` header.
#[utoipa::path(
    post,
    path = "/session/organization",
    tag = "organizations",
    summary = "Set active organization",
    description = "Validate organization membership and set a cookie for the active organization context.",
    responses(
        (status = 200, description = "Organization context validated", body = SetActiveOrganizationResponse),
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
) -> Result<(HeaderMap, Json<SetActiveOrganizationResponse>)> {
    // If organization_id is provided, verify membership
    if let Some(org_id) = data.organization_id {
        let mut pool_conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
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

    // Build the dw_active_org cookie using the same security settings as the session cookie
    let session_config = &state.config.auth.native.session;
    let secure = if session_config.cookie_secure { "; Secure" } else { "" };
    let domain = session_config
        .cookie_domain
        .as_ref()
        .map(|d| format!("; Domain={d}"))
        .unwrap_or_default();
    let cookie = if let Some(org_id) = data.organization_id {
        // Set cookie with long max-age (30 days) — cleared explicitly when switching back
        format!(
            "dw_active_org={}; Path=/; HttpOnly{}{}; SameSite={}; Max-Age={}",
            org_id,
            secure,
            domain,
            session_config.cookie_same_site,
            30 * 24 * 60 * 60
        )
    } else {
        // Clear cookie
        format!(
            "dw_active_org=; Path=/; HttpOnly{}{}; SameSite={}; Max-Age=0",
            secure, domain, session_config.cookie_same_site
        )
    };

    let mut headers = HeaderMap::new();
    headers.insert(header::SET_COOKIE, cookie.parse().unwrap());

    Ok((
        headers,
        Json(SetActiveOrganizationResponse {
            active_organization_id: data.organization_id,
        }),
    ))
}

/// Invite a user to an organization by email
#[utoipa::path(
    post,
    path = "/organizations/{id}/invites",
    tag = "organizations",
    summary = "Invite member by email",
    description = "Send an invitation email to join the organization. Requires owner/admin role or platform manager access.",
    params(
        ("id" = String, Path, description = "Organization ID (UUID)"),
    ),
    responses(
        (status = 201, description = "Invite created", body = InviteMemberResponse),
        (status = 400, description = "Bad request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 409, description = "Conflict - already a member or pending invite"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn invite_member<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(id): Path<UserId>,
    current_user: CurrentUser,
    Json(data): Json<InviteMemberRequest>,
) -> Result<(StatusCode, Json<InviteMemberResponse>)> {
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

    // Basic email validation
    let email = data.email.trim().to_lowercase();
    if !email.contains('@') || !email.contains('.') {
        return Err(Error::BadRequest {
            message: "Invalid email address".to_string(),
        });
    }

    let role = data.role.as_deref().unwrap_or("member");
    validate_role(role)?;

    // Only owners (or platform managers) can assign the owner role
    check_role_assignment_privilege(&current_user, id, role, can_all, &mut pool_conn).await?;

    // Check if email is already an active member
    let mut users_repo = Users::new(&mut pool_conn);
    let existing_user = users_repo.get_user_by_email(&email).await?;
    let existing_user_id = existing_user.as_ref().map(|u| u.id);

    if let Some(ref user) = existing_user {
        let mut org_repo = Organizations::new(&mut pool_conn);
        let existing_role = org_repo.get_user_org_role(user.id, id).await?;
        if existing_role.is_some() {
            return Err(Error::Conflict {
                message: "User is already an active member of this organization".to_string(),
                conflicts: None,
            });
        }
    }

    // Generate invite token and hash
    let mut org_repo = Organizations::new(&mut pool_conn);
    let token = crate::auth::password::generate_reset_token();
    let token_hash = hash_invite_token(&token);
    let expires_at = chrono::Utc::now() + Duration::days(7);

    // Create the invite
    let invite = org_repo
        .create_invite(id, existing_user_id, &email, role, current_user.id, &token_hash, expires_at)
        .await?;

    // Get org name and inviter name for the email
    let mut users_repo = Users::new(&mut pool_conn);
    let org_user = users_repo.get_by_id(id).await?;
    let org_name = org_user
        .as_ref()
        .and_then(|u| u.display_name.clone())
        .unwrap_or_else(|| org_user.as_ref().map(|u| u.username.clone()).unwrap_or_default());

    let inviter = users_repo.get_by_id(current_user.id).await?;
    let inviter_name = inviter
        .as_ref()
        .and_then(|u| u.display_name.clone())
        .unwrap_or_else(|| inviter.as_ref().map(|u| u.username.clone()).unwrap_or_default());

    // Send invite email
    let invite_link = format!("{}/org-invite?token={}", state.config.dashboard_url.trim_end_matches('/'), token);
    let email_service = EmailService::new(&state.config)?;
    if let Err(e) = email_service
        .send_org_invite_email(&email, &org_name, &inviter_name, role, &invite_link)
        .await
    {
        tracing::warn!("Failed to send invite email to {email}: {e}");
    }

    Ok((
        StatusCode::CREATED,
        Json(InviteMemberResponse {
            id: invite.id,
            email,
            role: invite.role,
            status: invite.status,
            created_at: invite.created_at,
            expires_at: invite.expires_at.expect("invite must have expires_at"),
        }),
    ))
}

/// Get details about a pending invite by token
#[utoipa::path(
    get,
    path = "/organizations/invites/{token}",
    tag = "organizations",
    summary = "Get invite details",
    description = "Look up a pending invite by its token. Returns organization name, role, and inviter info.",
    params(
        ("token" = String, Path, description = "Invite token"),
    ),
    responses(
        (status = 200, description = "Invite details", body = InviteDetailsResponse),
        (status = 401, description = "Unauthorized"),
        (status = 400, description = "Bad request - invite has expired"),
        (status = 404, description = "Not found"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_invite_details<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(token): Path<String>,
    _current_user: CurrentUser,
) -> Result<Json<InviteDetailsResponse>> {
    let token_hash = hash_invite_token(&token);

    let mut pool_conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut org_repo = Organizations::new(&mut pool_conn);

    let invite = org_repo
        .find_invite_by_token_hash(&token_hash)
        .await?
        .ok_or_else(|| Error::NotFound {
            resource: "Invite".to_string(),
            id: "invalid or expired token".to_string(),
        })?;

    // Check expiry
    if let Some(expires_at) = invite.expires_at
        && expires_at < chrono::Utc::now()
    {
        return Err(Error::BadRequest {
            message: "This invite has expired".to_string(),
        });
    }

    // Get org name
    let mut users_repo = Users::new(&mut pool_conn);
    let org_user = users_repo.get_by_id(invite.organization_id).await?;
    let org_name = org_user
        .as_ref()
        .and_then(|u| u.display_name.clone())
        .unwrap_or_else(|| org_user.as_ref().map(|u| u.username.clone()).unwrap_or_default());

    // Get inviter name
    let inviter_name = if let Some(invited_by) = invite.invited_by {
        let inviter = users_repo.get_by_id(invited_by).await?;
        inviter.and_then(|u| u.display_name.or(Some(u.username)))
    } else {
        None
    };

    Ok(Json(InviteDetailsResponse {
        org_name,
        role: invite.role,
        inviter_name,
        expires_at: invite.expires_at.expect("invite must have expires_at"),
    }))
}

/// Accept a pending invite
#[utoipa::path(
    post,
    path = "/organizations/invites/{token}/accept",
    tag = "organizations",
    summary = "Accept invite",
    description = "Accept a pending organization invite. The authenticated user's email must match the invite email.",
    params(
        ("token" = String, Path, description = "Invite token"),
    ),
    responses(
        (status = 200, description = "Invite accepted"),
        (status = 400, description = "Bad request - expired or email mismatch"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Not found"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn accept_invite<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(token): Path<String>,
    current_user: CurrentUser,
) -> Result<Json<serde_json::Value>> {
    let token_hash = hash_invite_token(&token);

    let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut org_repo = Organizations::new(&mut pool_conn);

    let invite = org_repo
        .find_invite_by_token_hash(&token_hash)
        .await?
        .ok_or_else(|| Error::NotFound {
            resource: "Invite".to_string(),
            id: "invalid or expired token".to_string(),
        })?;

    // Check expiry
    if let Some(expires_at) = invite.expires_at
        && expires_at < chrono::Utc::now()
    {
        return Err(Error::BadRequest {
            message: "This invite has expired".to_string(),
        });
    }

    // Verify the current user's email matches the invite
    if let Some(ref invite_email) = invite.invite_email
        && current_user.email.to_lowercase() != invite_email.to_lowercase()
    {
        return Err(Error::BadRequest {
            message: "Your email address does not match this invite".to_string(),
        });
    }

    org_repo.accept_invite(invite.id, current_user.id).await?;

    Ok(Json(serde_json::json!({ "message": "Invite accepted" })))
}

/// Decline a pending invite
#[utoipa::path(
    post,
    path = "/organizations/invites/{token}/decline",
    tag = "organizations",
    summary = "Decline invite",
    description = "Decline a pending organization invite. The authenticated user's email must match the invite email.",
    params(
        ("token" = String, Path, description = "Invite token"),
    ),
    responses(
        (status = 200, description = "Invite declined"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Not found"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn decline_invite<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(token): Path<String>,
    current_user: CurrentUser,
) -> Result<Json<serde_json::Value>> {
    let token_hash = hash_invite_token(&token);

    let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut org_repo = Organizations::new(&mut pool_conn);

    let invite = org_repo
        .find_invite_by_token_hash(&token_hash)
        .await?
        .ok_or_else(|| Error::NotFound {
            resource: "Invite".to_string(),
            id: "invalid or expired token".to_string(),
        })?;

    // Verify the current user's email matches the invite
    if let Some(ref invite_email) = invite.invite_email
        && current_user.email.to_lowercase() != invite_email.to_lowercase()
    {
        return Err(Error::BadRequest {
            message: "Your email address does not match this invite".to_string(),
        });
    }

    // Delete the pending invite row
    org_repo.cancel_invite(invite.organization_id, invite.id).await?;

    Ok(Json(serde_json::json!({ "message": "Invite declined" })))
}

/// Cancel a pending invite (by org admin/owner)
#[utoipa::path(
    delete,
    path = "/organizations/{id}/invites/{invite_id}",
    tag = "organizations",
    summary = "Cancel invite",
    description = "Cancel a pending invite. Requires owner/admin role or platform manager access.",
    params(
        ("id" = String, Path, description = "Organization ID (UUID)"),
        ("invite_id" = String, Path, description = "Invite row ID (UUID)"),
    ),
    responses(
        (status = 204, description = "Invite cancelled"),
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
pub async fn cancel_invite<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((id, invite_id)): Path<(UserId, UserId)>,
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
                resource: format!("Organization {id} invites"),
            });
        }
    }

    let mut org_repo = Organizations::new(&mut pool_conn);
    let cancelled = org_repo.cancel_invite(id, invite_id).await?;
    if !cancelled {
        return Err(Error::NotFound {
            resource: "Pending invite".to_string(),
            id: format!("{invite_id} in organization {id}"),
        });
    }

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use crate::api::models::users::Role;
    use crate::test::utils::{add_auth_headers, create_test_admin_user, create_test_app, create_test_user};
    use serde_json::json;
    use sqlx::PgPool;

    // ── Last-owner guard ─────────────────────────────────────────────────

    #[sqlx::test]
    #[test_log::test]
    async fn test_cannot_remove_last_owner(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
        let admin_headers = add_auth_headers(&admin);

        let owner = create_test_user(&pool, Role::StandardUser).await;

        // Platform manager creates an org with owner as the designated owner
        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&admin_headers[0].0, &admin_headers[0].1)
            .add_header(&admin_headers[1].0, &admin_headers[1].1)
            .json(&json!({ "name": "test-org-last-owner", "email": "org@example.com", "owner_id": owner.id }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

        // Try to remove the only owner (via platform admin) — should fail
        let resp = server
            .delete(&format!("/admin/api/v1/organizations/{org_id}/members/{}", owner.id))
            .add_header(&admin_headers[0].0, &admin_headers[0].1)
            .add_header(&admin_headers[1].0, &admin_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
        let body = resp.text();
        assert!(body.contains("last owner"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_can_remove_owner_when_another_exists(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
        let admin_headers = add_auth_headers(&admin);

        let owner1 = create_test_user(&pool, Role::StandardUser).await;
        let owner2 = create_test_user(&pool, Role::StandardUser).await;

        // Platform manager creates an org with owner1 as the designated owner
        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&admin_headers[0].0, &admin_headers[0].1)
            .add_header(&admin_headers[1].0, &admin_headers[1].1)
            .json(&json!({ "name": "test-org-two-owners", "email": "org@example.com", "owner_id": owner1.id }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

        // Admin adds owner2 as owner
        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/members"))
            .add_header(&admin_headers[0].0, &admin_headers[0].1)
            .add_header(&admin_headers[1].0, &admin_headers[1].1)
            .json(&json!({ "user_id": owner2.id, "role": "owner" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);

        // Now removing owner1 should succeed (owner2 still exists)
        let resp = server
            .delete(&format!("/admin/api/v1/organizations/{org_id}/members/{}", owner1.id))
            .add_header(&admin_headers[0].0, &admin_headers[0].1)
            .add_header(&admin_headers[1].0, &admin_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::NO_CONTENT);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_cannot_demote_last_owner(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
        let admin_headers = add_auth_headers(&admin);

        let owner = create_test_user(&pool, Role::StandardUser).await;

        // Platform manager creates an org with owner as the designated owner
        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&admin_headers[0].0, &admin_headers[0].1)
            .add_header(&admin_headers[1].0, &admin_headers[1].1)
            .json(&json!({ "name": "test-org-demote", "email": "org@example.com", "owner_id": owner.id }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

        // Try to demote the only owner to member — should fail
        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}/members/{}", owner.id))
            .add_header(&admin_headers[0].0, &admin_headers[0].1)
            .add_header(&admin_headers[1].0, &admin_headers[1].1)
            .json(&json!({ "role": "member" }))
            .await;
        resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
        let body = resp.text();
        assert!(body.contains("last owner"));
    }

    // ── Privilege escalation prevention ──────────────────────────────────

    #[sqlx::test]
    #[test_log::test]
    async fn test_admin_cannot_assign_owner_role(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);

        let owner = create_test_user(&pool, Role::StandardUser).await;
        let org_admin = create_test_user(&pool, Role::StandardUser).await;
        let org_admin_headers = add_auth_headers(&org_admin);
        let member = create_test_user(&pool, Role::StandardUser).await;

        // Platform manager creates an org with owner as the designated owner
        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .json(&json!({ "name": "test-org-priv-esc", "email": "org@example.com", "owner_id": owner.id }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

        // Platform manager adds org_admin as admin
        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/members"))
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .json(&json!({ "user_id": org_admin.id, "role": "admin" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);

        // Platform manager adds member
        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/members"))
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .json(&json!({ "user_id": member.id, "role": "member" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);

        // Org admin tries to promote member to owner — should fail
        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}/members/{}", member.id))
            .add_header(&org_admin_headers[0].0, &org_admin_headers[0].1)
            .add_header(&org_admin_headers[1].0, &org_admin_headers[1].1)
            .json(&json!({ "role": "owner" }))
            .await;
        resp.assert_status(axum::http::StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_admin_cannot_add_member_as_owner(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);

        let owner = create_test_user(&pool, Role::StandardUser).await;
        let org_admin = create_test_user(&pool, Role::StandardUser).await;
        let org_admin_headers = add_auth_headers(&org_admin);
        let new_user = create_test_user(&pool, Role::StandardUser).await;

        // Platform manager creates an org with owner as the designated owner
        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .json(&json!({ "name": "test-org-add-owner", "email": "org@example.com", "owner_id": owner.id }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

        // Platform manager adds org_admin as admin
        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/members"))
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .json(&json!({ "user_id": org_admin.id, "role": "admin" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);

        // Org admin tries to add new_user as owner — should fail
        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/members"))
            .add_header(&org_admin_headers[0].0, &org_admin_headers[0].1)
            .add_header(&org_admin_headers[1].0, &org_admin_headers[1].1)
            .json(&json!({ "user_id": new_user.id, "role": "owner" }))
            .await;
        resp.assert_status(axum::http::StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_owner_can_assign_owner_role(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);

        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);
        let new_user = create_test_user(&pool, Role::StandardUser).await;

        // Platform manager creates an org with owner as the designated owner
        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .json(&json!({ "name": "test-org-owner-assign", "email": "org@example.com", "owner_id": owner.id }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

        // Owner adds new_user as owner — should succeed
        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/members"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "user_id": new_user.id, "role": "owner" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let body = resp.json::<serde_json::Value>();
        assert_eq!(body["role"].as_str().unwrap(), "owner");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_can_assign_owner_role(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;

        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let new_user = create_test_user(&pool, Role::StandardUser).await;

        // Platform manager creates an org with owner as the designated owner
        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .json(&json!({ "name": "test-org-pm-assign", "email": "org@example.com", "owner_id": owner.id }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

        // Platform manager adds new_user as owner — should succeed
        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/members"))
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .json(&json!({ "user_id": new_user.id, "role": "owner" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let body = resp.json::<serde_json::Value>();
        assert_eq!(body["role"].as_str().unwrap(), "owner");
    }

    // ── Validation endpoint ──────────────────────────────────────────────

    #[sqlx::test]
    #[test_log::test]
    async fn test_set_active_organization_validates_membership(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);

        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);
        let non_member = create_test_user(&pool, Role::StandardUser).await;
        let non_member_headers = add_auth_headers(&non_member);

        // Platform manager creates an org with owner as the designated owner
        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .json(&json!({ "name": "test-org-session", "email": "org@example.com", "owner_id": owner.id }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

        // Owner can set active org
        let resp = server
            .post("/admin/api/v1/session/organization")
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "organization_id": org_id }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        let body = resp.json::<serde_json::Value>();
        assert_eq!(body["active_organization_id"].as_str().unwrap(), org_id);

        // Non-member cannot set active org
        let resp = server
            .post("/admin/api/v1/session/organization")
            .add_header(&non_member_headers[0].0, &non_member_headers[0].1)
            .add_header(&non_member_headers[1].0, &non_member_headers[1].1)
            .json(&json!({ "organization_id": org_id }))
            .await;
        resp.assert_status(axum::http::StatusCode::FORBIDDEN);

        // Clearing active org always succeeds
        let resp = server
            .post("/admin/api/v1/session/organization")
            .add_header(&non_member_headers[0].0, &non_member_headers[0].1)
            .add_header(&non_member_headers[1].0, &non_member_headers[1].1)
            .json(&json!({ "organization_id": null }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
    }
}
