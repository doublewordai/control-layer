//! Database repository for inference endpoints.

use crate::db::errors::{DbError, Result};
use crate::db::handlers::repository::Repository;
use crate::db::models::inference_endpoints::{
    InferenceEndpointCreateDBRequest, InferenceEndpointDBResponse, InferenceEndpointUpdateDBRequest,
};
use crate::types::{abbrev_uuid, InferenceEndpointId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection};
use tracing::instrument;

/// Filter for listing inference endpoints
#[derive(Debug, Clone)]
pub struct InferenceEndpointFilter {
    pub skip: i64,
    pub limit: i64,
}

impl InferenceEndpointFilter {
    pub fn new(skip: i64, limit: i64) -> Self {
        Self { skip, limit }
    }
}

// Database entity model
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
struct InferenceEndpoint {
    pub id: InferenceEndpointId,
    pub name: String,
    pub description: Option<String>,
    pub url: String,
    pub api_key: Option<String>,
    pub model_filter: Option<Vec<String>>,
    pub auth_header_name: String,
    pub auth_header_prefix: String,
    pub created_by: UserId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TryFrom<InferenceEndpoint> for InferenceEndpointDBResponse {
    type Error = anyhow::Error;

    fn try_from(src: InferenceEndpoint) -> std::result::Result<Self, Self::Error> {
        Ok(Self {
            id: src.id,
            name: src.name,
            description: src.description,
            url: src.url.parse()?, // url::Url from String
            api_key: src.api_key,
            model_filter: src.model_filter,
            auth_header_name: src.auth_header_name,
            auth_header_prefix: src.auth_header_prefix,
            created_by: src.created_by,
            created_at: src.created_at,
            updated_at: src.updated_at,
        })
    }
}

pub struct InferenceEndpoints<'c> {
    db: &'c mut PgConnection,
}

#[async_trait::async_trait] // consider #[async_trait(?Send)] if Send bounds bite
impl<'c> Repository for InferenceEndpoints<'c> {
    type CreateRequest = InferenceEndpointCreateDBRequest;
    type UpdateRequest = InferenceEndpointUpdateDBRequest;
    type Response = InferenceEndpointDBResponse;
    type Id = InferenceEndpointId;
    type Filter = InferenceEndpointFilter;

    #[instrument(skip(self, request), fields(name = %request.name, url = %request.url), err)]
    async fn create(&mut self, request: &Self::CreateRequest) -> Result<Self::Response> {
        // created_at and updated_at use database DEFAULT NOW() for consistency
        let endpoint = sqlx::query_as!(
            InferenceEndpoint,
            r#"
            INSERT INTO inference_endpoints (name, description, url, api_key, model_filter, auth_header_name, auth_header_prefix, created_by)
            VALUES ($1, $2, $3, $4, $5, COALESCE($6, 'Authorization'), COALESCE($7, 'Bearer '), $8)
            RETURNING *
            "#,
            request.name,
            request.description,
            request.url.as_str(),
            request.api_key,
            request.model_filter.as_deref(),
            request.auth_header_name,
            request.auth_header_prefix,
            request.created_by
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(endpoint.try_into()?)
    }

    #[instrument(skip(self), fields(endpoint_id = %abbrev_uuid(&id)), err)]
    async fn get_by_id(&mut self, id: Self::Id) -> Result<Option<Self::Response>> {
        let endpoint = sqlx::query_as!(InferenceEndpoint, "SELECT * FROM inference_endpoints WHERE id = $1", id)
            .fetch_optional(&mut *self.db)
            .await?;

        match endpoint {
            Some(e) => Ok(Some(e.try_into()?)),
            None => Ok(None),
        }
    }

    #[instrument(skip(self, ids), fields(count = ids.len()), err)]
    async fn get_bulk(&mut self, ids: Vec<Self::Id>) -> Result<std::collections::HashMap<Self::Id, Self::Response>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let rows = sqlx::query!("SELECT * FROM inference_endpoints WHERE id = ANY($1)", &ids)
            .fetch_all(&mut *self.db)
            .await?;

        let endpoints: Vec<InferenceEndpoint> = rows
            .into_iter()
            .map(|row| InferenceEndpoint {
                id: row.id,
                name: row.name,
                description: row.description,
                url: row.url,
                api_key: row.api_key,
                model_filter: row.model_filter,
                auth_header_name: row.auth_header_name,
                auth_header_prefix: row.auth_header_prefix,
                created_by: row.created_by,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
            .collect();

        let mut result = std::collections::HashMap::new();
        for endpoint in endpoints {
            result.insert(endpoint.id, endpoint.try_into()?);
        }

        Ok(result)
    }

    #[instrument(skip(self), fields(endpoint_id = %abbrev_uuid(&id)), err)]
    async fn delete(&mut self, id: Self::Id) -> Result<bool> {
        let result = sqlx::query!("DELETE FROM inference_endpoints WHERE id = $1", id)
            .execute(&mut *self.db)
            .await?;

        Ok(result.rows_affected() > 0)
    }

    #[instrument(skip(self, request), fields(endpoint_id = %abbrev_uuid(&id)), err)]
    async fn update(&mut self, id: Self::Id, request: &Self::UpdateRequest) -> Result<Self::Response> {
        // Atomic update with conditional field updates
        let endpoint = sqlx::query_as!(
            InferenceEndpoint,
            r#"
            UPDATE inference_endpoints SET
                name = COALESCE($2, name),
                description = CASE
                    WHEN $3::text IS NOT NULL THEN $3
                    ELSE description
                END,
                url = COALESCE($4, url),
                api_key = CASE
                    WHEN $5::text IS NOT NULL THEN $5
                    ELSE api_key
                END,
                model_filter = CASE
                    WHEN $6::text[] IS NOT NULL THEN $6
                    ELSE model_filter
                END,
                auth_header_name = COALESCE($7, auth_header_name),
                auth_header_prefix = COALESCE($8, auth_header_prefix),
                updated_at = NOW()
            WHERE id = $1
            RETURNING *
            "#,
            id,
            request.name,
            request.description.as_deref(),
            request.url.as_ref().map(|u| u.as_str()),
            request.api_key.as_ref().and_then(|opt| opt.as_deref()),
            request.model_filter.as_ref().and_then(|opt| opt.as_ref().map(|v| v.as_slice())),
            request.auth_header_name,
            request.auth_header_prefix
        )
        .fetch_optional(&mut *self.db)
        .await?
        .ok_or_else(|| DbError::NotFound)?;

        Ok(endpoint.try_into()?)
    }

    #[instrument(skip(self, filter), fields(limit = filter.limit, skip = filter.skip), err)]
    async fn list(&mut self, filter: &Self::Filter) -> Result<Vec<Self::Response>> {
        let endpoints = sqlx::query_as!(
            InferenceEndpoint,
            "SELECT * FROM inference_endpoints ORDER BY created_at DESC LIMIT $1 OFFSET $2",
            filter.limit,
            filter.skip
        )
        .fetch_all(&mut *self.db)
        .await?;

        endpoints.into_iter().map(|e| Ok(e.try_into()?)).collect()
    }
}

impl<'c> InferenceEndpoints<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    /// Returns the ID of the default inference endpoint
    pub fn default_endpoint_id() -> InferenceEndpointId {
        // Use a deterministic UUID for tests
        uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::models::users::{Role, UserCreate},
        db::{
            handlers::{repository::Repository, Users},
            models::{
                inference_endpoints::{InferenceEndpointCreateDBRequest, InferenceEndpointUpdateDBRequest},
                users::UserCreateDBRequest,
            },
        },
    };
    use sqlx::PgPool;

    async fn create_test_user(pool: &PgPool) -> crate::api::models::users::UserResponse {
        let mut user_conn = pool.acquire().await.unwrap();
        let mut user_repo = Users::new(&mut user_conn);
        let user_create = UserCreateDBRequest::from(UserCreate {
            username: format!("testuser_{}", uuid::Uuid::new_v4()),
            email: format!("test_{}@example.com", uuid::Uuid::new_v4()),
            display_name: None,
            avatar_url: None,
            roles: vec![Role::StandardUser],
        });
        user_repo.create(&user_create).await.unwrap().into()
    }

    fn create_test_endpoint_request(created_by: uuid::Uuid, name: &str) -> InferenceEndpointCreateDBRequest {
        InferenceEndpointCreateDBRequest {
            name: name.to_string(),
            description: Some(format!("Test endpoint: {name}")),
            url: "https://api.example.com".parse().unwrap(),
            api_key: Some("test-api-key".to_string()),
            model_filter: Some(vec!["gpt-4".to_string(), "gpt-3.5-turbo".to_string()]),
            auth_header_name: None,
            auth_header_prefix: None,
            created_by,
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_bulk_empty_ids(pool: PgPool) {
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = InferenceEndpoints::new(&mut conn);
        let result = repo.get_bulk(vec![]).await.unwrap();
        assert!(result.is_empty());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_bulk_single_endpoint(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = InferenceEndpoints::new(&mut conn);

        // Create a test endpoint
        let endpoint_request = create_test_endpoint_request(user.id, "bulk-test-endpoint");
        let created_endpoint = repo.create(&endpoint_request).await.unwrap();

        // Test get_bulk with single ID
        let result = repo.get_bulk(vec![created_endpoint.id]).await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains_key(&created_endpoint.id));

        let retrieved_endpoint = &result[&created_endpoint.id];
        assert_eq!(retrieved_endpoint.name, "bulk-test-endpoint");
        assert_eq!(
            retrieved_endpoint.description,
            Some("Test endpoint: bulk-test-endpoint".to_string())
        );
        assert_eq!(retrieved_endpoint.url.as_str(), "https://api.example.com/");
        assert_eq!(retrieved_endpoint.api_key, Some("test-api-key".to_string()));
        assert_eq!(
            retrieved_endpoint.model_filter,
            Some(vec!["gpt-4".to_string(), "gpt-3.5-turbo".to_string()])
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_bulk_multiple_endpoints(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = InferenceEndpoints::new(&mut conn);

        // Create multiple test endpoints
        let endpoint1_request = create_test_endpoint_request(user.id, "bulk-endpoint-1");
        let endpoint2_request = create_test_endpoint_request(user.id, "bulk-endpoint-2");
        let endpoint3_request = create_test_endpoint_request(user.id, "bulk-endpoint-3");

        let endpoint1 = repo.create(&endpoint1_request).await.unwrap();
        let endpoint2 = repo.create(&endpoint2_request).await.unwrap();
        let endpoint3 = repo.create(&endpoint3_request).await.unwrap();

        // Test get_bulk with multiple IDs
        let result = repo.get_bulk(vec![endpoint1.id, endpoint2.id, endpoint3.id]).await.unwrap();
        assert_eq!(result.len(), 3);

        assert!(result.contains_key(&endpoint1.id));
        assert!(result.contains_key(&endpoint2.id));
        assert!(result.contains_key(&endpoint3.id));

        assert_eq!(result[&endpoint1.id].name, "bulk-endpoint-1");
        assert_eq!(result[&endpoint2.id].name, "bulk-endpoint-2");
        assert_eq!(result[&endpoint3.id].name, "bulk-endpoint-3");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_bulk_nonexistent_ids(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = InferenceEndpoints::new(&mut conn);

        // Create one endpoint
        let endpoint_request = create_test_endpoint_request(user.id, "existing-endpoint");
        let existing_endpoint = repo.create(&endpoint_request).await.unwrap();

        // Test get_bulk with mix of existing and non-existing IDs
        let fake_id = uuid::Uuid::new_v4();
        let result = repo.get_bulk(vec![existing_endpoint.id, fake_id]).await.unwrap();

        // Should only return the existing endpoint
        assert_eq!(result.len(), 1);
        assert!(result.contains_key(&existing_endpoint.id));
        assert!(!result.contains_key(&fake_id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_existing_endpoint(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = InferenceEndpoints::new(&mut conn);

        // Create a test endpoint
        let endpoint_request = create_test_endpoint_request(user.id, "delete-test-endpoint");
        let created_endpoint = repo.create(&endpoint_request).await.unwrap();

        // Verify it exists
        let found_endpoint = repo.get_by_id(created_endpoint.id).await.unwrap();
        assert!(found_endpoint.is_some());

        // Delete the endpoint
        let deleted = repo.delete(created_endpoint.id).await.unwrap();
        assert!(deleted);

        // Verify it's gone
        let found_endpoint = repo.get_by_id(created_endpoint.id).await.unwrap();
        assert!(found_endpoint.is_none());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_nonexistent_endpoint(pool: PgPool) {
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = InferenceEndpoints::new(&mut conn);
        let fake_id = uuid::Uuid::new_v4();

        // Try to delete non-existent endpoint
        let deleted = repo.delete(fake_id).await.unwrap();
        assert!(!deleted);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_apply_update_all_fields(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = InferenceEndpoints::new(&mut conn);

        // Create a test endpoint
        let endpoint_request = create_test_endpoint_request(user.id, "update-test-endpoint");
        let created_endpoint = repo.create(&endpoint_request).await.unwrap();

        // Create update request with all fields
        let update_request = InferenceEndpointUpdateDBRequest {
            name: Some("updated-endpoint-name".to_string()),
            description: Some("Updated description".to_string()),
            url: Some("https://updated.example.com".parse().unwrap()),
            api_key: Some(Some("updated-api-key".to_string())),
            model_filter: Some(Some(vec!["claude-3".to_string(), "gpt-4-turbo".to_string()])),
            auth_header_name: None,
            auth_header_prefix: None,
        };

        // Apply update
        let updated_endpoint = repo.update(created_endpoint.id, &update_request).await.unwrap();

        // Verify all fields were updated
        assert_eq!(updated_endpoint.name, "updated-endpoint-name");
        assert_eq!(updated_endpoint.description, Some("Updated description".to_string()));
        assert_eq!(updated_endpoint.url.as_str(), "https://updated.example.com/");
        assert_eq!(updated_endpoint.api_key, Some("updated-api-key".to_string()));
        assert_eq!(
            updated_endpoint.model_filter,
            Some(vec!["claude-3".to_string(), "gpt-4-turbo".to_string()])
        );

        // Verify timestamp was updated
        assert!(updated_endpoint.updated_at >= created_endpoint.updated_at);

        // Verify other fields stayed the same
        assert_eq!(updated_endpoint.id, created_endpoint.id);
        assert_eq!(updated_endpoint.created_by, created_endpoint.created_by);
        assert_eq!(updated_endpoint.created_at, created_endpoint.created_at);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_apply_update_partial_fields(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = InferenceEndpoints::new(&mut conn);

        // Create a test endpoint
        let endpoint_request = create_test_endpoint_request(user.id, "partial-update-endpoint");
        let created_endpoint = repo.create(&endpoint_request).await.unwrap();

        // Create update request with only some fields
        let update_request = InferenceEndpointUpdateDBRequest {
            name: Some("partially-updated-name".to_string()),
            description: None,
            url: None,
            api_key: Some(Some("new-api-key".to_string())),
            model_filter: None,
            auth_header_name: None,
            auth_header_prefix: None,
        };

        // Apply update
        let updated_endpoint = repo.update(created_endpoint.id, &update_request).await.unwrap();

        // Verify only specified fields were updated
        assert_eq!(updated_endpoint.name, "partially-updated-name");
        assert_eq!(updated_endpoint.api_key, Some("new-api-key".to_string()));

        // Verify unchanged fields
        assert_eq!(updated_endpoint.description, created_endpoint.description);
        assert_eq!(updated_endpoint.url, created_endpoint.url);
        assert_eq!(updated_endpoint.model_filter, created_endpoint.model_filter);

        // Verify timestamp was updated
        assert!(updated_endpoint.updated_at >= created_endpoint.updated_at);
    }

    /// Mock function that simulates COALESCE behavior for updates
    fn mock_coalesce_update(
        update_request: InferenceEndpointUpdateDBRequest,
        mut original: InferenceEndpointDBResponse,
    ) -> InferenceEndpointDBResponse {
        // COALESCE behavior: use update value if Some, otherwise keep original
        if let Some(name) = update_request.name {
            original.name = name;
        }
        if let Some(description) = update_request.description {
            original.description = Some(description);
        }
        if let Some(url) = update_request.url {
            original.url = url;
        }
        if let Some(api_key) = update_request.api_key {
            original.api_key = api_key;
        }
        if let Some(model_filter) = update_request.model_filter {
            original.model_filter = model_filter;
        }
        if let Some(auth_header_name) = update_request.auth_header_name {
            original.auth_header_name = auth_header_name;
        }
        if let Some(auth_header_prefix) = update_request.auth_header_prefix {
            original.auth_header_prefix = auth_header_prefix;
        }

        // Always update the timestamp like COALESCE would with NOW()
        original.updated_at = chrono::Utc::now();

        original
    }

    #[test]
    fn test_apply_update_trait_directly() {
        // Create a mock response object
        let original_response = InferenceEndpointDBResponse {
            id: uuid::Uuid::new_v4(),
            name: "original-name".to_string(),
            description: Some("original description".to_string()),
            url: "https://original.example.com".parse().unwrap(),
            api_key: Some("original-key".to_string()),
            model_filter: Some(vec!["gpt-3.5".to_string()]),
            auth_header_name: "Authorization".to_string(),
            auth_header_prefix: "Bearer ".to_string(),
            created_by: uuid::Uuid::new_v4(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        // Test ApplyUpdate trait directly
        let update_request = InferenceEndpointUpdateDBRequest {
            name: Some("trait-updated-name".to_string()),
            description: Some("trait updated description".to_string()),
            url: None,
            api_key: Some(Some("trait-updated-key".to_string())),
            model_filter: Some(Some(vec!["claude-3".to_string(), "gpt-4".to_string()])),
            auth_header_name: None,
            auth_header_prefix: None,
        };

        let updated_response = mock_coalesce_update(update_request, original_response.clone());

        // Verify updates
        assert_eq!(updated_response.name, "trait-updated-name");
        assert_eq!(updated_response.description, Some("trait updated description".to_string()));
        assert_eq!(updated_response.url, original_response.url); // Should be unchanged
        assert_eq!(updated_response.api_key, Some("trait-updated-key".to_string()));
        assert_eq!(
            updated_response.model_filter,
            Some(vec!["claude-3".to_string(), "gpt-4".to_string()])
        );

        // Verify timestamp was updated
        assert!(updated_response.updated_at >= original_response.updated_at);

        // Verify unchanged fields
        assert_eq!(updated_response.id, original_response.id);
        assert_eq!(updated_response.created_by, original_response.created_by);
        assert_eq!(updated_response.created_at, original_response.created_at);
    }

    #[test]
    fn test_apply_update_empty_update() {
        // Create a mock response object
        let original_response = InferenceEndpointDBResponse {
            id: uuid::Uuid::new_v4(),
            name: "original-name".to_string(),
            description: Some("original description".to_string()),
            url: "https://original.example.com".parse().unwrap(),
            api_key: Some("original-key".to_string()),
            model_filter: Some(vec!["gpt-3.5".to_string()]),
            auth_header_name: "Authorization".to_string(),
            auth_header_prefix: "Bearer ".to_string(),
            created_by: uuid::Uuid::new_v4(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now() - chrono::Duration::seconds(1),
        };

        // Test ApplyUpdate with empty update (all None fields)
        let update_request = InferenceEndpointUpdateDBRequest {
            name: None,
            description: None,
            url: None,
            api_key: None,
            model_filter: None,
            auth_header_name: None,
            auth_header_prefix: None,
        };

        let updated_response = mock_coalesce_update(update_request, original_response.clone());

        // Verify all fields stayed the same except updated_at
        assert_eq!(updated_response.name, original_response.name);
        assert_eq!(updated_response.description, original_response.description);
        assert_eq!(updated_response.url, original_response.url);
        assert_eq!(updated_response.api_key, original_response.api_key);
        assert_eq!(updated_response.model_filter, original_response.model_filter);
        assert_eq!(updated_response.id, original_response.id);
        assert_eq!(updated_response.created_by, original_response.created_by);
        assert_eq!(updated_response.created_at, original_response.created_at);

        // Only timestamp should be updated
        assert!(updated_response.updated_at >= original_response.updated_at);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_nonexistent_endpoint_returns_not_found(pool: PgPool) {
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = InferenceEndpoints::new(&mut conn);
        let fake_id = uuid::Uuid::new_v4();

        let update_request = InferenceEndpointUpdateDBRequest {
            name: Some("should-not-work".to_string()),
            description: None,
            url: None,
            api_key: None,
            model_filter: None,
            auth_header_name: None,
            auth_header_prefix: None,
        };

        let result = repo.update(fake_id, &update_request).await;
        assert!(result.is_err());
        // Should return DbError::NotFound
        match result {
            Err(crate::db::errors::DbError::NotFound) => {
                // Expected error
            }
            _ => panic!("Expected NotFound error"),
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_endpoint_name_conflict(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = InferenceEndpoints::new(&mut conn);

        // Create the first endpoint with a unique name
        let endpoint_request = create_test_endpoint_request(user.id, "conflict-endpoint");
        let created_endpoint = repo.create(&endpoint_request).await.unwrap();
        assert_eq!(created_endpoint.name, "conflict-endpoint");

        // Try to create another endpoint with the same name
        let endpoint_request2 = create_test_endpoint_request(user.id, "conflict-endpoint");
        let result = repo.create(&endpoint_request2).await;

        // Should return a unique violation error
        match result {
            Err(crate::db::errors::DbError::UniqueViolation { .. }) => { /* expected */ }
            _ => panic!("Expected UniqueViolation error"),
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_endpoint_name_unique(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = InferenceEndpoints::new(&mut conn);

        // Create two endpoints with different names
        let endpoint_request1 = create_test_endpoint_request(user.id, "unique-endpoint-1");
        let endpoint_request2 = create_test_endpoint_request(user.id, "unique-endpoint-2");

        let created1 = repo.create(&endpoint_request1).await.unwrap();
        let created2 = repo.create(&endpoint_request2).await.unwrap();

        assert_eq!(created1.name, "unique-endpoint-1");
        assert_eq!(created2.name, "unique-endpoint-2");
    }
}
