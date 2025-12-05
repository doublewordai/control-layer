//! API response models for model tariffs (read-only).

use crate::{db::models::tariffs::ModelTariff, types::DeploymentId};
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
    pub is_default: bool,
    pub valid_from: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,
    /// Indicates if this tariff is currently active (valid_until IS NULL)
    #[serde(default)]
    pub is_active: bool,
}

impl From<ModelTariff> for TariffResponse {
    fn from(tariff: ModelTariff) -> Self {
        Self {
            id: tariff.id,
            deployed_model_id: tariff.deployed_model_id,
            name: tariff.name,
            input_price_per_token: tariff.input_price_per_token,
            output_price_per_token: tariff.output_price_per_token,
            is_default: tariff.is_default,
            valid_from: tariff.valid_from,
            valid_until: tariff.valid_until,
            is_active: tariff.valid_until.is_none(),
        }
    }
}
