//! Database repository for organizations and memberships.

use crate::db::{
    errors::{DbError, Result},
    models::{
        organizations::{OrganizationCreateDBRequest, OrganizationMemberDBResponse, OrganizationUpdateDBRequest},
        users::UserDBResponse,
    },
};
use crate::types::{UserId, abbrev_uuid};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Acquire, FromRow, PgConnection, QueryBuilder};
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

    /// Create a new organization. The creator is automatically added as owner.
    #[instrument(skip(self, request), fields(name = %request.name), err)]
    pub async fn create(&mut self, request: &OrganizationCreateDBRequest) -> Result<UserDBResponse> {
        use crate::api::models::users::Role;

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
                      low_balance_notification_sent, low_balance_threshold, user_type
            "#,
            org_id,
            request.name,
            request.email,
            request.display_name,
            request.avatar_url,
        )
        .fetch_one(&mut *tx)
        .await?;

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
            auth_source: row.auth_source,
            is_admin: row.is_admin,
            roles: vec![Role::StandardUser],
            password_hash: row.password_hash,
            external_user_id: row.external_user_id,
            payment_provider_id: row.payment_provider_id,
            batch_notifications_enabled: row.batch_notifications_enabled,
            first_batch_email_sent: row.first_batch_email_sent,
            low_balance_notification_sent: row.low_balance_notification_sent,
            low_balance_threshold: row.low_balance_threshold,
            user_type: row.user_type,
        })
    }

    /// List organizations with pagination and optional search
    #[instrument(skip(self, filter), fields(limit = filter.limit, skip = filter.skip), err)]
    pub async fn list(&mut self, filter: &OrganizationFilter) -> Result<Vec<UserDBResponse>> {
        use crate::api::models::users::Role;

        let mut query = QueryBuilder::new("SELECT * FROM users WHERE user_type = 'organization' AND is_deleted = false");

        if let Some(ref search) = filter.search {
            let search_pattern = format!("%{}%", search.to_lowercase());
            query.push(" AND (LOWER(COALESCE(display_name, '')) LIKE ");
            query.push_bind(search_pattern.clone());
            query.push(" OR LOWER(username) LIKE ");
            query.push_bind(search_pattern.clone());
            query.push(" OR LOWER(email) LIKE ");
            query.push_bind(search_pattern);
            query.push(")");
        }

        query.push(" ORDER BY created_at DESC LIMIT ");
        query.push_bind(filter.limit);
        query.push(" OFFSET ");
        query.push_bind(filter.skip);

        // We need to define a row struct since we're using QueryBuilder
        #[derive(FromRow)]
        struct OrgRow {
            id: UserId,
            username: String,
            email: String,
            display_name: Option<String>,
            avatar_url: Option<String>,
            auth_source: String,
            created_at: DateTime<Utc>,
            updated_at: DateTime<Utc>,
            is_admin: bool,
            password_hash: Option<String>,
            external_user_id: Option<String>,
            payment_provider_id: Option<String>,
            batch_notifications_enabled: bool,
            first_batch_email_sent: bool,
            low_balance_notification_sent: bool,
            low_balance_threshold: Option<f32>,
            user_type: String,
            // Columns that exist but we don't use in the response
            #[allow(dead_code)]
            is_deleted: bool,
            #[allow(dead_code)]
            is_internal: bool,
            #[allow(dead_code)]
            last_login: Option<DateTime<Utc>>,
        }

        let orgs: Vec<OrgRow> = query.build_query_as().fetch_all(&mut *self.db).await?;

        Ok(orgs
            .into_iter()
            .map(|o| UserDBResponse {
                id: o.id,
                username: o.username,
                email: o.email,
                display_name: o.display_name,
                avatar_url: o.avatar_url,
                created_at: o.created_at,
                updated_at: o.updated_at,
                auth_source: o.auth_source,
                is_admin: o.is_admin,
                roles: vec![Role::StandardUser],
                password_hash: o.password_hash,
                external_user_id: o.external_user_id,
                payment_provider_id: o.payment_provider_id,
                batch_notifications_enabled: o.batch_notifications_enabled,
                first_batch_email_sent: o.first_batch_email_sent,
                low_balance_notification_sent: o.low_balance_notification_sent,
                low_balance_threshold: o.low_balance_threshold,
                user_type: o.user_type,
            })
            .collect())
    }

    /// Count organizations matching the filter
    #[instrument(skip(self, filter), fields(search = filter.search), err)]
    pub async fn count(&mut self, filter: &OrganizationFilter) -> Result<i64> {
        let mut query = QueryBuilder::new("SELECT COUNT(*) FROM users WHERE user_type = 'organization' AND is_deleted = false");

        if let Some(ref search) = filter.search {
            let search_pattern = format!("%{}%", search.to_lowercase());
            query.push(" AND (LOWER(COALESCE(display_name, '')) LIKE ");
            query.push_bind(search_pattern.clone());
            query.push(" OR LOWER(username) LIKE ");
            query.push_bind(search_pattern.clone());
            query.push(" OR LOWER(email) LIKE ");
            query.push_bind(search_pattern);
            query.push(")");
        }

        let count: (i64,) = query.build_query_as().fetch_one(&mut *self.db).await?;
        Ok(count.0)
    }

    /// Update an organization's details
    #[instrument(skip(self, request), fields(org_id = %abbrev_uuid(&id)), err)]
    pub async fn update(&mut self, id: UserId, request: &OrganizationUpdateDBRequest) -> Result<UserDBResponse> {
        use crate::api::models::users::Role;

        let row = sqlx::query!(
            r#"
            UPDATE users SET
                display_name = COALESCE($2, display_name),
                avatar_url = COALESCE($3, avatar_url),
                email = COALESCE($4, email),
                updated_at = NOW()
            WHERE id = $1 AND user_type = 'organization' AND is_deleted = false
            RETURNING id, username, email, display_name, avatar_url, auth_source, created_at, updated_at,
                      is_admin, password_hash, external_user_id, payment_provider_id,
                      batch_notifications_enabled, first_batch_email_sent,
                      low_balance_notification_sent, low_balance_threshold, user_type
            "#,
            id,
            request.display_name,
            request.avatar_url,
            request.email,
        )
        .fetch_optional(&mut *self.db)
        .await?
        .ok_or(DbError::NotFound)?;

        Ok(UserDBResponse {
            id: row.id,
            username: row.username,
            email: row.email,
            display_name: row.display_name,
            avatar_url: row.avatar_url,
            created_at: row.created_at,
            updated_at: row.updated_at,
            auth_source: row.auth_source,
            is_admin: row.is_admin,
            roles: vec![Role::StandardUser],
            password_hash: row.password_hash,
            external_user_id: row.external_user_id,
            payment_provider_id: row.payment_provider_id,
            batch_notifications_enabled: row.batch_notifications_enabled,
            first_batch_email_sent: row.first_batch_email_sent,
            low_balance_notification_sent: row.low_balance_notification_sent,
            low_balance_threshold: row.low_balance_threshold,
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
            .create(&OrganizationCreateDBRequest {
                name: "acme-corp".to_string(),
                email: "billing@acme.example.com".to_string(),
                display_name: Some("Acme Corporation".to_string()),
                avatar_url: None,
                created_by: creator,
            })
            .await
            .unwrap();

        assert_eq!(org.username, "acme-corp");
        assert_eq!(org.email, "billing@acme.example.com");
        assert_eq!(org.display_name.as_deref(), Some("Acme Corporation"));
        assert_eq!(org.user_type, "organization");
        assert_eq!(org.auth_source, "organization");
        assert!(!org.is_admin);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_organization_adds_creator_as_owner(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        let org = orgs
            .create(&OrganizationCreateDBRequest {
                name: "acme-corp".to_string(),
                email: "billing@acme.example.com".to_string(),
                display_name: None,
                avatar_url: None,
                created_by: creator,
            })
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

        orgs.create(&OrganizationCreateDBRequest {
            name: "acme-corp".to_string(),
            email: "billing@acme.example.com".to_string(),
            display_name: Some("Acme Corporation".to_string()),
            avatar_url: None,
            created_by: creator,
        })
        .await
        .unwrap();

        orgs.create(&OrganizationCreateDBRequest {
            name: "globex-inc".to_string(),
            email: "info@globex.example.com".to_string(),
            display_name: Some("Globex Inc".to_string()),
            avatar_url: None,
            created_by: creator,
        })
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

        orgs.create(&OrganizationCreateDBRequest {
            name: "acme-corp".to_string(),
            email: "billing@acme.example.com".to_string(),
            display_name: Some("Acme Corporation".to_string()),
            avatar_url: None,
            created_by: creator,
        })
        .await
        .unwrap();

        orgs.create(&OrganizationCreateDBRequest {
            name: "globex-inc".to_string(),
            email: "info@globex.example.com".to_string(),
            display_name: Some("Globex Inc".to_string()),
            avatar_url: None,
            created_by: creator,
        })
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

        orgs.create(&OrganizationCreateDBRequest {
            name: "acme-corp".to_string(),
            email: "billing@acme.example.com".to_string(),
            display_name: None,
            avatar_url: None,
            created_by: creator,
        })
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
            .create(&OrganizationCreateDBRequest {
                name: "acme-corp".to_string(),
                email: "old@acme.example.com".to_string(),
                display_name: Some("Old Name".to_string()),
                avatar_url: None,
                created_by: creator,
            })
            .await
            .unwrap();

        let updated = orgs
            .update(
                org.id,
                &OrganizationUpdateDBRequest {
                    display_name: Some("New Acme Name".to_string()),
                    avatar_url: None,
                    email: Some("new@acme.example.com".to_string()),
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
            .create(&OrganizationCreateDBRequest {
                name: "acme-corp".to_string(),
                email: "billing@acme.example.com".to_string(),
                display_name: Some("Acme Corporation".to_string()),
                avatar_url: None,
                created_by: creator,
            })
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
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.display_name.as_deref(), Some("Acme Corporation"));
        assert_eq!(updated.email, "new@acme.example.com");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_organization(pool: PgPool) {
        let creator = create_individual(&pool, "alice", "alice@example.com").await;

        let mut conn = pool.acquire().await.unwrap();
        let mut orgs = Organizations::new(&mut conn);

        let org = orgs
            .create(&OrganizationCreateDBRequest {
                name: "acme-corp".to_string(),
                email: "billing@acme.example.com".to_string(),
                display_name: Some("Acme Corporation".to_string()),
                avatar_url: None,
                created_by: creator,
            })
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
            .create(&OrganizationCreateDBRequest {
                name: "acme-corp".to_string(),
                email: "billing@acme.example.com".to_string(),
                display_name: Some("Acme Corporation".to_string()),
                avatar_url: None,
                created_by: creator,
            })
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
            .create(&OrganizationCreateDBRequest {
                name: "acme-corp".to_string(),
                email: "billing@acme.example.com".to_string(),
                display_name: None,
                avatar_url: None,
                created_by: creator,
            })
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
            .create(&OrganizationCreateDBRequest {
                name: "acme-corp".to_string(),
                email: "billing@acme.example.com".to_string(),
                display_name: None,
                avatar_url: None,
                created_by: creator,
            })
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
            .create(&OrganizationCreateDBRequest {
                name: "acme-corp".to_string(),
                email: "billing@acme.example.com".to_string(),
                display_name: None,
                avatar_url: None,
                created_by: creator,
            })
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
            .create(&OrganizationCreateDBRequest {
                name: "acme-corp".to_string(),
                email: "billing@acme.example.com".to_string(),
                display_name: None,
                avatar_url: None,
                created_by: creator,
            })
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
            .create(&OrganizationCreateDBRequest {
                name: "acme-corp".to_string(),
                email: "billing@acme.example.com".to_string(),
                display_name: None,
                avatar_url: None,
                created_by: creator,
            })
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
            .create(&OrganizationCreateDBRequest {
                name: "acme-corp".to_string(),
                email: "billing@acme.example.com".to_string(),
                display_name: None,
                avatar_url: None,
                created_by: alice,
            })
            .await
            .unwrap();

        orgs.create(&OrganizationCreateDBRequest {
            name: "globex-inc".to_string(),
            email: "info@globex.example.com".to_string(),
            display_name: None,
            avatar_url: None,
            created_by: alice,
        })
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
            .create(&OrganizationCreateDBRequest {
                name: "acme-corp".to_string(),
                email: "billing@acme.example.com".to_string(),
                display_name: None,
                avatar_url: None,
                created_by: alice,
            })
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
            .create(&OrganizationCreateDBRequest {
                name: "acme-corp".to_string(),
                email: "billing@acme.example.com".to_string(),
                display_name: None,
                avatar_url: None,
                created_by: alice,
            })
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
            orgs.create(&OrganizationCreateDBRequest {
                name: format!("org-{i}"),
                email: format!("org-{i}@example.com"),
                display_name: None,
                avatar_url: None,
                created_by: creator,
            })
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
}
