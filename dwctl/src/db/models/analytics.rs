//! Database models for analytics queries.

use rust_decimal::Decimal;
use uuid::Uuid;

/// Batch metadata from http_analytics (shared between billing and analytics)
#[derive(Debug, Clone)]
pub struct BatchHttpAnalyticsMetadata {
    pub batch_id: Uuid,
    pub model: Option<String>,
    pub request_count: i64,
    pub total_prompt_tokens: Decimal,
    pub total_completion_tokens: Decimal,
    pub total_tokens: Decimal,
    pub avg_duration_ms: Option<Decimal>,
    pub avg_ttfb_ms: Option<Decimal>,
    pub calculated_cost: Option<Decimal>,
}
