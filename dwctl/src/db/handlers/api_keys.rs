//! Database repository for API keys.

use std::collections::HashMap;

use crate::crypto::generate_api_key;
use crate::db::errors::DbError;
use crate::db::errors::Result;
use crate::db::handlers::repository::Repository;
use crate::db::models::api_keys::{ApiKeyCreateDBRequest, ApiKeyDBResponse, ApiKeyPurpose, ApiKeyUpdateDBRequest};
use crate::types::{ApiKeyId, DeploymentId, UserId, abbrev_uuid};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use sqlx::PgConnection;
use tracing::{debug, instrument};
use uuid::Uuid;

/// Filter for listing API keys
#[derive(Debug, Clone)]
pub struct ApiKeyFilter {
    pub skip: i64,
    pub limit: i64,
    pub user_id: Option<UserId>,
}

impl ApiKeyFilter {
    // Currently only constructed in testing.
    #[cfg(test)]
    pub fn new(skip: i64, limit: i64, user_id: Option<UserId>) -> Self {
        Self { skip, limit, user_id }
    }
}

// Database entity model
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
struct ApiKey {
    pub id: ApiKeyId,
    pub name: String,
    pub description: Option<String>,
    pub secret: String,
    pub purpose: String,
    pub user_id: UserId,
    pub created_at: DateTime<Utc>,
    pub last_used: Option<DateTime<Utc>>,
    pub requests_per_second: Option<f32>,
    pub burst_size: Option<i32>,
    pub hidden: bool,
}

impl From<(Vec<DeploymentId>, ApiKey)> for ApiKeyDBResponse {
    fn from((model_access, api_key): (Vec<DeploymentId>, ApiKey)) -> Self {
        // Parse purpose string to enum - default to Inference for backwards compatibility
        let purpose = api_key
            .purpose
            .parse::<serde_json::Value>()
            .ok()
            .and_then(|v| serde_json::from_value::<ApiKeyPurpose>(v).ok())
            .or(match api_key.purpose.as_str() {
                "platform" => Some(ApiKeyPurpose::Platform),
                "inference" => Some(ApiKeyPurpose::Inference),
                _ => None,
            })
            .unwrap_or(ApiKeyPurpose::Inference);

        Self {
            id: api_key.id,
            name: api_key.name,
            description: api_key.description,
            secret: api_key.secret,
            purpose,
            user_id: api_key.user_id,
            created_at: api_key.created_at,
            last_used: api_key.last_used,
            model_access,
            requests_per_second: api_key.requests_per_second,
            burst_size: api_key.burst_size,
        }
    }
}

pub struct ApiKeys<'c> {
    db: &'c mut PgConnection,
}

#[async_trait::async_trait]
impl<'c> Repository for ApiKeys<'c> {
    type CreateRequest = ApiKeyCreateDBRequest;
    type UpdateRequest = ApiKeyUpdateDBRequest;
    type Response = ApiKeyDBResponse;
    type Id = ApiKeyId;
    type Filter = ApiKeyFilter;

    #[instrument(skip(self, request), fields(name = %request.name), err)]
    async fn create(&mut self, request: &Self::CreateRequest) -> Result<Self::Response> {
        // Generate a secure API key
        let secret = generate_api_key();

        // Convert purpose enum to string for database
        let purpose_str = match request.purpose {
            ApiKeyPurpose::Platform => "platform",
            ApiKeyPurpose::Inference => "inference",
        };

        let api_key = sqlx::query_as!(
            ApiKey,
            r#"
            INSERT INTO api_keys (name, description, secret, purpose, user_id, requests_per_second, burst_size, hidden)
            VALUES ($1, $2, $3, $4, $5, $6, $7, false)
            RETURNING *
            "#,
            request.name,
            request.description,
            secret,
            purpose_str,
            request.user_id,
            request.requests_per_second,
            request.burst_size
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(ApiKeyDBResponse::from((self.get_api_key_deployments(api_key.id).await?, api_key)))
    }

    #[instrument(skip(self), fields(api_key_id = %abbrev_uuid(&id)), err)]
    async fn get_by_id(&mut self, id: Self::Id) -> Result<Option<Self::Response>> {
        let api_key = sqlx::query_as!(
            ApiKey,
            "SELECT id, name, description, secret, purpose, user_id, created_at, last_used, requests_per_second, burst_size, hidden FROM api_keys WHERE id = $1",
            id
        )
            .fetch_optional(&mut *self.db)
            .await?;

        match api_key {
            Some(key) => Ok(Some(ApiKeyDBResponse::from((self.get_api_key_deployments(key.id).await?, key)))),
            None => Ok(None),
        }
    }

    #[instrument(skip(self, ids), fields(count = ids.len()), err)]
    async fn get_bulk(&mut self, ids: Vec<Self::Id>) -> Result<HashMap<Self::Id, Self::Response>> {
        let api_keys = sqlx::query_as!(
            ApiKey,
            "SELECT id, name, description, secret, purpose, user_id, created_at, last_used, requests_per_second, burst_size, hidden FROM api_keys WHERE id = ANY($1)",
            &ids
        )
            .fetch_all(&mut *self.db)
            .await?;

        let mut responses = HashMap::new();
        for key in api_keys {
            let deployments = self.get_api_key_deployments(key.id).await?;
            responses.insert(key.id, ApiKeyDBResponse::from((deployments, key)));
        }
        Ok(responses)
    }

    #[instrument(skip(self, filter), fields(limit = filter.limit, skip = filter.skip), err)]
    async fn list(&mut self, filter: &Self::Filter) -> Result<Vec<Self::Response>> {
        let api_keys = if let Some(user_id) = filter.user_id {
            sqlx::query_as!(
                ApiKey,
                "SELECT id, name, description, secret, purpose, user_id, created_at, last_used, requests_per_second, burst_size, hidden FROM api_keys WHERE user_id = $1 AND hidden = false ORDER BY created_at DESC LIMIT $2 OFFSET $3",
                user_id,
                filter.limit,
                filter.skip
            )
            .fetch_all(&mut *self.db)
            .await?
        } else {
            sqlx::query_as!(
                ApiKey,
                "SELECT id, name, description, secret, purpose, user_id, created_at, last_used, requests_per_second, burst_size, hidden FROM api_keys WHERE hidden = false ORDER BY created_at DESC LIMIT $1 OFFSET $2",
                filter.limit,
                filter.skip,
            )
            .fetch_all(&mut *self.db)
            .await?
        };

        let mut responses = Vec::new();
        for key in api_keys {
            let deployments = self.get_api_key_deployments(key.id).await?;

            responses.push(ApiKeyDBResponse::from((deployments, key)));
        }
        Ok(responses)
    }

    #[instrument(skip(self), fields(api_key_id = %abbrev_uuid(&id)), err)]
    async fn delete(&mut self, id: Self::Id) -> Result<bool> {
        let result = sqlx::query!("DELETE FROM api_keys WHERE id = $1", id)
            .execute(&mut *self.db)
            .await?;

        Ok(result.rows_affected() > 0)
    }

    #[instrument(skip(self, request), fields(api_key_id = %abbrev_uuid(&id)), err)]
    async fn update(&mut self, id: Self::Id, request: &Self::UpdateRequest) -> Result<Self::Response> {
        // Atomic update with conditional field updates
        let api_key = sqlx::query_as!(
            ApiKey,
            r#"
            UPDATE api_keys
            SET
                name = COALESCE($2, name),
                description = CASE
                    WHEN $3::text IS NOT NULL THEN $3
                    ELSE description
                END,
                requests_per_second = CASE
                    WHEN $4::real IS NOT NULL THEN $4
                    ELSE requests_per_second
                END,
                burst_size = CASE
                    WHEN $5::integer IS NOT NULL THEN $5
                    ELSE burst_size
                END
            WHERE id = $1
            RETURNING *
            "#,
            id,
            request.name,
            request.description,
            request.requests_per_second.unwrap_or(None),
            request.burst_size.unwrap_or(None)
        )
        .fetch_optional(&mut *self.db)
        .await?
        .ok_or_else(|| DbError::NotFound)?;

        Ok(ApiKeyDBResponse::from((self.get_api_key_deployments(api_key.id).await?, api_key)))
    }
}

impl<'c> ApiKeys<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    /// Get the user ID associated with an API key secret
    ///
    /// Joins with users table to verify the key exists and user is valid.
    ///
    /// # Arguments
    /// * `secret` - The API key secret to look up
    ///
    /// # Returns
    /// Returns Some(user_id) if found, None if the key doesn't exist
    #[instrument(skip(self, secret), err)]
    pub async fn get_user_id_by_secret(&mut self, secret: &str) -> Result<Option<UserId>> {
        let user_id = sqlx::query_scalar!(
            r#"
            SELECT u.id
            FROM api_keys ak
            JOIN users u ON ak.user_id = u.id
            WHERE ak.secret = $1
            "#,
            secret
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(user_id)
    }

    /// Get or create a user-specific hidden API key for internal use
    ///
    /// Hidden API keys are automatically managed by the system and are not visible to users.
    /// They are used for proxying requests through admin_api and batch processing.
    ///
    /// # Arguments
    /// * `user_id` - The user ID to get or create a hidden key for
    /// * `purpose` - The purpose of the key (should be ApiKeyPurpose::Inference for proxy use)
    ///
    /// # Returns
    /// Returns the secret of the hidden API key
    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn get_or_create_hidden_key(&mut self, user_id: UserId, purpose: ApiKeyPurpose) -> Result<String> {
        // Convert purpose enum to string for database
        let purpose_str = match purpose {
            ApiKeyPurpose::Platform => "platform",
            ApiKeyPurpose::Inference => "inference",
        };

        // Try to get existing hidden key for this user and purpose
        let existing_key = sqlx::query_scalar!(
            r#"
            SELECT secret
            FROM api_keys
            WHERE user_id = $1 AND purpose = $2 AND hidden = true
            LIMIT 1
            "#,
            user_id,
            purpose_str
        )
        .fetch_optional(&mut *self.db)
        .await?;

        if let Some(secret) = existing_key {
            tracing::debug!("Using existing hidden API key for user");
            return Ok(secret);
        }

        // No existing key found, create a new one
        let secret = generate_api_key();
        let name = format!("Internal {} Key", purpose_str);
        let description = Some(format!(
            "Automatically managed internal API key for {} operations. Not visible to users.",
            purpose_str
        ));

        tracing::info!("Creating new hidden API key for user");

        sqlx::query!(
            r#"
            INSERT INTO api_keys (name, description, secret, purpose, user_id, hidden)
            VALUES ($1, $2, $3, $4, $5, true)
            "#,
            name,
            description,
            secret,
            purpose_str,
            user_id
        )
        .execute(&mut *self.db)
        .await?;

        Ok(secret)
    }

    /// Get specific deployment IDs that an API key has access to
    #[instrument(skip(self), fields(api_key_id = %abbrev_uuid(&api_key_id)), err)]
    async fn get_api_key_deployments(&mut self, api_key_id: ApiKeyId) -> Result<Vec<DeploymentId>> {
        let deployment_ids = sqlx::query_scalar!(
            r#"
            SELECT DISTINCT dg.deployment_id
            FROM user_groups ug
            INNER JOIN deployment_groups dg ON ug.group_id = dg.group_id
            INNER JOIN api_keys ak ON ug.user_id = ak.user_id
            WHERE ak.id = $1

            UNION

            SELECT DISTINCT dg.deployment_id
            FROM deployment_groups dg
            INNER JOIN api_keys ak ON dg.group_id = '00000000-0000-0000-0000-000000000000'
            WHERE ak.id = $1
            AND ak.user_id != '00000000-0000-0000-0000-000000000000'  -- Exclude system user
            "#,
            api_key_id
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(deployment_ids.into_iter().flatten().collect())
    }

    /// Get all API keys that can access the specified deployment with full response data
    /// Excludes API keys from users with insufficient credits (balance <= 0)
    #[instrument(skip(self), fields(deployment_id = %abbrev_uuid(&deployment_id)), err)]
    pub async fn get_api_keys_for_deployment_with_sufficient_credit(
        &mut self,
        deployment_id: DeploymentId,
    ) -> Result<Vec<ApiKeyDBResponse>> {
        let api_keys = sqlx::query_as!(
            ApiKey,
            r#"
            SELECT DISTINCT
                ak.id as "id!",
                ak.name as "name!",
                ak.description,
                ak.secret as "secret!",
                ak.purpose as "purpose!",
                ak.user_id as "user_id!",
                ak.created_at as "created_at!",
                ak.last_used,
                ak.requests_per_second,
                ak.burst_size,
                ak.hidden as "hidden!"
            FROM api_keys ak
            WHERE ak.user_id = $2  -- System user has access to all deployments

            UNION

            SELECT DISTINCT
                ak.id as "id!",
                ak.name as "name!",
                ak.description,
                ak.secret as "secret!",
                ak.purpose as "purpose!",
                ak.user_id as "user_id!",
                ak.created_at as "created_at!",
                ak.last_used,
                ak.requests_per_second,
                ak.burst_size,
                ak.hidden as "hidden!"
            FROM api_keys ak
            INNER JOIN user_groups ug ON ak.user_id = ug.user_id
            INNER JOIN deployment_groups dg ON ug.group_id = dg.group_id
            INNER JOIN deployed_models dm ON dg.deployment_id = dm.id
            WHERE dg.deployment_id = $1
            AND (
                ak.user_id = $2  -- System user always has access
                OR EXISTS (
                    -- User's latest transaction has positive balance
                    SELECT 1 FROM credits_transactions ct
                    WHERE ct.user_id = ak.user_id
                    AND ct.balance_after > 0
                    AND NOT EXISTS (
                        -- Make sure there's no newer transaction
                        SELECT 1 FROM credits_transactions ct2
                        WHERE ct2.user_id = ct.user_id
                        AND (ct2.created_at > ct.created_at OR (ct2.created_at = ct.created_at AND ct2.id > ct.id))
                    )
                )
                OR (
                    -- Free models are accessible to all users (zero balance OK)
                    -- A model is free if pricing is NULL OR both prices are 0
                    (dm.upstream_input_price_per_token IS NULL OR dm.upstream_input_price_per_token = 0)
                    AND (dm.upstream_output_price_per_token IS NULL OR dm.upstream_output_price_per_token = 0)
                )
            )

            UNION

            SELECT DISTINCT
                ak.id as "id!",
                ak.name as "name!",
                ak.description,
                ak.secret as "secret!",
                ak.purpose as "purpose!",
                ak.user_id as "user_id!",
                ak.created_at as "created_at!",
                ak.last_used,
                ak.requests_per_second,
                ak.burst_size,
                ak.hidden as "hidden!"
            FROM api_keys ak
            INNER JOIN deployment_groups dg ON dg.group_id = '00000000-0000-0000-0000-000000000000'
            INNER JOIN deployed_models dm ON dg.deployment_id = dm.id
            WHERE dg.deployment_id = $1
            AND ak.user_id != '00000000-0000-0000-0000-000000000000'  -- Exclude system user (already covered above)
            AND (
                ak.user_id = $2  -- System user always has access
                OR EXISTS (
                    -- User's latest transaction has positive balance
                    SELECT 1 FROM credits_transactions ct
                    WHERE ct.user_id = ak.user_id
                    AND ct.balance_after > 0
                    AND NOT EXISTS (
                        -- Make sure there's no newer transaction
                        SELECT 1 FROM credits_transactions ct2
                        WHERE ct2.user_id = ct.user_id
                        AND (ct2.created_at > ct.created_at OR (ct2.created_at = ct.created_at AND ct2.id > ct.id))
                    )
                )
                OR (
                    -- Free models are accessible to all users (zero balance OK)
                    -- A model is free if pricing is NULL OR both prices are 0
                    (dm.upstream_input_price_per_token IS NULL OR dm.upstream_input_price_per_token = 0)
                    AND (dm.upstream_output_price_per_token IS NULL OR dm.upstream_output_price_per_token = 0)
                )
            )
            "#,
            deployment_id,
            Uuid::nil() // System user ID
        )
        .fetch_all(&mut *self.db)
        .await?;

        // Log the number of API keys returned after credit filtering
        debug!(
            "Found {} API keys with access to deployment {} after credit filtering",
            api_keys.len(),
            abbrev_uuid(&deployment_id)
        );

        // Get all deployment access for these API keys in bulk
        let api_key_ids: Vec<ApiKeyId> = api_keys.iter().map(|ak| ak.id).collect();
        let deployment_access = if !api_key_ids.is_empty() {
            let deployment_data = sqlx::query!(
                r#"
                SELECT ak.id as api_key_id, dg.deployment_id
                FROM api_keys ak
                INNER JOIN user_groups ug ON ak.user_id = ug.user_id
                INNER JOIN deployment_groups dg ON ug.group_id = dg.group_id
                WHERE ak.id = ANY($1)

                UNION

                SELECT ak.id as api_key_id, dg.deployment_id
                FROM api_keys ak
                INNER JOIN deployment_groups dg ON dg.group_id = '00000000-0000-0000-0000-000000000000'
                WHERE ak.id = ANY($1)
                AND ak.user_id != '00000000-0000-0000-0000-000000000000'
                "#,
                &api_key_ids
            )
            .fetch_all(&mut *self.db)
            .await?;

            // Group deployment IDs by API key ID
            let mut access_map: HashMap<ApiKeyId, Vec<DeploymentId>> = HashMap::new();
            for row in deployment_data {
                if let (Some(api_key_id), Some(deployment_id)) = (row.api_key_id, row.deployment_id) {
                    access_map.entry(api_key_id).or_default().push(deployment_id);
                }
            }
            access_map
        } else {
            HashMap::new()
        };

        let mut results = Vec::new();
        for api_key in api_keys {
            let deployments = deployment_access.get(&api_key.id).cloned().unwrap_or_default();
            results.push(ApiKeyDBResponse::from((deployments, api_key)));
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::models::users::{Role, UserCreate},
        db::{
            handlers::{credits::Credits, deployments::Deployments, groups::Groups, users::Users},
            models::{
                credits::{CreditTransactionCreateDBRequest, CreditTransactionType},
                deployments::{DeploymentCreateDBRequest, DeploymentDBResponse},
                groups::{GroupCreateDBRequest, GroupDBResponse},
                users::UserCreateDBRequest,
            },
        },
        errors::Error,
        test_utils::get_test_endpoint_id,
    };

    use rust_decimal::Decimal;
    use sqlx::{Acquire, PgPool};

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_api_key(pool: PgPool) {
        let api_key;
        let userid;
        {
            let mut tx = pool.begin().await.unwrap();

            {
                let mut user_repo = Users::new(tx.acquire().await.unwrap());

                let user_create = UserCreateDBRequest::from(UserCreate {
                    username: "apiuser".to_string(),
                    email: "api@example.com".to_string(),
                    display_name: None,
                    avatar_url: None,
                    roles: vec![Role::StandardUser],
                });

                userid = user_repo.create(&user_create).await.unwrap().id;
            }
            {
                let mut api_repo = ApiKeys::new(tx.acquire().await.unwrap());

                let api_key_create = ApiKeyCreateDBRequest {
                    user_id: userid,
                    name: "Test API Key".to_string(),
                    description: Some("Test description".to_string()),
                    purpose: ApiKeyPurpose::Inference,
                    requests_per_second: None,
                    burst_size: None,
                };

                api_key = api_repo.create(&api_key_create).await.unwrap();
            }
            tx.commit().await.unwrap();
        }
        assert_eq!(api_key.name, "Test API Key");
        assert_eq!(api_key.user_id, userid);
        assert!(api_key.secret.starts_with("sk-"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_user_api_keys(pool: PgPool) {
        let user;
        // Use a scope here to make explicit that user_repo and api_repo can't exist when tx is done
        let mut tx = pool.begin().await.unwrap();
        {
            let mut user_repo = Users::new(tx.acquire().await.unwrap());

            let user_create = UserCreateDBRequest::from(UserCreate {
                username: "keysuser".to_string(),
                email: "keys@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });

            user = user_repo.create(&user_create).await.unwrap();
        }
        {
            let mut api_repo = ApiKeys::new(tx.acquire().await.unwrap());

            let key1 = ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Key 1".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Inference,
                requests_per_second: None,
                burst_size: None,
            };
            let key2 = ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Key 2".to_string(),
                description: Some("Key 2 description".to_string()),
                purpose: ApiKeyPurpose::Inference,
                requests_per_second: None,
                burst_size: None,
            };

            api_repo.create(&key1).await.unwrap();
            api_repo.create(&key2).await.unwrap();
        }
        tx.commit().await.unwrap();

        // Need a new repo after doing the create part - transaction has been closed
        let mut pool_conn = pool.acquire().await.unwrap();

        let mut api_repo = ApiKeys::new(&mut pool_conn);
        // Use the new filter-based list method
        let keys = api_repo
            .list(&ApiKeyFilter {
                skip: 0,
                limit: 100,
                user_id: Some(user.id),
            })
            .await
            .unwrap();

        assert_eq!(keys.len(), 2);
        assert!(keys.iter().any(|k| k.name == "Key 1"));
        assert!(keys.iter().any(|k| k.name == "Key 2"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_api_key(pool: PgPool) {
        let api_key;
        let mut tx = pool.begin().await.unwrap();
        let user;
        {
            let mut user_repo = Users::new(tx.acquire().await.unwrap());

            let user_create = UserCreateDBRequest::from(UserCreate {
                username: "deleteuser".to_string(),
                email: "delete@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });

            user = user_repo.create(&user_create).await.unwrap();
        }
        {
            let mut api_repo = ApiKeys::new(tx.acquire().await.unwrap());
            let api_key_create = ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Delete Me".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Inference,
                requests_per_second: None,
                burst_size: None,
            };
            api_key = api_repo.create(&api_key_create).await.unwrap();
        }
        tx.commit().await.unwrap();
        let mut pool_conn = pool.acquire().await.unwrap();

        let mut api_repo = ApiKeys::new(&mut pool_conn);
        let deleted = api_repo.delete(api_key.id).await.unwrap();
        assert!(deleted);

        let found_key = api_repo.get_by_id(api_key.id).await.unwrap();
        assert!(found_key.is_none());
    }

    #[sqlx::test]
    async fn test_repository_trait_methods(pool: PgPool) {
        let api_key;
        let user;
        let mut tx = pool.begin().await.unwrap();

        {
            let mut user_repo = Users::new(tx.acquire().await.unwrap());

            let user_create = UserCreateDBRequest::from(UserCreate {
                username: "traituser".to_string(),
                email: "trait@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });

            user = user_repo.create(&user_create).await.unwrap();
        }
        {
            let mut api_repo = ApiKeys::new(tx.acquire().await.unwrap());

            let api_key_create = ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Trait Test Key".to_string(),
                description: Some("Test trait description".to_string()),
                purpose: ApiKeyPurpose::Inference,
                requests_per_second: None,
                burst_size: None,
            };

            // Test create via Repository trait
            api_key = api_repo.create(&api_key_create).await.unwrap();
            assert_eq!(api_key.name, "Trait Test Key");
        }

        tx.commit().await.unwrap();
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut api_repo = ApiKeys::new(&mut pool_conn);

        // Test get_by_id via Repository trait
        let found_key = api_repo.get_by_id(api_key.id).await.unwrap();
        assert!(found_key.is_some());
        assert_eq!(found_key.unwrap().name, "Trait Test Key");

        // Test update via Repository trait
        let update = ApiKeyUpdateDBRequest {
            name: Some("Updated Key Name".to_string()),
            description: Some("Updated description".to_string()),
            requests_per_second: None,
            burst_size: None,
        };
        let updated_key = api_repo.update(api_key.id, &update).await.unwrap();
        assert_eq!(updated_key.name, "Updated Key Name");

        // Test list via Repository trait
        let keys = api_repo.list(&ApiKeyFilter::new(0, 10, None)).await.unwrap();
        assert!(!keys.is_empty());
        assert!(keys.iter().any(|k| k.name == "Updated Key Name"));
    }

    // Tests for group-based API key access control

    #[sqlx::test]
    async fn test_api_key_access_through_group_membership(pool: PgPool) {
        let mut tx = pool.begin().await.unwrap();
        let admin_user;
        let user;
        let deployment;
        let api_key;
        let group;
        {
            let mut user_repo = Users::new(tx.acquire().await.unwrap());

            // Create admin user
            let admin_create = UserCreateDBRequest::from(UserCreate {
                username: "admin".to_string(),
                email: "admin@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::PlatformManager],
            });
            admin_user = user_repo.create(&admin_create).await.unwrap();

            // Create regular user
            let user_create = UserCreateDBRequest::from(UserCreate {
                username: "testuser".to_string(),
                email: "test@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });
            user = user_repo.create(&user_create).await.unwrap();
        }
        {
            let mut group_tx = tx.begin().await.unwrap();
            let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());
            // Create a group
            let group_create = GroupCreateDBRequest {
                name: "Test Group".to_string(),
                description: Some("Test group for API key access".to_string()),
                created_by: admin_user.id,
            };
            group = group_repo.create(&group_create).await.unwrap();
            group_tx.commit().await.unwrap();
        }
        // Create inference endpoint for deployments
        let config = crate::test_utils::create_test_config();
        crate::seed_database(&config.model_sources, &pool).await.unwrap();

        // Get a valid endpoint ID
        let test_endpoint_id = get_test_endpoint_id(&pool).await;
        // Create a deployment
        let mut deployment_create = DeploymentCreateDBRequest::builder()
            .created_by(admin_user.id)
            .model_name("test-model".to_string())
            .alias("test-alias".to_string())
            .build();
        deployment_create.hosted_on = test_endpoint_id;

        {
            let mut deployment_tx = tx.begin().await.unwrap();
            let mut deployment_repo = Deployments::new(deployment_tx.acquire().await.unwrap());

            deployment = deployment_repo.create(&deployment_create).await.unwrap();
            deployment_tx.commit().await.unwrap();
        }
        {
            let mut group_tx = tx.begin().await.unwrap();
            let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());
            // Add user to group first
            group_repo.add_user_to_group(user.id, group.id).await.unwrap();

            // Add deployment to group
            group_repo
                .add_deployment_to_group(deployment.id, group.id, admin_user.id)
                .await
                .unwrap();

            group_tx.commit().await.unwrap();
        }

        {
            let mut api_key_repo = ApiKeys::new(tx.acquire().await.unwrap());

            // NOW create an API key - it will automatically get access to deployments the user's groups can access
            let api_key_create = ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Test API Key".to_string(),
                description: Some("API key for testing group access".to_string()),
                purpose: ApiKeyPurpose::Inference,
                requests_per_second: None,
                burst_size: None,
            };
            api_key = api_key_repo.create(&api_key_create).await.unwrap();
        }
        // Commit all of that to the db so we can read it out to run checks.
        tx.commit().await.unwrap();

        // Credit standard users so they can access deployments
        {
            let mut credit_conn = pool.acquire().await.unwrap();
            let mut credit_repo = Credits::new(&mut credit_conn);
            credit_repo
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: user.id,
                    amount: Decimal::from(1000),
                    source_id: user.id.to_string(),
                    transaction_type: CreditTransactionType::AdminGrant,
                    description: Some("Initial credit for testing".to_string()),
                })
                .await
                .unwrap();
        }

        // Can't use our transaction now as its been commited, but can just make a new repo with a connection from the pool.
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut api_key_repo = ApiKeys::new(&mut pool_conn);

        // API key should have access to the deployment
        let keys_for_deployment = api_key_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment.id)
            .await
            .unwrap();
        assert!(keys_for_deployment.iter().any(|k| k.secret == api_key.secret));

        // API key should show the deployment in model_access
        assert!(api_key.model_access.contains(&deployment.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_api_key_loses_access_when_removed_from_group(pool: PgPool) {
        let mut tx = pool.begin().await.unwrap();
        let admin_user;
        let user;
        let deployment;
        let api_key;
        let group;

        {
            let mut user_repo = Users::new(tx.acquire().await.unwrap());

            // Create admin user
            let admin_create = UserCreateDBRequest::from(UserCreate {
                username: "admin".to_string(),
                email: "admin@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::PlatformManager],
            });
            admin_user = user_repo.create(&admin_create).await.unwrap();

            // Create regular user
            let user_create = UserCreateDBRequest::from(UserCreate {
                username: "testuser".to_string(),
                email: "test@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });
            user = user_repo.create(&user_create).await.unwrap();
        }

        // Create inference endpoint for deployments
        let config = crate::test_utils::create_test_config();
        crate::seed_database(&config.model_sources, &pool).await.unwrap();

        {
            let mut group_tx = tx.begin().await.unwrap();
            let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());

            // Create group
            let group_create = GroupCreateDBRequest {
                name: "Test Group".to_string(),
                description: Some("Test group for access removal".to_string()),
                created_by: admin_user.id,
            };
            group = group_repo.create(&group_create).await.unwrap();
            group_tx.commit().await.unwrap();
        }

        let test_endpoint_id = get_test_endpoint_id(&pool).await;
        let mut deployment_create = DeploymentCreateDBRequest::builder()
            .created_by(admin_user.id)
            .model_name("test-model".to_string())
            .alias("test-alias".to_string())
            .build();
        deployment_create.hosted_on = test_endpoint_id;

        {
            let mut deployment_tx = tx.begin().await.unwrap();
            let mut deployment_repo = Deployments::new(deployment_tx.acquire().await.unwrap());
            deployment = deployment_repo.create(&deployment_create).await.unwrap();
            deployment_tx.commit().await.unwrap();
        }

        {
            let mut group_tx = tx.begin().await.unwrap();
            let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());

            // Set up access: user in group, deployment in group
            group_repo.add_user_to_group(user.id, group.id).await.unwrap();
            group_repo
                .add_deployment_to_group(deployment.id, group.id, admin_user.id)
                .await
                .unwrap();
            group_tx.commit().await.unwrap();
        }

        {
            let mut api_key_repo = ApiKeys::new(tx.acquire().await.unwrap());

            // Create API key AFTER group relationships are established
            let api_key_create = ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Test API Key".to_string(),
                description: Some("API key for testing access removal".to_string()),
                purpose: ApiKeyPurpose::Inference,
                requests_per_second: None,
                burst_size: None,
            };
            api_key = api_key_repo.create(&api_key_create).await.unwrap();
        }

        // Commit transaction and create new connection for assertions
        tx.commit().await.unwrap();

        // Credit standard users so they can access deployments
        {
            let mut credit_conn = pool.acquire().await.unwrap();
            let mut credit_repo = Credits::new(&mut credit_conn);
            credit_repo
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: user.id,
                    amount: Decimal::from(1000),
                    source_id: user.id.to_string(),
                    transaction_type: CreditTransactionType::AdminGrant,
                    description: Some("Initial credit for testing".to_string()),
                })
                .await
                .unwrap();
        }

        let mut pool_conn = pool.acquire().await.unwrap();
        let mut api_key_repo = ApiKeys::new(&mut pool_conn);

        // Verify API key has access
        let keys_for_deployment = api_key_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment.id)
            .await
            .unwrap();
        assert!(keys_for_deployment.iter().any(|k| k.secret == api_key.secret));

        // Remove user from group
        let mut group_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut group_conn);
        group_repo.remove_user_from_group(user.id, group.id).await.unwrap();

        // API key should lose access
        let keys_for_deployment = api_key_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment.id)
            .await
            .unwrap();
        assert!(!keys_for_deployment.iter().any(|k| k.secret == api_key.secret));

        // API key should show no model access
        let api_key_details = api_key_repo.get_by_id(api_key.id).await.unwrap().unwrap();
        assert!(api_key_details.model_access.is_empty());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_api_key_loses_access_when_deployment_removed_from_group(pool: PgPool) {
        let mut tx = pool.begin().await.unwrap();
        let admin_user;
        let user;
        let deployment;
        let api_key;
        let group;

        {
            let mut user_repo = Users::new(tx.acquire().await.unwrap());

            // Create admin user
            let admin_create = UserCreateDBRequest::from(UserCreate {
                username: "admin".to_string(),
                email: "admin@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::PlatformManager],
            });
            admin_user = user_repo.create(&admin_create).await.unwrap();

            // Create regular user
            let user_create = UserCreateDBRequest::from(UserCreate {
                username: "testuser".to_string(),
                email: "test@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });
            user = user_repo.create(&user_create).await.unwrap();
        }

        // Create inference endpoint for deployments
        let config = crate::test_utils::create_test_config();

        crate::seed_database(&config.model_sources, &pool).await.unwrap();

        {
            let mut group_tx = tx.begin().await.unwrap();
            let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());

            // Create group
            let group_create = GroupCreateDBRequest {
                name: "Test Group".to_string(),
                description: Some("Test group for deployment removal".to_string()),
                created_by: admin_user.id,
            };
            group = group_repo.create(&group_create).await.unwrap();
            group_tx.commit().await.unwrap();
        }

        let test_endpoint_id = get_test_endpoint_id(&pool).await;
        let mut deployment_create = DeploymentCreateDBRequest::builder()
            .created_by(admin_user.id)
            .model_name("test-model".to_string())
            .alias("test-alias".to_string())
            .build();
        deployment_create.hosted_on = test_endpoint_id;

        {
            let mut deployment_tx = tx.begin().await.unwrap();
            let mut deployment_repo = Deployments::new(deployment_tx.acquire().await.unwrap());
            deployment = deployment_repo.create(&deployment_create).await.unwrap();
            deployment_tx.commit().await.unwrap();
        }

        {
            let mut group_tx = tx.begin().await.unwrap();
            let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());

            // Set up access: user in group, deployment in group
            group_repo.add_user_to_group(user.id, group.id).await.unwrap();
            group_repo
                .add_deployment_to_group(deployment.id, group.id, admin_user.id)
                .await
                .unwrap();
            group_tx.commit().await.unwrap();
        }

        {
            let mut api_key_repo = ApiKeys::new(tx.acquire().await.unwrap());

            // Create API key AFTER group relationships are established
            let api_key_create = ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Test API Key".to_string(),
                description: Some("API key for testing deployment removal".to_string()),
                purpose: ApiKeyPurpose::Inference,
                requests_per_second: None,
                burst_size: None,
            };
            api_key = api_key_repo.create(&api_key_create).await.unwrap();
        }

        // Commit transaction and create new connection for assertions
        tx.commit().await.unwrap();

        // Credit standard users so they can access deployments
        {
            let mut credit_conn = pool.acquire().await.unwrap();
            let mut credit_repo = Credits::new(&mut credit_conn);
            credit_repo
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: user.id,
                    amount: Decimal::from(1000),
                    source_id: user.id.to_string(),
                    transaction_type: CreditTransactionType::AdminGrant,
                    description: Some("Initial credit for testing".to_string()),
                })
                .await
                .unwrap();
        }

        let mut pool_conn = pool.acquire().await.unwrap();
        let mut api_key_repo = ApiKeys::new(&mut pool_conn);

        // Verify API key has access
        let keys_for_deployment = api_key_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment.id)
            .await
            .unwrap();
        assert!(keys_for_deployment.iter().any(|k| k.secret == api_key.secret));

        // Remove deployment from group
        let mut group_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut group_conn);
        group_repo.remove_deployment_from_group(deployment.id, group.id).await.unwrap();

        // API key should lose access
        let keys_for_deployment = api_key_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment.id)
            .await
            .unwrap();
        assert!(!keys_for_deployment.iter().any(|k| k.secret == api_key.secret));

        // API key should show no model access
        let api_key_details = api_key_repo.get_by_id(api_key.id).await.unwrap().unwrap();
        assert!(api_key_details.model_access.is_empty());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_multiple_api_keys_same_deployment_through_different_groups(pool: PgPool) {
        let mut tx = pool.begin().await.unwrap();
        let admin_user;
        let user1;
        let user2;
        let deployment;
        let api_key1;
        let api_key2;
        let group1;
        let group2;

        {
            let mut user_repo = Users::new(tx.acquire().await.unwrap());

            // Create admin user
            let admin_create = UserCreateDBRequest::from(UserCreate {
                username: "admin".to_string(),
                email: "admin@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::PlatformManager],
            });
            admin_user = user_repo.create(&admin_create).await.unwrap();

            // Create regular users
            let user1_create = UserCreateDBRequest::from(UserCreate {
                username: "user1".to_string(),
                email: "user1@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });
            user1 = user_repo.create(&user1_create).await.unwrap();

            let user2_create = UserCreateDBRequest::from(UserCreate {
                username: "user2".to_string(),
                email: "user2@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });
            user2 = user_repo.create(&user2_create).await.unwrap();
        }

        // Create inference endpoint for deployments
        let config = crate::test_utils::create_test_config();
        crate::seed_database(&config.model_sources, &pool).await.unwrap();

        {
            let mut group_tx = tx.begin().await.unwrap();
            let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());

            // Create two groups
            let group1_create = GroupCreateDBRequest {
                name: "Test Group 1".to_string(),
                description: Some("First test group".to_string()),
                created_by: admin_user.id,
            };
            group1 = group_repo.create(&group1_create).await.unwrap();

            let group2_create = GroupCreateDBRequest {
                name: "Test Group 2".to_string(),
                description: Some("Second test group".to_string()),
                created_by: admin_user.id,
            };
            group2 = group_repo.create(&group2_create).await.unwrap();
            group_tx.commit().await.unwrap();
        }

        // Get a valid endpoint ID
        let test_endpoint_id = get_test_endpoint_id(&pool).await;
        let mut deployment_create = DeploymentCreateDBRequest::builder()
            .created_by(admin_user.id)
            .model_name("shared-model".to_string())
            .alias("shared-alias".to_string())
            .build();
        deployment_create.hosted_on = test_endpoint_id;

        {
            let mut deployment_tx = tx.begin().await.unwrap();
            let mut deployment_repo = Deployments::new(deployment_tx.acquire().await.unwrap());
            deployment = deployment_repo.create(&deployment_create).await.unwrap();
            deployment_tx.commit().await.unwrap();
        }

        {
            let mut group_tx = tx.begin().await.unwrap();
            let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());

            // Add users to different groups
            group_repo.add_user_to_group(user1.id, group1.id).await.unwrap();
            group_repo.add_user_to_group(user2.id, group2.id).await.unwrap();

            // Add deployment to both groups
            group_repo
                .add_deployment_to_group(deployment.id, group1.id, admin_user.id)
                .await
                .unwrap();
            group_repo
                .add_deployment_to_group(deployment.id, group2.id, admin_user.id)
                .await
                .unwrap();
            group_tx.commit().await.unwrap();
        }

        {
            let mut api_key_repo = ApiKeys::new(tx.acquire().await.unwrap());

            // Create API keys for both users AFTER group relationships are established
            let api_key1_create = ApiKeyCreateDBRequest {
                user_id: user1.id,
                name: "User 1 Key".to_string(),
                description: Some("API key for user 1".to_string()),
                purpose: ApiKeyPurpose::Inference,
                requests_per_second: None,
                burst_size: None,
            };
            api_key1 = api_key_repo.create(&api_key1_create).await.unwrap();

            let api_key2_create = ApiKeyCreateDBRequest {
                user_id: user2.id,
                name: "User 2 Key".to_string(),
                description: Some("API key for user 2".to_string()),
                purpose: ApiKeyPurpose::Inference,
                requests_per_second: None,
                burst_size: None,
            };
            api_key2 = api_key_repo.create(&api_key2_create).await.unwrap();
        }

        // Commit transaction and create new connection for assertions
        tx.commit().await.unwrap();

        // Credit standard users so they can access deployments
        {
            let mut credit_conn = pool.acquire().await.unwrap();
            let mut credit_repo = Credits::new(&mut credit_conn);
            credit_repo
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: user1.id,
                    amount: Decimal::from(1000),
                    source_id: user1.id.to_string(),
                    transaction_type: CreditTransactionType::AdminGrant,
                    description: Some("Initial credit for testing".to_string()),
                })
                .await
                .unwrap();
            credit_repo
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: user2.id,
                    amount: Decimal::from(1000),
                    source_id: user2.id.to_string(),
                    transaction_type: CreditTransactionType::AdminGrant,
                    description: Some("Initial credit for testing".to_string()),
                })
                .await
                .unwrap();
        }

        let mut pool_conn = pool.acquire().await.unwrap();
        let mut api_key_repo = ApiKeys::new(&mut pool_conn);

        // Both API keys should have access to the deployment
        let keys_for_deployment = api_key_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment.id)
            .await
            .unwrap();
        assert!(keys_for_deployment.iter().any(|k| k.secret == api_key1.secret));
        assert!(keys_for_deployment.iter().any(|k| k.secret == api_key2.secret));
        assert_eq!(keys_for_deployment.len(), 2 + 1); // + 1 for system user

        // Remove deployment from group 1
        let mut group_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut group_conn);
        group_repo.remove_deployment_from_group(deployment.id, group1.id).await.unwrap();

        // Only user 2's API key should have access now
        let keys_for_deployment = api_key_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment.id)
            .await
            .unwrap();
        assert!(!keys_for_deployment.iter().any(|k| k.secret == api_key1.secret));
        assert!(keys_for_deployment.iter().any(|k| k.secret == api_key2.secret));
        assert_eq!(keys_for_deployment.len(), 1 + 1); // + 1 for system user
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_api_key_access_multiple_deployments_same_group(pool: PgPool) {
        let mut tx = pool.begin().await.unwrap();
        let admin_user;
        let user;
        let deployment1;
        let deployment2;
        let api_key;
        let group;

        {
            let mut user_repo = Users::new(tx.acquire().await.unwrap());

            // Create admin user
            let admin_create = UserCreateDBRequest::from(UserCreate {
                username: "admin".to_string(),
                email: "admin@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::PlatformManager],
            });
            admin_user = user_repo.create(&admin_create).await.unwrap();

            // Create regular user
            let user_create = UserCreateDBRequest::from(UserCreate {
                username: "testuser".to_string(),
                email: "test@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });
            user = user_repo.create(&user_create).await.unwrap();
        }
        {
            // Create inference endpoint for deployments
            let config = crate::test_utils::create_test_config();
            crate::seed_database(&config.model_sources, &pool).await.unwrap();

            {
                let mut group_tx = tx.begin().await.unwrap();
                let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());

                // Create a group
                let group_create = GroupCreateDBRequest {
                    name: "Multi Deployment Group".to_string(),
                    description: Some("Group with multiple deployments".to_string()),
                    created_by: admin_user.id,
                };
                group = group_repo.create(&group_create).await.unwrap();
                group_tx.commit().await.unwrap();
            }

            let test_endpoint_id = get_test_endpoint_id(&pool).await;
            let mut deployment1_create = DeploymentCreateDBRequest::builder()
                .created_by(admin_user.id)
                .model_name("model-1".to_string())
                .alias("alias-1".to_string())
                .build();
            deployment1_create.hosted_on = test_endpoint_id;
            let mut deployment2_create = DeploymentCreateDBRequest::builder()
                .created_by(admin_user.id)
                .model_name("model-2".to_string())
                .alias("alias-2".to_string())
                .build();
            deployment2_create.hosted_on = test_endpoint_id;

            {
                let mut deployment_tx = tx.begin().await.unwrap();
                let mut deployment_repo = Deployments::new(deployment_tx.acquire().await.unwrap());
                deployment1 = deployment_repo.create(&deployment1_create).await.unwrap();
                deployment2 = deployment_repo.create(&deployment2_create).await.unwrap();
                deployment_tx.commit().await.unwrap();
            }

            {
                let mut group_tx = tx.begin().await.unwrap();
                let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());

                // Add user to group
                group_repo.add_user_to_group(user.id, group.id).await.unwrap();

                // Add both deployments to group
                group_repo
                    .add_deployment_to_group(deployment1.id, group.id, admin_user.id)
                    .await
                    .unwrap();
                group_repo
                    .add_deployment_to_group(deployment2.id, group.id, admin_user.id)
                    .await
                    .unwrap();
                group_tx.commit().await.unwrap();
            }
        }

        {
            let mut api_key_repo = ApiKeys::new(tx.acquire().await.unwrap());

            // Create API key AFTER group relationships are established
            let api_key_create = ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Multi Access Key".to_string(),
                description: Some("API key for multiple deployments".to_string()),
                purpose: ApiKeyPurpose::Inference,
                requests_per_second: None,
                burst_size: None,
            };
            api_key = api_key_repo.create(&api_key_create).await.unwrap();
        }

        // Commit transaction and create new connection for assertions
        tx.commit().await.unwrap();

        // Credit standard users so they can access deployments
        {
            let mut credit_conn = pool.acquire().await.unwrap();
            let mut credit_repo = Credits::new(&mut credit_conn);
            credit_repo
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: user.id,
                    amount: Decimal::from(1000),
                    source_id: user.id.to_string(),
                    transaction_type: CreditTransactionType::AdminGrant,
                    description: Some("Initial credit for testing".to_string()),
                })
                .await
                .unwrap();
        }

        let mut pool_conn = pool.acquire().await.unwrap();
        let mut api_key_repo = ApiKeys::new(&mut pool_conn);

        // API key should have access to both deployments
        let keys_for_deployment1 = api_key_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment1.id)
            .await
            .unwrap();
        let keys_for_deployment2 = api_key_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment2.id)
            .await
            .unwrap();
        assert!(keys_for_deployment1.iter().any(|k| k.secret == api_key.secret));
        assert!(keys_for_deployment2.iter().any(|k| k.secret == api_key.secret));

        // API key should show both deployments in model_access
        assert!(api_key.model_access.contains(&deployment1.id));
        assert!(api_key.model_access.contains(&deployment2.id));
        assert_eq!(api_key.model_access.len(), 2);

        // Remove one deployment from group
        let mut group_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut group_conn);
        group_repo.remove_deployment_from_group(deployment1.id, group.id).await.unwrap();

        // API key should only have access to deployment 2
        let keys_for_deployment1 = api_key_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment1.id)
            .await
            .unwrap();
        let keys_for_deployment2 = api_key_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment2.id)
            .await
            .unwrap();
        assert!(!keys_for_deployment1.iter().any(|k| k.secret == api_key.secret));
        assert!(keys_for_deployment2.iter().any(|k| k.secret == api_key.secret));

        // API key should only show deployment 2 in model_access
        let api_key_details = api_key_repo.get_by_id(api_key.id).await.unwrap().unwrap();
        assert!(!api_key_details.model_access.contains(&deployment1.id));
        assert!(api_key_details.model_access.contains(&deployment2.id));
        assert_eq!(api_key_details.model_access.len(), 1);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_dynamic_api_key_access_after_group_membership_changes(pool: PgPool) {
        let mut tx = pool.begin().await.unwrap();
        let admin_user;
        let deployment;
        let user;
        let api_key;
        let group;
        {
            let mut user_repo = Users::new(tx.acquire().await.unwrap());

            // Create admin user
            let admin_create = UserCreateDBRequest::from(UserCreate {
                username: "admin".to_string(),
                email: "admin@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::PlatformManager],
            });
            admin_user = user_repo.create(&admin_create).await.unwrap();

            // Create regular user
            let user_create = UserCreateDBRequest::from(UserCreate {
                username: "testuser".to_string(),
                email: "test@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });
            user = user_repo.create(&user_create).await.unwrap();
        }
        // Create inference endpoint for deployments
        let config = crate::test_utils::create_test_config();
        crate::seed_database(&config.model_sources, &pool).await.unwrap();
        let test_endpoint_id = get_test_endpoint_id(&pool).await;
        {
            let mut deployment_repo = Deployments::new(tx.acquire().await.unwrap());
            let mut deployment_create = DeploymentCreateDBRequest::builder()
                .created_by(admin_user.id)
                .model_name("test-model".to_string())
                .alias("test-alias".to_string())
                .build();
            deployment_create.hosted_on = test_endpoint_id;
            deployment = deployment_repo.create(&deployment_create).await.unwrap();
        }
        {
            let mut group_repo = Groups::new(tx.acquire().await.unwrap());
            // Create a group and deployment
            let group_create = GroupCreateDBRequest {
                name: "Test Group".to_string(),
                description: Some("Test group for dynamic access".to_string()),
                created_by: admin_user.id,
            };
            group = group_repo.create(&group_create).await.unwrap();

            // Add deployment to group
            group_repo
                .add_deployment_to_group(deployment.id, group.id, admin_user.id)
                .await
                .unwrap();
        }

        {
            let mut api_key_repo = ApiKeys::new(tx.acquire().await.unwrap());
            // Create API key BEFORE user is added to group
            let api_key_create = ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Dynamic Access Key".to_string(),
                description: Some("API key for testing dynamic access".to_string()),
                purpose: ApiKeyPurpose::Inference,
                requests_per_second: None,
                burst_size: None,
            };
            api_key = api_key_repo.create(&api_key_create).await.unwrap();
        }
        tx.commit().await.unwrap();

        // Credit standard users so they can access deployments
        {
            let mut credit_conn = pool.acquire().await.unwrap();
            let mut credit_repo = Credits::new(&mut credit_conn);
            credit_repo
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: user.id,
                    amount: Decimal::from(1000),
                    source_id: user.id.to_string(),
                    transaction_type: CreditTransactionType::AdminGrant,
                    description: Some("Initial credit for testing".to_string()),
                })
                .await
                .unwrap();
        }

        let mut pool_conn = pool.acquire().await.unwrap();
        let mut api_key_repo = ApiKeys::new(&mut pool_conn);

        // Initially, API key should have NO access (user not in group yet)
        let keys_for_deployment = api_key_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment.id)
            .await
            .unwrap();
        assert!(
            !keys_for_deployment.iter().any(|k| k.secret == api_key.secret),
            "API key should not have access before user is added to group"
        );

        let api_key_details = api_key_repo.get_by_id(api_key.id).await.unwrap().unwrap();
        assert!(
            !api_key_details.model_access.contains(&deployment.id),
            "API key model_access should not include deployment before user is added to group"
        );

        // Get a new connection for group operations
        let mut group_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut group_conn);

        // NOW add user to group AFTER API key was created
        group_repo.add_user_to_group(user.id, group.id).await.unwrap();

        // API key should now dynamically gain access
        let keys_for_deployment = api_key_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment.id)
            .await
            .unwrap();
        assert!(
            keys_for_deployment.iter().any(|k| k.secret == api_key.secret),
            "API key should gain access after user is added to group"
        );

        let api_key_details = api_key_repo.get_by_id(api_key.id).await.unwrap().unwrap();
        assert!(
            api_key_details.model_access.contains(&deployment.id),
            "API key model_access should include deployment after user is added to group"
        );

        // Remove user from group - access should be revoked again
        group_repo.remove_user_from_group(user.id, group.id).await.unwrap();

        let keys_for_deployment = api_key_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment.id)
            .await
            .unwrap();
        assert!(
            !keys_for_deployment.iter().any(|k| k.secret == api_key.secret),
            "API key should lose access after user is removed from group"
        );

        let api_key_details = api_key_repo.get_by_id(api_key.id).await.unwrap().unwrap();
        assert!(
            !api_key_details.model_access.contains(&deployment.id),
            "API key model_access should not include deployment after user is removed from group"
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_api_key_access_through_everyone_group(pool: PgPool) {
        let mut tx = pool.begin().await.unwrap();
        let admin_user;
        let user;
        let deployment;
        let api_key;

        {
            let mut user_repo = Users::new(tx.acquire().await.unwrap());

            // Create admin user
            let admin_create = UserCreateDBRequest::from(UserCreate {
                username: "admin".to_string(),
                email: "admin@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::PlatformManager],
            });
            admin_user = user_repo.create(&admin_create).await.unwrap();

            // Create regular user
            let user_create = UserCreateDBRequest::from(UserCreate {
                username: "testuser".to_string(),
                email: "test@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });
            user = user_repo.create(&user_create).await.unwrap();
        }

        // Use the seeded endpoint instead of creating a new one
        let config = crate::config::Config {
            host: "localhost".to_string(),
            port: 3001,
            database_url: None,
            database: crate::config::DatabaseConfig::External {
                pool_config: crate::config::DatabasePoolConfig::default(),
                url: "postgres://test@localhost/test".to_string(),
            },
            admin_email: "admin@example.org".to_string(),
            admin_password: None,
            secret_key: None,
            model_sources: vec![crate::config::ModelSource {
                name: "test".to_string(),
                url: "http://localhost:8080".parse().unwrap(),
                api_key: None,
                sync_interval: std::time::Duration::from_secs(3600),
            }],
            metadata: crate::config::Metadata {
                region: "Test Region".to_string(),
                organization: "Test Org".to_string(),
                registration_enabled: false,
            },
            auth: Default::default(),
            enable_metrics: false,
            enable_request_logging: false,
            enable_otel_export: false,
            credits: Default::default(),
            batches: Default::default(),
            background_services: crate::config::BackgroundServicesConfig::default(),
            payment: None,
        };
        crate::seed_database(&config.model_sources, &pool).await.unwrap();

        let test_endpoint_id = get_test_endpoint_id(&pool).await;
        let mut deployment_create = DeploymentCreateDBRequest::builder()
            .created_by(admin_user.id)
            .model_name("test-model".to_string())
            .alias("test-alias".to_string())
            .build();
        deployment_create.hosted_on = test_endpoint_id;

        {
            let mut deployment_tx = tx.begin().await.unwrap();
            let mut deployment_repo = Deployments::new(deployment_tx.acquire().await.unwrap());
            deployment = deployment_repo.create(&deployment_create).await.unwrap();
            deployment_tx.commit().await.unwrap();
        }

        {
            let mut group_tx = tx.begin().await.unwrap();
            let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());

            // Add deployment to Everyone group
            let everyone_group_id = uuid::Uuid::nil();
            group_repo
                .add_deployment_to_group(deployment.id, everyone_group_id, admin_user.id)
                .await
                .unwrap();
            group_tx.commit().await.unwrap();
        }

        {
            let mut api_key_repo = ApiKeys::new(tx.acquire().await.unwrap());

            // Create API key for user
            let api_key_create = ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Test API Key".to_string(),
                description: Some("API key for testing Everyone group access".to_string()),
                purpose: ApiKeyPurpose::Inference,
                requests_per_second: None,
                burst_size: None,
            };
            api_key = api_key_repo.create(&api_key_create).await.unwrap();
        }

        // Commit transaction and create new connection for assertions
        tx.commit().await.unwrap();

        // Credit standard users so they can access deployments
        {
            let mut credit_conn = pool.acquire().await.unwrap();
            let mut credit_repo = Credits::new(&mut credit_conn);
            credit_repo
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: user.id,
                    amount: Decimal::from(1000),
                    source_id: user.id.to_string(),
                    transaction_type: CreditTransactionType::AdminGrant,
                    description: Some("Initial credit for testing".to_string()),
                })
                .await
                .unwrap();
        }

        let mut pool_conn = pool.acquire().await.unwrap();
        let mut api_key_repo = ApiKeys::new(&mut pool_conn);

        // API key should have access to the deployment through Everyone group
        let keys_for_deployment = api_key_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment.id)
            .await
            .unwrap();
        assert!(
            keys_for_deployment.iter().any(|k| k.secret == api_key.secret),
            "API key should have access through Everyone group"
        );

        // API key should show the deployment in model_access
        assert!(
            api_key.model_access.contains(&deployment.id),
            "API key model_access should include deployment through Everyone group"
        );

        // Verify that everyone group access works for keys_for_deployment
        let all_keys = api_key_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment.id)
            .await
            .unwrap();
        assert!(
            all_keys.iter().any(|k| k.secret == api_key.secret),
            "get_api_keys_for_deployment should include API keys with Everyone group access"
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_api_keys_pagination_with_filter(pool: PgPool) {
        let mut tx = pool.begin().await.unwrap();
        let user;
        {
            let mut user_repo = Users::new(tx.acquire().await.unwrap());
            let user_create = UserCreateDBRequest::from(UserCreate {
                username: "paginationuser".to_string(),
                email: "pagination@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });

            user = user_repo.create(&user_create).await.unwrap();
        }

        {
            let mut api_repo = ApiKeys::new(tx.acquire().await.unwrap());
            // Create 5 API keys for this user
            for i in 1..=5 {
                let key_create = ApiKeyCreateDBRequest {
                    user_id: user.id,
                    name: format!("Pagination Key {i}"),
                    description: Some(format!("Key {i} for pagination testing")),
                    purpose: ApiKeyPurpose::Inference,
                    requests_per_second: None,
                    burst_size: None,
                };
                api_repo.create(&key_create).await.unwrap();
            }
        }

        // Commit transaction and create new connection for assertions
        tx.commit().await.unwrap();
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut api_repo = ApiKeys::new(&mut pool_conn);

        // Test first page (skip=0, limit=2) using filter
        let first_page = api_repo
            .list(&ApiKeyFilter {
                skip: 0,
                limit: 2,
                user_id: Some(user.id),
            })
            .await
            .unwrap();
        assert_eq!(first_page.len(), 2, "First page should have 2 items");

        // Test second page (skip=2, limit=2) using filter
        let second_page = api_repo
            .list(&ApiKeyFilter {
                skip: 2,
                limit: 2,
                user_id: Some(user.id),
            })
            .await
            .unwrap();
        assert_eq!(second_page.len(), 2, "Second page should have 2 items");

        // Test third page (skip=4, limit=2) using filter
        let third_page = api_repo
            .list(&ApiKeyFilter {
                skip: 4,
                limit: 2,
                user_id: Some(user.id),
            })
            .await
            .unwrap();
        assert_eq!(third_page.len(), 1, "Third page should have 1 item");

        // Test beyond available data (skip=10, limit=2) using filter
        let empty_page = api_repo
            .list(&ApiKeyFilter {
                skip: 10,
                limit: 2,
                user_id: Some(user.id),
            })
            .await
            .unwrap();
        assert_eq!(empty_page.len(), 0, "Empty page should have 0 items");

        // Verify no overlap between pages (check names are different)
        let first_names: Vec<&String> = first_page.iter().map(|k| &k.name).collect();
        let second_names: Vec<&String> = second_page.iter().map(|k| &k.name).collect();
        for first_name in &first_names {
            assert!(!second_names.contains(first_name), "Pages should not overlap");
        }

        // Test ordering (newest first due to ORDER BY created_at DESC)
        let all_keys = api_repo
            .list(&ApiKeyFilter {
                skip: 0,
                limit: 10,
                user_id: Some(user.id),
            })
            .await
            .unwrap();
        assert_eq!(all_keys.len(), 5);

        for i in 1..all_keys.len() {
            assert!(
                all_keys[i - 1].created_at >= all_keys[i].created_at,
                "Keys should be ordered by created_at DESC"
            );
        }
    }

    // Also add a test to exercise both arms of the static SQL query

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_api_keys_filter_arms(pool: PgPool) {
        let mut tx = pool.begin().await.unwrap();
        let user1;
        let user2;

        {
            let mut user_repo = Users::new(tx.acquire().await.unwrap());

            // Create two users
            user1 = user_repo
                .create(&UserCreateDBRequest::from(UserCreate {
                    username: "user1".to_string(),
                    email: "user1@example.com".to_string(),
                    display_name: None,
                    avatar_url: None,
                    roles: vec![Role::StandardUser],
                }))
                .await
                .unwrap();

            user2 = user_repo
                .create(&UserCreateDBRequest::from(UserCreate {
                    username: "user2".to_string(),
                    email: "user2@example.com".to_string(),
                    display_name: None,
                    avatar_url: None,
                    roles: vec![Role::StandardUser],
                }))
                .await
                .unwrap();
        }

        {
            let mut api_repo = ApiKeys::new(tx.acquire().await.unwrap());

            // Create API keys for both users
            let key1 = ApiKeyCreateDBRequest {
                user_id: user1.id,
                name: "User1 Key".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Inference,
                requests_per_second: None,
                burst_size: None,
            };
            let key2 = ApiKeyCreateDBRequest {
                user_id: user2.id,
                name: "User2 Key".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Inference,
                requests_per_second: None,
                burst_size: None,
            };

            api_repo.create(&key1).await.unwrap();
            api_repo.create(&key2).await.unwrap();
        }

        // Commit transaction and create new connection for assertions
        tx.commit().await.unwrap();
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut api_repo = ApiKeys::new(&mut pool_conn);

        // Test with user_id filter (should only get user1's key)
        let user1_keys = api_repo
            .list(&ApiKeyFilter {
                skip: 0,
                limit: 10,
                user_id: Some(user1.id),
            })
            .await
            .unwrap();
        assert_eq!(user1_keys.len(), 1);
        assert_eq!(user1_keys[0].user_id, user1.id);

        // Test without user_id filter (should get both keys)
        let all_keys = api_repo
            .list(&ApiKeyFilter {
                skip: 0,
                limit: 10,
                user_id: None,
            })
            .await
            .unwrap();
        let user_ids: Vec<_> = all_keys.iter().map(|k| k.user_id).collect();
        assert!(user_ids.contains(&user1.id));
        assert!(user_ids.contains(&user2.id));
    }

    // Tests for bulk API key operations

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_bulk_api_keys_with_valid_ids(pool: PgPool) {
        let mut user_conn = pool.acquire().await.unwrap();
        let mut user_repo = Users::new(&mut user_conn);

        // Create a user
        let user_create = UserCreateDBRequest::from(UserCreate {
            username: "bulkuser".to_string(),
            email: "bulk@example.com".to_string(),
            display_name: None,
            avatar_url: None,
            roles: vec![Role::StandardUser],
        });
        let user = user_repo.create(&user_create).await.unwrap();

        // Create multiple API keys
        let key1_create = ApiKeyCreateDBRequest {
            user_id: user.id,
            name: "Bulk Key 1".to_string(),
            description: Some("First bulk key".to_string()),
            purpose: ApiKeyPurpose::Inference,
            requests_per_second: None,
            burst_size: None,
        };
        let key2_create = ApiKeyCreateDBRequest {
            user_id: user.id,
            name: "Bulk Key 2".to_string(),
            description: Some("Second bulk key".to_string()),
            purpose: ApiKeyPurpose::Inference,
            requests_per_second: None,
            burst_size: None,
        };
        let key3_create = ApiKeyCreateDBRequest {
            user_id: user.id,
            name: "Bulk Key 3".to_string(),
            description: None,
            purpose: ApiKeyPurpose::Inference,
            requests_per_second: None,
            burst_size: None,
        };

        let mut api_conn = pool.acquire().await.unwrap();
        let mut api_repo = ApiKeys::new(&mut api_conn);
        let key1 = api_repo.create(&key1_create).await.unwrap();
        let key2 = api_repo.create(&key2_create).await.unwrap();
        let key3 = api_repo.create(&key3_create).await.unwrap();

        // Test bulk retrieval with all three keys
        let bulk_ids = vec![key1.id, key2.id, key3.id];
        let bulk_results = api_repo.get_bulk(bulk_ids.clone()).await.unwrap();

        // Verify all keys are returned
        assert_eq!(bulk_results.len(), 3);
        assert!(bulk_results.contains_key(&key1.id));
        assert!(bulk_results.contains_key(&key2.id));
        assert!(bulk_results.contains_key(&key3.id));

        // Verify the data integrity
        let retrieved_key1 = &bulk_results[&key1.id];
        assert_eq!(retrieved_key1.name, "Bulk Key 1");
        assert_eq!(retrieved_key1.description, Some("First bulk key".to_string()));
        assert_eq!(retrieved_key1.user_id, user.id);
        assert!(retrieved_key1.secret.starts_with("sk-"));

        let retrieved_key2 = &bulk_results[&key2.id];
        assert_eq!(retrieved_key2.name, "Bulk Key 2");
        assert_eq!(retrieved_key2.description, Some("Second bulk key".to_string()));

        let retrieved_key3 = &bulk_results[&key3.id];
        assert_eq!(retrieved_key3.name, "Bulk Key 3");
        assert_eq!(retrieved_key3.description, None);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_bulk_api_keys_with_some_invalid_ids(pool: PgPool) {
        let mut user_conn = pool.acquire().await.unwrap();
        let mut user_repo = Users::new(&mut user_conn);

        // Create a user and one API key
        let user_create = UserCreateDBRequest::from(UserCreate {
            username: "partialuser".to_string(),
            email: "partial@example.com".to_string(),
            display_name: None,
            avatar_url: None,
            roles: vec![Role::StandardUser],
        });
        let user = user_repo.create(&user_create).await.unwrap();

        let key_create = ApiKeyCreateDBRequest {
            user_id: user.id,
            name: "Valid Key".to_string(),
            description: Some("Only valid key".to_string()),
            purpose: ApiKeyPurpose::Inference,
            requests_per_second: None,
            burst_size: None,
        };

        let mut api_conn = pool.acquire().await.unwrap();
        let mut api_repo = ApiKeys::new(&mut api_conn);
        let valid_key = api_repo.create(&key_create).await.unwrap();

        // Create fake IDs that don't exist
        let fake_id1 = uuid::Uuid::new_v4();
        let fake_id2 = uuid::Uuid::new_v4();

        // Test bulk retrieval with mix of valid and invalid IDs
        let bulk_ids = vec![valid_key.id, fake_id1, fake_id2];
        let bulk_results = api_repo.get_bulk(bulk_ids).await.unwrap();

        // Should only return the valid key
        assert_eq!(bulk_results.len(), 1);
        assert!(bulk_results.contains_key(&valid_key.id));
        assert!(!bulk_results.contains_key(&fake_id1));
        assert!(!bulk_results.contains_key(&fake_id2));

        // Verify the valid key data
        let retrieved_key = &bulk_results[&valid_key.id];
        assert_eq!(retrieved_key.name, "Valid Key");
        assert_eq!(retrieved_key.user_id, user.id);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_bulk_api_keys_with_empty_ids(pool: PgPool) {
        let mut pool_conn = pool.acquire().await.unwrap();

        let mut api_repo = ApiKeys::new(&mut pool_conn);
        // Test bulk retrieval with empty ID list
        let empty_ids: Vec<ApiKeyId> = vec![];
        let bulk_results = api_repo.get_bulk(empty_ids).await.unwrap();

        // Should return empty hashmap
        assert_eq!(bulk_results.len(), 0);
        assert!(bulk_results.is_empty());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_bulk_api_keys_with_all_invalid_ids(pool: PgPool) {
        let mut pool_conn = pool.acquire().await.unwrap();

        let mut api_repo = ApiKeys::new(&mut pool_conn);
        // Test bulk retrieval with only invalid IDs
        let fake_ids = vec![uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), uuid::Uuid::new_v4()];
        let bulk_results = api_repo.get_bulk(fake_ids).await.unwrap();

        // Should return empty hashmap
        assert_eq!(bulk_results.len(), 0);
        assert!(bulk_results.is_empty());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_bulk_api_keys_with_duplicate_ids(pool: PgPool) {
        let mut user_conn = pool.acquire().await.unwrap();
        let mut user_repo = Users::new(&mut user_conn);

        // Create a user and one API key
        let user_create = UserCreateDBRequest::from(UserCreate {
            username: "dupuser".to_string(),
            email: "dup@example.com".to_string(),
            display_name: None,
            avatar_url: None,
            roles: vec![Role::StandardUser],
        });
        let user = user_repo.create(&user_create).await.unwrap();

        let key_create = ApiKeyCreateDBRequest {
            user_id: user.id,
            name: "Duplicate Test Key".to_string(),
            description: Some("Key for testing duplicates".to_string()),
            purpose: ApiKeyPurpose::Inference,
            requests_per_second: None,
            burst_size: None,
        };
        let mut api_conn = pool.acquire().await.unwrap();
        let mut api_repo = ApiKeys::new(&mut api_conn);
        let api_key = api_repo.create(&key_create).await.unwrap();

        // Test bulk retrieval with duplicate IDs
        let duplicate_ids = vec![api_key.id, api_key.id, api_key.id];
        let bulk_results = api_repo.get_bulk(duplicate_ids).await.unwrap();

        // Should return only one entry (deduplication handled by database)
        assert_eq!(bulk_results.len(), 1);
        assert!(bulk_results.contains_key(&api_key.id));

        // Verify the data
        let retrieved_key = &bulk_results[&api_key.id];
        assert_eq!(retrieved_key.name, "Duplicate Test Key");
        assert_eq!(retrieved_key.user_id, user.id);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_bulk_api_keys_includes_model_access(pool: PgPool) {
        let mut tx = pool.begin().await.unwrap();

        let mut user_repo = Users::new(&mut tx);

        // Create admin user
        let admin_create = UserCreateDBRequest::from(UserCreate {
            username: "admin".to_string(),
            email: "admin@example.com".to_string(),
            display_name: None,
            avatar_url: None,
            roles: vec![Role::PlatformManager],
        });
        let admin_user = user_repo.create(&admin_create).await.unwrap();

        // Create regular user
        let user_create = UserCreateDBRequest::from(UserCreate {
            username: "bulkaccessuser".to_string(),
            email: "bulkaccess@example.com".to_string(),
            display_name: None,
            avatar_url: None,
            roles: vec![Role::StandardUser],
        });
        let user = user_repo.create(&user_create).await.unwrap();
        tx.commit().await.unwrap();

        // Create inference endpoint for deployments
        let config = crate::test_utils::create_test_config();
        crate::seed_database(&config.model_sources, &pool).await.unwrap();
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let mut tx = pool.begin().await.unwrap();
        let mut deployment_create = DeploymentCreateDBRequest::builder()
            .created_by(admin_user.id)
            .model_name("bulk-model".to_string())
            .alias("bulk-alias".to_string())
            .build();
        deployment_create.hosted_on = test_endpoint_id;

        let mut deployment_repo = Deployments::new(&mut tx);
        let deployment = deployment_repo.create(&deployment_create).await.unwrap();

        let mut group_repo = Groups::new(&mut tx);

        // Create group and deployment
        let group_create = GroupCreateDBRequest {
            name: "Bulk Test Group".to_string(),
            description: Some("Group for bulk API key testing".to_string()),
            created_by: admin_user.id,
        };
        let group = group_repo.create(&group_create).await.unwrap();

        // Set up access: user in group, deployment in group
        group_repo.add_user_to_group(user.id, group.id).await.unwrap();
        group_repo
            .add_deployment_to_group(deployment.id, group.id, admin_user.id)
            .await
            .unwrap();

        // Create multiple API keys
        let key1_create = ApiKeyCreateDBRequest {
            user_id: user.id,
            name: "Bulk Access Key 1".to_string(),
            description: Some("First key with model access".to_string()),
            purpose: ApiKeyPurpose::Inference,
            requests_per_second: None,
            burst_size: None,
        };
        let key2_create = ApiKeyCreateDBRequest {
            user_id: user.id,
            name: "Bulk Access Key 2".to_string(),
            description: Some("Second key with model access".to_string()),
            purpose: ApiKeyPurpose::Inference,
            requests_per_second: None,
            burst_size: None,
        };

        let mut api_repo = ApiKeys::new(&mut tx);

        let key1 = api_repo.create(&key1_create).await.unwrap();
        let key2 = api_repo.create(&key2_create).await.unwrap();

        tx.commit().await.map_err(|e| Error::Database(e.into())).unwrap();
        let mut api_conn = pool.acquire().await.unwrap();
        let mut api_repo = ApiKeys::new(&mut api_conn);

        // Test bulk retrieval includes model access
        let bulk_ids = vec![key1.id, key2.id];
        let bulk_results = api_repo.get_bulk(bulk_ids).await.unwrap();

        assert_eq!(bulk_results.len(), 2);

        // Verify both keys have model access to the deployment
        let retrieved_key1 = &bulk_results[&key1.id];
        let retrieved_key2 = &bulk_results[&key2.id];

        assert!(retrieved_key1.model_access.contains(&deployment.id));
        assert!(retrieved_key2.model_access.contains(&deployment.id));
        assert_eq!(retrieved_key1.model_access.len(), 1);
        assert_eq!(retrieved_key2.model_access.len(), 1);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_bulk_api_keys_from_different_users(pool: PgPool) {
        let mut tx = pool.begin().await.unwrap();
        let mut user_repo = Users::new(&mut tx);

        // Create two different users
        let user1_create = UserCreateDBRequest::from(UserCreate {
            username: "bulkuser1".to_string(),
            email: "bulk1@example.com".to_string(),
            display_name: None,
            avatar_url: None,
            roles: vec![Role::StandardUser],
        });
        let user1 = user_repo.create(&user1_create).await.unwrap();

        let user2_create = UserCreateDBRequest::from(UserCreate {
            username: "bulkuser2".to_string(),
            email: "bulk2@example.com".to_string(),
            display_name: None,
            avatar_url: None,
            roles: vec![Role::StandardUser],
        });
        let user2 = user_repo.create(&user2_create).await.unwrap();

        // Create API keys for both users
        let key1_create = ApiKeyCreateDBRequest {
            user_id: user1.id,
            name: "User1 Bulk Key".to_string(),
            description: Some("Key for user 1".to_string()),
            purpose: ApiKeyPurpose::Inference,
            requests_per_second: None,
            burst_size: None,
        };
        let key2_create = ApiKeyCreateDBRequest {
            user_id: user2.id,
            name: "User2 Bulk Key".to_string(),
            description: Some("Key for user 2".to_string()),
            purpose: ApiKeyPurpose::Inference,
            requests_per_second: None,
            burst_size: None,
        };
        let mut api_repo = ApiKeys::new(&mut tx);

        let key1 = api_repo.create(&key1_create).await.unwrap();
        let key2 = api_repo.create(&key2_create).await.unwrap();

        tx.commit().await.unwrap();
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut api_repo = ApiKeys::new(&mut pool_conn);

        // Test bulk retrieval across different users
        let bulk_ids = vec![key1.id, key2.id];
        let bulk_results = api_repo.get_bulk(bulk_ids).await.unwrap();

        assert_eq!(bulk_results.len(), 2);

        // Verify correct user associations
        let retrieved_key1 = &bulk_results[&key1.id];
        let retrieved_key2 = &bulk_results[&key2.id];

        assert_eq!(retrieved_key1.user_id, user1.id);
        assert_eq!(retrieved_key1.name, "User1 Bulk Key");

        assert_eq!(retrieved_key2.user_id, user2.id);
        assert_eq!(retrieved_key2.name, "User2 Bulk Key");

        // Verify they have different secrets
        assert_ne!(retrieved_key1.secret, retrieved_key2.secret);
        assert!(retrieved_key1.secret.starts_with("sk-"));
        assert!(retrieved_key2.secret.starts_with("sk-"));
    }

    // Helper struct to hold test setup data for credit filtering tests
    struct CreditFilteringTestSetup {
        deployment: DeploymentDBResponse,
        group: GroupDBResponse,
    }

    // Helper function to set up common test infrastructure for credit filtering tests
    async fn setup_credit_filtering_test(pool: &PgPool) -> CreditFilteringTestSetup {
        let mut tx = pool.begin().await.unwrap();

        // Create admin user
        let admin_user;
        {
            let mut user_repo = Users::new(tx.acquire().await.unwrap());
            let admin_create = UserCreateDBRequest::from(UserCreate {
                username: format!("admin_{}", uuid::Uuid::new_v4()),
                email: format!("admin_{}@example.com", uuid::Uuid::new_v4()),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::PlatformManager],
            });
            admin_user = user_repo.create(&admin_create).await.unwrap();
        }

        // Create group
        let group;
        {
            let mut group_tx = tx.begin().await.unwrap();
            let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());
            let group_create = GroupCreateDBRequest {
                name: format!("Test Group {}", uuid::Uuid::new_v4()),
                description: Some("Test group for credit filtering".to_string()),
                created_by: admin_user.id,
            };
            group = group_repo.create(&group_create).await.unwrap();
            group_tx.commit().await.unwrap();
        }

        // Seed database and get endpoint ID
        let config = crate::test_utils::create_test_config();
        crate::seed_database(&config.model_sources, pool).await.unwrap();
        let test_endpoint_id = get_test_endpoint_id(pool).await;

        // Create deployment with pricing (paid model by default for credit tests)
        let deployment;
        {
            use crate::db::models::deployments::{ModelPricing, TokenPricing};

            let mut deployment_tx = tx.begin().await.unwrap();
            let mut deployment_repo = Deployments::new(deployment_tx.acquire().await.unwrap());

            // Default to a paid model for credit filtering tests
            let pricing = ModelPricing {
                upstream: Some(TokenPricing {
                    input_price_per_token: Some(Decimal::new(1, 6)),  // $0.000001 per token
                    output_price_per_token: Some(Decimal::new(2, 6)), // $0.000002 per token
                }),
                downstream: None,
            };

            let mut deployment_create = DeploymentCreateDBRequest::builder()
                .created_by(admin_user.id)
                .model_name(format!("test-model-{}", uuid::Uuid::new_v4()))
                .alias(format!("test-alias-{}", uuid::Uuid::new_v4()))
                .pricing(pricing)
                .build();
            deployment_create.hosted_on = test_endpoint_id;
            deployment = deployment_repo.create(&deployment_create).await.unwrap();
            deployment_tx.commit().await.unwrap();
        }

        // Add deployment to group
        {
            let mut group_tx = tx.begin().await.unwrap();
            let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());
            group_repo
                .add_deployment_to_group(deployment.id, group.id, admin_user.id)
                .await
                .unwrap();
            group_tx.commit().await.unwrap();
        }

        tx.commit().await.unwrap();

        CreditFilteringTestSetup { deployment, group }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_api_keys_filtered_by_insufficient_credits(pool: PgPool) {
        let mut tx = pool.begin().await.unwrap();

        // Create admin and users
        let admin_user;
        let user_with_credits;
        let user_without_credits;
        {
            let mut user_repo = Users::new(tx.acquire().await.unwrap());

            // Create admin user
            let admin_create = UserCreateDBRequest::from(UserCreate {
                username: "admin".to_string(),
                email: "admin@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::PlatformManager],
            });
            admin_user = user_repo.create(&admin_create).await.unwrap();

            // Create user with credits
            let user_create1 = UserCreateDBRequest::from(UserCreate {
                username: "user_with_credits".to_string(),
                email: "user_with_credits@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });
            user_with_credits = user_repo.create(&user_create1).await.unwrap();

            // Create user without credits
            let user_create2 = UserCreateDBRequest::from(UserCreate {
                username: "user_without_credits".to_string(),
                email: "user_without_credits@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });
            user_without_credits = user_repo.create(&user_create2).await.unwrap();
        }

        // Create group
        let group;
        {
            let mut group_tx = tx.begin().await.unwrap();
            let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());
            let group_create = GroupCreateDBRequest {
                name: "Test Group".to_string(),
                description: Some("Test group for credit filtering".to_string()),
                created_by: admin_user.id,
            };
            group = group_repo.create(&group_create).await.unwrap();
            group_tx.commit().await.unwrap();
        }

        // Seed database and get endpoint ID
        let config = crate::test_utils::create_test_config();
        crate::seed_database(&config.model_sources, &pool).await.unwrap();
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create deployment with pricing (paid model)
        let deployment;
        {
            use crate::db::models::deployments::{ModelPricing, TokenPricing};

            let mut deployment_tx = tx.begin().await.unwrap();
            let mut deployment_repo = Deployments::new(deployment_tx.acquire().await.unwrap());

            let pricing = ModelPricing {
                upstream: Some(TokenPricing {
                    input_price_per_token: Some(Decimal::new(1, 6)),  // $0.000001 per token
                    output_price_per_token: Some(Decimal::new(2, 6)), // $0.000002 per token
                }),
                downstream: None,
            };

            let mut deployment_create = DeploymentCreateDBRequest::builder()
                .created_by(admin_user.id)
                .model_name("test-model".to_string())
                .alias("test-alias".to_string())
                .pricing(pricing)
                .build();
            deployment_create.hosted_on = test_endpoint_id;
            deployment = deployment_repo.create(&deployment_create).await.unwrap();
            deployment_tx.commit().await.unwrap();
        }

        // Add users to group and deployment to group
        {
            let mut group_tx = tx.begin().await.unwrap();
            let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());
            group_repo.add_user_to_group(user_with_credits.id, group.id).await.unwrap();
            group_repo.add_user_to_group(user_without_credits.id, group.id).await.unwrap();
            group_repo
                .add_deployment_to_group(deployment.id, group.id, admin_user.id)
                .await
                .unwrap();
            group_tx.commit().await.unwrap();
        }

        // Create API keys for both users
        let mut api_repo = ApiKeys::new(&mut tx);
        let key_with_credits = api_repo
            .create(&ApiKeyCreateDBRequest {
                name: "Key with credits".to_string(),
                description: Some("Has sufficient credits".to_string()),
                user_id: user_with_credits.id,
                requests_per_second: None,
                burst_size: None,
                purpose: ApiKeyPurpose::Inference,
            })
            .await
            .unwrap();

        let key_without_credits = api_repo
            .create(&ApiKeyCreateDBRequest {
                name: "Key without credits".to_string(),
                description: Some("Has insufficient credits".to_string()),
                user_id: user_without_credits.id,
                requests_per_second: None,
                burst_size: None,
                purpose: ApiKeyPurpose::Inference,
            })
            .await
            .unwrap();

        tx.commit().await.unwrap();

        // Add credits to user_with_credits
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut credits_repo = Credits::new(&mut pool_conn);

        credits_repo
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: user_with_credits.id,
                transaction_type: CreditTransactionType::Purchase,
                amount: Decimal::new(1000, 0),               // 1000 credits
                source_id: uuid::Uuid::new_v4().to_string(), // Mimics Stripe payment ID
                description: Some("Initial credits".to_string()),
            })
            .await
            .unwrap();

        // Add credits to user_without_credits but then deduct them all
        credits_repo
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: user_without_credits.id,
                transaction_type: CreditTransactionType::Purchase,
                amount: Decimal::new(100, 0),
                source_id: uuid::Uuid::new_v4().to_string(), // Mimics Stripe payment ID
                description: Some("Initial credits".to_string()),
            })
            .await
            .unwrap();

        credits_repo
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: user_without_credits.id,
                transaction_type: CreditTransactionType::Usage,
                amount: Decimal::new(120, 0),                // Deduct all credits
                source_id: uuid::Uuid::new_v4().to_string(), // Mimics request ID from http_analytics
                description: Some("Used all credits".to_string()),
            })
            .await
            .unwrap();

        // Get new connection to query
        let mut api_repo = ApiKeys::new(&mut pool_conn);

        // Get API keys for deployment - should only include key_with_credits
        let keys_for_deployment = api_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment.id)
            .await
            .unwrap();

        // Debug: Check credits
        let mut credits_repo = Credits::new(&mut pool_conn);
        let balance_with = credits_repo.get_user_balance(user_with_credits.id).await.unwrap();
        let balance_without = credits_repo.get_user_balance(user_without_credits.id).await.unwrap();
        println!("Balance for user_with_credits: {}", balance_with);
        println!("Balance for user_without_credits: {}", balance_without);

        println!("Keys returned: {}", keys_for_deployment.len());
        for key in &keys_for_deployment {
            println!(
                "  - {} (user: {}, secret: {}...)",
                key.name,
                abbrev_uuid(&key.user_id),
                &key.secret[..10]
            );
        }
        println!("Expected key_with_credits secret: {}...", &key_with_credits.secret[..10]);
        println!("Expected key_without_credits secret: {}...", &key_without_credits.secret[..10]);

        // Should include system user key + key_with_credits, but NOT key_without_credits
        assert!(
            keys_for_deployment.iter().any(|k| k.secret == key_with_credits.secret),
            "User with sufficient credits should have access"
        );

        assert!(
            !keys_for_deployment.iter().any(|k| k.secret == key_without_credits.secret),
            "User with insufficient credits (balance = 0) should NOT have access"
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_api_keys_filtered_by_credits(pool: PgPool) {
        let setup = setup_credit_filtering_test(&pool).await;
        let mut pool_conn = pool.acquire().await.unwrap();

        // Create a Platform Manager user with zero credits
        let platform_manager;
        {
            let mut user_repo = Users::new(&mut pool_conn);
            let user_create = UserCreateDBRequest::from(UserCreate {
                username: "platform".to_string(),
                email: "platform@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::PlatformManager],
            });
            platform_manager = user_repo.create(&user_create).await.unwrap();
        }

        // Add the Platform Manager to the group
        {
            let mut group_repo = Groups::new(&mut pool_conn);
            group_repo.add_user_to_group(platform_manager.id, setup.group.id).await.unwrap();
        }

        // Create API key for Platform Manager
        let platform_key;
        {
            let mut api_key_repo = ApiKeys::new(&mut pool_conn);
            platform_key = api_key_repo
                .create(&ApiKeyCreateDBRequest {
                    user_id: platform_manager.id,
                    name: "Platform Manager Key".to_string(),
                    description: None,
                    requests_per_second: None,
                    burst_size: None,
                    purpose: ApiKeyPurpose::Inference,
                })
                .await
                .unwrap();
        }

        // Retrieve API keys for the deployment - Platform Manager has no credits
        let keys_for_deployment;
        {
            let mut api_repo = ApiKeys::new(&mut pool_conn);
            keys_for_deployment = api_repo
                .get_api_keys_for_deployment_with_sufficient_credit(setup.deployment.id)
                .await
                .unwrap();
        }

        // Platform Manager should NOT have access with zero credits
        assert!(
            !keys_for_deployment.iter().any(|k| k.secret == platform_key.secret),
            "Platform Manager should be filtered when they have zero credits"
        );

        // Now give Platform Manager some credits
        {
            let mut credits_repo = Credits::new(&mut pool_conn);
            credits_repo
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: platform_manager.id,
                    transaction_type: CreditTransactionType::AdminGrant,
                    amount: Decimal::new(10000, 2), // 100.00 credits
                    source_id: "test".to_string(),
                    description: Some("Platform Manager credits".to_string()),
                })
                .await
                .unwrap();
        }

        // Retrieve API keys again - Platform Manager now has credits
        let keys_for_deployment;
        {
            let mut api_repo = ApiKeys::new(&mut pool_conn);
            keys_for_deployment = api_repo
                .get_api_keys_for_deployment_with_sufficient_credit(setup.deployment.id)
                .await
                .unwrap();
        }

        // Platform Manager should now have access with positive credits
        assert!(
            keys_for_deployment.iter().any(|k| k.secret == platform_key.secret),
            "Platform Manager should have access when they have positive credits"
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_users_without_credit_transactions_excluded(pool: PgPool) {
        let setup = setup_credit_filtering_test(&pool).await;
        let mut pool_conn = pool.acquire().await.unwrap();

        // Create a StandardUser with NO credit transactions
        let user_no_transactions;
        {
            let mut user_repo = Users::new(&mut pool_conn);
            let user_create = UserCreateDBRequest::from(UserCreate {
                username: "notransactions".to_string(),
                email: "notransactions@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });
            user_no_transactions = user_repo.create(&user_create).await.unwrap();
        }

        // Add the user to the group
        {
            let mut group_repo = Groups::new(&mut pool_conn);
            group_repo.add_user_to_group(user_no_transactions.id, setup.group.id).await.unwrap();
        }

        // Create API key for user
        let key_no_transactions;
        {
            let mut api_key_repo = ApiKeys::new(&mut pool_conn);
            key_no_transactions = api_key_repo
                .create(&ApiKeyCreateDBRequest {
                    user_id: user_no_transactions.id,
                    name: "No Transactions Key".to_string(),
                    description: None,
                    requests_per_second: None,
                    burst_size: None,
                    purpose: ApiKeyPurpose::Inference,
                })
                .await
                .unwrap();
        }

        // DO NOT create any credit transactions for this user

        // Retrieve API keys for the deployment
        let keys_for_deployment;
        {
            let mut api_repo = ApiKeys::new(&mut pool_conn);
            keys_for_deployment = api_repo
                .get_api_keys_for_deployment_with_sufficient_credit(setup.deployment.id)
                .await
                .unwrap();
        }

        // User without transactions should be excluded (balance considered 0)
        assert!(
            !keys_for_deployment.iter().any(|k| k.secret == key_no_transactions.secret),
            "User without credit transactions should be excluded (balance considered 0)"
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_negative_balance_user_excluded(pool: PgPool) {
        let setup = setup_credit_filtering_test(&pool).await;
        let mut pool_conn = pool.acquire().await.unwrap();

        // Create a StandardUser
        let user_negative;
        {
            let mut user_repo = Users::new(&mut pool_conn);
            let user_create = UserCreateDBRequest::from(UserCreate {
                username: "negative".to_string(),
                email: "negative@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });
            user_negative = user_repo.create(&user_create).await.unwrap();
        }

        // Add the user to the group
        {
            let mut group_repo = Groups::new(&mut pool_conn);
            group_repo.add_user_to_group(user_negative.id, setup.group.id).await.unwrap();
        }

        // Create API key for user
        let key_negative;
        {
            let mut api_key_repo = ApiKeys::new(&mut pool_conn);
            key_negative = api_key_repo
                .create(&ApiKeyCreateDBRequest {
                    user_id: user_negative.id,
                    name: "Negative Balance Key".to_string(),
                    description: None,
                    requests_per_second: None,
                    burst_size: None,
                    purpose: ApiKeyPurpose::Inference,
                })
                .await
                .unwrap();
        }

        // Give user a negative balance by using more credits than they have
        {
            let mut credits_repo = Credits::new(&mut pool_conn);
            // First add some credits
            credits_repo
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: user_negative.id,
                    transaction_type: CreditTransactionType::Purchase,
                    amount: Decimal::new(100, 2),                // +1.00
                    source_id: uuid::Uuid::new_v4().to_string(), // Mimics Stripe payment ID
                    description: Some("Initial credits".to_string()),
                })
                .await
                .unwrap();
            // Then deduct more than available to create negative balance
            credits_repo
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: user_negative.id,
                    transaction_type: CreditTransactionType::Usage,
                    amount: Decimal::new(1100, 2),               // -11.00
                    source_id: uuid::Uuid::new_v4().to_string(), // Mimics request ID from http_analytics
                    description: Some("Negative balance".to_string()),
                })
                .await
                .unwrap();
        }

        // Retrieve API keys for the deployment
        let keys_for_deployment;
        {
            let mut api_repo = ApiKeys::new(&mut pool_conn);
            keys_for_deployment = api_repo
                .get_api_keys_for_deployment_with_sufficient_credit(setup.deployment.id)
                .await
                .unwrap();
        }

        // User with negative balance should be excluded
        assert!(
            !keys_for_deployment.iter().any(|k| k.secret == key_negative.secret),
            "User with negative balance should be excluded"
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_user_regains_access_after_adding_credits(pool: PgPool) {
        let setup = setup_credit_filtering_test(&pool).await;
        let mut pool_conn = pool.acquire().await.unwrap();

        // Create a StandardUser
        let user;
        {
            let mut user_repo = Users::new(&mut pool_conn);
            let user_create = UserCreateDBRequest::from(UserCreate {
                username: "regains".to_string(),
                email: "regains@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });
            user = user_repo.create(&user_create).await.unwrap();
        }

        // Add the user to the group
        {
            let mut group_repo = Groups::new(&mut pool_conn);
            group_repo.add_user_to_group(user.id, setup.group.id).await.unwrap();
        }

        // Create API key for user
        let key;
        {
            let mut api_key_repo = ApiKeys::new(&mut pool_conn);
            key = api_key_repo
                .create(&ApiKeyCreateDBRequest {
                    user_id: user.id,
                    name: "Regains Access Key".to_string(),
                    description: None,
                    requests_per_second: None,
                    burst_size: None,
                    purpose: ApiKeyPurpose::Inference,
                })
                .await
                .unwrap();
        }

        // Verify user does NOT have access with zero balance
        let keys_no_access;
        {
            let mut api_repo = ApiKeys::new(&mut pool_conn);
            keys_no_access = api_repo
                .get_api_keys_for_deployment_with_sufficient_credit(setup.deployment.id)
                .await
                .unwrap();
        }

        assert!(
            !keys_no_access.iter().any(|k| k.secret == key.secret),
            "User with zero balance should NOT have access"
        );

        // Add credits to user
        {
            let mut credits_repo = Credits::new(&mut pool_conn);
            credits_repo
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: user.id,
                    transaction_type: CreditTransactionType::Purchase,
                    amount: Decimal::new(10000, 2), // +100.00
                    source_id: "test".to_string(),
                    description: Some("Added credits".to_string()),
                })
                .await
                .unwrap();
        }

        // Verify user NOW has access with positive balance
        let keys_with_access;
        {
            let mut api_repo = ApiKeys::new(&mut pool_conn);
            keys_with_access = api_repo
                .get_api_keys_for_deployment_with_sufficient_credit(setup.deployment.id)
                .await
                .unwrap();
        }

        assert!(
            keys_with_access.iter().any(|k| k.secret == key.secret),
            "User should regain access after adding credits"
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_zero_credit_user_can_access_free_model(pool: PgPool) {
        // This test will FAIL initially - demonstrating that zero-credit users
        // currently cannot access free models (but they should be able to)
        let mut tx = pool.begin().await.unwrap();

        // Create users
        let admin_user;
        let user_no_credits;
        {
            let mut user_repo = Users::new(tx.acquire().await.unwrap());

            let admin_create = UserCreateDBRequest::from(UserCreate {
                username: "admin".to_string(),
                email: "admin@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::PlatformManager],
            });
            admin_user = user_repo.create(&admin_create).await.unwrap();

            let user_create = UserCreateDBRequest::from(UserCreate {
                username: "zerocredits".to_string(),
                email: "zerocredits@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });
            user_no_credits = user_repo.create(&user_create).await.unwrap();
        }

        // Create group
        let group;
        {
            let mut group_tx = tx.begin().await.unwrap();
            let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());
            let group_create = GroupCreateDBRequest {
                name: "Free Model Test Group".to_string(),
                description: Some("Testing free model access".to_string()),
                created_by: admin_user.id,
            };
            group = group_repo.create(&group_create).await.unwrap();
            group_tx.commit().await.unwrap();
        }

        // Seed database and get endpoint ID
        let config = crate::test_utils::create_test_config();
        crate::seed_database(&config.model_sources, &pool).await.unwrap();
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create deployment with NO pricing (free model)
        let deployment;
        {
            let mut deployment_tx = tx.begin().await.unwrap();
            let mut deployment_repo = Deployments::new(deployment_tx.acquire().await.unwrap());
            let mut deployment_create = DeploymentCreateDBRequest::builder()
                .created_by(admin_user.id)
                .model_name("free-model".to_string())
                .alias("free-alias".to_string())
                .build();
            deployment_create.hosted_on = test_endpoint_id;
            deployment = deployment_repo.create(&deployment_create).await.unwrap();
            deployment_tx.commit().await.unwrap();
        }

        // Add user to group and deployment to group
        {
            let mut group_tx = tx.begin().await.unwrap();
            let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());
            group_repo.add_user_to_group(user_no_credits.id, group.id).await.unwrap();
            group_repo
                .add_deployment_to_group(deployment.id, group.id, admin_user.id)
                .await
                .unwrap();
            group_tx.commit().await.unwrap();
        }

        // Create API key for zero-credit user
        let mut api_repo = ApiKeys::new(&mut tx);
        let key_no_credits = api_repo
            .create(&ApiKeyCreateDBRequest {
                name: "Zero Credits Key".to_string(),
                description: Some("User with no credits".to_string()),
                user_id: user_no_credits.id,
                requests_per_second: None,
                burst_size: None,
                purpose: ApiKeyPurpose::Inference,
            })
            .await
            .unwrap();

        tx.commit().await.unwrap();

        // User has NO credit transactions - balance is 0

        // Get API keys for the free deployment
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut api_repo = ApiKeys::new(&mut pool_conn);
        let keys_for_deployment = api_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment.id)
            .await
            .unwrap();

        // Debug output
        println!("Keys returned for free model: {}", keys_for_deployment.len());
        for key in &keys_for_deployment {
            println!("  - {} (user: {})", key.name, key.user_id);
        }

        // Zero-credit user SHOULD have access to free models
        assert!(
            keys_for_deployment.iter().any(|k| k.secret == key_no_credits.secret),
            "User with zero credits SHOULD have access to FREE models"
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_zero_credit_user_cannot_access_paid_model(pool: PgPool) {
        // This test should PASS - demonstrating that zero-credit users
        // correctly cannot access paid models
        let mut tx = pool.begin().await.unwrap();

        // Create users
        let admin_user;
        let user_no_credits;
        {
            let mut user_repo = Users::new(tx.acquire().await.unwrap());

            let admin_create = UserCreateDBRequest::from(UserCreate {
                username: "admin".to_string(),
                email: "admin@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::PlatformManager],
            });
            admin_user = user_repo.create(&admin_create).await.unwrap();

            let user_create = UserCreateDBRequest::from(UserCreate {
                username: "zerocredits".to_string(),
                email: "zerocredits@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });
            user_no_credits = user_repo.create(&user_create).await.unwrap();
        }

        // Create group
        let group;
        {
            let mut group_tx = tx.begin().await.unwrap();
            let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());
            let group_create = GroupCreateDBRequest {
                name: "Paid Model Test Group".to_string(),
                description: Some("Testing paid model access".to_string()),
                created_by: admin_user.id,
            };
            group = group_repo.create(&group_create).await.unwrap();
            group_tx.commit().await.unwrap();
        }

        // Seed database and get endpoint ID
        let config = crate::test_utils::create_test_config();
        crate::seed_database(&config.model_sources, &pool).await.unwrap();
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create deployment WITH pricing (paid model)
        let deployment;
        {
            let mut deployment_tx = tx.begin().await.unwrap();
            let mut deployment_repo = Deployments::new(deployment_tx.acquire().await.unwrap());

            use crate::db::models::deployments::{ModelPricing, TokenPricing};
            let pricing = ModelPricing {
                upstream: Some(TokenPricing {
                    input_price_per_token: Some(Decimal::new(1, 6)),  // $0.000001 per token
                    output_price_per_token: Some(Decimal::new(2, 6)), // $0.000002 per token
                }),
                downstream: None,
            };

            let mut deployment_create = DeploymentCreateDBRequest::builder()
                .created_by(admin_user.id)
                .model_name("paid-model".to_string())
                .alias("paid-alias".to_string())
                .pricing(pricing) // Paid model - has pricing
                .build();
            deployment_create.hosted_on = test_endpoint_id;
            deployment = deployment_repo.create(&deployment_create).await.unwrap();
            deployment_tx.commit().await.unwrap();
        }

        // Add user to group and deployment to group
        {
            let mut group_tx = tx.begin().await.unwrap();
            let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());
            group_repo.add_user_to_group(user_no_credits.id, group.id).await.unwrap();
            group_repo
                .add_deployment_to_group(deployment.id, group.id, admin_user.id)
                .await
                .unwrap();
            group_tx.commit().await.unwrap();
        }

        // Create API key for zero-credit user
        let mut api_repo = ApiKeys::new(&mut tx);
        let key_no_credits = api_repo
            .create(&ApiKeyCreateDBRequest {
                name: "Zero Credits Key".to_string(),
                description: Some("User with no credits".to_string()),
                user_id: user_no_credits.id,
                requests_per_second: None,
                burst_size: None,
                purpose: ApiKeyPurpose::Inference,
            })
            .await
            .unwrap();

        tx.commit().await.unwrap();

        // User has NO credit transactions - balance is 0

        // Get API keys for the paid deployment
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut api_repo = ApiKeys::new(&mut pool_conn);
        let keys_for_deployment = api_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment.id)
            .await
            .unwrap();

        // Debug output
        println!("Keys returned for paid model: {}", keys_for_deployment.len());
        for key in &keys_for_deployment {
            println!("  - {} (user: {})", key.name, key.user_id);
        }

        // Zero-credit user should NOT have access to paid models
        assert!(
            !keys_for_deployment.iter().any(|k| k.secret == key_no_credits.secret),
            "User with zero credits should NOT have access to PAID models"
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_zero_credit_user_can_access_zero_price_model(pool: PgPool) {
        // This test verifies that models with explicit $0.00 pricing (not NULL)
        // are also accessible to zero-credit users
        let mut tx = pool.begin().await.unwrap();

        // Create users
        let admin_user;
        let user_no_credits;
        {
            let mut user_repo = Users::new(tx.acquire().await.unwrap());

            let admin_create = UserCreateDBRequest::from(UserCreate {
                username: "admin".to_string(),
                email: "admin@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::PlatformManager],
            });
            admin_user = user_repo.create(&admin_create).await.unwrap();

            let user_create = UserCreateDBRequest::from(UserCreate {
                username: "zerocredits".to_string(),
                email: "zerocredits@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });
            user_no_credits = user_repo.create(&user_create).await.unwrap();
        }

        // Create group
        let group;
        {
            let mut group_tx = tx.begin().await.unwrap();
            let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());
            let group_create = GroupCreateDBRequest {
                name: "Zero Price Model Test Group".to_string(),
                description: Some("Testing zero-price model access".to_string()),
                created_by: admin_user.id,
            };
            group = group_repo.create(&group_create).await.unwrap();
            group_tx.commit().await.unwrap();
        }

        // Seed database and get endpoint ID
        let config = crate::test_utils::create_test_config();
        crate::seed_database(&config.model_sources, &pool).await.unwrap();
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create deployment with explicit zero pricing (free model)
        let deployment;
        {
            let mut deployment_tx = tx.begin().await.unwrap();
            let mut deployment_repo = Deployments::new(deployment_tx.acquire().await.unwrap());

            use crate::db::models::deployments::{ModelPricing, TokenPricing};
            let pricing = ModelPricing {
                upstream: Some(TokenPricing {
                    input_price_per_token: Some(Decimal::ZERO),  // Explicit $0.00
                    output_price_per_token: Some(Decimal::ZERO), // Explicit $0.00
                }),
                downstream: None,
            };

            let mut deployment_create = DeploymentCreateDBRequest::builder()
                .created_by(admin_user.id)
                .model_name("zero-price-model".to_string())
                .alias("zero-price-alias".to_string())
                .pricing(pricing) // Free model with explicit zero pricing
                .build();
            deployment_create.hosted_on = test_endpoint_id;
            deployment = deployment_repo.create(&deployment_create).await.unwrap();
            deployment_tx.commit().await.unwrap();
        }

        // Add user to group and deployment to group
        {
            let mut group_tx = tx.begin().await.unwrap();
            let mut group_repo = Groups::new(group_tx.acquire().await.unwrap());
            group_repo.add_user_to_group(user_no_credits.id, group.id).await.unwrap();
            group_repo
                .add_deployment_to_group(deployment.id, group.id, admin_user.id)
                .await
                .unwrap();
            group_tx.commit().await.unwrap();
        }

        // Create API key for zero-credit user
        let mut api_repo = ApiKeys::new(&mut tx);
        let key_no_credits = api_repo
            .create(&ApiKeyCreateDBRequest {
                name: "Zero Credits Key".to_string(),
                description: Some("User with no credits".to_string()),
                user_id: user_no_credits.id,
                requests_per_second: None,
                burst_size: None,
                purpose: ApiKeyPurpose::Inference,
            })
            .await
            .unwrap();

        tx.commit().await.unwrap();

        // User has NO credit transactions - balance is 0

        // Get API keys for the zero-price deployment
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut api_repo = ApiKeys::new(&mut pool_conn);
        let keys_for_deployment = api_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment.id)
            .await
            .unwrap();

        // Debug output
        println!("Keys returned for zero-price model: {}", keys_for_deployment.len());
        for key in &keys_for_deployment {
            println!("  - {} (user: {})", key.name, key.user_id);
        }

        // Zero-credit user SHOULD have access to models with explicit $0.00 pricing
        assert!(
            keys_for_deployment.iter().any(|k| k.secret == key_no_credits.secret),
            "User with zero credits SHOULD have access to models with explicit $0.00 pricing"
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_user_id_by_secret_success(pool: PgPool) {
        let mut tx = pool.begin().await.unwrap();
        let user;
        let api_key;

        {
            let mut user_repo = Users::new(tx.acquire().await.unwrap());

            let user_create = UserCreateDBRequest::from(UserCreate {
                username: "secretuser".to_string(),
                email: "secret@example.com".to_string(),
                display_name: None,
                avatar_url: None,
                roles: vec![Role::StandardUser],
            });

            user = user_repo.create(&user_create).await.unwrap();
        }

        {
            let mut api_repo = ApiKeys::new(tx.acquire().await.unwrap());

            let api_key_create = ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Test Secret Key".to_string(),
                description: Some("Key for testing secret lookup".to_string()),
                purpose: ApiKeyPurpose::Inference,
                requests_per_second: None,
                burst_size: None,
            };

            api_key = api_repo.create(&api_key_create).await.unwrap();
        }

        tx.commit().await.unwrap();

        // Look up user by API key secret
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut api_repo = ApiKeys::new(&mut pool_conn);
        let result = api_repo.get_user_id_by_secret(&api_key.secret).await.unwrap();

        // Should return the correct user ID
        assert_eq!(result, Some(user.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_user_id_by_secret_not_found(pool: PgPool) {
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut api_repo = ApiKeys::new(&mut pool_conn);

        // Look up with a non-existent secret
        let result = api_repo.get_user_id_by_secret("nonexistent_secret_key").await.unwrap();

        // Should return None
        assert_eq!(result, None);
    }
}
