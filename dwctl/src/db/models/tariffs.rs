//! Database models for model tariffs.

use crate::types::DeploymentId;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Database representation of a model tariff
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ModelTariff {
    pub id: Uuid,
    pub deployed_model_id: DeploymentId,
    pub name: String,
    pub input_price_per_token: Decimal,
    pub output_price_per_token: Decimal,
    pub is_default: bool,
    pub valid_from: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Request to create a new tariff
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TariffCreateDBRequest {
    pub deployed_model_id: DeploymentId,
    pub name: String,
    pub input_price_per_token: Decimal,
    pub output_price_per_token: Decimal,
    pub is_default: bool,
    /// Optional valid_from timestamp (defaults to NOW())
    pub valid_from: Option<DateTime<Utc>>,
}

/// Request to update an existing tariff
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TariffUpdateDBRequest {
    pub input_price_per_token: Option<Decimal>,
    pub output_price_per_token: Option<Decimal>,
    pub is_default: Option<bool>,
}

/// Response from database after creating or updating a tariff
pub type TariffDBResponse = ModelTariff;
