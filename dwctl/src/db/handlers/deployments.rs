use crate::db::{
    errors::{DbError, Result},
    handlers::repository::Repository,
    models::deployments::{
        DeploymentCreateDBRequest, DeploymentDBResponse, DeploymentUpdateDBRequest, FlatPricingFields, ModelPricing, ModelStatus, ModelType,
    },
};
use crate::types::{DeploymentId, InferenceEndpointId, UserId};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use sqlx::{query_builder::QueryBuilder, FromRow};

/// Filter options for listing deployments
#[derive(Debug, Clone)]
pub struct DeploymentFilter {
    pub skip: i64,
    pub limit: i64,
    pub endpoint_id: Option<InferenceEndpointId>,
    pub statuses: Option<Vec<ModelStatus>>,
    pub deleted: Option<bool>, // None = show all, Some(false) = show non-deleted only, Some(true) = show deleted only
    pub accessible_to: Option<UserId>, // None = show all deployments, Some(user_id) = show only deployments accessible to that user
    pub aliases: Option<Vec<String>>,
}

impl DeploymentFilter {
    pub fn new(skip: i64, limit: i64) -> Self {
        Self {
            skip,
            limit,
            endpoint_id: None,
            statuses: None,
            deleted: None,       // Default: show all models
            accessible_to: None, // Default: show all deployments
            aliases: None,
        }
    }

    pub fn with_endpoint(mut self, endpoint_id: InferenceEndpointId) -> Self {
        self.endpoint_id = Some(endpoint_id);
        self
    }

    pub fn with_deleted(mut self, deleted: bool) -> Self {
        self.deleted = Some(deleted);
        self
    }

    pub fn with_accessible_to(mut self, user_id: UserId) -> Self {
        self.accessible_to = Some(user_id);
        self
    }

    pub fn with_statuses(mut self, statuses: Vec<ModelStatus>) -> Self {
        self.statuses = Some(statuses);
        self
    }

    pub fn with_aliases(mut self, aliases: Vec<String>) -> Self {
        self.aliases = Some(aliases);
        self
    }
}

/// Result of checking user access to a deployment
/// Contains both deployment info and system API key for middleware optimization
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct DeploymentAccessInfo {
    pub deployment_id: DeploymentId,
    pub deployment_alias: String,
    pub system_api_key: String,
}

// Database entity model
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
struct DeployedModel {
    pub id: DeploymentId,
    pub model_name: String,
    pub alias: String,
    pub description: Option<String>,
    pub r#type: Option<String>,
    pub capabilities: Option<Vec<String>>,
    pub created_by: UserId,
    pub hosted_on: InferenceEndpointId,
    pub status: String,
    pub last_sync: Option<DateTime<Utc>>,
    pub deleted: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub requests_per_second: Option<f32>,
    pub burst_size: Option<i32>,
    // User-facing pricing (always per-token)
    pub upstream_input_price_per_token: Option<Decimal>,
    pub upstream_output_price_per_token: Option<Decimal>,
    // Provider pricing (flexible)
    pub downstream_pricing_mode: Option<String>,
    pub downstream_input_price_per_token: Option<Decimal>,
    pub downstream_output_price_per_token: Option<Decimal>,
    pub downstream_hourly_rate: Option<Decimal>,
    pub downstream_input_token_cost_ratio: Option<Decimal>,
}

pub struct Deployments<'c> {
    db: &'c mut PgConnection,
}

impl From<(Option<ModelType>, DeployedModel)> for DeploymentDBResponse {
    fn from((model_type, m): (Option<ModelType>, DeployedModel)) -> Self {
        // Convert flat database fields to structured pricing
        let pricing = ModelPricing::from_flat_fields(FlatPricingFields {
            upstream_input_price_per_token: m.upstream_input_price_per_token,
            upstream_output_price_per_token: m.upstream_output_price_per_token,
            downstream_pricing_mode: m.downstream_pricing_mode,
            downstream_input_price_per_token: m.downstream_input_price_per_token,
            downstream_output_price_per_token: m.downstream_output_price_per_token,
            downstream_hourly_rate: m.downstream_hourly_rate,
            downstream_input_token_cost_ratio: m.downstream_input_token_cost_ratio,
        });

        Self {
            id: m.id,
            model_name: m.model_name,
            alias: m.alias,
            description: m.description,
            model_type,
            capabilities: m.capabilities,
            created_by: m.created_by,
            hosted_on: m.hosted_on,
            status: ModelStatus::from_db_string(&m.status),
            last_sync: m.last_sync,
            deleted: m.deleted,
            created_at: m.created_at,
            updated_at: m.updated_at,
            requests_per_second: m.requests_per_second,
            burst_size: m.burst_size,
            pricing,
        }
    }
}

#[async_trait::async_trait]
impl<'c> Repository for Deployments<'c> {
    type CreateRequest = DeploymentCreateDBRequest;
    type UpdateRequest = DeploymentUpdateDBRequest;
    type Response = DeploymentDBResponse;
    type Id = DeploymentId;
    type Filter = DeploymentFilter;

    async fn create(&mut self, request: &Self::CreateRequest) -> Result<Self::Response> {
        let created_at = Utc::now();
        let updated_at = created_at;

        let model_name = request.model_name.trim();
        let alias = request.alias.trim();
        if model_name.is_empty() {
            return Err(DbError::InvalidModelField { field: "model_name" });
        }
        if alias.is_empty() {
            return Err(DbError::InvalidModelField { field: "alias" });
        }

        let model_type_str = request.model_type.as_ref().map(|t| match t {
            ModelType::Chat => "CHAT",
            ModelType::Embeddings => "EMBEDDINGS",
        });

        // Convert structured pricing to flat database fields
        let flat_pricing = request.pricing.as_ref().map(|p| p.to_flat_fields()).unwrap_or_default();

        let model = sqlx::query_as!(
            DeployedModel,
            r#"
            INSERT INTO deployed_models (
                model_name, alias, description, type, capabilities, created_by, hosted_on, created_at, updated_at,
                requests_per_second, burst_size, upstream_input_price_per_token, upstream_output_price_per_token,
                downstream_pricing_mode, downstream_input_price_per_token, downstream_output_price_per_token,
                downstream_hourly_rate, downstream_input_token_cost_ratio
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
            RETURNING *
            "#,
            request.model_name.trim(),
            request.alias.trim(),
            request.description,
            model_type_str,
            request.capabilities.as_ref().map(|caps| caps.as_slice()),
            request.created_by,
            request.hosted_on,
            created_at,
            updated_at,
            request.requests_per_second,
            request.burst_size,
            flat_pricing.upstream_input_price_per_token,
            flat_pricing.upstream_output_price_per_token,
            flat_pricing.downstream_pricing_mode,
            flat_pricing.downstream_input_price_per_token,
            flat_pricing.downstream_output_price_per_token,
            flat_pricing.downstream_hourly_rate,
            flat_pricing.downstream_input_token_cost_ratio
        )
        .fetch_one(&mut *self.db)
        .await?;

        let model_type = model.r#type.as_ref().and_then(|s| match s.as_str() {
            "CHAT" => Some(ModelType::Chat),
            "EMBEDDINGS" => Some(ModelType::Embeddings),
            _ => None,
        });

        Ok(DeploymentDBResponse::from((model_type, model)))
    }

    async fn get_by_id(&mut self, id: Self::Id) -> Result<Option<Self::Response>> {
        let model = sqlx::query_as!(
            DeployedModel,
            "SELECT id, model_name, alias, description, type, capabilities, created_by, hosted_on, status, last_sync, deleted, created_at, updated_at, requests_per_second, burst_size, upstream_input_price_per_token, upstream_output_price_per_token, downstream_pricing_mode, downstream_input_price_per_token, downstream_output_price_per_token, downstream_hourly_rate, downstream_input_token_cost_ratio FROM deployed_models WHERE id = $1",
            id
        )
            .fetch_optional(&mut *self.db)
            .await?;

        let model_type = model.as_ref().and_then(|m| {
            m.r#type.as_ref().and_then(|s| match s.as_str() {
                "CHAT" => Some(ModelType::Chat),
                "EMBEDDINGS" => Some(ModelType::Embeddings),
                _ => None,
            })
        });

        Ok(model.map(|m| DeploymentDBResponse::from((model_type, m))))
    }

    async fn get_bulk(&mut self, ids: Vec<Self::Id>) -> Result<std::collections::HashMap<Self::Id, DeploymentDBResponse>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let deployments = sqlx::query_as!(
            DeployedModel,
            "SELECT id, model_name, alias, description, type, capabilities, created_by, hosted_on, status, last_sync, deleted, created_at, updated_at, requests_per_second, burst_size, upstream_input_price_per_token, upstream_output_price_per_token, downstream_pricing_mode, downstream_input_price_per_token, downstream_output_price_per_token, downstream_hourly_rate, downstream_input_token_cost_ratio FROM deployed_models WHERE id = ANY($1)",
            ids.as_slice()
        )
            .fetch_all(&mut *self.db)
            .await?;

        let mut result = std::collections::HashMap::new();

        for deployment in deployments {
            let model_type = deployment.r#type.as_ref().and_then(|s| match s.as_str() {
                "CHAT" => Some(ModelType::Chat),
                "EMBEDDINGS" => Some(ModelType::Embeddings),
                _ => None,
            });
            result.insert(deployment.id, DeploymentDBResponse::from((model_type, deployment)));
        }

        Ok(result)
    }

    async fn delete(&mut self, id: Self::Id) -> Result<bool> {
        let result = sqlx::query!("DELETE FROM deployed_models WHERE id = $1", id)
            .execute(&mut *self.db)
            .await?;

        Ok(result.rows_affected() > 0)
    }

    async fn update(&mut self, id: Self::Id, request: &Self::UpdateRequest) -> Result<Self::Response> {
        if let Some(model_name) = &request.model_name {
            if model_name.trim().is_empty() {
                return Err(DbError::InvalidModelField { field: "model_name" });
            }
        }
        if let Some(alias) = &request.alias {
            if alias.trim().is_empty() {
                return Err(DbError::InvalidModelField { field: "alias" });
            }
        }

        // Convert model_type into DB string if provided
        let model_type_str: Option<&str> = request.model_type.as_ref().and_then(|inner| {
            inner.as_ref().map(|t| match t {
                ModelType::Chat => "CHAT",
                ModelType::Embeddings => "EMBEDDINGS",
            })
        });

        // Convert status into DB string if provided
        let status_str: Option<String> = request.status.as_ref().map(|s| s.to_db_string().to_string());

        // Convert capabilities to slice if provided
        let capabilities_slice: Option<&[String]> = request.capabilities.as_ref().and_then(|inner| inner.as_ref().map(|v| v.as_slice()));

        // Extract pricing update information using clean intermediate struct
        let pricing_params = request
            .pricing
            .as_ref()
            .map(|pricing_update| pricing_update.to_update_params())
            .unwrap_or_default();

        // Info logging for rate limiting
        tracing::info!(
            "Updating deployment {} - requests_per_second: {:?}, burst_size: {:?}",
            id,
            request.requests_per_second,
            request.burst_size
        );

        let model = sqlx::query_as!(
            DeployedModel,
            r#"
        UPDATE deployed_models SET
            model_name   = COALESCE($2, model_name),
            alias        = COALESCE($3, alias),
            description  = CASE
                WHEN $4 THEN $5
                ELSE description
            END,

            -- Three-state update for model_type
            type = CASE
                WHEN $6 THEN $7
                ELSE type
            END,

            -- Three-state update for capabilities
            capabilities = CASE
                WHEN $8 THEN $9
                ELSE capabilities
            END,

            status     = COALESCE($10, status),
            last_sync  = CASE
                WHEN $11 THEN $12
                ELSE last_sync
            END,
            deleted    = COALESCE($13, deleted),

            -- Three-state update for rate limiting
            requests_per_second = CASE
                WHEN $14 THEN $15
                ELSE requests_per_second
            END,
            burst_size = CASE
                WHEN $16 THEN $17
                ELSE burst_size
            END,

            -- Individual field updates for customer/upstream pricing
            upstream_input_price_per_token = CASE
                WHEN $18 THEN $19
                ELSE upstream_input_price_per_token
            END,
            upstream_output_price_per_token = CASE
                WHEN $20 THEN $21
                ELSE upstream_output_price_per_token
            END,

            -- Individual field updates for downstream pricing
            downstream_pricing_mode = CASE
                WHEN $22 THEN $23
                ELSE downstream_pricing_mode
            END,
            downstream_input_price_per_token = CASE
                WHEN $24 THEN $25
                ELSE downstream_input_price_per_token
            END,
            downstream_output_price_per_token = CASE
                WHEN $26 THEN $27
                ELSE downstream_output_price_per_token
            END,
            downstream_hourly_rate = CASE
                WHEN $28 THEN $29
                ELSE downstream_hourly_rate
            END,
            downstream_input_token_cost_ratio = CASE
                WHEN $30 THEN $31
                ELSE downstream_input_token_cost_ratio
            END,

            updated_at = NOW()
        WHERE id = $1
        RETURNING *
        "#,
            id,                                            // $1
            request.model_name.as_ref().map(|s| s.trim()), // $2
            request.alias.as_ref().map(|s| s.trim()),      // $3
            // For description
            request.description.is_some() as bool,                         // $4
            request.description.as_ref().and_then(|inner| inner.as_ref()), // $5
            // For model_type
            request.model_type.is_some() as bool, // $6
            model_type_str,                       // $7
            // For capabilities
            request.capabilities.is_some() as bool, // $8
            capabilities_slice,                     // $9
            status_str.as_deref(),                  // $10
            // For last_sync
            request.last_sync.is_some() as bool,                         // $11
            request.last_sync.as_ref().and_then(|inner| inner.as_ref()), // $12
            request.deleted,                                             // $13
            // For rate limiting
            request.requests_per_second.is_some() as bool,                         // $14
            request.requests_per_second.as_ref().and_then(|inner| inner.as_ref()), // $15
            request.burst_size.is_some() as bool,                                  // $16
            request.burst_size.as_ref().and_then(|inner| inner.as_ref()),          // $17
            // For individual customer/upstream pricing fields
            pricing_params.should_update_customer_input,  // $18
            pricing_params.customer_input,                // $19
            pricing_params.should_update_customer_output, // $20
            pricing_params.customer_output,               // $21
            // For individual downstream pricing fields
            pricing_params.should_update_downstream_mode,   // $22
            pricing_params.downstream_mode,                 // $23
            pricing_params.should_update_downstream_input,  // $24
            pricing_params.downstream_input,                // $25
            pricing_params.should_update_downstream_output, // $26
            pricing_params.downstream_output,               // $27
            pricing_params.should_update_downstream_hourly, // $28
            pricing_params.downstream_hourly,               // $29
            pricing_params.should_update_downstream_ratio,  // $30
            pricing_params.downstream_ratio                 // $31
        )
        .fetch_one(&mut *self.db)
        .await?;

        // Convert DB model_type back to enum
        let model_type = model.r#type.as_deref().and_then(|s| match s {
            "CHAT" => Some(ModelType::Chat),
            "EMBEDDINGS" => Some(ModelType::Embeddings),
            _ => None,
        });

        Ok(DeploymentDBResponse::from((model_type, model)))
    }

    async fn list(&mut self, filter: &Self::Filter) -> Result<Vec<Self::Response>> {
        let mut query = QueryBuilder::new("SELECT * FROM deployed_models WHERE 1=1");

        // Add endpoint filter if specified
        if let Some(endpoint_id) = filter.endpoint_id {
            query.push(" AND hosted_on = ");
            query.push_bind(endpoint_id);
        }

        // Add status filter if specified
        if let Some(ref statuses) = filter.statuses {
            let status_strings: Vec<String> = statuses.iter().map(|s| s.to_db_string().to_string()).collect();
            query.push(" AND status = ANY(");
            query.push_bind(status_strings);
            query.push(")");
        }

        // Add deleted filter if specified
        if let Some(deleted) = filter.deleted {
            query.push(" AND deleted = ");
            query.push_bind(deleted);
        }

        // Add aliases filter if specified
        if let Some(ref aliases) = filter.aliases {
            if !aliases.is_empty() {
                query.push(" AND alias = ANY(");
                query.push_bind(aliases);
                query.push(")");
            }
        }

        // Add accessibility filter if specified
        if let Some(user_id) = filter.accessible_to {
            query.push(" AND id IN (");
            query.push("SELECT dg.deployment_id FROM deployment_groups dg WHERE dg.group_id IN (");
            query.push("SELECT ug.group_id FROM user_groups ug WHERE ug.user_id = ");
            query.push_bind(user_id);
            query.push(" UNION SELECT '00000000-0000-0000-0000-000000000000'::uuid WHERE ");
            query.push_bind(user_id);
            query.push(" != '00000000-0000-0000-0000-000000000000'::uuid");
            query.push("))");
        }

        // Add ordering and pagination
        query.push(" ORDER BY created_at DESC LIMIT ");
        query.push_bind(filter.limit);
        query.push(" OFFSET ");
        query.push_bind(filter.skip);

        let models = query.build_query_as::<DeployedModel>().fetch_all(&mut *self.db).await?;

        Ok(models
            .into_iter()
            .map(|m| {
                let model_type = m.r#type.as_ref().and_then(|s| match s.as_str() {
                    "CHAT" => Some(ModelType::Chat),
                    "EMBEDDINGS" => Some(ModelType::Embeddings),
                    _ => None,
                });

                DeploymentDBResponse::from((model_type, m))
            })
            .collect())
    }
}

impl<'c> Deployments<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    /// Check if a user has access to a deployment through group membership
    /// Returns deployment info and system API key if access is granted
    pub async fn check_user_access(&mut self, deployment_alias: &str, user_email: &str) -> Result<Option<DeploymentAccessInfo>> {
        let result = sqlx::query_as!(
            DeploymentAccessInfo,
            r#"
            SELECT 
                d.id as deployment_id, 
                d.alias as deployment_alias, 
                ak.secret as system_api_key
            FROM users u
            JOIN deployment_groups dg ON (
                dg.group_id IN (
                    SELECT ug.group_id FROM user_groups ug WHERE ug.user_id = u.id
                    UNION 
                    SELECT '00000000-0000-0000-0000-000000000000'::uuid 
                    WHERE u.id != '00000000-0000-0000-0000-000000000000'
                )
            )
            JOIN deployed_models d ON dg.deployment_id = d.id
            JOIN api_keys ak ON ak.id = '00000000-0000-0000-0000-000000000000'::uuid
            WHERE u.email = $1 AND d.alias = $2
            LIMIT 1
            "#,
            user_email,
            deployment_alias
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::models::users::{Role, UserCreate, UserResponse},
        db::{
            handlers::{inference_endpoints::InferenceEndpoints, Groups, Users},
            models::{
                deployments::{ModelPricing, ModelPricingUpdate, ProviderPricing, ProviderPricingUpdate, TokenPricing, TokenPricingUpdate},
                groups::GroupCreateDBRequest,
                inference_endpoints::InferenceEndpointCreateDBRequest,
                users::UserCreateDBRequest,
            },
        },
        test_utils::get_test_endpoint_id,
    };

    use rust_decimal::Decimal;
    use sqlx::{Acquire, PgPool};
    use std::str::FromStr;

    async fn create_test_user(pool: &PgPool) -> UserResponse {
        let mut conn = pool.acquire().await.unwrap();
        let mut user_repo = Users::new(&mut conn);
        let user_create = UserCreateDBRequest::from(UserCreate {
            username: format!("testuser_{}", uuid::Uuid::new_v4()),
            email: format!("test_{}@example.com", uuid::Uuid::new_v4()),
            display_name: None,
            avatar_url: None,
            roles: vec![Role::StandardUser],
        });
        user_repo.create(&user_create).await.unwrap().into()
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_deployed_model(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let user = create_test_user(&pool).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let model;
        {
            let mut tx = pool.begin().await.unwrap();
            {
                let mut repo = Deployments::new(tx.acquire().await.unwrap());
                let model_create = DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("test-model".to_string())
                    .alias("test-deployment".to_string())
                    .hosted_on(test_endpoint_id)
                    .model_type(ModelType::Chat)
                    .capabilities(vec!["text-generation".to_string(), "streaming".to_string()])
                    .build();

                model = repo.create(&model_create).await.unwrap();
            }
            tx.commit().await.unwrap();
        }
        assert_eq!(model.model_name, "test-model");
        assert_eq!(model.alias, "test-deployment");
        assert_eq!(model.created_by, user.id);
        assert_eq!(model.model_type, Some(ModelType::Chat));
        assert_eq!(
            model.capabilities,
            Some(vec!["text-generation".to_string(), "streaming".to_string()])
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_deployed_model(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let user = create_test_user(&pool).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let created_model;
        let found_model;
        {
            let mut tx = pool.begin().await.unwrap();
            {
                let mut repo = Deployments::new(tx.acquire().await.unwrap());
                let mut model_create = DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("get-test-model".to_string())
                    .alias("get-test-deployment".to_string())
                    .build();
                model_create.hosted_on = test_endpoint_id;

                created_model = repo.create(&model_create).await.unwrap();
                found_model = repo.get_by_id(created_model.id).await.unwrap();
            }
            tx.commit().await.unwrap();
        }

        assert!(found_model.is_some());
        let found_model = found_model.unwrap();
        assert_eq!(found_model.model_name, "get-test-model");
        assert_eq!(found_model.alias, "get-test-deployment");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_deployed_model(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let user = create_test_user(&pool).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let created_model;
        let updated_model;
        {
            let mut tx = pool.begin().await.unwrap();
            {
                let mut repo = Deployments::new(tx.acquire().await.unwrap());

                let model_create = DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("update-test-model".to_string())
                    .alias("update-test-deployment".to_string())
                    .hosted_on(test_endpoint_id)
                    .build();

                created_model = repo.create(&model_create).await.unwrap();

                let update = DeploymentUpdateDBRequest::builder()
                    .model_name("updated-model".to_string())
                    .alias("updated-deployment".to_string())
                    .description(Some("Updated description".to_string()))
                    .model_type(Some(ModelType::Embeddings))
                    .capabilities(Some(vec!["embeddings".to_string(), "similarity".to_string()]))
                    .build();

                updated_model = repo.update(created_model.id, &update).await.unwrap();
            }
            tx.commit().await.unwrap();
        }
        assert_eq!(updated_model.model_name, "updated-model");
        assert_eq!(updated_model.alias, "updated-deployment");
        assert_eq!(updated_model.model_type, Some(ModelType::Embeddings));
        assert_eq!(
            updated_model.capabilities,
            Some(vec!["embeddings".to_string(), "similarity".to_string()])
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_deployed_model_with_null_fields(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let user = create_test_user(&pool).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let model;
        {
            let mut tx = pool.begin().await.unwrap();
            {
                let mut repo = Deployments::new(tx.acquire().await.unwrap());
                // Test creating a model with null type and capabilities (using the builder)
                let mut model_create = DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("null-fields-model".to_string())
                    .alias("null-fields-deployment".to_string())
                    .build();
                model_create.hosted_on = test_endpoint_id;

                model = repo.create(&model_create).await.unwrap();
            }
            tx.commit().await.unwrap();
        }
        assert_eq!(model.model_name, "null-fields-model");
        assert_eq!(model.alias, "null-fields-deployment");
        assert_eq!(model.created_by, user.id);
        assert_eq!(model.model_type, None);
        assert_eq!(model.capabilities, None);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_deployed_model_to_null_fields(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let user = create_test_user(&pool).await;

        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let created_model;
        let updated_model;
        {
            let mut tx = pool.begin().await.unwrap();
            {
                let mut repo = Deployments::new(tx.acquire().await.unwrap());
                // Create a model with type and capabilities
                let mut model_create = DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("to-null-model".to_string())
                    .alias("to-null-deployment".to_string())
                    .build();
                model_create.hosted_on = test_endpoint_id;
                model_create.model_type = Some(ModelType::Chat);
                model_create.capabilities = Some(vec!["test-capability".to_string()]);

                created_model = repo.create(&model_create).await.unwrap();

                // Update to null values
                let update = DeploymentUpdateDBRequest::builder()
                    .maybe_model_type(Some(None))
                    .maybe_capabilities(Some(None))
                    .build();

                updated_model = repo.update(created_model.id, &update).await.unwrap();
            }
            tx.commit().await.unwrap();
        }
        assert_eq!(created_model.model_type, Some(ModelType::Chat));
        assert_eq!(created_model.capabilities, Some(vec!["test-capability".to_string()]));
        assert_eq!(updated_model.model_type, None);
        assert_eq!(updated_model.capabilities, None);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_deployed_model(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let user = create_test_user(&pool).await;

        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let created_model;
        {
            let mut tx = pool.begin().await.unwrap();
            {
                let mut repo = Deployments::new(tx.acquire().await.unwrap());

                let mut model_create = DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("delete-test-model".to_string())
                    .alias("delete-test-deployment".to_string())
                    .build();
                model_create.hosted_on = test_endpoint_id;

                created_model = repo.create(&model_create).await.unwrap();
                let deleted = repo.delete(created_model.id).await.unwrap();
                assert!(deleted);

                let found_model = repo.get_by_id(created_model.id).await.unwrap();
                assert!(found_model.is_none());
            }
            tx.commit().await.unwrap();
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_deployed_models(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let mut pool_conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut pool_conn);

        // Create multiple models
        let user = create_test_user(&pool).await;

        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let mut model1 = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("list-test-model-1".to_string())
            .alias("list-test-deployment-1".to_string())
            .build();
        model1.hosted_on = test_endpoint_id;

        let mut model2 = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("list-test-model-2".to_string())
            .alias("list-test-deployment-2".to_string())
            .build();
        model2.hosted_on = test_endpoint_id;

        repo.create(&model1).await.unwrap();
        repo.create(&model2).await.unwrap();

        let mut models = repo.list(&DeploymentFilter::new(0, 10)).await.unwrap();
        models.sort_by(|a, b| a.model_name.cmp(&b.model_name));
        assert!(models.len() >= 2);
        assert!(models[0].model_name == "list-test-model-1");
        assert!(models[1].model_name == "list-test-model-2");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_with_endpoint_filter(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let mut pool_conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut pool_conn);
        let user = create_test_user(&pool).await;

        // Get the endpoint ID
        let endpoint_id = get_test_endpoint_id(&pool).await;

        let mut model_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("endpoint-filter-model".to_string())
            .alias("endpoint-filter-deployment".to_string())
            .build();
        model_create.hosted_on = endpoint_id;
        let deployment = repo.create(&model_create).await.unwrap();

        // Test filtering by endpoint
        let filter = DeploymentFilter::new(0, 10).with_endpoint(endpoint_id);
        let models = repo.list(&filter).await.unwrap();

        assert!(models.iter().any(|m| m.id == deployment.id));
        assert!(models.iter().all(|m| m.hosted_on == endpoint_id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_with_status_filter(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let mut pool_conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut pool_conn);
        let user = create_test_user(&pool).await;

        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let mut model_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("status-filter-model".to_string())
            .alias("status-filter-deployment".to_string())
            .build();
        model_create.hosted_on = test_endpoint_id;
        let deployment = repo.create(&model_create).await.unwrap();

        // Update deployment to a specific status
        let update = DeploymentUpdateDBRequest::builder().status(ModelStatus::Active).build();
        repo.update(deployment.id, &update).await.unwrap();

        // Test filtering by status
        let mut filter = DeploymentFilter::new(0, 10);
        filter.statuses = Some(vec![ModelStatus::Active]);
        let models = repo.list(&filter).await.unwrap();

        assert!(models.iter().any(|m| m.id == deployment.id));
        assert!(models.iter().all(|m| m.status == ModelStatus::Active));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_with_deleted_filter(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let mut pool_conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut pool_conn);
        let user = create_test_user(&pool).await;

        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let mut model_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("deleted-filter-model".to_string())
            .alias("deleted-filter-deployment".to_string())
            .build();
        model_create.hosted_on = test_endpoint_id;
        let deployment = repo.create(&model_create).await.unwrap();

        // Mark deployment as deleted
        let update = DeploymentUpdateDBRequest::builder().deleted(true).build();
        repo.update(deployment.id, &update).await.unwrap();

        // Test filtering for deleted deployments
        let filter = DeploymentFilter::new(0, 10).with_deleted(true);
        let models = repo.list(&filter).await.unwrap();

        assert!(models.iter().any(|m| m.id == deployment.id));
        assert!(models.iter().all(|m| m.deleted));

        // Test filtering for non-deleted deployments
        let filter = DeploymentFilter::new(0, 10).with_deleted(false);
        let models = repo.list(&filter).await.unwrap();

        assert!(!models.iter().any(|m| m.id == deployment.id));
        assert!(models.iter().all(|m| !m.deleted));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_with_accessible_to_filter(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let mut pool_conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut pool_conn);
        let mut group_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut group_conn);
        let user1 = create_test_user(&pool).await;
        let user2 = create_test_user(&pool).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create deployments
        let mut model1_create = DeploymentCreateDBRequest::builder()
            .created_by(user1.id)
            .model_name("accessible-model-1".to_string())
            .alias("accessible-deployment-1".to_string())
            .build();
        model1_create.hosted_on = test_endpoint_id;
        let mut model2_create = DeploymentCreateDBRequest::builder()
            .created_by(user1.id)
            .model_name("accessible-model-2".to_string())
            .alias("accessible-deployment-2".to_string())
            .build();
        model2_create.hosted_on = test_endpoint_id;
        let deployment1 = repo.create(&model1_create).await.unwrap();
        let deployment2 = repo.create(&model2_create).await.unwrap();

        // Create group and add user1 to it
        let group_create = GroupCreateDBRequest {
            name: "Test Group".to_string(),
            description: Some("Test group for access control".to_string()),
            created_by: user1.id,
        };
        let group = group_repo.create(&group_create).await.unwrap();
        group_repo.add_user_to_group(user1.id, group.id).await.unwrap();

        // Add deployment1 to group (deployment2 stays inaccessible)
        group_repo
            .add_deployment_to_group(deployment1.id, group.id, user1.id)
            .await
            .unwrap();

        // Test that user1 can only see deployment1 when filtering by accessibility
        let filter = DeploymentFilter::new(0, 10).with_accessible_to(user1.id);
        let models = repo.list(&filter).await.unwrap();

        assert!(models.iter().any(|m| m.id == deployment1.id));
        assert!(!models.iter().any(|m| m.id == deployment2.id));

        // Test that user2 cannot see any deployments when filtering by accessibility
        let filter = DeploymentFilter::new(0, 10).with_accessible_to(user2.id);
        let models = repo.list(&filter).await.unwrap();

        assert!(!models.iter().any(|m| m.id == deployment1.id));
        assert!(!models.iter().any(|m| m.id == deployment2.id));

        // Test that without accessibility filter, all deployments are visible
        let filter = DeploymentFilter::new(0, 10);
        let models = repo.list(&filter).await.unwrap();

        assert!(models.iter().any(|m| m.id == deployment1.id));
        assert!(models.iter().any(|m| m.id == deployment2.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_with_combined_filters(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let mut pool_conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut pool_conn);
        let mut group_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut group_conn);

        let user = create_test_user(&pool).await;

        // Get the endpoint ID
        let endpoint_id = get_test_endpoint_id(&pool).await;

        // Create deployment with specific status
        let mut model_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("combined-filter-model".to_string())
            .alias("combined-filter-deployment".to_string())
            .build();
        model_create.hosted_on = endpoint_id;
        let deployment = repo.create(&model_create).await.unwrap();

        // Update to running status
        let update = DeploymentUpdateDBRequest::builder().status(ModelStatus::Active).build();
        repo.update(deployment.id, &update).await.unwrap();

        // Setup access control
        let group_create = GroupCreateDBRequest {
            name: "Combined Filter Group".to_string(),
            description: Some("Test group for combined filters".to_string()),
            created_by: user.id,
        };
        let group = group_repo.create(&group_create).await.unwrap();
        group_repo.add_user_to_group(user.id, group.id).await.unwrap();
        group_repo.add_deployment_to_group(deployment.id, group.id, user.id).await.unwrap();

        // Test combining endpoint, status, and accessibility filters
        let mut filter = DeploymentFilter::new(0, 10).with_endpoint(endpoint_id).with_accessible_to(user.id);
        filter.statuses = Some(vec![ModelStatus::Active]);

        let models = repo.list(&filter).await.unwrap();

        assert!(models.iter().any(|m| m.id == deployment.id));
        assert!(models.iter().all(|m| m.hosted_on == endpoint_id));
        assert!(models.iter().all(|m| m.status == ModelStatus::Active));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_pagination(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let mut pool_conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut pool_conn);
        let user = create_test_user(&pool).await;

        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create 5 test deployments
        for i in 1..=5 {
            let mut model_create = DeploymentCreateDBRequest::builder()
                .created_by(user.id)
                .model_name(format!("pagination-model-{i}"))
                .alias(format!("pagination-deployment-{i}"))
                .build();
            model_create.hosted_on = test_endpoint_id;
            repo.create(&model_create).await.unwrap();
        }

        // Test first page (limit 2)
        let filter = DeploymentFilter::new(0, 2);
        let page1 = repo.list(&filter).await.unwrap();
        assert_eq!(page1.len(), 2);

        // Test second page (skip 2, limit 2)
        let filter = DeploymentFilter::new(2, 2);
        let page2 = repo.list(&filter).await.unwrap();
        assert_eq!(page2.len(), 2);

        // Ensure different results
        let page1_ids: std::collections::HashSet<_> = page1.iter().map(|m| m.id).collect();
        let page2_ids: std::collections::HashSet<_> = page2.iter().map(|m| m.id).collect();
        assert!(page1_ids.is_disjoint(&page2_ids));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_embeddings_deployment(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut conn);
        let user = create_test_user(&pool).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let mut model_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("embeddings-model".to_string())
            .alias("embeddings-deployment".to_string())
            .build();
        model_create.hosted_on = test_endpoint_id;
        model_create.model_type = Some(ModelType::Embeddings);
        model_create.capabilities = Some(vec!["embeddings".to_string(), "similarity".to_string()]);

        let result = repo.create(&model_create).await;
        assert!(result.is_ok());

        let model = result.unwrap();
        assert_eq!(model.model_name, "embeddings-model");
        assert_eq!(model.alias, "embeddings-deployment");
        assert_eq!(model.created_by, user.id);
        assert_eq!(model.model_type, Some(ModelType::Embeddings));
        assert_eq!(model.capabilities, Some(vec!["embeddings".to_string(), "similarity".to_string()]));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_by_id_with_embeddings_type(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut conn);
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let mut model_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("get-embeddings-model".to_string())
            .alias("get-embeddings-deployment".to_string())
            .build();
        model_create.hosted_on = test_endpoint_id;
        model_create.model_type = Some(ModelType::Embeddings);

        let created_model = repo.create(&model_create).await.unwrap();
        let found_model = repo.get_by_id(created_model.id).await.unwrap();

        assert!(found_model.is_some());
        let found_model = found_model.unwrap();
        assert_eq!(found_model.model_name, "get-embeddings-model");
        assert_eq!(found_model.alias, "get-embeddings-deployment");
        assert_eq!(found_model.model_type, Some(ModelType::Embeddings));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_bulk_with_mixed_model_types(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut conn);
        let user = create_test_user(&pool).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create chat deployment
        let mut chat_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("bulk-chat-model".to_string())
            .alias("bulk-chat-deployment".to_string())
            .build();
        chat_create.hosted_on = test_endpoint_id;
        chat_create.model_type = Some(ModelType::Chat);
        let chat_deployment = repo.create(&chat_create).await.unwrap();

        // Create embeddings deployment
        let mut embeddings_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("bulk-embeddings-model".to_string())
            .alias("bulk-embeddings-deployment".to_string())
            .build();
        embeddings_create.hosted_on = test_endpoint_id;
        embeddings_create.model_type = Some(ModelType::Embeddings);
        let embeddings_deployment = repo.create(&embeddings_create).await.unwrap();

        // Create deployment with no type
        let mut no_type_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("bulk-no-type-model".to_string())
            .alias("bulk-no-type-deployment".to_string())
            .build();
        no_type_create.hosted_on = test_endpoint_id;
        let no_type_deployment = repo.create(&no_type_create).await.unwrap();

        // Test bulk retrieval
        let ids = vec![chat_deployment.id, embeddings_deployment.id, no_type_deployment.id];
        let bulk_result = repo.get_bulk(ids).await.unwrap();

        assert_eq!(bulk_result.len(), 3);

        let chat_result = bulk_result.get(&chat_deployment.id).unwrap();
        assert_eq!(chat_result.model_type, Some(ModelType::Chat));

        let embeddings_result = bulk_result.get(&embeddings_deployment.id).unwrap();
        assert_eq!(embeddings_result.model_type, Some(ModelType::Embeddings));

        let no_type_result = bulk_result.get(&no_type_deployment.id).unwrap();
        assert_eq!(no_type_result.model_type, None);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_with_mixed_model_types(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();
        let user = create_test_user(&pool).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut conn);
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create chat deployment
        let mut chat_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("list-chat-model".to_string())
            .alias("list-chat-deployment".to_string())
            .build();
        chat_create.hosted_on = test_endpoint_id;
        chat_create.model_type = Some(ModelType::Chat);
        let chat_deployment = repo.create(&chat_create).await.unwrap();

        // Create embeddings deployment
        let mut embeddings_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("list-embeddings-model".to_string())
            .alias("list-embeddings-deployment".to_string())
            .build();
        embeddings_create.hosted_on = test_endpoint_id;
        embeddings_create.model_type = Some(ModelType::Embeddings);
        let embeddings_deployment = repo.create(&embeddings_create).await.unwrap();

        // List deployments and verify model types are correctly parsed
        let deployments = repo.list(&DeploymentFilter::new(0, 10)).await.unwrap();

        let chat_found = deployments.iter().find(|d| d.id == chat_deployment.id).unwrap();
        assert_eq!(chat_found.model_type, Some(ModelType::Chat));

        let embeddings_found = deployments.iter().find(|d| d.id == embeddings_deployment.id).unwrap();
        assert_eq!(embeddings_found.model_type, Some(ModelType::Embeddings));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_chat_to_embeddings(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();
        let user = create_test_user(&pool).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut conn);
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create chat deployment
        let mut model_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("chat-to-embeddings-model".to_string())
            .alias("chat-to-embeddings-deployment".to_string())
            .build();
        model_create.hosted_on = test_endpoint_id;
        model_create.model_type = Some(ModelType::Chat);
        let created_model = repo.create(&model_create).await.unwrap();
        assert_eq!(created_model.model_type, Some(ModelType::Chat));

        // Update to embeddings
        let update = DeploymentUpdateDBRequest::builder()
            .model_type(Some(ModelType::Embeddings))
            .capabilities(Some(vec!["embeddings".to_string()]))
            .build();

        let updated_model = repo.update(created_model.id, &update).await.unwrap();
        assert_eq!(updated_model.model_type, Some(ModelType::Embeddings));
        assert_eq!(updated_model.capabilities, Some(vec!["embeddings".to_string()]));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_embeddings_to_chat(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut conn);
        let user = create_test_user(&pool).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create embeddings deployment
        let mut model_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("embeddings-to-chat-model".to_string())
            .alias("embeddings-to-chat-deployment".to_string())
            .build();
        model_create.hosted_on = test_endpoint_id;
        model_create.model_type = Some(ModelType::Embeddings);
        let created_model = repo.create(&model_create).await.unwrap();
        assert_eq!(created_model.model_type, Some(ModelType::Embeddings));

        // Update to chat
        let update = DeploymentUpdateDBRequest::builder()
            .model_type(Some(ModelType::Chat))
            .capabilities(Some(vec!["text-generation".to_string(), "streaming".to_string()]))
            .build();

        let updated_model = repo.update(created_model.id, &update).await.unwrap();
        assert_eq!(updated_model.model_type, Some(ModelType::Chat));
        assert_eq!(
            updated_model.capabilities,
            Some(vec!["text-generation".to_string(), "streaming".to_string()])
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_bulk_empty_ids(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut conn);

        // Test empty IDs vector
        let result = repo.get_bulk(vec![]).await.unwrap();
        assert!(result.is_empty());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_check_user_access(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();
        let mut deploy_conn = pool.acquire().await.unwrap();
        let mut deployment_repo = Deployments::new(&mut deploy_conn);
        let mut group_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut group_conn);

        // Create a test user
        let user = create_test_user(&pool).await;

        // The system API key should already exist from application setup,
        // but let's verify and get its current secret for our assertions
        let system_key_result = sqlx::query!(
            "SELECT secret FROM api_keys WHERE id = $1",
            uuid::Uuid::from_u128(0) // 00000000-0000-0000-0000-000000000000
        )
        .fetch_optional(&pool)
        .await
        .expect("Failed to query system API key");

        let system_key_secret = if let Some(key) = system_key_result {
            key.secret
        } else {
            // If system key doesn't exist in test environment, create it
            sqlx::query!(
                "INSERT INTO api_keys (id, name, secret, user_id) VALUES ($1, $2, $3, $4)",
                uuid::Uuid::from_u128(0), // 00000000-0000-0000-0000-000000000000
                "System Key",
                "test_system_secret",
                user.id
            )
            .execute(&pool)
            .await
            .expect("Failed to create system API key");
            "test_system_secret".to_string()
        };

        // Create a deployment
        let test_endpoint_id = get_test_endpoint_id(&pool).await;
        let mut deployment_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("access-test-model".to_string())
            .alias("access-test-alias".to_string())
            .build();
        deployment_create.hosted_on = test_endpoint_id;
        let deployment = deployment_repo.create(&deployment_create).await.unwrap();

        // Create a group
        let group_create = GroupCreateDBRequest {
            name: "Access Test Group".to_string(),
            description: Some("Test group for access control".to_string()),
            created_by: user.id,
        };
        let group = group_repo.create(&group_create).await.unwrap();

        // Test user access without group membership - should return None
        let access_result = deployment_repo.check_user_access("access-test-alias", &user.email).await.unwrap();
        assert!(access_result.is_none());

        // Add user to group
        group_repo
            .add_user_to_group(user.id, group.id)
            .await
            .expect("Failed to add user to group");

        // Test user access without deployment in group - should still return None
        let access_result = deployment_repo.check_user_access("access-test-alias", &user.email).await.unwrap();
        assert!(access_result.is_none());

        // Add deployment to group
        group_repo
            .add_deployment_to_group(deployment.id, group.id, user.id)
            .await
            .expect("Failed to add deployment to group");

        // Test user access with proper group membership - should return access info
        let access_result = deployment_repo.check_user_access("access-test-alias", &user.email).await.unwrap();
        assert!(access_result.is_some());

        let access_info = access_result.unwrap();
        assert_eq!(access_info.deployment_id, deployment.id);
        assert_eq!(access_info.deployment_alias, "access-test-alias");
        assert_eq!(access_info.system_api_key, system_key_secret);

        // Test with non-existent user - should return None
        let access_result = deployment_repo
            .check_user_access("access-test-alias", "nonexistent@example.com")
            .await
            .unwrap();
        assert!(access_result.is_none());

        // Test with non-existent deployment - should return None
        let access_result = deployment_repo.check_user_access("nonexistent-alias", &user.email).await.unwrap();
        assert!(access_result.is_none());

        // Remove user from group and test access again - should return None
        group_repo
            .remove_user_from_group(user.id, group.id)
            .await
            .expect("Failed to remove user from group");

        let access_result = deployment_repo.check_user_access("access-test-alias", &user.email).await.unwrap();
        assert!(access_result.is_none());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_partial_customer_pricing_updates(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let user = create_test_user(&pool).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let created_model;
        {
            let mut tx = pool.begin().await.unwrap();
            {
                let mut repo = Deployments::new(tx.acquire().await.unwrap());

                // Create model with initial pricing
                let initial_pricing = ModelPricing {
                    upstream: Some(TokenPricing {
                        input_price_per_token: Some(Decimal::from_str("0.01").unwrap()),
                        output_price_per_token: Some(Decimal::from_str("0.02").unwrap()),
                    }),
                    downstream: Some(ProviderPricing::PerToken {
                        input_price_per_token: Some(Decimal::from_str("0.005").unwrap()),
                        output_price_per_token: Some(Decimal::from_str("0.01").unwrap()),
                    }),
                };

                let model_create = DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("pricing-test-model".to_string())
                    .alias("pricing-test-deployment".to_string())
                    .hosted_on(test_endpoint_id)
                    .pricing(initial_pricing)
                    .build();

                created_model = repo.create(&model_create).await.unwrap();
            }
            tx.commit().await.unwrap();
        }

        // Verify initial pricing
        assert!(created_model.pricing.is_some());
        if let Some(pricing) = &created_model.pricing {
            if let Some(upstream) = &pricing.upstream {
                assert_eq!(upstream.input_price_per_token, Some(Decimal::from_str("0.01").unwrap()));
                assert_eq!(upstream.output_price_per_token, Some(Decimal::from_str("0.02").unwrap()));
            }
        }

        // Test 1: Update only customer input pricing, leave output unchanged
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut repo = Deployments::new(&mut conn);

            let pricing_update = ModelPricingUpdate {
                upstream: Some(TokenPricingUpdate {
                    input_price_per_token: Some(Some(Decimal::from_str("0.015").unwrap())),
                    output_price_per_token: None, // No change
                }),
                downstream: None, // No downstream changes
            };

            let update = DeploymentUpdateDBRequest::builder().pricing(pricing_update).build();

            let updated_model = repo.update(created_model.id, &update).await.unwrap();

            // Verify partial update worked
            assert!(updated_model.pricing.is_some());
            if let Some(pricing) = &updated_model.pricing {
                if let Some(upstream) = &pricing.upstream {
                    assert_eq!(upstream.input_price_per_token, Some(Decimal::from_str("0.015").unwrap()));
                    assert_eq!(upstream.output_price_per_token, Some(Decimal::from_str("0.02").unwrap()));
                    // Unchanged
                }
                // Downstream should remain unchanged
                assert!(pricing.downstream.is_some());
            }
        }

        // Test 2: Update only customer output pricing, leave input unchanged
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut repo = Deployments::new(&mut conn);

            let pricing_update = ModelPricingUpdate {
                upstream: Some(TokenPricingUpdate {
                    input_price_per_token: None, // No change
                    output_price_per_token: Some(Some(Decimal::from_str("0.025").unwrap())),
                }),
                downstream: None, // No downstream changes
            };

            let update = DeploymentUpdateDBRequest::builder().pricing(pricing_update).build();

            let updated_model = repo.update(created_model.id, &update).await.unwrap();

            // Verify partial update worked
            assert!(updated_model.pricing.is_some());
            if let Some(pricing) = &updated_model.pricing {
                if let Some(upstream) = &pricing.upstream {
                    assert_eq!(upstream.input_price_per_token, Some(Decimal::from_str("0.015").unwrap())); // From previous update
                    assert_eq!(upstream.output_price_per_token, Some(Decimal::from_str("0.025").unwrap()));
                    // New value
                }
            }
        }

        // Test 3: Clear customer input pricing (set to null)
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut repo = Deployments::new(&mut conn);

            let pricing_update = ModelPricingUpdate {
                upstream: Some(TokenPricingUpdate {
                    input_price_per_token: Some(None), // Clear this field
                    output_price_per_token: None,      // No change
                }),
                downstream: None,
            };

            let update = DeploymentUpdateDBRequest::builder().pricing(pricing_update).build();

            let updated_model = repo.update(created_model.id, &update).await.unwrap();

            // Verify clearing worked
            if let Some(pricing) = &updated_model.pricing {
                if let Some(upstream) = &pricing.upstream {
                    assert_eq!(upstream.input_price_per_token, None); // Cleared
                    assert_eq!(upstream.output_price_per_token, Some(Decimal::from_str("0.025").unwrap()));
                    // Unchanged
                }
            }
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_partial_downstream_per_token_pricing_updates(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let user = create_test_user(&pool).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let created_model;
        {
            let mut tx = pool.begin().await.unwrap();
            {
                let mut repo = Deployments::new(tx.acquire().await.unwrap());

                // Create model with initial downstream per-token pricing
                let initial_pricing = ModelPricing {
                    upstream: Some(TokenPricing {
                        input_price_per_token: Some(Decimal::from_str("0.01").unwrap()),
                        output_price_per_token: Some(Decimal::from_str("0.02").unwrap()),
                    }),
                    downstream: Some(ProviderPricing::PerToken {
                        input_price_per_token: Some(Decimal::from_str("0.005").unwrap()),
                        output_price_per_token: Some(Decimal::from_str("0.01").unwrap()),
                    }),
                };

                let model_create = DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("downstream-per-token-test".to_string())
                    .alias("downstream-per-token-alias".to_string())
                    .hosted_on(test_endpoint_id)
                    .pricing(initial_pricing)
                    .build();

                created_model = repo.create(&model_create).await.unwrap();
            }
            tx.commit().await.unwrap();
        }

        // Test 1: Update only downstream input pricing
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut repo = Deployments::new(&mut conn);

            let pricing_update = ModelPricingUpdate {
                upstream: None, // No customer changes
                downstream: Some(ProviderPricingUpdate::PerToken {
                    input_price_per_token: Some(Some(Decimal::from_str("0.003").unwrap())),
                    output_price_per_token: None, // No change
                }),
            };

            let update = DeploymentUpdateDBRequest::builder().pricing(pricing_update).build();

            let updated_model = repo.update(created_model.id, &update).await.unwrap();

            // Verify partial downstream update
            if let Some(pricing) = &updated_model.pricing {
                if let Some(ProviderPricing::PerToken {
                    input_price_per_token,
                    output_price_per_token,
                }) = &pricing.downstream
                {
                    assert_eq!(input_price_per_token, &Some(Decimal::from_str("0.003").unwrap()));
                    assert_eq!(output_price_per_token, &Some(Decimal::from_str("0.01").unwrap()));
                    // Unchanged
                }
                // Customer pricing should remain unchanged
                if let Some(upstream) = &pricing.upstream {
                    assert_eq!(upstream.input_price_per_token, Some(Decimal::from_str("0.01").unwrap()));
                    assert_eq!(upstream.output_price_per_token, Some(Decimal::from_str("0.02").unwrap()));
                }
            }
        }

        // Test 2: Update only downstream output pricing
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut repo = Deployments::new(&mut conn);

            let pricing_update = ModelPricingUpdate {
                upstream: None,
                downstream: Some(ProviderPricingUpdate::PerToken {
                    input_price_per_token: None, // No change
                    output_price_per_token: Some(Some(Decimal::from_str("0.008").unwrap())),
                }),
            };

            let update = DeploymentUpdateDBRequest::builder().pricing(pricing_update).build();

            let updated_model = repo.update(created_model.id, &update).await.unwrap();

            // Verify partial downstream update
            if let Some(pricing) = &updated_model.pricing {
                if let Some(ProviderPricing::PerToken {
                    input_price_per_token,
                    output_price_per_token,
                }) = &pricing.downstream
                {
                    assert_eq!(input_price_per_token, &Some(Decimal::from_str("0.003").unwrap())); // From previous update
                    assert_eq!(output_price_per_token, &Some(Decimal::from_str("0.008").unwrap()));
                    // New value
                }
            }
        }

        // Test 3: Clear downstream input pricing
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut repo = Deployments::new(&mut conn);

            let pricing_update = ModelPricingUpdate {
                upstream: None,
                downstream: Some(ProviderPricingUpdate::PerToken {
                    input_price_per_token: Some(None), // Clear this field
                    output_price_per_token: None,      // No change
                }),
            };

            let update = DeploymentUpdateDBRequest::builder().pricing(pricing_update).build();

            let updated_model = repo.update(created_model.id, &update).await.unwrap();

            // Verify clearing worked
            if let Some(pricing) = &updated_model.pricing {
                if let Some(ProviderPricing::PerToken {
                    input_price_per_token,
                    output_price_per_token,
                }) = &pricing.downstream
                {
                    assert_eq!(input_price_per_token, &None); // Cleared
                    assert_eq!(output_price_per_token, &Some(Decimal::from_str("0.008").unwrap()));
                    // Unchanged
                }
            }
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_downstream_hourly_pricing_updates(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let user = create_test_user(&pool).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let created_model;
        {
            let mut tx = pool.begin().await.unwrap();
            {
                let mut repo = Deployments::new(tx.acquire().await.unwrap());

                // Create model with initial hourly pricing
                let initial_pricing = ModelPricing {
                    upstream: Some(TokenPricing {
                        input_price_per_token: Some(Decimal::from_str("0.01").unwrap()),
                        output_price_per_token: Some(Decimal::from_str("0.02").unwrap()),
                    }),
                    downstream: Some(ProviderPricing::Hourly {
                        rate: Decimal::from_str("5.00").unwrap(),
                        input_token_cost_ratio: Decimal::from_str("0.8").unwrap(),
                    }),
                };

                let model_create = DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("hourly-pricing-test".to_string())
                    .alias("hourly-pricing-alias".to_string())
                    .hosted_on(test_endpoint_id)
                    .pricing(initial_pricing)
                    .build();

                created_model = repo.create(&model_create).await.unwrap();
            }
            tx.commit().await.unwrap();
        }

        // Verify initial hourly pricing
        assert!(created_model.pricing.is_some());
        if let Some(pricing) = &created_model.pricing {
            if let Some(ProviderPricing::Hourly {
                rate,
                input_token_cost_ratio,
            }) = &pricing.downstream
            {
                assert_eq!(rate, &Decimal::from_str("5.00").unwrap());
                assert_eq!(input_token_cost_ratio, &Decimal::from_str("0.8").unwrap());
            }
        }

        // Test 1: Update hourly rate only
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut repo = Deployments::new(&mut conn);

            let pricing_update = ModelPricingUpdate {
                upstream: None, // No customer changes
                downstream: Some(ProviderPricingUpdate::Hourly {
                    rate: Some(Decimal::from_str("6.50").unwrap()),
                    input_token_cost_ratio: None, // Keep existing value
                }),
            };

            let update = DeploymentUpdateDBRequest::builder().pricing(pricing_update).build();

            let updated_model = repo.update(created_model.id, &update).await.unwrap();

            // Verify hourly rate update
            if let Some(pricing) = &updated_model.pricing {
                if let Some(ProviderPricing::Hourly {
                    rate,
                    input_token_cost_ratio,
                }) = &pricing.downstream
                {
                    assert_eq!(rate, &Decimal::from_str("6.50").unwrap()); // Updated
                    assert_eq!(input_token_cost_ratio, &Decimal::from_str("0.8").unwrap());
                    // Should remain unchanged
                }
                // Customer pricing should remain unchanged
                if let Some(upstream) = &pricing.upstream {
                    assert_eq!(upstream.input_price_per_token, Some(Decimal::from_str("0.01").unwrap()));
                    assert_eq!(upstream.output_price_per_token, Some(Decimal::from_str("0.02").unwrap()));
                }
            }
        }

        // Test 2: Update input token cost ratio only
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut repo = Deployments::new(&mut conn);

            let pricing_update = ModelPricingUpdate {
                upstream: None,
                downstream: Some(ProviderPricingUpdate::Hourly {
                    rate: None, // Keep existing value
                    input_token_cost_ratio: Some(Decimal::from_str("0.9").unwrap()),
                }),
            };

            let update = DeploymentUpdateDBRequest::builder().pricing(pricing_update).build();

            let updated_model = repo.update(created_model.id, &update).await.unwrap();

            // Verify input token cost ratio update
            if let Some(pricing) = &updated_model.pricing {
                if let Some(ProviderPricing::Hourly {
                    rate,
                    input_token_cost_ratio,
                }) = &pricing.downstream
                {
                    assert_eq!(rate, &Decimal::from_str("6.50").unwrap()); // From previous update
                    assert_eq!(input_token_cost_ratio, &Decimal::from_str("0.9").unwrap());
                    // Updated
                }
            }
        }

        // Test 3: Update both hourly fields
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut repo = Deployments::new(&mut conn);

            let pricing_update = ModelPricingUpdate {
                upstream: None,
                downstream: Some(ProviderPricingUpdate::Hourly {
                    rate: Some(Decimal::from_str("7.00").unwrap()),
                    input_token_cost_ratio: Some(Decimal::from_str("0.75").unwrap()),
                }),
            };

            let update = DeploymentUpdateDBRequest::builder().pricing(pricing_update).build();

            let updated_model = repo.update(created_model.id, &update).await.unwrap();

            // Verify both fields updated
            if let Some(pricing) = &updated_model.pricing {
                if let Some(ProviderPricing::Hourly {
                    rate,
                    input_token_cost_ratio,
                }) = &pricing.downstream
                {
                    assert_eq!(rate, &Decimal::from_str("7.00").unwrap());
                    assert_eq!(input_token_cost_ratio, &Decimal::from_str("0.75").unwrap());
                }
            }
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_with_inactive_status_filter(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let mut pool_conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut pool_conn);
        let user = create_test_user(&pool).await;

        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create two deployments
        let mut model_create1 = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("active-test-model".to_string())
            .alias("active-test-deployment".to_string())
            .build();
        model_create1.hosted_on = test_endpoint_id;

        let mut model_create2 = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("inactive-test-model".to_string())
            .alias("inactive-test-deployment".to_string())
            .build();
        model_create2.hosted_on = test_endpoint_id;

        let deployment1 = repo.create(&model_create1).await.unwrap();
        let deployment2 = repo.create(&model_create2).await.unwrap();

        // Set deployment1 to Active and deployment2 to Inactive
        let update_active = DeploymentUpdateDBRequest::builder().status(ModelStatus::Active).build();
        repo.update(deployment1.id, &update_active).await.unwrap();

        let update_inactive = DeploymentUpdateDBRequest::builder().status(ModelStatus::Inactive).build();
        repo.update(deployment2.id, &update_inactive).await.unwrap();

        // Test filtering for active models only
        let mut filter = DeploymentFilter::new(0, 10);
        filter.statuses = Some(vec![ModelStatus::Active]);
        let active_models = repo.list(&filter).await.unwrap();

        assert!(active_models.iter().any(|m| m.id == deployment1.id));
        assert!(!active_models.iter().any(|m| m.id == deployment2.id));
        assert!(active_models.iter().all(|m| m.status == ModelStatus::Active));

        // Test filtering for inactive models only
        let mut filter = DeploymentFilter::new(0, 10);
        filter.statuses = Some(vec![ModelStatus::Inactive]);
        let inactive_models = repo.list(&filter).await.unwrap();

        assert!(!inactive_models.iter().any(|m| m.id == deployment1.id));
        assert!(inactive_models.iter().any(|m| m.id == deployment2.id));
        assert!(inactive_models.iter().all(|m| m.status == ModelStatus::Inactive));

        // Test with no status filter - should see both
        let filter = DeploymentFilter::new(0, 10);
        let all_models = repo.list(&filter).await.unwrap();

        assert!(all_models.iter().any(|m| m.id == deployment1.id));
        assert!(all_models.iter().any(|m| m.id == deployment2.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_with_combined_deleted_and_inactive_filters(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let mut pool_conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut pool_conn);
        let user = create_test_user(&pool).await;

        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create deployment
        let mut model_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("combined-filter-model".to_string())
            .alias("combined-filter-deployment".to_string())
            .build();
        model_create.hosted_on = test_endpoint_id;
        let deployment = repo.create(&model_create).await.unwrap();

        // Set deployment to inactive and deleted
        let update = DeploymentUpdateDBRequest::builder()
            .status(ModelStatus::Inactive)
            .deleted(true)
            .build();
        repo.update(deployment.id, &update).await.unwrap();

        // Test filter for non-deleted active models (should not find it)
        let filter = DeploymentFilter::new(0, 10)
            .with_deleted(false)
            .with_statuses(vec![ModelStatus::Active]);
        let models = repo.list(&filter).await.unwrap();
        assert!(!models.iter().any(|m| m.id == deployment.id));

        // Test filter for deleted inactive models (should find it)
        let filter = DeploymentFilter::new(0, 10)
            .with_deleted(true)
            .with_statuses(vec![ModelStatus::Inactive]);
        let models = repo.list(&filter).await.unwrap();
        assert!(models.iter().any(|m| m.id == deployment.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_deployment_alias_conflict(pool: PgPool) {
        let user = create_test_user(&pool).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut endpoints_repo = InferenceEndpoints::new(&mut conn);
        let endpoint_create = InferenceEndpointCreateDBRequest {
            name: format!("test-endpoint-{}", uuid::Uuid::new_v4()),
            url: url::Url::parse("http://localhost:8080").unwrap(),
            api_key: None,
            description: None,
            model_filter: None,
            created_by: user.id,
        };
        let endpoint = endpoints_repo.create(&endpoint_create).await.unwrap();
        let test_endpoint_id = endpoint.id;

        let mut repo = Deployments::new(&mut conn);

        // Create the first deployment with a unique alias
        let model_create1 = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("model-1".to_string())
            .alias("shared-alias".to_string())
            .hosted_on(test_endpoint_id)
            .build();
        let _ = repo.create(&model_create1).await.unwrap();

        // Try to create another deployment with the same alias (should fail)
        let model_create2 = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("model-2".to_string())
            .alias("shared-alias".to_string())
            .hosted_on(test_endpoint_id)
            .build();
        let result = repo.create(&model_create2).await;

        match result {
            Err(crate::db::errors::DbError::UniqueViolation { .. }) => { /* expected */ }
            _ => panic!("Expected UniqueViolation error for alias"),
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_deployment_alias_conflict(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();

        let mut endpoints_repo = InferenceEndpoints::new(&mut conn);
        let endpoint_create = InferenceEndpointCreateDBRequest {
            name: format!("test-endpoint-{}", uuid::Uuid::new_v4()),
            url: url::Url::parse("http://localhost:8080").unwrap(),
            api_key: None,
            description: None,
            model_filter: None,
            created_by: user.id,
        };
        let endpoint = endpoints_repo.create(&endpoint_create).await.unwrap();
        let test_endpoint_id = endpoint.id;

        let mut repo = Deployments::new(&mut conn);

        // Create two deployments with unique aliases
        let model_create1 = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("model-1".to_string())
            .alias("alias-1".to_string())
            .hosted_on(test_endpoint_id)
            .build();
        let _deployment1 = repo.create(&model_create1).await.unwrap();

        let model_create2 = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("model-2".to_string())
            .alias("alias-2".to_string())
            .hosted_on(test_endpoint_id)
            .build();
        let deployment2 = repo.create(&model_create2).await.unwrap();

        // Try to update deployment2 to use alias-1 (should fail)
        let update = DeploymentUpdateDBRequest::builder().alias("alias-1".to_string()).build();
        let result = repo.update(deployment2.id, &update).await;

        match result {
            Err(crate::db::errors::DbError::UniqueViolation { .. }) => { /* expected */ }
            _ => panic!("Expected UniqueViolation error for alias update"),
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_deployment_with_empty_model_name_or_alias(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let user = create_test_user(&pool).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut conn);

        // Empty model name
        let mut model_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("   ".to_string())
            .alias("valid-alias".to_string())
            .hosted_on(test_endpoint_id)
            .build();
        let result = repo.create(&model_create).await;
        match result {
            Err(DbError::InvalidModelField { field }) => assert_eq!(field, "model_name"),
            _ => panic!("Expected InvalidModelField error for empty model_name"),
        }

        // Empty alias
        let mut model_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("valid-model".to_string())
            .alias("   ".to_string())
            .hosted_on(test_endpoint_id)
            .build();
        let result = repo.create(&model_create).await;
        match result {
            Err(DbError::InvalidModelField { field }) => assert_eq!(field, "alias"),
            _ => panic!("Expected InvalidModelField error for empty alias"),
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_deployment_with_empty_model_name_or_alias(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let user = create_test_user(&pool).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut conn);

        // Create a valid deployment first
        let mut model_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("valid-model".to_string())
            .alias("valid-alias".to_string())
            .hosted_on(test_endpoint_id)
            .build();
        let deployment = repo.create(&model_create).await.unwrap();

        // Try to update model_name to empty
        let update = DeploymentUpdateDBRequest::builder().model_name("   ".to_string()).build();
        let result = repo.update(deployment.id, &update).await;
        match result {
            Err(DbError::InvalidModelField { field }) => assert_eq!(field, "model_name"),
            _ => panic!("Expected InvalidModelField error for empty model_name"),
        }

        // Try to update alias to empty
        let update = DeploymentUpdateDBRequest::builder().alias("   ".to_string()).build();
        let result = repo.update(deployment.id, &update).await;
        match result {
            Err(DbError::InvalidModelField { field }) => assert_eq!(field, "alias"),
            _ => panic!("Expected InvalidModelField error for empty alias"),
        }
    }
}
