//! Metrics recorder that extracts attributes and records GenAI metrics

use crate::request_logging::serializers::HttpAnalyticsRow;

use async_trait::async_trait;

/// Trait for recording GenAI metrics from analytics data
#[async_trait]
pub trait MetricsRecorder: Send + Sync {
    /// Record metrics from a complete http_analytics table row
    async fn record_from_analytics(&self, row: &HttpAnalyticsRow);
}
