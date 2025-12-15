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
}

impl UserFilter {
    pub fn new(skip: i64, limit: i64) -> Self {
        Self { skip, limit }
    }
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
                ARRAY_AGG(ur.role) FILTER (WHERE ur.role IS NOT NULL) as "roles: Vec<Role>"
            FROM users u
            LEFT JOIN user_roles ur ON ur.user_id = u.id
            WHERE u.id = $1 AND u.id != '00000000-0000-0000-0000-000000000000' AND u.is_deleted = false
            GROUP BY u.id, u.username, u.email, u.display_name, u.avatar_url, u.auth_source, u.created_at, u.updated_at, u.last_login, u.is_admin, u.password_hash, u.external_user_id, u.payment_provider_id, u.is_deleted
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
                ARRAY_AGG(ur.role) FILTER (WHERE ur.role IS NOT NULL) as "roles: Vec<Role>"
            FROM users u
            LEFT JOIN user_roles ur ON ur.user_id = u.id
            WHERE u.id = ANY($1) AND u.id != '00000000-0000-0000-0000-000000000000' AND u.is_deleted = false
            GROUP BY u.id, u.username, u.email, u.display_name, u.avatar_url, u.auth_source, u.created_at, u.updated_at, u.last_login, u.is_admin, u.password_hash, u.external_user_id, u.payment_provider_id, u.is_deleted
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
            };

            let roles = row.roles.unwrap_or_default();

            result.insert(user.id, UserDBResponse::from((roles, user)));
        }

        Ok(result)
    }
    #[instrument(skip(self, filter), fields(limit = filter.limit, skip = filter.skip), err)]
    async fn list(&mut self, filter: &Self::Filter) -> Result<Vec<Self::Response>> {
        let users = sqlx::query_as!(
            User,
            "SELECT * FROM users WHERE id != '00000000-0000-0000-0000-000000000000' AND is_deleted = false ORDER BY created_at DESC LIMIT $1 OFFSET $2",
            filter.limit,
            filter.skip
        )
        .fetch_all(&mut *self.db)
        .await?;

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
                updated_at = NOW()
            WHERE id = $1
            RETURNING *
            "#,
                id,
                request.display_name,
                request.avatar_url,
                request.password_hash,
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

    #[instrument(skip(self), err)]
    pub async fn count(&mut self) -> Result<i64> {
        // Note: This query counts the number of users excluding the admin user and deleted users.
        // It will likely need to filter by organization etc in future
        let count =
            sqlx::query_scalar!("SELECT COUNT(*) FROM users WHERE id != '00000000-0000-0000-0000-000000000000' AND is_deleted = false")
                .fetch_one(&mut *self.db)
                .await?;

        Ok(count.unwrap_or(0))
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

        // Pre-create hidden API key for inference to avoid race condition with onwards sync
        // This ensures the key exists before the user makes their first AI request
        let mut api_keys_repo = ApiKeys::new(&mut *self.db);
        api_keys_repo.get_or_create_hidden_key(user.id, ApiKeyPurpose::Inference).await?;

        Ok((user, was_created))
    }
}

#[cfg(test)]
mod tests {
    use super::super::repository::Repository;
    use super::*;
    use crate::api::models::users::{Role, UserCreate};
    use sqlx::PgPool;

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
        let admin_user = crate::test_utils::get_system_user(&mut conn).await;
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
        };

        let updated_user = repo.update(created_user.id, &update_request).await.unwrap();

        // StandardUser should still be present
        assert_eq!(updated_user.roles.len(), 1);
        assert!(updated_user.roles.contains(&Role::StandardUser)); // Should be automatically added
    }
}
