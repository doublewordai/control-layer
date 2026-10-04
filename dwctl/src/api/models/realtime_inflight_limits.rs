use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::db::handlers::realtime_inflight_limits::RealtimeInflightOverride;
use crate::types::{DeploymentId, UserId};

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RealtimeInflightLimitsResponse {
    #[schema(value_type = String, format = "uuid")]
    pub deployed_model_id: DeploymentId,
    pub default_limit: i32,
    pub overrides: Vec<RealtimeInflightOverrideResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RealtimeInflightOverrideResponse {
    #[schema(value_type = String, format = "uuid")]
    pub account_id: UserId,
    pub account_name: String,
    pub limit: Option<i32>,
    pub reason: String,
    #[schema(value_type = String, format = "uuid")]
    pub set_by: UserId,
    pub valid_from: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,
}

impl From<RealtimeInflightOverride> for RealtimeInflightOverrideResponse {
    fn from(row: RealtimeInflightOverride) -> Self {
        Self {
            account_id: row.account_id,
            account_name: row.account_name,
            limit: row.inflight_limit,
            reason: row.reason,
            set_by: row.set_by,
            valid_from: row.valid_from,
            valid_until: row.valid_until,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SetRealtimeInflightOverride {
    pub limit: i32,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ClearRealtimeInflightOverride {
    pub reason: String,
}
