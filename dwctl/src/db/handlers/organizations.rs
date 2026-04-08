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
        organizations::{OrganizationCreateDBRequest, OrganizationMemberDBResponse, OrganizationUpdateDBRequest},
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

    /// Returns `true` if a non-deleted organization with the given ID exists.
    #[instrument(skip(self), fields(org_id = %abbrev_uuid(&id)), err)]
    pub async fn exists(&mut self, id: UserId) -> Result<bool> {
        let exists = sqlx::query_scalar!(
            "SELECT EXISTS(SELECT 1 FROM users WHERE id = $1 AND user_type = 'organization' AND is_deleted = false) as \"exists!\"",
            id
        )
        .fetch_one(&mut *self.db)
        .await?;
        Ok(exists)
    }

    /// Find an organization by its domain (stored as username).
    /// Returns `None` if no active (non-deleted) organization exists with that domain.
    #[instrument(skip(self), fields(domain = %domain), err)]
    pub async fn find_by_domain(&mut self, domain: &str) -> Result<Option<UserDBResponse>> {
        let row = sqlx::query!(
            r#"
            SELECT id, username, email, display_name, avatar_url, auth_source, created_at, updated_at,
                   is_admin, password_hash, external_user_id, payment_provider_id,
                   is_deleted, is_internal, batch_notifications_enabled, first_batch_email_sent,
                   low_balance_notification_sent, low_balance_threshold,
                   auto_topup_amount, auto_topup_threshold, auto_topup_monthly_limit, user_type
            FROM users
            WHERE username = $1 AND user_type = 'organization' AND is_deleted = false
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
                }))
            }
            None => Ok(None),
        }
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
                      auto_topup_amount, auto_topup_threshold, auto_topup_monthly_limit, user_type
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
                updated_at = NOW()
            WHERE id = $1 AND user_type = 'organization' AND is_deleted = false
            RETURNING id, username, email, display_name, avatar_url, auth_source, created_at, updated_at,
                      is_admin, password_hash, external_user_id, payment_provider_id,
                      batch_notifications_enabled, first_batch_email_sent,
                      low_balance_notification_sent, low_balance_threshold,
                      auto_topup_amount, auto_topup_threshold, auto_topup_monthly_limit, user_type
            "#,
            id,
            request.display_name,
            request.avatar_url,
            request.email,
            request.batch_notifications_enabled,
            request.low_balance_threshold.is_some() as bool,
            request.low_balance_threshold.flatten(),
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
              AND (uo.user_id IS NULL OR u.is_deleted = false)
            ORDER BY uo.status ASC, uo.created_at ASC
            "#,
            org_id
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().map(Into::into).collect())
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
    #[instrument(skip(self), fields(invite_id = %abbrev_uuid(&invite_id), user_id = %abbrev_uuid(&user_id)), err)]
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

    /// Count active members of an organization (for member_count in list responses)
    #[instrument(skip(self), fields(org_id = %abbrev_uuid(&org_id)), err)]
    pub async fn count_members(&mut self, org_id: UserId) -> Result<i64> {
        let count = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM user_organizations WHERE organization_id = $1 AND status = 'active'",
            org_id
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(count.unwrap_or(0))
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
}
