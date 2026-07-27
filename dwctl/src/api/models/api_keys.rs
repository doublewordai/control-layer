//! API request/response models for API keys.

use super::pagination::Pagination;
use crate::db::models::api_keys::{ApiKeyDBResponse, ApiKeyPurpose};
use crate::types::{ApiKeyId, DeploymentId, UserId};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Default API key purpose when not specified
fn default_api_key_purpose() -> ApiKeyPurpose {
    ApiKeyPurpose::Realtime
}

// API Key request models.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ApiKeyCreate {
    pub name: String,
    pub description: Option<String>,
    /// Purpose of the API key. Defaults to 'realtime' if not specified.
    /// 'platform' keys require PlatformManager role. 'batch' and 'playground' are reserved for internal system use.
    #[serde(default = "default_api_key_purpose")]
    pub purpose: ApiKeyPurpose,
    /// Per-API-key rate limit: requests per second (null = no limit)
    pub requests_per_second: Option<f32>,
    /// Per-API-key rate limit: maximum burst size (null = no limit)
    pub burst_size: Option<i32>,
    /// Organization member to attribute this key to. Only usable when the caller has
    /// permission to create keys for any member of the organization (e.g. PlatformManagers
    /// or admins). The specified user must be a member of the org.
    #[schema(value_type = Option<String>, format = "uuid")]
    pub member_id: Option<UserId>,
    /// Optional spending cap (credits) for this key. Covers realtime, batch and
    /// flex usage made with the key; playground and dashboard-created batches
    /// are not counted. Enforced post-hoc (small overshoot possible). Null = no cap.
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub spend_limit: Option<Decimal>,
    /// Cap reset period: null = one-off (spend since the cap was set), or
    /// 'daily' / 'weekly' / 'monthly' on CALENDAR-ALIGNED UTC boundaries (not
    /// rolling windows). Requires spend_limit.
    #[serde(default)]
    pub spend_limit_interval: Option<String>,
}

// API Key update.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ApiKeyUpdate {
    pub name: Option<String>,
    pub description: Option<String>,
    /// Per-API-key rate limit: requests per second (null = no limit, Some(None) = remove limit)
    pub requests_per_second: Option<Option<f32>>,
    /// Per-API-key rate limit: maximum burst size (null = no limit, Some(None) = remove limit)
    pub burst_size: Option<Option<i32>>,
    /// Spending cap (credits). Absent = unchanged; explicit null = remove the
    /// cap; a value = set/change it. Setting a cap where none existed resets
    /// the spend window and provisions cap-scope execution for batch/flex.
    #[serde(default, with = "::serde_with::rust::double_option", skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub spend_limit: Option<Option<Decimal>>,
    /// Cap reset period (see ApiKeyCreate.spend_limit_interval). Absent =
    /// unchanged; explicit null = one-off; a value = daily/weekly/monthly.
    /// Changing the interval resets the spend window.
    #[serde(default, with = "::serde_with::rust::double_option", skip_serializing_if = "Option::is_none")]
    pub spend_limit_interval: Option<Option<String>>,
    /// Re-arm the cap now: zero the current window's counted spend and start a
    /// fresh window. Caps are internal budgeting controls, so the key's owner
    /// may reset them (this does not grant credits).
    #[serde(default)]
    pub reset_window: Option<bool>,
}

// API Key response models
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ApiKeyResponse {
    #[schema(value_type = String, format = "uuid")]
    pub id: ApiKeyId,
    pub name: String,
    pub description: Option<String>,
    pub key: String,
    pub purpose: ApiKeyPurpose,
    #[schema(value_type = String, format = "uuid")]
    pub user_id: UserId,
    /// The individual user who created this key
    #[schema(value_type = String, format = "uuid")]
    pub created_by: UserId,
    pub created_at: DateTime<Utc>,
    pub last_used: Option<DateTime<Utc>>,
    #[schema(value_type = Vec<String>)]
    pub model_access: Vec<DeploymentId>,
    /// Per-API-key rate limit: requests per second (null = no limit)
    pub requests_per_second: Option<f32>,
    /// Per-API-key rate limit: maximum burst size (null = no limit)
    pub burst_size: Option<i32>,
    /// Spending cap in credits (null = no cap)
    #[schema(value_type = Option<String>)]
    pub spend_limit: Option<Decimal>,
    /// Cap reset period: null = one-off, else daily/weekly/monthly (calendar-aligned UTC)
    pub spend_limit_interval: Option<String>,
    /// Spend counted against the cap in the current window (null when never used / uncapped)
    #[schema(value_type = Option<String>)]
    pub spend: Option<Decimal>,
    /// Lifetime tracked spend for this key's cap scope (null when never used / uncapped)
    #[schema(value_type = Option<String>)]
    pub total_spend: Option<Decimal>,
    /// When the current cap window resets (null for one-off caps and uncapped keys)
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ApiKeyInfoResponse {
    #[schema(value_type = String, format = "uuid")]
    pub id: ApiKeyId,
    pub name: String,
    pub description: Option<String>,
    pub purpose: ApiKeyPurpose,
    #[schema(value_type = String, format = "uuid")]
    pub user_id: UserId,
    /// The individual user who created this key
    #[schema(value_type = String, format = "uuid")]
    pub created_by: UserId,
    pub created_at: DateTime<Utc>,
    pub last_used: Option<DateTime<Utc>>,
    #[schema(value_type = Vec<String>)]
    pub model_access: Vec<DeploymentId>,
    /// Per-API-key rate limit: requests per second (null = no limit)
    pub requests_per_second: Option<f32>,
    /// Per-API-key rate limit: maximum burst size (null = no limit)
    pub burst_size: Option<i32>,
    /// Spending cap in credits (null = no cap)
    #[schema(value_type = Option<String>)]
    pub spend_limit: Option<Decimal>,
    /// Cap reset period: null = one-off, else daily/weekly/monthly (calendar-aligned UTC)
    pub spend_limit_interval: Option<String>,
    /// Spend counted against the cap in the current window (null when never used / uncapped)
    #[schema(value_type = Option<String>)]
    pub spend: Option<Decimal>,
    /// Lifetime tracked spend for this key's cap scope (null when never used / uncapped)
    #[schema(value_type = Option<String>)]
    pub total_spend: Option<Decimal>,
    /// When the current cap window resets (null for one-off caps and uncapped keys)
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct ListApiKeysQuery {
    /// Pagination parameters
    #[serde(flatten)]
    #[param(inline)]
    pub pagination: Pagination,
}

impl From<ApiKeyDBResponse> for ApiKeyResponse {
    fn from(db: ApiKeyDBResponse) -> Self {
        Self {
            id: db.id,
            name: db.name,
            description: db.description,
            key: db.secret,
            purpose: db.purpose,
            user_id: db.user_id,
            created_by: db.created_by,
            created_at: db.created_at,
            last_used: db.last_used,
            model_access: db.model_access,
            requests_per_second: db.requests_per_second,
            burst_size: db.burst_size,
            spend_limit: db.spend_limit,
            spend_limit_interval: db.spend_limit_interval,
            // Checkpoint-derived display fields; populated by the handler via
            // ApiKeys::get_spend_states (see with_spend_state).
            spend: None,
            total_spend: None,
            resets_at: None,
        }
    }
}

impl ApiKeyResponse {
    /// Attach checkpoint-derived spend display fields.
    pub fn with_spend_state(mut self, state: Option<&crate::db::models::api_keys::ApiKeySpendState>) -> Self {
        if let Some(state) = state {
            self.spend = state.spend;
            self.total_spend = state.total_spend;
            self.resets_at = state.resets_at;
        }
        self
    }
}

impl From<ApiKeyDBResponse> for ApiKeyInfoResponse {
    fn from(db: ApiKeyDBResponse) -> Self {
        Self {
            id: db.id,
            name: db.name,
            description: db.description,
            purpose: db.purpose,
            user_id: db.user_id,
            created_by: db.created_by,
            created_at: db.created_at,
            last_used: db.last_used,
            model_access: db.model_access,
            requests_per_second: db.requests_per_second,
            burst_size: db.burst_size,
            spend_limit: db.spend_limit,
            spend_limit_interval: db.spend_limit_interval,
            // Checkpoint-derived display fields; populated by the handler via
            // ApiKeys::get_spend_states (see with_spend_state).
            spend: None,
            total_spend: None,
            resets_at: None,
        }
    }
}

impl ApiKeyInfoResponse {
    /// Attach checkpoint-derived spend display fields.
    pub fn with_spend_state(mut self, state: Option<&crate::db::models::api_keys::ApiKeySpendState>) -> Self {
        if let Some(state) = state {
            self.spend = state.spend;
            self.total_spend = state.total_spend;
            self.resets_at = state.resets_at;
        }
        self
    }
}
