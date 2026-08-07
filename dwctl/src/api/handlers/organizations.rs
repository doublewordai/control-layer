//! HTTP handlers for organization management endpoints.

use crate::{
    AppState,
    api::models::{
        organizations::{
            AddMemberRequest, ApproveJoinRequestRequest, InviteDetailsResponse, InviteMemberRequest, InviteMemberResponse,
            ListOrganizationsQuery, OrganizationCreate, OrganizationMemberResponse, OrganizationResponse, OrganizationUpdate,
            PendingEmailChangeResponse, SetActiveOrganizationRequest, SetActiveOrganizationResponse, UpdateMemberRoleRequest,
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

/// Process-wide DNS resolver used by [`verify_email_deliverable`].
///
/// hickory's per-instance cache respects each record's TTL, so sharing one
/// resolver across requests lets repeated lookups for the same domain reuse
/// cached MX/A results within their TTL window instead of re-parsing
/// `/etc/resolv.conf` and opening fresh sockets per call. The resolver is
/// instantiated lazily on first use so we don't pay the parse cost at app
/// startup, and the initialization itself can be retried on subsequent calls
/// if the first attempt fails (e.g. `/etc/resolv.conf` not readable yet).
static DNS_RESOLVER: tokio::sync::OnceCell<hickory_resolver::TokioResolver> = tokio::sync::OnceCell::const_new();

async fn dns_resolver() -> Result<&'static hickory_resolver::TokioResolver> {
    DNS_RESOLVER
        .get_or_try_init(|| async {
            hickory_resolver::TokioResolver::builder_tokio()
                .and_then(|b| b.build())
                .map_err(|e| Error::Internal {
                    operation: format!("initialize DNS resolver: {e}"),
                })
        })
        .await
}

/// Verify that the domain part of `email` has at least one publishable mail
/// destination — an MX record, or an implicit A/AAAA per RFC 5321 §5.
///
/// `Error::BadRequest` is returned for domains with no resolvable mail
/// destination (typos, disposable domains, parked domains). `Error::Internal`
/// is returned for transient DNS failures so the caller retries rather than
/// silently accepting an unverifiable address.
async fn verify_email_deliverable(email: &str) -> Result<()> {
    let domain = email.rsplit_once('@').map(|(_, d)| d).ok_or_else(|| Error::BadRequest {
        message: "Invalid email address".to_string(),
    })?;
    if domain.is_empty() {
        return Err(Error::BadRequest {
            message: "Invalid email address".to_string(),
        });
    }

    let resolver = dns_resolver().await?;

    // Try MX first. An empty MX response means the domain explicitly opts out
    // of mail (rare but valid); we then fall back to A/AAAA per RFC 5321 §5.
    match resolver.mx_lookup(domain).await {
        Ok(mx) if !mx.answers().is_empty() => Ok(()),
        Ok(_) => {
            // No MX records — check for an implicit A/AAAA.
            match resolver.lookup_ip(domain).await {
                Ok(ips) if ips.iter().next().is_some() => Ok(()),
                Ok(_) => Err(Error::BadRequest {
                    message: format!("Email domain '{domain}' has no mail servers and no address records"),
                }),
                Err(e) if e.is_no_records_found() => Err(Error::BadRequest {
                    message: format!("Email domain '{domain}' has no mail servers and no address records"),
                }),
                Err(e) => Err(Error::Internal {
                    operation: format!("DNS A lookup for {domain}: {e}"),
                }),
            }
        }
        Err(e) if e.is_no_records_found() => {
            // NXDOMAIN or NODATA on MX — try A as implicit MX.
            match resolver.lookup_ip(domain).await {
                Ok(ips) if ips.iter().next().is_some() => Ok(()),
                Ok(_) => Err(Error::BadRequest {
                    message: format!("Email domain '{domain}' has no mail servers and no address records"),
                }),
                Err(ae) if ae.is_no_records_found() => Err(Error::BadRequest {
                    message: format!("Email domain '{domain}' has no mail servers and no address records"),
                }),
                Err(ae) => Err(Error::Internal {
                    operation: format!("DNS A lookup for {domain}: {ae}"),
                }),
            }
        }
        Err(e) => Err(Error::Internal {
            operation: format!("DNS MX lookup for {domain}: {e}"),
        }),
    }
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

    // The organization's username IS its email domain, which is what makes an
    // organization findable by the people who belong to it: a later signup from
    // the same domain is offered a join request instead of a second workspace.
    // `users.username` is already UNIQUE, so that also enforces one
    // organization per domain without a separate claim table.
    //
    // Derived from the owner's own address rather than the submitted name, so a
    // caller can't claim a domain they don't have mail at. The name the user
    // typed becomes the display name.
    //
    // Falls back to the submitted name when the owner is on a personal email
    // domain (gmail and friends) - there's no meaningful domain to claim, and
    // claiming "gmail.com" would swallow every consumer signup into one org.
    let owner_email = if owner_id == current_user.id {
        current_user.email.clone()
    } else {
        let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
        Users::new(&mut conn)
            .get_by_id(owner_id)
            .await?
            .ok_or(Error::NotFound {
                resource: "User".to_string(),
                id: owner_id.to_string(),
            })?
            .email
    };
    let claimable_domain = crate::auth::utils::email_domain(&owner_email).filter(|d| !crate::auth::utils::is_personal_email_domain(d));

    // `{domain}~{suffix}`: the domain is what a colleague's signup matches on,
    // and the suffix keeps `users.username` unique so one company can hold
    // several workspaces - prod and dev being the obvious pair. `~` can't occur
    // in a domain, which is what makes the match exact on the way back out.
    //
    // Falls back to the submitted name when the owner is on a personal email
    // domain: there's nothing worth matching on, and an organization claiming
    // "gmail.com" would catch every consumer signup.
    let username = match claimable_domain.as_deref() {
        Some(domain) => format!("{domain}~{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
        None => data.name.clone(),
    };

    let display_name = data.display_name.clone().or_else(|| Some(data.name.clone()));
    let db_request = OrganizationCreateDBRequest {
        name: username,
        email,
        display_name,
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

    // Surface any in-flight email-change verification so the dashboard can
    // render the pending state without requiring the user to refresh from the
    // exact PATCH response that initiated the change. Reads from the read
    // pool inherit normal replica-lag semantics; the verification row is the
    // source of truth either way.
    let pending_email_change = org_repo.find_pending_email_change_for_org(id).await?;
    let auto_join_enabled = org_repo.auto_join_enabled(id).await?;

    let mut response = OrganizationResponse::from_user(UserResponse::from(org))
        .with_member_count(members.len() as i64)
        .with_auto_join_enabled(auto_join_enabled);
    if let Some(pending) = pending_email_change {
        response = response.with_pending_email_change(PendingEmailChangeResponse::from(pending));
    }

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
/// Zero data retention is restricted to organization owners and platform managers.
#[utoipa::path(
    patch,
    path = "/organizations/{id}",
    tag = "organizations",
    summary = "Update organization",
    description = "Update organization details. Requires owner/admin role or platform manager access. Zero data retention can only be changed by an organization owner or platform manager.",
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

    let caller_org_role = if can_all {
        None
    } else {
        let mut org_repo = Organizations::new(&mut pool_conn);
        let role = org_repo.get_user_org_role(current_user.id, id).await?;
        if !matches!(role.as_deref(), Some("owner" | "admin")) {
            return Err(Error::InsufficientPermissions {
                required: Permission::Allow(Resource::Organizations, Operation::UpdateOwn),
                action: Operation::UpdateOwn,
                resource: format!("Organization {id}"),
            });
        }
        role
    };

    // SECURITY: Organization admins can manage ordinary org fields, but only
    // owners (or platform managers via UpdateAll) may change this compliance
    // setting.
    if !can_all && data.zero_data_retention.is_some() && caller_org_role.as_deref() != Some("owner") {
        return Err(Error::InsufficientPermissions {
            required: Permission::Allow(Resource::Organizations, Operation::UpdateOwn),
            action: Operation::UpdateOwn,
            resource: format!("zero data retention for organization {id}"),
        });
    }

    // SECURITY: same owner-only gate, for the same kind of reason. Auto-join
    // decides who gets into the workspace with nobody reviewing them — it is
    // the one setting that can hand membership to a person no member has ever
    // seen. An admin able to flip it could admit anyone who can obtain an
    // address at the domain, without the owner ever being in the loop.
    if !can_all && data.auto_join_enabled.is_some() && caller_org_role.as_deref() != Some("owner") {
        return Err(Error::InsufficientPermissions {
            required: Permission::Allow(Resource::Organizations, Operation::UpdateOwn),
            action: Operation::UpdateOwn,
            resource: format!("domain auto-join for organization {id}"),
        });
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

    // If an email change is requested, validate the format, check the domain
    // is mail-deliverable, and route it through the double-opt-in verification
    // flow instead of applying it directly. Both the current contact mailbox
    // AND the new mailbox must click their respective verification links
    // within 24 hours; the change is only applied to `users.email` once both
    // are confirmed. The contact email is rendered into Stripe receipts,
    // invitation emails and audit notifications, so a silent change could
    // redirect security-sensitive mail to an attacker (session hijack) or to
    // a typo'd address (deliverability failure).
    let mut pending_email_info: Option<PendingEmailChangeResponse> = None;
    if let Some(ref requested) = data.email {
        let normalized = validate_contact_email(requested)?;
        if normalized != current_org.email.to_lowercase() {
            // MX-record (or implicit-A) deliverability check. Rejects typo'd
            // and disposable domains before we ever generate tokens. Failures
            // are fatal on this PATCH — better to make the user retry than to
            // queue a verification email at an undeliverable domain.
            verify_email_deliverable(&normalized).await?;

            let new_email_token = crate::auth::password::generate_reset_token();
            let old_email_token = crate::auth::password::generate_reset_token();
            let new_email_token_hash = hash_invite_token(&new_email_token);
            let old_email_token_hash = hash_invite_token(&old_email_token);
            let expires_at = chrono::Utc::now() + Duration::hours(EMAIL_CHANGE_TOKEN_TTL_HOURS);

            // Single UPSERT atomically supersedes any prior pending change for this org,
            // invalidating both older verification tokens and clearing prior
            // partial confirmations in the same statement. We look up the
            // prior row first so we can audit-log the supersede with the
            // displaced requester's identity — once the UPSERT runs, only
            // `current_user` is retained.
            let mut org_repo = Organizations::new(&mut pool_conn);
            if let Some(prior) = org_repo.find_pending_email_change_for_org(id).await? {
                tracing::warn!(
                    org_id = %id,
                    superseded_requested_by = %prior.requested_by,
                    superseded_new_email = %prior.new_email,
                    superseded_created_at = %prior.created_at,
                    new_requested_by = %current_user.id,
                    new_email = %normalized,
                    kind = "org_email_change_superseded",
                    "PATCH superseded a prior pending org email change",
                );
            }
            let pending = org_repo
                .upsert_pending_email_change(
                    id,
                    &normalized,
                    current_user.id,
                    &new_email_token_hash,
                    &old_email_token_hash,
                    expires_at,
                )
                .await?;

            // Best-effort: send both verification emails. The verification row
            // already gates the actual email change, so we never let mail
            // failures (transport down, template error, service misconfig)
            // roll back the API call or hide the pending state from the
            // client. Each failure is logged with structured fields so ops
            // can alert specifically on the old-side path — that one is the
            // legitimate owner's authorisation gate, and its silent absence
            // would benefit a session-hijack attacker.
            let config = state.current_config();
            let org_name = current_org.display_name.clone().unwrap_or_else(|| current_org.username.clone());
            // Verification links are backend GETs that return HTML, so they
            // work straight from any mail client (no dashboard route needed).
            // The backend and dashboard share an origin in production
            // deployments, so `dashboard_url` is the right base — matches how
            // invite links are built.
            let make_link = |tok: &str| {
                format!(
                    "{}/admin/api/v1/organizations/email-change/{}/confirm",
                    config.dashboard_url.trim_end_matches('/'),
                    tok,
                )
            };
            let new_email_link = make_link(&new_email_token);
            let old_email_link = make_link(&old_email_token);

            match EmailService::new(&config) {
                Ok(email_service) => {
                    if let Err(error) = email_service
                        .send_org_email_change_verify_new(&normalized, &org_name, &new_email_link)
                        .await
                    {
                        tracing::warn!(
                            org_id = %id,
                            new_email = %normalized,
                            error = %error,
                            kind = "org_email_change_verify_new_failed",
                            "Failed to send org email-change verify-new email",
                        );
                    }
                    if let Err(error) = email_service
                        .send_org_email_change_verify_old(
                            &current_org.email,
                            &org_name,
                            &normalized,
                            &old_email_link,
                            Some(&config.support_email),
                        )
                        .await
                    {
                        // Security-relevant: failure here means the legitimate
                        // owner can't authorize the change, but it also means
                        // an attacker can't get the old-side approval — so the
                        // verification gate still holds. We still alert on
                        // `kind = "org_email_change_verify_old_failed"` so ops
                        // can spot silent SMTP failures against the old address.
                        //
                        // Follow-up: a durable retry (via the task runner /
                        // batched email queue) would harden this further
                        // against transient SMTP failures. Out of scope for
                        // this PR.
                        tracing::warn!(
                            org_id = %id,
                            old_email = %current_org.email,
                            new_email = %normalized,
                            error = %error,
                            kind = "org_email_change_verify_old_failed",
                            "Failed to send org email-change verify-old email — current owner cannot authorize",
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
    // either it was unchanged, or it's now gated behind the verification flow
    // above. The debug-only assert turns the convention into a fail-fast
    // runtime check so a future refactor that mistakenly threads `data.email`
    // through here trips a test before reaching production.
    let db_request = OrganizationUpdateDBRequest {
        display_name: data.display_name,
        avatar_url: None,
        email: None,
        batch_notifications_enabled: data.batch_notifications_enabled,
        low_balance_threshold: data.low_balance_threshold,
        zero_data_retention: data.zero_data_retention,
    };
    debug_assert!(
        db_request.email.is_none(),
        "user-facing PATCH must never write email directly — verification flow gates this field",
    );

    let mut repo = Organizations::new(&mut pool_conn);
    let org = repo.update(id, &db_request).await?;

    // Written separately rather than threaded through OrganizationUpdateDBRequest:
    // the flag lives on `users` but is read in only two places, so it isn't
    // carried on the materialised user row (see `Organizations::auto_join_enabled`).
    if let Some(enabled) = data.auto_join_enabled {
        repo.set_auto_join_enabled(id, enabled).await?;
    }
    let auto_join_enabled = repo.auto_join_enabled(id).await?;

    // Surface any pending email-change on EVERY PATCH response — not just
    // the one that created it — so a dashboard mid-verification doesn't lose
    // the state when the user PATCHes another field (e.g. renames the org).
    // If this PATCH triggered a new pending change we already have it in
    // `pending_email_info`; otherwise look up whatever's currently pending.
    let pending_email_change = match pending_email_info {
        Some(info) => Some(info),
        None => repo
            .find_pending_email_change_for_org(id)
            .await?
            .map(PendingEmailChangeResponse::from),
    };

    let mut response = OrganizationResponse::from_user(UserResponse::from(org)).with_auto_join_enabled(auto_join_enabled);
    if let Some(info) = pending_email_change {
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
    let manage_keys_ids = repo.list_manage_keys_membership_ids(id).await?;
    let effective_manage_keys = |m: &crate::db::models::organizations::OrganizationMemberDBResponse| {
        matches!(m.role.as_str(), "owner" | "admin") || manage_keys_ids.contains(&m.id)
    };

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
                    can_manage_keys: effective_manage_keys(m),
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
                    can_manage_keys: effective_manage_keys(m),
                })
            }
        })
        .collect();

    Ok(Json(members))
}

/// Re-send a pending organization invite
#[utoipa::path(
    post,
    path = "/organizations/{id}/invites/{invite_id}/resend",
    tag = "organizations",
    summary = "Resend invite",
    description = "Issue a fresh token for a pending invite and email it again. The previous link stops working. Requires owner/admin role or platform manager access.",
    params(
        ("id" = String, Path, description = "Organization ID"),
        ("invite_id" = String, Path, description = "Invite (membership row) ID"),
    ),
    responses(
        (status = 204, description = "Invite re-sent"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Pending invite not found"),
    ),
    security(("BearerAuth" = []), ("CookieAuth" = []), ("X-Doubleword-User" = [])),
)]
#[tracing::instrument(skip_all)]
pub async fn resend_invite<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((id, invite_id)): Path<(UserId, uuid::Uuid)>,
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

    // A resend necessarily mints a new token: the original is only stored as a
    // hash, so it can't be recovered to send again.
    let token = crate::auth::password::generate_reset_token();
    let token_hash = hash_invite_token(&token);
    let expires_at = chrono::Utc::now() + Duration::days(7);

    let mut org_repo = Organizations::new(&mut pool_conn);
    let Some((email, role)) = org_repo.refresh_invite_token(id, invite_id, &token_hash, expires_at).await? else {
        return Err(Error::NotFound {
            resource: "Pending invite".to_string(),
            id: format!("{invite_id} in organization {id}"),
        });
    };

    let mut users_repo = Users::new(&mut pool_conn);
    let org_user = users_repo.get_by_id(id).await?;
    let org_name = org_user
        .as_ref()
        .and_then(|u| u.display_name.clone())
        .unwrap_or_else(|| org_user.as_ref().map(|u| u.username.clone()).unwrap_or_default());

    let sender = users_repo.get_by_id(current_user.id).await?;
    let sender_name = sender
        .as_ref()
        .and_then(|u| u.display_name.clone())
        .unwrap_or_else(|| sender.as_ref().map(|u| u.username.clone()).unwrap_or_default());

    // The token is already rotated in the database. Treat a send failure as a
    // warning rather than an error, matching `invite_member`: failing the
    // request would imply nothing happened, when in fact the old link is now
    // dead and the admin would need to resend anyway.
    let config = state.current_config();
    let invite_link = format!("{}/org-invite?token={}", config.dashboard_url.trim_end_matches('/'), token);
    let email_service = EmailService::new(&config)?;
    if let Err(e) = email_service
        .send_org_invite_email(&email, &org_name, &sender_name, &role, &invite_link)
        .await
    {
        tracing::warn!("Failed to re-send invite email to {email}: {e}");
    }

    Ok(StatusCode::NO_CONTENT)
}

/// List outstanding join requests for an organization
#[utoipa::path(
    get,
    path = "/organizations/{id}/join-requests",
    tag = "organizations",
    summary = "List join requests",
    description = "List users who have asked to join this organization and are awaiting a decision. Requires owner/admin role or platform manager access.",
    params(("id" = String, Path, description = "Organization ID")),
    responses(
        (status = 200, description = "Outstanding join requests", body = [OrganizationMemberResponse]),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Organization not found"),
    ),
    security(("BearerAuth" = []), ("CookieAuth" = []), ("X-Doubleword-User" = [])),
)]
#[tracing::instrument(skip_all)]
pub async fn list_join_requests<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(id): Path<UserId>,
    current_user: CurrentUser,
) -> Result<Json<Vec<OrganizationMemberResponse>>> {
    let can_all = crate::auth::permissions::has_permission(&current_user, Resource::Organizations, Operation::UpdateAll);

    // Primary pool: a user's first login records their request, and an admin
    // may well be looking at this queue moments later.
    let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;

    // Deliberately gated on *manage*, not read: the queue exposes the identities
    // of people who share your email domain, which ordinary members shouldn't see.
    if !can_all {
        let can_org = can_manage_org_resource(&current_user, id, &mut pool_conn).await?;
        if !can_org {
            return Err(Error::InsufficientPermissions {
                required: Permission::Allow(Resource::Organizations, Operation::UpdateOwn),
                action: Operation::UpdateOwn,
                resource: format!("Organization {id} join requests"),
            });
        }
    }

    let mut repo = Organizations::new(&mut pool_conn);
    let requests = repo.list_join_requests(id).await?;

    let user_ids: Vec<UserId> = requests.iter().filter_map(|m| m.user_id).collect();
    let mut users_repo = Users::new(&mut pool_conn);
    let user_map = users_repo.get_bulk(user_ids).await?;

    let responses: Vec<OrganizationMemberResponse> = requests
        .iter()
        .filter_map(|m| {
            let uid = m.user_id?;
            user_map.get(&uid).map(|u| OrganizationMemberResponse {
                id: m.id,
                user: Some(UserResponse::from(u.clone())),
                role: m.role.clone(),
                status: m.status.clone(),
                created_at: m.created_at,
                invite_email: m.invite_email.clone(),
                // A request carries no capability until it's approved.
                can_manage_keys: false,
            })
        })
        .collect();

    Ok(Json(responses))
}

/// Approve a join request
#[utoipa::path(
    post,
    path = "/organizations/{id}/join-requests/{request_id}/approve",
    tag = "organizations",
    summary = "Approve join request",
    description = "Approve a pending join request, making the requester an active member. Requires owner/admin role or platform manager access.",
    params(
        ("id" = String, Path, description = "Organization ID"),
        ("request_id" = String, Path, description = "Join request ID"),
    ),
    request_body(content = ApproveJoinRequestRequest, description = "Optional role to grant; defaults to 'member'"),
    responses(
        (status = 204, description = "Request approved"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Join request not found"),
    ),
    security(("BearerAuth" = []), ("CookieAuth" = []), ("X-Doubleword-User" = [])),
)]
#[tracing::instrument(skip_all)]
pub async fn approve_join_request<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((id, request_id)): Path<(UserId, uuid::Uuid)>,
    current_user: CurrentUser,
    body: Option<Json<ApproveJoinRequestRequest>>,
) -> Result<StatusCode> {
    let can_all = crate::auth::permissions::has_permission(&current_user, Resource::Organizations, Operation::UpdateAll);

    let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;

    if !can_all {
        let can_org = can_manage_org_resource(&current_user, id, &mut pool_conn).await?;
        if !can_org {
            return Err(Error::InsufficientPermissions {
                required: Permission::Allow(Resource::Organizations, Operation::UpdateOwn),
                action: Operation::UpdateOwn,
                resource: format!("Organization {id} join requests"),
            });
        }
    }

    let role = body.and_then(|b| b.0.role).unwrap_or_else(|| "member".to_string());
    validate_role(&role)?;
    // Approving straight to owner would let an admin promote past themselves,
    // so the same guard as invites applies.
    check_role_assignment_privilege(&current_user, id, &role, can_all, &mut pool_conn).await?;

    let mut repo = Organizations::new(&mut pool_conn);
    let approved = repo.approve_join_request(id, request_id, &role).await?;
    if !approved {
        return Err(Error::NotFound {
            resource: "Join request".to_string(),
            id: format!("{request_id} in organization {id}"),
        });
    }

    Ok(StatusCode::NO_CONTENT)
}

/// Decline a join request
#[utoipa::path(
    post,
    path = "/organizations/{id}/join-requests/{request_id}/decline",
    tag = "organizations",
    summary = "Decline join request",
    description = "Decline a pending join request. The requester may ask again later. Requires owner/admin role or platform manager access.",
    params(
        ("id" = String, Path, description = "Organization ID"),
        ("request_id" = String, Path, description = "Join request ID"),
    ),
    responses(
        (status = 204, description = "Request declined"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Join request not found"),
    ),
    security(("BearerAuth" = []), ("CookieAuth" = []), ("X-Doubleword-User" = [])),
)]
#[tracing::instrument(skip_all)]
pub async fn decline_join_request<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((id, request_id)): Path<(UserId, uuid::Uuid)>,
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
                resource: format!("Organization {id} join requests"),
            });
        }
    }

    let mut repo = Organizations::new(&mut pool_conn);
    let declined = repo.decline_join_request(id, request_id).await?;
    if !declined {
        return Err(Error::NotFound {
            resource: "Join request".to_string(),
            id: format!("{request_id} in organization {id}"),
        });
    }

    Ok(StatusCode::NO_CONTENT)
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

    // Additive 'manage_keys' role: defaults to NOT granted — new members
    // can't self-serve keys until an admin opts them in (blast-radius
    // minimization; existing members were backfilled with the grant in
    // migration 128, so nobody lost capabilities). Owners/admins hold it
    // implicitly — no row.
    let is_manager_role = matches!(role, "owner" | "admin");
    let granted = data.can_manage_keys.unwrap_or(false);
    if !is_manager_role && granted {
        repo.set_membership_manage_keys(membership.id, true).await?;
    }

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
            can_manage_keys: is_manager_role || granted,
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

    // Grant/revoke the additive 'manage_keys' role alongside the base role.
    // Rows are kept only for plain members; a promotion to owner/admin makes
    // the capability implicit (any leftover row is harmless but we take the
    // explicit instruction when given).
    let is_manager_role = matches!(membership.role.as_str(), "owner" | "admin");
    let effective_manage_keys = if let Some(granted) = data.can_manage_keys {
        repo.set_membership_manage_keys(membership.id, granted).await?;
        is_manager_role || granted
    } else {
        is_manager_role || repo.membership_has_manage_keys(membership.id).await?
    };

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
        can_manage_keys: effective_manage_keys,
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

    // Primary, not the replica: these summaries carry `verified`, and clients
    // read it straight after a write that sets it - the card-verification
    // return path marks the organization verified, then the UI re-reads to drop
    // its "Verification pending" badge. Off the replica that read can land
    // before the flag has propagated, so the badge stays up on an account that
    // has just paid. Same inter-endpoint consistency trap `get_user` already
    // avoids for the equivalent reason (see CLAUDE.md).
    let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Organizations::new(&mut pool_conn);
    let memberships = repo.list_user_organizations(target_user_id).await?;
    let manage_keys_orgs = repo.user_manage_keys_org_ids(target_user_id).await?;

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
                    can_manage_keys: matches!(m.role.as_str(), "owner" | "admin") || manage_keys_orgs.contains(&o.id),
                    role: m.role.clone(),
                    zero_data_retention: o.zero_data_retention,
                    verified: o.verified,
                })
        })
        .collect();

    Ok(Json(summaries))
}

/// List the caller's own outstanding join requests.
///
/// The requester-side counterpart to `/organizations/{id}/join-requests`. That
/// endpoint is gated on managing the organization, which a requester by
/// definition cannot do — they are not a member yet — so without this a user
/// has no way to learn that a request exists on their behalf. Signup files one
/// automatically when the caller's email domain matches a claimed organization,
/// which makes the invisible case the common one.
///
/// Answers only "what have *I* asked to join". It deliberately does not answer
/// "does an organization exist for this domain" in general: that would let
/// anyone probe for the existence of a company's workspace by signing up.
///
/// On authorization, note that "own" is about the *subject* of the query, not
/// the caller: a platform manager holding ReadAll on Users may read another
/// user's requests, exactly as they may read that user's organizations via
/// `/users/{id}/organizations` beside it. An ordinary caller is confined to
/// their own, and a non-`current` path they don't own is a 403.
#[utoipa::path(
    get,
    path = "/users/{user_id}/join-requests",
    tag = "organizations",
    summary = "List user's pending join requests",
    description = "List organizations the user has asked to join and is awaiting a decision on. Readable by that user, or by a platform manager with read-all on users.",
    params(
        ("user_id" = String, Path, description = "User ID (UUID) or 'current' for current user"),
    ),
    responses(
        (status = 200, description = "Pending join requests", body = Vec<crate::api::models::organizations::PendingJoinRequestResponse>),
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
pub async fn list_user_join_requests<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(user_id): Path<UserIdOrCurrent>,
    current_user: CurrentUser,
) -> Result<Json<Vec<crate::api::models::organizations::PendingJoinRequestResponse>>> {
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
            resource: format!("Join requests for user {target_user_id}"),
        });
    }

    // Primary, not the replica: signup records the request during the very
    // first login, and onboarding reads this immediately afterwards to decide
    // whether to offer "request to join" instead of "create a workspace". Off
    // the replica that read can land before the row propagates, and the user
    // gets the create-workspace screen this endpoint exists to replace. Same
    // trap `list_user_organizations` above avoids for the same reason.
    let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Organizations::new(&mut pool_conn);
    let requests = repo.list_user_join_requests(target_user_id).await?;

    let org_ids: Vec<UserId> = requests.iter().map(|r| r.organization_id).collect();
    let mut users_repo = Users::new(&mut pool_conn);
    let org_map = users_repo.get_bulk(org_ids).await?;

    // `filter_map` cannot silently swallow a live request here: both halves
    // apply the same `is_deleted = false` filter — `list_user_join_requests`
    // INNER JOINs it, `get_bulk` has it in its WHERE — so a request that
    // survived the first query has its organization in `org_map`. The only way
    // to miss is the organization being deleted between the two, and dropping
    // it is then the correct answer rather than a lost row: a deleted
    // organization has nobody left to approve, which is exactly why the query
    // excludes them in the first place.
    let responses: Vec<crate::api::models::organizations::PendingJoinRequestResponse> = requests
        .iter()
        .filter_map(|r| {
            org_map
                .get(&r.organization_id)
                .map(|o| crate::api::models::organizations::PendingJoinRequestResponse {
                    id: r.id,
                    organization_id: o.id,
                    organization_name: o.username.clone(),
                    requested_at: r.created_at,
                })
        })
        .collect();

    Ok(Json(responses))
}

/// Everything onboarding needs to pick a screen, in one read.
///
/// A user arriving at onboarding is in exactly one of four situations, and
/// which one decides whether they are shown a workspace-creation form, an
/// invitation, or a notice that their company already has a workspace. The
/// client applies this precedence:
///
/// 1. **`invitation`** — someone deliberately invited this address. Skip the
///    workspace screen and let them accept.
/// 2. **`domain_match` with `auto_join_enabled`** — the workspace has said in
///    advance that its domain belongs. Complete the join, same destination.
/// 3. **`domain_match` without it** — offer the choice to ask, and let them
///    ask or not. `join_request` says whether they already have.
/// 4. **Neither** — no workspace claims their domain, or they signed up with a
///    personal address. Ordinary workspace setup.
///
/// One endpoint rather than three because the client can render nothing until
/// it knows all three answers; three round trips would buy a flash of the
/// wrong screen. That flash is the whole failure this replaces — a user shown
/// "create a workspace" while their company already has one creates a second
/// one, which is how a domain ends up with two workspaces nobody meant.
///
/// **Disclosure.** The domain is taken from the caller's own authenticated
/// address; there is no parameter to pass someone else's. Learning that
/// `acme.com` has a workspace therefore requires receiving mail at acme.com,
/// and even then reveals only that — see [`DomainMatchResponse`].
#[utoipa::path(
    get,
    path = "/users/{user_id}/onboarding-context",
    tag = "organizations",
    summary = "Onboarding routing context",
    description = "Pending invitation, matching domain workspace, and outstanding join request for a user — the inputs to the onboarding routing decision.",
    params(
        ("user_id" = String, Path, description = "User ID (UUID) or 'current' for current user"),
    ),
    responses(
        (status = 200, description = "Onboarding context", body = crate::api::models::organizations::OnboardingContextResponse),
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
pub async fn get_onboarding_context<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(user_id): Path<UserIdOrCurrent>,
    current_user: CurrentUser,
) -> Result<Json<crate::api::models::organizations::OnboardingContextResponse>> {
    use crate::api::models::organizations::{
        DomainMatchResponse, OnboardingContextResponse, PendingInvitationResponse, PendingJoinRequestResponse,
    };

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
            resource: format!("Onboarding context for user {target_user_id}"),
        });
    }

    // Primary, not the replica. This is read on the very first page load after
    // signup, and signup may have just written the membership that auto-join
    // creates. Off the replica that read can land first, and the user gets the
    // create-workspace screen this endpoint exists to keep them away from.
    // Same trap `list_user_organizations` avoids for the same reason (CLAUDE.md).
    let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;

    // The target's address, not the caller's: a platform manager reading
    // someone else's context must get *their* domain match, not their own.
    let mut users_repo = Users::new(&mut pool_conn);
    let target = users_repo.get_by_id(target_user_id).await?.ok_or_else(|| Error::NotFound {
        resource: "User".to_string(),
        id: target_user_id.to_string(),
    })?;
    let target_email = target.email.clone();

    let mut org_repo = Organizations::new(&mut pool_conn);

    // 1. An invitation addressed to this exact mailbox.
    let invite = org_repo.find_pending_invite_for_email(&target_email).await?;
    let invitation = match invite {
        Some(invite) => {
            let mut users_repo = Users::new(&mut pool_conn);
            let org = users_repo.get_by_id(invite.organization_id).await?;
            // An invitation into a workspace that has since been deleted leads
            // nowhere; the query already excludes those, so this is belt and
            // braces against the row vanishing between the two reads.
            match org {
                Some(org) => {
                    let inviter_name = match invite.invited_by {
                        Some(inviter_id) => users_repo
                            .get_by_id(inviter_id)
                            .await?
                            .map(|u| u.display_name.unwrap_or(u.username)),
                        None => None,
                    };
                    Some(PendingInvitationResponse {
                        id: invite.id,
                        organization_id: invite.organization_id,
                        organization_name: org.display_name.unwrap_or(org.username),
                        role: invite.role.clone(),
                        inviter_name,
                        expires_at: invite.expires_at,
                    })
                }
                None => None,
            }
        }
        None => None,
    };

    // 2. An outstanding request of their own.
    let mut org_repo = Organizations::new(&mut pool_conn);
    let requests = org_repo.list_user_join_requests(target_user_id).await?;
    let join_request = match requests.first() {
        Some(request) => {
            let mut users_repo = Users::new(&mut pool_conn);
            users_repo
                .get_by_id(request.organization_id)
                .await?
                .map(|org| PendingJoinRequestResponse {
                    id: request.id,
                    organization_id: org.id,
                    organization_name: org.username,
                    requested_at: request.created_at,
                })
        }
        None => None,
    };

    // 3. A workspace that has claimed their domain.
    //
    // Personal domains are excluded here exactly as they are at signup: a
    // workspace claiming gmail.com would otherwise match every consumer signup
    // and tell each of them their "company" already has a workspace.
    let domain = crate::auth::utils::email_domain(&target_email).filter(|d| !crate::auth::utils::is_personal_email_domain(d));
    let mut org_repo = Organizations::new(&mut pool_conn);
    let domain_match = match domain {
        Some(domain) => match org_repo.find_by_domain(&domain).await? {
            Some(org) => {
                // Already inside it: there is no decision left to offer, and
                // saying "this workspace exists" to a member is noise.
                let existing_role = org_repo.get_user_org_role(target_user_id, org.id).await?;
                if existing_role.is_some() {
                    None
                } else {
                    let auto_join_enabled = org_repo.auto_join_enabled(org.id).await?;
                    Some(DomainMatchResponse {
                        organization_id: org.id,
                        domain,
                        auto_join_enabled,
                    })
                }
            }
            None => None,
        },
        None => None,
    };

    Ok(Json(OnboardingContextResponse {
        invitation,
        domain_match,
        join_request,
    }))
}

/// Act on a domain match: join the workspace, or ask to.
///
/// The opt-in half of the intercept. Nothing files a join request on a user's
/// behalf any more — not signup, not the screen that tells them a workspace
/// exists. This endpoint runs only because they pressed the button.
///
/// The organization is resolved from the **caller's own email domain**, never
/// from the request body. A caller cannot name the workspace they want into:
/// that would turn this into a way to spam requests at arbitrary
/// organizations, and to enumerate which ids are organizations at all.
///
/// Which of the two things happens is the organization's choice, not the
/// caller's — see [`DomainJoinOutcome`]. Idempotent in both directions:
/// pressing the button twice returns the same outcome rather than failing, so
/// a double-click, a retry, or a reloaded page cannot produce a second row or
/// a spurious error.
#[utoipa::path(
    post,
    path = "/users/{user_id}/join-requests",
    tag = "organizations",
    summary = "Request access to the domain-matched workspace",
    description = "Ask to join the organization that has claimed the user's email domain. Becomes an immediate membership if that organization has auto-join enabled.",
    params(
        ("user_id" = String, Path, description = "User ID (UUID) or 'current' for current user"),
    ),
    responses(
        (status = 200, description = "Request filed or membership granted", body = crate::api::models::organizations::DomainJoinResponse),
        (status = 400, description = "No workspace claims this user's email domain"),
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
pub async fn create_user_join_request<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(user_id): Path<UserIdOrCurrent>,
    current_user: CurrentUser,
) -> Result<Json<crate::api::models::organizations::DomainJoinResponse>> {
    use crate::api::models::organizations::{DomainJoinOutcome, DomainJoinResponse, PendingJoinRequestResponse};

    let target_user_id = match user_id {
        UserIdOrCurrent::Current(_) => current_user.id,
        UserIdOrCurrent::Id(uuid) => uuid,
    };

    let can_all = can_read_all_resources(&current_user, Resource::Users);
    let can_own = can_read_own_resource(&current_user, Resource::Users, target_user_id);
    if !can_all && !can_own {
        return Err(Error::InsufficientPermissions {
            required: Permission::Allow(Resource::Users, Operation::UpdateOwn),
            action: Operation::UpdateOwn,
            resource: format!("Join request for user {target_user_id}"),
        });
    }

    let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;

    let mut users_repo = Users::new(&mut pool_conn);
    let target = users_repo.get_by_id(target_user_id).await?.ok_or_else(|| Error::NotFound {
        resource: "User".to_string(),
        id: target_user_id.to_string(),
    })?;

    let domain = crate::auth::utils::email_domain(&target.email)
        .filter(|d| !crate::auth::utils::is_personal_email_domain(d))
        .ok_or_else(|| Error::BadRequest {
            message: "No workspace matches this account's email domain".to_string(),
        })?;

    let mut org_repo = Organizations::new(&mut pool_conn);
    let org = org_repo.find_by_domain(&domain).await?.ok_or_else(|| Error::BadRequest {
        message: "No workspace matches this account's email domain".to_string(),
    })?;

    // Active memberships only — a `requested` row is not one, which is the
    // case that must fall through to the idempotent insert below.
    if org_repo.get_user_org_role(target_user_id, org.id).await?.is_some() {
        return Ok(Json(DomainJoinResponse {
            outcome: DomainJoinOutcome::AlreadyMember,
            organization_id: org.id,
            join_request: None,
        }));
    }

    if org_repo.auto_join_enabled(org.id).await? {
        // The organization has already said yes to everyone at its domain, so
        // there is nobody left to ask. Normally signup got here first; this
        // path covers auto-join being switched on after the user signed up.
        let org_count = org_repo.count_user_organizations(target_user_id).await?;
        if org_count >= MAX_ORGS_PER_USER {
            return Err(Error::BadRequest {
                message: format!("Cannot join: you are already a member of {MAX_ORGS_PER_USER} organizations (maximum)"),
            });
        }
        org_repo.add_member(org.id, target_user_id, "member").await?;
        return Ok(Json(DomainJoinResponse {
            outcome: DomainJoinOutcome::Joined,
            organization_id: org.id,
            join_request: None,
        }));
    }

    let request = org_repo.create_join_request(org.id, target_user_id).await?;

    Ok(Json(DomainJoinResponse {
        outcome: DomainJoinOutcome::Requested,
        organization_id: org.id,
        join_request: Some(PendingJoinRequestResponse {
            id: request.id,
            organization_id: org.id,
            organization_name: org.username,
            requested_at: request.created_at,
        }),
    }))
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

    // Invite-time pre-configuration of the additive 'manage_keys' role: the
    // grant attaches to the pending membership row and is already in place
    // when the invite is accepted. Defaults to NOT granted (blast-radius
    // minimization); owner/admin invites hold the capability implicitly.
    if !matches!(role, "owner" | "admin") && data.can_manage_keys.unwrap_or(false) {
        org_repo.set_membership_manage_keys(invite.id, true).await?;
    }

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

/// Accept an invitation the invitee reached without its link.
///
/// The by-id twin of [`accept_invite`]. Both put the same person in the same
/// organization; they differ only in what proves the mailbox.
///
/// On the link path, possession of the token is the proof — anyone holding it
/// is treated as the addressee. Here there is no token to hold: the invitee
/// arrived through onboarding because the platform told them an invitation
/// existed, and invite tokens are stored only as hashes, so the link cannot be
/// reconstructed to hand back to them. What proves the mailbox instead is the
/// caller's own authenticated address, which the identity provider vouched
/// for. **That comparison is the entire authorization for this endpoint** —
/// without it, an invitation id would be enough to join a stranger's
/// organization.
///
/// Notably this is *not* gated on organization membership, unlike its
/// neighbours on the same path. The one person entitled to accept an
/// invitation is by definition not a member yet.
#[utoipa::path(
    post,
    path = "/organizations/{id}/invites/{invite_id}/accept",
    tag = "organizations",
    summary = "Accept an invitation by id",
    description = "Accept a pending invitation addressed to the authenticated user, without its emailed token.",
    params(
        ("id" = String, Path, description = "Organization ID (UUID)"),
        ("invite_id" = String, Path, description = "Invitation ID (UUID)"),
    ),
    responses(
        (status = 200, description = "Invitation accepted"),
        (status = 400, description = "Bad request - expired, email mismatch, or org limit reached"),
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
pub async fn accept_invite_by_id<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((id, invite_id)): Path<(UserId, uuid::Uuid)>,
    current_user: CurrentUser,
) -> Result<Json<serde_json::Value>> {
    let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut org_repo = Organizations::new(&mut pool_conn);

    let invite = require_own_invite(&mut org_repo, id, invite_id, &current_user).await?;

    if let Some(expires_at) = invite.expires_at
        && expires_at < chrono::Utc::now()
    {
        return Err(Error::BadRequest {
            message: "This invite has expired".to_string(),
        });
    }

    let org_count = org_repo.count_user_organizations(current_user.id).await?;
    if org_count >= MAX_ORGS_PER_USER {
        return Err(Error::BadRequest {
            message: format!("Cannot accept invite: you are already a member of {MAX_ORGS_PER_USER} organizations (maximum)"),
        });
    }

    org_repo.accept_invite(invite.id, current_user.id).await?;

    Ok(Json(serde_json::json!({ "message": "Invite accepted" })))
}

/// Decline an invitation the invitee reached without its link.
///
/// The by-id twin of [`decline_invite`], authorized the same way as
/// [`accept_invite_by_id`]. Declining deletes the row, so the organization can
/// invite them again later and onboarding stops offering it.
///
/// No expiry check: letting someone clear an invitation that has already
/// lapsed is strictly helpful, and refusing would leave a dead row offered to
/// them forever.
#[utoipa::path(
    post,
    path = "/organizations/{id}/invites/{invite_id}/decline",
    tag = "organizations",
    summary = "Decline an invitation by id",
    description = "Decline a pending invitation addressed to the authenticated user, without its emailed token.",
    params(
        ("id" = String, Path, description = "Organization ID (UUID)"),
        ("invite_id" = String, Path, description = "Invitation ID (UUID)"),
    ),
    responses(
        (status = 200, description = "Invitation declined"),
        (status = 400, description = "Bad request - email mismatch"),
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
pub async fn decline_invite_by_id<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((id, invite_id)): Path<(UserId, uuid::Uuid)>,
    current_user: CurrentUser,
) -> Result<Json<serde_json::Value>> {
    let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut org_repo = Organizations::new(&mut pool_conn);

    let invite = require_own_invite(&mut org_repo, id, invite_id, &current_user).await?;
    org_repo.cancel_invite(invite.organization_id, invite.id).await?;

    Ok(Json(serde_json::json!({ "message": "Invite declined" })))
}

/// Load a pending invitation and prove it is addressed to the caller.
///
/// Shared by the two by-id endpoints so the address comparison cannot be
/// present on one and forgotten on the other — it is the only thing standing
/// between an invitation id and someone else's organization.
///
/// A mismatch is reported as a 404 rather than a 403: to a caller the
/// invitation is not theirs to know about, and distinguishing "wrong person"
/// from "no such invitation" would confirm that a given id is a live
/// invitation into a given organization.
///
/// An invitation with no `invite_email` cannot be matched against anyone and
/// is refused rather than waved through.
async fn require_own_invite(
    org_repo: &mut Organizations<'_>,
    org_id: UserId,
    invite_id: uuid::Uuid,
    current_user: &CurrentUser,
) -> Result<crate::db::models::organizations::OrganizationMemberDBResponse> {
    let not_found = || Error::NotFound {
        resource: "Invite".to_string(),
        id: invite_id.to_string(),
    };

    let invite = org_repo.find_invite_by_id(org_id, invite_id).await?.ok_or_else(not_found)?;

    let addressed_to_caller = invite
        .invite_email
        .as_ref()
        .is_some_and(|email| email.to_lowercase() == current_user.email.to_lowercase());

    if !addressed_to_caller {
        return Err(not_found());
    }

    Ok(invite)
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

/// Confirm one side of a pending organization email change.
///
/// The PATCH endpoint sends two verification links: one to the new contact
/// address (the "new" side) and one to the current contact address (the
/// "old" side). Each link carries a distinct token. The change is only
/// applied to `users.email` once BOTH sides have been clicked within their
/// 24-hour TTL — clicking one side records the confirmation and either
/// finalizes the change (if the other side is already confirmed) or shows
/// a "waiting on the other party" page.
///
/// Mail clients render the links as plain anchors, so this is a `GET`
/// endpoint that returns a small HTML page describing the result — no
/// dashboard route is required to make the link work, and no separate API
/// call is needed.
///
/// No authentication is required because the secret token itself proves
/// possession of the corresponding mailbox. (Note: this codebase's
/// authentication is opt-in via the `CurrentUser` extractor on each
/// handler, not via a router layer around `/admin/api/v1`, so the absence
/// of `CurrentUser` here is what makes the endpoint public.)
///
/// **Pool routing:** this endpoint MUST always run against the primary
/// database pool. The transaction below uses `state.db.write().begin()`,
/// which routes to the primary via the `DbPools.write()` API. Do not add
/// a replica fast-path here or split the consume + update across pools —
/// the security guarantee (no replay, atomic finalize) only holds if all
/// statements run inside the same primary-pool transaction.
#[utoipa::path(
    get,
    path = "/organizations/email-change/{token}/confirm",
    tag = "organizations",
    summary = "Confirm one side of an organization email change",
    description = "Mark one side (old or new) of a pending organization email change as confirmed. When both sides are confirmed, the change is applied. Returns an HTML page.",
    params(
        ("token" = String, Path, description = "Email change verification token"),
    ),
    responses(
        (status = 200, description = "Side recorded as confirmed, or change applied (HTML page)"),
        (status = 404, description = "Invalid, already-used, or expired token (HTML page)"),
    ),
)]
#[tracing::instrument(skip_all)]
pub async fn confirm_email_change<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(token): Path<String>,
) -> Result<axum::response::Response> {
    let token_hash = hash_invite_token(&token);

    // The lookup + finalize pair runs in a single transaction so a failure
    // applying the change to `users.email` rolls back the confirmation
    // timestamp update — leaving the token usable for a retry rather than
    // silently consumed with no email change applied.
    let mut tx = state.db.write().begin().await.map_err(|e| Error::Database(e.into()))?;
    let mut org_repo = Organizations::new(&mut tx);

    // Try the token against each side. Each `confirm_*_email_side` call is an
    // atomic `UPDATE … RETURNING` that matches at most one row, scoped to the
    // corresponding `*_email_token_hash` column AND (not already confirmed)
    // AND (not expired) AND (org not soft-deleted). Because the two token-hash
    // columns are independently UNIQUE, exactly one of these two queries can
    // match a given click, so the `else if` short-circuits the second
    // `UPDATE`: this click only ever updates one column on at most one row.
    let (pending, just_confirmed_side) = if let Some(p) = org_repo.confirm_new_email_side(&token_hash).await? {
        (p, ConfirmedSide::New)
    } else if let Some(p) = org_repo.confirm_old_email_side(&token_hash).await? {
        (p, ConfirmedSide::Old)
    } else {
        // No row matched — invalid, already-confirmed, expired, or org soft-deleted.
        tx.rollback().await.map_err(|e| Error::Database(e.into()))?;
        return Ok(email_change_html(
            StatusCode::NOT_FOUND,
            "This confirmation link is invalid, has already been used, or has expired.",
        ));
    };

    if pending.is_fully_confirmed() {
        // Apply the email change and delete the pending row, all in this
        // transaction — so a failure here rolls back the just-recorded
        // confirmation and the user can retry.
        let update = OrganizationUpdateDBRequest {
            display_name: None,
            avatar_url: None,
            email: Some(pending.new_email.clone()),
            batch_notifications_enabled: None,
            low_balance_threshold: None,
            zero_data_retention: None,
        };
        org_repo.update(pending.organization_id, &update).await?;
        // The `confirm_*_email_side` UPDATE above already locked this row, so
        // a `false` here would mean the row was somehow deleted from within
        // our own transaction — a logic bug, not a race. Fail loudly so we
        // don't silently commit a state where `users.email` was updated but
        // the pending row stuck around.
        let deleted = org_repo.delete_pending_email_change(pending.id).await?;
        if !deleted {
            return Err(Error::Internal {
                operation: format!(
                    "delete_pending_email_change for {} returned false inside confirm transaction; logic bug",
                    pending.id,
                ),
            });
        }
        tx.commit().await.map_err(|e| Error::Database(e.into()))?;

        return Ok(email_change_html(
            StatusCode::OK,
            "Your organization's contact email has been updated. You can close this tab.",
        ));
    }

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    let waiting_message = match just_confirmed_side {
        ConfirmedSide::New => {
            "Thanks — we've verified the new address. The change will take effect once the current contact also confirms via the link sent to their inbox."
        }
        ConfirmedSide::Old => {
            "Thanks — we've recorded the current owner's authorization. The change will take effect once the new address also confirms via the link sent to their inbox."
        }
    };
    Ok(email_change_html(StatusCode::OK, waiting_message))
}

/// Which side of the double-opt-in flow was just confirmed by the
/// most recent click.
enum ConfirmedSide {
    New,
    Old,
}

/// Render a minimal HTML confirmation page for the email-change flow.
///
/// Hardening headers, even though the threat model for this GET endpoint is
/// low (the token reaches the user out-of-band via email; no other origin
/// learns it):
///
/// * `Referrer-Policy: no-referrer` — if the user navigates onward from
///   this page (or any embedded resource it loaded), the verification URL
///   must not leak to the next origin via the `Referer` header.
/// * `Cache-Control: no-store` — keeps the URL out of intermediate caches
///   and browser disk cache; pairs with `Pragma: no-cache` for legacy proxies.
fn email_change_html(status: StatusCode, message: &str) -> axum::response::Response {
    let body = format!(
        r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><meta name="referrer" content="no-referrer"><title>Email change</title></head>
<body style="font-family: system-ui, sans-serif; max-width: 540px; margin: 60px auto; padding: 20px; color: #333;">
<h2>Organization contact email</h2>
<p>{}</p>
</body></html>"#,
        html_escape(message),
    );
    axum::response::Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::REFERRER_POLICY, "no-referrer")
        .header(header::CACHE_CONTROL, "no-store")
        .header("Pragma", "no-cache")
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
        create_test_user_on_domain,
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
        // The username is the owner's email domain plus a suffix - the domain
        // is what makes the workspace findable by colleagues, the suffix keeps
        // it unique so they can have more than one. The submitted name becomes
        // the display name.
        let username = body["username"].as_str().unwrap();
        assert!(username.starts_with("example.com~"), "got {username}");
        assert_eq!(body["display_name"].as_str().unwrap(), "my-org");
    }

    /// Domains are case-insensitive; the things we compare them against are
    /// not. Without normalising, `Alice@Acme.test` claims `Acme.test` and a
    /// later `bob@acme.test` matches nothing.
    #[sqlx::test]
    #[test_log::test]
    async fn test_domain_claim_is_case_insensitive(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_on_domain(&pool, Role::StandardUser, "MixedCase.test").await;
        let headers = add_auth_headers(&user);

        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .json(&json!({ "name": "Mixed Case Ltd", "email": "billing@mixedcase.test" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let username = resp.json::<serde_json::Value>()["username"].as_str().unwrap().to_string();
        assert!(
            username.starts_with("mixedcase.test~"),
            "claim should be lowercased, got {username}"
        );

        // A colleague whose address is cased differently still finds it.
        let mut conn = pool.acquire().await.unwrap();
        assert!(
            crate::db::handlers::Organizations::new(&mut conn)
                .find_by_domain("mixedcase.test")
                .await
                .unwrap()
                .is_some(),
            "a differently-cased colleague must still match the workspace"
        );
    }

    /// A domain can hold several workspaces - prod and dev, say - and a join
    /// request goes to the oldest surviving one.
    #[sqlx::test]
    #[test_log::test]
    async fn test_domain_can_hold_several_workspaces(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_on_domain(&pool, Role::StandardUser, "acme.test").await;
        let headers = add_auth_headers(&user);

        let mut ids = Vec::new();
        for name in ["Acme Prod", "Acme Dev"] {
            let resp = server
                .post("/admin/api/v1/organizations")
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
                .json(&json!({ "name": name, "email": "billing@acme.test" }))
                .await;
            resp.assert_status(axum::http::StatusCode::CREATED);
            let body = resp.json::<serde_json::Value>();
            assert!(body["username"].as_str().unwrap().starts_with("acme.test~"));
            ids.push(body["id"].as_str().unwrap().to_string());
        }
        assert_ne!(ids[0], ids[1], "both workspaces exist independently");

        // A colleague's join request goes to the first one created.
        let mut conn = pool.acquire().await.unwrap();
        let matched = crate::db::handlers::Organizations::new(&mut conn)
            .find_by_domain("acme.test")
            .await
            .unwrap()
            .expect("a workspace matches the domain");
        assert_eq!(matched.id.to_string(), ids[0], "the oldest surviving workspace wins");
    }

    /// A soft-deleted workspace must never receive join requests - nobody is
    /// left to approve them.
    #[sqlx::test]
    #[test_log::test]
    async fn test_deleted_workspace_is_not_matched_by_domain(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let user = create_test_user_on_domain(&pool, Role::StandardUser, "gone.test").await;
        let headers = add_auth_headers(&user);

        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .json(&json!({ "name": "Gone Ltd", "email": "billing@gone.test" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

        {
            let mut conn = pool.acquire().await.unwrap();
            assert!(
                crate::db::handlers::Organizations::new(&mut conn)
                    .find_by_domain("gone.test")
                    .await
                    .unwrap()
                    .is_some(),
                "matches while it exists"
            );
        }

        let resp = server
            .delete(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .await;
        assert!(resp.status_code().is_success(), "delete failed: {}", resp.text());

        let mut conn = pool.acquire().await.unwrap();
        assert!(
            crate::db::handlers::Organizations::new(&mut conn)
                .find_by_domain("gone.test")
                .await
                .unwrap()
                .is_none(),
            "a deleted workspace must not collect join requests"
        );
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

    /// The org's verification is its own, not the member's.
    ///
    /// Paying as an org admin verifies the organization rather than the human
    /// who clicked, so a client asking "can this workspace pay" has to read the
    /// summary. This pins that the two never get conflated.
    #[sqlx::test]
    #[test_log::test]
    async fn test_user_organizations_summary_reflects_org_verification(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let user_headers = add_auth_headers(&user);

        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&user_headers[0].0, &user_headers[0].1)
            .add_header(&user_headers[1].0, &user_headers[1].1)
            .json(&json!({ "name": "verify-org", "email": "contact@verify-org.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id: uuid::Uuid = resp.json::<serde_json::Value>()["id"].as_str().unwrap().parse().unwrap();

        let orgs_url = format!("/admin/api/v1/users/{}/organizations", user.id);
        let fetch_summary = async |server: &axum_test::TestServer| -> serde_json::Value {
            let resp = server
                .get(&orgs_url)
                .add_header(&user_headers[0].0, &user_headers[0].1)
                .add_header(&user_headers[1].0, &user_headers[1].1)
                .await;
            resp.assert_status(axum::http::StatusCode::OK);
            resp.json::<Vec<serde_json::Value>>().into_iter().next().expect("one org")
        };

        // A newly created org has never paid for anything.
        assert_eq!(fetch_summary(&server).await["verified"].as_bool(), Some(false));

        // The same call the payment and card-verification paths make.
        let mut conn = pool.acquire().await.unwrap();
        crate::db::handlers::users::Users::new(&mut conn)
            .set_verified(org_id)
            .await
            .unwrap();
        drop(conn);

        assert_eq!(fetch_summary(&server).await["verified"].as_bool(), Some(true));

        // Verifying the organization must not have verified the member, or the
        // topbar would clear its badge for the wrong entity.
        let resp = server
            .get("/admin/api/v1/users/current")
            .add_header(&user_headers[0].0, &user_headers[0].1)
            .add_header(&user_headers[1].0, &user_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        assert_eq!(
            resp.json::<serde_json::Value>()["verified"].as_bool(),
            Some(false),
            "the member is still unverified in their own right"
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_user_organizations_summary_reflects_org_zero_data_retention(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let user_headers = add_auth_headers(&user);
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
        let admin_headers = add_auth_headers(&admin);

        // User self-serves an org and becomes its owner.
        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&user_headers[0].0, &user_headers[0].1)
            .add_header(&user_headers[1].0, &user_headers[1].1)
            .json(&json!({ "name": "zdr-org", "email": "contact@zdr-org.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

        let orgs_url = format!("/admin/api/v1/users/{}/organizations", user.id);

        // The membership summary defaults to zero_data_retention = false.
        let resp = server
            .get(&orgs_url)
            .add_header(&user_headers[0].0, &user_headers[0].1)
            .add_header(&user_headers[1].0, &user_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        let orgs = resp.json::<Vec<serde_json::Value>>();
        assert_eq!(orgs.len(), 1);
        assert_eq!(orgs[0]["zero_data_retention"].as_bool(), Some(false));

        // An admin enables ZDR on the org.
        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&admin_headers[0].0, &admin_headers[0].1)
            .add_header(&admin_headers[1].0, &admin_headers[1].1)
            .json(&json!({ "zero_data_retention": true }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);

        // The member's org-membership summary now reflects the org's flag.
        let resp = server
            .get(&orgs_url)
            .add_header(&user_headers[0].0, &user_headers[0].1)
            .add_header(&user_headers[1].0, &user_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        let orgs = resp.json::<Vec<serde_json::Value>>();
        assert_eq!(orgs[0]["zero_data_retention"].as_bool(), Some(true));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_organization_owner_can_update_zero_data_retention(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "name": "owner-zdr-org", "email": "contact@owner-zdr-org.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "zero_data_retention": true }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        assert_eq!(resp.json::<serde_json::Value>()["zero_data_retention"].as_bool(), Some(true));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_organization_non_owners_cannot_update_zero_data_retention(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let org_admin = create_test_user(&pool, Role::StandardUser).await;
        let member = create_test_user(&pool, Role::StandardUser).await;
        let other_owner = create_test_user(&pool, Role::StandardUser).await;
        let other_owner_headers = add_auth_headers(&other_owner);

        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&other_owner_headers[0].0, &other_owner_headers[0].1)
            .add_header(&other_owner_headers[1].0, &other_owner_headers[1].1)
            .json(&json!({ "name": "other-owner-org", "email": "contact@other-owner-org.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);

        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .json(&json!({
                "name": "non-owner-zdr-org",
                "email": "contact@non-owner-zdr-org.com",
                "owner_id": owner.id,
            }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

        for (user_id, role) in [(org_admin.id, "admin"), (member.id, "member")] {
            let resp = server
                .post(&format!("/admin/api/v1/organizations/{org_id}/members"))
                .add_header(&pm_headers[0].0, &pm_headers[0].1)
                .add_header(&pm_headers[1].0, &pm_headers[1].1)
                .json(&json!({ "user_id": user_id, "role": role }))
                .await;
            resp.assert_status(axum::http::StatusCode::CREATED);
        }

        for user in [&org_admin, &member, &other_owner] {
            let headers = add_auth_headers(user);
            let resp = server
                .patch(&format!("/admin/api/v1/organizations/{org_id}"))
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
                .json(&json!({ "zero_data_retention": true }))
                .await;
            resp.assert_status(axum::http::StatusCode::FORBIDDEN);
        }

        let resp = server
            .get(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&pm_headers[0].0, &pm_headers[0].1)
            .add_header(&pm_headers[1].0, &pm_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        assert_eq!(resp.json::<serde_json::Value>()["zero_data_retention"].as_bool(), Some(false));
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

    #[sqlx::test]
    #[test_log::test]
    async fn test_resend_invite_rotates_the_token(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "name": "resend-org", "email": "resend@example.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/invites"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "email": "invitee@example.com", "role": "member" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let invite_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();

        let hash_before: Option<String> = sqlx::query_scalar!(
            "SELECT invite_token_hash FROM user_organizations WHERE id = $1",
            uuid::Uuid::parse_str(&invite_id).unwrap()
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/invites/{invite_id}/resend"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::NO_CONTENT);

        let hash_after: Option<String> = sqlx::query_scalar!(
            "SELECT invite_token_hash FROM user_organizations WHERE id = $1",
            uuid::Uuid::parse_str(&invite_id).unwrap()
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert!(hash_before.is_some() && hash_after.is_some());
        assert_ne!(hash_before, hash_after, "the old invite link must stop working");
    }

    // ── Join requests ──────────────────────────────────────────────────────

    /// Create an org owned by `owner` and park a join request from `joiner`.
    async fn org_with_join_request(pool: &PgPool, owner_id: uuid::Uuid, joiner_id: uuid::Uuid) -> (uuid::Uuid, uuid::Uuid) {
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = crate::db::handlers::Organizations::new(&mut conn);
        let org = repo
            .create(
                &crate::db::models::organizations::OrganizationCreateDBRequest {
                    name: format!("jr-{}", &owner_id.to_string()[..8]),
                    email: format!("org-{}@example.com", &owner_id.to_string()[..8]),
                    display_name: None,
                    avatar_url: None,
                    created_by: owner_id,
                },
                &[Role::StandardUser],
            )
            .await
            .unwrap();
        let request = repo.create_join_request(org.id, joiner_id).await.unwrap();
        (org.id, request.id)
    }

    /// An organization that has claimed `domain`, the way creating a workspace
    /// does: the domain is the username, so `find_by_domain` matches it.
    async fn org_claiming_domain(pool: &PgPool, owner_id: uuid::Uuid, domain: &str) -> uuid::Uuid {
        let mut conn = pool.acquire().await.unwrap();
        let org = crate::db::handlers::Organizations::new(&mut conn)
            .create(
                &crate::db::models::organizations::OrganizationCreateDBRequest {
                    name: domain.to_string(),
                    email: format!("billing@{domain}"),
                    display_name: Some("Acme Corporation".to_string()),
                    avatar_url: None,
                    created_by: owner_id,
                },
                &[Role::StandardUser],
            )
            .await
            .unwrap();
        org.id
    }

    async fn set_auto_join(pool: &PgPool, org_id: uuid::Uuid, enabled: bool) {
        let mut conn = pool.acquire().await.unwrap();
        crate::db::handlers::Organizations::new(&mut conn)
            .set_auto_join_enabled(org_id, enabled)
            .await
            .unwrap();
    }

    /// Park an invitation addressed to `email`, as the invite endpoint would.
    async fn invite_email_to_org(pool: &PgPool, org_id: uuid::Uuid, inviter_id: uuid::Uuid, email: &str) -> uuid::Uuid {
        let mut conn = pool.acquire().await.unwrap();
        crate::db::handlers::Organizations::new(&mut conn)
            .create_invite(
                org_id,
                None,
                email,
                "member",
                inviter_id,
                "test-token-hash",
                chrono::Utc::now() + chrono::Duration::days(7),
            )
            .await
            .unwrap()
            .id
    }

    fn onboarding_context(
        server: &axum_test::TestServer,
        user: &crate::api::models::users::UserResponse,
    ) -> impl Future<Output = serde_json::Value> {
        let headers = add_auth_headers(user);
        let request = server
            .get("/admin/api/v1/users/current/onboarding-context")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1);
        async move {
            let resp = request.await;
            resp.assert_status_ok();
            resp.json::<serde_json::Value>()
        }
    }

    /// Scenario 3: the domain matches, auto-join is off, and the user is told
    /// so and left to decide. Nothing is filed for them — that silent filing
    /// is precisely what this replaces.
    #[sqlx::test]
    #[test_log::test]
    async fn test_onboarding_context_offers_the_choice_on_a_domain_match(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user_on_domain(&pool, Role::StandardUser, "acme.test").await;
        let org_id = org_claiming_domain(&pool, owner.id, "acme.test").await;
        let joiner = create_test_user_on_domain(&pool, Role::StandardUser, "acme.test").await;

        let body = onboarding_context(&server, &joiner).await;

        let matched = &body["domain_match"];
        assert_eq!(matched["organization_id"].as_str().unwrap(), org_id.to_string());
        assert_eq!(matched["domain"].as_str().unwrap(), "acme.test");
        assert_eq!(matched["auto_join_enabled"].as_bool().unwrap(), false);
        assert!(body["invitation"].is_null());
        assert!(body["join_request"].is_null(), "nothing may be filed on their behalf");

        let mut conn = pool.acquire().await.unwrap();
        assert!(
            crate::db::handlers::Organizations::new(&mut conn)
                .list_join_requests(org_id)
                .await
                .unwrap()
                .is_empty(),
            "reading the context must not queue anything"
        );
    }

    /// The match names the domain and nothing else.
    ///
    /// Receiving mail at acme.test earns you "your company has a workspace"
    /// and no more. Leaking the workspace's display name, size or owner would
    /// turn every signup into a lookup of what a given company runs.
    #[sqlx::test]
    #[test_log::test]
    async fn test_domain_match_names_only_the_domain(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user_on_domain(&pool, Role::StandardUser, "acme.test").await;
        org_claiming_domain(&pool, owner.id, "acme.test").await;
        let joiner = create_test_user_on_domain(&pool, Role::StandardUser, "acme.test").await;

        let body = onboarding_context(&server, &joiner).await;
        let matched = body["domain_match"].as_object().unwrap();

        // Asserted as an exact key set rather than a list of absences: a field
        // added to the struct later would slip past `get(..).is_none()` checks,
        // and this response is a disclosure boundary.
        let mut keys: Vec<&str> = matched.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["auto_join_enabled", "domain", "organization_id"]);
        assert!(
            !serde_json::to_string(&body).unwrap().contains("Acme Corporation"),
            "the workspace's display name must not reach a non-member"
        );
    }

    /// Scenario 4: a personal address matches nothing, even if some workspace
    /// has claimed gmail.com. Otherwise every consumer signup would be told
    /// their "company" already has a workspace.
    #[sqlx::test]
    #[test_log::test]
    async fn test_onboarding_context_ignores_personal_domains(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user(&pool, Role::StandardUser).await;
        org_claiming_domain(&pool, owner.id, "gmail.com").await;
        let consumer = create_test_user_on_domain(&pool, Role::StandardUser, "gmail.com").await;

        let body = onboarding_context(&server, &consumer).await;

        assert!(body["domain_match"].is_null());
        assert!(body["invitation"].is_null());
        assert!(body["join_request"].is_null());
    }

    /// Scenario 1: an explicit invitation is surfaced, with enough to render
    /// the accept screen — this one *was* a deliberate human decision, so it
    /// names the organization.
    #[sqlx::test]
    #[test_log::test]
    async fn test_onboarding_context_surfaces_an_invitation(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let org_id = org_claiming_domain(&pool, owner.id, "elsewhere.test").await;
        let invitee = create_test_user_on_domain(&pool, Role::StandardUser, "unrelated.test").await;
        let invite_id = invite_email_to_org(&pool, org_id, owner.id, &invitee.email).await;

        let body = onboarding_context(&server, &invitee).await;

        let invitation = &body["invitation"];
        assert_eq!(invitation["id"].as_str().unwrap(), invite_id.to_string());
        assert_eq!(invitation["organization_id"].as_str().unwrap(), org_id.to_string());
        assert_eq!(invitation["organization_name"].as_str().unwrap(), "Acme Corporation");
        assert_eq!(invitation["role"].as_str().unwrap(), "member");
        assert!(body["domain_match"].is_null(), "their own domain claims nothing");
    }

    /// Once you're in, there is no decision left to offer.
    #[sqlx::test]
    #[test_log::test]
    async fn test_onboarding_context_goes_quiet_for_a_member(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user_on_domain(&pool, Role::StandardUser, "acme.test").await;
        let org_id = org_claiming_domain(&pool, owner.id, "acme.test").await;
        let member = create_test_user_on_domain(&pool, Role::StandardUser, "acme.test").await;

        assert!(
            !onboarding_context(&server, &member).await["domain_match"].is_null(),
            "offered before they are in"
        );

        {
            let mut conn = pool.acquire().await.unwrap();
            crate::db::handlers::Organizations::new(&mut conn)
                .add_member(org_id, member.id, "member")
                .await
                .unwrap();
        }

        assert!(
            onboarding_context(&server, &member).await["domain_match"].is_null(),
            "and silent once they are"
        );
    }

    /// Asking is a thing the user does, and doing it twice is not an error.
    ///
    /// The button is real UI: a double-click, a retry after a flaky network,
    /// or a reloaded tab must not produce a second row or a spurious failure.
    #[sqlx::test]
    #[test_log::test]
    async fn test_request_access_is_opt_in_and_idempotent(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user_on_domain(&pool, Role::StandardUser, "acme.test").await;
        let org_id = org_claiming_domain(&pool, owner.id, "acme.test").await;
        let joiner = create_test_user_on_domain(&pool, Role::StandardUser, "acme.test").await;
        let headers = add_auth_headers(&joiner);

        let ask = || async {
            let resp = server
                .post("/admin/api/v1/users/current/join-requests")
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
                .await;
            resp.assert_status_ok();
            resp.json::<serde_json::Value>()
        };

        let first = ask().await;
        assert_eq!(first["outcome"].as_str().unwrap(), "requested");
        assert_eq!(first["organization_id"].as_str().unwrap(), org_id.to_string());
        let request_id = first["join_request"]["id"].as_str().unwrap().to_string();

        let second = ask().await;
        assert_eq!(second["outcome"].as_str().unwrap(), "requested");
        assert_eq!(
            second["join_request"]["id"].as_str().unwrap(),
            request_id,
            "the same request, not a second one"
        );

        let mut conn = pool.acquire().await.unwrap();
        let queued = crate::db::handlers::Organizations::new(&mut conn)
            .list_join_requests(org_id)
            .await
            .unwrap();
        assert_eq!(queued.len(), 1);

        // And the user can now see it, so onboarding renders the sent state.
        let body = onboarding_context(&server, &joiner).await;
        assert_eq!(body["join_request"]["id"].as_str().unwrap(), request_id);
        assert!(
            !body["domain_match"].is_null(),
            "still on the intercept screen, now showing the request as sent"
        );
    }

    /// Scenario 2, late: the owner switched auto-join on after this user
    /// signed up, so there is nobody left to ask and they go straight in.
    #[sqlx::test]
    #[test_log::test]
    async fn test_request_access_joins_immediately_when_auto_join_is_on(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user_on_domain(&pool, Role::StandardUser, "acme.test").await;
        let org_id = org_claiming_domain(&pool, owner.id, "acme.test").await;
        let joiner = create_test_user_on_domain(&pool, Role::StandardUser, "acme.test").await;
        set_auto_join(&pool, org_id, true).await;
        let headers = add_auth_headers(&joiner);

        assert_eq!(
            onboarding_context(&server, &joiner).await["domain_match"]["auto_join_enabled"]
                .as_bool()
                .unwrap(),
            true,
            "the client is told not to bother asking"
        );

        let resp = server
            .post("/admin/api/v1/users/current/join-requests")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await;
        resp.assert_status_ok();
        let body = resp.json::<serde_json::Value>();
        assert_eq!(body["outcome"].as_str().unwrap(), "joined");
        assert!(body["join_request"].is_null());

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = crate::db::handlers::Organizations::new(&mut conn);
        assert_eq!(repo.get_user_org_role(joiner.id, org_id).await.unwrap(), Some("member".to_string()));
        assert!(
            repo.list_join_requests(org_id).await.unwrap().is_empty(),
            "nobody was queued — they were admitted"
        );
    }

    /// The organization is resolved from the caller's own address, so a caller
    /// with no matching domain has nothing to ask and no way to name a target.
    #[sqlx::test]
    #[test_log::test]
    async fn test_request_access_needs_a_matching_domain(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user_on_domain(&pool, Role::StandardUser, "acme.test").await;
        org_claiming_domain(&pool, owner.id, "acme.test").await;
        let outsider = create_test_user_on_domain(&pool, Role::StandardUser, "elsewhere.test").await;
        let headers = add_auth_headers(&outsider);

        let resp = server
            .post("/admin/api/v1/users/current/join-requests")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    /// Scenario 1's acceptance path. The invitee never opened the emailed
    /// link — and could not be handed it, since only its hash is stored — so
    /// they accept by id, proving the mailbox with their own address instead.
    #[sqlx::test]
    #[test_log::test]
    async fn test_invitation_can_be_accepted_by_id_without_its_token(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let org_id = org_claiming_domain(&pool, owner.id, "acme.test").await;
        let invitee = create_test_user_on_domain(&pool, Role::StandardUser, "unrelated.test").await;
        let invite_id = invite_email_to_org(&pool, org_id, owner.id, &invitee.email).await;
        let headers = add_auth_headers(&invitee);

        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/invites/{invite_id}/accept"))
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await;
        resp.assert_status_ok();

        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(
            crate::db::handlers::Organizations::new(&mut conn)
                .get_user_org_role(invitee.id, org_id)
                .await
                .unwrap(),
            Some("member".to_string())
        );

        // The invitation is spent, so onboarding stops offering it.
        assert!(onboarding_context(&server, &invitee).await["invitation"].is_null());
    }

    /// The address comparison is the whole authorization: without it an
    /// invitation id would be enough to join a stranger's organization.
    ///
    /// Reported as 404 rather than 403 — telling a caller "that invitation is
    /// real, just not yours" confirms a live invitation into a named org.
    #[sqlx::test]
    #[test_log::test]
    async fn test_invitation_by_id_is_refused_to_anyone_else(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let org_id = org_claiming_domain(&pool, owner.id, "acme.test").await;
        let invitee = create_test_user_on_domain(&pool, Role::StandardUser, "unrelated.test").await;
        let invite_id = invite_email_to_org(&pool, org_id, owner.id, &invitee.email).await;

        let stranger = create_test_user_on_domain(&pool, Role::StandardUser, "stranger.test").await;
        let headers = add_auth_headers(&stranger);

        for action in ["accept", "decline"] {
            let resp = server
                .post(&format!("/admin/api/v1/organizations/{org_id}/invites/{invite_id}/{action}"))
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
                .await;
            resp.assert_status(axum::http::StatusCode::NOT_FOUND);
        }

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = crate::db::handlers::Organizations::new(&mut conn);
        assert_eq!(repo.get_user_org_role(stranger.id, org_id).await.unwrap(), None);
        assert!(
            repo.find_invite_by_id(org_id, invite_id).await.unwrap().is_some(),
            "the real invitee's invitation must survive the attempt"
        );
    }

    /// Declining clears it, so onboarding falls through to ordinary setup.
    #[sqlx::test]
    #[test_log::test]
    async fn test_invitation_can_be_declined_by_id(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let org_id = org_claiming_domain(&pool, owner.id, "acme.test").await;
        let invitee = create_test_user_on_domain(&pool, Role::StandardUser, "unrelated.test").await;
        let invite_id = invite_email_to_org(&pool, org_id, owner.id, &invitee.email).await;
        let headers = add_auth_headers(&invitee);

        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/invites/{invite_id}/decline"))
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await;
        resp.assert_status_ok();

        let body = onboarding_context(&server, &invitee).await;
        assert!(body["invitation"].is_null());

        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(
            crate::db::handlers::Organizations::new(&mut conn)
                .get_user_org_role(invitee.id, org_id)
                .await
                .unwrap(),
            None,
            "declining must not admit them"
        );
    }

    /// Auto-join decides who gets in with nobody reviewing them, so it sits
    /// beside zero data retention on the owner-only side of the line.
    #[sqlx::test]
    #[test_log::test]
    async fn test_auto_join_is_owner_only(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user_on_domain(&pool, Role::StandardUser, "acme.test").await;
        let org_id = org_claiming_domain(&pool, owner.id, "acme.test").await;
        let admin = create_test_user_on_domain(&pool, Role::StandardUser, "acme.test").await;
        {
            let mut conn = pool.acquire().await.unwrap();
            crate::db::handlers::Organizations::new(&mut conn)
                .add_member(org_id, admin.id, "admin")
                .await
                .unwrap();
        }

        let admin_headers = add_auth_headers(&admin);
        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&admin_headers[0].0, &admin_headers[0].1)
            .add_header(&admin_headers[1].0, &admin_headers[1].1)
            .json(&json!({ "auto_join_enabled": true }))
            .await;
        resp.assert_status(axum::http::StatusCode::FORBIDDEN);

        let owner_headers = add_auth_headers(&owner);
        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "auto_join_enabled": true }))
            .await;
        resp.assert_status_ok();
        assert_eq!(resp.json::<serde_json::Value>()["auto_join_enabled"].as_bool().unwrap(), true);

        let resp = server
            .get(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .await;
        resp.assert_status_ok();
        assert_eq!(
            resp.json::<serde_json::Value>()["auto_join_enabled"].as_bool().unwrap(),
            true,
            "and it survives the round trip"
        );
    }

    /// Off unless someone turns it on — including for every organization that
    /// existed before the setting did.
    #[sqlx::test]
    #[test_log::test]
    async fn test_auto_join_defaults_off(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user_on_domain(&pool, Role::StandardUser, "acme.test").await;
        let org_id = org_claiming_domain(&pool, owner.id, "acme.test").await;
        let headers = add_auth_headers(&owner);

        let resp = server
            .get(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await;
        resp.assert_status_ok();
        assert_eq!(resp.json::<serde_json::Value>()["auto_join_enabled"].as_bool().unwrap(), false);
    }

    /// Someone else's onboarding context is not readable.
    #[sqlx::test]
    #[test_log::test]
    async fn test_onboarding_context_is_not_readable_for_others(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let target = create_test_user_on_domain(&pool, Role::StandardUser, "acme.test").await;
        let snooper = create_test_user_on_domain(&pool, Role::StandardUser, "elsewhere.test").await;
        let headers = add_auth_headers(&snooper);

        for path in [
            format!("/admin/api/v1/users/{}/onboarding-context", target.id),
            format!("/admin/api/v1/users/{}/join-requests", target.id),
        ] {
            let resp = server
                .get(&path)
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
                .await;
            resp.assert_status(axum::http::StatusCode::FORBIDDEN);
        }

        let resp = server
            .post(&format!("/admin/api/v1/users/{}/join-requests", target.id))
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::FORBIDDEN);
    }

    /// The queue names people who share your email domain, so it's gated on
    /// managing the org — being a member of it isn't enough.
    #[sqlx::test]
    #[test_log::test]
    async fn test_join_requests_hidden_from_ordinary_members(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let joiner = create_test_user(&pool, Role::StandardUser).await;
        let (org_id, _) = org_with_join_request(&pool, owner.id, joiner.id).await;

        let member = create_test_user(&pool, Role::StandardUser).await;
        {
            let mut conn = pool.acquire().await.unwrap();
            crate::db::handlers::Organizations::new(&mut conn)
                .add_member(org_id, member.id, "member")
                .await
                .unwrap();
        }
        let member_headers = add_auth_headers(&member);

        let resp = server
            .get(&format!("/admin/api/v1/organizations/{org_id}/join-requests"))
            .add_header(&member_headers[0].0, &member_headers[0].1)
            .add_header(&member_headers[1].0, &member_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::FORBIDDEN);

        let owner_headers = add_auth_headers(&owner);
        let resp = server
            .get(&format!("/admin/api/v1/organizations/{org_id}/join-requests"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .await;
        resp.assert_status_ok();
        assert_eq!(resp.json::<serde_json::Value>().as_array().unwrap().len(), 1);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_approve_join_request_makes_an_active_member(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let joiner = create_test_user(&pool, Role::StandardUser).await;
        let (org_id, request_id) = org_with_join_request(&pool, owner.id, joiner.id).await;
        let owner_headers = add_auth_headers(&owner);

        // Not a member while the request is outstanding.
        {
            let mut conn = pool.acquire().await.unwrap();
            let role = crate::db::handlers::Organizations::new(&mut conn)
                .get_user_org_role(joiner.id, org_id)
                .await
                .unwrap();
            assert_eq!(role, None);
        }

        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/join-requests/{request_id}/approve"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::NO_CONTENT);

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = crate::db::handlers::Organizations::new(&mut conn);
        assert_eq!(repo.get_user_org_role(joiner.id, org_id).await.unwrap(), Some("member".to_string()));
        assert!(repo.list_join_requests(org_id).await.unwrap().is_empty(), "queue is cleared");

        // Approving twice must not double-add.
        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/join-requests/{request_id}/approve"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::NOT_FOUND);
    }

    /// Declining removes the row rather than tombstoning it, so the user can
    /// ask again - a permanent record would silently block them forever via
    /// the unique constraint on (user_id, organization_id).
    #[sqlx::test]
    #[test_log::test]
    async fn test_decline_join_request_allows_asking_again(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let joiner = create_test_user(&pool, Role::StandardUser).await;
        let (org_id, request_id) = org_with_join_request(&pool, owner.id, joiner.id).await;
        let owner_headers = add_auth_headers(&owner);

        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/join-requests/{request_id}/decline"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::NO_CONTENT);

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = crate::db::handlers::Organizations::new(&mut conn);
        assert!(repo.list_join_requests(org_id).await.unwrap().is_empty());
        assert_eq!(repo.get_user_org_role(joiner.id, org_id).await.unwrap(), None);

        // The door isn't bolted: they can request again.
        repo.create_join_request(org_id, joiner.id).await.expect("can ask again");
    }

    /// Managing one org must not grant authority over another's queue.
    #[sqlx::test]
    #[test_log::test]
    async fn test_join_request_decisions_are_scoped_to_their_org(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner_a = create_test_user(&pool, Role::StandardUser).await;
        let owner_b = create_test_user(&pool, Role::StandardUser).await;
        let joiner = create_test_user(&pool, Role::StandardUser).await;

        let (_org_a, _) = org_with_join_request(&pool, owner_a.id, joiner.id).await;
        let other = create_test_user(&pool, Role::StandardUser).await;
        let (org_b, request_b) = org_with_join_request(&pool, owner_b.id, other.id).await;

        // Owner A tries to approve a request belonging to org B, via org B's path.
        let a_headers = add_auth_headers(&owner_a);
        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_b}/join-requests/{request_b}/approve"))
            .add_header(&a_headers[0].0, &a_headers[0].1)
            .add_header(&a_headers[1].0, &a_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::FORBIDDEN);

        let mut conn = pool.acquire().await.unwrap();
        let role = crate::db::handlers::Organizations::new(&mut conn)
            .get_user_org_role(other.id, org_b)
            .await
            .unwrap();
        assert_eq!(role, None, "the request must still be outstanding");
    }

    /// The requester can't read the organization-scoped queue - that's gated on
    /// managing the org, which they can't do until they're approved. Without a
    /// user-scoped view, a request filed on their behalf at signup would be
    /// invisible to the only person waiting on it.
    #[sqlx::test]
    #[test_log::test]
    async fn test_user_sees_their_own_pending_join_request(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let joiner = create_test_user(&pool, Role::StandardUser).await;
        let (org_id, request_id) = org_with_join_request(&pool, owner.id, joiner.id).await;
        let joiner_headers = add_auth_headers(&joiner);

        // The org-scoped queue stays shut to them.
        let resp = server
            .get(&format!("/admin/api/v1/organizations/{org_id}/join-requests"))
            .add_header(&joiner_headers[0].0, &joiner_headers[0].1)
            .add_header(&joiner_headers[1].0, &joiner_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::FORBIDDEN);

        let resp = server
            .get("/admin/api/v1/users/current/join-requests")
            .add_header(&joiner_headers[0].0, &joiner_headers[0].1)
            .add_header(&joiner_headers[1].0, &joiner_headers[1].1)
            .await;
        resp.assert_status_ok();
        let body = resp.json::<serde_json::Value>();
        let items = body.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"].as_str().unwrap(), request_id.to_string());
        assert_eq!(items[0]["organization_id"].as_str().unwrap(), org_id.to_string());
        assert!(items[0]["organization_name"].as_str().is_some_and(|n| !n.is_empty()));

        // It carries only the organization's identity - nothing about who else
        // is in it, or is queued for it.
        assert!(items[0].get("user").is_none());
        assert!(items[0].get("member_count").is_none());
    }

    /// Approval is the end of the request, so the banner it drives must clear
    /// on its own rather than lingering after the user is already inside.
    #[sqlx::test]
    #[test_log::test]
    async fn test_own_join_requests_clear_once_decided(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let joiner = create_test_user(&pool, Role::StandardUser).await;
        let (org_id, request_id) = org_with_join_request(&pool, owner.id, joiner.id).await;
        let joiner_headers = add_auth_headers(&joiner);
        let owner_headers = add_auth_headers(&owner);

        let resp = server
            .get("/admin/api/v1/users/current/join-requests")
            .add_header(&joiner_headers[0].0, &joiner_headers[0].1)
            .add_header(&joiner_headers[1].0, &joiner_headers[1].1)
            .await;
        resp.assert_status_ok();
        assert_eq!(
            resp.json::<serde_json::Value>().as_array().unwrap().len(),
            1,
            "outstanding before the decision"
        );

        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/join-requests/{request_id}/approve"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::NO_CONTENT);

        let resp = server
            .get("/admin/api/v1/users/current/join-requests")
            .add_header(&joiner_headers[0].0, &joiner_headers[0].1)
            .add_header(&joiner_headers[1].0, &joiner_headers[1].1)
            .await;
        resp.assert_status_ok();
        assert!(
            resp.json::<serde_json::Value>().as_array().unwrap().is_empty(),
            "an approved request is no longer pending"
        );

        // The membership it became is visible in the place that does list it.
        let resp = server
            .get("/admin/api/v1/users/current/organizations")
            .add_header(&joiner_headers[0].0, &joiner_headers[0].1)
            .add_header(&joiner_headers[1].0, &joiner_headers[1].1)
            .await;
        resp.assert_status_ok();
        assert_eq!(resp.json::<serde_json::Value>().as_array().unwrap().len(), 1);
    }

    /// Requests are the caller's own business: one user must not be able to
    /// enumerate which organizations another has asked to join.
    #[sqlx::test]
    #[test_log::test]
    async fn test_own_join_requests_are_not_readable_by_other_users(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let joiner = create_test_user(&pool, Role::StandardUser).await;
        let (_org_id, _) = org_with_join_request(&pool, owner.id, joiner.id).await;

        let snooper = create_test_user(&pool, Role::StandardUser).await;
        let snooper_headers = add_auth_headers(&snooper);
        let resp = server
            .get(&format!("/admin/api/v1/users/{}/join-requests", joiner.id))
            .add_header(&snooper_headers[0].0, &snooper_headers[0].1)
            .add_header(&snooper_headers[1].0, &snooper_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::FORBIDDEN);

        // Their own list is unaffected by the attempt, and empty.
        let resp = server
            .get("/admin/api/v1/users/current/join-requests")
            .add_header(&snooper_headers[0].0, &snooper_headers[0].1)
            .add_header(&snooper_headers[1].0, &snooper_headers[1].1)
            .await;
        resp.assert_status_ok();
        assert!(resp.json::<serde_json::Value>().as_array().unwrap().is_empty());
    }

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

    /// Helper: plant a pending email-change row directly and return both
    /// plaintext tokens (new-side, old-side). Tests use this so they can
    /// click each verification link without going through the email-send
    /// path (which can't easily expose tokens to the test). `requested_by`
    /// defaults to the org's notional owner; `expires_at` to NOW + 1h.
    async fn plant_pending_email_change(
        pool: &PgPool,
        org_id: uuid::Uuid,
        new_email: &str,
        requested_by: crate::types::UserId,
        expires_at_relative_seconds: i64,
    ) -> (String, String) {
        let new_token = crate::auth::password::generate_reset_token();
        let old_token = crate::auth::password::generate_reset_token();
        let new_hash = super::hash_invite_token(&new_token);
        let old_hash = super::hash_invite_token(&old_token);
        sqlx::query(
            "INSERT INTO pending_org_email_changes
                (organization_id, new_email, requested_by,
                 new_email_token_hash, old_email_token_hash, expires_at)
             VALUES ($1, $2, $3, $4, $5, NOW() + ($6 || ' seconds')::interval)",
        )
        .bind(org_id)
        .bind(new_email)
        .bind(requested_by)
        .bind(&new_hash)
        .bind(&old_hash)
        .bind(expires_at_relative_seconds.to_string())
        .execute(pool)
        .await
        .unwrap();
        (new_token, old_token)
    }

    /// Helper: click a confirmation link.
    async fn click_confirm(server: &axum_test::TestServer, token: &str) -> axum_test::TestResponse {
        server
            .get(&format!("/admin/api/v1/organizations/email-change/{token}/confirm"))
            .await
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

        // The owner asks to change to a new address. The PATCH succeeds but
        // the change must be gated behind the (now double-opt-in) verification
        // flow. example.com is used because the MX-deliverability check on
        // the new domain runs as part of the PATCH.
        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "email": "attacker@example.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        let body = resp.json::<serde_json::Value>();
        assert_eq!(
            body["email"].as_str().unwrap(),
            "billing@example.com",
            "old email must remain until both sides confirm"
        );
        assert_eq!(body["pending_email_change"]["new_email"].as_str().unwrap(), "attacker@example.com");
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
    async fn test_neither_side_alone_applies_email_change(pool: PgPool) {
        // The security claim of double-opt-in: confirming only ONE side
        // (either) must not change `users.email`. Both clicks are required.
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        let org_id = create_org_for(&server, &pm_headers, "single-side-org", "billing@example.com", owner.id).await;
        let org_uuid = uuid::Uuid::parse_str(&org_id).unwrap();

        let (new_token, _old_token) = plant_pending_email_change(&pool, org_uuid, "new@example.com", owner.id, 3600).await;

        // Click the new-side link only.
        let resp = click_confirm(&server, &new_token).await;
        resp.assert_status(axum::http::StatusCode::OK);
        let body = resp.text();
        assert!(
            body.contains("current contact"),
            "expected a 'waiting on the other side' message; got: {body}"
        );

        // users.email must NOT have changed.
        let resp = server
            .get(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .await;
        assert_eq!(resp.json::<serde_json::Value>()["email"].as_str().unwrap(), "billing@example.com");

        // Pending row still exists with only the new side confirmed.
        let row: (Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
            "SELECT new_email_confirmed_at, old_email_confirmed_at FROM pending_org_email_changes WHERE organization_id = $1",
        )
        .bind(org_uuid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(row.0.is_some(), "new side should be confirmed");
        assert!(row.1.is_none(), "old side should NOT be confirmed");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_both_sides_confirm_applies_change_in_any_order(pool: PgPool) {
        // Two scenarios in one test: click new-then-old, and click old-then-new.
        // Both must result in the change being applied and the pending row removed.
        for &order in &[("new_first", true), ("old_first", false)] {
            let _ = order; // for clarity in failure messages
        }
        for &new_first in &[true, false] {
            let (server, _bg) = create_test_app(pool.clone(), false).await;
            let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
            let pm_headers = add_auth_headers(&pm);
            let owner = create_test_user(&pool, Role::StandardUser).await;
            let owner_headers = add_auth_headers(&owner);

            let label = if new_first { "new-first" } else { "old-first" };
            let org_id = create_org_for(&server, &pm_headers, &format!("apply-{label}-org"), "billing@example.com", owner.id).await;
            let org_uuid = uuid::Uuid::parse_str(&org_id).unwrap();

            let (new_token, old_token) = plant_pending_email_change(&pool, org_uuid, "new@example.com", owner.id, 3600).await;

            let (first, second) = if new_first {
                (&new_token, &old_token)
            } else {
                (&old_token, &new_token)
            };

            // First click → waiting page, email unchanged.
            click_confirm(&server, first).await.assert_status(axum::http::StatusCode::OK);
            let resp = server
                .get(&format!("/admin/api/v1/organizations/{org_id}"))
                .add_header(&owner_headers[0].0, &owner_headers[0].1)
                .add_header(&owner_headers[1].0, &owner_headers[1].1)
                .await;
            assert_eq!(
                resp.json::<serde_json::Value>()["email"].as_str().unwrap(),
                "billing@example.com",
                "{label}: email must remain after only the first confirmation",
            );

            // Second click → applied + row removed.
            let resp = click_confirm(&server, second).await;
            resp.assert_status(axum::http::StatusCode::OK);
            assert!(
                resp.text().contains("contact email has been updated"),
                "{label}: expected applied-message on second click",
            );

            let resp = server
                .get(&format!("/admin/api/v1/organizations/{org_id}"))
                .add_header(&owner_headers[0].0, &owner_headers[0].1)
                .add_header(&owner_headers[1].0, &owner_headers[1].1)
                .await;
            assert_eq!(resp.json::<serde_json::Value>()["email"].as_str().unwrap(), "new@example.com");

            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_org_email_changes WHERE organization_id = $1")
                .bind(org_uuid)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(count, 0, "{label}: pending row should be deleted once both sides confirmed");
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_confirm_email_change_invalid_token(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;

        let resp = click_confirm(&server, "not-a-real-token").await;
        resp.assert_status(axum::http::StatusCode::NOT_FOUND);
        assert!(resp.text().contains("invalid"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_expired_token_returns_not_found(pool: PgPool) {
        // The UPDATE in confirm_*_email_side filters `expires_at > NOW()`, so
        // an expired token matches no row and returns 404. The pending row is
        // NOT deleted — it just becomes inert until the org is hard-deleted
        // or a fresh PATCH supersedes it.
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        let org_id = create_org_for(&server, &pm_headers, "expired-org", "billing@example.com", owner.id).await;
        let org_uuid = uuid::Uuid::parse_str(&org_id).unwrap();

        // Negative TTL → already expired at insert time.
        let (new_token, old_token) = plant_pending_email_change(&pool, org_uuid, "new@example.com", owner.id, -3600).await;

        for token in [&new_token, &old_token] {
            let resp = click_confirm(&server, token).await;
            resp.assert_status(axum::http::StatusCode::NOT_FOUND);
        }

        // The email must NOT have been updated.
        let resp = server
            .get(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .await;
        assert_eq!(resp.json::<serde_json::Value>()["email"].as_str().unwrap(), "billing@example.com");
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
    async fn test_superseded_tokens_become_inert(pool: PgPool) {
        // After a second PATCH, BOTH of the *first* request's tokens must be
        // inert — clicking either old verification link must not move the
        // change forward and must not resurrect the old new_email.
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        let org_id = create_org_for(&server, &pm_headers, "stale-token-org", "billing@example.com", owner.id).await;
        let org_uuid = uuid::Uuid::parse_str(&org_id).unwrap();

        // Plant the first request's tokens directly so we know the plaintext.
        let (stale_new, stale_old) = plant_pending_email_change(&pool, org_uuid, "first@example.com", owner.id, 3600).await;

        // A second PATCH supersedes via UPSERT — clears confirmations, rotates tokens.
        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "email": "second@example.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);

        // Both stale tokens are inert.
        for token in [&stale_new, &stale_old] {
            click_confirm(&server, token).await.assert_status(axum::http::StatusCode::NOT_FOUND);
        }

        // The org's email is still the original — only the second token pair
        // (which the test cannot read here) could change it.
        let resp = server
            .get(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .await;
        assert_eq!(resp.json::<serde_json::Value>()["email"].as_str().unwrap(), "billing@example.com");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_confirm_token_cannot_be_replayed(pool: PgPool) {
        // Each side's UPDATE filters on `*_confirmed_at IS NULL`, so replaying
        // the same click against the same column returns 404. The other side
        // is still usable independently.
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;

        let org_id = create_org_for(&server, &pm_headers, "replay-org", "billing@example.com", owner.id).await;
        let org_uuid = uuid::Uuid::parse_str(&org_id).unwrap();

        let (new_token, _old_token) = plant_pending_email_change(&pool, org_uuid, "new@example.com", owner.id, 3600).await;

        // First new-side click succeeds (waiting message).
        click_confirm(&server, &new_token).await.assert_status(axum::http::StatusCode::OK);

        // Replay of the same new-side click is now 404 — its column is already set.
        click_confirm(&server, &new_token)
            .await
            .assert_status(axum::http::StatusCode::NOT_FOUND);
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
            .json(&json!({ "email": "attacker@example.com" }))
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
            .json(&json!({ "email": "attacker@example.com" }))
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
        let org_a_uuid = uuid::Uuid::parse_str(&org_a_id).unwrap();

        // Plant both tokens for org A so we can apply the full change.
        let (new_a, old_a) = plant_pending_email_change(&pool, org_a_uuid, "new-a@example.com", owner_a.id, 3600).await;

        click_confirm(&server, &new_a).await.assert_status(axum::http::StatusCode::OK);
        click_confirm(&server, &old_a).await.assert_status(axum::http::StatusCode::OK);

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
        // If an org is soft-deleted between PATCH and click, both tokens must
        // become inert. The UPDATE in confirm_*_email_side joins users WHERE
        // is_deleted = false, so a stale pending row matches no row.
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;

        let org_id = create_org_for(&server, &pm_headers, "soft-delete-org", "billing@example.com", owner.id).await;
        let org_uuid = uuid::Uuid::parse_str(&org_id).unwrap();

        let (new_token, old_token) = plant_pending_email_change(&pool, org_uuid, "new@example.com", owner.id, 3600).await;

        // Soft-delete the org directly (mirrors what `delete_organization` does).
        sqlx::query("UPDATE users SET is_deleted = true WHERE id = $1")
            .bind(org_uuid)
            .execute(&pool)
            .await
            .unwrap();

        // Both tokens are now inert.
        for token in [&new_token, &old_token] {
            click_confirm(&server, token).await.assert_status(axum::http::StatusCode::NOT_FOUND);
        }

        // Pending row is still present (the UPDATE didn't match anything) —
        // it'll be reaped when the org is hard-deleted via CASCADE.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_org_email_changes WHERE organization_id = $1")
            .bind(org_uuid)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_patch_writes_both_verification_emails(pool: PgPool) {
        // The file transport in the default test config writes each email to
        // a temp directory shared across the process. We scope our assertions
        // to a per-test directory via create_test_app_with_config so other
        // tests' emails don't pollute the scan. Double-opt-in means BOTH
        // mailboxes get a verification link, not just the new one.
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

        // Read all .eml files written to the scratch dir. We expect two:
        // one to the new address and one to the current address, both
        // containing a confirmation link (not a notice).
        let mut to_addresses: Vec<String> = Vec::new();
        let mut bodies: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&scratch).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) == Some("eml") {
                let body = std::fs::read_to_string(&path).unwrap();
                for line in body.lines() {
                    if let Some(rest) = line.strip_prefix("To: ") {
                        to_addresses.push(rest.trim().to_string());
                    }
                }
                bodies.push(body);
            }
        }
        to_addresses.sort();
        assert_eq!(
            to_addresses,
            vec!["current@example.com".to_string(), "new@example.com".to_string()],
            "expected verification email to both addresses; got {to_addresses:?}",
        );
        // Both emails should contain a confirmation link (the URL path is the
        // backend GET endpoint). This proves both sides got verification
        // requests rather than one verification + one notice.
        let confirm_link_count = bodies
            .iter()
            .filter(|b| b.contains("/admin/api/v1/organizations/email-change/"))
            .count();
        assert_eq!(confirm_link_count, 2, "both verification emails must carry a confirmation link");
    }

    // ── MX-record deliverability check ─────────────────────────────────

    #[sqlx::test]
    #[test_log::test]
    async fn test_mx_check_rejects_undeliverable_domain(pool: PgPool) {
        // RFC 6761 reserves `.invalid` as a TLD that must always NXDOMAIN.
        // The MX check should fail-closed on that and return 400.
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        let org_id = create_org_for(&server, &pm_headers, "mx-check-org", "billing@example.com", owner.id).await;

        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "email": "bounce@no-such-host-please.invalid" }))
            .await;
        resp.assert_status(axum::http::StatusCode::BAD_REQUEST);

        // No pending row should have been created.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_org_email_changes WHERE organization_id = $1")
            .bind(uuid::Uuid::parse_str(&org_id).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    // ── Pending state visibility ──────────────────────────────────────

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_org_surfaces_pending_email_change(pool: PgPool) {
        // A dashboard refresh mid-verification should still see the pending
        // state via GET, not only on the PATCH that created it.
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        let org_id = create_org_for(&server, &pm_headers, "pending-visible-org", "billing@example.com", owner.id).await;

        // Initiate change.
        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "email": "next@example.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);

        // GET the org — pending_email_change should be present.
        let resp = server
            .get(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        let body = resp.json::<serde_json::Value>();
        assert_eq!(body["email"].as_str().unwrap(), "billing@example.com");
        assert_eq!(
            body["pending_email_change"]["new_email"].as_str().unwrap(),
            "next@example.com",
            "GET should surface in-flight pending change; got: {body}",
        );
        assert!(body["pending_email_change"]["expires_at"].is_string());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_patch_other_fields_still_reports_pending_email_change(pool: PgPool) {
        // After initiating an email change, a follow-up PATCH that touches
        // only non-email fields must still reflect the still-pending change
        // in its response — otherwise the dashboard loses visibility.
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let pm_headers = add_auth_headers(&pm);
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        let org_id = create_org_for(&server, &pm_headers, "pending-survives-org", "billing@example.com", owner.id).await;

        // Initiate the change.
        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "email": "next@example.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);

        // Follow-up PATCH that only renames the org — no email field.
        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "display_name": "Renamed Org" }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        let body = resp.json::<serde_json::Value>();
        assert_eq!(body["display_name"].as_str().unwrap(), "Renamed Org");
        assert_eq!(
            body["pending_email_change"]["new_email"].as_str().unwrap(),
            "next@example.com",
            "non-email PATCH must still surface the in-flight change; got: {body}",
        );
    }

    // ── Additive manage_keys org role (per-member key governance) ─────────

    #[sqlx::test]
    #[test_log::test]
    async fn test_member_manage_keys_grant_lifecycle(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let defaulted = create_test_user(&pool, Role::StandardUser).await;
        let restricted = create_test_user(&pool, Role::StandardUser).await;
        let owner_headers = add_auth_headers(&owner);

        let resp = server
            .post("/admin/api/v1/organizations")
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "name": "keyrole-org", "email": "contact@keyrole-org.com" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let org_id = resp.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();
        let org_uuid: uuid::Uuid = org_id.parse().unwrap();
        // Adding a member WITHOUT specifying the toggle defaults to NOT
        // granted (blast-radius minimization — the intern can't self-serve a
        // key until an admin opts them in)…
        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/members"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "user_id": restricted.id, "role": "member" }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        assert_eq!(resp.json::<serde_json::Value>()["can_manage_keys"].as_bool(), Some(false));

        // …while an explicit opt-in grants it.
        let resp = server
            .post(&format!("/admin/api/v1/organizations/{org_id}/members"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "user_id": defaulted.id, "role": "member", "can_manage_keys": true }))
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        assert_eq!(resp.json::<serde_json::Value>()["can_manage_keys"].as_bool(), Some(true));

        // The member list reports the effective capability per member; the
        // owner is implicitly true.
        let resp = server
            .get(&format!("/admin/api/v1/organizations/{org_id}/members"))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        let members = resp.json::<Vec<serde_json::Value>>();
        let by_user = |uid: &str| {
            members
                .iter()
                .find(|m| m["user"]["id"].as_str() == Some(uid))
                .unwrap_or_else(|| panic!("member {uid} missing"))
        };
        assert_eq!(by_user(&owner.id.to_string())["can_manage_keys"].as_bool(), Some(true));
        assert_eq!(by_user(&defaulted.id.to_string())["can_manage_keys"].as_bool(), Some(true));
        assert_eq!(by_user(&restricted.id.to_string())["can_manage_keys"].as_bool(), Some(false));

        // The grant gates key creation for real.
        let restricted_headers = add_auth_headers(&restricted);
        server
            .post(&format!("/admin/api/v1/users/{org_uuid}/api-keys"))
            .add_header(&restricted_headers[0].0, &restricted_headers[0].1)
            .add_header(&restricted_headers[1].0, &restricted_headers[1].1)
            .json(&json!({ "name": "Nope", "purpose": "realtime" }))
            .await
            .assert_status_forbidden();

        // PATCHing the member role can grant it back…
        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}/members/{}", restricted.id))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "role": "member", "can_manage_keys": true }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        assert_eq!(resp.json::<serde_json::Value>()["can_manage_keys"].as_bool(), Some(true));

        server
            .post(&format!("/admin/api/v1/users/{org_uuid}/api-keys"))
            .add_header(&restricted_headers[0].0, &restricted_headers[0].1)
            .add_header(&restricted_headers[1].0, &restricted_headers[1].1)
            .json(&json!({ "name": "Now allowed", "purpose": "realtime" }))
            .await
            .assert_status(axum::http::StatusCode::CREATED);

        // …and revoke it again without touching the base role. A promotion to
        // admin makes the capability implicit regardless of grant rows.
        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}/members/{}", restricted.id))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "role": "member", "can_manage_keys": false }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        assert_eq!(resp.json::<serde_json::Value>()["can_manage_keys"].as_bool(), Some(false));

        let resp = server
            .patch(&format!("/admin/api/v1/organizations/{org_id}/members/{}", restricted.id))
            .add_header(&owner_headers[0].0, &owner_headers[0].1)
            .add_header(&owner_headers[1].0, &owner_headers[1].1)
            .json(&json!({ "role": "admin" }))
            .await;
        resp.assert_status(axum::http::StatusCode::OK);
        assert_eq!(resp.json::<serde_json::Value>()["can_manage_keys"].as_bool(), Some(true));
    }
}
