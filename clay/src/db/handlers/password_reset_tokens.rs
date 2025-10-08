use std::collections::HashMap;

use chrono::Utc;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    auth::password,
    config::Config,
    db::{
        errors::{DbError, Result},
        handlers::repository::Repository,
        models::password_reset_tokens::{
            PasswordResetToken, PasswordResetTokenCreateRequest, PasswordResetTokenFilter, PasswordResetTokenResponse,
            PasswordResetTokenUpdateRequest,
        },
    },
    types::UserId,
};

pub struct PasswordResetTokens<'c> {
    db: &'c mut PgConnection,
}

#[async_trait::async_trait]
impl<'c> Repository for PasswordResetTokens<'c> {
    type CreateRequest = PasswordResetTokenCreateRequest;
    type UpdateRequest = PasswordResetTokenUpdateRequest;
    type Response = PasswordResetTokenResponse;
    type Id = Uuid;
    type Filter = PasswordResetTokenFilter;

    async fn create(&mut self, request: &Self::CreateRequest) -> Result<Self::Response> {
        let token_hash = password::hash_string(&request.raw_token).map_err(|e| DbError::Other(anyhow::anyhow!(e)))?;

        let token = sqlx::query_as!(
            PasswordResetToken,
            r#"
            INSERT INTO password_reset_tokens (user_id, token_hash, expires_at)
            VALUES ($1, $2, $3)
            RETURNING id, user_id, token_hash, expires_at, created_at, used_at
            "#,
            request.user_id,
            token_hash,
            request.expires_at
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(token)
    }

    async fn get_by_id(&mut self, id: Self::Id) -> Result<Option<Self::Response>> {
        let token = sqlx::query_as!(
            PasswordResetToken,
            "SELECT id, user_id, token_hash, expires_at, created_at, used_at FROM password_reset_tokens WHERE id = $1",
            id
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(token)
    }

    async fn get_bulk(&mut self, ids: Vec<Self::Id>) -> Result<HashMap<Self::Id, Self::Response>> {
        let tokens = sqlx::query_as!(
            PasswordResetToken,
            "SELECT id, user_id, token_hash, expires_at, created_at, used_at FROM password_reset_tokens WHERE id = ANY($1)",
            &ids
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(tokens.into_iter().map(|t| (t.id, t)).collect())
    }

    async fn list(&mut self, filter: &Self::Filter) -> Result<Vec<Self::Response>> {
        let mut query =
            String::from("SELECT id, user_id, token_hash, expires_at, created_at, used_at FROM password_reset_tokens WHERE 1=1");
        let mut conditions = Vec::new();

        if filter.user_id.is_some() {
            conditions.push(format!("user_id = ${}", conditions.len() + 1));
        }

        if !conditions.is_empty() {
            query.push_str(" AND ");
            query.push_str(&conditions.join(" AND "));
        }

        query.push_str(&format!(" ORDER BY created_at DESC LIMIT {} OFFSET {}", filter.limit, filter.skip));

        let mut sql_query = sqlx::query_as::<_, PasswordResetToken>(&query);

        if let Some(user_id) = filter.user_id {
            sql_query = sql_query.bind(user_id);
        }

        let tokens = sql_query.fetch_all(&mut *self.db).await?;
        Ok(tokens)
    }

    async fn update(&mut self, id: Self::Id, request: &Self::UpdateRequest) -> Result<Self::Response> {
        let token = sqlx::query_as!(
            PasswordResetToken,
            r#"
            UPDATE password_reset_tokens
            SET used_at = COALESCE($2, used_at)
            WHERE id = $1
            RETURNING id, user_id, token_hash, expires_at, created_at, used_at
            "#,
            id,
            request.used_at
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(token)
    }

    async fn delete(&mut self, id: Self::Id) -> Result<bool> {
        let result = sqlx::query!("DELETE FROM password_reset_tokens WHERE id = $1", id)
            .execute(&mut *self.db)
            .await?;

        Ok(result.rows_affected() > 0)
    }
}

impl<'c> PasswordResetTokens<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    /// Create a password reset token for a user
    /// TODO: why does this return the token, and then an object that wraps the token
    pub async fn create_for_user(&mut self, user_id: UserId, config: &Config) -> Result<(String, PasswordResetToken)> {
        let raw_token = password::generate_reset_token();
        let expires_at = Utc::now()
            + chrono::Duration::from_std(config.auth.native.email.password_reset.token_expiry).unwrap_or(chrono::Duration::minutes(30));

        let request = PasswordResetTokenCreateRequest {
            user_id,
            raw_token: raw_token.clone(),
            expires_at,
        };

        let token = self.create(&request).await?;
        Ok((raw_token, token))
    }

    /// Find a valid token by ID and verify the raw token
    pub async fn find_valid_token_by_id(&mut self, token_id: Uuid, raw_token: &str) -> Result<Option<PasswordResetToken>> {
        let token = self.get_by_id(token_id).await?;

        if let Some(token) = token {
            // Check if token is still valid (not expired and not used)
            if token.used_at.is_some() {
                return Ok(None);
            }
            if Utc::now() > token.expires_at {
                return Ok(None);
            }

            // Verify the raw token matches the hash
            match password::verify_string(raw_token, &token.token_hash) {
                Ok(true) => Ok(Some(token)),
                Ok(false) => Ok(None),
                Err(e) => {
                    tracing::error!("Token verification error for token {}: {:?}", token_id, e);
                    Ok(None)
                }
            }
        } else {
            Ok(None)
        }
    }

    /// Invalidate all tokens for a user
    pub async fn invalidate_for_user(&mut self, user_id: UserId) -> Result<u64> {
        let result = sqlx::query!(
            r#"
            UPDATE password_reset_tokens
            SET used_at = NOW()
            WHERE user_id = $1 AND used_at IS NULL
            "#,
            user_id
        )
        .execute(&mut *self.db)
        .await?;

        Ok(result.rows_affected())
    }
}
