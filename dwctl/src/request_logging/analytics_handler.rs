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
use crate::metrics::errors::component::ANALYTICS;
use crate::request_logging::AiResponse;
use crate::request_logging::batcher::{AnalyticsSender, RawAnalyticsRecord};
use crate::request_logging::serializers::{Auth, UsageMetrics, parse_ai_response};
use crate::request_logging::utils::{extract_header_as_string, extract_header_as_uuid};
use axum::http::Uri;
use outlet::{RequestData, RequestHandler, ResponseData};
use serde_json::Value;
use tracing::{Instrument, info_span};
use uuid::Uuid;

/// ZDR-safe descriptor for a payload (de)serialization error.
///
/// Logs only the underlying JSON error's location and category, never its
/// `Display` message — serde messages can echo a fragment of the request or
/// response body (e.g. `invalid type: string "<content>"`), which must never
/// reach logs.
fn zdr_safe_parse_error(err: &(dyn std::error::Error + Send + Sync + 'static)) -> String {
    match err.downcast_ref::<serde_json::Error>() {
        Some(e) => format!("json parse error ({:?}) at line {} column {}", e.classify(), e.line(), e.column()),
        None => "parse error".to_string(),
    }
}

/// Header carrying the User-Agent of the call that CREATED a batch.
///
/// `create_batch` stores the submitter's User-Agent in the batch metadata as
/// `dw_user_agent`; fusillade replays every metadata key onto each dispatched
/// request as `x-fusillade-batch-<key>`.
const BATCH_CREATOR_USER_AGENT_HEADER: &str = "x-fusillade-batch-dw-user-agent";

/// The client this request should be attributed to.
///
/// Not always the User-Agent on the wire. A batch's requests are dispatched by the
/// fusillade daemon minutes-to-hours after the customer submitted the batch, over an
/// HTTP client that sends no User-Agent at all — which is why this column was empty on
/// 100% of batch rows. The creating call's User-Agent is carried across that gap in the
/// batch metadata, so batch traffic can be attributed to the client that asked for it
/// rather than to the daemon that ran it.
///
/// The batch header WINS where both exist. It is only ever present on a fusillade
/// dispatch, and on those the wire value is fusillade's own — an implementation detail —
/// while this one is the customer's. Ordering it this way also keeps the column correct
/// if fusillade ever starts identifying itself.
///
/// Truncated to 256 chars rather than in the DB: this lands on every row of a ~176M-row
/// table and a misbehaving client can send kilobytes. 256 covers every real SDK/CLI
/// string. Sliced on a char boundary, not a byte one, so a multi-byte UA can't be cut
/// mid-codepoint into invalid UTF-8.
fn resolve_user_agent(request_data: &RequestData) -> Option<String> {
    extract_header_as_string(request_data, BATCH_CREATOR_USER_AGENT_HEADER)
        .or_else(|| extract_header_as_string(request_data, "user-agent"))
        .map(|ua| ua.chars().take(256).collect())
}

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

    fn usage_required_endpoint(uri: &Uri) -> Option<&'static str> {
        match uri.path() {
            path if path.ends_with("/v1/chat/completions") || path.ends_with("/chat/completions") => Some("/v1/chat/completions"),
            path if path.ends_with("/v1/completions") || path.ends_with("/completions") => Some("/v1/completions"),
            path if path.ends_with("/v1/responses") || path.ends_with("/responses") => Some("/v1/responses"),
            _ => None,
        }
    }

    fn is_fusillade_stream(request_data: &RequestData) -> bool {
        request_data
            .headers
            .get("x-fusillade-stream")
            .and_then(|values| values.first())
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            == Some("true")
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
            "dwctl.analytics_handler",
            correlation_id = correlation_id,
            status = %response_data.status
        );

        async {
            let usage_required_endpoint = Self::usage_required_endpoint(&request_data.uri);
            let fusillade_stream = Self::is_fusillade_stream(&request_data);

            // Try to parse the response - may fail for error responses (4xx, 5xx)
            let parse_result = parse_ai_response(&request_data, &response_data);

            // Use parsed response for metrics, or fallback to Other for error responses
            let metrics_response = match &parse_result {
                Ok(response) => response.clone(),
                Err(e) => {
                    if response_data.status.is_success() {
                        tracing::warn!(
                            correlation_id = correlation_id,
                            uri = %request_data.uri,
                            error = %zdr_safe_parse_error(e.error.as_ref()),
                            "Failed to parse successful AI response — tokens will be zero"
                        );
                        if let Some(endpoint) = usage_required_endpoint {
                            crate::background_error!(
                                ANALYTICS, "parse_error", Error,
                                correlation_id = correlation_id,
                                uri = %request_data.uri,
                                endpoint,
                                fusillade_stream,
                                error = %zdr_safe_parse_error(e.error.as_ref()),
                                "Failed to parse usage from a successful generative response"
                            );
                        }
                    }
                    AiResponse::Other(Value::Null)
                }
            };

            // Extract basic metrics - captures status_code, duration, model from request, etc.
            let metrics = UsageMetrics::extract(self.instance_id, &request_data, &response_data, &metrics_response, &self.config);

            // Gate on the (possibly reclassified) status from metrics, not the raw upstream
            // status — streams that opened 200 but ended with an embedded error frame have
            // already been rewritten to 502 by UsageMetrics::extract and shouldn't trip this.
            if (200..300).contains(&metrics.status_code)
                && metrics.total_tokens == 0
                && let Some(endpoint) = usage_required_endpoint
            {
                crate::background_error!(
                    ANALYTICS, "missing_usage", Error,
                    correlation_id = correlation_id,
                    uri = %request_data.uri,
                    endpoint,
                    response_type = %metrics.response_type,
                    fusillade_stream,
                    request_model = ?metrics.request_model,
                    response_model = ?metrics.response_model,
                    "Successful generative response recorded zero tokens"
                );
            }

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
                reasoning_tokens: metrics.reasoning_tokens,
                total_tokens: metrics.total_tokens,
                cache_read_input_tokens: metrics.cache_read_input_tokens,
                cache_creation_5m_input_tokens: metrics.cache_creation_5m_input_tokens,
                cache_creation_1h_input_tokens: metrics.cache_creation_1h_input_tokens,
                cache_creation_24h_input_tokens: metrics.cache_creation_24h_input_tokens,
                response_type: metrics.response_type,
                finish_reason: metrics.finish_reason,
                // The client to attribute this request to — the batch's creator where there
                // is one, else whoever is on the wire. See `resolve_user_agent`.
                user_agent: resolve_user_agent(&request_data),
                server_address: metrics.server_address,
                server_port: metrics.server_port,
                served_by: metrics.served_by,
                bearer_token,
                fusillade_batch_id,
                fusillade_request_id,
                custom_id,
                batch_completion_window,
                batch_created_at,
                batch_request_source,
                trace_id: request_data.trace_id.clone(),
            };

            // Send to batcher (non-blocking, just puts in channel)
            if let Err(e) = self.sender.send(record).await {
                crate::background_error!(
                    ANALYTICS, "send_failed", Error,
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

    /// ZDR: the serialization-error descriptor used in the parse-failure
    /// logs must report only the JSON error's location/category, never a fragment
    /// of the offending body that serde's `Display` can echo.
    #[test]
    fn zdr_safe_parse_error_omits_body_fragment() {
        const SENTINEL: &str = "ZDR-SENTINEL-BODY-FRAGMENT-8d2c";
        // A type mismatch makes serde's Display echo the offending value
        // (`invalid type: string "<content>"`), which is exactly what we must drop.
        let err: serde_json::Error =
            serde_json::from_str::<std::collections::HashMap<String, u32>>(&format!("{{\"k\": \"{SENTINEL}\"}}")).unwrap_err();
        // Sanity: the raw serde message really does contain the sentinel.
        assert!(err.to_string().contains(SENTINEL));

        let boxed: Box<dyn std::error::Error + Send + Sync> = Box::new(err);
        let safe = zdr_safe_parse_error(boxed.as_ref());
        assert!(!safe.contains(SENTINEL), "body fragment leaked: {safe}");
        assert!(safe.contains("line") && safe.contains("column"), "expected location: {safe}");
    }

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
            extensions: Default::default(),
            correlation_id: 123,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: None,
            duration_to_first_byte: Duration::from_millis(10),
            duration: Duration::from_millis(100),
        }
    }

    fn request_data_with_headers(headers: &[(&str, &str)]) -> RequestData {
        RequestData {
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), vec![bytes::Bytes::from(value.to_string())]))
                .collect(),
            ..create_test_request_data()
        }
    }

    /// A batch's dispatched requests carry no wire User-Agent (fusillade's HTTP client
    /// sends none), so without the batch header this column is empty for all batch
    /// traffic — which is what it was before the creator's UA was carried across.
    #[test]
    fn batch_creator_user_agent_is_used_when_the_wire_carries_none() {
        let data = request_data_with_headers(&[(BATCH_CREATOR_USER_AGENT_HEADER, "OpenAI/Python 1.2.3")]);
        assert_eq!(resolve_user_agent(&data).as_deref(), Some("OpenAI/Python 1.2.3"));
    }

    /// The batch header only ever appears on a fusillade dispatch, where the wire value is
    /// fusillade's own. The customer's client is the useful answer, so it wins.
    #[test]
    fn batch_creator_user_agent_beats_the_dispatching_daemons() {
        let data = request_data_with_headers(&[
            (BATCH_CREATOR_USER_AGENT_HEADER, "claude-cli/1.0.0"),
            ("user-agent", "fusillade/22.0.1"),
        ]);
        assert_eq!(resolve_user_agent(&data).as_deref(), Some("claude-cli/1.0.0"));
    }

    /// Live traffic is untouched by any of this.
    #[test]
    fn live_traffic_still_reports_the_wire_user_agent() {
        let data = request_data_with_headers(&[("user-agent", "curl/8.4.0")]);
        assert_eq!(resolve_user_agent(&data).as_deref(), Some("curl/8.4.0"));
        assert_eq!(resolve_user_agent(&create_test_request_data()), None);
    }

    /// 256 CHARS, not bytes: slicing a multi-byte UA on a byte boundary would produce
    /// invalid UTF-8 and fail the insert for every row in the batch it lands in.
    #[test]
    fn a_pathological_user_agent_is_truncated_on_a_char_boundary() {
        let long = "é".repeat(400);
        let data = request_data_with_headers(&[(BATCH_CREATOR_USER_AGENT_HEADER, &long)]);
        let resolved = resolve_user_agent(&data).expect("a user agent");
        assert_eq!(resolved.chars().count(), 256);
        assert_eq!(resolved.len(), 512, "expected 2 bytes per char, i.e. no mid-codepoint cut");
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

    #[test]
    fn test_usage_required_endpoint_matches_proxied_and_v1_paths() {
        assert_eq!(
            AnalyticsHandler::usage_required_endpoint(&Uri::from_static("/v1/chat/completions")),
            Some("/v1/chat/completions")
        );
        assert_eq!(
            AnalyticsHandler::usage_required_endpoint(&Uri::from_static("/chat/completions")),
            Some("/v1/chat/completions")
        );
        assert_eq!(
            AnalyticsHandler::usage_required_endpoint(&Uri::from_static("/v1/completions")),
            Some("/v1/completions")
        );
        assert_eq!(
            AnalyticsHandler::usage_required_endpoint(&Uri::from_static("/completions")),
            Some("/v1/completions")
        );
        assert_eq!(
            AnalyticsHandler::usage_required_endpoint(&Uri::from_static("/v1/responses")),
            Some("/v1/responses")
        );
        assert_eq!(
            AnalyticsHandler::usage_required_endpoint(&Uri::from_static("/responses")),
            Some("/v1/responses")
        );
        assert_eq!(AnalyticsHandler::usage_required_endpoint(&Uri::from_static("/embeddings")), None);
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

    /// `resolve_user_agent` being correct is not the same as it being CALLED. This drives
    /// the handler the way outlet does — a dispatched batch request, carrying the creator's
    /// User-Agent as a header and nothing on the wire — and asserts the value lands on the
    /// record that goes to the batcher.
    #[tokio::test]
    async fn a_dispatched_batch_request_reports_the_batchs_creator_as_its_client() {
        let (tx, mut rx) = mpsc::channel::<RawAnalyticsRecord>(100);
        let handler = AnalyticsHandler::new(tx, Uuid::new_v4(), Config::default());

        handler
            .handle_response(
                request_data_with_headers(&[(BATCH_CREATOR_USER_AGENT_HEADER, "OpenAI/Python 1.2.3")]),
                create_test_response_data(),
            )
            .await;

        let record = rx.try_recv().expect("Should have received a record");
        assert_eq!(
            record.user_agent.as_deref(),
            Some("OpenAI/Python 1.2.3"),
            "the batch creator's client must reach the analytics record"
        );
    }
}
