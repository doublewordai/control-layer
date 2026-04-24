use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::pagination::Pagination;

/// Query parameters for listing batch requests
#[derive(Debug, Default, Deserialize, IntoParams, ToSchema)]
pub struct ListBatchRequestsQuery {
    #[serde(flatten)]
    #[param(inline)]
    pub pagination: Pagination,

    /// Filter by batch completion window (e.g., "1h", "24h")
    pub completion_window: Option<String>,

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

    /// Filter by service tier (e.g., "auto", "default", "flex", "priority")
    pub service_tier: Option<String>,

    /// Only include requests with a non-NULL service_tier (excludes standard batch requests)
    pub require_service_tier: Option<bool>,

    /// Sort active requests first (default: true)
    pub active_first: Option<bool>,
}

/// Individual batch request summary for list endpoint.
/// Combines fusillade request data with http_analytics enrichment.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BatchRequestSummary {
    pub id: Uuid,
    pub batch_id: Uuid,
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

/// Full batch request detail including input/output.
/// Combines fusillade request detail with http_analytics enrichment.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BatchRequestDetail {
    pub id: Uuid,
    pub batch_id: Uuid,
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
    pub completion_window: Option<String>,
    pub batch_created_by: Option<String>,
    pub created_by_email: Option<String>,
}
