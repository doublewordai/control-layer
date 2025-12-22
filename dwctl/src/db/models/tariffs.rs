//! Database models for model tariffs.

use crate::db::models::api_keys::ApiKeyPurpose;
use crate::types::DeploymentId;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Database representation of a model tariff
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ModelTariff {
    pub id: Uuid,
    pub deployed_model_id: DeploymentId,
    pub name: String,
    pub input_price_per_token: Decimal,
    pub output_price_per_token: Decimal,
    pub valid_from: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,
    /// Optional API key purpose this tariff applies to
    /// If None, tariff is not automatically applied (legacy/manual pricing)
    /// If Some(purpose), tariff applies when model is accessed via that purpose
    pub api_key_purpose: Option<ApiKeyPurpose>,
    /// Optional completion window (SLA) for batch tariffs (e.g., "24h", "1h")
    /// Required for batch tariffs to allow multiple pricing tiers per SLA
    /// Not applicable for realtime/playground tariffs
    pub completion_window: Option<String>,
}

/// Request to create a new tariff
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TariffCreateDBRequest {
    pub deployed_model_id: DeploymentId,
    pub name: String,
    pub input_price_per_token: Decimal,
    pub output_price_per_token: Decimal,
    /// Optional API key purpose this tariff applies to
    pub api_key_purpose: Option<ApiKeyPurpose>,
    /// Optional completion window (SLA) for batch tariffs (e.g., "24h", "1h")
    /// Required when api_key_purpose is Batch to support multiple pricing tiers per SLA
    pub completion_window: Option<String>,
    /// Optional valid_from timestamp (defaults to NOW())
    pub valid_from: Option<DateTime<Utc>>,
}

/// Response from database after creating or updating a tariff
pub type TariffDBResponse = ModelTariff;
