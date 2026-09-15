//! Database repository for organizations and memberships.
//!
//! Organizations are stored as rows in the `users` table with `user_type = 'organization'`.
//! List/count operations delegate to [`Users`] with a `user_type` filter to avoid duplication.
//! Only `user_organizations`-specific logic (membership CRUD) and org-specific mutations
//! (create/update/delete with different column sets or safety guards) live here.

use crate::api::models::users::Role;
use crate::db::{
    errors::{DbError, Result},
    handlers::users::{UserFilter, Users},
    models::{
        organizations::{
            OrganizationCreateDBRequest, OrganizationMemberDBResponse, OrganizationUpdateDBRequest, PendingOrgEmailChangeDBResponse,
        },
        users::UserDBResponse,
    },
};
use crate::types::{UserId, abbrev_uuid};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Acquire, FromRow, PgConnection};
use tracing::instrument;
use uuid::Uuid;

/// Filter for listing organizations
#[derive(Debug, Clone)]
pub struct OrganizationFilter {
    pub skip: i64,
    pub limit: i64,
    pub search: Option<String>,
}

impl OrganizationFilter {
    pub fn new(skip: i64, limit: i64) -> Self {
        Self { skip, limit, search: None }
    }

    pub fn with_search(mut self, search: String) -> Self {
        self.search = Some(search);
        self
    }

    /// Convert to a [`UserFilter`] targeting organizations.
    fn to_user_filter(&self) -> UserFilter {
        let filter = UserFilter::organizations(self.skip, self.limit);
        if let Some(ref search) = self.search {
            filter.with_search(search.clone())
        } else {
            filter
        }
    }
}

/// Internal row struct for organization membership
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
struct MemberRow {
    pub id: UserId,
    pub user_id: Option<UserId>,
    pub organization_id: UserId,
    pub role: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub invite_email: Option<String>,
    pub invited_by: Option<UserId>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl From<MemberRow> for OrganizationMemberDBResponse {
    fn from(r: MemberRow) -> Self {
        Self {
            id: r.id,
            user_id: r.user_id,
            organization_id: r.organization_id,
            role: r.role,
            status: r.status,
            created_at: r.created_at,
            invite_email: r.invite_email,
            invited_by: r.invited_by,
            expires_at: r.expires_at,
        }
    }
}

pub struct Organizations<'c> {
    db: &'c mut PgConnection,
}

impl<'c> Organizations<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    /// Find an organization by its domain (stored as username).
    /// Returns `None` if no active (non-deleted) organization exists with that domain.
    #[instrument(skip(self), fields(domain = %domain), err)]
    /// The organization a join request for `domain` should go to.
    ///
    /// Organization usernames are `{domain}~{suffix}`, so one company can hold
    /// several workspaces - prod and dev being the obvious pair - while
    /// `users.username` stays unique. The bare `username = $1` arm matches
    /// organizations created before the suffix existed, whose username is the
    /// domain alone.
    ///
    /// `~` is the separator precisely because it can't occur in a domain, so
    /// the prefix match is exact: "acme.com" cannot match an organization for
    /// "acme.com.au", which a plain `LIKE 'acme.com%'` would have done and
    /// routed a join request to the wrong company.
    ///
    /// Several may match; the oldest surviving one wins. That's the workspace a
    /// colleague signing up is most likely to mean, and it stays stable as
    /// later ones come and go.
    ///
    /// A workspace with no live owner or admin is not a candidate at all - see
    /// the `EXISTS` below.
    pub async fn find_by_domain(&mut self, domain: &str) -> Result<Option<UserDBResponse>> {
        // Guard the query itself rather than trusting every call site to have
        // filtered first. `$1` is interpolated into a `LIKE` pattern below, so
        // a `%` or `_` reaching here matches unrelated workspaces, and a
        // single-label name matches the opaque `user~{suffix}` given to every
        // workspace with no domain to claim. Both are reachable wherever a
        // proxy hands over an unvalidated address. See
        // [`crate::auth::utils::is_claimable_domain_shape`].
        if !crate::auth::utils::is_claimable_domain_shape(domain) {
            return Ok(None);
        }

        let row = sqlx::query!(
            r#"
            SELECT id, username, email, display_name, avatar_url, auth_source, created_at, updated_at,
                   is_admin, password_hash, external_user_id, payment_provider_id,
                   is_deleted, is_internal, batch_notifications_enabled, first_batch_email_sent,
                   low_balance_notification_sent, low_balance_threshold,
                   auto_topup_amount, auto_topup_threshold, auto_topup_monthly_limit, user_type, verified, invoicing_enabled, zero_data_retention
            FROM users
            WHERE (username = $1 OR username LIKE $1 || '~%')
              AND user_type = 'organization'
              -- A soft-deleted workspace must never receive join requests:
              -- nobody is left to approve them, so the requester would sit on a
              -- queue no one can see.
              AND is_deleted = false
              -- Same reasoning one level down: a workspace whose owners and
              -- admins have all been deleted is still `is_deleted = false`, but
              -- there is equally nobody home to approve anything.
              --
              -- `Users::delete` no longer creates these - it hands a workspace
              -- to a successor or closes it - but it used to walk away from the
              -- departing user's `user_organizations` rows, which stranded any
              -- workspace whose only owner was deleted: still claiming the
              -- domain, still offered to every colleague who signed up,
              -- unadministrable. This covers those legacy rows, and anything
              -- else that reaches the same state by a route we have not thought
              -- of. `users` inside the subquery is the outer row - the inner one
              -- is aliased.
              AND EXISTS (
                  SELECT 1
                  FROM user_organizations uo
                  JOIN users admin ON admin.id = uo.user_id
                  WHERE uo.organization_id = users.id
                    AND uo.role IN ('owner', 'admin')
                    AND uo.status = 'active'
                    AND admin.is_deleted = false
              )
            ORDER BY created_at ASC
            LIMIT 1
            "#,
            domain
        )
        .fetch_optional(&mut *self.db)
        .await?;

        match row {
            Some(r) => {
                let roles = sqlx::query_scalar!(r#"SELECT role as "role!: Role" FROM user_roles WHERE user_id = $1"#, r.id)
                    .fetch_all(&mut *self.db)
                    .await?;

                Ok(Some(UserDBResponse {
                    id: r.id,
                    username: r.username,
                    email: r.email,
                    display_name: r.display_name,
                    avatar_url: r.avatar_url,
                    created_at: r.created_at,
                    updated_at: r.updated_at,
                    last_login: None,
                    auth_source: r.auth_source,
                    is_admin: r.is_admin,
                    roles,
                    password_hash: r.password_hash,
                    external_user_id: r.external_user_id,
                    payment_provider_id: r.payment_provider_id,
                    batch_notifications_enabled: r.batch_notifications_enabled,
                    first_batch_email_sent: r.first_batch_email_sent,
                    low_balance_notification_sent: r.low_balance_notification_sent,
                    low_balance_threshold: r.low_balance_threshold,
                    auto_topup_amount: r.auto_topup_amount,
                    auto_topup_threshold: r.auto_topup_threshold,
                    auto_topup_monthly_limit: r.auto_topup_monthly_limit,
                    user_type: r.user_type,
                    verified: r.verified,
                    invoicing_enabled: r.invoicing_enabled,
                    zero_data_retention: r.zero_data_retention,
                }))
            }
            None => Ok(None),
        }
    }

    /// Whether this organization admits signups from its claimed domain
    /// automatically, rather than offering them the choice to ask.
    ///
    /// Read separately rather than carried on [`UserDBResponse`] deliberately:
    /// the flag is consulted in exactly two places — the domain match at signup
    /// and the organization's own settings — so threading it through every
    /// query that materialises a user row would cost far more than the extra
    /// statement. Both callers already hold the organization id.
    ///
    /// Returns `false` for a row that doesn't exist or isn't an organization,
    /// which is the safe direction: unknown means "do not admit".
    #[instrument(skip(self), fields(org_id = %abbrev_uuid(&org_id)), err)]
    pub async fn auto_join_enabled(&mut self, org_id: UserId) -> Result<bool> {
        let enabled = sqlx::query_scalar!(
            r#"
            SELECT auto_join_enabled
            FROM users
            WHERE id = $1 AND user_type = 'organization' AND is_deleted = false
            "#,
            org_id,
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(enabled.unwrap_or(false))
    }

    /// Turn domain auto-join on or off. Returns false if there is no such
    /// organization to change.
    #[instrument(skip(self), fields(org_id = %abbrev_uuid(&org_id), enabled), err)]
    pub async fn set_auto_join_enabled(&mut self, org_id: UserId, enabled: bool) -> Result<bool> {
        let result = sqlx::query!(
            r#"
            UPDATE users SET auto_join_enabled = $2, updated_at = NOW()
            WHERE id = $1 AND user_type = 'organization' AND is_deleted = false
            "#,
            org_id,
            enabled,
        )
        .execute(&mut *self.db)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Create a new organization. The creator is automatically added as owner.
    ///
    /// `default_roles` specifies which roles to assign to the org user entity.
    /// These roles determine what API keys scoped to the org can do (e.g. BatchAPIUser
    /// for file/batch operations). StandardUser is always included.
    #[instrument(skip(self, request, default_roles), fields(name = %request.name), err)]
    pub async fn create(&mut self, request: &OrganizationCreateDBRequest, default_roles: &[Role]) -> Result<UserDBResponse> {
        let org_id = Uuid::new_v4();
        let mut tx = self.db.begin().await?;

        // Insert organization as a user row
        let row = sqlx::query!(
            r#"
            INSERT INTO users (id, username, email, display_name, avatar_url, auth_source, user_type, is_admin)
            VALUES ($1, $2, $3, $4, $5, 'organization', 'organization', false)
            RETURNING id, username, email, display_name, avatar_url, auth_source, created_at, updated_at,
                      is_admin, password_hash, external_user_id, payment_provider_id,
                      is_deleted, is_internal, batch_notifications_enabled, first_batch_email_sent,
                      low_balance_notification_sent, low_balance_threshold,
                      auto_topup_amount, auto_topup_threshold, auto_topup_monthly_limit, user_type, verified, invoicing_enabled, zero_data_retention
            "#,
            org_id,
            request.name,
            request.email,
            request.display_name,
            request.avatar_url,
        )
        .fetch_one(&mut *tx)
        .await?;

        // Assign roles to the org user entity so API keys linked to the org have
        // the necessary permissions (e.g. BatchAPIUser for file/batch operations).
        // Ensure StandardUser is always present.
        let mut org_roles: Vec<Role> = default_roles.to_vec();
        if !org_roles.iter().any(|r| matches!(r, Role::StandardUser)) {
            org_roles.push(Role::StandardUser);
        }
        for role in &org_roles {
            sqlx::query!("INSERT INTO user_roles (user_id, role) VALUES ($1, $2)", org_id, role as &Role)
                .execute(&mut *tx)
                .await?;
        }

        // Add creator as owner
        sqlx::query!(
            "INSERT INTO user_organizations (user_id, organization_id, role, status) VALUES ($1, $2, 'owner', 'active')",
            request.created_by,
            org_id
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(UserDBResponse {
            id: row.id,
            username: row.username,
            email: row.email,
            display_name: row.display_name,
            avatar_url: row.avatar_url,
            created_at: row.created_at,
            updated_at: row.updated_at,
            last_login: None,
            auth_source: row.auth_source,
            is_admin: row.is_admin,
            roles: org_roles,
            password_hash: row.password_hash,
            external_user_id: row.external_user_id,
            payment_provider_id: row.payment_provider_id,
            batch_notifications_enabled: row.batch_notifications_enabled,
            first_batch_email_sent: row.first_batch_email_sent,
            low_balance_notification_sent: row.low_balance_notification_sent,
            low_balance_threshold: row.low_balance_threshold,
            auto_topup_amount: row.auto_topup_amount,
            auto_topup_threshold: row.auto_topup_threshold,
            auto_topup_monthly_limit: row.auto_topup_monthly_limit,
            user_type: row.user_type,
            verified: row.verified,
            invoicing_enabled: row.invoicing_enabled,
            zero_data_retention: row.zero_data_retention,
        })
    }

    /// List organizations with pagination and optional search.
    /// Delegates to [`Users::list`] with `user_type = 'organization'`.
    #[instrument(skip(self, filter), fields(limit = filter.limit, skip = filter.skip), err)]
    pub async fn list(&mut self, filter: &OrganizationFilter) -> Result<Vec<UserDBResponse>> {
        use crate::db::handlers::repository::Repository;
        Users::new(self.db).list(&filter.to_user_filter()).await
    }

    /// Count organizations matching the filter.
    /// Delegates to [`Users::count`] with `user_type = 'organization'`.
    #[instrument(skip(self, filter), fields(search = filter.search), err)]
    pub async fn count(&mut self, filter: &OrganizationFilter) -> Result<i64> {
        Users::new(self.db).count(&filter.to_user_filter()).await
    }

    /// Update an organization's details
    #[instrument(skip(self, request), fields(org_id = %abbrev_uuid(&id)), err)]
    pub async fn update(&mut self, id: UserId, request: &OrganizationUpdateDBRequest) -> Result<UserDBResponse> {
        let row = sqlx::query!(
            r#"
            UPDATE users SET
                display_name = COALESCE($2, display_name),
                avatar_url = COALESCE($3, avatar_url),
                email = COALESCE($4, email),
                batch_notifications_enabled = COALESCE($5, batch_notifications_enabled),
                low_balance_threshold = CASE
                    WHEN $6::boolean THEN $7
                    ELSE low_balance_threshold
                END,
                low_balance_notification_sent = CASE
                    WHEN $6::boolean THEN false
                    ELSE low_balance_notification_sent
                END,
                zero_data_retention = COALESCE($8, zero_data_retention),
                updated_at = NOW()
            WHERE id = $1 AND user_type = 'organization' AND is_deleted = false
            RETURNING id, username, email, display_name, avatar_url, auth_source, created_at, updated_at,
                      is_admin, password_hash, external_user_id, payment_provider_id,
                      batch_notifications_enabled, first_batch_email_sent,
                      low_balance_notification_sent, low_balance_threshold,
                      auto_topup_amount, auto_topup_threshold, auto_topup_monthly_limit, user_type, verified, invoicing_enabled, zero_data_retention
            "#,
            id,
            request.display_name,
            request.avatar_url,
            request.email,
            request.batch_notifications_enabled,
            request.low_balance_threshold.is_some() as bool,
            request.low_balance_threshold.flatten(),
            request.zero_data_retention,
        )
        .fetch_optional(&mut *self.db)
        .await?
        .ok_or(DbError::NotFound)?;

        let roles: Vec<Role> = sqlx::query_scalar!(r#"SELECT role as "role: Role" FROM user_roles WHERE user_id = $1"#, id)
            .fetch_all(&mut *self.db)
            .await?;

        Ok(UserDBResponse {
            id: row.id,
            username: row.username,
            email: row.email,
            display_name: row.display_name,
            avatar_url: row.avatar_url,
            created_at: row.created_at,
            updated_at: row.updated_at,
            last_login: None,
            auth_source: row.auth_source,
            is_admin: row.is_admin,
            roles,
            password_hash: row.password_hash,
            external_user_id: row.external_user_id,
            payment_provider_id: row.payment_provider_id,
            batch_notifications_enabled: row.batch_notifications_enabled,
            first_batch_email_sent: row.first_batch_email_sent,
            low_balance_notification_sent: row.low_balance_notification_sent,
            low_balance_threshold: row.low_balance_threshold,
            auto_topup_amount: row.auto_topup_amount,
            auto_topup_threshold: row.auto_topup_threshold,
            auto_topup_monthly_limit: row.auto_topup_monthly_limit,
            user_type: row.user_type,
            verified: row.verified,
            invoicing_enabled: row.invoicing_enabled,
            zero_data_retention: row.zero_data_retention,
        })
    }

    /// Soft-delete an organization
    #[instrument(skip(self), fields(org_id = %abbrev_uuid(&id)), err)]
    pub async fn delete(&mut self, id: UserId) -> Result<bool> {
        let scrubbed_email = format!("deleted-{}@deleted.local", id);
        let scrubbed_username = format!("deleted-{}", id);

        let result = sqlx::query!(
            r#"
            UPDATE users
            SET
                email = $1,
                username = $2,
                display_name = NULL,
                avatar_url = NULL,
                is_deleted = true,
                updated_at = NOW()
            WHERE id = $3 AND user_type = 'organization' AND is_deleted = false
            "#,
            scrubbed_email,
            scrubbed_username,
            id
        )
        .execute(&mut *self.db)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// The organizations this user is an active member of, whatever their role.
    ///
    /// Read before deleting the account, because `Users::delete` clears the
    /// membership rows that identify them: afterwards there is nothing left to
    /// join on. Deliberately not restricted to the ones they own - a member
    /// promoted to owner between the read and the delete can still be closed by
    /// that transaction, and a caller narrowing to owners here would never learn
    /// it needed cleaning up.
    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn list_member_organization_ids(&mut self, user_id: UserId) -> Result<Vec<UserId>> {
        Ok(sqlx::query_scalar!(
            r#"SELECT organization_id FROM user_organizations WHERE user_id = $1 AND status = 'active'"#,
            user_id
        )
        .fetch_all(&mut *self.db)
        .await?)
    }

    /// Add a member to an organization (active status)
    #[instrument(skip(self), fields(org_id = %abbrev_uuid(&org_id), user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn add_member(&mut self, org_id: UserId, user_id: UserId, role: &str) -> Result<OrganizationMemberDBResponse> {
        let row = sqlx::query_as!(
            MemberRow,
            r#"
            INSERT INTO user_organizations (user_id, organization_id, role, status)
            VALUES ($1, $2, $3, 'active')
            RETURNING id, user_id, organization_id, role, status, created_at,
                      invite_email, invited_by, expires_at
            "#,
            user_id,
            org_id,
            role,
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(row.into())
    }

    /// Grant or revoke the additive 'manage_keys' org role on a membership
    /// row (organization_member_roles). Only meaningful for base role
    /// 'member' — owners/admins hold the capability implicitly.
    #[instrument(skip(self), fields(membership_id = %abbrev_uuid(&membership_id)), err)]
    pub async fn set_membership_manage_keys(&mut self, membership_id: Uuid, granted: bool) -> Result<()> {
        if granted {
            sqlx::query!(
                "INSERT INTO organization_member_roles (user_organization_id, role) VALUES ($1, 'manage_keys') ON CONFLICT DO NOTHING",
                membership_id
            )
            .execute(&mut *self.db)
            .await?;
        } else {
            sqlx::query!(
                "DELETE FROM organization_member_roles WHERE user_organization_id = $1 AND role = 'manage_keys'",
                membership_id
            )
            .execute(&mut *self.db)
            .await?;
        }
        Ok(())
    }

    /// Does this membership row carry the 'manage_keys' org role?
    #[instrument(skip(self), fields(membership_id = %abbrev_uuid(&membership_id)), err)]
    pub async fn membership_has_manage_keys(&mut self, membership_id: Uuid) -> Result<bool> {
        let exists = sqlx::query_scalar!(
            r#"SELECT EXISTS(SELECT 1 FROM organization_member_roles WHERE user_organization_id = $1 AND role = 'manage_keys') AS "exists!""#,
            membership_id
        )
        .fetch_one(&mut *self.db)
        .await?;
        Ok(exists)
    }

    /// Membership row ids in this org that carry the 'manage_keys' role
    /// (one query for member-list rendering).
    #[instrument(skip(self), fields(org_id = %abbrev_uuid(&org_id)), err)]
    pub async fn list_manage_keys_membership_ids(&mut self, org_id: UserId) -> Result<std::collections::HashSet<Uuid>> {
        let ids = sqlx::query_scalar!(
            r#"
            SELECT omr.user_organization_id
            FROM organization_member_roles omr
            JOIN user_organizations uo ON uo.id = omr.user_organization_id
            WHERE uo.organization_id = $1 AND omr.role = 'manage_keys'
            "#,
            org_id
        )
        .fetch_all(&mut *self.db)
        .await?;
        Ok(ids.into_iter().collect())
    }

    /// Org ids where this user's membership carries the 'manage_keys' role
    /// (one query for the session's org-context capabilities).
    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn user_manage_keys_org_ids(&mut self, user_id: UserId) -> Result<std::collections::HashSet<UserId>> {
        let ids = sqlx::query_scalar!(
            r#"
            SELECT uo.organization_id
            FROM user_organizations uo
            JOIN organization_member_roles omr ON omr.user_organization_id = uo.id
            WHERE uo.user_id = $1 AND omr.role = 'manage_keys'
            "#,
            user_id
        )
        .fetch_all(&mut *self.db)
        .await?;
        Ok(ids.into_iter().collect())
    }

    /// Remove a member from an organization
    #[instrument(skip(self), fields(org_id = %abbrev_uuid(&org_id), user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn remove_member(&mut self, org_id: UserId, user_id: UserId) -> Result<bool> {
        let result = sqlx::query!(
            "DELETE FROM user_organizations WHERE user_id = $1 AND organization_id = $2",
            user_id,
            org_id
        )
        .execute(&mut *self.db)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Update a member's role in an organization
    #[instrument(skip(self), fields(org_id = %abbrev_uuid(&org_id), user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn update_member_role(&mut self, org_id: UserId, user_id: UserId, role: &str) -> Result<OrganizationMemberDBResponse> {
        let row = sqlx::query_as!(
            MemberRow,
            r#"
            UPDATE user_organizations SET role = $3
            WHERE user_id = $1 AND organization_id = $2 AND status = 'active'
            RETURNING id, user_id, organization_id, role, status, created_at,
                      invite_email, invited_by, expires_at
            "#,
            user_id,
            org_id,
            role,
        )
        .fetch_optional(&mut *self.db)
        .await?
        .ok_or(DbError::NotFound)?;

        Ok(row.into())
    }

    /// List members of an organization (includes both active and pending)
    #[instrument(skip(self), fields(org_id = %abbrev_uuid(&org_id)), err)]
    pub async fn list_members(&mut self, org_id: UserId) -> Result<Vec<OrganizationMemberDBResponse>> {
        let rows = sqlx::query_as!(
            MemberRow,
            r#"
            SELECT uo.id, uo.user_id, uo.organization_id, uo.role, uo.status,
                   uo.created_at, uo.invite_email, uo.invited_by, uo.expires_at
            FROM user_organizations uo
            LEFT JOIN users u ON u.id = uo.user_id
            WHERE uo.organization_id = $1
              AND uo.status <> 'requested'
              AND (uo.user_id IS NULL OR u.is_deleted = false)
            ORDER BY uo.status ASC, uo.created_at ASC
            "#,
            org_id
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Record a request to join an organization, awaiting owner/admin approval.
    ///
    /// The mirror of an invite: same table, same lifecycle, opposite direction.
    ///
    /// Idempotent. `UNIQUE (user_id, organization_id)` means a second request
    /// while one is outstanding — or while the user is already a member —
    /// collides rather than duplicating, and the whole point of this call is a
    /// button a user can press twice. On collision the **existing** row comes
    /// back rather than an error, so the caller reads `status` to tell the
    /// cases apart: `requested` is "you already asked", `active` is "you are
    /// already in", `pending` is "you were invited and haven't accepted".
    /// Every one of those is a state the caller must describe, not a failure.
    #[instrument(skip(self), fields(org_id = %abbrev_uuid(&org_id), user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn create_join_request(&mut self, org_id: UserId, user_id: UserId) -> Result<OrganizationMemberDBResponse> {
        let inserted = sqlx::query_as!(
            MemberRow,
            r#"
            INSERT INTO user_organizations (user_id, organization_id, role, status)
            VALUES ($1, $2, 'member', 'requested')
            ON CONFLICT (user_id, organization_id) DO NOTHING
            RETURNING id, user_id, organization_id, role, status, created_at,
                      invite_email, invited_by, expires_at
            "#,
            user_id,
            org_id,
        )
        .fetch_optional(&mut *self.db)
        .await?;

        if let Some(row) = inserted {
            return Ok(row.into());
        }

        let existing = sqlx::query_as!(
            MemberRow,
            r#"
            SELECT id, user_id, organization_id, role, status, created_at,
                   invite_email, invited_by, expires_at
            FROM user_organizations
            WHERE user_id = $1 AND organization_id = $2
            "#,
            user_id,
            org_id,
        )
        .fetch_optional(&mut *self.db)
        .await?
        .ok_or(DbError::NotFound)?;

        Ok(existing.into())
    }

    /// A user's own outstanding join requests, oldest first.
    ///
    /// The mirror of [`Self::list_join_requests`], which answers "who wants
    /// into my organization" for an owner. This one answers "what have I asked
    /// to join" for the requester, who is by definition not a member yet and so
    /// cannot read the organization-scoped queue.
    ///
    /// Deleted organizations are excluded for the same reason they don't match
    /// on signup: nobody is left to approve the request, so surfacing it would
    /// promise an outcome that can never arrive.
    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn list_user_join_requests(&mut self, user_id: UserId) -> Result<Vec<OrganizationMemberDBResponse>> {
        let rows = sqlx::query_as!(
            MemberRow,
            r#"
            SELECT uo.id, uo.user_id, uo.organization_id, uo.role, uo.status,
                   uo.created_at, uo.invite_email, uo.invited_by, uo.expires_at
            FROM user_organizations uo
            INNER JOIN users o ON o.id = uo.organization_id
            WHERE uo.user_id = $1
              AND uo.status = 'requested'
              AND o.is_deleted = false
            ORDER BY uo.created_at ASC
            "#,
            user_id
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Outstanding join requests for an organization, oldest first.
    #[instrument(skip(self), fields(org_id = %abbrev_uuid(&org_id)), err)]
    pub async fn list_join_requests(&mut self, org_id: UserId) -> Result<Vec<OrganizationMemberDBResponse>> {
        let rows = sqlx::query_as!(
            MemberRow,
            r#"
            SELECT uo.id, uo.user_id, uo.organization_id, uo.role, uo.status,
                   uo.created_at, uo.invite_email, uo.invited_by, uo.expires_at
            FROM user_organizations uo
            INNER JOIN users u ON u.id = uo.user_id
            WHERE uo.organization_id = $1
              AND uo.status = 'requested'
              AND u.is_deleted = false
            ORDER BY uo.created_at ASC
            "#,
            org_id
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Approve a join request: the requester becomes an active member.
    ///
    /// Scoped to `org_id` as well as the request id so a caller who can manage
    /// one organization can't approve a request belonging to another. Returns
    /// `None` if the request no longer exists or was already decided, which
    /// makes concurrent approvals a no-op rather than a double-add.
    ///
    /// The approved user's id comes back so the caller can tell them — only
    /// the winner of a concurrent approval gets a `Some`, so the requester is
    /// mailed once rather than once per racing admin.
    #[instrument(skip(self), fields(org_id = %abbrev_uuid(&org_id), request_id = %abbrev_uuid(&request_id)), err)]
    pub async fn approve_join_request(&mut self, org_id: UserId, request_id: Uuid, role: &str) -> Result<Option<UserId>> {
        let approved = sqlx::query_scalar!(
            r#"
            UPDATE user_organizations
            SET status = 'active', role = $3
            WHERE id = $1 AND organization_id = $2 AND status = 'requested'
            RETURNING user_id
            "#,
            request_id,
            org_id,
            role,
        )
        .fetch_optional(&mut *self.db)
        .await?;

        // `user_id` is nullable on the table (an invite by address has no
        // account behind it yet), but a `requested` row is always filed by a
        // signed-in user, so the inner `None` is unreachable in practice.
        Ok(approved.flatten())
    }

    /// Email addresses of everyone who can act on a join request: the active
    /// owners and admins.
    ///
    /// Deleted accounts are filtered out here rather than at the send site —
    /// a soft-deleted user keeps their row in `users`, and mailing them a
    /// workspace's join requests would be a live notification to a closed
    /// account.
    #[instrument(skip(self), fields(org_id = %abbrev_uuid(&org_id)), err)]
    pub async fn list_admin_emails(&mut self, org_id: UserId) -> Result<Vec<String>> {
        let emails = sqlx::query_scalar!(
            r#"
            SELECT u.email
            FROM user_organizations uo
            JOIN users u ON u.id = uo.user_id
            WHERE uo.organization_id = $1
              AND uo.status = 'active'
              AND uo.role IN ('owner', 'admin')
              AND u.is_deleted = false
            ORDER BY u.email
            "#,
            org_id,
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(emails)
    }

    /// Decline a join request, removing the row.
    ///
    /// Deleted rather than kept in a `declined` state so the user can ask again
    /// later - a permanent tombstone would silently block them forever via the
    /// unique constraint.
    #[instrument(skip(self), fields(org_id = %abbrev_uuid(&org_id), request_id = %abbrev_uuid(&request_id)), err)]
    pub async fn decline_join_request(&mut self, org_id: UserId, request_id: Uuid) -> Result<bool> {
        let result = sqlx::query!(
            "DELETE FROM user_organizations WHERE id = $1 AND organization_id = $2 AND status = 'requested'",
            request_id,
            org_id,
        )
        .execute(&mut *self.db)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// List organizations a user belongs to (active memberships only)
    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn list_user_organizations(&mut self, user_id: UserId) -> Result<Vec<OrganizationMemberDBResponse>> {
        let rows = sqlx::query_as!(
            MemberRow,
            r#"
            SELECT uo.id, uo.user_id, uo.organization_id, uo.role, uo.status,
                   uo.created_at, uo.invite_email, uo.invited_by, uo.expires_at
            FROM user_organizations uo
            INNER JOIN users u ON u.id = uo.organization_id
            WHERE uo.user_id = $1 AND uo.status = 'active' AND u.is_deleted = false
            ORDER BY uo.created_at ASC
            "#,
            user_id
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Count the number of active organizations a user belongs to.
    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn count_user_organizations(&mut self, user_id: UserId) -> Result<i64> {
        let count = sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) as "count!"
            FROM user_organizations uo
            INNER JOIN users u ON u.id = uo.organization_id
            WHERE uo.user_id = $1 AND uo.status = 'active' AND u.is_deleted = false
            "#,
            user_id
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(count)
    }

    /// Get a user's role in an organization (active memberships only, None if not a member)
    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id), org_id = %abbrev_uuid(&org_id)), err)]
    pub async fn get_user_org_role(&mut self, user_id: UserId, org_id: UserId) -> Result<Option<String>> {
        let row = sqlx::query_scalar!(
            "SELECT role FROM user_organizations WHERE user_id = $1 AND organization_id = $2 AND status = 'active'",
            user_id,
            org_id
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(row)
    }

    /// Create a pending invite
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip(self, token_hash), fields(org_id = %abbrev_uuid(&org_id), invite_email = %invite_email), err)]
    pub async fn create_invite(
        &mut self,
        org_id: UserId,
        user_id: Option<UserId>,
        invite_email: &str,
        role: &str,
        invited_by: UserId,
        token_hash: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<OrganizationMemberDBResponse> {
        let row = sqlx::query_as!(
            MemberRow,
            r#"
            INSERT INTO user_organizations (user_id, organization_id, role, status, invite_email, invited_by, invite_token_hash, expires_at)
            VALUES ($1, $2, $3, 'pending', $4, $5, $6, $7)
            RETURNING id, user_id, organization_id, role, status, created_at,
                      invite_email, invited_by, expires_at
            "#,
            user_id,
            org_id,
            role,
            invite_email,
            invited_by,
            token_hash,
            expires_at,
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(row.into())
    }

    /// The pending invite addressed to `email`, if one is outstanding.
    ///
    /// The token-hash lookup above answers "who is this link for"; this one
    /// answers "has anyone invited *me*", which is the question onboarding has
    /// to ask. A user who signed up without ever opening the invitation mail —
    /// or who lost it — has no token, and the stored hash is one-way, so the
    /// link cannot be reconstructed to hand back to them.
    ///
    /// Matched on the address rather than `user_id` because an invite raised
    /// before the invitee had an account carries no `user_id` at all; that
    /// column is only filled in on acceptance. Compared case-insensitively for
    /// the same reason the token path does: mailbox addresses are routinely
    /// capitalised differently by the sender and the identity provider.
    ///
    /// Expired invites and invites into deleted organizations are excluded:
    /// both would offer the user a door that cannot open.
    #[instrument(skip(self), err)]
    pub async fn find_pending_invite_for_email(&mut self, email: &str) -> Result<Option<OrganizationMemberDBResponse>> {
        let row = sqlx::query_as!(
            MemberRow,
            r#"
            SELECT uo.id, uo.user_id, uo.organization_id, uo.role, uo.status,
                   uo.created_at, uo.invite_email, uo.invited_by, uo.expires_at
            FROM user_organizations uo
            INNER JOIN users o ON o.id = uo.organization_id
            WHERE LOWER(uo.invite_email) = LOWER($1)
              AND uo.status = 'pending'
              AND o.is_deleted = false
              AND (uo.expires_at IS NULL OR uo.expires_at > NOW())
            ORDER BY uo.created_at ASC
            LIMIT 1
            "#,
            email,
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(row.map(Into::into))
    }

    /// A pending invite by row id, scoped to its organization.
    ///
    /// The by-id counterpart to [`Self::find_invite_by_token_hash`], for the
    /// invitee who reached the invite through onboarding rather than through
    /// the emailed link. Possession of the token is what proves the mailbox on
    /// the link path; here the caller's own authenticated address does, so
    /// callers MUST compare `invite_email` against it before acting.
    #[instrument(skip(self), fields(org_id = %abbrev_uuid(&org_id), invite_id = %abbrev_uuid(&invite_id)), err)]
    pub async fn find_invite_by_id(&mut self, org_id: UserId, invite_id: Uuid) -> Result<Option<OrganizationMemberDBResponse>> {
        let row = sqlx::query_as!(
            MemberRow,
            r#"
            SELECT id, user_id, organization_id, role, status, created_at,
                   invite_email, invited_by, expires_at
            FROM user_organizations
            WHERE id = $1 AND organization_id = $2 AND status = 'pending'
            "#,
            invite_id,
            org_id,
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(row.map(Into::into))
    }

    /// Find a pending invite by token hash
    #[instrument(skip(self, token_hash), err)]
    pub async fn find_invite_by_token_hash(&mut self, token_hash: &str) -> Result<Option<OrganizationMemberDBResponse>> {
        let row = sqlx::query_as!(
            MemberRow,
            r#"
            SELECT id, user_id, organization_id, role, status, created_at,
                   invite_email, invited_by, expires_at
            FROM user_organizations
            WHERE invite_token_hash = $1 AND status = 'pending'
            "#,
            token_hash,
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(row.map(Into::into))
    }

    /// Accept an invite: set status to active, set user_id, clear token
    ///
    /// Errors here are expected client conditions (UniqueViolation means already a member,
    /// a 409; NotFound means invite not pending, a 404), so log at warn rather than error. A
    /// genuine server error still pages via the 5xx route metric regardless of this log level.
    #[instrument(skip(self), fields(invite_id = %abbrev_uuid(&invite_id), user_id = %abbrev_uuid(&user_id)), err(level = "warn"))]
    pub async fn accept_invite(&mut self, invite_id: UserId, user_id: UserId) -> Result<OrganizationMemberDBResponse> {
        let row = sqlx::query_as!(
            MemberRow,
            r#"
            UPDATE user_organizations
            SET status = 'active', user_id = $2, invite_token_hash = NULL
            WHERE id = $1 AND status = 'pending'
            RETURNING id, user_id, organization_id, role, status, created_at,
                      invite_email, invited_by, expires_at
            "#,
            invite_id,
            user_id,
        )
        .fetch_optional(&mut *self.db)
        .await?
        .ok_or(DbError::NotFound)?;

        Ok(row.into())
    }

    /// Re-issue a pending invite: new token, fresh expiry.
    ///
    /// The original token is only stored as a hash, so it can't be recovered to
    /// re-send - a resend necessarily mints a new one. That also invalidates
    /// the old link, which is the behaviour you want if the first was sent to
    /// the wrong place or has leaked.
    ///
    /// Returns the invite's email and role so the caller can send the mail,
    /// or None if there's no pending invite by that id in this organization.
    #[instrument(skip(self, token_hash), fields(org_id = %abbrev_uuid(&org_id), invite_id = %abbrev_uuid(&invite_id)), err)]
    pub async fn refresh_invite_token(
        &mut self,
        org_id: UserId,
        invite_id: Uuid,
        token_hash: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<Option<(String, String)>> {
        let row = sqlx::query!(
            r#"
            UPDATE user_organizations
            SET invite_token_hash = $3, expires_at = $4
            WHERE id = $1 AND organization_id = $2 AND status = 'pending'
            RETURNING invite_email, role
            "#,
            invite_id,
            org_id,
            token_hash,
            expires_at,
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(row.and_then(|r| r.invite_email.map(|email| (email, r.role))))
    }

    /// Cancel (delete) a pending invite by row ID
    #[instrument(skip(self), fields(org_id = %abbrev_uuid(&org_id), invite_id = %abbrev_uuid(&invite_id)), err)]
    pub async fn cancel_invite(&mut self, org_id: UserId, invite_id: UserId) -> Result<bool> {
        let result = sqlx::query!(
            "DELETE FROM user_organizations WHERE id = $1 AND organization_id = $2 AND status = 'pending'",
            invite_id,
            org_id
        )
        .execute(&mut *self.db)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Look up the current pending email-change row for an org, if any.
    ///
    /// Intended for audit-logging just before [`Self::upsert_pending_email_change`]
    /// supersedes the row — callers can capture the prior `requested_by` and
    /// `new_email` for the audit trail, since the UPSERT will overwrite them.
    #[instrument(skip(self), fields(org_id = %abbrev_uuid(&org_id)), err)]
    pub async fn find_pending_email_change_for_org(&mut self, org_id: UserId) -> Result<Option<PendingOrgEmailChangeDBResponse>> {
        let row = sqlx::query!(
            r#"
            SELECT id, organization_id, new_email, requested_by,
                   new_email_confirmed_at, old_email_confirmed_at,
                   created_at, expires_at
            FROM pending_org_email_changes
            WHERE organization_id = $1
            "#,
            org_id,
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(row.map(|r| PendingOrgEmailChangeDBResponse {
            id: r.id,
            organization_id: r.organization_id,
            new_email: r.new_email,
            requested_by: r.requested_by,
            new_email_confirmed_at: r.new_email_confirmed_at,
            old_email_confirmed_at: r.old_email_confirmed_at,
            created_at: r.created_at,
            expires_at: r.expires_at,
        }))
    }

    /// Atomically insert or replace the pending email-change row for an org.
    ///
    /// The `pending_org_email_changes.organization_id` column has a UNIQUE
    /// constraint, so `ON CONFLICT` ensures that at most one pending change
    /// exists per org and that older verification tokens are invalidated the
    /// instant a new one is accepted — without a read-then-write race
    /// window. The `created_at` reset and the cleared `*_confirmed_at`
    /// columns guarantee that a superseded change never finalizes using a
    /// confirmation that was recorded against the previous (now-replaced)
    /// row.
    #[instrument(
        skip(self, new_email_token_hash, old_email_token_hash),
        fields(org_id = %abbrev_uuid(&org_id)),
        err,
    )]
    pub async fn upsert_pending_email_change(
        &mut self,
        org_id: UserId,
        new_email: &str,
        requested_by: UserId,
        new_email_token_hash: &str,
        old_email_token_hash: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<PendingOrgEmailChangeDBResponse> {
        let row = sqlx::query!(
            r#"
            INSERT INTO pending_org_email_changes
                (organization_id, new_email, requested_by, new_email_token_hash, old_email_token_hash, expires_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (organization_id) DO UPDATE SET
                new_email = EXCLUDED.new_email,
                requested_by = EXCLUDED.requested_by,
                new_email_token_hash = EXCLUDED.new_email_token_hash,
                old_email_token_hash = EXCLUDED.old_email_token_hash,
                new_email_confirmed_at = NULL,
                old_email_confirmed_at = NULL,
                expires_at = EXCLUDED.expires_at,
                created_at = NOW()
            RETURNING id, organization_id, new_email, requested_by,
                      new_email_confirmed_at, old_email_confirmed_at,
                      created_at, expires_at
            "#,
            org_id,
            new_email,
            requested_by,
            new_email_token_hash,
            old_email_token_hash,
            expires_at,
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(PendingOrgEmailChangeDBResponse {
            id: row.id,
            organization_id: row.organization_id,
            new_email: row.new_email,
            requested_by: row.requested_by,
            new_email_confirmed_at: row.new_email_confirmed_at,
            old_email_confirmed_at: row.old_email_confirmed_at,
            created_at: row.created_at,
            expires_at: row.expires_at,
        })
    }

    /// Mark the new-email side of the pending change as confirmed.
    ///
    /// Atomic update with the same hardening as the confirm-side lookups:
    /// joins `users` and requires `is_deleted = false`, blocks
    /// already-confirmed and expired tokens. Returns the freshly-updated row
    /// (including both `*_confirmed_at` timestamps so the caller can check
    /// if the change is now ready to apply), or `None` if no row matched.
    #[instrument(skip(self, token_hash), err)]
    pub async fn confirm_new_email_side(&mut self, token_hash: &str) -> Result<Option<PendingOrgEmailChangeDBResponse>> {
        let row = sqlx::query!(
            r#"
            UPDATE pending_org_email_changes p
            SET new_email_confirmed_at = NOW()
            FROM users u
            WHERE p.new_email_token_hash = $1
              AND p.new_email_confirmed_at IS NULL
              AND p.expires_at > NOW()
              AND p.organization_id = u.id
              AND u.is_deleted = false
            RETURNING p.id, p.organization_id, p.new_email, p.requested_by,
                      p.new_email_confirmed_at, p.old_email_confirmed_at,
                      p.created_at, p.expires_at
            "#,
            token_hash,
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(row.map(|r| PendingOrgEmailChangeDBResponse {
            id: r.id,
            organization_id: r.organization_id,
            new_email: r.new_email,
            requested_by: r.requested_by,
            new_email_confirmed_at: r.new_email_confirmed_at,
            old_email_confirmed_at: r.old_email_confirmed_at,
            created_at: r.created_at,
            expires_at: r.expires_at,
        }))
    }

    /// Mark the old-email side of the pending change as confirmed.
    /// Symmetric to [`Self::confirm_new_email_side`].
    #[instrument(skip(self, token_hash), err)]
    pub async fn confirm_old_email_side(&mut self, token_hash: &str) -> Result<Option<PendingOrgEmailChangeDBResponse>> {
        let row = sqlx::query!(
            r#"
            UPDATE pending_org_email_changes p
            SET old_email_confirmed_at = NOW()
            FROM users u
            WHERE p.old_email_token_hash = $1
              AND p.old_email_confirmed_at IS NULL
              AND p.expires_at > NOW()
              AND p.organization_id = u.id
              AND u.is_deleted = false
            RETURNING p.id, p.organization_id, p.new_email, p.requested_by,
                      p.new_email_confirmed_at, p.old_email_confirmed_at,
                      p.created_at, p.expires_at
            "#,
            token_hash,
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(row.map(|r| PendingOrgEmailChangeDBResponse {
            id: r.id,
            organization_id: r.organization_id,
            new_email: r.new_email,
            requested_by: r.requested_by,
            new_email_confirmed_at: r.new_email_confirmed_at,
            old_email_confirmed_at: r.old_email_confirmed_at,
            created_at: r.created_at,
            expires_at: r.expires_at,
        }))
    }

    /// Delete a pending email change row by id. Used by the confirm
    /// handler once both sides are confirmed and the change has been
    /// applied to `users.email` — both operations must run inside the
    /// same transaction so a failure in either rolls back the other.
    #[instrument(skip(self), err)]
    pub async fn delete_pending_email_change(&mut self, id: Uuid) -> Result<bool> {
        let result = sqlx::query!("DELETE FROM pending_org_email_changes WHERE id = $1", id)
            .execute(&mut *self.db)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::users::{Role, UserCreate};
    use crate::db::handlers::repository::Repository;
    use crate::db::handlers::users::Users;
    use crate::db::models::organizations::{OrganizationCreateDBRequest, OrganizationUpdateDBRequest};
    use crate::db::models::users::UserCreateDBRequest;
    use sqlx::PgPool;

    /// Default roles used in tests — mirrors the default config.yaml
    const TEST_DEFAULT_ROLES: &[Role] = &[Role::StandardUser, Role::BatchAPIUser];

    /// Helper: create a regular individual user and return their id
    async fn create_individual(pool: &PgPool, username: &str, email: &str) -> UserId {
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Users::new(&mut conn);
        let user = repo
            .create(&UserCreateDBRequest::from(UserCreate {
                username: username.to_string(),
                email: email.to_string(),
                display_name: Some(format!("User {username}")),
                avatar_url: None,
                roles: vec![Role::StandardUser],
            }))
            .await
            .unwrap();
        user.id
    }

    // ── CRUD ──────────────────────────────────────────────────────────────

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_organization(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme-corp".to_string(),
                    email: "billing@acme.example.com".to_string(),
                    display_name: Some("Acme Corporation".to_string()),
                    avatar_url: None,
                    created_by: creator,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();

        assert_eq!(org.username, "acme-corp");
        assert_eq!(org.email, "billing@acme.example.com");
        assert_eq!(org.display_name.as_deref(), Some("Acme Corporation"));
        assert_eq!(org.user_type, "organization");
        assert_eq!(org.auth_source, "organization");
        assert!(!org.is_admin);

        // Verify roles are persisted in user_roles (org gets the configured default roles)
        let mut persisted_roles: Vec<String> =
            sqlx::query_scalar!(r#"SELECT role::text as "role!" FROM user_roles WHERE user_id = $1"#, org.id)
                .fetch_all(&pool)
                .await
                .unwrap();
        persisted_roles.sort();
        assert_eq!(persisted_roles, vec!["BATCHAPIUSER", "STANDARDUSER"]);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_organization_adds_creator_as_owner(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme-corp".to_string(),
                    email: "billing@acme.example.com".to_string(),
                    display_name: None,
                    avatar_url: None,
                    created_by: creator,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();

        // Creator should be an owner
        let role = orgs.get_user_org_role(creator, org.id).await.unwrap();
        assert_eq!(role, Some("owner".to_string()));

        // Should appear in member list
        let members = orgs.list_members(org.id).await.unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].user_id, Some(creator));
        assert_eq!(members[0].role, "owner");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_organizations(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        orgs.create(
            &OrganizationCreateDBRequest {
                name: "acme-corp".to_string(),
                email: "billing@acme.example.com".to_string(),
                display_name: Some("Acme Corporation".to_string()),
                avatar_url: None,
                created_by: creator,
            },
            TEST_DEFAULT_ROLES,
        )
        .await
        .unwrap();

        orgs.create(
            &OrganizationCreateDBRequest {
                name: "globex-inc".to_string(),
                email: "info@globex.example.com".to_string(),
                display_name: Some("Globex Inc".to_string()),
                avatar_url: None,
                created_by: creator,
            },
            TEST_DEFAULT_ROLES,
        )
        .await
        .unwrap();

        let filter = OrganizationFilter::new(0, 100);
        let list = orgs.list(&filter).await.unwrap();
        assert_eq!(list.len(), 2);
        for o in &list {
            assert_eq!(o.user_type, "organization");
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_organizations_with_search(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        orgs.create(
            &OrganizationCreateDBRequest {
                name: "acme-corp".to_string(),
                email: "billing@acme.example.com".to_string(),
                display_name: Some("Acme Corporation".to_string()),
                avatar_url: None,
                created_by: creator,
            },
            TEST_DEFAULT_ROLES,
        )
        .await
        .unwrap();

        orgs.create(
            &OrganizationCreateDBRequest {
                name: "globex-inc".to_string(),
                email: "info@globex.example.com".to_string(),
                display_name: Some("Globex Inc".to_string()),
                avatar_url: None,
                created_by: creator,
            },
            TEST_DEFAULT_ROLES,
        )
        .await
        .unwrap();

        // Search by display name
        let filter = OrganizationFilter::new(0, 100).with_search("acme".to_string());
        let list = orgs.list(&filter).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].username, "acme-corp");

        // Search by username
        let filter = OrganizationFilter::new(0, 100).with_search("globex".to_string());
        let list = orgs.list(&filter).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].username, "globex-inc");

        // Search with no match
        let filter = OrganizationFilter::new(0, 100).with_search("nonexistent".to_string());
        let list = orgs.list(&filter).await.unwrap();
        assert!(list.is_empty());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_count_organizations(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        let filter = OrganizationFilter::new(0, 100);
        assert_eq!(orgs.count(&filter).await.unwrap(), 0);

        orgs.create(
            &OrganizationCreateDBRequest {
                name: "acme-corp".to_string(),
                email: "billing@acme.example.com".to_string(),
                display_name: None,
                avatar_url: None,
                created_by: creator,
            },
            TEST_DEFAULT_ROLES,
        )
        .await
        .unwrap();

        assert_eq!(orgs.count(&filter).await.unwrap(), 1);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_organization(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme-corp".to_string(),
                    email: "old@acme.example.com".to_string(),
                    display_name: Some("Old Name".to_string()),
                    avatar_url: None,
                    created_by: creator,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();

        let updated = orgs
            .update(
                org.id,
                &OrganizationUpdateDBRequest {
                    display_name: Some("New Acme Name".to_string()),
                    avatar_url: None,
                    email: Some("new@acme.example.com".to_string()),
                    batch_notifications_enabled: None,
                    low_balance_threshold: None,
                    zero_data_retention: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.display_name.as_deref(), Some("New Acme Name"));
        assert_eq!(updated.email, "new@acme.example.com");
        assert_eq!(updated.user_type, "organization");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_organization_partial(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme-corp".to_string(),
                    email: "billing@acme.example.com".to_string(),
                    display_name: Some("Acme Corporation".to_string()),
                    avatar_url: None,
                    created_by: creator,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();

        // Update only email, leave display_name unchanged
        let updated = orgs
            .update(
                org.id,
                &OrganizationUpdateDBRequest {
                    display_name: None,
                    avatar_url: None,
                    email: Some("new@acme.example.com".to_string()),
                    batch_notifications_enabled: None,
                    low_balance_threshold: None,
                    zero_data_retention: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.display_name.as_deref(), Some("Acme Corporation"));
        assert_eq!(updated.email, "new@acme.example.com");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_organization_notification_settings(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme-corp".to_string(),
                    email: "billing@acme.example.com".to_string(),
                    display_name: Some("Acme Corporation".to_string()),
                    avatar_url: None,
                    created_by: creator,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();

        // Default: notifications disabled, no threshold
        assert!(!org.batch_notifications_enabled);
        assert!(org.low_balance_threshold.is_none());

        // Enable notifications and set threshold
        let updated = orgs
            .update(
                org.id,
                &OrganizationUpdateDBRequest {
                    display_name: None,
                    avatar_url: None,
                    email: None,
                    batch_notifications_enabled: Some(true),
                    low_balance_threshold: Some(Some(10.0)),
                    zero_data_retention: None,
                },
            )
            .await
            .unwrap();

        assert!(updated.batch_notifications_enabled);
        assert_eq!(updated.low_balance_threshold, Some(10.0));
        assert!(!updated.low_balance_notification_sent);

        // Partial update: change threshold only, notifications stay enabled
        let updated = orgs
            .update(
                org.id,
                &OrganizationUpdateDBRequest {
                    display_name: None,
                    avatar_url: None,
                    email: None,
                    batch_notifications_enabled: None,
                    low_balance_threshold: Some(Some(25.0)),
                    zero_data_retention: None,
                },
            )
            .await
            .unwrap();

        assert!(updated.batch_notifications_enabled);
        assert_eq!(updated.low_balance_threshold, Some(25.0));
        // Threshold change resets notification_sent flag
        assert!(!updated.low_balance_notification_sent);

        // Clear threshold to disable alerts
        let updated = orgs
            .update(
                org.id,
                &OrganizationUpdateDBRequest {
                    display_name: None,
                    avatar_url: None,
                    email: None,
                    batch_notifications_enabled: None,
                    low_balance_threshold: Some(None),
                    zero_data_retention: None,
                },
            )
            .await
            .unwrap();

        assert!(updated.batch_notifications_enabled);
        assert!(updated.low_balance_threshold.is_none());

        // Omitting threshold entirely leaves it unchanged
        let updated = orgs
            .update(
                org.id,
                &OrganizationUpdateDBRequest {
                    display_name: None,
                    avatar_url: None,
                    email: None,
                    batch_notifications_enabled: None,
                    low_balance_threshold: None,
                    zero_data_retention: None,
                },
            )
            .await
            .unwrap();

        assert!(updated.low_balance_threshold.is_none());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_organization(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme-corp".to_string(),
                    email: "billing@acme.example.com".to_string(),
                    display_name: Some("Acme Corporation".to_string()),
                    avatar_url: None,
                    created_by: creator,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();

        let deleted = orgs.delete(org.id).await.unwrap();
        assert!(deleted);

        // Should not appear in list
        let filter = OrganizationFilter::new(0, 100);
        let list = orgs.list(&filter).await.unwrap();
        assert!(list.is_empty());

        // Double-delete should return false
        let deleted_again = orgs.delete(org.id).await.unwrap();
        assert!(!deleted_again);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_organization_scrubs_data(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme-corp".to_string(),
                    email: "billing@acme.example.com".to_string(),
                    display_name: Some("Acme Corporation".to_string()),
                    avatar_url: None,
                    created_by: creator,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();

        orgs.delete(org.id).await.unwrap();

        // Verify scrubbed data via raw SQL
        let row = sqlx::query!("SELECT username, email, display_name, is_deleted FROM users WHERE id = $1", org.id)
            .fetch_one(&pool)
            .await
            .unwrap();

        assert!(row.is_deleted);
        assert!(row.display_name.is_none());
        assert!(row.email.contains("deleted"));
        assert!(row.username.contains("deleted"));
    }

    // ── Membership ────────────────────────────────────────────────────────

    #[sqlx::test]
    #[test_log::test]
    async fn test_add_member(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;
        let bob = create_individual(&pool, "bob", "bob@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme-corp".to_string(),
                    email: "billing@acme.example.com".to_string(),
                    display_name: None,
                    avatar_url: None,
                    created_by: creator,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();

        let member = orgs.add_member(org.id, bob, "member").await.unwrap();
        assert_eq!(member.user_id, Some(bob));
        assert_eq!(member.organization_id, org.id);
        assert_eq!(member.role, "member");
        assert_eq!(member.status, "active");

        // Should now have two members
        let members = orgs.list_members(org.id).await.unwrap();
        assert_eq!(members.len(), 2);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_add_duplicate_member_fails(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;
        let bob = create_individual(&pool, "bob", "bob@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme-corp".to_string(),
                    email: "billing@acme.example.com".to_string(),
                    display_name: None,
                    avatar_url: None,
                    created_by: creator,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();

        orgs.add_member(org.id, bob, "member").await.unwrap();

        // Adding same member again should fail (unique constraint)
        let result = orgs.add_member(org.id, bob, "admin").await;
        assert!(result.is_err());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_remove_member(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;
        let bob = create_individual(&pool, "bob", "bob@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme-corp".to_string(),
                    email: "billing@acme.example.com".to_string(),
                    display_name: None,
                    avatar_url: None,
                    created_by: creator,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();

        orgs.add_member(org.id, bob, "member").await.unwrap();

        let removed = orgs.remove_member(org.id, bob).await.unwrap();
        assert!(removed);

        // Should only have the creator
        let members = orgs.list_members(org.id).await.unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].user_id, Some(creator));

        // Removing non-member returns false
        let removed_again = orgs.remove_member(org.id, bob).await.unwrap();
        assert!(!removed_again);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_member_role(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;
        let bob = create_individual(&pool, "bob", "bob@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme-corp".to_string(),
                    email: "billing@acme.example.com".to_string(),
                    display_name: None,
                    avatar_url: None,
                    created_by: creator,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();

        orgs.add_member(org.id, bob, "member").await.unwrap();

        let updated = orgs.update_member_role(org.id, bob, "admin").await.unwrap();
        assert_eq!(updated.role, "admin");

        let role = orgs.get_user_org_role(bob, org.id).await.unwrap();
        assert_eq!(role, Some("admin".to_string()));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_member_role_nonexistent_returns_not_found(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;
        let bob = create_individual(&pool, "bob", "bob@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme-corp".to_string(),
                    email: "billing@acme.example.com".to_string(),
                    display_name: None,
                    avatar_url: None,
                    created_by: creator,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();

        // Bob is not a member — update should fail
        let result = orgs.update_member_role(org.id, bob, "admin").await;
        assert!(result.is_err());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_user_organizations(pool: PgPool) {
        let alice = create_individual(&pool, "alice", "alice@example.com").await;
        let bob = create_individual(&pool, "bob", "bob@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        let org1 = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme-corp".to_string(),
                    email: "billing@acme.example.com".to_string(),
                    display_name: None,
                    avatar_url: None,
                    created_by: alice,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();

        orgs.create(
            &OrganizationCreateDBRequest {
                name: "globex-inc".to_string(),
                email: "info@globex.example.com".to_string(),
                display_name: None,
                avatar_url: None,
                created_by: alice,
            },
            TEST_DEFAULT_ROLES,
        )
        .await
        .unwrap();

        // Add bob to only the first org
        orgs.add_member(org1.id, bob, "member").await.unwrap();

        // Alice should belong to both
        let alice_orgs = orgs.list_user_organizations(alice).await.unwrap();
        assert_eq!(alice_orgs.len(), 2);

        // Bob should belong to one
        let bob_orgs = orgs.list_user_organizations(bob).await.unwrap();
        assert_eq!(bob_orgs.len(), 1);
        assert_eq!(bob_orgs[0].organization_id, org1.id);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_user_org_role_not_member(pool: PgPool) {
        let alice = create_individual(&pool, "alice", "alice@example.com").await;
        let bob = create_individual(&pool, "bob", "bob@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme-corp".to_string(),
                    email: "billing@acme.example.com".to_string(),
                    display_name: None,
                    avatar_url: None,
                    created_by: alice,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();

        let role = orgs.get_user_org_role(bob, org.id).await.unwrap();
        assert_eq!(role, None);
    }

    // ── Trigger: enforce_organization_membership_types ─────────────────────

    #[sqlx::test]
    #[test_log::test]
    async fn test_cannot_add_member_to_individual_user(pool: PgPool) {
        let alice = create_individual(&pool, "alice", "alice@example.com").await;
        let bob = create_individual(&pool, "bob", "bob@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        // Try to add bob as a member of alice (an individual, not an org)
        let result = orgs.add_member(alice, bob, "member").await;
        assert!(result.is_err(), "Should not allow adding members to an individual user");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_deleted_org_excluded_from_list_user_organizations(pool: PgPool) {
        let alice = create_individual(&pool, "alice", "alice@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme-corp".to_string(),
                    email: "billing@acme.example.com".to_string(),
                    display_name: None,
                    avatar_url: None,
                    created_by: alice,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();

        orgs.delete(org.id).await.unwrap();

        let alice_orgs = orgs.list_user_organizations(alice).await.unwrap();
        assert!(alice_orgs.is_empty());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_organizations_pagination(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        for i in 0..5 {
            orgs.create(
                &OrganizationCreateDBRequest {
                    name: format!("org-{i}"),
                    email: format!("org-{i}@example.com"),
                    display_name: None,
                    avatar_url: None,
                    created_by: creator,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();
        }

        let page1 = orgs.list(&OrganizationFilter::new(0, 2)).await.unwrap();
        assert_eq!(page1.len(), 2);

        let page2 = orgs.list(&OrganizationFilter::new(2, 2)).await.unwrap();
        assert_eq!(page2.len(), 2);

        let page3 = orgs.list(&OrganizationFilter::new(4, 2)).await.unwrap();
        assert_eq!(page3.len(), 1);
    }

    /// Organizations can share the same contact email (non-unique for org users).
    #[sqlx::test]
    #[test_log::test]
    async fn test_orgs_can_share_contact_email(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        let shared_email = "shared@contact.example.com";

        let org1 = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "org-alpha".to_string(),
                    email: shared_email.to_string(),
                    display_name: None,
                    avatar_url: None,
                    created_by: creator,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();

        let org2 = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "org-beta".to_string(),
                    email: shared_email.to_string(),
                    display_name: None,
                    avatar_url: None,
                    created_by: creator,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();

        assert_eq!(org1.email, shared_email);
        assert_eq!(org2.email, shared_email);
        assert_ne!(org1.id, org2.id);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_find_by_domain(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        // No org yet
        let result = orgs.find_by_domain("acme.com").await.unwrap();
        assert!(result.is_none());

        // Create org with domain as username
        orgs.create(
            &OrganizationCreateDBRequest {
                name: "acme.com".to_string(),
                email: "contact@acme.com".to_string(),
                display_name: Some("Acme Corp".to_string()),
                avatar_url: None,
                created_by: creator,
            },
            TEST_DEFAULT_ROLES,
        )
        .await
        .unwrap();

        // Now found
        let result = orgs.find_by_domain("acme.com").await.unwrap();
        assert!(result.is_some());
        let org = result.unwrap();
        assert_eq!(org.username, "acme.com");
        assert_eq!(org.user_type, "organization");
    }

    /// Deleting an owner hands the workspace to the longest-standing admin.
    #[sqlx::test]
    async fn test_deleting_owner_promotes_earliest_admin(pool: PgPool) {
        let owner = create_individual(&pool, "owner", "owner@acme.com").await;
        let member = create_individual(&pool, "member", "member@acme.com").await;
        let admin_early = create_individual(&pool, "admin1", "admin1@acme.com").await;
        let admin_late = create_individual(&pool, "admin2", "admin2@acme.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);
        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme.com".to_string(),
                    email: "contact@acme.com".to_string(),
                    display_name: Some("Acme Corp".to_string()),
                    avatar_url: None,
                    created_by: owner,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();
        // Joined before either admin, so seniority alone would pick them - the
        // role ranking has to win.
        orgs.add_member(org.id, member, "member").await.unwrap();
        orgs.add_member(org.id, admin_early, "admin").await.unwrap();
        orgs.add_member(org.id, admin_late, "admin").await.unwrap();
        drop(conn);

        let mut conn = pool.acquire().await.unwrap();
        assert!(Users::new(&mut conn).delete(owner).await.unwrap());

        let mut orgs = Organizations::new(&mut conn);
        assert_eq!(
            orgs.get_user_org_role(admin_early, org.id).await.unwrap(),
            Some("owner".to_string()),
            "the earliest admin takes over"
        );
        assert_eq!(orgs.get_user_org_role(admin_late, org.id).await.unwrap(), Some("admin".to_string()));
        assert_eq!(orgs.get_user_org_role(member, org.id).await.unwrap(), Some("member".to_string()));
        assert_eq!(
            orgs.get_user_org_role(owner, org.id).await.unwrap(),
            None,
            "the departed owner keeps no membership"
        );
        assert!(
            orgs.find_by_domain("acme.com").await.unwrap().is_some(),
            "and the workspace stays routable"
        );
    }

    /// With no admins left, the longest-standing ordinary member takes over
    /// rather than the workspace being closed on people who are still using it.
    #[sqlx::test]
    async fn test_deleting_owner_promotes_earliest_member_when_no_admins(pool: PgPool) {
        let owner = create_individual(&pool, "owner", "owner@acme.com").await;
        let first = create_individual(&pool, "first", "first@acme.com").await;
        let second = create_individual(&pool, "second", "second@acme.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);
        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme.com".to_string(),
                    email: "contact@acme.com".to_string(),
                    display_name: Some("Acme Corp".to_string()),
                    avatar_url: None,
                    created_by: owner,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();
        orgs.add_member(org.id, first, "member").await.unwrap();
        orgs.add_member(org.id, second, "member").await.unwrap();
        drop(conn);

        let mut conn = pool.acquire().await.unwrap();
        assert!(Users::new(&mut conn).delete(owner).await.unwrap());

        let mut orgs = Organizations::new(&mut conn);
        assert_eq!(orgs.get_user_org_role(first, org.id).await.unwrap(), Some("owner".to_string()));
        assert_eq!(orgs.get_user_org_role(second, org.id).await.unwrap(), Some("member".to_string()));
    }

    /// A key issued to a member inside a workspace is owned by the workspace
    /// and only attributed to them, so account deletion's `WHERE user_id = $1`
    /// never saw it. API-key auth checks only `api_keys.is_deleted` and not
    /// whether the creator still exists, so the key kept working after the
    /// account it belonged to was gone.
    #[sqlx::test]
    async fn test_deleting_a_member_revokes_the_org_keys_they_hold(pool: PgPool) {
        let owner = create_individual(&pool, "owner", "owner@acme.com").await;
        let member = create_individual(&pool, "member", "member@acme.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);
        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme.com".to_string(),
                    email: "contact@acme.com".to_string(),
                    display_name: Some("Acme Corp".to_string()),
                    avatar_url: None,
                    created_by: owner,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();
        orgs.add_member(org.id, member, "member").await.unwrap();

        // Issued key: owned by the workspace, attributed to the member.
        sqlx::query!(
            "INSERT INTO api_keys (user_id, created_by, name, secret, purpose) VALUES ($1, $2, 'issued', 'sk-issued-key', 'inference')",
            org.id,
            member
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        // A key the owner holds in the same workspace must be left alone.
        sqlx::query!(
            "INSERT INTO api_keys (user_id, created_by, name, secret, purpose) VALUES ($1, $2, 'owners', 'sk-owner-key', 'inference')",
            org.id,
            owner
        )
        .execute(&mut *conn)
        .await
        .unwrap();

        assert!(Users::new(&mut conn).delete(member).await.unwrap());

        let live: Vec<String> = sqlx::query_scalar!(
            r#"SELECT name as "name!" FROM api_keys WHERE user_id = $1 AND is_deleted = false ORDER BY name"#,
            org.id
        )
        .fetch_all(&mut *conn)
        .await
        .unwrap();
        assert_eq!(
            live,
            vec!["owners".to_string()],
            "the departed member's issued key must stop authenticating"
        );
    }

    /// Closing a workspace must not fall over on a key something else still
    /// references. `connections.api_key_id` points at `api_keys(id)` with NO
    /// ACTION, so hard-deleting the workspace's keys raised a foreign-key
    /// violation and rolled back the entire account deletion - the account
    /// could not be deleted at all while the workspace had a connection.
    #[sqlx::test]
    async fn test_closing_a_workspace_with_a_connection_still_deletes_the_account(pool: PgPool) {
        let owner = create_individual(&pool, "owner", "owner@acme.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);
        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme.com".to_string(),
                    email: "contact@acme.com".to_string(),
                    display_name: Some("Acme Corp".to_string()),
                    avatar_url: None,
                    created_by: owner,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();

        let key_id: uuid::Uuid = sqlx::query_scalar!(
            "INSERT INTO api_keys (user_id, created_by, name, secret, purpose) VALUES ($1, $1, 'org key', 'sk-conn-key', 'inference') RETURNING id",
            org.id
        )
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO connections (user_id, api_key_id, kind, provider, name, config_encrypted) VALUES ($1, $2, 'source', 'openai', 'conn', '\\x00'::bytea)",
            org.id,
            key_id
        )
        .execute(&mut *conn)
        .await
        .unwrap();

        // The workspace has no successor, so this closes it.
        assert!(
            Users::new(&mut conn).delete(owner).await.unwrap(),
            "the account must delete even though a connection pins the workspace's key"
        );

        let (org_deleted, key_live) = sqlx::query!(
            r#"SELECT (SELECT is_deleted FROM users WHERE id = $1) as "org_deleted!",
                      (SELECT count(*) FROM api_keys WHERE user_id = $1 AND is_deleted = false) as "key_live!""#,
            org.id
        )
        .fetch_one(&mut *conn)
        .await
        .map(|r| (r.org_deleted, r.key_live))
        .unwrap();
        assert!(org_deleted, "the workspace is still closed");
        assert_eq!(key_live, 0, "and its keys stop authenticating");
    }

    /// A co-owner keeps the workspace; nobody is promoted and nothing closes.
    #[sqlx::test]
    async fn test_deleting_one_of_two_owners_leaves_the_other(pool: PgPool) {
        let leaving = create_individual(&pool, "leaving", "leaving@acme.com").await;
        let staying = create_individual(&pool, "staying", "staying@acme.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);
        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme.com".to_string(),
                    email: "contact@acme.com".to_string(),
                    display_name: Some("Acme Corp".to_string()),
                    avatar_url: None,
                    created_by: leaving,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();
        orgs.add_member(org.id, staying, "owner").await.unwrap();
        drop(conn);

        let mut conn = pool.acquire().await.unwrap();
        assert!(Users::new(&mut conn).delete(leaving).await.unwrap());

        let mut orgs = Organizations::new(&mut conn);
        assert_eq!(orgs.get_user_org_role(staying, org.id).await.unwrap(), Some("owner".to_string()));
        assert!(orgs.find_by_domain("acme.com").await.unwrap().is_some());
    }

    /// Nobody left to hand it to: the workspace is closed the same way the
    /// account is - scrubbed, flagged deleted, and stripped of the keys that
    /// authenticate as it.
    #[sqlx::test]
    async fn test_deleting_sole_owner_closes_the_workspace(pool: PgPool) {
        let owner = create_individual(&pool, "owner", "owner@acme.com").await;
        let departed = create_individual(&pool, "departed", "departed@acme.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);
        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme.com".to_string(),
                    email: "contact@acme.com".to_string(),
                    display_name: Some("Acme Corp".to_string()),
                    avatar_url: None,
                    created_by: owner,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();
        // The only other member is already deleted, so there is no live
        // successor even though a membership row exists.
        orgs.add_member(org.id, departed, "admin").await.unwrap();
        drop(conn);

        let mut conn = pool.acquire().await.unwrap();
        assert!(Users::new(&mut conn).delete(departed).await.unwrap());
        sqlx::query!(
            "INSERT INTO api_keys (user_id, created_by, name, secret, purpose) VALUES ($1, $1, 'org key', 'sk-test-org-key', 'inference')",
            org.id
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        assert!(Users::new(&mut conn).delete(owner).await.unwrap());

        let closed = sqlx::query!("SELECT username, email, display_name, is_deleted FROM users WHERE id = $1", org.id)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert!(closed.is_deleted, "the workspace is closed with its last owner");
        assert_eq!(
            closed.username,
            format!("deleted-{}", org.id),
            "and scrubbed the same way a user is"
        );
        assert_eq!(closed.email, format!("deleted-{}@deleted.local", org.id));
        assert!(closed.display_name.is_none());

        // Revoked rather than removed: a hard delete trips
        // `connections.api_key_id`, which is NO ACTION, and rolls back the whole
        // account deletion. Authentication checks `is_deleted`, so revoking is
        // what actually matters here.
        let keys = sqlx::query_scalar!(
            r#"SELECT count(*) as "count!" FROM api_keys WHERE user_id = $1 AND is_deleted = false"#,
            org.id
        )
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        assert_eq!(keys, 0, "nothing may keep authenticating as a closed workspace");

        let mut orgs = Organizations::new(&mut conn);
        assert!(orgs.find_by_domain("acme.com").await.unwrap().is_none());
    }

    /// Deleting a user scrubs their row and leaves their `user_organizations`
    /// rows behind, so deleting an organization's only owner leaves the
    /// workspace live and still claiming its domain with nobody able to
    /// administer it. Colleagues signing up were offered that workspace and
    /// could file join requests into it that no one could ever approve - the
    /// same dead-end the `is_deleted = false` filter exists to prevent, one
    /// level down.
    #[sqlx::test]
    async fn test_find_by_domain_skips_orgs_with_no_live_admin(pool: PgPool) {
        let owner = create_individual(&pool, "owner", "owner@acme.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);
        let org = orgs
            .create(
                &OrganizationCreateDBRequest {
                    name: "acme.com".to_string(),
                    email: "contact@acme.com".to_string(),
                    display_name: Some("Acme Corp".to_string()),
                    avatar_url: None,
                    created_by: owner,
                },
                TEST_DEFAULT_ROLES,
            )
            .await
            .unwrap();
        assert!(
            orgs.find_by_domain("acme.com").await.unwrap().is_some(),
            "routable while the owner is live"
        );
        drop(conn);

        // Strand the workspace the way the legacy rows were stranded: mark only
        // the owner deleted, leaving the organization and the membership row
        // live. Going through `Users::delete` would no longer reproduce it -
        // that now closes an unhandable workspace outright, so the lookup would
        // return `None` via the older `is_deleted = false` filter and this test
        // would pass without ever reaching the predicate it exists to cover.
        let mut conn = pool.acquire().await.unwrap();
        sqlx::query!("UPDATE users SET is_deleted = true WHERE id = $1", owner)
            .execute(&mut *conn)
            .await
            .unwrap();

        let still_live = sqlx::query_scalar!(r#"SELECT is_deleted as "is_deleted!" FROM users WHERE id = $1"#, org.id)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert!(!still_live, "precondition: the workspace itself is not deleted");
        let mut orgs = Organizations::new(&mut conn);
        assert!(
            orgs.find_by_domain("acme.com").await.unwrap().is_none(),
            "an ownerless workspace must not collect join requests nobody can approve"
        );
    }

    /// `find_by_domain` interpolates its argument into a `LIKE` pattern, so a
    /// wildcard reaching it matches unrelated workspaces - `%` searches for
    /// `LIKE '%~%'`, which every suffixed username satisfies. Nothing upstream
    /// guarantees the domain is a DNS name: proxy-header auth stores whatever
    /// address it is handed.
    #[sqlx::test]
    async fn test_find_by_domain_ignores_sql_wildcards(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;
        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);
        orgs.create(
            &OrganizationCreateDBRequest {
                name: "acme.com~a1b2c3d4".to_string(),
                email: "contact@acme.com".to_string(),
                display_name: Some("Acme Corp".to_string()),
                avatar_url: None,
                created_by: creator,
            },
            TEST_DEFAULT_ROLES,
        )
        .await
        .unwrap();

        assert!(
            orgs.find_by_domain("acme.com").await.unwrap().is_some(),
            "control: the real domain resolves"
        );
        for wildcard in ["%", "_", "%.com", "acme.co_", "acme%"] {
            assert!(
                orgs.find_by_domain(wildcard).await.unwrap().is_none(),
                "{wildcard} must not be treated as a pattern"
            );
        }
    }

    /// The opaque `user~{suffix}` username is only unroutable because `user`
    /// cannot be a domain. If a single-label name reached the lookup it would
    /// match every personal-email workspace at once, which is the
    /// unauthorised-membership path the opaque username exists to close.
    #[sqlx::test]
    async fn test_find_by_domain_ignores_single_label_names(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@gmail.com").await;
        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);
        orgs.create(
            &OrganizationCreateDBRequest {
                // Exactly what `create_organization` stores for an owner with
                // no domain to claim.
                name: "user~a1b2c3d4".to_string(),
                email: "alice@gmail.com".to_string(),
                display_name: Some("Alice's workspace".to_string()),
                avatar_url: None,
                created_by: creator,
            },
            TEST_DEFAULT_ROLES,
        )
        .await
        .unwrap();

        assert!(
            orgs.find_by_domain("user").await.unwrap().is_none(),
            "a personal-email workspace must not be reachable through its namespace"
        );
        assert!(orgs.find_by_domain("localhost").await.unwrap().is_none());
    }
}
