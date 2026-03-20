//! Analytics request handler for AI proxy requests.
//!
//! This module provides [`AnalyticsHandler`], an implementation of the [`outlet::RequestHandler`]
//! trait that handles analytics, billing (credit deduction), and Prometheus metrics recording.
//!
//! # Architecture
//!
//! The handler overrides `handle_response_batch` to process outlet's batched responses
//! directly, with no intermediate channel or accumulation loop:
//!
//! ```text
//! outlet background task
//!   → accumulates responses into batch
//!   → calls handle_response_batch(&[(req, res)])
//!       → extracts RawAnalyticsRecords
//!       → calls AnalyticsWriter.flush()
//!           → batch enrichment (user lookup, pricing lookup)
//!           → transactional write (analytics + credits)
//!           → retry with exponential backoff
//!           → metrics recording
//! ```
//!
//! # Example
//!
//! ```ignore
//! use outlet::{MultiHandler, RequestLoggerConfig, RequestLoggerLayer};
//! use dwctl::request_logging::AnalyticsHandler;
//!
//! let analytics = AnalyticsHandler::new(pool, instance_id, config, metrics_recorder);
//!
//! let handler = MultiHandler::new()
//!     .with(postgres_handler)  // request logging
//!     .with(analytics);        // analytics/billing
//!
//! let layer = RequestLoggerLayer::new(outlet_config, handler);
//! ```

use crate::config::Config;
use crate::metrics::MetricsRecorder;
use crate::request_logging::AiResponse;
use crate::request_logging::batcher::{AnalyticsWriter, RawAnalyticsRecord};
use crate::request_logging::serializers::{Auth, UsageMetrics, parse_ai_response};
use crate::request_logging::utils::{extract_header_as_string, extract_header_as_uuid};
use outlet::{RequestData, RequestHandler, ResponseData};
use serde_json::Value;
use sqlx::PgPool;
use tracing::{Instrument, info_span};
use uuid::Uuid;

/// A request handler that processes analytics data via batched writes.
///
/// This handler implements [`outlet::RequestHandler`] and overrides `handle_response_batch`
/// to process outlet's batched responses directly. For each batch, it extracts raw metrics,
/// then delegates to [`AnalyticsWriter`] for enrichment and transactional database writes.
pub struct AnalyticsHandler<M = crate::metrics::GenAiMetrics>
where
    M: MetricsRecorder + Clone + Send + Sync + 'static,
{
    writer: AnalyticsWriter<M>,
    instance_id: Uuid,
    config: Config,
}

impl<M> AnalyticsHandler<M>
where
    M: MetricsRecorder + Clone + Send + Sync + 'static,
{
    /// Creates a new analytics handler.
    ///
    /// # Arguments
    ///
    /// * `pool` - Database connection pool for analytics writes
    /// * `instance_id` - Unique identifier for this service instance
    /// * `config` - Application configuration
    /// * `metrics_recorder` - Optional metrics recorder for Prometheus metrics
    pub fn new(pool: PgPool, instance_id: Uuid, config: Config, metrics_recorder: Option<M>) -> Self {
        let writer = AnalyticsWriter::new(pool, config.clone(), metrics_recorder);
        Self {
            writer,
            instance_id,
            config,
        }
    }

    /// Extract a [`RawAnalyticsRecord`] from a request/response pair.
    fn extract_record(&self, request_data: &RequestData, response_data: &ResponseData) -> RawAnalyticsRecord {
        // Try to parse the response - may fail for error responses (4xx, 5xx)
        let parse_result = parse_ai_response(request_data, response_data);

        // Use parsed response for metrics, or fallback to Other for error responses
        let metrics_response = match &parse_result {
            Ok(response) => response.clone(),
            Err(_) => AiResponse::Other(Value::Null),
        };

        // Extract basic metrics - captures status_code, duration, model from request, etc.
        let metrics = UsageMetrics::extract(self.instance_id, request_data, response_data, &metrics_response, &self.config);

        // Extract auth information from headers
        let auth = Auth::from_request(request_data, &self.config);

        // Extract fusillade batch metadata from headers
        let fusillade_batch_id = extract_header_as_uuid(request_data, "x-fusillade-batch-id");
        let fusillade_request_id = extract_header_as_uuid(request_data, "x-fusillade-request-id");
        let custom_id = extract_header_as_string(request_data, "x-fusillade-custom-id");
        let batch_completion_window = extract_header_as_string(request_data, "x-fusillade-batch-completion-window");
        let batch_request_source = extract_header_as_string(request_data, "x-fusillade-batch-request-source").unwrap_or_default();

        // Extract batch creation timestamp for pricing lookup
        let batch_created_at = extract_header_as_string(request_data, "x-fusillade-batch-created-at")
            .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok());

        // Extract bearer token from auth
        let bearer_token = match &auth {
            Auth::ApiKey { bearer_token } => Some(bearer_token.clone()),
            Auth::None => None,
        };

        RawAnalyticsRecord {
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
            trace_id: request_data.trace_id.clone(),
        }
    }
}

impl<M> RequestHandler for AnalyticsHandler<M>
where
    M: MetricsRecorder + Clone + Send + Sync + 'static,
{
    /// No-op for request phase - analytics only needs response data.
    async fn handle_request(&self, _data: RequestData) {}

    /// Extracts analytics data and flushes a single record.
    ///
    /// In practice, outlet 0.8+ always calls `handle_response_batch` instead.
    /// This method exists as a fallback for direct single-item calls.
    async fn handle_response(&self, request_data: RequestData, response_data: ResponseData) {
        let record = self.extract_record(&request_data, &response_data);
        self.writer.flush(&[record]).await;
    }

    /// Extracts analytics data from a batch of responses and flushes to the database.
    ///
    /// This is the primary entry point — outlet's background task dispatches accumulated
    /// responses as batches. For each pair, we extract a `RawAnalyticsRecord`, then
    /// delegate to the writer for batch enrichment and transactional writes.
    async fn handle_response_batch(&self, batch: &[(RequestData, ResponseData)]) {
        if batch.is_empty() {
            return;
        }

        let span = info_span!("dwctl.analytics_handler_batch", batch_size = batch.len(),);

        async {
            let records: Vec<RawAnalyticsRecord> = batch.iter().map(|(req, res)| self.extract_record(req, res)).collect();

            self.writer.flush(&records).await;
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
            trace_id: None,
            span_id: None,
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
