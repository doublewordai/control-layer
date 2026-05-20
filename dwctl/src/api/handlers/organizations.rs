//! HTTP handlers for organization management endpoints.

use crate::{
    AppState,
    api::models::{
        organizations::{
            AddMemberRequest, InviteDetailsResponse, InviteMemberRequest, InviteMemberResponse, ListOrganizationsQuery, OrganizationCreate,
            OrganizationMemberResponse, OrganizationResponse, OrganizationUpdate, PendingEmailChangeResponse, SetActiveOrganizationRequest,
            SetActiveOrganizationResponse, UpdateMemberRoleRequest,
        },
        pagination::PaginatedResponse,
        users::{CurrentUser, UserResponse},
    },
    auth::permissions::{can_manage_org_resource, can_read_all_resources, can_read_own_resource},
    db::handlers::{Credits, Organizations, Repository, Users, api_keys::ApiKeys, organizations::OrganizationFilter},
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

/// Maximum number of organizations a user can belong to simultaneously.
const MAX_ORGS_PER_USER: i64 = 3;

/// Hash a token with SHA-256 for deterministic DB lookup.
/// Since invite tokens are 256 bits of cryptographic randomness,
/// a fast hash is secure enough (no brute-force risk).
fn hash_invite_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
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

/// Validate and normalize a contact email address. Returns the trimmed,
/// lowercased form, or `Error::BadRequest` if the address can't be parsed
/// as an SMTP mailbox.
fn validate_contact_email(input: &str) -> Result<String> {
    let trimmed = input.trim().to_lowercase();
    if trimmed.is_empty() {
        return Err(Error::BadRequest {
            message: "Email address cannot be empty".to_string(),
        });
    }
    if trimmed.parse::<lettre::Address>().is_err() {
        return Err(Error::BadRequest {
            message: "Invalid email address".to_string(),
        });
    }
    Ok(trimmed)
}

/// How long a pending email-change verification token stays valid.
const EMAIL_CHANGE_TOKEN_TTL_HOURS: i64 = 24;

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
    let is_platform_manager = crate::auth::permissions::has_permission(&current_user, Resource::Organizations, Operation::CreateAll);

    if !is_platform_manager && !crate::auth::permissions::has_permission(&current_user, Resource::Organizations, Operation::CreateOwn) {
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

    let email = validate_contact_email(&data.email)?;

    // Only platform managers can specify a different owner
    let owner_id = if is_platform_manager {
        data.owner_id.unwrap_or(current_user.id)
    } else {
        if data.owner_id.is_some() {
            return Err(Error::InsufficientPermissions {
                required: Permission::Allow(Resource::Organizations, Operation::CreateAll),
                action: Operation::CreateAll,
                resource: "Organization owner assignment".to_string(),
            });
        }
        current_user.id
    };

    let db_request = OrganizationCreateDBRequest {
        name: data.name,
        email,
        display_name: data.display_name,
        avatar_url: None,
        created_by: owner_id,
    };

    let config = state.current_config();
    let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Organizations::new(&mut pool_conn);

    // Check org membership limit for the owner
    let org_count = repo.count_user_organizations(owner_id).await?;
    if org_count >= MAX_ORGS_PER_USER {
        return Err(Error::BadRequest {
            message: format!("Cannot create organization: user is already a member of {MAX_ORGS_PER_USER} organizations (maximum)"),
        });
    }

    let org = repo.create(&db_request, &config.auth.default_user_roles).await?;

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

    // Look up the current org so we can compare emails and address verification emails.
    let mut users_repo = Users::new(&mut pool_conn);
    let current_org = users_repo.get_by_id(id).await?.ok_or_else(|| Error::NotFound {
        resource: "Organization".to_string(),
        id: id.to_string(),
    })?;
    if current_org.user_type != "organization" {
        return Err(Error::NotFound {
            resource: "Organization".to_string(),
            id: id.to_string(),
        });
    }

    // If an email change is requested, validate the format and (when it's actually
    // different from the current address) route it through the verification flow
    // instead of applying it directly. The contact email is rendered into Stripe
    // receipts, invitation emails and audit notifications, so a silent change
    // could redirect security-sensitive mail to an attacker.
    let mut pending_email_info: Option<PendingEmailChangeResponse> = None;
    if let Some(ref requested) = data.email {
        let normalized = validate_contact_email(requested)?;
        if normalized != current_org.email.to_lowercase() {
            let token = crate::auth::password::generate_reset_token();
            let token_hash = hash_invite_token(&token);
            let expires_at = chrono::Utc::now() + Duration::hours(EMAIL_CHANGE_TOKEN_TTL_HOURS);

            // Single UPSERT atomically supersedes any prior pending change for this org,
            // invalidating the older verification token in the same statement.
            let mut org_repo = Organizations::new(&mut pool_conn);
            let pending = org_repo
                .upsert_pending_email_change(id, &normalized, current_user.id, &token_hash, expires_at)
                .await?;

            // Best-effort: send a verification link to the new address and a notice to
            // the current address. The verification row already gates the actual email
            // change, so we never let mail failures (transport down, template error,
            // service misconfiguration) roll back the API call or hide the pending
            // state from the client. Each failure is logged with structured fields so
            // ops can alert on the notice-to-old-address path specifically — that one
            // is the user's heads-up that someone is trying to take over the org.
            let config = state.current_config();
            let org_name = current_org.display_name.clone().unwrap_or_else(|| current_org.username.clone());
            // The verification link is a backend GET that returns HTML, so it works
            // straight from any mail client (no dashboard route needed). The backend
            // and dashboard share an origin in production deployments, so
            // `dashboard_url` is the right base — matches how invite links are built.
            let confirm_link = format!(
                "{}/admin/api/v1/organizations/email-change/{}/confirm",
                config.dashboard_url.trim_end_matches('/'),
                token,
            );

            match EmailService::new(&config) {
                Ok(email_service) => {
                    if let Err(error) = email_service
                        .send_org_email_change_verify(&normalized, &org_name, &confirm_link)
                        .await
                    {
                        tracing::warn!(
                            org_id = %id,
                            new_email = %normalized,
                            error = %error,
                            kind = "org_email_change_verify_failed",
                            "Failed to send org email-change verify email to new address",
                        );
                    }
                    if let Err(error) = email_service
                        .send_org_email_change_notice(&current_org.email, &org_name, &normalized, Some(&config.support_email))
                        .await
                    {
                        // Promote to a structured warn because failure here is
                        // security-relevant: an attacker benefits from the legitimate
                        // owner not being notified of the change request.
                        tracing::warn!(
                            org_id = %id,
                            old_email = %current_org.email,
                            new_email = %normalized,
                            error = %error,
                            kind = "org_email_change_notice_failed",
                            "Failed to send org email-change NOTICE to current address — legitimate owner not warned",
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        org_id = %id,
                        error = %error,
                        kind = "org_email_change_email_service_unavailable",
                        "Email service unavailable for org email-change verification",
                    );
                }
            }

            pending_email_info = Some(PendingEmailChangeResponse::from(pending));
        }
    }

    // Apply non-email updates only. The email never flows through this DB call:
    // either it was unchanged, or it's now gated behind the verification flow above.
    let db_request = OrganizationUpdateDBRequest {
        display_name: data.display_name,
        avatar_url: None,
        email: None,
        batch_notifications_enabled: data.batch_notifications_enabled,
        low_balance_threshold: data.low_balance_threshold,
    };

    let mut repo = Organizations::new(&mut pool_conn);
    let org = repo.update(id, &db_request).await?;

    let mut response = OrganizationResponse::from_user(UserResponse::from(org));
    if let Some(info) = pending_email_info {
        response = response.with_pending_email_change(info);
    }

    Ok(Json(response))
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

    // Check org membership limit for the target user
    let org_count = repo.count_user_organizations(data.user_id).await?;
    if org_count >= MAX_ORGS_PER_USER {
        return Err(Error::BadRequest {
            message: format!("Cannot add member: user is already a member of {MAX_ORGS_PER_USER} organizations (maximum)"),
        });
    }

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

    // Soft-delete the removed member's org API keys
    let mut api_keys_repo = ApiKeys::new(&mut tx);
    let keys_deleted = api_keys_repo.soft_delete_member_org_keys(id, user_id).await?;
    if keys_deleted > 0 {
        tracing::info!("Soft-deleted {keys_deleted} API key(s) for removed member {user_id} in org {id}");
    }

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Leave an organization (self-removal)
#[utoipa::path(
    post,
    path = "/organizations/{id}/leave",
    tag = "organizations",
    summary = "Leave organization",
    description = "Leave an organization voluntarily. Cannot leave if you are the last owner.",
    params(
        ("id" = String, Path, description = "Organization ID (UUID)"),
    ),
    responses(
        (status = 204, description = "Left organization"),
        (status = 400, description = "Bad request - last owner cannot leave"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Not found - not a member"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn leave_organization<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(id): Path<UserId>,
    current_user: CurrentUser,
) -> Result<StatusCode> {
    let mut tx = state.db.write().begin().await.map_err(|e| Error::Database(e.into()))?;

    let mut repo = Organizations::new(&mut tx);

    // Verify user is a member
    let role = repo.get_user_org_role(current_user.id, id).await?;
    if role.is_none() {
        return Err(Error::NotFound {
            resource: "Organization membership".to_string(),
            id: format!("{} in organization {id}", current_user.id),
        });
    }

    // Prevent last owner from leaving
    if role.as_deref() == Some("owner") {
        let members = repo.list_members(id).await?;
        let owner_count = members.iter().filter(|m| m.role == "owner").count();
        if owner_count <= 1 {
            return Err(Error::BadRequest {
                message: "Cannot leave as the last owner. Transfer ownership first.".to_string(),
            });
        }
    }

    repo.remove_member(id, current_user.id).await?;

    // Soft-delete the leaving member's org API keys
    let mut api_keys_repo = ApiKeys::new(&mut tx);
    let keys_deleted = api_keys_repo.soft_delete_member_org_keys(id, current_user.id).await?;
    if keys_deleted > 0 {
        tracing::info!(
            "Soft-deleted {keys_deleted} API key(s) for user {} leaving org {id}",
            current_user.id
        );
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
    let config = state.current_config();
    let session_config = &config.auth.native.session;
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
    let config = state.current_config();
    let invite_link = format!("{}/org-invite?token={}", config.dashboard_url.trim_end_matches('/'), token);
    let email_service = EmailService::new(&config)?;
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

    // Check org membership limit
    let org_count = org_repo.count_user_organizations(current_user.id).await?;
    if org_count >= MAX_ORGS_PER_USER {
        return Err(Error::BadRequest {
            message: format!("Cannot accept invite: you are already a member of {MAX_ORGS_PER_USER} organizations (maximum)"),
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

/// Confirm a pending organization email change by consuming a verification token.
///
/// The token is sent (via `update_organization`) to the requested new contact
/// address. Mail clients render the link as a plain anchor, so this is a `GET`
/// endpoint that returns a small HTML page describing the result — no
/// dashboard route is required to make the link work, and no separate API
/// call is needed.
///
/// No authentication is required because the secret token itself proves
/// possession of the new mailbox. (Note: this codebase's authentication is
/// opt-in via the `CurrentUser` extractor on each handler, not via a router
/// layer around `/admin/api/v1`, so the absence of `CurrentUser` here is what
/// makes the endpoint public.)
#[utoipa::path(
    get,
    path = "/organizations/email-change/{token}/confirm",
    tag = "organizations",
    summary = "Confirm an organization email change",
    description = "Apply a pending organization contact email change using the verification token from the email sent to the new address. Returns an HTML confirmation page.",
    params(
        ("token" = String, Path, description = "Email change verification token"),
    ),
    responses(
        (status = 200, description = "Email change applied (HTML page)"),
        (status = 400, description = "Token has expired (HTML page)"),
        (status = 404, description = "Invalid or already-consumed token (HTML page)"),
    ),
)]
#[tracing::instrument(skip_all)]
pub async fn confirm_email_change<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(token): Path<String>,
) -> Result<axum::response::Response> {
    let token_hash = hash_invite_token(&token);

    // The consume + update pair runs in a single transaction so a failure in
    // the email write rolls back the DELETE — leaving the token usable for a
    // retry rather than silently consumed with no email change applied.
    let mut tx = state.db.write().begin().await.map_err(|e| Error::Database(e.into()))?;
    let mut org_repo = Organizations::new(&mut tx);

    // Atomic claim: `DELETE ... RETURNING` ensures two concurrent confirmations
    // cannot both succeed, and a PATCH that supersedes this row cannot race
    // with the lookup. If the row was already consumed/replaced (or belongs
    // to a soft-deleted org), we return 404.
    let pending = match org_repo.consume_pending_email_change(&token_hash).await? {
        Some(p) => p,
        None => {
            return Ok(email_change_html(
                StatusCode::NOT_FOUND,
                "This confirmation link is invalid or has already been used.",
            ));
        }
    };

    if pending.expires_at < chrono::Utc::now() {
        // Commit the DELETE so an expired token can't be probed repeatedly,
        // and log so ops can correlate user complaints to a real (expired) row.
        tx.commit().await.map_err(|e| Error::Database(e.into()))?;
        tracing::info!(
            org_id = %pending.organization_id,
            expired_at = %pending.expires_at,
            "Org email-change token consumed after expiry",
        );
        return Ok(email_change_html(
            StatusCode::BAD_REQUEST,
            "This confirmation link has expired. Please request the email change again from the dashboard.",
        ));
    }

    let update = OrganizationUpdateDBRequest {
        display_name: None,
        avatar_url: None,
        email: Some(pending.new_email.clone()),
        batch_notifications_enabled: None,
        low_balance_threshold: None,
    };
    org_repo.update(pending.organization_id, &update).await?;

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(email_change_html(
        StatusCode::OK,
        "Your organization's contact email has been updated. You can close this tab.",
    ))
}

/// Render a minimal HTML confirmation page for the email-change flow.
fn email_change_html(status: StatusCode, message: &str) -> axum::response::Response {
    let body = format!(
        r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>Email change</title></head>
<body style="font-family: system-ui, sans-serif; max-width: 540px; margin: 60px auto; padding: 20px; color: #333;">
<h2>Organization contact email</h2>
<p>{}</p>
</body></html>"#,
        html_escape(message),
    );
    axum::response::Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(body.into())
        .expect("static HTML response builds")
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use crate::api::models::users::Role;
    use crate::test::utils::{
        add_auth_headers, create_test_admin_user, create_test_app, create_test_app_with_config, create_test_config, create_test_user,
    };
    use serde_json::json;
    use sqlx::PgPool;

    // ── Self-serve org creation ────────────────────────────────────────────

    #[sqlx::test]
    #[test_log::test]
    async fn test_standard_user_can_create_organization(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let user_headers = add_auth_headers(&user);

        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&user_headers[0].0, &user_headers[0].1)
            .add_header(&user_headers[1].0, &user_headers[1].1)
            .json(&json!({ "name": "my-org", "email": "contact@my-org.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let body = resp.json::<serde_json::Value>();
        assert_eq!(body["username"].as_str().unwrap(), "my-org");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_standard_user_becomes_owner_of_created_org(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let user_headers = add_auth_headers(&user);

        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&user_headers[0].0, &user_headers[0].1)
            .add_header(&user_headers[1].0, &user_headers[1].1)
            .json(&json!({ "name": "self-serve-org", "email": "contact@example.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

        // Verify the creator is listed as owner
        let resp = server
            .get(&format!("/admin/api/v1/organizations/{org_id}/members"))
            .add_header(&user_headers[0].0, &user_headers[0].1)
            .add_header(&user_headers[1].0, &user_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        let members = resp.json::<Vec<serde_json::Value>>();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0]["role"].as_str().unwrap(), "owner");
        assert_eq!(members[0]["user"]["id"].as_str().unwrap(), user.id.to_string());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_standard_user_cannot_set_owner_id(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let user_headers = add_auth_headers(&user);
        let other = create_test_user(&pool, Role::StandardUser).await;

        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&user_headers[0].0, &user_headers[0].1)
            .add_header(&user_headers[1].0, &user_headers[1].1)
            .json(&json!({ "name": "hijack-org", "email": "x@example.com", "owner_id": other.id }))
            .await;
        resp.assert_status(axum::http::StatusCode::FORBIDDEN);
    }

    // ── Org membership limit ──────────────────────────────────────────────

    #[sqlx::test]
    #[test_log::test]
    async fn test_cannot_create_org_when_at_limit(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let user_headers = add_auth_headers(&user);

        // Create MAX_ORGS_PER_USER orgs
        for i in 0..super::MAX_ORGS_PER_USER {
            let resp = server
                .post("/admin/api/v1/organizations")
                .add_header(&user_headers[0].0, &user_headers[0].1)
                .add_header(&user_headers[1].0, &user_headers[1].1)
                .json(&json!({ "name": format!("org-{i}"), "email": format!("org{i}@example.com") }))
                .await;
            resp.assert_status(axum::http::StatusCode::CREATED);
        }

        // Next one should fail
        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&user_headers[0].0, &user_headers[0].1)
            .add_header(&user_headers[1].0, &user_headers[1].1)
            .json(&json!({ "name": "one-too-many", "email": "extra@example.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
        let body = resp.text();
        assert!(body.contains("maximum"));
    }

    // ── Leave organization ────────────────────────────────────────────────

    #[sqlx::test]
    #[test_log::test]
    async fn test_member_can_leave_organization(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);

        let owner = create_test_user(&pool, Role::StandardUser).await;
        let member = create_test_user(&pool, Role::StandardUser).await;
        let member_headers = add_auth_headers(&member);

        // PM creates org with owner
        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .json(&json!({ "name": "leave-org", "email": "org@example.com", "owner_id": owner.id }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

        // PM adds member
        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/members"))
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .json(&json!({ "user_id": member.id, "role": "member" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);

        // Member leaves
        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/leave"))
            .add_header(&member_headers[0].0, &member_headers[0].1)
            .add_header(&member_headers[1].0, &member_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::NO_CONTENT);

        // Verify member is no longer in the org
        let resp = server
            .get(&format!("/admin/api/v1/organizations/{org_id}/members"))
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        let members = resp.json::<Vec<serde_json::Value>>();
        assert_eq!(members.len(), 1); // Only owner remains
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_last_owner_cannot_leave(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        // Owner creates org (self-serve)
        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "name": "solo-org", "email": "solo@example.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

        // Last owner tries to leave — should fail
        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/leave"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
        let body = resp.text();
        assert!(body.contains("last owner"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_non_member_cannot_leave(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);

        let owner = create_test_user(&pool, Role::StandardUser).await;
        let outsider = create_test_user(&pool, Role::StandardUser).await;
        let outsider_headers = add_auth_headers(&outsider);

        // PM creates org
        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .json(&json!({ "name": "private-org", "email": "org@example.com", "owner_id": owner.id }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

        // Non-member tries to leave
        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/leave"))
            .add_header(&outsider_headers[0].0, &outsider_headers[0].1)
            .add_header(&outsider_headers[1].0, &outsider_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::NOT_FOUND);
    }

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

    #[sqlx::test]
    #[test_log::test]
    async fn test_set_active_org_cookie_includes_domain_when_configured(pool: PgPool) {
        let mut config = create_test_config();
        config.auth.native.session.cookie_domain = Some(".example.com".to_string());
        let (server, _bg) = create_test_app_with_config(pool.clone(), config, false).await;

        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        // Create org
        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .json(&json!({ "name": "domain-test-org", "email": "org@example.com", "owner_id": owner.id }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

        // Set active org — cookie should include Domain
        let resp = server
            .post("/admin/api/v1/session/organization")
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "organization_id": org_id }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        let cookie = resp.headers().get("set-cookie").unwrap().to_str().unwrap();
        assert!(cookie.contains("Domain=.example.com"), "set cookie should include Domain: {cookie}");

        // Clear active org — cookie should also include Domain
        let resp = server
            .post("/admin/api/v1/session/organization")
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "organization_id": null }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        let cookie = resp.headers().get("set-cookie").unwrap().to_str().unwrap();
        assert!(
            cookie.contains("Domain=.example.com"),
            "clear cookie should include Domain: {cookie}"
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_set_active_org_cookie_omits_domain_when_not_configured(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;

        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        // Create org
        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .json(&json!({ "name": "no-domain-org", "email": "org@example.com", "owner_id": owner.id }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

        // Set active org — cookie should NOT include Domain
        let resp = server
            .post("/admin/api/v1/session/organization")
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "organization_id": org_id }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        let cookie = resp.headers().get("set-cookie").unwrap().to_str().unwrap();
        assert!(!cookie.contains("Domain="), "cookie should not include Domain: {cookie}");
    }

    // ── Organization update / email-change verification flow ────────────

    /// Helper: PM creates an org owned by `owner` with the given contact email,
    /// returning the new organization's ID.
    async fn create_org_for(
        server: &axum_test::TestServer,
        pm_headers: &[(String, String)],
        name: &str,
        email: &str,
        owner_id: crate::types::UserId,
    ) -> String {
        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .json(&json!({ "name": name, "email": email, "owner_id": owner_id }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string()
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_patch_org_with_invalid_email_rejected(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        let org_id = create_org_for(&server, &pm_headers, "validate-email-org", "billing@example.com", owner.id).await;

        for bad in ["not-an-email", "missing-at.com", "  ", "spaces in@addr.com"] {
            let resp = server
                .patch(&format!("/admin/api/v1/organizations/{org_id}"))
                .add_header(&owner_headers[0].0, &owner_headers[0].1)
                .add_header(&owner_headers[1].0, &owner_headers[1].1)
                .json(&json!({ "email": bad }))
                .await;
            resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
        }

        // The contact email must be untouched after the rejected requests.
        let resp = server
            .get(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        let body = resp.json::<serde_json::Value>();
        assert_eq!(body["email"].as_str().unwrap(), "billing@example.com");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_patch_org_email_does_not_apply_immediately(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        let org_id = create_org_for(&server, &pm_headers, "verify-flow-org", "billing@example.com", owner.id).await;

        // The owner asks to change to an attacker-controlled address. The
        // PATCH succeeds but the change must be gated behind verification.
        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "email": "attacker@evil.example.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        let body = resp.json::<serde_json::Value>();
        assert_eq!(
            body["email"].as_str().unwrap(),
            "billing@example.com",
            "old email must remain until confirmation"
        );
        assert_eq!(
            body["pending_email_change"]["new_email"].as_str().unwrap(),
            "attacker@evil.example.com"
        );
        assert!(body["pending_email_change"]["expires_at"].is_string());

        // Re-fetch to make sure the field really wasn't written.
        let resp = server
            .get(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        let body = resp.json::<serde_json::Value>();
        assert_eq!(body["email"].as_str().unwrap(), "billing@example.com");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_patch_org_display_name_still_applies_when_email_pending(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        let org_id = create_org_for(&server, &pm_headers, "rename-org", "billing@example.com", owner.id).await;

        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "display_name": "Renamed Org", "email": "new@example.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        let body = resp.json::<serde_json::Value>();
        assert_eq!(body["display_name"].as_str().unwrap(), "Renamed Org");
        assert_eq!(body["email"].as_str().unwrap(), "billing@example.com");
        assert_eq!(body["pending_email_change"]["new_email"].as_str().unwrap(), "new@example.com");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_patch_org_with_same_email_is_noop(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        let org_id = create_org_for(&server, &pm_headers, "same-email-org", "billing@example.com", owner.id).await;

        // Resending the same address (different case) must not start a verification flow.
        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "email": "Billing@Example.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        let body = resp.json::<serde_json::Value>();
        assert!(
            body.get("pending_email_change").is_none() || body["pending_email_change"].is_null(),
            "no pending change when email did not actually change: {body}"
        );

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_org_email_changes WHERE organization_id = $1")
            .bind(uuid::Uuid::parse_str(&org_id).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_confirm_email_change_applies_new_email(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        let org_id = create_org_for(&server, &pm_headers, "confirm-org", "billing@example.com", owner.id).await;

        // Inject the verification row directly so we know the plaintext token. The
        // request-side test above proves the handler does this, so we don't need
        // to re-route through the file-transport email here.
        let token = crate::auth::password::generate_reset_token();
        let token_hash = super::hash_invite_token(&token);
        sqlx::query(
            "INSERT INTO pending_org_email_changes (organization_id, new_email, requested_by, token_hash, expires_at)
             VALUES ($1, $2, $3, $4, NOW() + INTERVAL '1 hour')",
        )
        .bind(uuid::Uuid::parse_str(&org_id).unwrap())
        .bind("new@example.com")
        .bind(owner.id)
        .bind(&token_hash)
        .execute(&pool)
        .await
        .unwrap();

        // No auth required — the token IS the proof. The link must be clickable
        // straight from an email client, so the endpoint is GET and returns HTML.
        let resp = server
            .get(&format!("/admin/api/v1/organizations/email-change/{token}/confirm"))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        let body = resp.text();
        assert!(
            body.contains("contact email has been updated"),
            "unexpected confirmation body: {body}"
        );

        // Underlying state: email written, pending row consumed.
        let resp = server
            .get(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .await;
        let body = resp.json::<serde_json::Value>();
        assert_eq!(body["email"].as_str().unwrap(), "new@example.com");

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_org_email_changes WHERE token_hash = $1")
            .bind(&token_hash)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "pending row should be consumed");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_confirm_email_change_invalid_token(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;

        let resp = server
            .get("/admin/api/v1/organizations/email-change/not-a-real-token/confirm")
            .await;
        resp.assert_status(axum::http::StatusCode::NOT_FOUND);
        assert!(resp.text().contains("invalid"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_confirm_email_change_expired_token_rejected(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        let org_id = create_org_for(&server, &pm_headers, "expired-org", "billing@example.com", owner.id).await;

        let token = crate::auth::password::generate_reset_token();
        let token_hash = super::hash_invite_token(&token);
        sqlx::query(
            "INSERT INTO pending_org_email_changes (organization_id, new_email, requested_by, token_hash, expires_at)
             VALUES ($1, $2, $3, $4, NOW() - INTERVAL '1 hour')",
        )
        .bind(uuid::Uuid::parse_str(&org_id).unwrap())
        .bind("new@example.com")
        .bind(owner.id)
        .bind(&token_hash)
        .execute(&pool)
        .await
        .unwrap();

        let resp = server
            .get(&format!("/admin/api/v1/organizations/email-change/{token}/confirm"))
            .await;
        resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
        assert!(resp.text().contains("expired"));

        // The email must NOT have been updated, and the expired row must be cleaned up.
        let resp = server
            .get(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .await;
        let body = resp.json::<serde_json::Value>();
        assert_eq!(body["email"].as_str().unwrap(), "billing@example.com");

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_org_email_changes WHERE token_hash = $1")
            .bind(&token_hash)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "expired row should be removed on probe");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_second_email_change_supersedes_first(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        let org_id = create_org_for(&server, &pm_headers, "supersede-org", "billing@example.com", owner.id).await;

        for new_email in ["one@example.com", "two@example.com"] {
            let resp = server
                .patch(&format!("/admin/api/v1/organizations/{org_id}"))
                .add_header(&owner_headers[0].0, &owner_headers[0].1)
                .add_header(&owner_headers[1].0, &owner_headers[1].1)
                .json(&json!({ "email": new_email }))
                .await;
            resp.assert_status(axum::http::StatusCode::OK);
        }

        // Only the latest pending change should remain, with `requested_by`
        // set to the caller — this is the audit field, and a regression that
        // swaps the email but not the actor would be silent without this check.
        let rows: Vec<(String, uuid::Uuid)> =
            sqlx::query_as("SELECT new_email, requested_by FROM pending_org_email_changes WHERE organization_id = $1")
                .bind(uuid::Uuid::parse_str(&org_id).unwrap())
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows, vec![("two@example.com".to_string(), owner.id)]);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_superseded_token_cannot_confirm(pool: PgPool) {
        // After a second PATCH, the *first* token must be inert — clicking the
        // old verification link must not roll back the new pending change or
        // resurrect the old one.
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        let org_id = create_org_for(&server, &pm_headers, "stale-token-org", "billing@example.com", owner.id).await;

        // Plant the first token directly so we know its plaintext.
        let token_one = crate::auth::password::generate_reset_token();
        let token_one_hash = super::hash_invite_token(&token_one);
        sqlx::query(
            "INSERT INTO pending_org_email_changes (organization_id, new_email, requested_by, token_hash, expires_at)
             VALUES ($1, $2, $3, $4, NOW() + INTERVAL '1 hour')",
        )
        .bind(uuid::Uuid::parse_str(&org_id).unwrap())
        .bind("first@example.com")
        .bind(owner.id)
        .bind(&token_one_hash)
        .execute(&pool)
        .await
        .unwrap();

        // A second PATCH supersedes via UPSERT.
        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "email": "second@example.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);

        // The first token should now be invalid.
        let resp = server
            .get(&format!("/admin/api/v1/organizations/email-change/{token_one}/confirm"))
            .await;
        resp.assert_status(axum::http::StatusCode::NOT_FOUND);

        // The org's email is still the original — only the second token (which the
        // test cannot read here) could change it.
        let resp = server
            .get(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .await;
        let body = resp.json::<serde_json::Value>();
        assert_eq!(body["email"].as_str().unwrap(), "billing@example.com");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_confirm_token_consumed_after_first_use(pool: PgPool) {
        // The DELETE RETURNING in consume_pending_email_change must make the
        // token single-use — replaying the same URL returns 404 the second time.
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;

        let org_id = create_org_for(&server, &pm_headers, "replay-org", "billing@example.com", owner.id).await;

        let token = crate::auth::password::generate_reset_token();
        let token_hash = super::hash_invite_token(&token);
        sqlx::query(
            "INSERT INTO pending_org_email_changes (organization_id, new_email, requested_by, token_hash, expires_at)
             VALUES ($1, $2, $3, $4, NOW() + INTERVAL '1 hour')",
        )
        .bind(uuid::Uuid::parse_str(&org_id).unwrap())
        .bind("new@example.com")
        .bind(owner.id)
        .bind(&token_hash)
        .execute(&pool)
        .await
        .unwrap();

        let first = server
            .get(&format!("/admin/api/v1/organizations/email-change/{token}/confirm"))
            .await;
        first.assert_status(axum::http::StatusCode::OK);

        let second = server
            .get(&format!("/admin/api/v1/organizations/email-change/{token}/confirm"))
            .await;
        second.assert_status(axum::http::StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_org_with_invalid_email_rejected(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let headers = add_auth_headers(&user);

        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .json(&json!({ "name": "bad-email-org", "email": "not-an-email" }))
            .await;
        resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    // ── Authorization on PATCH /organizations/{id} ──────────────────────
    // These prove the security claim of the fix: only callers with
    // owner/admin org-role (or platform manager) can start a verification
    // flow. A future refactor of `can_manage_org_resource` that broadens
    // access would be caught here.

    #[sqlx::test]
    #[test_log::test]
    async fn test_plain_member_cannot_patch_org_email(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let plain_member = create_test_user(&pool, Role::StandardUser).await;
        let plain_member_headers = add_auth_headers(&plain_member);

        let org_id = create_org_for(&server, &pm_headers, "auth-member-org", "billing@example.com", owner.id).await;

        // Add as plain `member` — NOT owner/admin.
        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/members"))
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .json(&json!({ "user_id": plain_member.id, "role": "member" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);

        // Plain member tries to PATCH the email — must be denied with 403.
        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&plain_member_headers[0].0, &plain_member_headers[0].1)
            .add_header(&plain_member_headers[1].0, &plain_member_headers[1].1)
            .json(&json!({ "email": "attacker@evil.example.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::FORBIDDEN);

        // No pending row should have been created.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_org_email_changes WHERE organization_id = $1")
            .bind(uuid::Uuid::parse_str(&org_id).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_non_member_cannot_patch_org_email(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let outsider = create_test_user(&pool, Role::StandardUser).await;
        let outsider_headers = add_auth_headers(&outsider);

        let org_id = create_org_for(&server, &pm_headers, "auth-outsider-org", "billing@example.com", owner.id).await;

        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&outsider_headers[0].0, &outsider_headers[0].1)
            .add_header(&outsider_headers[1].0, &outsider_headers[1].1)
            .json(&json!({ "email": "attacker@evil.example.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::FORBIDDEN);

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_org_email_changes WHERE organization_id = $1")
            .bind(uuid::Uuid::parse_str(&org_id).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_can_patch_org_email_without_membership(pool: PgPool) {
        // A platform manager who is NOT a member of the org must still be able
        // to drive the change — they're our break-glass admin.
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;

        let org_id = create_org_for(&server, &pm_headers, "auth-pm-org", "billing@example.com", owner.id).await;

        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .json(&json!({ "email": "new-contact@example.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        let body = resp.json::<serde_json::Value>();
        assert_eq!(
            body["pending_email_change"]["new_email"].as_str().unwrap(),
            "new-contact@example.com"
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_token_for_one_org_cannot_affect_another(pool: PgPool) {
        // Per-row tenancy: a token issued for org A must never change org B's
        // email even though the same handler serves both.
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner_a = create_test_user(&pool, Role::StandardUser).await;
        let owner_a_headers = add_auth_headers(&owner_a);
        let owner_b = create_test_user(&pool, Role::StandardUser).await;
        let owner_b_headers = add_auth_headers(&owner_b);

        let org_a_id = create_org_for(&server, &pm_headers, "tenancy-org-a", "a@example.com", owner_a.id).await;
        let org_b_id = create_org_for(&server, &pm_headers, "tenancy-org-b", "b@example.com", owner_b.id).await;

        // Plant a pending change for org A so we know the token.
        let token = crate::auth::password::generate_reset_token();
        let token_hash = super::hash_invite_token(&token);
        sqlx::query(
            "INSERT INTO pending_org_email_changes (organization_id, new_email, requested_by, token_hash, expires_at)
             VALUES ($1, $2, $3, $4, NOW() + INTERVAL '1 hour')",
        )
        .bind(uuid::Uuid::parse_str(&org_a_id).unwrap())
        .bind("new-a@example.com")
        .bind(owner_a.id)
        .bind(&token_hash)
        .execute(&pool)
        .await
        .unwrap();

        // Confirm with the org-A token.
        let resp = server
            .get(&format!("/admin/api/v1/organizations/email-change/{token}/confirm"))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);

        // Org A's email moved.
        let resp = server
            .get(&format!("/admin/api/v1/organizations/{org_a_id}"))
            .add_header(&owner_a_headers[0].0, &owner_a_headers[0].1)
            .add_header(&owner_a_headers[1].0, &owner_a_headers[1].1)
            .await;
        assert_eq!(resp.json::<serde_json::Value>()["email"].as_str().unwrap(), "new-a@example.com");

        // Org B's email is untouched.
        let resp = server
            .get(&format!("/admin/api/v1/organizations/{org_b_id}"))
            .add_header(&owner_b_headers[0].0, &owner_b_headers[0].1)
            .add_header(&owner_b_headers[1].0, &owner_b_headers[1].1)
            .await;
        assert_eq!(resp.json::<serde_json::Value>()["email"].as_str().unwrap(), "b@example.com");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_soft_deleted_org_cannot_have_email_changed_via_token(pool: PgPool) {
        // If an org is soft-deleted between PATCH and click, the token must
        // become inert. The DELETE in consume joins users WHERE is_deleted = false,
        // so a stale pending row is silently ignored — returning 404.
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;

        let org_id = create_org_for(&server, &pm_headers, "soft-delete-org", "billing@example.com", owner.id).await;
        let org_uuid = uuid::Uuid::parse_str(&org_id).unwrap();

        // Plant a valid pending change.
        let token = crate::auth::password::generate_reset_token();
        let token_hash = super::hash_invite_token(&token);
        sqlx::query(
            "INSERT INTO pending_org_email_changes (organization_id, new_email, requested_by, token_hash, expires_at)
             VALUES ($1, $2, $3, $4, NOW() + INTERVAL '1 hour')",
        )
        .bind(org_uuid)
        .bind("new@example.com")
        .bind(owner.id)
        .bind(&token_hash)
        .execute(&pool)
        .await
        .unwrap();

        // Soft-delete the org directly (mirrors what `delete_organization` does).
        sqlx::query("UPDATE users SET is_deleted = true WHERE id = $1")
            .bind(org_uuid)
            .execute(&pool)
            .await
            .unwrap();

        // Token now refers to a soft-deleted org — confirm endpoint returns 404.
        let resp = server
            .get(&format!("/admin/api/v1/organizations/email-change/{token}/confirm"))
            .await;
        resp.assert_status(axum::http::StatusCode::NOT_FOUND);

        // And the pending row is still present (consume joined out the soft-deleted org)
        // — it'll be reaped when the org is hard-deleted via CASCADE.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_org_email_changes WHERE token_hash = $1")
            .bind(&token_hash)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_patch_writes_verify_and_notice_emails(pool: PgPool) {
        // The file transport in the default test config writes each email to
        // a temp directory shared across the process. We scope our assertions
        // to a per-test directory by using create_test_app_with_config so other
        // tests' emails don't pollute the scan.
        let scratch = std::env::temp_dir().join(format!("dwctl-test-emails-patch-{}-{}", std::process::id(), uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&scratch).unwrap();

        let mut config = create_test_config();
        config.email.transport = crate::config::EmailTransportConfig::File {
            path: scratch.to_string_lossy().to_string(),
        };

        let (server, _bg) = create_test_app_with_config(pool.clone(), config, false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        let org_id = create_org_for(&server, &pm_headers, "email-send-org", "current@example.com", owner.id).await;

        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "email": "new@example.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);

        // Both emails should have landed in the scratch dir. Read them all and
        // collect To: addresses.
        let mut to_addresses: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&scratch).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) == Some("eml") {
                let body = std::fs::read_to_string(&path).unwrap();
                for line in body.lines() {
                    if let Some(rest) = line.strip_prefix("To: ") {
                        to_addresses.push(rest.trim().to_string());
                    }
                }
            }
        }
        to_addresses.sort();
        assert_eq!(
            to_addresses,
            vec!["current@example.com".to_string(), "new@example.com".to_string()],
            "expected verify to new and notice to current; got {to_addresses:?}",
        );
    }
}
