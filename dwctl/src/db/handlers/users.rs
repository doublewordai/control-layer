//! Database repository for users.

use crate::types::{abbrev_uuid, UserId};
use crate::{
    api::models::users::Role,
    db::{
        errors::{DbError, Result},
        handlers::repository::Repository,
        models::users::{UserCreateDBRequest, UserDBResponse, UserUpdateDBRequest},
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
            INSERT INTO users (id, username, email, display_name, avatar_url, auth_source, is_admin, password_hash)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            RETURNING *
            "#,
            user_id,
            request.username,
            request.email,
            request.display_name,
            request.avatar_url,
            request.auth_source,
            request.is_admin,
            request.password_hash
        )
        .fetch_one(&mut *tx)
        .await?;

        // Insert roles
        for role in &request.roles {
            sqlx::query!("INSERT INTO user_roles (user_id, role) VALUES ($1, $2)", user_id, role as &Role)
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;

        Ok(UserDBResponse::from((request.roles.clone(), user)))
    }

    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&id)), err)]
    async fn get_by_id(&mut self, id: Self::Id) -> Result<Option<Self::Response>> {
        let mut tx = self.db.begin().await?;
        let user = sqlx::query_as!(
            User,
            "SELECT * FROM users WHERE id = $1 AND id != '00000000-0000-0000-0000-000000000000'",
            id
        )
        .fetch_optional(&mut *tx)
        .await?;

        if let Some(user) = user {
            // Get roles for this user
            let roles = sqlx::query!("SELECT role as \"role: Role\" FROM user_roles WHERE user_id = $1", id)
                .fetch_all(&mut *tx)
                .await?;

            let roles: Vec<Role> = roles.into_iter().map(|r| r.role).collect();

            // This is a read operation, but we still need to commit the transaction to properly release database resources and close the transaction cleanly.
            tx.commit().await?;

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

        let mut tx = self.db.begin().await?;

        let users = sqlx::query_as!(
            User,
            "SELECT * FROM users WHERE id = ANY($1) AND id != '00000000-0000-0000-0000-000000000000'",
            ids.as_slice()
        )
        .fetch_all(&mut *tx)
        .await?;

        let mut result = std::collections::HashMap::new();

        for user in users {
            // Get roles for each user
            let roles = sqlx::query!("SELECT role as \"role: Role\" FROM user_roles WHERE user_id = $1", user.id)
                .fetch_all(&mut *tx)
                .await?;

            let roles: Vec<Role> = roles.into_iter().map(|r| r.role).collect();

            result.insert(user.id, UserDBResponse::from((roles, user)));
        }
        tx.commit().await?;

        Ok(result)
    }
    #[instrument(skip(self, filter), fields(limit = filter.limit, skip = filter.skip), err)]
    async fn list(&mut self, filter: &Self::Filter) -> Result<Vec<Self::Response>> {
        let users = sqlx::query_as!(
            User,
            "SELECT * FROM users WHERE id != '00000000-0000-0000-0000-000000000000' ORDER BY created_at DESC LIMIT $1 OFFSET $2",
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
        let result = sqlx::query!("DELETE FROM users WHERE id = $1", id).execute(&mut *self.db).await?;

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

    #[instrument(skip(self, email), err)]
    pub async fn get_user_by_email(&mut self, email: &str) -> Result<Option<UserDBResponse>> {
        let user = sqlx::query_as!(
            User,
            "SELECT * FROM users WHERE email = $1 AND id != '00000000-0000-0000-0000-000000000000'",
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
