//! API response models for model tariffs (read-only).

use crate::{
    db::models::{api_keys::ApiKeyPurpose, tariffs::ModelTariff},
    types::DeploymentId,
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// API response for a tariff
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TariffResponse {
    #[schema(value_type = String, format = "uuid")]
    pub id: Uuid,
    #[schema(value_type = String, format = "uuid")]
    pub deployed_model_id: DeploymentId,
    pub name: String,
    /// Input price per token (sent/returned as string to preserve precision)
    #[schema(value_type = String)]
    pub input_price_per_token: Decimal,
    /// Output price per token (sent/returned as string to preserve precision)
    #[schema(value_type = String)]
    pub output_price_per_token: Decimal,
    /// Optional API key purpose this tariff applies to (realtime, batch, playground)
    /// If null, tariff is not automatically applied
    pub api_key_purpose: Option<ApiKeyPurpose>,
    /// Optional completion window for batch tariffs (e.g., "24h", "1h")
    /// Only applicable when api_key_purpose is Batch
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "24h")]
    pub completion_window: Option<String>,
    pub valid_from: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,
    /// True once valid_from is reached and before valid_until, if an end is set.
    #[serde(default)]
    pub is_active: bool,
    /// Set when this is an organisation's own price rather than the model's general
    /// price. A customer sees it only on their own organisation's rows; platform
    /// managers see every organisation's rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "uuid")]
    pub organization_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serving_class: Option<String>,
}

impl From<ModelTariff> for TariffResponse {
    fn from(tariff: ModelTariff) -> Self {
        Self {
            id: tariff.id,
            deployed_model_id: tariff.deployed_model_id,
            name: tariff.name,
            input_price_per_token: tariff.input_price_per_token,
            output_price_per_token: tariff.output_price_per_token,
            api_key_purpose: tariff.api_key_purpose,
            completion_window: tariff.completion_window,
            valid_from: tariff.valid_from,
            valid_until: tariff.valid_until,
            is_active: tariff.valid_from <= Utc::now() && tariff.valid_until.is_none_or(|until| until > Utc::now()),
            organization_id: tariff.user_id,
            serving_class: tariff.serving_class,
        }
    }
}
