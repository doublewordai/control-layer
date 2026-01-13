//! API request/response models for composite models.
//!
//! Composite models are virtual models that distribute requests across multiple
//! underlying deployed models based on configurable weights.

use super::groups::GroupResponse;
use super::pagination::Pagination;
use crate::db::models::composite_models::{CompositeModelComponent, CompositeModelDBResponse, FallbackConfig, LoadBalancingStrategy};
use crate::db::models::deployments::ModelType;
use crate::types::{CompositeModelId, DeploymentId, GroupId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_with::rust::double_option;
use utoipa::{IntoParams, ToSchema};

/// Query parameters for listing composite models
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct ListCompositeModelsQuery {
    /// Pagination parameters
    #[serde(flatten)]
    #[param(inline)]
    pub pagination: Pagination,
    /// Include related data (comma-separated: "groups", "components")
    pub include: Option<String>,
    /// Filter to only models the current user can access (defaults to false for admins, true for users)
    pub accessible: Option<bool>,
    /// Search query to filter models by alias or description (case-insensitive substring match)
    pub search: Option<String>,
}

/// Query parameters for getting a single composite model
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct GetCompositeModelQuery {
    /// Include related data (comma-separated: "groups", "components")
    pub include: Option<String>,
}

/// Component definition for composite model creation/update
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CompositeModelComponentDefinition {
    /// ID of the deployed model to include as a component
    #[schema(value_type = String, format = "uuid")]
    pub deployed_model_id: DeploymentId,
    /// Relative weight for load balancing (1-100). Higher weight = more traffic
    pub weight: i32,
    /// Whether this component is active (defaults to true)
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

/// The data required to create a new composite model
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CompositeModelCreate {
    /// User-facing alias (e.g., "gpt-4-balanced"). Must be unique across all models.
    pub alias: String,
    /// Optional description of the composite model
    pub description: Option<String>,
    /// Model type (Chat, Embeddings, or Reranker)
    pub model_type: Option<ModelType>,
    /// Global rate limit: requests per second (null = no limit)
    pub requests_per_second: Option<f32>,
    /// Global rate limit: maximum burst size (null = no limit)
    pub burst_size: Option<i32>,
    /// Maximum number of concurrent requests allowed (null = no limit)
    pub capacity: Option<i32>,
    /// Maximum number of concurrent batch requests allowed (null = defaults to capacity or no limit)
    pub batch_capacity: Option<i32>,
    /// Load balancing strategy (weighted_random or priority)
    #[serde(default)]
    pub lb_strategy: LoadBalancingStrategy,
    /// Fallback configuration
    pub fallback: Option<FallbackConfig>,
    /// Components (underlying deployed models with weights)
    pub components: Option<Vec<CompositeModelComponentDefinition>>,
    /// Group IDs that should have access to this composite model
    #[schema(value_type = Option<Vec<String>>)]
    pub groups: Option<Vec<GroupId>>,
}

/// The data required to update a composite model
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CompositeModelUpdate {
    /// User-facing alias
    pub alias: Option<String>,
    /// Description (None = no change, Some(None) = clear, Some(text) = set)
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    pub description: Option<Option<String>>,
    /// Model type (None = no change, Some(None) = clear, Some(type) = set)
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    pub model_type: Option<Option<ModelType>>,
    /// Rate limit: requests per second (None = no change, Some(None) = remove limit)
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    pub requests_per_second: Option<Option<f32>>,
    /// Rate limit: burst size (None = no change, Some(None) = remove limit)
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    pub burst_size: Option<Option<i32>>,
    /// Max concurrent requests (None = no change, Some(None) = remove limit)
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    pub capacity: Option<Option<i32>>,
    /// Max concurrent batch requests (None = no change, Some(None) = remove limit)
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    pub batch_capacity: Option<Option<i32>>,
    /// Load balancing strategy (weighted_random or priority)
    pub lb_strategy: Option<LoadBalancingStrategy>,
    /// Fallback configuration
    pub fallback: Option<FallbackConfig>,
    /// Components - if provided, replaces all existing components
    pub components: Option<Vec<CompositeModelComponentDefinition>>,
    /// Groups - if provided, replaces all existing group assignments
    #[schema(value_type = Option<Vec<String>>)]
    pub groups: Option<Vec<GroupId>>,
}

/// Request to add or update a single component
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CompositeModelComponentUpdate {
    /// Relative weight for load balancing (1-100)
    pub weight: Option<i32>,
    /// Whether this component is active
    pub enabled: Option<bool>,
}

/// Request to add groups to a composite model
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CompositeModelGroupsUpdate {
    /// Group IDs to add
    #[schema(value_type = Vec<String>)]
    pub add: Option<Vec<GroupId>>,
    /// Group IDs to remove
    #[schema(value_type = Vec<String>)]
    pub remove: Option<Vec<GroupId>>,
}

/// Component with enriched information (includes deployed model details)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CompositeModelComponentResponse {
    #[schema(value_type = String, format = "uuid")]
    pub deployed_model_id: DeploymentId,
    /// The alias of the underlying deployed model
    pub deployed_model_alias: Option<String>,
    /// Relative weight for load balancing (1-100)
    pub weight: i32,
    /// Whether this component is active
    pub enabled: bool,
}

/// API response for a composite model
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CompositeModelResponse {
    #[schema(value_type = String, format = "uuid")]
    pub id: CompositeModelId,
    pub alias: String,
    pub description: Option<String>,
    pub model_type: Option<ModelType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "uuid")]
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Rate limit: requests per second (null = no limit)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests_per_second: Option<f32>,
    /// Rate limit: burst size (null = no limit)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub burst_size: Option<i32>,
    /// Max concurrent requests (null = no limit)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity: Option<i32>,
    /// Max concurrent batch requests (null = no limit)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_capacity: Option<i32>,
    /// Load balancing strategy (weighted_random or priority)
    pub lb_strategy: LoadBalancingStrategy,
    /// Fallback configuration
    pub fallback: FallbackConfig,
    /// Components (underlying deployed models with weights) - only included if requested
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(no_recursion)]
    pub components: Option<Vec<CompositeModelComponentResponse>>,
    /// Groups that have access to this composite model - only included if requested
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(no_recursion)]
    pub groups: Option<Vec<GroupResponse>>,
}

impl From<CompositeModelDBResponse> for CompositeModelResponse {
    fn from(db: CompositeModelDBResponse) -> Self {
        Self {
            id: db.id,
            alias: db.alias,
            description: db.description,
            model_type: db.model_type,
            created_by: Some(db.created_by),
            created_at: db.created_at,
            updated_at: db.updated_at,
            requests_per_second: db.requests_per_second,
            burst_size: db.burst_size,
            capacity: db.capacity,
            batch_capacity: db.batch_capacity,
            lb_strategy: db.lb_strategy,
            fallback: FallbackConfig {
                enabled: db.fallback_enabled,
                on_rate_limit: db.fallback_on_rate_limit,
                on_status: db.fallback_on_status,
            },
            components: None,
            groups: None,
        }
    }
}

impl CompositeModelResponse {
    /// Create a response with components included
    pub fn with_components(mut self, components: Vec<CompositeModelComponentResponse>) -> Self {
        self.components = Some(components);
        self
    }

    /// Create a response with groups included
    pub fn with_groups(mut self, groups: Vec<GroupResponse>) -> Self {
        self.groups = Some(groups);
        self
    }

    /// Mask rate limiting information (sets to None for users without permission)
    pub fn mask_rate_limiting(mut self) -> Self {
        self.requests_per_second = None;
        self.burst_size = None;
        self
    }

    /// Mask capacity information (sets to None for users without permission)
    pub fn mask_capacity(mut self) -> Self {
        self.capacity = None;
        self.batch_capacity = None;
        self
    }

    /// Mask created_by field (sets to None for users without system access)
    pub fn mask_created_by(mut self) -> Self {
        self.created_by = None;
        self
    }
}
