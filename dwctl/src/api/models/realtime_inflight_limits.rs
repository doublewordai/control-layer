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
    pub limit: i32,
}

impl From<RealtimeInflightOverride> for RealtimeInflightOverrideResponse {
    fn from(row: RealtimeInflightOverride) -> Self {
        Self {
            account_id: row.account_id,
            account_name: row.account_name,
            limit: row.inflight_limit,
        }
    }
}
