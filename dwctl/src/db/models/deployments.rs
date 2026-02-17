//! Database models for deployments.
//!
//! This module includes support for both regular deployed models (backed by a single
//! inference endpoint) and composite models (virtual models that distribute requests
//! across multiple underlying models based on configurable weights).

use crate::api::models::deployments::{DeployedModelCreate, DeployedModelUpdate};
use crate::types::{DeploymentId, InferenceEndpointId, UserId};
use bon::Builder;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_with::rust::double_option;
use utoipa::ToSchema;

// Mode constants for provider pricing
const MODE_PER_TOKEN: &str = "per_token";
const MODE_HOURLY: &str = "hourly";

/// Provider pricing options (enum for type safety)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ProviderPricing {
    PerToken {
        /// Input price per token (sent/returned as string to preserve precision)
        #[schema(value_type = Option<String>)]
        input_price_per_token: Option<Decimal>,
        /// Output price per token (sent/returned as string to preserve precision)
        #[schema(value_type = Option<String>)]
        output_price_per_token: Option<Decimal>,
    },
    Hourly {
        /// Hourly rate (sent/returned as string to preserve precision)
        #[schema(value_type = String)]
        rate: Decimal,
        /// Input token cost ratio (sent/returned as string to preserve precision)
        #[schema(value_type = String)]
        input_token_cost_ratio: Decimal,
    },
}

/// Flat database fields for provider pricing
#[derive(Debug, Clone, Default)]
pub struct ProviderPricingFields {
    pub mode: Option<String>,
    pub input_price_per_token: Option<Decimal>,
    pub output_price_per_token: Option<Decimal>,
    pub hourly_rate: Option<Decimal>,
    pub input_token_cost_ratio: Option<Decimal>,
}

impl ProviderPricing {
    /// Convert flat database fields to structured provider pricing
    pub fn from_flat_fields(fields: ProviderPricingFields) -> Option<Self> {
        match fields.mode.as_deref() {
            Some(MODE_HOURLY) => match (fields.hourly_rate, fields.input_token_cost_ratio) {
                (Some(rate), Some(input_token_cost_ratio)) => Some(ProviderPricing::Hourly {
                    rate,
                    input_token_cost_ratio,
                }),
                _ => None,
            },
            Some(MODE_PER_TOKEN) => Some(ProviderPricing::PerToken {
                input_price_per_token: fields.input_price_per_token,
                output_price_per_token: fields.output_price_per_token,
            }),
            _ => None,
        }
    }

    /// Convert structured provider pricing to flat database fields
    pub fn to_flat_fields(&self) -> ProviderPricingFields {
        match self {
            ProviderPricing::PerToken {
                input_price_per_token,
                output_price_per_token,
            } => ProviderPricingFields {
                mode: Some(MODE_PER_TOKEN.to_string()),
                input_price_per_token: *input_price_per_token,
                output_price_per_token: *output_price_per_token,
                hourly_rate: None,
                input_token_cost_ratio: None,
            },
            ProviderPricing::Hourly {
                rate,
                input_token_cost_ratio,
            } => ProviderPricingFields {
                mode: Some(MODE_HOURLY.to_string()),
                input_price_per_token: None,
                output_price_per_token: None,
                hourly_rate: Some(*rate),
                input_token_cost_ratio: Some(*input_token_cost_ratio),
            },
        }
    }
}

/// Provider pricing update structure for partial updates
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ProviderPricingUpdate {
    #[default]
    /// No change to provider pricing
    NoChange,
    /// Update per-token pricing fields
    PerToken {
        /// Update input pricing: None = no change, Some(None) = clear, Some(price) = set (sent as string to preserve precision)
        #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
        #[schema(value_type = Option<Option<String>>)]
        input_price_per_token: Option<Option<Decimal>>,
        /// Update output pricing: None = no change, Some(None) = clear, Some(price) = set (sent as string to preserve precision)
        #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
        #[schema(value_type = Option<Option<String>>)]
        output_price_per_token: Option<Option<Decimal>>,
    },
    /// Update hourly pricing fields
    Hourly {
        /// Update hourly rate: None = no change, Some(rate) = set (sent as string to preserve precision)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Option<String>)]
        rate: Option<Decimal>,
        /// Update input token cost ratio: None = no change, Some(ratio) = set (sent as string to preserve precision)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Option<String>)]
        input_token_cost_ratio: Option<Decimal>,
    },
}

/// Parameters for provider pricing database updates
#[derive(Debug, Clone, Default)]
pub struct ProviderPricingUpdateParams {
    pub should_update_mode: bool,
    pub mode: Option<String>,
    pub should_update_input: bool,
    pub input: Option<Decimal>,
    pub should_update_output: bool,
    pub output: Option<Decimal>,
    pub should_update_hourly: bool,
    pub hourly: Option<Decimal>,
    pub should_update_ratio: bool,
    pub ratio: Option<Decimal>,
}

impl ProviderPricingUpdate {
    /// Convert to parameters for database update query
    pub fn to_update_params(&self) -> ProviderPricingUpdateParams {
        match self {
            ProviderPricingUpdate::NoChange => ProviderPricingUpdateParams::default(),
            ProviderPricingUpdate::PerToken {
                input_price_per_token,
                output_price_per_token,
            } => ProviderPricingUpdateParams {
                should_update_mode: true,
                mode: Some(MODE_PER_TOKEN.to_string()),
                should_update_input: input_price_per_token.is_some(),
                input: input_price_per_token.flatten(),
                should_update_output: output_price_per_token.is_some(),
                output: output_price_per_token.flatten(),
                ..Default::default()
            },
            ProviderPricingUpdate::Hourly {
                rate,
                input_token_cost_ratio,
            } => ProviderPricingUpdateParams {
                should_update_mode: true,
                mode: Some(MODE_HOURLY.to_string()),
                should_update_hourly: rate.is_some(),
                hourly: *rate,
                should_update_ratio: input_token_cost_ratio.is_some(),
                ratio: *input_token_cost_ratio,
                ..Default::default()
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum ModelType {
    Chat,
    Embeddings,
    Reranker,
}

impl ModelType {
    /// Detect model type from model name using common patterns
    /// This helps automatically classify models when syncing from endpoints
    pub fn detect_from_name(model_name: &str) -> Self {
        let name_lower = model_name.to_lowercase();

        // Reranker model patterns - check these first as they're most specific
        let reranker_patterns = [
            "rerank",
            "reranker",
            "cross-encoder",
            "bge-reranker",
            "mixedbread-reranker",
            "mxbai-rerank",
        ];

        // Embedding model patterns
        let embedding_patterns = [
            "embed",
            "embedding",
            "ada", // OpenAI's ada embedding models
            "text-embedding",
            "sentence-transformer",
            "all-minilm",
            "bge-",
            "e5-",
        ];

        // Check if model name contains any reranker patterns
        if reranker_patterns.iter().any(|pattern| name_lower.contains(pattern)) {
            return Self::Reranker;
        }

        // Check if model name contains any embedding patterns
        if embedding_patterns.iter().any(|pattern| name_lower.contains(pattern)) {
            return Self::Embeddings;
        }

        // Default to chat for everything else
        Self::Chat
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ModelStatus {
    Active,
    Inactive,
}

impl ModelStatus {
    pub fn to_db_string(&self) -> &'static str {
        match self {
            ModelStatus::Active => "active",
            ModelStatus::Inactive => "inactive",
        }
    }

    pub fn from_db_string(s: &str) -> ModelStatus {
        match s {
            "active" => ModelStatus::Active,
            "inactive" => ModelStatus::Inactive,
            _ => ModelStatus::Active, // Default fallback
        }
    }
}

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

    pub fn try_parse(s: &str) -> Option<Self> {
        match s {
            "weighted_random" => Some(Self::WeightedRandom),
            "priority" => Some(Self::Priority),
            _ => None,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_fallback_status_codes() -> Vec<i32> {
    vec![429, 500, 502, 503, 504]
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
    /// When true, weighted random failover samples with replacement (default: false)
    #[serde(default)]
    pub with_replacement: bool,
    /// Maximum number of failover attempts (default: provider count)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<i32>,
}

impl FallbackConfig {
    pub fn new() -> Self {
        Self {
            enabled: true,
            on_rate_limit: true,
            on_status: default_fallback_status_codes(),
            with_replacement: false,
            max_attempts: None,
        }
    }
}

/// A component of a composite model (a deployed model with a weight)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DeploymentComponent {
    #[schema(value_type = String, format = "uuid")]
    pub deployed_model_id: DeploymentId,
    /// Relative weight for load balancing (1-100)
    pub weight: i32,
    /// Whether this component is active
    pub enabled: bool,
    /// Sort order for priority-based routing (lower = higher priority)
    #[serde(default)]
    pub sort_order: i32,
}

/// Database request for adding a component to a composite model
#[derive(Debug, Clone)]
pub struct DeploymentComponentCreateDBRequest {
    pub composite_model_id: DeploymentId,
    pub deployed_model_id: DeploymentId,
    pub weight: i32,
    pub enabled: bool,
    pub sort_order: i32,
}

/// Database response for a deployment component (flat structure with joined model info)
#[derive(Debug, Clone)]
pub struct DeploymentComponentDBResponse {
    // Component fields
    pub id: uuid::Uuid,
    pub composite_model_id: DeploymentId,
    pub deployed_model_id: DeploymentId,
    pub weight: i32,
    pub enabled: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    // Joined model fields
    pub model_alias: String,
    pub model_name: String,
    pub model_description: Option<String>,
    pub model_type: Option<String>,
    // Joined endpoint fields
    pub endpoint_id: Option<InferenceEndpointId>,
    pub endpoint_name: Option<String>,
}

/// Database request for creating a new deployment
#[derive(Debug, Clone, Builder)]
pub struct DeploymentCreateDBRequest {
    pub created_by: UserId,
    pub model_name: String,
    pub alias: String,
    pub description: Option<String>,
    pub model_type: Option<ModelType>,
    pub capabilities: Option<Vec<String>>,
    /// Inference endpoint for regular models. Must be None for composite models.
    pub hosted_on: Option<InferenceEndpointId>,
    pub requests_per_second: Option<f32>,
    pub burst_size: Option<i32>,
    pub capacity: Option<i32>,
    pub batch_capacity: Option<i32>,
    pub throughput: Option<f32>,
    // Provider/downstream pricing
    pub provider_pricing: Option<ProviderPricing>,
    // Composite model fields
    /// Whether this is a composite model
    #[builder(default)]
    pub is_composite: bool,
    /// Load balancing strategy for composite models (defaults to weighted_random)
    pub lb_strategy: Option<LoadBalancingStrategy>,
    /// Fallback configuration for composite models
    pub fallback_enabled: Option<bool>,
    pub fallback_on_rate_limit: Option<bool>,
    pub fallback_on_status: Option<Vec<i32>>,
    pub fallback_with_replacement: Option<bool>,
    pub fallback_max_attempts: Option<i32>,
    /// Whether to sanitize/filter sensitive data from model responses (defaults to true)
    #[builder(default = true)]
    pub sanitize_responses: bool,
}

impl DeploymentCreateDBRequest {
    /// Creates a deployment request from API model creation data
    pub fn from_api_create(created_by: UserId, create: DeployedModelCreate) -> Self {
        match create {
            DeployedModelCreate::Standard(standard) => Self::builder()
                .created_by(created_by)
                .model_name(standard.model_name.clone())
                .alias(standard.alias.unwrap_or(standard.model_name))
                .maybe_description(standard.description)
                .maybe_model_type(standard.model_type)
                .maybe_capabilities(standard.capabilities)
                .hosted_on(standard.hosted_on)
                .maybe_requests_per_second(standard.requests_per_second)
                .maybe_burst_size(standard.burst_size)
                .maybe_capacity(standard.capacity)
                .maybe_batch_capacity(standard.batch_capacity)
                .maybe_throughput(standard.throughput)
                .maybe_provider_pricing(standard.provider_pricing)
                .is_composite(false)
                .build(),
            DeployedModelCreate::Composite(composite) => Self::builder()
                .created_by(created_by)
                .model_name(composite.model_name.clone())
                .alias(composite.alias.unwrap_or(composite.model_name))
                .maybe_description(composite.description)
                .maybe_model_type(composite.model_type)
                .maybe_capabilities(composite.capabilities)
                .maybe_requests_per_second(composite.requests_per_second)
                .maybe_burst_size(composite.burst_size)
                .maybe_capacity(composite.capacity)
                .maybe_batch_capacity(composite.batch_capacity)
                .maybe_throughput(composite.throughput)
                .is_composite(true)
                .lb_strategy(composite.lb_strategy)
                .fallback_enabled(composite.fallback_enabled)
                .fallback_on_rate_limit(composite.fallback_on_rate_limit)
                .fallback_on_status(composite.fallback_on_status)
                .fallback_with_replacement(composite.fallback_with_replacement)
                .maybe_fallback_max_attempts(composite.fallback_max_attempts)
                .sanitize_responses(composite.sanitize_responses)
                .build(),
        }
    }
}

/// Database request for updating a deployment
#[derive(Debug, Clone, Builder)]
pub struct DeploymentUpdateDBRequest {
    pub model_name: Option<String>,
    pub alias: Option<String>,
    pub description: Option<Option<String>>,
    pub model_type: Option<Option<ModelType>>,
    pub capabilities: Option<Option<Vec<String>>>,
    pub status: Option<ModelStatus>,
    pub last_sync: Option<Option<DateTime<Utc>>>,
    pub deleted: Option<bool>,
    pub requests_per_second: Option<Option<f32>>,
    pub burst_size: Option<Option<i32>>,
    pub capacity: Option<Option<i32>>,
    pub batch_capacity: Option<Option<i32>>,
    pub throughput: Option<Option<f32>>,
    // Provider pricing updates
    pub provider_pricing: Option<ProviderPricingUpdate>,
    // Composite model fields (only applicable when is_composite = true)
    pub lb_strategy: Option<LoadBalancingStrategy>,
    pub fallback_enabled: Option<bool>,
    pub fallback_on_rate_limit: Option<bool>,
    pub fallback_on_status: Option<Vec<i32>>,
    pub fallback_with_replacement: Option<bool>,
    pub fallback_max_attempts: Option<Option<i32>>,
    /// Whether to sanitize/filter sensitive data from model responses
    pub sanitize_responses: Option<bool>,
}

impl From<DeployedModelUpdate> for DeploymentUpdateDBRequest {
    fn from(update: DeployedModelUpdate) -> Self {
        Self::builder()
            .maybe_alias(update.alias)
            .maybe_description(update.description)
            .maybe_model_type(update.model_type)
            .maybe_capabilities(update.capabilities)
            .maybe_requests_per_second(update.requests_per_second)
            .maybe_burst_size(update.burst_size)
            .maybe_capacity(update.capacity)
            .maybe_batch_capacity(update.batch_capacity)
            .maybe_throughput(update.throughput)
            .maybe_provider_pricing(update.provider_pricing)
            .maybe_lb_strategy(update.lb_strategy)
            .maybe_fallback_enabled(update.fallback_enabled)
            .maybe_fallback_on_rate_limit(update.fallback_on_rate_limit)
            .maybe_fallback_on_status(update.fallback_on_status)
            .maybe_fallback_with_replacement(update.fallback_with_replacement)
            .maybe_fallback_max_attempts(update.fallback_max_attempts)
            .maybe_sanitize_responses(update.sanitize_responses)
            .build()
    }
}

impl DeploymentUpdateDBRequest {
    /// Create an update request for sync operations (status and/or last_sync)
    pub fn status_update(status: Option<ModelStatus>, last_sync: DateTime<Utc>) -> Self {
        Self::builder().maybe_status(status).last_sync(Some(last_sync)).build()
    }

    /// Create an update request for hide/unhide operations
    pub fn visibility_update(deleted: bool) -> Self {
        Self::builder().deleted(deleted).build()
    }

    /// Create an update request for alias changes
    pub fn alias_update(new_alias: String) -> Self {
        Self::builder().maybe_alias(Some(new_alias)).build()
    }
}

/// Database response for a deployment
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DeploymentDBResponse {
    pub id: DeploymentId,
    pub model_name: String,
    pub alias: String,
    pub description: Option<String>,
    pub model_type: Option<ModelType>,
    pub capabilities: Option<Vec<String>>,
    pub created_by: UserId,
    /// Inference endpoint for regular models. None for composite models.
    pub hosted_on: Option<InferenceEndpointId>,
    pub status: ModelStatus,
    pub last_sync: Option<DateTime<Utc>>,
    pub deleted: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub requests_per_second: Option<f32>,
    pub burst_size: Option<i32>,
    pub capacity: Option<i32>,
    pub batch_capacity: Option<i32>,
    /// Throughput in requests/second for batch SLA capacity calculations
    pub throughput: Option<f32>,
    // Provider/downstream pricing
    pub provider_pricing: Option<ProviderPricing>,
    // Composite model fields
    /// Whether this is a composite model
    pub is_composite: bool,
    /// Load balancing strategy for composite models
    pub lb_strategy: LoadBalancingStrategy,
    /// Fallback configuration for composite models
    pub fallback_enabled: bool,
    pub fallback_on_rate_limit: bool,
    pub fallback_on_status: Vec<i32>,
    pub fallback_with_replacement: bool,
    pub fallback_max_attempts: Option<i32>,
    /// Whether to sanitize/filter sensitive data from model responses
    pub sanitize_responses: bool,
}
