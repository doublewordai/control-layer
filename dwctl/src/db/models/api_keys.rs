//! Database models for API keys.

use crate::api::models::api_keys::ApiKeyCreate;
use crate::types::{ApiKeyId, DeploymentId, UserId};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// API key purpose - determines which endpoints the key can access
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, sqlx::Type)]
#[sqlx(type_name = "VARCHAR", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum ApiKeyPurpose {
    /// Platform/management API access (/admin/api/*)
    Platform,
    /// Real-time inference API access (/ai/*) - user-created keys
    Realtime,
    /// Batch inference access (/ai/*) - hidden keys for batch processing
    Batch,
    /// Playground inference access (/ai/*) - hidden keys for dashboard playground
    Playground,
}

/// Whether an API key `purpose` (raw DB string) is permitted on the inference
/// data plane (`/ai/*`). `platform` keys are management-only: they are not
/// accepted for inference - rejected by the `current_user` gate and the Flex
/// pre-dispatch check, and excluded from onwards' key set (so onwards 403s them
/// if one is presented).
///
/// Single source of truth for those Rust-side checks. The onwards key-sync
/// queries in `sync::onwards_config` mirror this list as a SQL
/// `ak.purpose IN (...)` filter (SQL cannot call this) - keep them in sync.
pub fn is_inference_purpose(purpose: &str) -> bool {
    matches!(purpose, "realtime" | "batch" | "playground")
}

#[cfg(test)]
mod purpose_tests {
    use super::is_inference_purpose;

    #[test]
    fn inference_purposes_allowed_platform_rejected() {
        assert!(is_inference_purpose("realtime"));
        assert!(is_inference_purpose("batch"));
        assert!(is_inference_purpose("playground"));
        assert!(!is_inference_purpose("platform"));
        assert!(!is_inference_purpose("unknown"));
    }
}

/// Database request for creating a new API key
#[derive(Debug, Clone)]
pub struct ApiKeyCreateDBRequest {
    pub user_id: UserId,
    pub name: String,
    pub description: Option<String>,
    pub purpose: ApiKeyPurpose,
    pub requests_per_second: Option<f32>,
    pub burst_size: Option<i32>,
    /// The individual user who created this key
    pub created_by: UserId,
}

impl ApiKeyCreateDBRequest {
    pub fn new(user_id: UserId, created_by: UserId, create: ApiKeyCreate) -> Self {
        Self {
            user_id,
            name: create.name,
            description: create.description,
            purpose: create.purpose,
            requests_per_second: create.requests_per_second,
            burst_size: create.burst_size,
            created_by,
        }
    }
}

/// Database request for updating an API key
#[derive(Debug, Clone)]
pub struct ApiKeyUpdateDBRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub requests_per_second: Option<Option<f32>>,
    pub burst_size: Option<Option<i32>>,
}

/// Database response for an API key
#[derive(Debug, Clone)]
pub struct ApiKeyDBResponse {
    pub id: ApiKeyId,
    pub name: String,
    pub description: Option<String>,
    pub secret: String,
    pub purpose: ApiKeyPurpose,
    pub user_id: UserId,
    pub created_by: UserId,
    pub created_at: DateTime<Utc>,
    pub last_used: Option<DateTime<Utc>>,
    pub model_access: Vec<DeploymentId>,
    pub requests_per_second: Option<f32>,
    pub burst_size: Option<i32>,
    /// Optional spending cap (credits) for this key's cap scope. NULL = uncapped.
    pub spend_limit: Option<Decimal>,
    /// Cap reset period: None = one-off, else daily/weekly/monthly on
    /// calendar-aligned UTC boundaries (never rolling windows).
    pub spend_limit_interval: Option<String>,
    /// Set only on hidden cap-scope child keys; see migration 122. Spend
    /// accounting/enforcement group by COALESCE(parent_api_key_id, id).
    pub parent_api_key_id: Option<ApiKeyId>,
}
