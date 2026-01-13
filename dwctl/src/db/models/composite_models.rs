//! Database models for composite models.
//!
//! Composite models are virtual models that distribute requests across multiple
//! underlying deployed models based on configurable weights.

use crate::db::models::deployments::ModelType;
use crate::types::{CompositeModelId, DeploymentId, GroupId, UserId};
use bon::Builder;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Load balancing strategy for composite models
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalancingStrategy {
    /// Distribute requests randomly based on weights (default)
    #[default]
    WeightedRandom,
    /// Try providers in order of weight (highest first), falling back on failure
    Priority,
}

impl LoadBalancingStrategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::WeightedRandom => "weighted_random",
            Self::Priority => "priority",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "weighted_random" => Some(Self::WeightedRandom),
            "priority" => Some(Self::Priority),
            _ => None,
        }
    }
}

/// Fallback configuration for composite models
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct FallbackConfig {
    /// Whether fallback is enabled (default: true)
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Fall back when provider is rate limited (default: true)
    #[serde(default = "default_true")]
    pub on_rate_limit: bool,
    /// HTTP status codes that trigger fallback (default: [429, 500, 502, 503, 504])
    #[serde(default = "default_fallback_status_codes")]
    pub on_status: Vec<i32>,
}

fn default_true() -> bool {
    true
}

fn default_fallback_status_codes() -> Vec<i32> {
    vec![429, 500, 502, 503, 504]
}

impl FallbackConfig {
    pub fn new() -> Self {
        Self {
            enabled: true,
            on_rate_limit: true,
            on_status: default_fallback_status_codes(),
        }
    }
}

/// Database request for creating a new composite model
#[derive(Debug, Clone, Builder)]
pub struct CompositeModelCreateDBRequest {
    pub created_by: UserId,
    pub alias: String,
    pub description: Option<String>,
    pub model_type: Option<ModelType>,
    pub requests_per_second: Option<f32>,
    pub burst_size: Option<i32>,
    pub capacity: Option<i32>,
    pub batch_capacity: Option<i32>,
    /// Load balancing strategy (defaults to weighted_random)
    pub lb_strategy: Option<LoadBalancingStrategy>,
    /// Fallback configuration
    pub fallback_enabled: Option<bool>,
    pub fallback_on_rate_limit: Option<bool>,
    pub fallback_on_status: Option<Vec<i32>>,
}

/// Database request for updating a composite model
#[derive(Debug, Clone, Builder, Default)]
pub struct CompositeModelUpdateDBRequest {
    pub alias: Option<String>,
    pub description: Option<Option<String>>,
    pub model_type: Option<Option<ModelType>>,
    pub requests_per_second: Option<Option<f32>>,
    pub burst_size: Option<Option<i32>>,
    pub capacity: Option<Option<i32>>,
    pub batch_capacity: Option<Option<i32>>,
    pub lb_strategy: Option<LoadBalancingStrategy>,
    pub fallback_enabled: Option<bool>,
    pub fallback_on_rate_limit: Option<bool>,
    pub fallback_on_status: Option<Vec<i32>>,
}

/// Database response for a composite model
#[derive(Debug, Clone)]
pub struct CompositeModelDBResponse {
    pub id: CompositeModelId,
    pub alias: String,
    pub description: Option<String>,
    pub model_type: Option<ModelType>,
    pub requests_per_second: Option<f32>,
    pub burst_size: Option<i32>,
    pub capacity: Option<i32>,
    pub batch_capacity: Option<i32>,
    pub lb_strategy: LoadBalancingStrategy,
    pub fallback_enabled: bool,
    pub fallback_on_rate_limit: bool,
    pub fallback_on_status: Vec<i32>,
    pub created_by: UserId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A component of a composite model (a deployed model with a weight)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CompositeModelComponent {
    #[schema(value_type = String, format = "uuid")]
    pub deployed_model_id: DeploymentId,
    /// Relative weight for load balancing (1-100)
    pub weight: i32,
    /// Whether this component is active
    pub enabled: bool,
}

/// Database request for adding a component to a composite model
#[derive(Debug, Clone)]
pub struct CompositeModelComponentCreateDBRequest {
    pub composite_model_id: CompositeModelId,
    pub deployed_model_id: DeploymentId,
    pub weight: i32,
    pub enabled: bool,
}

/// Database response for a composite model component
#[derive(Debug, Clone)]
pub struct CompositeModelComponentDBResponse {
    pub id: uuid::Uuid,
    pub composite_model_id: CompositeModelId,
    pub deployed_model_id: DeploymentId,
    pub weight: i32,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

/// Database request for granting group access to a composite model
#[derive(Debug, Clone)]
pub struct CompositeModelGroupCreateDBRequest {
    pub composite_model_id: CompositeModelId,
    pub group_id: GroupId,
    pub granted_by: Option<UserId>,
}

/// Database response for a composite model group assignment
#[derive(Debug, Clone)]
pub struct CompositeModelGroupDBResponse {
    pub id: uuid::Uuid,
    pub composite_model_id: CompositeModelId,
    pub group_id: GroupId,
    pub granted_by: Option<UserId>,
    pub granted_at: DateTime<Utc>,
}
