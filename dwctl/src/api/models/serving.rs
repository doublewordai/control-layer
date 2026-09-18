//! Operator-facing (platform manager) views of an organisation's serving deal and of
//! the organisations that have one on a model. Read-only: the data is written by the
//! organisation catalog (overlays, prices) and the account settings endpoints.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::tariffs::TariffResponse;
use crate::types::{DeploymentId, UserId};

/// One organisation's overrides on one model (a `model_overlays` row).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OverlayResponse {
    #[schema(value_type = String, format = "uuid")]
    pub organization_id: UserId,
    /// The organisation's account username (what the catalog file names).
    pub organization_name: String,
    #[schema(value_type = String, format = "uuid")]
    pub deployed_model_id: DeploymentId,
    pub alias: String,
    /// Overrides the account's default class on this model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_serving_class: Option<String>,
    /// Explicit targets `{ttft_ms, itl_ms, priority}` sent as-is (resolved as `custom`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub targets: Option<serde_json::Value>,
    /// Overrides the account's `self_hosted_only` on this model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub self_hosted_only: Option<bool>,
    /// Which catalog file wrote the row; null = written by hand and left alone by the catalog.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provisioning_source: Option<String>,
    pub updated_at: DateTime<Utc>,
}

/// One organisation's active prompt-cache multipliers on one model.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OrganizationCacheTariffResponse {
    #[schema(value_type = String, format = "uuid")]
    pub deployed_model_id: DeploymentId,
    pub alias: String,
    #[schema(value_type = String)]
    pub write_multiplier_5m: Decimal,
    #[schema(value_type = String)]
    pub write_multiplier_1h: Decimal,
    #[schema(value_type = String)]
    pub write_multiplier_24h: Decimal,
    #[schema(value_type = String)]
    pub read_multiplier: Decimal,
    pub min_prefix_tokens: i32,
    pub valid_from: DateTime<Utc>,
}

/// Everything serving-related about one organisation, for the admin console's
/// organisation page: its account settings, its per-model overlays and its own prices.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OrganizationServingResponse {
    #[schema(value_type = String, format = "uuid")]
    pub organization_id: UserId,
    /// Elevated classes the organisation holds.
    pub granted_serving_classes: Vec<String>,
    /// Class its requests ask for when the request names none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_serving_class: Option<String>,
    /// Never fall over to an external provider.
    pub self_hosted_only: bool,
    /// Per-model overrides, one per model the organisation has a deal on.
    pub overlays: Vec<OverlayResponse>,
    /// The organisation's own active token prices, across models
    /// (`organization_id` is set on every row).
    pub tariffs: Vec<TariffResponse>,
    /// The organisation's own active cache multipliers, across models.
    pub cache_tariffs: Vec<OrganizationCacheTariffResponse>,
}

/// Row shape shared by the two overlay queries.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct OverlayRow {
    pub organization_id: Uuid,
    pub organization_name: String,
    pub deployed_model_id: Uuid,
    pub alias: String,
    pub default_serving_class: Option<String>,
    pub targets: Option<serde_json::Value>,
    pub self_hosted_only: Option<bool>,
    pub provisioning_source: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl From<OverlayRow> for OverlayResponse {
    fn from(row: OverlayRow) -> Self {
        Self {
            organization_id: row.organization_id,
            organization_name: row.organization_name,
            deployed_model_id: row.deployed_model_id,
            alias: row.alias,
            default_serving_class: row.default_serving_class,
            targets: row.targets,
            self_hosted_only: row.self_hosted_only,
            provisioning_source: row.provisioning_source,
            updated_at: row.updated_at,
        }
    }
}
