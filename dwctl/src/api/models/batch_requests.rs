use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::pagination::Pagination;

/// Query parameters for listing responses (batchless fusillade requests).
#[derive(Debug, Default, Deserialize, IntoParams, ToSchema)]
pub struct ListBatchRequestsQuery {
    #[serde(flatten)]
    #[param(inline)]
    pub pagination: Pagination,

    /// Filter by request state (pending, processing, completed, failed, canceled)
    pub status: Option<String>,

    /// Filter by model
    pub model: Option<String>,

    /// Filter by batch creator (user ID or org ID)
    pub member_id: Option<uuid::Uuid>,

    /// Filter to requests created after this timestamp
    pub created_after: Option<DateTime<Utc>>,

    /// Filter to requests created before this timestamp
    pub created_before: Option<DateTime<Utc>>,

    /// Filter by service tier(s), comma-separated (e.g., "flex,priority")
    pub service_tiers: Option<String>,

    /// Sort active requests first (default: true)
    pub active_first: Option<bool>,
}

/// Summary of a response (batchless fusillade request) for list views.
/// Combines fusillade request data with http_analytics enrichment.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResponseSummary {
    pub id: Uuid,
    pub batch_id: Option<Uuid>,
    pub model: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<f64>,
    pub response_status: Option<i16>,
    pub service_tier: Option<String>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub total_cost: Option<f64>,
    pub created_by_email: Option<String>,
}

/// Full response detail (batchless fusillade request) including input/output.
/// Combines fusillade request detail with http_analytics enrichment.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResponseDetail {
    pub id: Uuid,
    pub batch_id: Option<Uuid>,
    pub model: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<f64>,
    pub response_status: Option<i16>,
    pub service_tier: Option<String>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub total_cost: Option<f64>,
    pub body: String,
    pub response_body: Option<String>,
    pub error: Option<String>,
    /// Creator ID (user or org). Always non-empty: fusillade's
    /// `create_realtime` / `create_flex` coerce empty-string inputs to NULL,
    /// and the schema's `requests_attribution_xor` CHECK rejects NULL
    /// `created_by` for batchless rows. So a row reaching this struct came
    /// from a real attributed input.
    pub created_by: String,
    pub created_by_email: Option<String>,
}
