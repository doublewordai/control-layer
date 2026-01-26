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
//! # Architecture
//!
//! The handler does minimal work per-request - it extracts raw metrics and sends them to a
//! background batcher via a channel. The batcher handles:
//! - Batch enrichment (user lookup, model/tariff lookup)
//! - Transactional writes (analytics + credits in single transaction)
//!
//! This design keeps the hot path fast while ensuring data consistency.
//!
//! # Example
//!
//! ```ignore
//! use outlet::{MultiHandler, RequestLoggerConfig, RequestLoggerLayer};
//! use dwctl::request_logging::{AnalyticsHandler, AnalyticsBatcher};
//!
//! // Create batcher and get sender
//! let (batcher, sender) = AnalyticsBatcher::new(pool, config.analytics.clone());
//!
//! // Spawn batcher background task
//! tokio::spawn(batcher.run(cancellation_token));
//!
//! // Create handler with sender
//! let analytics = AnalyticsHandler::new(sender, instance_id, config, metrics_recorder);
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
use crate::request_logging::batcher::{AnalyticsSender, RawAnalyticsRecord};
use crate::request_logging::serializers::{Auth, UsageMetrics, parse_ai_response};
use crate::request_logging::utils::{extract_header_as_string, extract_header_as_uuid};
use outlet::{RequestData, RequestHandler, ResponseData};
use serde_json::Value;
use tracing::{Instrument, info_span, warn};
use uuid::Uuid;

/// A request handler that sends analytics data to a background batcher.
///
/// This handler implements [`outlet::RequestHandler`] and can be used standalone or composed
/// with other handlers using [`outlet::MultiHandler`].
///
/// The handler does minimal work per-request:
/// 1. Parses the AI response to extract token usage
/// 2. Extracts raw data from request headers (bearer token, fusillade metadata)
/// 3. Sends `RawAnalyticsRecord` to the batcher via channel
///
/// All database operations (enrichment, writes) happen in the background batcher.
pub struct AnalyticsHandler {
    sender: AnalyticsSender,
    instance_id: Uuid,
    config: Config,
}

impl AnalyticsHandler {
    /// Creates a new analytics handler.
    ///
    /// # Arguments
    ///
    /// * `sender` - Channel sender to the analytics batcher
    /// * `instance_id` - Unique identifier for this service instance
    /// * `config` - Application configuration
    pub fn new(sender: AnalyticsSender, instance_id: Uuid, config: Config) -> Self {
        Self {
            sender,
            instance_id,
            config,
        }
    }
}

impl RequestHandler for AnalyticsHandler {
    /// No-op for request phase - analytics only needs response data.
    async fn handle_request(&self, _data: RequestData) {
        // Analytics doesn't need the request phase
    }

    /// Extracts raw analytics data and sends to background batcher.
    ///
    /// This method does minimal work per-request:
    /// 1. Parses the AI response to extract token usage
    /// 2. Extracts raw data from headers (bearer token, fusillade metadata)
    /// 3. Sends `RawAnalyticsRecord` to batcher via channel
    ///
    /// All database work (enrichment, writes, credit deduction) happens in the batcher.
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

            // Extract fusillade batch metadata from headers
            let fusillade_batch_id = extract_header_as_uuid(&request_data, "x-fusillade-batch-id");
            let fusillade_request_id = extract_header_as_uuid(&request_data, "x-fusillade-request-id");
            let custom_id = extract_header_as_string(&request_data, "x-fusillade-custom-id");
            let batch_completion_window = extract_header_as_string(&request_data, "x-fusillade-batch-completion-window");
            let batch_request_source = extract_header_as_string(&request_data, "x-fusillade-batch-request-source").unwrap_or_default();

            // Extract batch creation timestamp for pricing lookup
            // This ensures batch requests are priced as of batch creation, not processing time
            let batch_created_at = extract_header_as_string(&request_data, "x-fusillade-batch-created-at")
                .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok());

            // Extract bearer token from auth
            let bearer_token = match &auth {
                Auth::ApiKey { bearer_token } => Some(bearer_token.clone()),
                Auth::None => None,
            };

            // Build the raw record (no DB enrichment)
            // Note: request_origin is computed in the batcher after api_key_purpose is resolved
            let record = RawAnalyticsRecord {
                instance_id: metrics.instance_id,
                correlation_id: metrics.correlation_id,
                timestamp: metrics.timestamp,
                method: metrics.method,
                uri: metrics.uri,
                request_model: metrics.request_model,
                response_model: metrics.response_model,
                status_code: metrics.status_code,
                duration_ms: metrics.duration_ms,
                duration_to_first_byte_ms: metrics.duration_to_first_byte_ms,
                prompt_tokens: metrics.prompt_tokens,
                completion_tokens: metrics.completion_tokens,
                total_tokens: metrics.total_tokens,
                response_type: metrics.response_type,
                server_address: metrics.server_address,
                server_port: metrics.server_port,
                bearer_token,
                fusillade_batch_id,
                fusillade_request_id,
                custom_id,
                batch_completion_window,
                batch_created_at,
                batch_request_source,
            };

            // Send to batcher (non-blocking, just puts in channel)
            if let Err(e) = self.sender.send(record).await {
                warn!(
                    correlation_id = correlation_id,
                    error = %e,
                    "Failed to send analytics record to batcher - channel may be full or closed"
                );
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
    use tokio::sync::mpsc;

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
        // Create a channel for testing
        let (tx, _rx) = mpsc::channel::<RawAnalyticsRecord>(100);
        let config = Config::default();

        // Verify the handler can be constructed
        let _handler = AnalyticsHandler::new(tx, Uuid::new_v4(), config);
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

    #[tokio::test]
    async fn test_handler_sends_to_channel() {
        let (tx, mut rx) = mpsc::channel::<RawAnalyticsRecord>(100);
        let config = Config::default();
        let handler = AnalyticsHandler::new(tx, Uuid::new_v4(), config);

        // Call handle_response
        let request_data = create_test_request_data();
        let response_data = create_test_response_data();
        handler.handle_response(request_data, response_data).await;

        // Verify record was sent to channel
        let record = rx.try_recv().expect("Should have received a record");
        assert_eq!(record.correlation_id, 123);
        assert_eq!(record.method, "POST");
        assert!(record.uri.contains("chat/completions"));
    }
}
