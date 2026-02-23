//! Database repository for model deployments.

use crate::db::{
    errors::{DbError, Result},
    handlers::repository::Repository,
    models::deployments::{
        DeploymentComponentCreateDBRequest, DeploymentComponentDBResponse, DeploymentCreateDBRequest, DeploymentDBResponse,
        DeploymentUpdateDBRequest, LoadBalancingStrategy, ModelStatus, ModelType, ProviderPricing, ProviderPricingFields,
    },
};
use crate::types::{DeploymentId, InferenceEndpointId, UserId, abbrev_uuid};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use sqlx::{FromRow, query_builder::QueryBuilder};
use tracing::instrument;

/// Filter options for listing deployments
#[derive(Debug, Clone)]
pub struct DeploymentFilter {
    pub skip: i64,
    pub limit: i64,
    pub endpoint_id: Option<InferenceEndpointId>,
    pub statuses: Option<Vec<ModelStatus>>,
    pub deleted: Option<bool>, // None = show all, Some(false) = show non-deleted only, Some(true) = show deleted only
    pub accessible_to: Option<UserId>, // None = show all deployments, Some(user_id) = show only deployments accessible to that user
    pub group_ids: Option<Vec<crate::types::GroupId>>, // None = show all, Some(group_ids) = show only models in any of these groups
    pub aliases: Option<Vec<String>>,
    pub search: Option<String>,     // Case-insensitive substring search on alias and model_name
    pub is_composite: Option<bool>, // None = show all, Some(true) = composite only, Some(false) = non-composite only
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
            group_ids: None,     // Default: show all groups
            aliases: None,
            search: None,
            is_composite: None, // Default: show all models
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

    pub fn with_groups(mut self, group_ids: Vec<crate::types::GroupId>) -> Self {
        self.group_ids = Some(group_ids);
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

    pub fn with_search(mut self, search: String) -> Self {
        self.search = Some(search);
        self
    }

    pub fn with_composite(mut self, is_composite: bool) -> Self {
        self.is_composite = Some(is_composite);
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
    pub hosted_on: Option<InferenceEndpointId>,
    pub status: String,
    pub last_sync: Option<DateTime<Utc>>,
    pub deleted: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub requests_per_second: Option<f32>,
    pub burst_size: Option<i32>,
    pub capacity: Option<i32>,
    pub batch_capacity: Option<i32>,
    pub throughput: Option<f32>,
    // Provider pricing (flexible)
    pub downstream_pricing_mode: Option<String>,
    pub downstream_input_price_per_token: Option<Decimal>,
    pub downstream_output_price_per_token: Option<Decimal>,
    pub downstream_hourly_rate: Option<Decimal>,
    pub downstream_input_token_cost_ratio: Option<Decimal>,
    // Composite model fields
    pub is_composite: bool,
    pub lb_strategy: Option<String>,
    pub fallback_enabled: Option<bool>,
    pub fallback_on_rate_limit: Option<bool>,
    pub fallback_on_status: Option<Vec<i32>>,
    pub fallback_with_replacement: Option<bool>,
    pub fallback_max_attempts: Option<i32>,
    pub sanitize_responses: bool,
    pub trusted: bool,
    pub open_responses_adapter: Option<bool>,
}

pub struct Deployments<'c> {
    db: &'c mut PgConnection,
}

impl From<(Option<ModelType>, DeployedModel)> for DeploymentDBResponse {
    fn from((model_type, m): (Option<ModelType>, DeployedModel)) -> Self {
        let provider_pricing = ProviderPricing::from_flat_fields(ProviderPricingFields {
            mode: m.downstream_pricing_mode,
            input_price_per_token: m.downstream_input_price_per_token,
            output_price_per_token: m.downstream_output_price_per_token,
            hourly_rate: m.downstream_hourly_rate,
            input_token_cost_ratio: m.downstream_input_token_cost_ratio,
        });

        // Parse load balancing strategy (default to WeightedRandom)
        let lb_strategy = m
            .lb_strategy
            .as_deref()
            .and_then(LoadBalancingStrategy::try_parse)
            .unwrap_or_default();

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
            capacity: m.capacity,
            batch_capacity: m.batch_capacity,
            throughput: m.throughput,
            provider_pricing,
            // Composite model fields
            is_composite: m.is_composite,
            lb_strategy,
            fallback_enabled: m.fallback_enabled.unwrap_or(true),
            fallback_on_rate_limit: m.fallback_on_rate_limit.unwrap_or(true),
            fallback_on_status: m.fallback_on_status.unwrap_or_else(|| vec![429, 500, 502, 503, 504]),
            fallback_with_replacement: m.fallback_with_replacement.unwrap_or(false),
            fallback_max_attempts: m.fallback_max_attempts,
            sanitize_responses: m.sanitize_responses,
            trusted: m.trusted,
            open_responses_adapter: m.open_responses_adapter.unwrap_or(true),
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

    #[instrument(skip(self, request), fields(model_name = %request.model_name, alias = %request.alias), err)]
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
            ModelType::Reranker => "RERANKER",
        });

        // Extract provider pricing fields
        let pricing_fields = request.provider_pricing.as_ref().map(|p| p.to_flat_fields()).unwrap_or_default();

        // Extract composite model fields
        let lb_strategy_str = request.lb_strategy.map(|s| s.as_str().to_string());

        let model = sqlx::query_as!(
            DeployedModel,
            r#"
            INSERT INTO deployed_models (
                model_name, alias, description, type, capabilities, created_by, hosted_on, created_at, updated_at,
                requests_per_second, burst_size, capacity, batch_capacity, throughput,
                downstream_pricing_mode, downstream_input_price_per_token, downstream_output_price_per_token,
                downstream_hourly_rate, downstream_input_token_cost_ratio,
                is_composite, lb_strategy, fallback_enabled, fallback_on_rate_limit, fallback_on_status,
                fallback_with_replacement, fallback_max_attempts,
                sanitize_responses, trusted, open_responses_adapter
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28, $29)
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
            request.capacity,
            request.batch_capacity,
            request.throughput,
            pricing_fields.mode,
            pricing_fields.input_price_per_token,
            pricing_fields.output_price_per_token,
            pricing_fields.hourly_rate,
            pricing_fields.input_token_cost_ratio,
            request.is_composite,
            lb_strategy_str,
            request.fallback_enabled,
            request.fallback_on_rate_limit,
            request.fallback_on_status.as_ref().map(|s| s.as_slice()),
            request.fallback_with_replacement,
            request.fallback_max_attempts,
            request.sanitize_responses,
            request.trusted,
            Some(request.open_responses_adapter)
        )
        .fetch_one(&mut *self.db)
        .await?;

        let model_type = model.r#type.as_ref().and_then(|s| match s.as_str() {
            "CHAT" => Some(ModelType::Chat),
            "EMBEDDINGS" => Some(ModelType::Embeddings),
            "RERANKER" => Some(ModelType::Reranker),
            _ => None,
        });

        Ok(DeploymentDBResponse::from((model_type, model)))
    }

    #[instrument(skip(self), fields(deployment_id = %abbrev_uuid(&id)), err)]
    async fn get_by_id(&mut self, id: Self::Id) -> Result<Option<Self::Response>> {
        let model = sqlx::query_as!(
            DeployedModel,
            "SELECT id, model_name, alias, description, type, capabilities, created_by, hosted_on, status, last_sync, deleted, created_at, updated_at, requests_per_second, burst_size, capacity, batch_capacity, throughput, downstream_pricing_mode, downstream_input_price_per_token, downstream_output_price_per_token, downstream_hourly_rate, downstream_input_token_cost_ratio, is_composite, lb_strategy, fallback_enabled, fallback_on_rate_limit, fallback_on_status, fallback_with_replacement, fallback_max_attempts, sanitize_responses, trusted, open_responses_adapter FROM deployed_models WHERE id = $1",
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

    #[instrument(skip(self, ids), fields(count = ids.len()), err)]
    async fn get_bulk(&mut self, ids: Vec<Self::Id>) -> Result<std::collections::HashMap<Self::Id, DeploymentDBResponse>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let deployments = sqlx::query_as!(
            DeployedModel,
            "SELECT id, model_name, alias, description, type, capabilities, created_by, hosted_on, status, last_sync, deleted, created_at, updated_at, requests_per_second, burst_size, capacity, batch_capacity, throughput, downstream_pricing_mode, downstream_input_price_per_token, downstream_output_price_per_token, downstream_hourly_rate, downstream_input_token_cost_ratio, is_composite, lb_strategy, fallback_enabled, fallback_on_rate_limit, fallback_on_status, fallback_with_replacement, fallback_max_attempts, sanitize_responses, trusted, open_responses_adapter FROM deployed_models WHERE id = ANY($1)",
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

    #[instrument(skip(self), fields(deployment_id = %abbrev_uuid(&id)), err)]
    async fn delete(&mut self, id: Self::Id) -> Result<bool> {
        let result = sqlx::query!("DELETE FROM deployed_models WHERE id = $1", id)
            .execute(&mut *self.db)
            .await?;

        Ok(result.rows_affected() > 0)
    }

    #[instrument(skip(self, request), fields(deployment_id = %abbrev_uuid(&id)), err)]
    async fn update(&mut self, id: Self::Id, request: &Self::UpdateRequest) -> Result<Self::Response> {
        if let Some(model_name) = &request.model_name
            && model_name.trim().is_empty()
        {
            return Err(DbError::InvalidModelField { field: "model_name" });
        }
        if let Some(alias) = &request.alias
            && alias.trim().is_empty()
        {
            return Err(DbError::InvalidModelField { field: "alias" });
        }

        // Convert model_type into DB string if provided
        let model_type_str: Option<&str> = request.model_type.as_ref().and_then(|inner| {
            inner.as_ref().map(|t| match t {
                ModelType::Chat => "CHAT",
                ModelType::Embeddings => "EMBEDDINGS",
                ModelType::Reranker => "RERANKER",
            })
        });

        // Convert status into DB string if provided
        let status_str: Option<String> = request.status.as_ref().map(|s| s.to_db_string().to_string());

        // Convert capabilities to slice if provided
        let capabilities_slice: Option<&[String]> = request.capabilities.as_ref().and_then(|inner| inner.as_ref().map(|v| v.as_slice()));

        // Extract provider pricing update information
        let pricing_params = request.provider_pricing.as_ref().map(|p| p.to_update_params()).unwrap_or_default();

        // Extract composite model update fields
        let lb_strategy_str = request.lb_strategy.map(|s| s.as_str().to_string());

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

            -- Three-state update for capacity
            capacity = CASE
                WHEN $18 THEN $19
                ELSE capacity
            END,
            batch_capacity = CASE
                WHEN $20 THEN $21
                ELSE batch_capacity
            END,

            -- Three-state update for throughput
            throughput = CASE
                WHEN $37 THEN $38
                ELSE throughput
            END,

            -- Individual field updates for provider/downstream pricing
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

            -- Composite model fields
            lb_strategy = COALESCE($32, lb_strategy),
            fallback_enabled = COALESCE($33, fallback_enabled),
            fallback_on_rate_limit = COALESCE($34, fallback_on_rate_limit),
            fallback_on_status = COALESCE($35, fallback_on_status),
            sanitize_responses = COALESCE($36, sanitize_responses),
            fallback_with_replacement = COALESCE($39, fallback_with_replacement),
            fallback_max_attempts = CASE
                WHEN $40 THEN $41
                ELSE fallback_max_attempts
            END,
            trusted = COALESCE($42, trusted),
            open_responses_adapter = COALESCE($43, open_responses_adapter),

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
            // For capacity
            request.capacity.is_some() as bool,                               // $18
            request.capacity.as_ref().and_then(|inner| inner.as_ref()),       // $19
            request.batch_capacity.is_some() as bool,                         // $20
            request.batch_capacity.as_ref().and_then(|inner| inner.as_ref()), // $21
            // For individual provider/downstream pricing fields
            pricing_params.should_update_mode,   // $22
            pricing_params.mode.as_deref(),      // $23
            pricing_params.should_update_input,  // $24
            pricing_params.input,                // $25
            pricing_params.should_update_output, // $26
            pricing_params.output,               // $27
            pricing_params.should_update_hourly, // $28
            pricing_params.hourly,               // $29
            pricing_params.should_update_ratio,  // $30
            pricing_params.ratio,                // $31
            // For composite model fields
            lb_strategy_str,                                                         // $32
            request.fallback_enabled,                                                // $33
            request.fallback_on_rate_limit,                                          // $34
            request.fallback_on_status.as_ref().map(|s| s.as_slice()),               // $35
            request.sanitize_responses,                                              // $36
            request.throughput.is_some() as bool,                                    // $37
            request.throughput.as_ref().and_then(|inner| inner.as_ref()),            // $38
            request.fallback_with_replacement,                                       // $39
            request.fallback_max_attempts.is_some() as bool,                         // $40
            request.fallback_max_attempts.as_ref().and_then(|inner| inner.as_ref()), // $41
            request.trusted,                                                         // $42
            request.open_responses_adapter,                                          // $43
        )
        .fetch_one(&mut *self.db)
        .await?;

        // Convert DB model_type back to enum
        let model_type = model.r#type.as_deref().and_then(|s| match s {
            "CHAT" => Some(ModelType::Chat),
            "EMBEDDINGS" => Some(ModelType::Embeddings),
            "RERANKER" => Some(ModelType::Reranker),
            _ => None,
        });

        Ok(DeploymentDBResponse::from((model_type, model)))
    }

    #[instrument(skip(self, filter), fields(limit = filter.limit, skip = filter.skip), err)]
    async fn list(&mut self, filter: &Self::Filter) -> Result<Vec<Self::Response>> {
        // Use LEFT JOIN with inference_endpoints to enable searching by endpoint name
        let mut query =
            QueryBuilder::new("SELECT dm.* FROM deployed_models dm LEFT JOIN inference_endpoints ie ON dm.hosted_on = ie.id WHERE 1=1");

        // Add endpoint filter if specified
        if let Some(endpoint_id) = filter.endpoint_id {
            query.push(" AND dm.hosted_on = ");
            query.push_bind(endpoint_id);
        }

        // Add status filter if specified
        if let Some(ref statuses) = filter.statuses {
            let status_strings: Vec<String> = statuses.iter().map(|s| s.to_db_string().to_string()).collect();
            query.push(" AND dm.status = ANY(");
            query.push_bind(status_strings);
            query.push(")");
        }

        // Add deleted filter if specified
        if let Some(deleted) = filter.deleted {
            query.push(" AND dm.deleted = ");
            query.push_bind(deleted);
        }

        // Add aliases filter if specified
        if let Some(ref aliases) = filter.aliases
            && !aliases.is_empty()
        {
            query.push(" AND dm.alias = ANY(");
            query.push_bind(aliases);
            query.push(")");
        }

        // Add accessibility filter if specified
        if let Some(user_id) = filter.accessible_to {
            query.push(" AND dm.id IN (");
            query.push("SELECT dg.deployment_id FROM deployment_groups dg WHERE dg.group_id IN (");
            query.push("SELECT ug.group_id FROM user_groups ug WHERE ug.user_id = ");
            query.push_bind(user_id);
            query.push(" UNION SELECT '00000000-0000-0000-0000-000000000000'::uuid WHERE ");
            query.push_bind(user_id);
            query.push(" != '00000000-0000-0000-0000-000000000000'::uuid");
            query.push("))");
        }

        // Add group filter if specified
        if let Some(ref group_ids) = filter.group_ids
            && !group_ids.is_empty()
        {
            query.push(" AND dm.id IN (");
            query.push("SELECT dg.deployment_id FROM deployment_groups dg WHERE dg.group_id = ANY(");
            query.push_bind(group_ids);
            query.push("))");
        }

        // Add search filter if specified (case-insensitive substring match on alias, model_name, or endpoint name)
        if let Some(ref search) = filter.search {
            let search_pattern = format!("%{}%", search.to_lowercase());
            query.push(" AND (LOWER(dm.alias) LIKE ");
            query.push_bind(search_pattern.clone());
            query.push(" OR LOWER(dm.model_name) LIKE ");
            query.push_bind(search_pattern.clone());
            query.push(" OR LOWER(ie.name) LIKE ");
            query.push_bind(search_pattern);
            query.push(")");
        }

        // Add is_composite filter if specified
        if let Some(is_composite) = filter.is_composite {
            query.push(" AND dm.is_composite = ");
            query.push_bind(is_composite);
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
    #[instrument(skip(self), fields(deployment_alias = %deployment_alias, user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn check_user_access(&mut self, deployment_alias: &str, user_id: UserId) -> Result<Option<DeploymentAccessInfo>> {
        let result = sqlx::query_as!(
            DeploymentAccessInfo,
            r#"
            SELECT
                d.id as deployment_id,
                d.alias as deployment_alias,
                ak.secret as system_api_key
            FROM deployed_models d
            JOIN deployment_groups dg ON dg.deployment_id = d.id
            JOIN api_keys ak ON ak.id = '00000000-0000-0000-0000-000000000000'::uuid
            WHERE d.alias = $1
            AND dg.group_id IN (
                SELECT ug.group_id FROM user_groups ug WHERE ug.user_id = $2
                UNION
                SELECT '00000000-0000-0000-0000-000000000000'::uuid
                WHERE $2 != '00000000-0000-0000-0000-000000000000'::uuid
            )
            LIMIT 1
            "#,
            deployment_alias,
            user_id
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(result)
    }

    /// Count deployments matching the given filter (without pagination)
    #[instrument(skip(self, filter), err)]
    pub async fn count(&mut self, filter: &DeploymentFilter) -> Result<i64> {
        // Use LEFT JOIN with inference_endpoints to enable searching by endpoint name
        let mut query =
            QueryBuilder::new("SELECT COUNT(*) FROM deployed_models dm LEFT JOIN inference_endpoints ie ON dm.hosted_on = ie.id WHERE 1=1");

        // Add endpoint filter if specified
        if let Some(endpoint_id) = filter.endpoint_id {
            query.push(" AND dm.hosted_on = ");
            query.push_bind(endpoint_id);
        }

        // Add status filter if specified
        if let Some(ref statuses) = filter.statuses {
            let status_strings: Vec<String> = statuses.iter().map(|s| s.to_db_string().to_string()).collect();
            query.push(" AND dm.status = ANY(");
            query.push_bind(status_strings);
            query.push(")");
        }

        // Add deleted filter if specified
        if let Some(deleted) = filter.deleted {
            query.push(" AND dm.deleted = ");
            query.push_bind(deleted);
        }

        // Add aliases filter if specified
        if let Some(ref aliases) = filter.aliases
            && !aliases.is_empty()
        {
            query.push(" AND dm.alias = ANY(");
            query.push_bind(aliases);
            query.push(")");
        }

        // Add accessibility filter if specified
        if let Some(user_id) = filter.accessible_to {
            query.push(" AND dm.id IN (");
            query.push("SELECT dg.deployment_id FROM deployment_groups dg WHERE dg.group_id IN (");
            query.push("SELECT ug.group_id FROM user_groups ug WHERE ug.user_id = ");
            query.push_bind(user_id);
            query.push(" UNION SELECT '00000000-0000-0000-0000-000000000000'::uuid WHERE ");
            query.push_bind(user_id);
            query.push(" != '00000000-0000-0000-0000-000000000000'::uuid");
            query.push("))");
        }

        // Add group filter if specified
        if let Some(ref group_ids) = filter.group_ids
            && !group_ids.is_empty()
        {
            query.push(" AND dm.id IN (");
            query.push("SELECT dg.deployment_id FROM deployment_groups dg WHERE dg.group_id = ANY(");
            query.push_bind(group_ids);
            query.push("))");
        }

        // Add search filter if specified (case-insensitive substring match on alias, model_name, or endpoint name)
        if let Some(ref search) = filter.search {
            let search_pattern = format!("%{}%", search.to_lowercase());
            query.push(" AND (LOWER(dm.alias) LIKE ");
            query.push_bind(search_pattern.clone());
            query.push(" OR LOWER(dm.model_name) LIKE ");
            query.push_bind(search_pattern.clone());
            query.push(" OR LOWER(ie.name) LIKE ");
            query.push_bind(search_pattern);
            query.push(")");
        }

        // Add is_composite filter if specified
        if let Some(is_composite) = filter.is_composite {
            query.push(" AND dm.is_composite = ");
            query.push_bind(is_composite);
        }

        let count: (i64,) = query.build_query_as().fetch_one(&mut *self.db).await?;
        Ok(count.0)
    }

    // ===== Composite Model Component Management =====

    /// Add a component to a composite model
    #[instrument(skip(self), fields(composite_id = %abbrev_uuid(&request.composite_model_id), deployed_id = %abbrev_uuid(&request.deployed_model_id)), err)]
    pub async fn add_component(&mut self, request: &DeploymentComponentCreateDBRequest) -> Result<DeploymentComponentDBResponse> {
        let result = sqlx::query!(
            r#"
            WITH inserted AS (
                INSERT INTO deployed_model_components (composite_model_id, deployed_model_id, weight, enabled, sort_order)
                VALUES ($1, $2, $3, $4, $5)
                RETURNING id, composite_model_id, deployed_model_id, weight, enabled, sort_order, created_at
            )
            SELECT
                inserted.id,
                inserted.composite_model_id,
                inserted.deployed_model_id,
                inserted.weight,
                inserted.enabled,
                inserted.sort_order,
                inserted.created_at,
                dm.alias as model_alias,
                dm.model_name,
                dm.description as model_description,
                dm.type as model_type,
                dm.trusted as model_trusted,
                dm.open_responses_adapter as "model_open_responses_adapter?",
                dm.hosted_on as endpoint_id,
                e.name as "endpoint_name?"
            FROM inserted
            JOIN deployed_models dm ON dm.id = inserted.deployed_model_id
            LEFT JOIN inference_endpoints e ON e.id = dm.hosted_on
            "#,
            request.composite_model_id,
            request.deployed_model_id,
            request.weight,
            request.enabled,
            request.sort_order
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(DeploymentComponentDBResponse {
            id: result.id,
            composite_model_id: result.composite_model_id,
            deployed_model_id: result.deployed_model_id,
            weight: result.weight,
            enabled: result.enabled,
            sort_order: result.sort_order,
            created_at: result.created_at,
            model_alias: result.model_alias,
            model_name: result.model_name,
            model_description: result.model_description,
            model_type: result.model_type,
            endpoint_id: result.endpoint_id,
            endpoint_name: result.endpoint_name,
            model_trusted: result.model_trusted,
            model_open_responses_adapter: result.model_open_responses_adapter.unwrap_or(true),
        })
    }

    /// Remove a component from a composite model
    #[instrument(skip(self), fields(composite_id = %abbrev_uuid(&composite_model_id), deployed_id = %abbrev_uuid(&deployed_model_id)), err)]
    pub async fn remove_component(&mut self, composite_model_id: DeploymentId, deployed_model_id: DeploymentId) -> Result<bool> {
        let result = sqlx::query!(
            "DELETE FROM deployed_model_components WHERE composite_model_id = $1 AND deployed_model_id = $2",
            composite_model_id,
            deployed_model_id
        )
        .execute(&mut *self.db)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Get all components of a composite model
    #[instrument(skip(self), fields(composite_id = %abbrev_uuid(&composite_model_id)), err)]
    pub async fn get_components(&mut self, composite_model_id: DeploymentId) -> Result<Vec<DeploymentComponentDBResponse>> {
        let results = sqlx::query!(
            r#"
            SELECT
                dmc.id,
                dmc.composite_model_id,
                dmc.deployed_model_id,
                dmc.weight,
                dmc.enabled,
                dmc.sort_order,
                dmc.created_at,
                dm.alias as model_alias,
                dm.model_name,
                dm.description as model_description,
                dm.type as model_type,
                dm.trusted as model_trusted,
                dm.open_responses_adapter as "model_open_responses_adapter?",
                dm.hosted_on as endpoint_id,
                e.name as "endpoint_name?"
            FROM deployed_model_components dmc
            JOIN deployed_models dm ON dm.id = dmc.deployed_model_id
            LEFT JOIN inference_endpoints e ON e.id = dm.hosted_on
            WHERE dmc.composite_model_id = $1
            ORDER BY dmc.sort_order ASC, dmc.weight DESC, dmc.created_at ASC
            "#,
            composite_model_id
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(results
            .into_iter()
            .map(|r| DeploymentComponentDBResponse {
                id: r.id,
                composite_model_id: r.composite_model_id,
                deployed_model_id: r.deployed_model_id,
                weight: r.weight,
                enabled: r.enabled,
                sort_order: r.sort_order,
                created_at: r.created_at,
                model_alias: r.model_alias,
                model_name: r.model_name,
                model_description: r.model_description,
                model_type: r.model_type,
                endpoint_id: r.endpoint_id,
                endpoint_name: r.endpoint_name,
                model_trusted: r.model_trusted,
                model_open_responses_adapter: r.model_open_responses_adapter.unwrap_or(true),
            })
            .collect())
    }

    /// Get components for multiple composite models in bulk
    #[instrument(skip(self, composite_model_ids), fields(count = composite_model_ids.len()), err)]
    pub async fn get_components_bulk(
        &mut self,
        composite_model_ids: Vec<DeploymentId>,
    ) -> Result<std::collections::HashMap<DeploymentId, Vec<DeploymentComponentDBResponse>>> {
        if composite_model_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let results = sqlx::query!(
            r#"
            SELECT
                dmc.id,
                dmc.composite_model_id,
                dmc.deployed_model_id,
                dmc.weight,
                dmc.enabled,
                dmc.sort_order,
                dmc.created_at,
                dm.alias as model_alias,
                dm.model_name,
                dm.description as model_description,
                dm.type as model_type,
                dm.trusted as model_trusted,
                dm.open_responses_adapter as "model_open_responses_adapter?",
                dm.hosted_on as endpoint_id,
                e.name as "endpoint_name?"
            FROM deployed_model_components dmc
            JOIN deployed_models dm ON dm.id = dmc.deployed_model_id
            LEFT JOIN inference_endpoints e ON e.id = dm.hosted_on
            WHERE dmc.composite_model_id = ANY($1)
            ORDER BY dmc.composite_model_id, dmc.sort_order ASC, dmc.weight DESC, dmc.created_at ASC
            "#,
            &composite_model_ids
        )
        .fetch_all(&mut *self.db)
        .await?;

        let mut map: std::collections::HashMap<DeploymentId, Vec<DeploymentComponentDBResponse>> = std::collections::HashMap::new();

        for r in results {
            map.entry(r.composite_model_id).or_default().push(DeploymentComponentDBResponse {
                id: r.id,
                composite_model_id: r.composite_model_id,
                deployed_model_id: r.deployed_model_id,
                weight: r.weight,
                enabled: r.enabled,
                sort_order: r.sort_order,
                created_at: r.created_at,
                model_alias: r.model_alias,
                model_name: r.model_name,
                model_description: r.model_description,
                model_type: r.model_type,
                endpoint_id: r.endpoint_id,
                endpoint_name: r.endpoint_name,
                model_trusted: r.model_trusted,
                model_open_responses_adapter: r.model_open_responses_adapter.unwrap_or(true),
            });
        }

        Ok(map)
    }

    /// Set all components of a composite model (replace existing)
    /// Tuple is (deployed_model_id, weight, enabled, sort_order)
    #[instrument(skip(self, components), fields(composite_id = %abbrev_uuid(&composite_model_id), count = components.len()), err)]
    pub async fn set_components(
        &mut self,
        composite_model_id: DeploymentId,
        components: Vec<(DeploymentId, i32, bool, i32)>,
    ) -> Result<Vec<DeploymentComponentDBResponse>> {
        // Delete existing components
        sqlx::query!(
            "DELETE FROM deployed_model_components WHERE composite_model_id = $1",
            composite_model_id
        )
        .execute(&mut *self.db)
        .await?;

        // Insert new components
        let mut results = Vec::new();
        for (deployed_model_id, weight, enabled, sort_order) in components {
            let request = DeploymentComponentCreateDBRequest {
                composite_model_id,
                deployed_model_id,
                weight,
                enabled,
                sort_order,
            };
            results.push(self.add_component(&request).await?);
        }

        Ok(results)
    }

    /// Update a component's weight, enabled status, and sort_order
    #[instrument(skip(self), fields(composite_id = %abbrev_uuid(&composite_model_id), deployed_id = %abbrev_uuid(&deployed_model_id)), err)]
    pub async fn update_component(
        &mut self,
        composite_model_id: DeploymentId,
        deployed_model_id: DeploymentId,
        weight: Option<i32>,
        enabled: Option<bool>,
        sort_order: Option<i32>,
    ) -> Result<Option<DeploymentComponentDBResponse>> {
        let result = sqlx::query!(
            r#"
            WITH updated AS (
                UPDATE deployed_model_components
                SET weight = COALESCE($3, weight),
                    enabled = COALESCE($4, enabled),
                    sort_order = COALESCE($5, sort_order)
                WHERE composite_model_id = $1 AND deployed_model_id = $2
                RETURNING id, composite_model_id, deployed_model_id, weight, enabled, sort_order, created_at
            )
            SELECT
                updated.id,
                updated.composite_model_id,
                updated.deployed_model_id,
                updated.weight,
                updated.enabled,
                updated.sort_order,
                updated.created_at,
                dm.alias as model_alias,
                dm.model_name,
                dm.description as model_description,
                dm.type as model_type,
                dm.trusted as model_trusted,
                dm.open_responses_adapter as "model_open_responses_adapter?",
                dm.hosted_on as endpoint_id,
                e.name as "endpoint_name?"
            FROM updated
            JOIN deployed_models dm ON dm.id = updated.deployed_model_id
            LEFT JOIN inference_endpoints e ON e.id = dm.hosted_on
            "#,
            composite_model_id,
            deployed_model_id,
            weight,
            enabled,
            sort_order
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(result.map(|r| DeploymentComponentDBResponse {
            id: r.id,
            composite_model_id: r.composite_model_id,
            deployed_model_id: r.deployed_model_id,
            weight: r.weight,
            enabled: r.enabled,
            sort_order: r.sort_order,
            created_at: r.created_at,
            model_alias: r.model_alias,
            model_name: r.model_name,
            model_description: r.model_description,
            model_type: r.model_type,
            endpoint_id: r.endpoint_id,
            endpoint_name: r.endpoint_name,
            model_trusted: r.model_trusted,
            model_open_responses_adapter: r.model_open_responses_adapter.unwrap_or(true),
        }))
    }

    /// Get throughput values for the given model aliases
    /// Returns a map of alias -> throughput for models that have throughput configured
    #[instrument(skip(self, aliases), fields(count = aliases.len()), err)]
    pub async fn get_throughputs_by_aliases(&mut self, aliases: &[String]) -> Result<std::collections::HashMap<String, f32>> {
        if aliases.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let rows = sqlx::query!(
            r#"
            SELECT alias, throughput
            FROM deployed_models
            WHERE alias = ANY($1)
              AND deleted = false
              AND throughput IS NOT NULL
            "#,
            aliases
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().filter_map(|r| r.throughput.map(|t| (r.alias, t))).collect())
    }

    /// Get model UUIDs keyed by alias for the given aliases.
    /// Aliases are enforced to be unique, so this should be a one to one mapping
    /// Only returns rows where `deleted = false`.
    #[instrument(skip(self, aliases), fields(count = aliases.len()), err)]
    pub async fn get_model_ids_by_aliases(&mut self, aliases: &[String]) -> Result<std::collections::HashMap<String, uuid::Uuid>> {
        if aliases.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let rows = sqlx::query!(
            r#"
            SELECT id, alias
            FROM deployed_models
            WHERE alias = ANY($1)
              AND deleted = false
            "#,
            aliases
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().map(|r| (r.alias, r.id)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::models::users::{Role, UserCreate, UserResponse},
        db::{
            handlers::{Groups, Users, inference_endpoints::InferenceEndpoints},
            models::{
                deployments::{ProviderPricing, ProviderPricingUpdate},
                groups::GroupCreateDBRequest,
                inference_endpoints::InferenceEndpointCreateDBRequest,
                users::UserCreateDBRequest,
            },
        },
        test::utils::get_test_endpoint_id,
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
            default_models: None,
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
                    .capacity(100)
                    .batch_capacity(50)
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
        assert_eq!(model.capacity, Some(100));
        assert_eq!(model.batch_capacity, Some(50));
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
            default_models: None,
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
                model_create.hosted_on = Some(test_endpoint_id);

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
            default_models: None,
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
                    .capacity(Some(200))
                    .batch_capacity(Some(75))
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
        assert_eq!(updated_model.capacity, Some(200));
        assert_eq!(updated_model.batch_capacity, Some(75));
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
            default_models: None,
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
                model_create.hosted_on = Some(test_endpoint_id);

                model = repo.create(&model_create).await.unwrap();
            }
            tx.commit().await.unwrap();
        }
        assert_eq!(model.model_name, "null-fields-model");
        assert_eq!(model.alias, "null-fields-deployment");
        assert_eq!(model.created_by, user.id);
        assert_eq!(model.model_type, None);
        assert_eq!(model.capabilities, None);
        assert_eq!(model.capacity, None);
        assert_eq!(model.batch_capacity, None);
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
            default_models: None,
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
                // Create a model with type, capabilities, and capacity
                let mut model_create = DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("to-null-model".to_string())
                    .alias("to-null-deployment".to_string())
                    .build();
                model_create.hosted_on = Some(test_endpoint_id);
                model_create.model_type = Some(ModelType::Chat);
                model_create.capabilities = Some(vec!["test-capability".to_string()]);
                model_create.capacity = Some(150);
                model_create.batch_capacity = Some(60);

                created_model = repo.create(&model_create).await.unwrap();

                // Update to null values
                let update = DeploymentUpdateDBRequest::builder()
                    .maybe_model_type(Some(None))
                    .maybe_capabilities(Some(None))
                    .maybe_capacity(Some(None))
                    .maybe_batch_capacity(Some(None))
                    .build();

                updated_model = repo.update(created_model.id, &update).await.unwrap();
            }
            tx.commit().await.unwrap();
        }
        assert_eq!(created_model.model_type, Some(ModelType::Chat));
        assert_eq!(created_model.capabilities, Some(vec!["test-capability".to_string()]));
        assert_eq!(created_model.capacity, Some(150));
        assert_eq!(created_model.batch_capacity, Some(60));
        assert_eq!(updated_model.model_type, None);
        assert_eq!(updated_model.capabilities, None);
        assert_eq!(updated_model.capacity, None);
        assert_eq!(updated_model.batch_capacity, None);
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
            default_models: None,
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
                model_create.hosted_on = Some(test_endpoint_id);

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
            default_models: None,
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
        model1.hosted_on = Some(test_endpoint_id);

        let mut model2 = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("list-test-model-2".to_string())
            .alias("list-test-deployment-2".to_string())
            .build();
        model2.hosted_on = Some(test_endpoint_id);

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
            default_models: None,
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
        model_create.hosted_on = Some(endpoint_id);
        let deployment = repo.create(&model_create).await.unwrap();

        // Test filtering by endpoint
        let filter = DeploymentFilter::new(0, 10).with_endpoint(endpoint_id);
        let models = repo.list(&filter).await.unwrap();

        assert!(models.iter().any(|m| m.id == deployment.id));
        assert!(models.iter().all(|m| m.hosted_on == Some(endpoint_id)));
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
            default_models: None,
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
        model_create.hosted_on = Some(test_endpoint_id);
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
            default_models: None,
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
        model_create.hosted_on = Some(test_endpoint_id);
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
            default_models: None,
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
        model1_create.hosted_on = Some(test_endpoint_id);
        let mut model2_create = DeploymentCreateDBRequest::builder()
            .created_by(user1.id)
            .model_name("accessible-model-2".to_string())
            .alias("accessible-deployment-2".to_string())
            .build();
        model2_create.hosted_on = Some(test_endpoint_id);
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
            default_models: None,
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
        model_create.hosted_on = Some(endpoint_id);
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
        assert!(models.iter().all(|m| m.hosted_on == Some(endpoint_id)));
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
            default_models: None,
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
            model_create.hosted_on = Some(test_endpoint_id);
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
            default_models: None,
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
        model_create.hosted_on = Some(test_endpoint_id);
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
            default_models: None,
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
        model_create.hosted_on = Some(test_endpoint_id);
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
            default_models: None,
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
        chat_create.hosted_on = Some(test_endpoint_id);
        chat_create.model_type = Some(ModelType::Chat);
        let chat_deployment = repo.create(&chat_create).await.unwrap();

        // Create embeddings deployment
        let mut embeddings_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("bulk-embeddings-model".to_string())
            .alias("bulk-embeddings-deployment".to_string())
            .build();
        embeddings_create.hosted_on = Some(test_endpoint_id);
        embeddings_create.model_type = Some(ModelType::Embeddings);
        let embeddings_deployment = repo.create(&embeddings_create).await.unwrap();

        // Create deployment with no type
        let mut no_type_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("bulk-no-type-model".to_string())
            .alias("bulk-no-type-deployment".to_string())
            .build();
        no_type_create.hosted_on = Some(test_endpoint_id);
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
            default_models: None,
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
        chat_create.hosted_on = Some(test_endpoint_id);
        chat_create.model_type = Some(ModelType::Chat);
        let chat_deployment = repo.create(&chat_create).await.unwrap();

        // Create embeddings deployment
        let mut embeddings_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("list-embeddings-model".to_string())
            .alias("list-embeddings-deployment".to_string())
            .build();
        embeddings_create.hosted_on = Some(test_endpoint_id);
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
            default_models: None,
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
        model_create.hosted_on = Some(test_endpoint_id);
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
            default_models: None,
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
        model_create.hosted_on = Some(test_endpoint_id);
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
            default_models: None,
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
            default_models: None,
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
        deployment_create.hosted_on = Some(test_endpoint_id);
        let deployment = deployment_repo.create(&deployment_create).await.unwrap();

        // Create a group
        let group_create = GroupCreateDBRequest {
            name: "Access Test Group".to_string(),
            description: Some("Test group for access control".to_string()),
            created_by: user.id,
        };
        let group = group_repo.create(&group_create).await.unwrap();

        // Test user access without group membership - should return None
        let access_result = deployment_repo.check_user_access("access-test-alias", user.id).await.unwrap();
        assert!(access_result.is_none());

        // Add user to group
        group_repo
            .add_user_to_group(user.id, group.id)
            .await
            .expect("Failed to add user to group");

        // Test user access without deployment in group - should still return None
        let access_result = deployment_repo.check_user_access("access-test-alias", user.id).await.unwrap();
        assert!(access_result.is_none());

        // Add deployment to group
        group_repo
            .add_deployment_to_group(deployment.id, group.id, user.id)
            .await
            .expect("Failed to add deployment to group");

        // Test user access with proper group membership - should return access info
        let access_result = deployment_repo.check_user_access("access-test-alias", user.id).await.unwrap();
        assert!(access_result.is_some());

        let access_info = access_result.unwrap();
        assert_eq!(access_info.deployment_id, deployment.id);
        assert_eq!(access_info.deployment_alias, "access-test-alias");
        assert_eq!(access_info.system_api_key, system_key_secret);

        // Test with non-existent user - should return None
        let nonexistent_user_id = uuid::Uuid::new_v4();
        let access_result = deployment_repo
            .check_user_access("access-test-alias", nonexistent_user_id)
            .await
            .unwrap();
        assert!(access_result.is_none());

        // Test with non-existent deployment - should return None
        let access_result = deployment_repo.check_user_access("nonexistent-alias", user.id).await.unwrap();
        assert!(access_result.is_none());

        // Remove user from group and test access again - should return None
        group_repo
            .remove_user_from_group(user.id, group.id)
            .await
            .expect("Failed to remove user from group");

        let access_result = deployment_repo.check_user_access("access-test-alias", user.id).await.unwrap();
        assert!(access_result.is_none());
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
            default_models: None,
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let user = create_test_user(&pool).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let created_model;
        {
            let mut tx = pool.begin().await.unwrap();
            {
                let mut repo = Deployments::new(tx.acquire().await.unwrap());

                // Create model with initial provider per-token pricing
                let initial_provider_pricing = ProviderPricing::PerToken {
                    input_price_per_token: Some(Decimal::from_str("0.005").unwrap()),
                    output_price_per_token: Some(Decimal::from_str("0.01").unwrap()),
                };

                let model_create = DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("provider-per-token-test".to_string())
                    .alias("provider-per-token-alias".to_string())
                    .hosted_on(test_endpoint_id)
                    .provider_pricing(initial_provider_pricing)
                    .build();

                created_model = repo.create(&model_create).await.unwrap();
            }
            tx.commit().await.unwrap();
        }

        // Test 1: Update only provider input pricing
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut repo = Deployments::new(&mut conn);

            let pricing_update = ProviderPricingUpdate::PerToken {
                input_price_per_token: Some(Some(Decimal::from_str("0.003").unwrap())),
                output_price_per_token: None, // No change
            };

            let update = DeploymentUpdateDBRequest::builder().provider_pricing(pricing_update).build();

            let updated_model = repo.update(created_model.id, &update).await.unwrap();

            // Verify partial provider update
            if let Some(ProviderPricing::PerToken {
                input_price_per_token,
                output_price_per_token,
            }) = &updated_model.provider_pricing
            {
                assert_eq!(input_price_per_token, &Some(Decimal::from_str("0.003").unwrap()));
                assert_eq!(output_price_per_token, &Some(Decimal::from_str("0.01").unwrap())); // Unchanged
            }
        }

        // Test 2: Update only provider output pricing
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut repo = Deployments::new(&mut conn);

            let pricing_update = ProviderPricingUpdate::PerToken {
                input_price_per_token: None, // No change
                output_price_per_token: Some(Some(Decimal::from_str("0.008").unwrap())),
            };

            let update = DeploymentUpdateDBRequest::builder().provider_pricing(pricing_update).build();

            let updated_model = repo.update(created_model.id, &update).await.unwrap();

            // Verify partial provider update
            if let Some(ProviderPricing::PerToken {
                input_price_per_token,
                output_price_per_token,
            }) = &updated_model.provider_pricing
            {
                assert_eq!(input_price_per_token, &Some(Decimal::from_str("0.003").unwrap())); // From previous update
                assert_eq!(output_price_per_token, &Some(Decimal::from_str("0.008").unwrap()));
            }
        }

        // Test 3: Clear provider input pricing
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut repo = Deployments::new(&mut conn);

            let pricing_update = ProviderPricingUpdate::PerToken {
                input_price_per_token: Some(None), // Clear this field
                output_price_per_token: None,      // No change
            };

            let update = DeploymentUpdateDBRequest::builder().provider_pricing(pricing_update).build();

            let updated_model = repo.update(created_model.id, &update).await.unwrap();

            // Verify clearing worked
            if let Some(ProviderPricing::PerToken {
                input_price_per_token,
                output_price_per_token,
            }) = &updated_model.provider_pricing
            {
                assert_eq!(input_price_per_token, &None); // Cleared
                assert_eq!(output_price_per_token, &Some(Decimal::from_str("0.008").unwrap())); // Unchanged
            }
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_provider_hourly_pricing_updates(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
            default_models: None,
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let user = create_test_user(&pool).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let created_model;
        {
            let mut tx = pool.begin().await.unwrap();
            {
                let mut repo = Deployments::new(tx.acquire().await.unwrap());

                // Create model with initial provider hourly pricing
                let initial_provider_pricing = ProviderPricing::Hourly {
                    rate: Decimal::from_str("5.00").unwrap(),
                    input_token_cost_ratio: Decimal::from_str("0.8").unwrap(),
                };

                let model_create = DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("provider-hourly-test".to_string())
                    .alias("provider-hourly-alias".to_string())
                    .hosted_on(test_endpoint_id)
                    .provider_pricing(initial_provider_pricing)
                    .build();

                created_model = repo.create(&model_create).await.unwrap();
            }
            tx.commit().await.unwrap();
        }

        // Verify initial provider hourly pricing
        assert!(created_model.provider_pricing.is_some());
        if let Some(ProviderPricing::Hourly {
            rate,
            input_token_cost_ratio,
        }) = &created_model.provider_pricing
        {
            assert_eq!(rate, &Decimal::from_str("5.00").unwrap());
            assert_eq!(input_token_cost_ratio, &Decimal::from_str("0.8").unwrap());
        }

        // Test 1: Update hourly rate only
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut repo = Deployments::new(&mut conn);

            let pricing_update = ProviderPricingUpdate::Hourly {
                rate: Some(Decimal::from_str("6.50").unwrap()),
                input_token_cost_ratio: None, // Keep existing value
            };

            let update = DeploymentUpdateDBRequest::builder().provider_pricing(pricing_update).build();

            let updated_model = repo.update(created_model.id, &update).await.unwrap();

            // Verify hourly rate update
            if let Some(ProviderPricing::Hourly {
                rate,
                input_token_cost_ratio,
            }) = &updated_model.provider_pricing
            {
                assert_eq!(rate, &Decimal::from_str("6.50").unwrap()); // Updated
                assert_eq!(input_token_cost_ratio, &Decimal::from_str("0.8").unwrap()); // Unchanged
            }
        }

        // Test 2: Update input token cost ratio only
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut repo = Deployments::new(&mut conn);

            let pricing_update = ProviderPricingUpdate::Hourly {
                rate: None, // Keep existing value
                input_token_cost_ratio: Some(Decimal::from_str("0.9").unwrap()),
            };

            let update = DeploymentUpdateDBRequest::builder().provider_pricing(pricing_update).build();

            let updated_model = repo.update(created_model.id, &update).await.unwrap();

            // Verify input token cost ratio update
            if let Some(ProviderPricing::Hourly {
                rate,
                input_token_cost_ratio,
            }) = &updated_model.provider_pricing
            {
                assert_eq!(rate, &Decimal::from_str("6.50").unwrap()); // From previous update
                assert_eq!(input_token_cost_ratio, &Decimal::from_str("0.9").unwrap()); // Updated
            }
        }

        // Test 3: Update both hourly fields
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut repo = Deployments::new(&mut conn);

            let pricing_update = ProviderPricingUpdate::Hourly {
                rate: Some(Decimal::from_str("7.00").unwrap()),
                input_token_cost_ratio: Some(Decimal::from_str("0.75").unwrap()),
            };

            let update = DeploymentUpdateDBRequest::builder().provider_pricing(pricing_update).build();

            let updated_model = repo.update(created_model.id, &update).await.unwrap();

            // Verify both fields updated
            if let Some(ProviderPricing::Hourly {
                rate,
                input_token_cost_ratio,
            }) = &updated_model.provider_pricing
            {
                assert_eq!(rate, &Decimal::from_str("7.00").unwrap());
                assert_eq!(input_token_cost_ratio, &Decimal::from_str("0.75").unwrap());
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
            default_models: None,
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
        model_create1.hosted_on = Some(test_endpoint_id);

        let mut model_create2 = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("inactive-test-model".to_string())
            .alias("inactive-test-deployment".to_string())
            .build();
        model_create2.hosted_on = Some(test_endpoint_id);

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
            default_models: None,
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
        model_create.hosted_on = Some(test_endpoint_id);
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
            auth_header_name: None,
            auth_header_prefix: None,
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
            auth_header_name: None,
            auth_header_prefix: None,
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
            default_models: None,
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let user = create_test_user(&pool).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut conn);

        // Empty model name
        let model_create = DeploymentCreateDBRequest::builder()
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
        let model_create = DeploymentCreateDBRequest::builder()
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
            default_models: None,
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let user = create_test_user(&pool).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut conn);

        // Create a valid deployment first
        let model_create = DeploymentCreateDBRequest::builder()
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

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_with_group_filter(pool: PgPool) {
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
            default_models: None,
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        let mut pool_conn = pool.acquire().await.unwrap();
        let mut repo = Deployments::new(&mut pool_conn);
        let mut group_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut group_conn);
        let user = create_test_user(&pool).await;
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create three deployments
        let mut model1_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("group-model-1".to_string())
            .alias("group-deployment-1".to_string())
            .build();
        model1_create.hosted_on = Some(test_endpoint_id);
        let deployment1 = repo.create(&model1_create).await.unwrap();

        let mut model2_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("group-model-2".to_string())
            .alias("group-deployment-2".to_string())
            .build();
        model2_create.hosted_on = Some(test_endpoint_id);
        let deployment2 = repo.create(&model2_create).await.unwrap();

        let mut model3_create = DeploymentCreateDBRequest::builder()
            .created_by(user.id)
            .model_name("group-model-3".to_string())
            .alias("group-deployment-3".to_string())
            .build();
        model3_create.hosted_on = Some(test_endpoint_id);
        let deployment3 = repo.create(&model3_create).await.unwrap();

        // Create two groups
        let group1_create = GroupCreateDBRequest {
            name: "Production".to_string(),
            description: Some("Production group".to_string()),
            created_by: user.id,
        };
        let group1 = group_repo.create(&group1_create).await.unwrap();

        let group2_create = GroupCreateDBRequest {
            name: "Staging".to_string(),
            description: Some("Staging group".to_string()),
            created_by: user.id,
        };
        let group2 = group_repo.create(&group2_create).await.unwrap();

        // Add deployment1 to group1 (production)
        group_repo
            .add_deployment_to_group(deployment1.id, group1.id, user.id)
            .await
            .unwrap();

        // Add deployment2 to group2 (staging)
        group_repo
            .add_deployment_to_group(deployment2.id, group2.id, user.id)
            .await
            .unwrap();

        // deployment3 has no groups

        // Test 1: Filter by single group (production)
        let filter = DeploymentFilter::new(0, 10).with_groups(vec![group1.id]);
        let models = repo.list(&filter).await.unwrap();
        assert_eq!(models.len(), 1);
        assert!(models.iter().any(|m| m.id == deployment1.id));
        assert!(!models.iter().any(|m| m.id == deployment2.id));
        assert!(!models.iter().any(|m| m.id == deployment3.id));

        // Test 2: Filter by multiple groups (production + staging)
        let filter = DeploymentFilter::new(0, 10).with_groups(vec![group1.id, group2.id]);
        let models = repo.list(&filter).await.unwrap();
        assert_eq!(models.len(), 2);
        assert!(models.iter().any(|m| m.id == deployment1.id));
        assert!(models.iter().any(|m| m.id == deployment2.id));
        assert!(!models.iter().any(|m| m.id == deployment3.id));

        // Test 3: Filter by empty group list (should show all models)
        let filter = DeploymentFilter::new(0, 10).with_groups(vec![]);
        let models = repo.list(&filter).await.unwrap();
        // Empty groups list is treated as no filter, so all models are returned
        assert!(models.len() >= 3);

        // Test 4: Count should also respect group filter
        let filter = DeploymentFilter::new(0, 10).with_groups(vec![group1.id]);
        let count = repo.count(&filter).await.unwrap();
        assert_eq!(count, 1);

        let filter = DeploymentFilter::new(0, 10).with_groups(vec![group1.id, group2.id]);
        let count = repo.count(&filter).await.unwrap();
        assert_eq!(count, 2);
    }
}
