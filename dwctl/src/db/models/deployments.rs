use crate::api::models::deployments::{DeployedModelCreate, DeployedModelUpdate};
use crate::db::handlers::inference_endpoints::InferenceEndpoints;
use crate::types::{DeploymentId, InferenceEndpointId, UserId};
use bon::Builder;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_with::rust::double_option;
use utoipa::ToSchema;

/// Token-based pricing structure
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema, Default)]
pub struct TokenPricing {
    #[schema(value_type = Option<f64>)]
    pub input_price_per_token: Option<Decimal>,
    #[schema(value_type = Option<f64>)]
    pub output_price_per_token: Option<Decimal>,
}

/// Token pricing update structure for partial updates
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct TokenPricingUpdate {
    /// Update input pricing: None = no change, Some(None) = clear, Some(price) = set
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    #[schema(value_type = Option<Option<f64>>)]
    pub input_price_per_token: Option<Option<Decimal>>,
    /// Update output pricing: None = no change, Some(None) = clear, Some(price) = set
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    #[schema(value_type = Option<Option<f64>>)]
    pub output_price_per_token: Option<Option<Decimal>>,
}

/// Provider pricing options (enum for type safety)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ProviderPricing {
    PerToken {
        #[schema(value_type = Option<f64>)]
        input_price_per_token: Option<Decimal>,
        #[schema(value_type = Option<f64>)]
        output_price_per_token: Option<Decimal>,
    },
    Hourly {
        #[schema(value_type = f64)]
        rate: Decimal,
        #[schema(value_type = Option<f64>)]
        input_token_cost_ratio: Decimal,
    },
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
        /// Update input pricing: None = no change, Some(None) = clear, Some(price) = set
        #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
        #[schema(value_type = Option<Option<f64>>)]
        input_price_per_token: Option<Option<Decimal>>,
        /// Update output pricing: None = no change, Some(None) = clear, Some(price) = set
        #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
        #[schema(value_type = Option<Option<f64>>)]
        output_price_per_token: Option<Option<Decimal>>,
    },
    /// Update hourly pricing fields
    Hourly {
        /// Update hourly rate: None = no change, Some(rate) = set
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Option<f64>)]
        rate: Option<Decimal>,
        /// Update input token cost ratio: None = no change, Some(ratio) = set
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Option<f64>)]
        input_token_cost_ratio: Option<Decimal>,
    },
}

/// Complete model pricing structure (internal storage)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema, Default)]
pub struct ModelPricing {
    /// User-facing pricing (always per-token)
    pub upstream: Option<TokenPricing>,
    /// Provider pricing (flexible mode)
    pub downstream: Option<ProviderPricing>,
}

/// Model pricing update structure for partial updates
#[derive(Debug, Clone, Default)]
pub struct ModelPricingUpdate {
    /// Customer pricing partial update
    pub upstream: Option<TokenPricingUpdate>,
    /// Provider pricing partial update
    pub downstream: Option<ProviderPricingUpdate>,
}

/// Clean intermediate struct for pricing update parameters
#[derive(Debug, Clone, Default)]
pub struct PricingUpdateParams {
    // Customer pricing fields
    pub should_update_customer_input: bool,
    pub customer_input: Option<Decimal>,
    pub should_update_customer_output: bool,
    pub customer_output: Option<Decimal>,

    // Downstream pricing fields
    pub should_update_downstream_mode: bool,
    pub downstream_mode: Option<String>,
    pub should_update_downstream_input: bool,
    pub downstream_input: Option<Decimal>,
    pub should_update_downstream_output: bool,
    pub downstream_output: Option<Decimal>,
    pub should_update_downstream_hourly: bool,
    pub downstream_hourly: Option<Decimal>,
    pub should_update_downstream_ratio: bool,
    pub downstream_ratio: Option<Decimal>,
}

impl ModelPricingUpdate {
    /// Convert to clean parameter structure for database updates
    pub fn to_update_params(&self) -> PricingUpdateParams {
        let mut params = PricingUpdateParams::default();

        // Extract customer pricing field flags and values
        if let Some(upstream) = &self.upstream {
            params.should_update_customer_input = upstream.input_price_per_token.is_some();
            params.customer_input = upstream.input_price_per_token.flatten();

            params.should_update_customer_output = upstream.output_price_per_token.is_some();
            params.customer_output = upstream.output_price_per_token.flatten();
        }

        // Extract downstream pricing fields with individual flags
        if let Some(downstream) = &self.downstream {
            match downstream {
                ProviderPricingUpdate::NoChange => {
                    // All downstream flags remain false
                }
                ProviderPricingUpdate::PerToken {
                    input_price_per_token,
                    output_price_per_token,
                } => {
                    // Always update mode when switching to per_token
                    params.should_update_downstream_mode = true;
                    params.downstream_mode = Some("per_token".to_string());

                    // Update individual per-token fields
                    params.should_update_downstream_input = input_price_per_token.is_some();
                    params.downstream_input = input_price_per_token.flatten();

                    params.should_update_downstream_output = output_price_per_token.is_some();
                    params.downstream_output = output_price_per_token.flatten();
                }
                ProviderPricingUpdate::Hourly {
                    rate,
                    input_token_cost_ratio,
                } => {
                    // Always update mode when switching to hourly
                    params.should_update_downstream_mode = true;
                    params.downstream_mode = Some("hourly".to_string());

                    // Update individual hourly fields
                    params.should_update_downstream_hourly = rate.is_some();
                    params.downstream_hourly = *rate;

                    params.should_update_downstream_ratio = input_token_cost_ratio.is_some();
                    params.downstream_ratio = *input_token_cost_ratio;
                }
            }
        }

        params
    }
}

/// Flat pricing fields for database storage
#[derive(Debug, Clone, Default)]
pub struct FlatPricingFields {
    pub upstream_input_price_per_token: Option<Decimal>,
    pub upstream_output_price_per_token: Option<Decimal>,
    pub downstream_pricing_mode: Option<String>,
    pub downstream_input_price_per_token: Option<Decimal>,
    pub downstream_output_price_per_token: Option<Decimal>,
    pub downstream_hourly_rate: Option<Decimal>,
    pub downstream_input_token_cost_ratio: Option<Decimal>,
}

impl ModelPricing {
    /// Convert structured pricing to flat database fields
    pub fn to_flat_fields(&self) -> FlatPricingFields {
        let upstream_input_price_per_token = self.upstream.as_ref().and_then(|u| u.input_price_per_token);
        let upstream_output_price_per_token = self.upstream.as_ref().and_then(|u| u.output_price_per_token);

        let (
            downstream_pricing_mode,
            downstream_input_price_per_token,
            downstream_output_price_per_token,
            downstream_hourly_rate,
            downstream_input_token_cost_ratio,
        ) = match &self.downstream {
            Some(ProviderPricing::PerToken {
                input_price_per_token,
                output_price_per_token,
            }) => (
                Some("per_token".to_string()),
                *input_price_per_token,
                *output_price_per_token,
                None,
                None,
            ),
            Some(ProviderPricing::Hourly {
                rate,
                input_token_cost_ratio,
            }) => (Some("hourly".to_string()), None, None, Some(*rate), Some(*input_token_cost_ratio)),
            None => (None, None, None, None, None),
        };

        FlatPricingFields {
            upstream_input_price_per_token,
            upstream_output_price_per_token,
            downstream_pricing_mode,
            downstream_input_price_per_token,
            downstream_output_price_per_token,
            downstream_hourly_rate,
            downstream_input_token_cost_ratio,
        }
    }

    /// Convert flat database fields to structured pricing
    pub fn from_flat_fields(fields: FlatPricingFields) -> Option<Self> {
        let upstream = match (fields.upstream_input_price_per_token, fields.upstream_output_price_per_token) {
            (None, None) => None,
            (input, output) => Some(TokenPricing {
                input_price_per_token: input,
                output_price_per_token: output,
            }),
        };

        let downstream = match fields.downstream_pricing_mode.as_deref() {
            Some("hourly") => fields
                .downstream_hourly_rate
                .and_then(|rate| fields.downstream_input_token_cost_ratio.map(|ratio| (rate, ratio)))
                .map(|(rate, input_token_cost_ratio)| ProviderPricing::Hourly {
                    rate,
                    input_token_cost_ratio,
                }),
            _ if fields.downstream_input_price_per_token.is_some() || fields.downstream_output_price_per_token.is_some() => {
                Some(ProviderPricing::PerToken {
                    input_price_per_token: fields.downstream_input_price_per_token,
                    output_price_per_token: fields.downstream_output_price_per_token,
                })
            }
            _ => None,
        };

        match (upstream.as_ref(), downstream.as_ref()) {
            (None, None) => None,
            _ => Some(Self { upstream, downstream }),
        }
    }

    /// Convert to customer-facing pricing (simple format)
    pub fn to_customer_pricing(&self) -> Option<TokenPricing> {
        self.upstream.clone()
    }

    /// Create ModelPricing from separate API pricing fields
    pub fn from_api_pricing(pricing: Option<TokenPricing>, downstream_pricing: Option<ProviderPricing>) -> Option<Self> {
        match (pricing, downstream_pricing) {
            (None, None) => None,
            (upstream, downstream) => Some(Self { upstream, downstream }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum ModelType {
    Chat,
    Embeddings,
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
    // Clean structured pricing
    pub pricing: Option<ModelPricing>,
}

impl DeploymentCreateDBRequest {
    /// Creates a deployment request from API model creation data
    pub fn from_api_create(created_by: UserId, create: DeployedModelCreate) -> Self {
        let combined_pricing = ModelPricing::from_api_pricing(create.pricing, create.downstream_pricing);

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
            .maybe_pricing(combined_pricing)
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
    // Pricing updates using double-option pattern
    pub pricing: Option<ModelPricingUpdate>,
}

impl From<DeployedModelUpdate> for DeploymentUpdateDBRequest {
    fn from(update: DeployedModelUpdate) -> Self {
        let pricing_update = if update.pricing.is_some() || update.downstream_pricing.is_some() {
            Some(ModelPricingUpdate {
                upstream: update.pricing,
                downstream: update.downstream_pricing,
            })
        } else {
            None
        };

        Self::builder()
            .maybe_alias(update.alias)  // Change from deployment_name to alias
            .maybe_description(update.description)
            .maybe_model_type(update.model_type)
            .maybe_capabilities(update.capabilities)
            .maybe_requests_per_second(update.requests_per_second)
            .maybe_burst_size(update.burst_size)
            .maybe_pricing(pricing_update)
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
        Self::builder().maybe_alias(Some(new_alias)).build()  // Use maybe_alias with Some()
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
    // Clean structured pricing
    pub pricing: Option<ModelPricing>,
}
