//! Analytics request handler for AI proxy requests.
//!
//! This module provides [`AnalyticsHandler`], a standalone implementation of the [`outlet::RequestHandler`]
//! trait that handles analytics, billing (credit deduction), and Prometheus metrics recording.
//!
//! # Decoupling from Request Logging
//!
//! Previously, analytics was coupled to request logging via outlet-postgres. The analytics logic
//! lived inside a "serializer" callback, meaning if request logging was disabled, no analytics
//! would be recorded either.
//!
//! This handler can be used independently or composed with other handlers (like PostgresHandler
//! for request logging) using [`outlet::MultiHandler`].
//!
//! # What This Handler Does
//!
//! For each response:
//! 1. Parses the AI response to extract token usage
//! 2. Extracts authentication info from request headers
//! 3. Stores analytics to `http_analytics` table
//! 4. Deducts credits based on token usage and model pricing
//! 5. Records Prometheus metrics
//!
//! # Example
//!
//! ```ignore
//! use outlet::{MultiHandler, RequestLoggerConfig, RequestLoggerLayer};
//! use dwctl::request_logging::AnalyticsHandler;
//!
//! // Create standalone analytics handler
//! let analytics = AnalyticsHandler::new(pool, instance_id, config, metrics_recorder);
//!
//! // Use with MultiHandler for composition
//! let handler = MultiHandler::new()
//!     .with(postgres_handler)  // request logging
//!     .with(analytics);        // analytics/billing
//!
//! let layer = RequestLoggerLayer::new(outlet_config, handler);
//! ```

use crate::config::Config;
use crate::request_logging::AiResponse;
use crate::request_logging::serializers::{Auth, UsageMetrics, parse_ai_response, store_analytics_record};
use metrics::histogram;
use outlet::{RequestData, RequestHandler, ResponseData};
use serde_json::Value;
use sqlx::PgPool;
use tracing::{Instrument, error, info_span};
use uuid::Uuid;

/// A request handler that stores analytics data and records metrics.
///
/// This handler implements [`outlet::RequestHandler`] and can be used standalone or composed
/// with other handlers using [`outlet::MultiHandler`].
///
/// # Type Parameters
///
/// * `M` - The metrics recorder type, must implement [`crate::metrics::MetricsRecorder`]
pub struct AnalyticsHandler<M = crate::metrics::GenAiMetrics>
where
    M: crate::metrics::MetricsRecorder + Clone + 'static,
{
    pool: PgPool,
    instance_id: Uuid,
    config: Config,
    metrics_recorder: Option<M>,
}

impl<M> AnalyticsHandler<M>
where
    M: crate::metrics::MetricsRecorder + Clone + 'static,
{
    /// Creates a new analytics handler.
    ///
    /// # Arguments
    ///
    /// * `pool` - Database connection pool for storing analytics data
    /// * `instance_id` - Unique identifier for this service instance
    /// * `config` - Application configuration
    /// * `metrics_recorder` - Optional metrics recorder for Prometheus metrics
    pub fn new(pool: PgPool, instance_id: Uuid, config: Config, metrics_recorder: Option<M>) -> Self {
        Self {
            pool,
            instance_id,
            config,
            metrics_recorder,
        }
    }
}

impl<M> RequestHandler for AnalyticsHandler<M>
where
    M: crate::metrics::MetricsRecorder + Clone + Send + Sync + 'static,
{
    /// No-op for request phase - analytics only needs response data.
    async fn handle_request(&self, _data: RequestData) {
        // Analytics doesn't need the request phase
    }

    /// Handles analytics, billing, and metrics recording for a completed response.
    ///
    /// This method:
    /// 1. Parses the AI response to extract token usage
    /// 2. Extracts authentication info from request headers
    /// 3. Stores analytics to `http_analytics` table
    /// 4. Deducts credits based on token usage and model pricing
    /// 5. Records Prometheus metrics
    async fn handle_response(&self, request_data: RequestData, response_data: ResponseData) {
        let correlation_id = request_data.correlation_id;
        let span = info_span!(
            "analytics_handler",
            correlation_id = correlation_id,
            status = %response_data.status
        );

        async {
            // Try to parse the response - may fail for error responses (4xx, 5xx)
            let parse_result = parse_ai_response(&request_data, &response_data);

            // Use parsed response for metrics, or fallback to Other for error responses
            let metrics_response = match &parse_result {
                Ok(response) => response.clone(),
                Err(_) => AiResponse::Other(Value::Null),
            };

            // Extract basic metrics - captures status_code, duration, model from request, etc.
            let metrics = UsageMetrics::extract(self.instance_id, &request_data, &response_data, &metrics_response, &self.config);

            // Extract auth information from headers
            let auth = Auth::from_request(&request_data, &self.config);

            // Store to database - this enriches with user/pricing data and returns complete row
            let result = store_analytics_record(&self.pool, &metrics, &auth, &request_data).await;

            // Record analytics processing lag regardless of success/failure
            // This measures time from response completion to storage attempt completion
            let total_ms = chrono::Utc::now().signed_duration_since(metrics.timestamp).num_milliseconds();
            let lag_ms = total_ms - metrics.duration_ms;
            histogram!("dwctl_analytics_lag_seconds").record(lag_ms as f64 / 1000.0);

            match result {
                Ok(complete_row) => {
                    // Record metrics using the complete row (called AFTER database write)
                    if let Some(ref recorder) = self.metrics_recorder {
                        recorder.record_from_analytics(&complete_row).await;
                    }
                }
                Err(e) => {
                    error!(
                        correlation_id = metrics.correlation_id,
                        error = %e,
                        "Failed to store analytics data"
                    );
                }
            }
        }
        .instrument(span)
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Method, StatusCode, Uri};
    use std::collections::HashMap;
    use std::time::{Duration, SystemTime};

    fn create_test_request_data() -> RequestData {
        RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: Uri::from_static("/ai/v1/chat/completions"),
            headers: HashMap::new(),
            body: None,
        }
    }

    fn create_test_response_data() -> ResponseData {
        ResponseData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: None,
            duration_to_first_byte: Duration::from_millis(10),
            duration: Duration::from_millis(100),
        }
    }

    #[test]
    fn test_analytics_handler_creation() {
        // Just verify the type can be constructed with the expected parameters
        // Actual database tests would require integration test setup
        let _config = Config::default();
        // Handler would be created like:
        // let handler = AnalyticsHandler::new(pool, Uuid::new_v4(), config, None::<GenAiMetrics>);
    }

    #[test]
    fn test_request_data_creation() {
        let data = create_test_request_data();
        assert_eq!(data.correlation_id, 123);
        assert_eq!(data.method, Method::POST);
    }

    #[test]
    fn test_response_data_creation() {
        let data = create_test_response_data();
        assert_eq!(data.correlation_id, 123);
        assert_eq!(data.status, StatusCode::OK);
    }
}
