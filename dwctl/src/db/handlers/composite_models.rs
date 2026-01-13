//! Database repository for composite models.
//!
//! Composite models are virtual models that distribute requests across multiple
//! underlying deployed models based on configurable weights.

use crate::db::{
    errors::{DbError, Result},
    handlers::repository::Repository,
    models::{
        composite_models::{
            CompositeModelComponentCreateDBRequest, CompositeModelComponentDBResponse, CompositeModelCreateDBRequest,
            CompositeModelDBResponse, CompositeModelGroupCreateDBRequest, CompositeModelGroupDBResponse, CompositeModelUpdateDBRequest,
            LoadBalancingStrategy,
        },
        deployments::ModelType,
    },
};
use crate::types::{CompositeModelId, DeploymentId, GroupId, UserId, abbrev_uuid};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection, query_builder::QueryBuilder};
use std::collections::HashMap;
use tracing::instrument;

/// Filter options for listing composite models
#[derive(Debug, Clone)]
pub struct CompositeModelFilter {
    pub skip: i64,
    pub limit: i64,
    pub accessible_to: Option<UserId>,
    pub search: Option<String>,
}

impl CompositeModelFilter {
    pub fn new(skip: i64, limit: i64) -> Self {
        Self {
            skip,
            limit,
            accessible_to: None,
            search: None,
        }
    }

    pub fn with_accessible_to(mut self, user_id: UserId) -> Self {
        self.accessible_to = Some(user_id);
        self
    }

    pub fn with_search(mut self, search: String) -> Self {
        self.search = Some(search);
        self
    }
}

// Database entity model
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
struct CompositeModel {
    pub id: CompositeModelId,
    pub alias: String,
    pub description: Option<String>,
    pub model_type: Option<String>,
    pub requests_per_second: Option<f32>,
    pub burst_size: Option<i32>,
    pub capacity: Option<i32>,
    pub batch_capacity: Option<i32>,
    /// Load balancing strategy (weighted_random or priority)
    pub lb_strategy: Option<String>,
    /// Whether fallback is enabled
    pub fallback_enabled: Option<bool>,
    /// Whether to fallback on rate limit
    pub fallback_on_rate_limit: Option<bool>,
    /// HTTP status codes that trigger fallback
    pub fallback_on_status: Option<Vec<i32>>,
    pub created_by: UserId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// Database entity model for components
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
struct CompositeModelComponentRow {
    pub id: uuid::Uuid,
    pub composite_model_id: CompositeModelId,
    pub deployed_model_id: DeploymentId,
    pub weight: i32,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

// Database entity model for group assignments
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
struct CompositeModelGroupRow {
    pub id: uuid::Uuid,
    pub composite_model_id: CompositeModelId,
    pub group_id: GroupId,
    pub granted_by: Option<UserId>,
    pub granted_at: DateTime<Utc>,
}

fn parse_model_type(s: &str) -> Option<ModelType> {
    match s {
        "CHAT" => Some(ModelType::Chat),
        "EMBEDDINGS" => Some(ModelType::Embeddings),
        "RERANKER" => Some(ModelType::Reranker),
        _ => None,
    }
}

fn model_type_to_string(t: &ModelType) -> &'static str {
    match t {
        ModelType::Chat => "CHAT",
        ModelType::Embeddings => "EMBEDDINGS",
        ModelType::Reranker => "RERANKER",
    }
}

impl From<CompositeModel> for CompositeModelDBResponse {
    fn from(m: CompositeModel) -> Self {
        // Parse lb_strategy from string, defaulting to WeightedRandom
        let lb_strategy = m
            .lb_strategy
            .as_deref()
            .and_then(LoadBalancingStrategy::from_str)
            .unwrap_or_default();

        Self {
            id: m.id,
            alias: m.alias,
            description: m.description,
            model_type: m.model_type.as_deref().and_then(parse_model_type),
            requests_per_second: m.requests_per_second,
            burst_size: m.burst_size,
            capacity: m.capacity,
            batch_capacity: m.batch_capacity,
            lb_strategy,
            fallback_enabled: m.fallback_enabled.unwrap_or(true),
            fallback_on_rate_limit: m.fallback_on_rate_limit.unwrap_or(true),
            fallback_on_status: m.fallback_on_status.unwrap_or_else(|| vec![429, 500, 502, 503, 504]),
            created_by: m.created_by,
            created_at: m.created_at,
            updated_at: m.updated_at,
        }
    }
}

impl From<CompositeModelComponentRow> for CompositeModelComponentDBResponse {
    fn from(r: CompositeModelComponentRow) -> Self {
        Self {
            id: r.id,
            composite_model_id: r.composite_model_id,
            deployed_model_id: r.deployed_model_id,
            weight: r.weight,
            enabled: r.enabled,
            created_at: r.created_at,
        }
    }
}

impl From<CompositeModelGroupRow> for CompositeModelGroupDBResponse {
    fn from(r: CompositeModelGroupRow) -> Self {
        Self {
            id: r.id,
            composite_model_id: r.composite_model_id,
            group_id: r.group_id,
            granted_by: r.granted_by,
            granted_at: r.granted_at,
        }
    }
}

pub struct CompositeModels<'c> {
    db: &'c mut PgConnection,
}

#[async_trait::async_trait]
impl<'c> Repository for CompositeModels<'c> {
    type CreateRequest = CompositeModelCreateDBRequest;
    type UpdateRequest = CompositeModelUpdateDBRequest;
    type Response = CompositeModelDBResponse;
    type Id = CompositeModelId;
    type Filter = CompositeModelFilter;

    #[instrument(skip(self, request), fields(alias = %request.alias), err)]
    async fn create(&mut self, request: &Self::CreateRequest) -> Result<Self::Response> {
        let alias = request.alias.trim();
        if alias.is_empty() {
            return Err(DbError::InvalidModelField { field: "alias" });
        }

        let model_type_str = request.model_type.as_ref().map(model_type_to_string);
        let lb_strategy_str = request.lb_strategy.as_ref().map(LoadBalancingStrategy::as_str);

        let model = sqlx::query_as!(
            CompositeModel,
            r#"
            INSERT INTO composite_models (
                alias, description, model_type, requests_per_second, burst_size,
                capacity, batch_capacity, lb_strategy, fallback_enabled,
                fallback_on_rate_limit, fallback_on_status, created_by
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            RETURNING *
            "#,
            alias,
            request.description,
            model_type_str,
            request.requests_per_second,
            request.burst_size,
            request.capacity,
            request.batch_capacity,
            lb_strategy_str,
            request.fallback_enabled,
            request.fallback_on_rate_limit,
            request.fallback_on_status.as_deref(),
            request.created_by
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(CompositeModelDBResponse::from(model))
    }

    #[instrument(skip(self), fields(composite_model_id = %abbrev_uuid(&id)), err)]
    async fn get_by_id(&mut self, id: Self::Id) -> Result<Option<Self::Response>> {
        let model = sqlx::query_as!(CompositeModel, "SELECT * FROM composite_models WHERE id = $1", id)
            .fetch_optional(&mut *self.db)
            .await?;

        Ok(model.map(CompositeModelDBResponse::from))
    }

    #[instrument(skip(self, ids), fields(count = ids.len()), err)]
    async fn get_bulk(&mut self, ids: Vec<Self::Id>) -> Result<HashMap<Self::Id, Self::Response>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }

        let models = sqlx::query_as!(CompositeModel, "SELECT * FROM composite_models WHERE id = ANY($1)", ids.as_slice())
            .fetch_all(&mut *self.db)
            .await?;

        let mut result = HashMap::new();
        for model in models {
            result.insert(model.id, CompositeModelDBResponse::from(model));
        }

        Ok(result)
    }

    #[instrument(skip(self), fields(composite_model_id = %abbrev_uuid(&id)), err)]
    async fn delete(&mut self, id: Self::Id) -> Result<bool> {
        let result = sqlx::query!("DELETE FROM composite_models WHERE id = $1", id)
            .execute(&mut *self.db)
            .await?;

        Ok(result.rows_affected() > 0)
    }

    #[instrument(skip(self, request), fields(composite_model_id = %abbrev_uuid(&id)), err)]
    async fn update(&mut self, id: Self::Id, request: &Self::UpdateRequest) -> Result<Self::Response> {
        if let Some(alias) = &request.alias
            && alias.trim().is_empty()
        {
            return Err(DbError::InvalidModelField { field: "alias" });
        }

        let model_type_str: Option<&str> = request
            .model_type
            .as_ref()
            .and_then(|inner| inner.as_ref().map(model_type_to_string));
        let lb_strategy_str = request.lb_strategy.as_ref().map(LoadBalancingStrategy::as_str);

        let model = sqlx::query_as!(
            CompositeModel,
            r#"
            UPDATE composite_models SET
                alias = COALESCE($2, alias),
                description = CASE
                    WHEN $3 THEN $4
                    ELSE description
                END,
                model_type = CASE
                    WHEN $5 THEN $6
                    ELSE model_type
                END,
                requests_per_second = CASE
                    WHEN $7 THEN $8
                    ELSE requests_per_second
                END,
                burst_size = CASE
                    WHEN $9 THEN $10
                    ELSE burst_size
                END,
                capacity = CASE
                    WHEN $11 THEN $12
                    ELSE capacity
                END,
                batch_capacity = CASE
                    WHEN $13 THEN $14
                    ELSE batch_capacity
                END,
                lb_strategy = COALESCE($15, lb_strategy),
                fallback_enabled = COALESCE($16, fallback_enabled),
                fallback_on_rate_limit = COALESCE($17, fallback_on_rate_limit),
                fallback_on_status = COALESCE($18, fallback_on_status),
                updated_at = NOW()
            WHERE id = $1
            RETURNING *
            "#,
            id,
            request.alias.as_ref().map(|s| s.trim()),
            // description
            request.description.is_some() as bool,
            request.description.as_ref().and_then(|inner| inner.as_ref()),
            // model_type
            request.model_type.is_some() as bool,
            model_type_str,
            // requests_per_second
            request.requests_per_second.is_some() as bool,
            request.requests_per_second.as_ref().and_then(|inner| inner.as_ref()),
            // burst_size
            request.burst_size.is_some() as bool,
            request.burst_size.as_ref().and_then(|inner| inner.as_ref()),
            // capacity
            request.capacity.is_some() as bool,
            request.capacity.as_ref().and_then(|inner| inner.as_ref()),
            // batch_capacity
            request.batch_capacity.is_some() as bool,
            request.batch_capacity.as_ref().and_then(|inner| inner.as_ref()),
            // lb_strategy
            lb_strategy_str,
            // fallback_enabled
            request.fallback_enabled,
            // fallback_on_rate_limit
            request.fallback_on_rate_limit,
            // fallback_on_status
            request.fallback_on_status.as_deref(),
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(CompositeModelDBResponse::from(model))
    }

    #[instrument(skip(self, filter), fields(limit = filter.limit, skip = filter.skip), err)]
    async fn list(&mut self, filter: &Self::Filter) -> Result<Vec<Self::Response>> {
        let mut query = QueryBuilder::new("SELECT * FROM composite_models WHERE 1=1");

        // Add accessibility filter if specified
        if let Some(user_id) = filter.accessible_to {
            query.push(" AND id IN (");
            query.push("SELECT cmg.composite_model_id FROM composite_model_groups cmg WHERE cmg.group_id IN (");
            query.push("SELECT ug.group_id FROM user_groups ug WHERE ug.user_id = ");
            query.push_bind(user_id);
            query.push(" UNION SELECT '00000000-0000-0000-0000-000000000000'::uuid WHERE ");
            query.push_bind(user_id);
            query.push(" != '00000000-0000-0000-0000-000000000000'::uuid");
            query.push("))");
        }

        // Add search filter if specified
        if let Some(ref search) = filter.search {
            let search_pattern = format!("%{}%", search.to_lowercase());
            query.push(" AND (LOWER(alias) LIKE ");
            query.push_bind(search_pattern.clone());
            query.push(" OR LOWER(COALESCE(description, '')) LIKE ");
            query.push_bind(search_pattern);
            query.push(")");
        }

        query.push(" ORDER BY created_at DESC LIMIT ");
        query.push_bind(filter.limit);
        query.push(" OFFSET ");
        query.push_bind(filter.skip);

        let models = query.build_query_as::<CompositeModel>().fetch_all(&mut *self.db).await?;

        Ok(models.into_iter().map(CompositeModelDBResponse::from).collect())
    }
}

impl<'c> CompositeModels<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    /// Count composite models matching the filter
    #[instrument(skip(self, filter), err)]
    pub async fn count(&mut self, filter: &CompositeModelFilter) -> Result<i64> {
        let mut query = QueryBuilder::new("SELECT COUNT(*) FROM composite_models WHERE 1=1");

        if let Some(user_id) = filter.accessible_to {
            query.push(" AND id IN (");
            query.push("SELECT cmg.composite_model_id FROM composite_model_groups cmg WHERE cmg.group_id IN (");
            query.push("SELECT ug.group_id FROM user_groups ug WHERE ug.user_id = ");
            query.push_bind(user_id);
            query.push(" UNION SELECT '00000000-0000-0000-0000-000000000000'::uuid WHERE ");
            query.push_bind(user_id);
            query.push(" != '00000000-0000-0000-0000-000000000000'::uuid");
            query.push("))");
        }

        if let Some(ref search) = filter.search {
            let search_pattern = format!("%{}%", search.to_lowercase());
            query.push(" AND (LOWER(alias) LIKE ");
            query.push_bind(search_pattern.clone());
            query.push(" OR LOWER(COALESCE(description, '')) LIKE ");
            query.push_bind(search_pattern);
            query.push(")");
        }

        let count: (i64,) = query.build_query_as().fetch_one(&mut *self.db).await?;
        Ok(count.0)
    }

    /// Get a composite model by its alias
    #[instrument(skip(self), fields(alias = %alias), err)]
    pub async fn get_by_alias(&mut self, alias: &str) -> Result<Option<CompositeModelDBResponse>> {
        let model = sqlx::query_as!(CompositeModel, "SELECT * FROM composite_models WHERE alias = $1", alias)
            .fetch_optional(&mut *self.db)
            .await?;

        Ok(model.map(CompositeModelDBResponse::from))
    }

    // ===== Component Management =====

    /// Add a component to a composite model
    #[instrument(skip(self, request), fields(composite_model_id = %abbrev_uuid(&request.composite_model_id), deployed_model_id = %abbrev_uuid(&request.deployed_model_id)), err)]
    pub async fn add_component(&mut self, request: &CompositeModelComponentCreateDBRequest) -> Result<CompositeModelComponentDBResponse> {
        if request.weight < 1 || request.weight > 100 {
            return Err(DbError::InvalidModelField { field: "weight" });
        }

        let component = sqlx::query_as!(
            CompositeModelComponentRow,
            r#"
            INSERT INTO composite_model_components (composite_model_id, deployed_model_id, weight, enabled)
            VALUES ($1, $2, $3, $4)
            RETURNING *
            "#,
            request.composite_model_id,
            request.deployed_model_id,
            request.weight,
            request.enabled
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(CompositeModelComponentDBResponse::from(component))
    }

    /// Remove a component from a composite model
    #[instrument(skip(self), fields(composite_model_id = %abbrev_uuid(&composite_model_id), deployed_model_id = %abbrev_uuid(&deployed_model_id)), err)]
    pub async fn remove_component(&mut self, composite_model_id: CompositeModelId, deployed_model_id: DeploymentId) -> Result<bool> {
        let result = sqlx::query!(
            "DELETE FROM composite_model_components WHERE composite_model_id = $1 AND deployed_model_id = $2",
            composite_model_id,
            deployed_model_id
        )
        .execute(&mut *self.db)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Update a component's weight and enabled status
    #[instrument(skip(self), fields(composite_model_id = %abbrev_uuid(&composite_model_id), deployed_model_id = %abbrev_uuid(&deployed_model_id)), err)]
    pub async fn update_component(
        &mut self,
        composite_model_id: CompositeModelId,
        deployed_model_id: DeploymentId,
        weight: Option<i32>,
        enabled: Option<bool>,
    ) -> Result<CompositeModelComponentDBResponse> {
        if let Some(w) = weight
            && (w < 1 || w > 100)
        {
            return Err(DbError::InvalidModelField { field: "weight" });
        }

        let component = sqlx::query_as!(
            CompositeModelComponentRow,
            r#"
            UPDATE composite_model_components SET
                weight = COALESCE($3, weight),
                enabled = COALESCE($4, enabled)
            WHERE composite_model_id = $1 AND deployed_model_id = $2
            RETURNING *
            "#,
            composite_model_id,
            deployed_model_id,
            weight,
            enabled
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(CompositeModelComponentDBResponse::from(component))
    }

    /// Get all components for a composite model
    #[instrument(skip(self), fields(composite_model_id = %abbrev_uuid(&composite_model_id)), err)]
    pub async fn get_components(&mut self, composite_model_id: CompositeModelId) -> Result<Vec<CompositeModelComponentDBResponse>> {
        let components = sqlx::query_as!(
            CompositeModelComponentRow,
            "SELECT * FROM composite_model_components WHERE composite_model_id = $1 ORDER BY weight DESC",
            composite_model_id
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(components.into_iter().map(CompositeModelComponentDBResponse::from).collect())
    }

    /// Get components for multiple composite models (bulk)
    #[instrument(skip(self, composite_model_ids), fields(count = composite_model_ids.len()), err)]
    pub async fn get_components_bulk(
        &mut self,
        composite_model_ids: &[CompositeModelId],
    ) -> Result<HashMap<CompositeModelId, Vec<CompositeModelComponentDBResponse>>> {
        if composite_model_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let components = sqlx::query_as!(
            CompositeModelComponentRow,
            "SELECT * FROM composite_model_components WHERE composite_model_id = ANY($1) ORDER BY weight DESC",
            composite_model_ids
        )
        .fetch_all(&mut *self.db)
        .await?;

        let mut result: HashMap<CompositeModelId, Vec<CompositeModelComponentDBResponse>> = HashMap::new();
        for component in components {
            result
                .entry(component.composite_model_id)
                .or_default()
                .push(CompositeModelComponentDBResponse::from(component));
        }

        Ok(result)
    }

    /// Set all components for a composite model (replaces existing)
    #[instrument(skip(self, components), fields(composite_model_id = %abbrev_uuid(&composite_model_id), count = components.len()), err)]
    pub async fn set_components(
        &mut self,
        composite_model_id: CompositeModelId,
        components: Vec<(DeploymentId, i32, bool)>,
    ) -> Result<Vec<CompositeModelComponentDBResponse>> {
        // Validate weights
        for (_, weight, _) in &components {
            if *weight < 1 || *weight > 100 {
                return Err(DbError::InvalidModelField { field: "weight" });
            }
        }

        // Delete existing components
        sqlx::query!(
            "DELETE FROM composite_model_components WHERE composite_model_id = $1",
            composite_model_id
        )
        .execute(&mut *self.db)
        .await?;

        // Insert new components
        let mut result = Vec::new();
        for (deployed_model_id, weight, enabled) in components {
            let component = sqlx::query_as!(
                CompositeModelComponentRow,
                r#"
                INSERT INTO composite_model_components (composite_model_id, deployed_model_id, weight, enabled)
                VALUES ($1, $2, $3, $4)
                RETURNING *
                "#,
                composite_model_id,
                deployed_model_id,
                weight,
                enabled
            )
            .fetch_one(&mut *self.db)
            .await?;

            result.push(CompositeModelComponentDBResponse::from(component));
        }

        Ok(result)
    }

    // ===== Group Management =====

    /// Add a group to a composite model
    #[instrument(skip(self, request), fields(composite_model_id = %abbrev_uuid(&request.composite_model_id), group_id = %abbrev_uuid(&request.group_id)), err)]
    pub async fn add_group(&mut self, request: &CompositeModelGroupCreateDBRequest) -> Result<CompositeModelGroupDBResponse> {
        let group = sqlx::query_as!(
            CompositeModelGroupRow,
            r#"
            INSERT INTO composite_model_groups (composite_model_id, group_id, granted_by)
            VALUES ($1, $2, $3)
            ON CONFLICT (composite_model_id, group_id) DO UPDATE SET granted_by = EXCLUDED.granted_by
            RETURNING *
            "#,
            request.composite_model_id,
            request.group_id,
            request.granted_by
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(CompositeModelGroupDBResponse::from(group))
    }

    /// Remove a group from a composite model
    #[instrument(skip(self), fields(composite_model_id = %abbrev_uuid(&composite_model_id), group_id = %abbrev_uuid(&group_id)), err)]
    pub async fn remove_group(&mut self, composite_model_id: CompositeModelId, group_id: GroupId) -> Result<bool> {
        let result = sqlx::query!(
            "DELETE FROM composite_model_groups WHERE composite_model_id = $1 AND group_id = $2",
            composite_model_id,
            group_id
        )
        .execute(&mut *self.db)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Get all groups for a composite model
    #[instrument(skip(self), fields(composite_model_id = %abbrev_uuid(&composite_model_id)), err)]
    pub async fn get_groups(&mut self, composite_model_id: CompositeModelId) -> Result<Vec<GroupId>> {
        let groups = sqlx::query!(
            "SELECT group_id FROM composite_model_groups WHERE composite_model_id = $1",
            composite_model_id
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(groups.into_iter().map(|r| r.group_id).collect())
    }

    /// Get groups for multiple composite models (bulk)
    #[instrument(skip(self, composite_model_ids), fields(count = composite_model_ids.len()), err)]
    pub async fn get_groups_bulk(&mut self, composite_model_ids: &[CompositeModelId]) -> Result<HashMap<CompositeModelId, Vec<GroupId>>> {
        if composite_model_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let rows = sqlx::query!(
            "SELECT composite_model_id, group_id FROM composite_model_groups WHERE composite_model_id = ANY($1)",
            composite_model_ids
        )
        .fetch_all(&mut *self.db)
        .await?;

        let mut result: HashMap<CompositeModelId, Vec<GroupId>> = HashMap::new();
        for row in rows {
            result.entry(row.composite_model_id).or_default().push(row.group_id);
        }

        Ok(result)
    }

    /// Set all groups for a composite model (replaces existing)
    #[instrument(skip(self, group_ids), fields(composite_model_id = %abbrev_uuid(&composite_model_id), count = group_ids.len()), err)]
    pub async fn set_groups(&mut self, composite_model_id: CompositeModelId, group_ids: Vec<GroupId>, granted_by: UserId) -> Result<()> {
        // Delete existing groups
        sqlx::query!(
            "DELETE FROM composite_model_groups WHERE composite_model_id = $1",
            composite_model_id
        )
        .execute(&mut *self.db)
        .await?;

        // Insert new groups
        for group_id in group_ids {
            sqlx::query!(
                "INSERT INTO composite_model_groups (composite_model_id, group_id, granted_by) VALUES ($1, $2, $3)",
                composite_model_id,
                group_id,
                granted_by
            )
            .execute(&mut *self.db)
            .await?;
        }

        Ok(())
    }

    /// Check if a user has access to a composite model through group membership
    #[instrument(skip(self), fields(composite_model_alias = %alias, user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn check_user_access(&mut self, alias: &str, user_id: UserId) -> Result<Option<CompositeModelDBResponse>> {
        let model = sqlx::query_as!(
            CompositeModel,
            r#"
            SELECT cm.*
            FROM composite_models cm
            JOIN composite_model_groups cmg ON cmg.composite_model_id = cm.id
            WHERE cm.alias = $1
            AND cmg.group_id IN (
                SELECT ug.group_id FROM user_groups ug WHERE ug.user_id = $2
                UNION
                SELECT '00000000-0000-0000-0000-000000000000'::uuid
                WHERE $2 != '00000000-0000-0000-0000-000000000000'::uuid
            )
            LIMIT 1
            "#,
            alias,
            user_id
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(model.map(CompositeModelDBResponse::from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::models::users::{Role, UserCreate, UserResponse},
        db::{
            handlers::{Deployments, Users},
            models::{deployments::DeploymentCreateDBRequest, users::UserCreateDBRequest},
        },
        test::utils::get_test_endpoint_id,
    };
    use sqlx::{Acquire, PgPool};

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
    async fn test_create_composite_model(pool: PgPool) {
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

        let model;
        {
            let mut tx = pool.begin().await.unwrap();
            {
                let mut repo = CompositeModels::new(tx.acquire().await.unwrap());
                let create_request = CompositeModelCreateDBRequest::builder()
                    .created_by(user.id)
                    .alias("test-composite".to_string())
                    .description(Some("Test composite model".to_string()))
                    .model_type(ModelType::Chat)
                    .capacity(100)
                    .build();

                model = repo.create(&create_request).await.unwrap();
            }
            tx.commit().await.unwrap();
        }

        assert_eq!(model.alias, "test-composite");
        assert_eq!(model.description, Some("Test composite model".to_string()));
        assert_eq!(model.model_type, Some(ModelType::Chat));
        assert_eq!(model.capacity, Some(100));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_add_component_to_composite_model(pool: PgPool) {
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

        let composite_model;
        let deployment;
        let component;
        {
            let mut tx = pool.begin().await.unwrap();
            {
                // Create a deployment to use as a component
                let mut deploy_repo = Deployments::new(tx.acquire().await.unwrap());
                let deploy_create = DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("component-model".to_string())
                    .alias("component-deployment".to_string())
                    .hosted_on(test_endpoint_id)
                    .build();
                deployment = deploy_repo.create(&deploy_create).await.unwrap();

                // Create a composite model
                let mut composite_repo = CompositeModels::new(tx.acquire().await.unwrap());
                let create_request = CompositeModelCreateDBRequest::builder()
                    .created_by(user.id)
                    .alias("test-composite-with-component".to_string())
                    .build();
                composite_model = composite_repo.create(&create_request).await.unwrap();

                // Add the deployment as a component
                let component_request = CompositeModelComponentCreateDBRequest {
                    composite_model_id: composite_model.id,
                    deployed_model_id: deployment.id,
                    weight: 50,
                    enabled: true,
                };
                component = composite_repo.add_component(&component_request).await.unwrap();
            }
            tx.commit().await.unwrap();
        }

        assert_eq!(component.composite_model_id, composite_model.id);
        assert_eq!(component.deployed_model_id, deployment.id);
        assert_eq!(component.weight, 50);
        assert!(component.enabled);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_set_components(pool: PgPool) {
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

        let composite_model;
        let deployment1;
        let deployment2;
        let components;
        {
            let mut tx = pool.begin().await.unwrap();
            {
                let mut deploy_repo = Deployments::new(tx.acquire().await.unwrap());

                // Create two deployments
                let deploy_create1 = DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("component-model-1".to_string())
                    .alias("component-deployment-1".to_string())
                    .hosted_on(test_endpoint_id)
                    .build();
                deployment1 = deploy_repo.create(&deploy_create1).await.unwrap();

                let deploy_create2 = DeploymentCreateDBRequest::builder()
                    .created_by(user.id)
                    .model_name("component-model-2".to_string())
                    .alias("component-deployment-2".to_string())
                    .hosted_on(test_endpoint_id)
                    .build();
                deployment2 = deploy_repo.create(&deploy_create2).await.unwrap();

                // Create composite model and set components
                let mut composite_repo = CompositeModels::new(tx.acquire().await.unwrap());
                let create_request = CompositeModelCreateDBRequest::builder()
                    .created_by(user.id)
                    .alias("test-composite-set-components".to_string())
                    .build();
                composite_model = composite_repo.create(&create_request).await.unwrap();

                components = composite_repo
                    .set_components(composite_model.id, vec![(deployment1.id, 60, true), (deployment2.id, 40, true)])
                    .await
                    .unwrap();
            }
            tx.commit().await.unwrap();
        }

        assert_eq!(components.len(), 2);
        assert!(components.iter().any(|c| c.deployed_model_id == deployment1.id && c.weight == 60));
        assert!(components.iter().any(|c| c.deployed_model_id == deployment2.id && c.weight == 40));
    }
}
