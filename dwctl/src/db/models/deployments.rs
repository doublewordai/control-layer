//! Database models for deployments.

use crate::api::models::deployments::{DeployedModelCreate, DeployedModelUpdate};
use crate::db::handlers::inference_endpoints::InferenceEndpoints;
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

impl ProviderPricing {
    /// Convert flat database fields to structured provider pricing
    pub fn from_flat_fields(
        mode: Option<String>,
        input_price_per_token: Option<Decimal>,
        output_price_per_token: Option<Decimal>,
        hourly_rate: Option<Decimal>,
        input_token_cost_ratio: Option<Decimal>,
    ) -> Option<Self> {
        match mode.as_deref() {
            Some(MODE_HOURLY) => match (hourly_rate, input_token_cost_ratio) {
                (Some(rate), Some(input_token_cost_ratio)) => Some(ProviderPricing::Hourly {
                    rate,
                    input_token_cost_ratio,
                }),
                _ => None,
            },
            Some(MODE_PER_TOKEN) => Some(ProviderPricing::PerToken {
                input_price_per_token,
                output_price_per_token,
            }),
            _ => None,
        }
    }

    /// Convert structured provider pricing to flat database fields
    pub fn to_flat_fields(&self) -> (Option<String>, Option<Decimal>, Option<Decimal>, Option<Decimal>, Option<Decimal>) {
        match self {
            ProviderPricing::PerToken {
                input_price_per_token,
                output_price_per_token,
            } => (
                Some(MODE_PER_TOKEN.to_string()),
                *input_price_per_token,
                *output_price_per_token,
                None,
                None,
            ),
            ProviderPricing::Hourly {
                rate,
                input_token_cost_ratio,
            } => (
                Some(MODE_HOURLY.to_string()),
                None,
                None,
                Some(*rate),
                Some(*input_token_cost_ratio),
            ),
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

/// Database request for creating a new deployment
#[derive(Debug, Clone, Builder)]
pub struct DeploymentCreateDBRequest {
    pub created_by: UserId,
    pub model_name: String,
    pub alias: String,
    pub description: Option<String>,
    pub model_type: Option<ModelType>,
    pub capabilities: Option<Vec<String>>,
    #[builder(default = InferenceEndpoints::default_endpoint_id())]
    pub hosted_on: InferenceEndpointId,
    pub requests_per_second: Option<f32>,
    pub burst_size: Option<i32>,
    pub capacity: Option<i32>,
    pub batch_capacity: Option<i32>,
    // Provider/downstream pricing
    pub provider_pricing: Option<ProviderPricing>,
}

impl DeploymentCreateDBRequest {
    /// Creates a deployment request from API model creation data
    pub fn from_api_create(created_by: UserId, create: DeployedModelCreate) -> Self {
        Self::builder()
            .created_by(created_by)
            .model_name(create.model_name.clone())
            .alias(create.alias.unwrap_or(create.model_name))
            .maybe_description(create.description)
            .maybe_model_type(create.model_type)
            .maybe_capabilities(create.capabilities)
            .hosted_on(create.hosted_on)
            .maybe_requests_per_second(create.requests_per_second)
            .maybe_burst_size(create.burst_size)
            .maybe_capacity(create.capacity)
            .maybe_batch_capacity(create.batch_capacity)
            .maybe_provider_pricing(create.provider_pricing)
            .build()
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
    // Provider pricing updates
    pub provider_pricing: Option<ProviderPricingUpdate>,
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
            .maybe_provider_pricing(update.provider_pricing)
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
    pub hosted_on: InferenceEndpointId,
    pub status: ModelStatus,
    pub last_sync: Option<DateTime<Utc>>,
    pub deleted: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub requests_per_second: Option<f32>,
    pub burst_size: Option<i32>,
    pub capacity: Option<i32>,
    pub batch_capacity: Option<i32>,
    // Provider/downstream pricing
    pub provider_pricing: Option<ProviderPricing>,
}
