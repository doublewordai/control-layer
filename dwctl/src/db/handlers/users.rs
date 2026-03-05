//! Database repository for users.

use crate::types::{UserId, abbrev_uuid};
use crate::{
    api::models::users::Role,
    db::{
        errors::{DbError, Result},
        handlers::{Groups, api_keys::ApiKeys, repository::Repository},
        models::{
            api_keys::ApiKeyPurpose,
            users::{UserCreateDBRequest, UserDBResponse, UserUpdateDBRequest},
        },
    },
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Connection, FromRow, PgConnection};
use tracing::instrument;
use uuid::Uuid;

/// Filter for listing users
#[derive(Debug, Clone)]
pub struct UserFilter {
    pub skip: i64,
    pub limit: i64,
    pub search: Option<String>, // Case-insensitive substring search on display_name, username, and email
    pub user_type: String,
}

impl UserFilter {
    pub fn new(skip: i64, limit: i64) -> Self {
        Self { skip, limit, search: None, user_type: "individual".to_string() }
    }

    pub fn organizations(skip: i64, limit: i64) -> Self {
        Self { skip, limit, search: None, user_type: "organization".to_string() }
    }

    pub fn with_search(mut self, search: String) -> Self {
        self.search = Some(search);
        self
    }
}

/// Minimal user info for low-balance notifications.
#[derive(Debug, Clone)]
pub struct LowBalanceUser {
    pub id: UserId,
    pub email: String,
    pub username: String,
    pub display_name: Option<String>,
    pub balance: rust_decimal::Decimal,
}

// Database entity model
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
struct User {
    pub id: UserId,
    pub username: String,
    pub email: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub auth_source: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_login: Option<DateTime<Utc>>,
    pub is_admin: bool,
    pub password_hash: Option<String>,
    pub external_user_id: Option<String>,
    pub payment_provider_id: Option<String>,
    pub is_deleted: bool,
    pub is_internal: bool,
    pub batch_notifications_enabled: bool,
    pub first_batch_email_sent: bool,
    pub low_balance_notification_sent: bool,
    pub low_balance_threshold: Option<f32>,
    pub user_type: String,
}

pub struct Users<'c> {
    db: &'c mut PgConnection,
}

impl From<(Vec<Role>, User)> for UserDBResponse {
    fn from((roles, user): (Vec<Role>, User)) -> Self {
        Self {
            id: user.id,
            username: user.username,
            email: user.email,
            display_name: user.display_name,
            avatar_url: user.avatar_url,
            created_at: user.created_at,
            updated_at: user.updated_at,
            auth_source: user.auth_source,
            is_admin: user.is_admin,
            roles,
            password_hash: user.password_hash,
            external_user_id: user.external_user_id,
            payment_provider_id: user.payment_provider_id,
            batch_notifications_enabled: user.batch_notifications_enabled,
            first_batch_email_sent: user.first_batch_email_sent,
            low_balance_notification_sent: user.low_balance_notification_sent,
            low_balance_threshold: user.low_balance_threshold,
            user_type: user.user_type,
        }
    }
}

#[async_trait::async_trait]
impl<'c> Repository for Users<'c> {
    type CreateRequest = UserCreateDBRequest;
    type UpdateRequest = UserUpdateDBRequest;
    type Response = UserDBResponse;
    type Id = UserId;
    type Filter = UserFilter;

    #[instrument(skip(self, request), fields(username = %request.username), err)]
    async fn create(&mut self, request: &Self::CreateRequest) -> Result<Self::Response> {
        // Always generate a new ID for users
        let user_id = Uuid::new_v4();

        let mut tx = self.db.begin().await?;
        // Insert user
        let user = sqlx::query_as!(
            User,
            r#"
            INSERT INTO users (id, username, email, display_name, avatar_url, auth_source, is_admin, password_hash, external_user_id)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            RETURNING *
            "#,
            user_id,
            request.username,
            request.email,
            request.display_name,
            request.avatar_url,
            request.auth_source,
            request.is_admin,
            request.password_hash,
            request.external_user_id
        )
        .fetch_one(&mut *tx)
        .await?;

        // Ensure StandardUser role is always present
        let mut roles_to_insert = request.roles.clone();
        if !roles_to_insert.contains(&Role::StandardUser) {
            roles_to_insert.push(Role::StandardUser);
        }

        // Insert roles (with StandardUser guaranteed to be included)
        for role in &roles_to_insert {
            sqlx::query!("INSERT INTO user_roles (user_id, role) VALUES ($1, $2)", user_id, role as &Role)
                .execute(&mut *tx)
                .await?;
        }

        // Pre-create hidden API keys for batch and playground to avoid race condition with onwards sync
        // These keys must exist before the user's first request to ensure immediate access
        // Realtime keys are NOT pre-created - users create them explicitly via API and can tolerate activation delay
        let mut api_keys_repo = ApiKeys::new(&mut tx);
        api_keys_repo
            .get_or_create_hidden_key(user_id, ApiKeyPurpose::Batch, user_id)
            .await?;
        api_keys_repo
            .get_or_create_hidden_key(user_id, ApiKeyPurpose::Playground, user_id)
            .await?;

        tx.commit().await?;

        Ok(UserDBResponse::from((roles_to_insert, user)))
    }

    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&id)), err)]
    async fn get_by_id(&mut self, id: Self::Id) -> Result<Option<Self::Response>> {
        let result = sqlx::query!(
            r#"
            SELECT
                u.id,
                u.username,
                u.email,
                u.display_name,
                u.avatar_url,
                u.auth_source,
                u.created_at,
                u.updated_at,
                u.last_login,
                u.is_admin,
                u.password_hash,
                u.external_user_id,
                u.payment_provider_id,
                u.is_deleted,
                u.is_internal,
                u.batch_notifications_enabled,
                u.first_batch_email_sent,
                u.low_balance_notification_sent,
                u.low_balance_threshold,
                u.user_type,
                ARRAY_AGG(ur.role) FILTER (WHERE ur.role IS NOT NULL) as "roles: Vec<Role>"
            FROM users u
            LEFT JOIN user_roles ur ON ur.user_id = u.id
            WHERE u.id = $1 AND u.id != '00000000-0000-0000-0000-000000000000' AND u.is_deleted = false
            GROUP BY u.id, u.username, u.email, u.display_name, u.avatar_url, u.auth_source, u.created_at, u.updated_at, u.last_login, u.is_admin, u.password_hash, u.external_user_id, u.payment_provider_id, u.is_deleted, u.is_internal, u.batch_notifications_enabled, u.first_batch_email_sent, u.low_balance_notification_sent, u.low_balance_threshold, u.user_type
            "#,
            id
        )
        .fetch_optional(&mut *self.db)
        .await?;

        if let Some(row) = result {
            let user = User {
                id: row.id,
                username: row.username,
                email: row.email,
                display_name: row.display_name,
                avatar_url: row.avatar_url,
                auth_source: row.auth_source,
                created_at: row.created_at,
                updated_at: row.updated_at,
                last_login: row.last_login,
                is_admin: row.is_admin,
                password_hash: row.password_hash,
                external_user_id: row.external_user_id,
                payment_provider_id: row.payment_provider_id,
                is_deleted: row.is_deleted,
                is_internal: row.is_internal,
                batch_notifications_enabled: row.batch_notifications_enabled,
                first_batch_email_sent: row.first_batch_email_sent,
                low_balance_notification_sent: row.low_balance_notification_sent,
                low_balance_threshold: row.low_balance_threshold,
                user_type: row.user_type,
            };

            let roles = row.roles.unwrap_or_default();

            Ok(Some(UserDBResponse::from((roles, user))))
        } else {
            Ok(None)
        }
    }

    #[instrument(skip(self, ids), fields(count = ids.len()), err)]
    async fn get_bulk(&mut self, ids: Vec<UserId>) -> Result<std::collections::HashMap<Self::Id, UserDBResponse>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        // Use a single JOIN query to avoid N+1 queries
        let rows = sqlx::query!(
            r#"
            SELECT
                u.id,
                u.username,
                u.email,
                u.display_name,
                u.avatar_url,
                u.auth_source,
                u.created_at,
                u.updated_at,
                u.last_login,
                u.is_admin,
                u.password_hash,
                u.external_user_id,
                u.payment_provider_id,
                u.is_deleted,
                u.is_internal,
                u.batch_notifications_enabled,
                u.first_batch_email_sent,
                u.low_balance_notification_sent,
                u.low_balance_threshold,
                u.user_type,
                ARRAY_AGG(ur.role) FILTER (WHERE ur.role IS NOT NULL) as "roles: Vec<Role>"
            FROM users u
            LEFT JOIN user_roles ur ON ur.user_id = u.id
            WHERE u.id = ANY($1) AND u.id != '00000000-0000-0000-0000-000000000000' AND u.is_deleted = false
            GROUP BY u.id, u.username, u.email, u.display_name, u.avatar_url, u.auth_source, u.created_at, u.updated_at, u.last_login, u.is_admin, u.password_hash, u.external_user_id, u.payment_provider_id, u.is_deleted, u.is_internal, u.batch_notifications_enabled, u.first_batch_email_sent, u.low_balance_notification_sent, u.low_balance_threshold, u.user_type
            "#,
            ids.as_slice()
        )
        .fetch_all(&mut *self.db)
        .await?;

        let mut result = std::collections::HashMap::new();

        for row in rows {
            let user = User {
                id: row.id,
                username: row.username,
                email: row.email,
                display_name: row.display_name,
                avatar_url: row.avatar_url,
                auth_source: row.auth_source,
                created_at: row.created_at,
                updated_at: row.updated_at,
                last_login: row.last_login,
                is_admin: row.is_admin,
                password_hash: row.password_hash,
                external_user_id: row.external_user_id,
                payment_provider_id: row.payment_provider_id,
                is_deleted: row.is_deleted,
                is_internal: row.is_internal,
                batch_notifications_enabled: row.batch_notifications_enabled,
                first_batch_email_sent: row.first_batch_email_sent,
                low_balance_notification_sent: row.low_balance_notification_sent,
                low_balance_threshold: row.low_balance_threshold,
                user_type: row.user_type,
            };

            let roles = row.roles.unwrap_or_default();

            result.insert(user.id, UserDBResponse::from((roles, user)));
        }

        Ok(result)
    }
    #[instrument(skip(self, filter), fields(limit = filter.limit, skip = filter.skip, search = filter.search), err)]
    async fn list(&mut self, filter: &Self::Filter) -> Result<Vec<Self::Response>> {
        use sqlx::QueryBuilder;

        let mut query = QueryBuilder::new(
            "SELECT * FROM users WHERE id != '00000000-0000-0000-0000-000000000000' AND is_deleted = false AND user_type = ",
        );
        query.push_bind(filter.user_type.clone());

        // Add search filter if specified (case-insensitive substring match on display_name, username, or email)
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

        let users = query.build_query_as::<User>().fetch_all(&mut *self.db).await?;

        let mut tx = self.db.begin().await?;

        let mut result = Vec::new();
        for user in users {
            // Get roles for this user
            let roles = sqlx::query!("SELECT role as \"role: Role\" FROM user_roles WHERE user_id = $1", user.id)
                .fetch_all(&mut *tx)
                .await?;

            let roles: Vec<Role> = roles.into_iter().map(|r| r.role).collect();

            result.push(UserDBResponse::from((roles, user)));
        }
        tx.commit().await?;
        Ok(result)
    }

    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&id)), err)]
    async fn delete(&mut self, id: Self::Id) -> Result<bool> {
        // Soft delete with GDPR-compliant data scrubbing
        // We scrub all personal information but keep the record for referential integrity
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
                password_hash = NULL,
                external_user_id = NULL,
                payment_provider_id = NULL,
                is_deleted = true,
                updated_at = NOW()
            WHERE id = $3 AND is_deleted = false
            "#,
            scrubbed_email,
            scrubbed_username,
            id
        )
        .execute(&mut *self.db)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    #[instrument(skip(self, request), fields(user_id = %abbrev_uuid(&id)), err)]
    async fn update(&mut self, id: Self::Id, request: &Self::UpdateRequest) -> Result<Self::Response> {
        // This update touches multiple tables, so regardless of the connection passed in, we still need a transaction.

        let user;
        {
            let mut tx = self.db.begin().await?;

            // Atomic update with conditional field updates
            user = sqlx::query_as!(
                User,
                r#"
            UPDATE users SET
                display_name = COALESCE($2, display_name),
                avatar_url = COALESCE($3, avatar_url),
                password_hash = COALESCE($4, password_hash),
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
            WHERE id = $1
            RETURNING *
            "#,
                id,
                request.display_name,
                request.avatar_url,
                request.password_hash,
                request.batch_notifications_enabled,
                request.low_balance_threshold.is_some() as bool,
                request.low_balance_threshold.flatten(),
            )
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| DbError::NotFound)?;

            // Handle role updates if provided
            if let Some(roles) = &request.roles {
                // Ensure StandardUser role is always present
                let mut updated_roles = roles.clone();
                if !updated_roles.contains(&Role::StandardUser) {
                    updated_roles.push(Role::StandardUser);
                }

                // Delete existing roles
                sqlx::query!("DELETE FROM user_roles WHERE user_id = $1", id)
                    .execute(&mut *tx)
                    .await?;

                // Insert new roles (with StandardUser guaranteed to be included)
                for role in &updated_roles {
                    sqlx::query!("INSERT INTO user_roles (user_id, role) VALUES ($1, $2)", id, role as &Role)
                        .execute(&mut *tx)
                        .await?;
                }
            }
            tx.commit().await?;
        }
        // Now that the transaction is committed, we continue using the original connection reference (self.db)

        // Get current roles for the response
        let roles = sqlx::query!("SELECT role as \"role: Role\" FROM user_roles WHERE user_id = $1", id)
            .fetch_all(&mut *self.db)
            .await?;

        let roles: Vec<Role> = roles.into_iter().map(|r| r.role).collect();

        Ok(UserDBResponse::from((roles, user)))
    }
}

impl<'c> Users<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    #[instrument(skip(self, filter), fields(search = filter.search), err)]
    pub async fn count(&mut self, filter: &UserFilter) -> Result<i64> {
        use sqlx::QueryBuilder;

        let mut query = QueryBuilder::new(
            "SELECT COUNT(*) FROM users WHERE id != '00000000-0000-0000-0000-000000000000' AND is_deleted = false AND user_type = ",
        );
        query.push_bind(filter.user_type.clone());

        // Add search filter if specified (case-insensitive substring match on display_name, username, or email)
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

    #[instrument(skip(self, email), err)]
    pub async fn get_user_by_email(&mut self, email: &str) -> Result<Option<UserDBResponse>> {
        let user = sqlx::query_as!(
            User,
            "SELECT * FROM users WHERE email = $1 AND id != '00000000-0000-0000-0000-000000000000' AND is_deleted = false",
            email
        )
        .fetch_optional(&mut *self.db)
        .await?;

        if let Some(user) = user {
            // Get roles for this user
            let roles = sqlx::query!("SELECT role as \"role: Role\" FROM user_roles WHERE user_id = $1", user.id)
                .fetch_all(&mut *self.db)
                .await?;

            let roles: Vec<Role> = roles.into_iter().map(|r| r.role).collect();

            Ok(Some(UserDBResponse::from((roles, user))))
        } else {
            Ok(None)
        }
    }

    #[instrument(skip(self, external_user_id), err)]
    pub async fn get_user_by_external_user_id(&mut self, external_user_id: &str) -> Result<Option<UserDBResponse>> {
        let user = sqlx::query_as!(
            User,
            "SELECT * FROM users WHERE external_user_id = $1 AND id != '00000000-0000-0000-0000-000000000000' AND is_deleted = false",
            external_user_id
        )
        .fetch_optional(&mut *self.db)
        .await?;

        if let Some(user) = user {
            // Get roles for this user
            let roles = sqlx::query!("SELECT role as \"role: Role\" FROM user_roles WHERE user_id = $1", user.id)
                .fetch_all(&mut *self.db)
                .await?;

            let roles: Vec<Role> = roles.into_iter().map(|r| r.role).collect();

            Ok(Some(UserDBResponse::from((roles, user))))
        } else {
            Ok(None)
        }
    }

    /// Update a user's email address
    #[instrument(skip(self, email), fields(user_id = %abbrev_uuid(&user_id)), err)]
    async fn update_user_email(&mut self, user_id: UserId, email: &str) -> Result<()> {
        sqlx::query!("UPDATE users SET email = $1 WHERE id = $2", email, user_id)
            .execute(&mut *self.db)
            .await?;
        Ok(())
    }

    /// Update a user's external_user_id
    #[instrument(skip(self, external_user_id), fields(user_id = %abbrev_uuid(&user_id)), err)]
    async fn update_user_external_id(&mut self, user_id: UserId, external_user_id: &str) -> Result<()> {
        sqlx::query!("UPDATE users SET external_user_id = $1 WHERE id = $2", external_user_id, user_id)
            .execute(&mut *self.db)
            .await?;
        Ok(())
    }

    /// Get or create a user for proxy header authentication.
    ///
    /// This method handles the complete proxy header auth flow:
    /// 1. Look up by external_user_id → if found, update email and groups
    /// 2. Fall back to email lookup (TEMPORARY migration support) → update external_user_id and groups
    /// 3. If not found, create new user
    ///
    /// Email is required for user creation. Groups are synced if provided (along with provider).
    ///
    /// Returns a tuple of (user, was_created) where was_created is true if a new user was created.
    #[instrument(skip(self, external_user_id, email, groups_and_provider, default_roles), err)]
    pub async fn get_or_create_proxy_header_user(
        &mut self,
        external_user_id: &str,
        email: &str,
        groups_and_provider: Option<(Vec<String>, &str)>,
        default_roles: &[Role],
    ) -> Result<(UserDBResponse, bool)> {
        tracing::trace!(
            "Starting get_or_create_proxy_header_user for external_user_id: {}",
            external_user_id
        );

        // Acquire advisory lock to prevent concurrent creation of same user
        // Lock is automatically released when transaction commits/rolls back
        // Use PostgreSQL's hashtext function for deterministic hashing across replicas
        sqlx::query!("SELECT pg_advisory_xact_lock(hashtext($1))", external_user_id)
            .execute(&mut *self.db)
            .await?;
        tracing::trace!("Acquired advisory lock for external_user_id");

        let (user, was_created) = 'user_lookup: {
            // Look up by external_user_id
            if let Some(mut user) = self.get_user_by_external_user_id(external_user_id).await? {
                tracing::debug!("Found existing user by external_user_id");
                tracing::trace!("Found user by external_user_id: {}", external_user_id);
                // Found by external_user_id - update email if needed
                if user.email != email {
                    tracing::debug!("Updating email for user {}", abbrev_uuid(&user.id));
                    tracing::trace!("Updating email from {} to {}", user.email, email);
                    self.update_user_email(user.id, email).await?;
                    user.email = email.to_string();
                }

                break 'user_lookup (user, false);
            }

            // external user id not found (might be NULL). Lookup by email for single header mode
            if let Some(mut user) = self.get_user_by_email(email).await? {
                tracing::debug!("Found existing user by email");
                tracing::trace!("Found user by email: {}", email);
                // Found by email - check if we should use this user or create a new one
                if let Some(existing_external_id) = &user.external_user_id {
                    tracing::debug!("User {} has existing external_user_id set", abbrev_uuid(&user.id));
                    tracing::trace!("Existing external_user_id: {}", existing_external_id);
                    if existing_external_id == external_user_id {
                        tracing::debug!("External user ID matches for user {}, using existing user", abbrev_uuid(&user.id));
                        // Exact match - use this user
                        break 'user_lookup (user, false);
                    }
                    tracing::debug!("External user ID mismatch for user {}, creating new user", abbrev_uuid(&user.id));
                    // External user ID mismatch - this is a different federated identity with the same email
                    // Skip this user and fall through to create a new one
                } else {
                    // No external_user_id set - check if we should backfill

                    // Skip backfill if external_user_id == email (backwards compatibility mode)
                    // This happens when proxy sends single header and new code falls back to using it for both
                    // We want to wait until proxy sends separate headers before backfilling
                    if external_user_id == email {
                        tracing::debug!(
                            "External user ID equals email for user {}, skipping backfill",
                            abbrev_uuid(&user.id)
                        );
                        // Backwards compatibility mode - use this user but don't backfill yet
                        break 'user_lookup (user, false);
                    }
                    tracing::debug!("Backfilling external_user_id for user {}", abbrev_uuid(&user.id));
                    tracing::trace!("Backfilling external_user_id to {}", external_user_id);

                    // Backfill external_user_id for this existing user
                    self.update_user_external_id(user.id, external_user_id).await?;
                    user.external_user_id = Some(external_user_id.to_string());

                    break 'user_lookup (user, false);
                }
            }

            // User not found by either email or id, create new user
            tracing::debug!("Creating new user via proxy header auth");
            tracing::trace!(
                "No existing user found for external_user_id: {} and email: {}, creating new user",
                external_user_id,
                email
            );
            let display_name = crate::auth::utils::generate_random_display_name();
            tracing::debug!("Generated display name: {}", display_name);

            let create_request = UserCreateDBRequest {
                username: external_user_id.to_string(),
                email: email.to_string(),
                display_name: Some(display_name),
                avatar_url: None,
                is_admin: false,
                roles: default_roles.to_vec(),
                auth_source: "proxy-header".to_string(),
                password_hash: None,
                external_user_id: Some(external_user_id.to_string()),
            };

            let created_user = self.create(&create_request).await?;
            (created_user, true)
        };

        // Sync groups once at the end, regardless of which path we took
        if let Some((groups, provider)) = groups_and_provider {
            let mut group_repo = Groups::new(&mut *self.db);
            group_repo
                .sync_groups_with_sso(
                    user.id,
                    groups,
                    provider,
                    &format!("A group provisioned by the {provider} SSO source."),
                )
                .await?;
        }

        // Note: Hidden API keys for batch and playground are pre-created by the create() method
        // to avoid race condition with onwards sync

        Ok((user, was_created))
    }

    /// Mark that the first-batch welcome email has been sent for a user.
    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn mark_first_batch_email_sent(&mut self, user_id: UserId) -> Result<()> {
        sqlx::query!("UPDATE users SET first_batch_email_sent = true WHERE id = $1", user_id)
            .execute(&mut *self.db)
            .await?;
        Ok(())
    }

    /// Get users whose balance is below their configured threshold and haven't been notified yet.
    ///
    /// Uses the checkpoint+delta CTE to calculate balance efficiently.
    /// Only includes users with low_balance_threshold set (non-NULL = opted in).
    /// Clear recovered notification flags and return users needing low-balance notifications.
    ///
    /// In a single query:
    /// 1. Computes balance for all users with a threshold set
    /// 2. Clears `low_balance_notification_sent` for users whose balance recovered above threshold
    /// 3. Returns users whose balance is below threshold and haven't been notified yet
    #[instrument(skip(self), err)]
    pub async fn poll_low_balance_users(&mut self) -> Result<Vec<LowBalanceUser>> {
        // Clear recovered users and fetch low-balance users in one round-trip.
        // Uses the cached checkpoint balance (not the full delta recalculation) — good enough
        // for notification thresholds and avoids expensive per-tick aggregation.
        let rows = sqlx::query_as!(
            LowBalanceUser,
            r#"
            WITH clear_recovered AS (
                UPDATE users u
                SET low_balance_notification_sent = false
                FROM user_balance_checkpoints c
                WHERE u.id = c.user_id
                  AND u.low_balance_notification_sent = true
                  AND u.low_balance_threshold IS NOT NULL
                  AND c.balance >= u.low_balance_threshold
            )
            SELECT u.id, u.email, u.username, u.display_name, c.balance
            FROM users u
            JOIN user_balance_checkpoints c ON u.id = c.user_id
            WHERE u.id != '00000000-0000-0000-0000-000000000000'
              AND u.is_deleted = false
              AND u.low_balance_notification_sent = false
              AND u.low_balance_threshold IS NOT NULL
              AND c.balance < u.low_balance_threshold
            "#,
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows)
    }

    /// Mark that low-balance notifications have been sent for the given users.
    #[instrument(skip(self, user_ids), fields(count = user_ids.len()), err)]
    pub async fn mark_low_balance_notification_sent(&mut self, user_ids: &[UserId]) -> Result<()> {
        sqlx::query!("UPDATE users SET low_balance_notification_sent = true WHERE id = ANY($1)", user_ids)
            .execute(&mut *self.db)
            .await?;
        Ok(())
    }

    /// Set the payment provider ID for a user if it's not already set
    /// Returns true if the ID was updated, false if the user already had one or user not found
    #[instrument(skip(self), err)]
    pub async fn set_payment_provider_id_if_empty(&mut self, user_id: UserId, payment_provider_id: &str) -> Result<bool> {
        let rows_affected = sqlx::query!(
            "UPDATE users SET payment_provider_id = $1 WHERE id = $2 AND payment_provider_id IS NULL",
            payment_provider_id,
            user_id
        )
        .execute(&mut *self.db)
        .await?
        .rows_affected();

        Ok(rows_affected > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::super::repository::Repository;
    use super::*;
    use crate::api::models::users::{Role, UserCreate};
    use crate::db::handlers::credits::Credits;
    use crate::db::models::credits::CreditTransactionCreateDBRequest;
    use rust_decimal::Decimal;
    use sqlx::PgPool;
    use std::str::FromStr;

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_user(pool: PgPool) {
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Users::new(&mut conn);

        let user_create = UserCreateDBRequest::from(UserCreate {
            username: "testuser".to_string(),
            email: "test@example.com".to_string(),
            display_name: Some("Test User".to_string()),
            avatar_url: None,
            roles: vec![Role::StandardUser],
        });

        let result = repo.create(&user_create).await;
        assert!(result.is_ok());

        let user = result.unwrap();
        assert_eq!(user.username, "testuser");
        assert_eq!(user.email, "test@example.com");
        assert_eq!(user.display_name, Some("Test User".to_string()));
        assert_eq!(user.roles, vec![Role::StandardUser]);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_user_by_email(pool: PgPool) {
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Users::new(&mut conn);

        let user_create = UserCreateDBRequest::from(UserCreate {
            username: "emailuser".to_string(),
            email: "email@example.com".to_string(),
            display_name: None,
            avatar_url: None,
            roles: vec![Role::StandardUser],
        });

        let created_user = repo.create(&user_create).await.unwrap();

        let found_user = repo.get_user_by_email("email@example.com").await.unwrap();
        assert!(found_user.is_some());

        let found_user = found_user.unwrap();
        assert_eq!(found_user.id, created_user.id);
        assert_eq!(found_user.username, "emailuser");
        assert_eq!(found_user.roles, vec![Role::StandardUser]);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_system_user(pool: PgPool) {
        let mut conn = pool.acquire().await.unwrap();
        let admin_user = crate::test::utils::get_system_user(&mut conn).await;
        assert_eq!(admin_user.username, "system");
        assert_eq!(admin_user.email, "system@internal");
        assert_eq!(admin_user.id.to_string(), "00000000-0000-0000-0000-000000000000");
        assert!(admin_user.is_admin);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_user_roles_always_includes_standard_user(pool: PgPool) {
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Users::new(&mut conn);

        // Create a user with multiple roles including StandardUser
        let user_create = UserCreateDBRequest::from(UserCreate {
            username: "roleuser".to_string(),
            email: "roleuser@example.com".to_string(),
            display_name: None,
            avatar_url: None,
            roles: vec![Role::StandardUser, Role::PlatformManager],
        });

        let created_user = repo.create(&user_create).await.unwrap();
        assert_eq!(created_user.roles.len(), 2);
        assert!(created_user.roles.contains(&Role::StandardUser));
        assert!(created_user.roles.contains(&Role::PlatformManager));

        // Try to update roles to only RequestViewer (without StandardUser)
        let update_request = UserUpdateDBRequest {
            display_name: None,
            avatar_url: None,
            roles: Some(vec![Role::RequestViewer]), // Intentionally omitting StandardUser
            password_hash: None,
            batch_notifications_enabled: None,
            low_balance_threshold: None,
        };

        let updated_user = repo.update(created_user.id, &update_request).await.unwrap();

        // StandardUser should still be present, plus the new RequestViewer role
        assert_eq!(updated_user.roles.len(), 2);
        assert!(updated_user.roles.contains(&Role::StandardUser)); // Should be automatically added
        assert!(updated_user.roles.contains(&Role::RequestViewer));
        assert!(!updated_user.roles.contains(&Role::PlatformManager)); // Should be removed

        // Try to update with empty roles
        let update_request = UserUpdateDBRequest {
            display_name: None,
            avatar_url: None,
            roles: Some(vec![]), // Empty roles
            password_hash: None,
            batch_notifications_enabled: None,
            low_balance_threshold: None,
        };

        let updated_user = repo.update(created_user.id, &update_request).await.unwrap();

        // StandardUser should still be present
        assert_eq!(updated_user.roles.len(), 1);
        assert!(updated_user.roles.contains(&Role::StandardUser)); // Should be automatically added
    }

    /// Helper: create a user, set their threshold, grant credits, and refresh checkpoint.
    async fn create_user_with_balance(pool: &PgPool, balance: &str, threshold: Option<f32>) -> UserId {
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Users::new(&mut conn);

        let user_create = UserCreateDBRequest::from(UserCreate {
            username: format!("lowbal_{}", Uuid::new_v4().simple()),
            email: format!("lowbal_{}@example.com", Uuid::new_v4().simple()),
            display_name: Some("Low Balance Test".to_string()),
            avatar_url: None,
            roles: vec![Role::StandardUser],
        });
        let user = repo.create(&user_create).await.unwrap();

        // Set threshold if provided
        if threshold.is_some() {
            let update = UserUpdateDBRequest {
                display_name: None,
                avatar_url: None,
                roles: None,
                password_hash: None,
                batch_notifications_enabled: None,
                low_balance_threshold: Some(threshold),
            };
            repo.update(user.id, &update).await.unwrap();
        }

        // Grant credits and refresh checkpoint so poll_low_balance_users can see it
        let amount = Decimal::from_str(balance).unwrap();
        if amount > Decimal::ZERO {
            drop(conn);
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            let grant = CreditTransactionCreateDBRequest::admin_grant(user.id, user.id, amount, None);
            credits.create_transaction(&grant).await.unwrap();
            credits.refresh_checkpoint(user.id).await.unwrap();
        }

        user.id
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_poll_low_balance_skips_users_without_threshold(pool: PgPool) {
        // User with no threshold set should never appear
        create_user_with_balance(&pool, "1.00", None).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut users = Users::new(&mut conn);
        let low = users.poll_low_balance_users().await.unwrap();
        assert!(low.is_empty());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_poll_low_balance_returns_user_below_threshold(pool: PgPool) {
        let user_id = create_user_with_balance(&pool, "1.50", Some(2.0)).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut users = Users::new(&mut conn);
        let low = users.poll_low_balance_users().await.unwrap();
        assert_eq!(low.len(), 1);
        assert_eq!(low[0].id, user_id);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_poll_low_balance_skips_user_above_threshold(pool: PgPool) {
        create_user_with_balance(&pool, "10.00", Some(2.0)).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut users = Users::new(&mut conn);
        let low = users.poll_low_balance_users().await.unwrap();
        assert!(low.is_empty());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_poll_low_balance_skips_already_notified(pool: PgPool) {
        let user_id = create_user_with_balance(&pool, "1.00", Some(2.0)).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut users = Users::new(&mut conn);

        // First poll: user appears
        let low = users.poll_low_balance_users().await.unwrap();
        assert_eq!(low.len(), 1);

        // Mark as notified
        users.mark_low_balance_notification_sent(&[user_id]).await.unwrap();

        // Second poll: user should not appear
        let low = users.poll_low_balance_users().await.unwrap();
        assert!(low.is_empty());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_poll_low_balance_clears_flag_after_topup(pool: PgPool) {
        let user_id = create_user_with_balance(&pool, "1.00", Some(2.0)).await;

        // Poll and mark notified
        let mut conn = pool.acquire().await.unwrap();
        let mut users = Users::new(&mut conn);
        let low = users.poll_low_balance_users().await.unwrap();
        assert_eq!(low.len(), 1);
        users.mark_low_balance_notification_sent(&[user_id]).await.unwrap();
        drop(conn);

        // Topup: add credits and refresh checkpoint so balance > threshold
        let mut conn = pool.acquire().await.unwrap();
        let mut credits = Credits::new(&mut conn);
        let grant = CreditTransactionCreateDBRequest::admin_grant(user_id, user_id, Decimal::from_str("10.00").unwrap(), None);
        credits.create_transaction(&grant).await.unwrap();
        credits.refresh_checkpoint(user_id).await.unwrap();
        drop(conn);

        // Poll again: the clear_recovered CTE should reset the flag,
        // and the user should NOT appear (balance is now above threshold)
        let mut conn = pool.acquire().await.unwrap();
        let mut users = Users::new(&mut conn);
        let low = users.poll_low_balance_users().await.unwrap();
        assert!(low.is_empty());

        // Verify flag was actually cleared
        let user = users.get_by_id(user_id).await.unwrap().unwrap();
        assert!(!user.low_balance_notification_sent);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_poll_low_balance_full_cycle(pool: PgPool) {
        // 1. Create user with $100, threshold $2
        let user_id = create_user_with_balance(&pool, "100.00", Some(2.0)).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut users = Users::new(&mut conn);
        let low = users.poll_low_balance_users().await.unwrap();
        assert!(low.is_empty(), "User above threshold should not appear");
        drop(conn);

        // 2. Deduct $99 → balance $1 (below threshold)
        let mut conn = pool.acquire().await.unwrap();
        let mut credits = Credits::new(&mut conn);
        let deduct = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: crate::db::models::credits::CreditTransactionType::AdminRemoval,
            amount: Decimal::from_str("99.00").unwrap(),
            source_id: Uuid::new_v4().to_string(),
            description: None,
            fusillade_batch_id: None,
            api_key_id: None,
        };
        credits.create_transaction(&deduct).await.unwrap();
        credits.refresh_checkpoint(user_id).await.unwrap();
        drop(conn);

        // 3. Poll: user should appear
        let mut conn = pool.acquire().await.unwrap();
        let mut users = Users::new(&mut conn);
        let low = users.poll_low_balance_users().await.unwrap();
        assert_eq!(low.len(), 1, "User below threshold should appear");
        assert_eq!(low[0].id, user_id);

        // 4. Mark notified
        users.mark_low_balance_notification_sent(&[user_id]).await.unwrap();
        let low = users.poll_low_balance_users().await.unwrap();
        assert!(low.is_empty(), "Notified user should not appear again");
        drop(conn);

        // 5. Topup $50 → balance $51 (above threshold)
        let mut conn = pool.acquire().await.unwrap();
        let mut credits = Credits::new(&mut conn);
        let grant = CreditTransactionCreateDBRequest::admin_grant(user_id, user_id, Decimal::from_str("50.00").unwrap(), None);
        credits.create_transaction(&grant).await.unwrap();
        credits.refresh_checkpoint(user_id).await.unwrap();
        drop(conn);

        // 6. Poll: clear_recovered CTE resets the flag, user above threshold → not returned
        let mut conn = pool.acquire().await.unwrap();
        let mut users = Users::new(&mut conn);
        let low = users.poll_low_balance_users().await.unwrap();
        assert!(low.is_empty(), "Topped-up user should not appear");
        drop(conn);

        // 7. Deduct $50 → balance $1 again (below threshold)
        let mut conn = pool.acquire().await.unwrap();
        let mut credits = Credits::new(&mut conn);
        let deduct2 = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: crate::db::models::credits::CreditTransactionType::AdminRemoval,
            amount: Decimal::from_str("50.00").unwrap(),
            source_id: Uuid::new_v4().to_string(),
            description: None,
            fusillade_batch_id: None,
            api_key_id: None,
        };
        credits.create_transaction(&deduct2).await.unwrap();
        credits.refresh_checkpoint(user_id).await.unwrap();
        drop(conn);

        // 8. Poll: user should appear again (flag was cleared by step 6)
        let mut conn = pool.acquire().await.unwrap();
        let mut users = Users::new(&mut conn);
        let low = users.poll_low_balance_users().await.unwrap();
        assert_eq!(low.len(), 1, "User should be notifiable again after recovery + re-drop");
        assert_eq!(low[0].id, user_id);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_poll_low_balance_negative_balance(pool: PgPool) {
        // User with negative balance should still be returned
        let user_id = create_user_with_balance(&pool, "5.00", Some(2.0)).await;

        // Deduct more than the balance
        let mut conn = pool.acquire().await.unwrap();
        let mut credits = Credits::new(&mut conn);
        let deduct = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: crate::db::models::credits::CreditTransactionType::AdminRemoval,
            amount: Decimal::from_str("10.00").unwrap(),
            source_id: Uuid::new_v4().to_string(),
            description: None,
            fusillade_batch_id: None,
            api_key_id: None,
        };
        credits.create_transaction(&deduct).await.unwrap();
        credits.refresh_checkpoint(user_id).await.unwrap();
        drop(conn);

        let mut conn = pool.acquire().await.unwrap();
        let mut users = Users::new(&mut conn);
        let low = users.poll_low_balance_users().await.unwrap();
        assert_eq!(low.len(), 1);
        assert_eq!(low[0].id, user_id);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_low_balance_threshold_resets_flag(pool: PgPool) {
        let user_id = create_user_with_balance(&pool, "1.00", Some(2.0)).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut users = Users::new(&mut conn);

        // Trigger notification
        let low = users.poll_low_balance_users().await.unwrap();
        assert_eq!(low.len(), 1);
        users.mark_low_balance_notification_sent(&[user_id]).await.unwrap();

        // Update threshold — should reset the flag so user can be re-notified at new level
        let update = UserUpdateDBRequest {
            display_name: None,
            avatar_url: None,
            roles: None,
            password_hash: None,
            batch_notifications_enabled: None,
            low_balance_threshold: Some(Some(5.0)),
        };
        let updated = users.update(user_id, &update).await.unwrap();
        assert!(!updated.low_balance_notification_sent);
        assert_eq!(updated.low_balance_threshold, Some(5.0));

        // Poll again: user should appear at new threshold
        let low = users.poll_low_balance_users().await.unwrap();
        assert_eq!(low.len(), 1);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_users_excludes_organizations(pool: PgPool) {
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Users::new(&mut conn);

        // Create a regular user
        let user_create = UserCreateDBRequest::from(UserCreate {
            username: "individual".to_string(),
            email: "individual@example.com".to_string(),
            display_name: Some("Individual User".to_string()),
            avatar_url: None,
            roles: vec![Role::StandardUser],
        });
        repo.create(&user_create).await.unwrap();

        // Create an organization user directly via SQL (since Users::create always creates individuals)
        sqlx::query!(
            "INSERT INTO users (id, username, email, auth_source, user_type) VALUES ($1, $2, $3, 'organization', 'organization')",
            uuid::Uuid::new_v4(),
            "acme-org",
            "billing@acme.example.com",
        )
        .execute(&pool)
        .await
        .unwrap();

        // List should only return individual users (plus any seeded system user)
        let filter = UserFilter::new(0, 100);
        let users = repo.list(&filter).await.unwrap();

        for u in &users {
            assert_eq!(u.user_type, "individual", "Organization users should not appear in list");
        }
        assert!(users.iter().any(|u| u.username == "individual"));
        assert!(!users.iter().any(|u| u.username == "acme-org"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_count_users_excludes_organizations(pool: PgPool) {
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Users::new(&mut conn);

        let initial_count = repo.count(&UserFilter::new(0, 100)).await.unwrap();

        // Create a regular user
        repo.create(&UserCreateDBRequest::from(UserCreate {
            username: "countuser".to_string(),
            email: "countuser@example.com".to_string(),
            display_name: None,
            avatar_url: None,
            roles: vec![Role::StandardUser],
        }))
        .await
        .unwrap();

        // Create an org user via raw SQL
        sqlx::query!(
            "INSERT INTO users (id, username, email, auth_source, user_type) VALUES ($1, $2, $3, 'organization', 'organization')",
            uuid::Uuid::new_v4(),
            "count-org",
            "count-org@example.com",
        )
        .execute(&pool)
        .await
        .unwrap();

        let new_count = repo.count(&UserFilter::new(0, 100)).await.unwrap();
        assert_eq!(new_count, initial_count + 1, "Count should increase by 1 (the individual), not 2");
    }
}
