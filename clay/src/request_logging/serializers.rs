use crate::config::Config;
use crate::request_logging::models::{AiRequest, AiResponse, ChatCompletionChunk};
use outlet::{RequestData, ResponseData};
use outlet_postgres::SerializationError;
use serde_json::Value;
use sqlx::PgPool;
use std::fmt;
use std::str;
use tracing::{error, instrument, warn};
use uuid::Uuid;

use super::utils;

/// Access source types for analytics tracking
#[derive(Clone, Debug)]
pub enum AccessSource {
    Playground,
    ApiKey,
    UnknownApiKey,
    Unauthenticated,
}

impl fmt::Display for AccessSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccessSource::Playground => write!(f, "playground"),
            AccessSource::ApiKey => write!(f, "api_key"),
            AccessSource::UnknownApiKey => write!(f, "unknown_api_key"),
            AccessSource::Unauthenticated => write!(f, "unauthenticated"),
        }
    }
}

/// Authentication information extracted from request headers
#[derive(Clone)]
pub enum Auth {
    /// Playground access via SSO proxy (X-Doubleword-User header)
    Playground { user_email: String },
    /// API key access (Authorization: Bearer <key>)
    ApiKey { bearer_token: String },
    /// No authentication found
    None,
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Auth::Playground { user_email } => f.debug_struct("Playground").field("user_email", user_email).finish(),
            Auth::ApiKey { .. } => f.debug_struct("ApiKey").field("bearer_token", &"<redacted>").finish(),
            Auth::None => write!(f, "None"),
        }
    }
}

/// Complete row structure for http_analytics table
#[derive(Debug, Clone)]
pub struct HttpAnalyticsRow {
    pub instance_id: Uuid,
    pub correlation_id: i64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub method: String,
    pub uri: String,
    pub request_model: Option<String>,
    pub response_model: Option<String>,
    pub status_code: i32,
    pub duration_ms: i64,
    pub duration_to_first_byte_ms: Option<i64>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub response_type: String,
    pub user_id: Option<Uuid>,
    pub user_email: Option<String>,
    pub access_source: String,
    pub input_price_per_token: Option<rust_decimal::Decimal>,
    pub output_price_per_token: Option<rust_decimal::Decimal>,
    pub server_address: String,
    pub server_port: u16,
    pub provider_name: Option<String>,
}

/// Usage metrics extracted from AI responses (subset of HttpAnalyticsRow)
#[derive(Debug, Clone)]
pub struct UsageMetrics {
    pub instance_id: Uuid,
    pub correlation_id: i64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub method: String,
    pub uri: String,
    pub request_model: Option<String>,
    pub response_model: Option<String>,
    pub status_code: i32,
    pub duration_ms: i64,
    pub duration_to_first_byte_ms: Option<i64>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub response_type: String,
    pub server_address: String,
    pub server_port: u16,
}

/// Parses HTTP request body data into structured AI request types.
///
/// # Arguments
/// * `request_data` - The HTTP request data containing body and metadata
///
/// # Returns
/// * `Ok(AiRequest)` - Successfully parsed request as chat completion, completion, embeddings, or other
/// * `Err(SerializationError)` - Parse error with base64-encoded fallback data for storage
///
/// # Behavior
/// - Returns `AiRequest::Other(Value::Null)` for missing or empty bodies
/// - On parse failure, returns error with base64-encoded body for safe PostgreSQL storage
pub fn parse_ai_request(request_data: &RequestData) -> Result<AiRequest, SerializationError> {
    let bytes = match &request_data.body {
        Some(body) => body.as_ref(),
        None => return Ok(AiRequest::Other(Value::Null)),
    };

    let body_str = String::from_utf8_lossy(bytes);

    if body_str.trim().is_empty() {
        return Ok(AiRequest::Other(Value::Null));
    }

    match serde_json::from_str(&body_str) {
        Ok(request) => Ok(request),
        Err(e) => {
            // Always base64 encode unparseable content to avoid PostgreSQL issues
            let base64_encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
            Err(SerializationError {
                fallback_data: format!("base64:{base64_encoded}"),
                error: Box::new(e),
            })
        }
    }
}

/// Parses HTTP response body data into structured AI response types.
///
/// # Arguments
/// * `request_data` - The original HTTP request data (used to determine response parsing strategy)  
/// * `response_data` - The HTTP response data containing body, headers, and metadata
///
/// # Returns
/// * `Ok(AiResponse)` - Successfully parsed response as chat completion, completion, embeddings, or other
/// * `Err(SerializationError)` - Parse error with base64-encoded fallback data for storage
///
/// # Behavior
/// - Returns `AiResponse::Other(Value::Null)` for missing or empty response bodies
/// - Handles gzip/brotli decompression based on Content-Encoding headers
/// - Parses streaming responses (SSE format) vs non-streaming based on request stream parameter
/// - On parse failure, returns error with base64-encoded decompressed body
pub fn parse_ai_response(request_data: &RequestData, response_data: &ResponseData) -> Result<AiResponse, SerializationError> {
    let bytes = match &response_data.body {
        Some(body) => body.as_ref(),
        None => return Ok(AiResponse::Other(Value::Null)),
    };

    if bytes.is_empty() {
        return Ok(AiResponse::Other(Value::Null));
    }

    // Decompress if needed
    let final_bytes = utils::decompress_response_if_needed(bytes, &response_data.headers)?;
    let body_str = String::from_utf8_lossy(&final_bytes);
    if body_str.trim().is_empty() {
        return Ok(AiResponse::Other(Value::Null));
    }

    // Parse response based on request type
    let result = match parse_ai_request(request_data) {
        Ok(AiRequest::ChatCompletions(chat_req)) if chat_req.stream.unwrap_or(false) => utils::parse_streaming_response(&body_str),
        Ok(AiRequest::Completions(completion_req)) if completion_req.stream.unwrap_or(false) => utils::parse_streaming_response(&body_str),
        _ => utils::parse_non_streaming_response(&body_str),
    };

    result.map_err(|_| SerializationError {
        fallback_data: format!(
            "base64:{}",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &final_bytes)
        ),
        error: "Failed to parse response as JSON or SSE".into(),
    })
}

impl UsageMetrics {
    /// Extracts usage metrics from request and response data.
    ///
    /// # Arguments
    /// * `instance_id` - Unique identifier for the service instance
    /// * `request_data` - HTTP request data containing method, URI, timestamp, and correlation ID
    /// * `response_data` - HTTP response data containing status code and duration
    /// * `parsed_response` - The parsed AI response for token usage extraction
    /// * `config` - Configuration containing server address and port
    ///
    /// # Returns
    /// A `UsageMetrics` struct with extracted model, tokens, and timing data
    pub fn extract(
        instance_id: Uuid,
        request_data: &RequestData,
        response_data: &ResponseData,
        parsed_response: &AiResponse,
        config: &Config,
    ) -> Self {
        // Extract model from request
        let request_model = match parse_ai_request(request_data) {
            Ok(AiRequest::ChatCompletions(req)) => Some(req.model),
            Ok(AiRequest::Completions(req)) => Some(req.model),
            Ok(AiRequest::Embeddings(req)) => Some(req.model),
            _ => None,
        };

        // Extract token metrics and response model from response
        let response_metrics = TokenMetrics::from(parsed_response);

        Self {
            instance_id,
            correlation_id: request_data.correlation_id as i64,
            timestamp: chrono::DateTime::<chrono::Utc>::from(request_data.timestamp),
            method: request_data.method.to_string(),
            uri: request_data.uri.to_string(),
            request_model,
            response_model: response_metrics.response_model,
            status_code: response_data.status.as_u16() as i32,
            duration_ms: response_data.duration.as_millis() as i64,
            duration_to_first_byte_ms: Some(response_data.duration_to_first_byte.as_millis() as i64),
            prompt_tokens: response_metrics.prompt_tokens,
            completion_tokens: response_metrics.completion_tokens,
            total_tokens: response_metrics.total_tokens,
            response_type: response_metrics.response_type,
            server_address: config.host.clone(),
            server_port: config.port,
        }
    }
}

impl Auth {
    /// Extract authentication from request headers
    pub fn from_request(request_data: &RequestData, config: &Config) -> Self {
        // Check for proxy header (Playground/SSO access)
        let proxy_header_name = &config.auth.proxy_header.header_name;
        if let Some(email) = Self::get_header_value(request_data, proxy_header_name) {
            return Auth::Playground { user_email: email };
        }

        // Check for API key in Authorization header
        if let Some(auth_header) = Self::get_header_value(request_data, "authorization") {
            if let Some(bearer_token) = auth_header.strip_prefix("Bearer ") {
                return Auth::ApiKey {
                    bearer_token: bearer_token.to_string(),
                };
            }
        }

        Auth::None
    }

    /// Extract header value as string
    fn get_header_value(request_data: &RequestData, header_name: &str) -> Option<String> {
        request_data
            .headers
            .get(header_name)
            .and_then(|values| values.first())
            .and_then(|bytes| str::from_utf8(bytes).ok())
            .map(|s| s.to_string())
    }
}

/// Maps provider URLs to OpenTelemetry GenAI Semantic Convention well-known values
/// https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-metrics/
fn map_url_to_otel_provider(url: &str) -> Option<&'static str> {
    let url_lower = url.to_lowercase();

    if url_lower.contains("anthropic.com") || url_lower.contains("claude.ai") {
        Some("anthropic")
    } else if url_lower.contains("bedrock") {
        Some("aws.bedrock")
    } else if url_lower.contains("inference.azure.com") {
        Some("azure.ai.inference")
    } else if url_lower.contains("openai.azure.com") {
        Some("azure.ai.openai")
    } else if url_lower.contains("cohere.com") || url_lower.contains("cohere.ai") {
        Some("cohere")
    } else if url_lower.contains("deepseek.com") {
        Some("deepseek")
    } else if url_lower.contains("gemini") {
        Some("gcp.gemini")
    } else if url_lower.contains("generativelanguage.googleapis.com") {
        Some("gcp.gen_ai")
    } else if url_lower.contains("vertexai") || url_lower.contains("vertex-ai") || url_lower.contains("aiplatform.googleapis.com") {
        Some("gcp.vertex_ai")
    } else if url_lower.contains("groq.com") {
        Some("groq")
    } else if url_lower.contains("watsonx") || url_lower.contains("ml.cloud.ibm.com") {
        Some("ibm.watsonx.ai")
    } else if url_lower.contains("mistral.ai") {
        Some("mistral_ai")
    } else if url_lower.contains("openai.com") || url_lower.contains("api.openai.com") {
        Some("openai")
    } else if url_lower.contains("perplexity.ai") {
        Some("perplexity")
    } else if url_lower.contains("x.ai") {
        Some("x_ai")
    } else {
        None
    }
}

/// Store analytics record with user and pricing enrichment, returns the complete row
#[instrument(skip(pool))]
pub async fn store_analytics_record(pool: &PgPool, metrics: &UsageMetrics, auth: &Auth) -> Result<HttpAnalyticsRow, sqlx::Error> {
    // Extract user information based on auth type
    let (user_id, user_email, access_source) = match auth {
        Auth::Playground { user_email } => {
            // Try to get user ID from email
            match sqlx::query_scalar!("SELECT id FROM users WHERE email = $1", user_email)
                .fetch_optional(pool)
                .await?
            {
                Some(user_id) => (Some(user_id), Some(user_email.clone()), AccessSource::Playground),
                None => {
                    warn!("User not found for email: {}", user_email);
                    (None, Some(user_email.clone()), AccessSource::Playground)
                }
            }
        }
        Auth::ApiKey { bearer_token } => {
            // Try to get user ID and email from API key
            match sqlx::query!(
                "SELECT u.id, u.email FROM api_keys ak JOIN users u ON ak.user_id = u.id WHERE ak.secret = $1",
                bearer_token
            )
            .fetch_optional(pool)
            .await?
            {
                Some(row) => (Some(row.id), Some(row.email), AccessSource::ApiKey),
                None => {
                    warn!("Unknown API key used");
                    (None, None, AccessSource::UnknownApiKey)
                }
            }
        }
        Auth::None => (None, None, AccessSource::Unauthenticated),
    };

    // Get model pricing and provider name if we have a model
    // Use request_model for lookup since that's what the user specified
    let (input_price_per_token, output_price_per_token, provider_name) = if let Some(ref model_name) = metrics.request_model {
        match sqlx::query!(
            r#"
            SELECT
                dm.upstream_input_price_per_token,
                dm.upstream_output_price_per_token,
                ie.name as "provider_name?",
                ie.url as "provider_url?"
            FROM deployed_models dm
            LEFT JOIN inference_endpoints ie ON dm.hosted_on = ie.id
            WHERE dm.alias = $1 OR dm.model_name = $1
            LIMIT 1
            "#,
            model_name
        )
        .fetch_optional(pool)
        .await?
        {
            Some(row) => {
                // Map URL to OTel-compliant provider name, falling back to configured name
                let otel_provider = row
                    .provider_url
                    .as_ref()
                    .and_then(|url| map_url_to_otel_provider(url))
                    .map(|s| s.to_string())
                    .or(row.provider_name);

                (
                    row.upstream_input_price_per_token,
                    row.upstream_output_price_per_token,
                    otel_provider,
                )
            }
            None => (None, None, None),
        }
    } else {
        (None, None, None)
    };

    // Construct the complete row
    let row = HttpAnalyticsRow {
        instance_id: metrics.instance_id,
        correlation_id: metrics.correlation_id,
        timestamp: metrics.timestamp,
        method: metrics.method.clone(),
        uri: metrics.uri.clone(),
        request_model: metrics.request_model.clone(),
        response_model: metrics.response_model.clone(),
        status_code: metrics.status_code,
        duration_ms: metrics.duration_ms,
        duration_to_first_byte_ms: metrics.duration_to_first_byte_ms,
        prompt_tokens: metrics.prompt_tokens,
        completion_tokens: metrics.completion_tokens,
        total_tokens: metrics.total_tokens,
        response_type: metrics.response_type.clone(),
        user_id,
        user_email: user_email.clone(),
        access_source: access_source.to_string(),
        input_price_per_token,
        output_price_per_token,
        server_address: metrics.server_address.clone(),
        server_port: metrics.server_port,
        provider_name,
    };

    // Insert the analytics record using the row data
    sqlx::query!(
        r#"
        INSERT INTO http_analytics (
            instance_id, correlation_id, timestamp, method, uri, model,
            status_code, duration_ms, duration_to_first_byte_ms, prompt_tokens, completion_tokens,
            total_tokens, response_type, user_id, user_email, access_source,
            input_price_per_token, output_price_per_token
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
        ON CONFLICT (instance_id, correlation_id)
        DO UPDATE SET
            status_code = EXCLUDED.status_code,
            duration_ms = EXCLUDED.duration_ms,
            duration_to_first_byte_ms = EXCLUDED.duration_to_first_byte_ms,
            prompt_tokens = EXCLUDED.prompt_tokens,
            completion_tokens = EXCLUDED.completion_tokens,
            total_tokens = EXCLUDED.total_tokens,
            response_type = EXCLUDED.response_type,
            user_id = EXCLUDED.user_id,
            user_email = EXCLUDED.user_email,
            access_source = EXCLUDED.access_source,
            input_price_per_token = EXCLUDED.input_price_per_token,
            output_price_per_token = EXCLUDED.output_price_per_token
        "#,
        row.instance_id,
        row.correlation_id,
        row.timestamp,
        row.method,
        row.uri,
        row.request_model,
        row.status_code,
        row.duration_ms,
        row.duration_to_first_byte_ms,
        row.prompt_tokens,
        row.completion_tokens,
        row.total_tokens,
        row.response_type,
        row.user_id,
        row.user_email,
        row.access_source,
        row.input_price_per_token,
        row.output_price_per_token
    )
    .execute(pool)
    .await?;

    Ok(row)
}

/// Helper struct for extracting token metrics from responses
#[derive(Debug, Clone)]
struct TokenMetrics {
    prompt_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
    response_type: String,
    response_model: Option<String>,
}

impl From<&AiResponse> for TokenMetrics {
    fn from(response: &AiResponse) -> Self {
        match response {
            AiResponse::ChatCompletions(response) => {
                if let Some(usage) = &response.usage {
                    Self {
                        prompt_tokens: usage.prompt_tokens as i64,
                        completion_tokens: usage.completion_tokens as i64,
                        total_tokens: usage.total_tokens as i64,
                        response_type: "chat_completion".to_string(),
                        response_model: Some(response.model.clone()),
                    }
                } else {
                    Self {
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        total_tokens: 0,
                        response_type: "chat_completion".to_string(),
                        response_model: Some(response.model.clone()),
                    }
                }
            }
            AiResponse::ChatCompletionsStream(chunks) => {
                // For streaming responses, token usage and model are in the last Normal chunk (not Done marker)
                // Find the last Normal chunk, prioritizing those with usage data
                let last_normal_with_usage = chunks.iter().rev().find_map(|chunk| match chunk {
                    ChatCompletionChunk::Normal(normal_chunk) if normal_chunk.usage.is_some() => Some(normal_chunk),
                    _ => None,
                });

                let model = chunks.iter().find_map(|chunk| match chunk {
                    ChatCompletionChunk::Normal(c) => Some(c.model.clone()),
                    _ => None,
                });

                if let Some(chunk) = last_normal_with_usage {
                    if let Some(usage) = &chunk.usage {
                        Self {
                            prompt_tokens: usage.prompt_tokens as i64,
                            completion_tokens: usage.completion_tokens as i64,
                            total_tokens: usage.total_tokens as i64,
                            response_type: "chat_completion_stream".to_string(),
                            response_model: model,
                        }
                    } else {
                        // This shouldn't happen since we filtered for usage.is_some()
                        Self {
                            prompt_tokens: 0,
                            completion_tokens: 0,
                            total_tokens: 0,
                            response_type: "chat_completion_stream".to_string(),
                            response_model: model,
                        }
                    }
                } else {
                    Self {
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        total_tokens: 0,
                        response_type: "chat_completion_stream".to_string(),
                        response_model: model,
                    }
                }
            }
            AiResponse::Completions(response) => {
                if let Some(usage) = &response.usage {
                    Self {
                        prompt_tokens: usage.prompt_tokens as i64,
                        completion_tokens: usage.completion_tokens as i64,
                        total_tokens: usage.total_tokens as i64,
                        response_type: "completion".to_string(),
                        response_model: Some(response.model.clone()),
                    }
                } else {
                    Self {
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        total_tokens: 0,
                        response_type: "completion".to_string(),
                        response_model: Some(response.model.clone()),
                    }
                }
            }
            AiResponse::Embeddings(response) => {
                let usage = &response.usage;
                Self {
                    prompt_tokens: usage.prompt_tokens as i64,
                    completion_tokens: 0, // Embeddings don't have completion tokens
                    total_tokens: usage.total_tokens as i64,
                    response_type: "embeddings".to_string(),
                    response_model: Some(response.model.clone()),
                }
            }
            AiResponse::Base64Embeddings(response) => {
                let usage = &response.usage;
                Self {
                    prompt_tokens: usage.prompt_tokens as i64,
                    completion_tokens: 0, // Embeddings don't have completion tokens
                    total_tokens: usage.total_tokens as i64,
                    response_type: "base64_embeddings".to_string(),
                    response_model: Some(response.model.clone()),
                }
            }
            AiResponse::Other(_) => Self {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                response_type: "other".to_string(),
                response_model: None,
            },
        }
    }
}

pub struct AnalyticsResponseSerializer<M = crate::metrics::GenAiMetrics>
where
    M: crate::metrics::MetricsRecorder + Clone + 'static,
{
    pool: PgPool,
    instance_id: Uuid,
    config: Config,
    metrics_recorder: Option<M>,
}

impl<M> AnalyticsResponseSerializer<M>
where
    M: crate::metrics::MetricsRecorder + Clone + 'static,
{
    /// Creates a new analytics response serializer.
    ///
    /// # Arguments
    /// * `pool` - Database connection pool for storing analytics data
    /// * `instance_id` - Unique identifier for this service instance
    /// * `config` - Application configuration
    /// * `metrics_recorder` - Optional metrics recorder
    pub fn new(pool: PgPool, instance_id: Uuid, config: Config, metrics_recorder: Option<M>) -> Self {
        Self {
            pool,
            instance_id,
            config,
            metrics_recorder,
        }
    }

    /// Creates a serializer function that parses responses and stores analytics data.
    ///
    /// # Returns
    /// A closure that implements the outlet-postgres serializer interface:
    /// - Takes `RequestData` and `ResponseData` as input
    /// - Returns parsed `AiResponse` or `SerializationError`
    /// - Asynchronously stores analytics metrics to database
    /// - Logs errors if analytics storage fails
    pub fn create_serializer(self) -> impl Fn(&RequestData, &ResponseData) -> Result<AiResponse, SerializationError> + Send + Sync {
        move |request_data: &RequestData, response_data: &ResponseData| {
            // The full response that gets written to the outlet-postgres database
            let parsed_response = parse_ai_response(request_data, response_data)?;

            // Basic metrics
            let metrics = UsageMetrics::extract(self.instance_id, request_data, response_data, &parsed_response, &self.config);

            // Auth information
            let auth = Auth::from_request(request_data, &self.config);

            // Clone data for async processing
            let pool_clone = self.pool.clone();
            let metrics_recorder_clone = self.metrics_recorder.clone();

            // The write to the analytics table and metrics recording
            tokio::spawn(async move {
                // Store to database - this enriches with user/pricing data and returns complete row
                match store_analytics_record(&pool_clone, &metrics, &auth).await {
                    Ok(complete_row) => {
                        // Record metrics using the complete row (called AFTER database write)
                        if let Some(ref recorder) = metrics_recorder_clone {
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
            });

            Ok(parsed_response)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_ai_request, parse_ai_response, UsageMetrics};
    use crate::request_logging::models::{AiRequest, AiResponse};
    use async_openai::types::{
        CreateBase64EmbeddingResponse, CreateChatCompletionResponse, CreateChatCompletionStreamResponse, CreateCompletionResponse,
        CreateEmbeddingResponse, EmbeddingUsage,
    };
    use axum::http::{Method, StatusCode, Uri};
    use bytes::Bytes;
    use outlet::{RequestData, ResponseData};
    use std::{
        collections::HashMap,
        time::{Duration, SystemTime},
    };
    use uuid::Uuid;

    #[test]
    fn test_parse_ai_request_no_body() {
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
        };

        let result = parse_ai_request(&request_data).unwrap();

        match result {
            AiRequest::Other(value) => assert!(value.is_null()),
            _ => panic!("Expected AiRequest::Other(null)"),
        }
    }

    #[test]
    fn test_parse_ai_request_empty_bytes() {
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: Some(Bytes::new()), // Empty bytes
        };

        let result = parse_ai_request(&request_data).unwrap();

        match result {
            AiRequest::Other(value) => assert!(value.is_null()),
            _ => panic!("Expected AiRequest::Other(null)"),
        }
    }

    #[test]
    fn test_parse_ai_request_invalid_json() {
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: Some(Bytes::from("invalid json")),
        };

        let result = parse_ai_request(&request_data);

        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.fallback_data.starts_with("base64:"));
    }

    #[test]
    fn test_parse_ai_request_valid_json() {
        let json_body = r#"{"model": "gpt-4", "messages": [{"role": "user", "content": "hello"}]}"#;
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: Some(Bytes::from(json_body)),
        };

        let result = parse_ai_request(&request_data).unwrap();

        match result {
            AiRequest::ChatCompletions(req) => {
                assert_eq!(req.model, "gpt-4");
                assert_eq!(req.messages.len(), 1);
            }
            _ => panic!("Expected AiRequest::ChatCompletions"),
        }
    }

    #[test]
    fn test_parse_ai_request_completions() {
        let json_body = r#"{"model": "gpt-3.5-turbo-instruct", "prompt": "Say hello"}"#;
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: Some(Bytes::from(json_body)),
        };

        let result = parse_ai_request(&request_data).unwrap();

        match result {
            AiRequest::Completions(req) => {
                assert_eq!(req.model, "gpt-3.5-turbo-instruct");
            }
            _ => panic!("Expected AiRequest::Completions"),
        }
    }

    #[test]
    fn test_parse_ai_request_embeddings() {
        let json_body = r#"{"model": "text-embedding-ada-002", "input": "hello world"}"#;
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: Some(Bytes::from(json_body)),
        };

        let result = parse_ai_request(&request_data).unwrap();

        match result {
            AiRequest::Embeddings(req) => {
                assert_eq!(req.model, "text-embedding-ada-002");
            }
            _ => panic!("Expected AiRequest::Embeddings"),
        }
    }

    #[test]
    fn test_parse_ai_response_no_body() {
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
        };

        let response_data = ResponseData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: None,
            duration: Duration::from_millis(100),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let result = parse_ai_response(&request_data, &response_data).unwrap();

        match result {
            AiResponse::Other(value) => assert!(value.is_null()),
            _ => panic!("Expected AiResponse::Other(null)"),
        }
    }

    #[test]
    fn test_parse_ai_response_empty_body() {
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
        };

        let response_data = ResponseData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: Some(Bytes::new()), // Empty bytes
            duration: Duration::from_millis(100),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let result = parse_ai_response(&request_data, &response_data).unwrap();

        match result {
            AiResponse::Other(value) => assert!(value.is_null()),
            _ => panic!("Expected AiResponse::Other(null)"),
        }
    }

    #[test]
    fn test_parse_ai_response_valid_json() {
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
        };

        let json_response = r#"{
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "created": 1677652288,
            "model": "gpt-4",
            "choices": [],
            "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
        }"#;

        let response_data = ResponseData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: Some(Bytes::from(json_response)),
            duration: Duration::from_millis(100),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let result = parse_ai_response(&request_data, &response_data).unwrap();

        match result {
            AiResponse::ChatCompletions(response) => {
                assert_eq!(response.model, "gpt-4");
                assert_eq!(response.id, "chatcmpl-123");
            }
            _ => panic!("Expected AiResponse::ChatCompletions"),
        }
    }

    #[test]
    fn test_parse_ai_response_streaming() {
        // Request with stream: true
        let request_json = r#"{"model": "gpt-4", "messages": [{"role": "user", "content": "hello"}], "stream": true}"#;
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: Some(Bytes::from(request_json)),
        };

        // SSE streaming response
        let sse_response = "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1677652288,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"}}]}\n\ndata: [DONE]\n\n";

        let response_data = ResponseData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: Some(Bytes::from(sse_response)),
            duration: Duration::from_millis(100),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let result = parse_ai_response(&request_data, &response_data).unwrap();

        match result {
            AiResponse::ChatCompletionsStream(chunks) => {
                assert!(!chunks.is_empty());
            }
            _ => panic!("Expected AiResponse::ChatCompletionsStream"),
        }
    }

    #[test]
    fn test_parse_ai_response_embeddings() {
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
        };

        let embeddings_response = r#"{
            "object": "list",
            "data": [{"object": "embedding", "embedding": [0.1, 0.2], "index": 0}],
            "model": "text-embedding-ada-002",
            "usage": {"prompt_tokens": 5, "total_tokens": 5}
        }"#;

        let response_data = ResponseData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: Some(Bytes::from(embeddings_response)),
            duration: Duration::from_millis(100),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let result = parse_ai_response(&request_data, &response_data).unwrap();

        match result {
            AiResponse::Embeddings(response) => {
                assert_eq!(response.model, "text-embedding-ada-002");
                assert_eq!(response.object, "list");
            }
            _ => panic!("Expected AiResponse::Embeddings"),
        }
    }

    #[test]
    fn test_parse_ai_response_invalid_json() {
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
        };

        let response_data = ResponseData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: Some(Bytes::from("invalid json response")),
            duration: Duration::from_millis(100),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let result = parse_ai_response(&request_data, &response_data);

        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.fallback_data.starts_with("base64:"));
    }

    #[test]
    fn test_analytics_metrics_extract_basic() {
        let instance_id = Uuid::new_v4();

        let request_data = RequestData {
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/v1/chat/completions".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
        };

        let response_data = ResponseData {
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: None,
            duration: Duration::from_millis(250),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let parsed_response = AiResponse::Other(serde_json::Value::Null);

        let metrics = UsageMetrics::extract(
            instance_id,
            &request_data,
            &response_data,
            &parsed_response,
            &crate::test_utils::create_test_config(),
        );

        assert_eq!(metrics.instance_id, instance_id);
        assert_eq!(metrics.correlation_id, 12345);
        assert_eq!(metrics.method, "POST");
        assert_eq!(metrics.uri, "/v1/chat/completions");
        assert_eq!(metrics.request_model, None);
        assert_eq!(metrics.response_model, None);
        assert_eq!(metrics.status_code, 200);
        assert_eq!(metrics.duration_ms, 250);
        assert_eq!(metrics.duration_to_first_byte_ms, Some(50));
        assert_eq!(metrics.prompt_tokens, 0);
        assert_eq!(metrics.completion_tokens, 0);
        assert_eq!(metrics.total_tokens, 0);
        assert_eq!(metrics.response_type, "other");
    }

    #[test]
    fn test_analytics_metrics_extract_with_tokens() {
        let instance_id = Uuid::new_v4();

        // Request with model info
        let request_json = r#"{"model": "gpt-4", "messages": [{"role": "user", "content": "hello"}]}"#;
        let request_data = RequestData {
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/v1/chat/completions".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: Some(Bytes::from(request_json)),
        };

        let response_data = ResponseData {
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: None,
            duration: Duration::from_millis(500),
            duration_to_first_byte: Duration::from_millis(50),
        };

        // Response with usage data
        let chat_response = CreateChatCompletionResponse {
            id: "chatcmpl-123".to_string(),
            object: "chat.completion".to_string(),
            created: 1677652288,
            model: "gpt-5".to_string(),
            choices: vec![],
            usage: Some(async_openai::types::CompletionUsage {
                prompt_tokens: 15,
                completion_tokens: 25,
                total_tokens: 40,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
            system_fingerprint: None,
            service_tier: None,
        };

        let parsed_response = AiResponse::ChatCompletions(chat_response);

        let metrics = UsageMetrics::extract(
            instance_id,
            &request_data,
            &response_data,
            &parsed_response,
            &crate::test_utils::create_test_config(),
        );

        assert_eq!(metrics.instance_id, instance_id);
        assert_eq!(metrics.correlation_id, 12345);
        assert_eq!(metrics.method, "POST");
        assert_eq!(metrics.uri, "/v1/chat/completions");
        assert_eq!(metrics.request_model, Some("gpt-4".to_string()));
        assert_eq!(metrics.response_model, Some("gpt-5".to_string()));
        assert_eq!(metrics.status_code, 200);
        assert_eq!(metrics.duration_ms, 500);
        assert_eq!(metrics.prompt_tokens, 15);
        assert_eq!(metrics.completion_tokens, 25);
        assert_eq!(metrics.total_tokens, 40);
        assert_eq!(metrics.response_type, "chat_completion");
    }

    #[test]
    fn test_analytics_metrics_extract_streaming_tokens() {
        let instance_id = Uuid::new_v4();

        let request_data = RequestData {
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/v1/chat/completions".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
        };

        let response_data = ResponseData {
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: None,
            duration: Duration::from_millis(300),
            duration_to_first_byte: Duration::from_millis(50),
        };

        // Streaming response with usage in the last chunk
        let stream_chunk = CreateChatCompletionStreamResponse {
            id: "chatcmpl-123".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1677652288,
            model: "gpt-4".to_string(),
            choices: vec![],
            usage: Some(async_openai::types::CompletionUsage {
                prompt_tokens: 8,
                completion_tokens: 12,
                total_tokens: 20,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
            system_fingerprint: None,
            service_tier: None,
        };

        let parsed_response =
            AiResponse::ChatCompletionsStream(vec![crate::request_logging::models::ChatCompletionChunk::Normal(stream_chunk)]);

        let metrics = UsageMetrics::extract(
            instance_id,
            &request_data,
            &response_data,
            &parsed_response,
            &crate::test_utils::create_test_config(),
        );

        assert_eq!(metrics.prompt_tokens, 8);
        assert_eq!(metrics.completion_tokens, 12);
        assert_eq!(metrics.total_tokens, 20);
        assert_eq!(metrics.response_type, "chat_completion_stream");
    }

    #[test]
    fn test_analytics_metrics_extract_embeddings_tokens() {
        let instance_id = Uuid::new_v4();

        let request_data = RequestData {
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/v1/embeddings".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
        };

        let response_data = ResponseData {
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: None,
            duration: Duration::from_millis(150),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let embeddings_response = CreateEmbeddingResponse {
            object: "list".to_string(),
            data: vec![],
            model: "text-embedding-ada-002".to_string(),
            usage: EmbeddingUsage {
                prompt_tokens: 6,
                total_tokens: 6,
            },
        };

        let parsed_response = AiResponse::Embeddings(embeddings_response);

        let metrics = UsageMetrics::extract(
            instance_id,
            &request_data,
            &response_data,
            &parsed_response,
            &crate::test_utils::create_test_config(),
        );

        assert_eq!(metrics.prompt_tokens, 6);
        assert_eq!(metrics.completion_tokens, 0); // Embeddings don't have completion tokens
        assert_eq!(metrics.total_tokens, 6);
        assert_eq!(metrics.response_type, "embeddings");
    }

    #[test]
    fn test_analytics_metrics_extract_completions_tokens() {
        let instance_id = Uuid::new_v4();

        let request_data = RequestData {
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/v1/completions".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
        };

        let response_data = ResponseData {
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: None,
            duration: Duration::from_millis(400),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let completions_response = CreateCompletionResponse {
            id: "cmpl-123".to_string(),
            object: "text_completion".to_string(),
            created: 1677652288,
            model: "gpt-3.5-turbo-instruct".to_string(),
            choices: vec![],
            usage: Some(async_openai::types::CompletionUsage {
                prompt_tokens: 10,
                completion_tokens: 15,
                total_tokens: 25,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
            system_fingerprint: None,
        };

        let parsed_response = AiResponse::Completions(completions_response);

        let metrics = UsageMetrics::extract(
            instance_id,
            &request_data,
            &response_data,
            &parsed_response,
            &crate::test_utils::create_test_config(),
        );

        assert_eq!(metrics.prompt_tokens, 10);
        assert_eq!(metrics.completion_tokens, 15);
        assert_eq!(metrics.total_tokens, 25);
        assert_eq!(metrics.response_type, "completion");
    }

    #[test]
    fn test_analytics_metrics_extract_base64_embeddings_tokens() {
        let instance_id = Uuid::new_v4();

        let request_data = RequestData {
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/v1/embeddings".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
        };

        let response_data = ResponseData {
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: None,
            duration: Duration::from_millis(200),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let base64_embeddings_response = CreateBase64EmbeddingResponse {
            object: "list".to_string(),
            data: vec![],
            model: "text-embedding-3-large".to_string(),
            usage: EmbeddingUsage {
                prompt_tokens: 4,
                total_tokens: 4,
            },
        };

        let parsed_response = AiResponse::Base64Embeddings(base64_embeddings_response);

        let metrics = UsageMetrics::extract(
            instance_id,
            &request_data,
            &response_data,
            &parsed_response,
            &crate::test_utils::create_test_config(),
        );

        assert_eq!(metrics.prompt_tokens, 4);
        assert_eq!(metrics.completion_tokens, 0); // Base64 embeddings don't have completion tokens
        assert_eq!(metrics.total_tokens, 4);
        assert_eq!(metrics.response_type, "base64_embeddings");
    }

    #[test]
    fn test_map_url_to_otel_provider_anthropic() {
        assert_eq!(
            super::map_url_to_otel_provider("https://api.anthropic.com/v1/messages"),
            Some("anthropic")
        );
        assert_eq!(super::map_url_to_otel_provider("https://claude.ai/api/"), Some("anthropic"));
    }

    #[test]
    fn test_map_url_to_otel_provider_openai() {
        assert_eq!(
            super::map_url_to_otel_provider("https://api.openai.com/v1/chat/completions"),
            Some("openai")
        );
        assert_eq!(super::map_url_to_otel_provider("https://openai.com/"), Some("openai"));
    }

    #[test]
    fn test_map_url_to_otel_provider_azure() {
        assert_eq!(
            super::map_url_to_otel_provider("https://my-resource.openai.azure.com/openai/deployments/gpt-4"),
            Some("azure.ai.openai")
        );
        assert_eq!(
            super::map_url_to_otel_provider("https://my-deployment.inference.azure.com/"),
            Some("azure.ai.inference")
        );
    }

    #[test]
    fn test_map_url_to_otel_provider_gcp() {
        assert_eq!(
            super::map_url_to_otel_provider("https://us-central1-aiplatform.googleapis.com/v1/projects/my-project"),
            Some("gcp.vertex_ai")
        );
        assert_eq!(
            super::map_url_to_otel_provider("https://generativelanguage.googleapis.com/v1beta/models"),
            Some("gcp.gen_ai")
        );
        assert_eq!(
            super::map_url_to_otel_provider("https://gemini-api.google.com/"),
            Some("gcp.gemini")
        );
    }

    #[test]
    fn test_map_url_to_otel_provider_aws() {
        assert_eq!(
            super::map_url_to_otel_provider("https://bedrock-runtime.us-east-1.amazonaws.com/model/"),
            Some("aws.bedrock")
        );
    }

    #[test]
    fn test_map_url_to_otel_provider_other_providers() {
        assert_eq!(
            super::map_url_to_otel_provider("https://api.cohere.com/v1/generate"),
            Some("cohere")
        );
        assert_eq!(
            super::map_url_to_otel_provider("https://api.deepseek.com/v1/chat"),
            Some("deepseek")
        );
        assert_eq!(super::map_url_to_otel_provider("https://api.groq.com/v1/models"), Some("groq"));
        assert_eq!(
            super::map_url_to_otel_provider("https://api.mistral.ai/v1/chat"),
            Some("mistral_ai")
        );
        assert_eq!(
            super::map_url_to_otel_provider("https://api.perplexity.ai/chat"),
            Some("perplexity")
        );
        assert_eq!(super::map_url_to_otel_provider("https://api.x.ai/v1/chat"), Some("x_ai"));
        assert_eq!(
            super::map_url_to_otel_provider("https://us-south.ml.cloud.ibm.com/ml/v1/text/generation?version=2023-05-29"),
            Some("ibm.watsonx.ai")
        );
    }

    #[test]
    fn test_map_url_to_otel_provider_unknown() {
        assert_eq!(super::map_url_to_otel_provider("https://my-custom-llm-provider.com/v1/chat"), None);
        assert_eq!(super::map_url_to_otel_provider("https://localhost:8080/v1/models"), None);
    }

    #[test]
    fn test_map_url_to_otel_provider_case_insensitive() {
        assert_eq!(super::map_url_to_otel_provider("https://API.OPENAI.COM/v1/chat"), Some("openai"));
        assert_eq!(super::map_url_to_otel_provider("HTTPS://API.ANTHROPIC.COM/"), Some("anthropic"));
    }
}
