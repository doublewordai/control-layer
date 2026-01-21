//! GenAI metrics implementation following OpenTelemetry Semantic Conventions
//!
//! Implements the four key metrics for GenAI observability:
//! - gen_ai.server.request.duration
//! - gen_ai.server.time_to_first_token
//! - gen_ai.server.time_per_output_token
//! - gen_ai.client.token.usage

use async_trait::async_trait;
use prometheus::{HistogramOpts, HistogramVec, Registry};
use tracing::instrument;

use crate::{metrics::MetricsRecorder, request_logging::serializers::HttpAnalyticsRow};

/// GenAI metrics instruments using Prometheus
#[derive(Clone)]
pub struct GenAiMetrics {
    /// Total request duration (required)
    request_duration: HistogramVec,
    /// Time until first byte received (recommended, streaming only)
    time_to_first_token: HistogramVec,
    /// Average time per output token during decode (recommended)
    time_per_output_token: HistogramVec,
    /// Token usage - input and output (recommended)
    token_usage: HistogramVec,
    /// Reference to the Prometheus registry
    registry: Registry,
}

impl GenAiMetrics {
    /// Create new GenAI metrics instruments and register with Prometheus
    pub fn new(registry: &Registry) -> Result<Self, prometheus::Error> {
        // Request duration histogram (required)
        // Buckets from OTel spec: 0.01s to 81.92s (exponential with factor 2)
        let duration_buckets = vec![
            0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92,
        ];
        let request_duration = HistogramVec::new(
            HistogramOpts::new("gen_ai_server_request_duration_seconds", "GenAI operation duration").buckets(duration_buckets),
            &[
                "gen_ai_operation_name",
                "gen_ai_provider_name",
                "gen_ai_request_model",
                "gen_ai_response_model",
                "server_address",
                "server_port",
                "error_type",
                "request_origin",
                "batch_sla",
            ],
        )?;
        registry.register(Box::new(request_duration.clone()))?;

        // Time to first token histogram (recommended)
        // Buckets from OTel spec: 0.001s to 10.0s
        let ttft_buckets = vec![
            0.001, 0.005, 0.01, 0.02, 0.04, 0.06, 0.08, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
        ];
        let time_to_first_token = HistogramVec::new(
            HistogramOpts::new(
                "gen_ai_server_time_to_first_token_seconds",
                "Time to generate first token for successful responses",
            )
            .buckets(ttft_buckets),
            &[
                "gen_ai_operation_name",
                "gen_ai_provider_name",
                "gen_ai_request_model",
                "gen_ai_response_model",
                "server_address",
                "server_port",
                "request_origin",
                "batch_sla",
            ],
        )?;
        registry.register(Box::new(time_to_first_token.clone()))?;

        // Time per output token histogram (recommended)
        // Buckets from OTel spec: 0.01s to 2.5s (exponential with factor 2)
        let tpot_buckets = vec![0.01, 0.025, 0.05, 0.075, 0.1, 0.15, 0.2, 0.3, 0.4, 0.5, 0.75, 1.0, 2.5];
        let time_per_output_token = HistogramVec::new(
            HistogramOpts::new(
                "gen_ai_server_time_per_output_token_seconds",
                "Time per output token generated after the first token",
            )
            .buckets(tpot_buckets),
            &[
                "gen_ai_operation_name",
                "gen_ai_provider_name",
                "gen_ai_request_model",
                "gen_ai_response_model",
                "server_address",
                "server_port",
                "request_origin",
                "batch_sla",
            ],
        )?;
        registry.register(Box::new(time_per_output_token.clone()))?;

        // Token usage histogram (recommended)
        // Buckets from OTel spec: 1 to 67108864 tokens (exponential with factor 4)
        let token_buckets = vec![
            1.0, 4.0, 16.0, 64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0, 4194304.0, 16777216.0, 67108864.0,
        ];
        let token_usage = HistogramVec::new(
            HistogramOpts::new("gen_ai_client_token_usage", "Number of tokens used in prompt and completion").buckets(token_buckets),
            &[
                "gen_ai_operation_name",
                "gen_ai_provider_name",
                "gen_ai_request_model",
                "gen_ai_response_model",
                "gen_ai_token_type",
                "server_address",
                "server_port",
                "request_origin",
                "batch_sla",
            ],
        )?;
        registry.register(Box::new(token_usage.clone()))?;

        Ok(Self {
            request_duration,
            time_to_first_token,
            time_per_output_token,
            token_usage,
            registry: registry.clone(),
        })
    }

    /// Get reference to the Prometheus registry
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Record request duration metric
    pub fn record_request_duration(&self, duration_seconds: f64, labels: &[&str]) {
        self.request_duration.with_label_values(labels).observe(duration_seconds);
    }

    /// Record time to first token (only for streaming requests)
    pub fn record_time_to_first_token(&self, ttfb_seconds: f64, labels: &[&str]) {
        self.time_to_first_token.with_label_values(labels).observe(ttfb_seconds);
    }

    /// Record time per output token (only when output tokens > 0)
    pub fn record_time_per_output_token(&self, time_per_token_seconds: f64, labels: &[&str]) {
        self.time_per_output_token.with_label_values(labels).observe(time_per_token_seconds);
    }

    /// Record token usage (called twice per request: once for input, once for output)
    pub fn record_token_usage(&self, token_count: f64, labels: &[&str]) {
        self.token_usage.with_label_values(labels).observe(token_count);
    }
}

#[async_trait]
impl MetricsRecorder for GenAiMetrics {
    #[instrument(skip_all)]
    async fn record_from_analytics(&self, row: &HttpAnalyticsRow) {
        // Extract operation from response_type
        let operation = match row.response_type.as_str() {
            "chat_completion" | "chat_completion_stream" => "chat",
            "completion" => "text_completion",
            "embeddings" | "base64_embeddings" => "embeddings",
            _ => "",
        };

        // Determine if this is a streaming request from response_type
        let is_streaming = row.response_type.ends_with("_stream");

        // Error type for failed requests
        let error_type = if row.status_code >= 400 {
            format!("{}", row.status_code)
        } else {
            String::new()
        };

        // Build labels from config
        let server_address = &row.server_address;
        let server_port = &row.server_port.to_string();

        let provider_name = row.provider_name.as_deref().unwrap_or("");
        let request_model = row.request_model.as_deref().unwrap_or("");
        let response_model = row.response_model.as_deref().unwrap_or("");
        let request_origin = &row.request_origin;
        let batch_sla = &row.batch_sla;

        // Record request duration (always)
        let duration_labels = vec![
            operation,
            provider_name,
            request_model,
            response_model,
            server_address,
            server_port,
            &error_type,
            request_origin,
            batch_sla,
        ];
        self.record_request_duration(row.duration_ms as f64 / 1000.0, &duration_labels);

        // Record time to first token (only for streaming)
        if is_streaming && let Some(ttfb_ms) = row.duration_to_first_byte_ms {
            let ttft_labels = vec![
                operation,
                provider_name,
                request_model,
                response_model,
                server_address,
                server_port,
                request_origin,
                batch_sla,
            ];
            self.record_time_to_first_token(ttfb_ms as f64 / 1000.0, &ttft_labels);
        }

        // Record time per output token (only if we have completion tokens and ttfb)
        if row.completion_tokens > 0
            && let Some(ttfb_ms) = row.duration_to_first_byte_ms
        {
            let time_after_first_token = (row.duration_ms - ttfb_ms) as f64 / 1000.0;
            let time_per_token = time_after_first_token / row.completion_tokens as f64;
            let tpot_labels = vec![
                operation,
                provider_name,
                request_model,
                response_model,
                server_address,
                server_port,
                request_origin,
                batch_sla,
            ];
            self.record_time_per_output_token(time_per_token, &tpot_labels);
        }

        // Record token usage (input tokens)
        if row.prompt_tokens > 0 {
            let input_labels = vec![
                operation,
                provider_name,
                request_model,
                response_model,
                "input",
                server_address,
                server_port,
                request_origin,
                batch_sla,
            ];
            self.record_token_usage(row.prompt_tokens as f64, &input_labels);
        }

        // Record token usage (output tokens)
        if row.completion_tokens > 0 {
            let output_labels = vec![
                operation,
                provider_name,
                request_model,
                response_model,
                "output",
                server_address,
                server_port,
                request_origin,
                batch_sla,
            ];
            self.record_token_usage(row.completion_tokens as f64, &output_labels);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request_logging::serializers::HttpAnalyticsRow;
    use uuid::Uuid;

    /// Helper to find a label value in a Prometheus metric
    fn find_label(labels: &[prometheus::proto::LabelPair], name: &str) -> Option<String> {
        labels.iter().find(|l| l.name() == name).map(|l| l.value().to_string())
    }

    #[tokio::test]
    async fn test_record_streaming_chat_completion() {
        // Create isolated registry for this test
        let registry = Registry::new();
        let metrics = GenAiMetrics::new(&registry).expect("Failed to create metrics");

        // Create test fixture for a streaming chat completion
        let row = HttpAnalyticsRow {
            instance_id: Uuid::new_v4(),
            correlation_id: 123,
            timestamp: chrono::Utc::now(),
            method: "POST".to_string(),
            uri: "/v1/chat/completions".to_string(),
            request_model: Some("gpt-4".to_string()),
            response_model: Some("gpt-4-0613".to_string()),
            status_code: 200,
            duration_ms: 1500,
            duration_to_first_byte_ms: Some(200),
            prompt_tokens: 10,
            completion_tokens: 50,
            total_tokens: 60,
            response_type: "chat_completion_stream".to_string(),
            user_id: None,
            user_email: None,
            access_source: "api_key".to_string(),
            input_price_per_token: None,
            output_price_per_token: None,
            server_address: "api.openai.com".to_string(),
            server_port: 443,
            provider_name: Some("openai".to_string()),
            fusillade_batch_id: None,
            fusillade_request_id: None,
            custom_id: None,
            request_origin: "api".to_string(),
            batch_sla: String::new(),
            batch_metadata_request_origin: String::new(),
        };

        // Call the function under test
        metrics.record_from_analytics(&row).await;

        // Gather metrics from registry
        let metric_families = registry.gather();

        // Verify request duration metric
        let duration_metric = metric_families
            .iter()
            .find(|m| m.name() == "gen_ai_server_request_duration_seconds")
            .expect("Should have request duration metric");

        let duration_histogram = duration_metric.get_metric().first().unwrap().get_histogram();
        assert_eq!(duration_histogram.get_sample_count(), 1, "Should record one duration sample");
        assert_eq!(duration_histogram.get_sample_sum(), 1.5, "Duration should be 1.5 seconds");

        let duration_labels = duration_metric.get_metric().first().unwrap().get_label();
        assert_eq!(find_label(duration_labels, "gen_ai_operation_name"), Some("chat".to_string()));
        assert_eq!(find_label(duration_labels, "gen_ai_provider_name"), Some("openai".to_string()));
        assert_eq!(find_label(duration_labels, "gen_ai_request_model"), Some("gpt-4".to_string()));
        assert_eq!(find_label(duration_labels, "gen_ai_response_model"), Some("gpt-4-0613".to_string()));
        assert_eq!(find_label(duration_labels, "server_address"), Some("api.openai.com".to_string()));
        assert_eq!(find_label(duration_labels, "server_port"), Some("443".to_string()));
        assert_eq!(find_label(duration_labels, "error_type"), Some("".to_string()));

        // Verify time to first token metric (only for streaming)
        let ttft_metric = metric_families
            .iter()
            .find(|m| m.name() == "gen_ai_server_time_to_first_token_seconds")
            .expect("Should have time to first token metric");

        let ttft_histogram = ttft_metric.get_metric().first().unwrap().get_histogram();
        assert_eq!(ttft_histogram.get_sample_count(), 1, "Should record one TTFT sample");
        assert_eq!(ttft_histogram.get_sample_sum(), 0.2, "TTFT should be 0.2 seconds");

        // Verify time per output token metric
        let tpot_metric = metric_families
            .iter()
            .find(|m| m.name() == "gen_ai_server_time_per_output_token_seconds")
            .expect("Should have time per output token metric");

        let tpot_histogram = tpot_metric.get_metric().first().unwrap().get_histogram();
        assert_eq!(tpot_histogram.get_sample_count(), 1, "Should record one TPOT sample");
        // Expected: (1500ms - 200ms) / 50 tokens = 1300ms / 50 = 26ms = 0.026s
        assert!(
            (tpot_histogram.get_sample_sum() - 0.026).abs() < 0.0001,
            "TPOT should be approximately 0.026 seconds, got {}",
            tpot_histogram.get_sample_sum()
        );

        // Verify token usage metrics (should have 2: input and output)
        let token_metric = metric_families
            .iter()
            .find(|m| m.name() == "gen_ai_client_token_usage")
            .expect("Should have token usage metric");

        assert_eq!(token_metric.get_metric().len(), 2, "Should have 2 token metrics (input + output)");

        // Find input token metric
        let input_token = token_metric
            .get_metric()
            .iter()
            .find(|m| find_label(m.get_label(), "gen_ai_token_type") == Some("input".to_string()))
            .expect("Should have input token metric");
        assert_eq!(input_token.get_histogram().get_sample_sum(), 10.0, "Should record 10 input tokens");

        // Find output token metric
        let output_token = token_metric
            .get_metric()
            .iter()
            .find(|m| find_label(m.get_label(), "gen_ai_token_type") == Some("output".to_string()))
            .expect("Should have output token metric");
        assert_eq!(
            output_token.get_histogram().get_sample_sum(),
            50.0,
            "Should record 50 output tokens"
        );
    }

    #[tokio::test]
    async fn test_record_non_streaming_chat_completion() {
        let registry = Registry::new();
        let metrics = GenAiMetrics::new(&registry).expect("Failed to create metrics");

        let row = HttpAnalyticsRow {
            instance_id: Uuid::new_v4(),
            correlation_id: 456,
            timestamp: chrono::Utc::now(),
            method: "POST".to_string(),
            uri: "/v1/chat/completions".to_string(),
            request_model: Some("claude-3-sonnet".to_string()),
            response_model: Some("claude-3-sonnet-20240229".to_string()),
            status_code: 200,
            duration_ms: 2000,
            duration_to_first_byte_ms: Some(1800), // Non-streaming still has TTFB but close to total
            prompt_tokens: 20,
            completion_tokens: 100,
            total_tokens: 120,
            response_type: "chat_completion".to_string(), // NOT streaming
            user_id: None,
            user_email: None,
            access_source: "api_key".to_string(),
            input_price_per_token: None,
            output_price_per_token: None,
            server_address: "api.anthropic.com".to_string(),
            server_port: 443,
            provider_name: Some("anthropic".to_string()),
            fusillade_batch_id: None,
            fusillade_request_id: None,
            custom_id: None,
            request_origin: "api".to_string(),
            batch_sla: String::new(),
            batch_metadata_request_origin: String::new(),
        };

        metrics.record_from_analytics(&row).await;
        let metric_families = registry.gather();

        // Should have request duration
        let duration_metric = metric_families
            .iter()
            .find(|m| m.name() == "gen_ai_server_request_duration_seconds")
            .expect("Should have request duration metric");
        assert_eq!(duration_metric.get_metric().first().unwrap().get_histogram().get_sample_count(), 1);

        // Should NOT have time to first token (not streaming)
        let ttft_metric = metric_families
            .iter()
            .find(|m| m.name() == "gen_ai_server_time_to_first_token_seconds");
        assert!(
            ttft_metric.is_none() || ttft_metric.unwrap().get_metric().is_empty(),
            "Non-streaming should not record TTFT"
        );

        // Should have time per output token (has completion tokens)
        let tpot_metric = metric_families
            .iter()
            .find(|m| m.name() == "gen_ai_server_time_per_output_token_seconds")
            .expect("Should have TPOT metric");
        assert_eq!(tpot_metric.get_metric().first().unwrap().get_histogram().get_sample_count(), 1);
    }

    #[tokio::test]
    async fn test_record_embeddings() {
        let registry = Registry::new();
        let metrics = GenAiMetrics::new(&registry).expect("Failed to create metrics");

        let row = HttpAnalyticsRow {
            instance_id: Uuid::new_v4(),
            correlation_id: 789,
            timestamp: chrono::Utc::now(),
            method: "POST".to_string(),
            uri: "/v1/embeddings".to_string(),
            request_model: Some("text-embedding-ada-002".to_string()),
            response_model: Some("text-embedding-ada-002".to_string()),
            status_code: 200,
            duration_ms: 500,
            duration_to_first_byte_ms: Some(450),
            prompt_tokens: 100,
            completion_tokens: 0, // Embeddings don't have completion tokens
            total_tokens: 100,
            response_type: "embeddings".to_string(),
            user_id: None,
            user_email: None,
            access_source: "api_key".to_string(),
            input_price_per_token: None,
            output_price_per_token: None,
            server_address: "api.openai.com".to_string(),
            server_port: 443,
            provider_name: Some("openai".to_string()),
            fusillade_batch_id: None,
            fusillade_request_id: None,
            custom_id: None,
            request_origin: "api".to_string(),
            batch_sla: String::new(),
            batch_metadata_request_origin: String::new(),
        };

        metrics.record_from_analytics(&row).await;
        let metric_families = registry.gather();

        // Verify operation name is "embeddings"
        let duration_metric = metric_families
            .iter()
            .find(|m| m.name() == "gen_ai_server_request_duration_seconds")
            .expect("Should have request duration metric");

        let duration_labels = duration_metric.get_metric().first().unwrap().get_label();
        assert_eq!(
            find_label(duration_labels, "gen_ai_operation_name"),
            Some("embeddings".to_string()),
            "Operation should be embeddings"
        );

        // Should only have input tokens, not output
        let token_metric = metric_families
            .iter()
            .find(|m| m.name() == "gen_ai_client_token_usage")
            .expect("Should have token usage metric");

        assert_eq!(token_metric.get_metric().len(), 1, "Should only have 1 token metric (input only)");

        let input_token = token_metric.get_metric().first().unwrap();
        assert_eq!(find_label(input_token.get_label(), "gen_ai_token_type"), Some("input".to_string()));
        assert_eq!(input_token.get_histogram().get_sample_sum(), 100.0);

        // Should NOT have time per output token (no completion tokens)
        let tpot_metric = metric_families
            .iter()
            .find(|m| m.name() == "gen_ai_server_time_per_output_token_seconds");
        assert!(
            tpot_metric.is_none() || tpot_metric.unwrap().get_metric().is_empty(),
            "Embeddings should not record TPOT"
        );
    }

    #[tokio::test]
    async fn test_record_failed_request() {
        let registry = Registry::new();
        let metrics = GenAiMetrics::new(&registry).expect("Failed to create metrics");

        let row = HttpAnalyticsRow {
            instance_id: Uuid::new_v4(),
            correlation_id: 999,
            timestamp: chrono::Utc::now(),
            method: "POST".to_string(),
            uri: "/v1/chat/completions".to_string(),
            request_model: Some("gpt-4".to_string()),
            response_model: None, // May not have response model on error
            status_code: 429,     // Rate limit error
            duration_ms: 100,
            duration_to_first_byte_ms: Some(100),
            prompt_tokens: 0, // No tokens on error
            completion_tokens: 0,
            total_tokens: 0,
            response_type: "chat_completion".to_string(),
            user_id: None,
            user_email: None,
            access_source: "api_key".to_string(),
            input_price_per_token: None,
            output_price_per_token: None,
            server_address: "api.openai.com".to_string(),
            server_port: 443,
            provider_name: Some("openai".to_string()),
            fusillade_batch_id: None,
            fusillade_request_id: None,
            custom_id: None,
            request_origin: "api".to_string(),
            batch_sla: String::new(),
            batch_metadata_request_origin: String::new(),
        };

        metrics.record_from_analytics(&row).await;
        let metric_families = registry.gather();

        // Should record request duration with error_type
        let duration_metric = metric_families
            .iter()
            .find(|m| m.name() == "gen_ai_server_request_duration_seconds")
            .expect("Should have request duration metric");

        let duration_labels = duration_metric.get_metric().first().unwrap().get_label();
        assert_eq!(
            find_label(duration_labels, "error_type"),
            Some("429".to_string()),
            "Should record error type for failed requests"
        );

        // Should NOT record token usage (no tokens)
        let token_metric = metric_families.iter().find(|m| m.name() == "gen_ai_client_token_usage");
        assert!(
            token_metric.is_none() || token_metric.unwrap().get_metric().is_empty(),
            "Should not record tokens on error"
        );
    }

    #[tokio::test]
    async fn test_record_with_missing_optional_fields() {
        let registry = Registry::new();
        let metrics = GenAiMetrics::new(&registry).expect("Failed to create metrics");

        let row = HttpAnalyticsRow {
            instance_id: Uuid::new_v4(),
            correlation_id: 111,
            timestamp: chrono::Utc::now(),
            method: "POST".to_string(),
            uri: "/v1/completions".to_string(),
            request_model: None, // Missing model
            response_model: None,
            status_code: 200,
            duration_ms: 1000,
            duration_to_first_byte_ms: None, // Missing TTFB
            prompt_tokens: 10,
            completion_tokens: 20,
            total_tokens: 30,
            response_type: "completion".to_string(),
            user_id: None,
            user_email: None,
            access_source: "api_key".to_string(),
            input_price_per_token: None,
            output_price_per_token: None,
            server_address: "localhost".to_string(),
            server_port: 8080,
            provider_name: None, // Missing provider
            fusillade_batch_id: None,
            fusillade_request_id: None,
            custom_id: None,
            request_origin: "api".to_string(),
            batch_sla: String::new(),
            batch_metadata_request_origin: String::new(),
        };

        metrics.record_from_analytics(&row).await;
        let metric_families = registry.gather();

        // Should still record metrics with empty strings for missing fields
        let duration_metric = metric_families
            .iter()
            .find(|m| m.name() == "gen_ai_server_request_duration_seconds")
            .expect("Should have request duration metric");

        let duration_labels = duration_metric.get_metric().first().unwrap().get_label();
        assert_eq!(
            find_label(duration_labels, "gen_ai_operation_name"),
            Some("text_completion".to_string())
        );
        assert_eq!(find_label(duration_labels, "gen_ai_provider_name"), Some("".to_string()));
        assert_eq!(find_label(duration_labels, "gen_ai_request_model"), Some("".to_string()));

        // Should NOT record TPOT without duration_to_first_byte_ms
        let tpot_metric = metric_families
            .iter()
            .find(|m| m.name() == "gen_ai_server_time_per_output_token_seconds");
        assert!(
            tpot_metric.is_none() || tpot_metric.unwrap().get_metric().is_empty(),
            "Should not record TPOT without TTFB"
        );
    }

    #[tokio::test]
    async fn test_record_zero_completion_tokens() {
        let registry = Registry::new();
        let metrics = GenAiMetrics::new(&registry).expect("Failed to create metrics");

        let row = HttpAnalyticsRow {
            instance_id: Uuid::new_v4(),
            correlation_id: 222,
            timestamp: chrono::Utc::now(),
            method: "POST".to_string(),
            uri: "/v1/chat/completions".to_string(),
            request_model: Some("gpt-4".to_string()),
            response_model: Some("gpt-4".to_string()),
            status_code: 200,
            duration_ms: 500,
            duration_to_first_byte_ms: Some(400),
            prompt_tokens: 50,
            completion_tokens: 0, // No output tokens
            total_tokens: 50,
            response_type: "chat_completion".to_string(),
            user_id: None,
            user_email: None,
            access_source: "api_key".to_string(),
            input_price_per_token: None,
            output_price_per_token: None,
            server_address: "api.openai.com".to_string(),
            server_port: 443,
            provider_name: Some("openai".to_string()),
            fusillade_batch_id: None,
            fusillade_request_id: None,
            custom_id: None,
            request_origin: "api".to_string(),
            batch_sla: String::new(),
            batch_metadata_request_origin: String::new(),
        };

        metrics.record_from_analytics(&row).await;
        let metric_families = registry.gather();

        // Should only record input tokens
        let token_metric = metric_families
            .iter()
            .find(|m| m.name() == "gen_ai_client_token_usage")
            .expect("Should have token usage metric");

        assert_eq!(token_metric.get_metric().len(), 1, "Should only have input tokens");

        let input_token = token_metric.get_metric().first().unwrap();
        assert_eq!(find_label(input_token.get_label(), "gen_ai_token_type"), Some("input".to_string()));

        // Should NOT record TPOT with zero completion tokens
        let tpot_metric = metric_families
            .iter()
            .find(|m| m.name() == "gen_ai_server_time_per_output_token_seconds");
        assert!(
            tpot_metric.is_none() || tpot_metric.unwrap().get_metric().is_empty(),
            "Should not record TPOT with zero completion tokens"
        );
    }

    #[tokio::test]
    async fn test_record_unknown_response_type() {
        let registry = Registry::new();
        let metrics = GenAiMetrics::new(&registry).expect("Failed to create metrics");

        let row = HttpAnalyticsRow {
            instance_id: Uuid::new_v4(),
            correlation_id: 333,
            timestamp: chrono::Utc::now(),
            method: "POST".to_string(),
            uri: "/v1/unknown".to_string(),
            request_model: Some("unknown-model".to_string()),
            response_model: Some("unknown-model".to_string()),
            status_code: 200,
            duration_ms: 100,
            duration_to_first_byte_ms: Some(50),
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            response_type: "other".to_string(),
            user_id: None,
            user_email: None,
            access_source: "api_key".to_string(),
            input_price_per_token: None,
            output_price_per_token: None,
            server_address: "api.example.com".to_string(),
            server_port: 443,
            provider_name: Some("custom".to_string()),
            fusillade_batch_id: None,
            fusillade_request_id: None,
            custom_id: None,
            request_origin: "api".to_string(),
            batch_sla: String::new(),
            batch_metadata_request_origin: String::new(),
        };

        metrics.record_from_analytics(&row).await;
        let metric_families = registry.gather();

        // Should record with empty operation name
        let duration_metric = metric_families
            .iter()
            .find(|m| m.name() == "gen_ai_server_request_duration_seconds")
            .expect("Should have request duration metric");

        let duration_labels = duration_metric.get_metric().first().unwrap().get_label();
        assert_eq!(
            find_label(duration_labels, "gen_ai_operation_name"),
            Some("".to_string()),
            "Unknown response type should have empty operation"
        );
    }

    #[tokio::test]
    async fn test_record_different_status_codes() {
        let registry = Registry::new();
        let metrics = GenAiMetrics::new(&registry).expect("Failed to create metrics");

        // Test various error codes
        for status_code in [400, 401, 500, 503] {
            let row = HttpAnalyticsRow {
                instance_id: Uuid::new_v4(),
                correlation_id: 444,
                timestamp: chrono::Utc::now(),
                method: "POST".to_string(),
                uri: "/v1/chat/completions".to_string(),
                request_model: Some("gpt-4".to_string()),
                response_model: None,
                status_code,
                duration_ms: 100,
                duration_to_first_byte_ms: Some(50),
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                response_type: "chat_completion".to_string(),
                user_id: None,
                user_email: None,
                access_source: "api_key".to_string(),
                input_price_per_token: None,
                output_price_per_token: None,
                server_address: "api.openai.com".to_string(),
                server_port: 443,
                provider_name: Some("openai".to_string()),
                fusillade_batch_id: None,
                fusillade_request_id: None,
                custom_id: None,
                request_origin: "api".to_string(),
                batch_sla: String::new(),
                batch_metadata_request_origin: String::new(),
            };

            metrics.record_from_analytics(&row).await;
        }

        let metric_families = registry.gather();
        let duration_metric = metric_families
            .iter()
            .find(|m| m.name() == "gen_ai_server_request_duration_seconds")
            .expect("Should have request duration metric");

        // Should have recorded 4 different error types
        assert_eq!(
            duration_metric.get_metric().len(),
            4,
            "Should have 4 metrics for different error codes"
        );
    }

    #[tokio::test]
    async fn test_base64_embeddings_response_type() {
        let registry = Registry::new();
        let metrics = GenAiMetrics::new(&registry).expect("Failed to create metrics");

        let row = HttpAnalyticsRow {
            instance_id: Uuid::new_v4(),
            correlation_id: 555,
            timestamp: chrono::Utc::now(),
            method: "POST".to_string(),
            uri: "/v1/embeddings".to_string(),
            request_model: Some("text-embedding-3-large".to_string()),
            response_model: Some("text-embedding-3-large".to_string()),
            status_code: 200,
            duration_ms: 300,
            duration_to_first_byte_ms: Some(250),
            prompt_tokens: 50,
            completion_tokens: 0,
            total_tokens: 50,
            response_type: "base64_embeddings".to_string(),
            user_id: None,
            user_email: None,
            access_source: "api_key".to_string(),
            input_price_per_token: None,
            output_price_per_token: None,
            server_address: "api.openai.com".to_string(),
            server_port: 443,
            provider_name: Some("openai".to_string()),
            fusillade_batch_id: None,
            fusillade_request_id: None,
            custom_id: None,
            request_origin: "api".to_string(),
            batch_sla: String::new(),
            batch_metadata_request_origin: String::new(),
        };

        metrics.record_from_analytics(&row).await;
        let metric_families = registry.gather();

        // Verify operation name is "embeddings" for base64_embeddings
        let duration_metric = metric_families
            .iter()
            .find(|m| m.name() == "gen_ai_server_request_duration_seconds")
            .expect("Should have request duration metric");

        let duration_labels = duration_metric.get_metric().first().unwrap().get_label();
        assert_eq!(
            find_label(duration_labels, "gen_ai_operation_name"),
            Some("embeddings".to_string()),
            "base64_embeddings should map to embeddings operation"
        );
    }
}
