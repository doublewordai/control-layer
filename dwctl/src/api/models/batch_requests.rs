use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
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

    /// Sort active requests first (default: true)
    pub active_first: Option<bool>,
}

/// Individual batch request summary for list endpoint
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, FromRow)]
pub struct BatchRequestSummary {
    pub id: Uuid,
    pub batch_id: Uuid,
    pub model: String,
    #[sqlx(rename = "state")]
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<f64>,
    pub response_status: Option<i16>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub total_cost: Option<f64>,
    pub created_by_email: Option<String>,
}

/// Full batch request detail including input/output
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, FromRow)]
pub struct BatchRequestDetail {
    pub id: Uuid,
    pub batch_id: Uuid,
    pub model: String,
    #[sqlx(rename = "state")]
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<f64>,
    pub response_status: Option<i16>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub total_cost: Option<f64>,
    pub body: String,
    pub response_body: Option<String>,
    pub error: Option<String>,
    pub completion_window: String,
    pub batch_created_by: String,
}
