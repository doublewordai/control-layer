use crate::config::{Config, ONWARDS_INPUT_TOKEN_PRICE_HEADER, ONWARDS_OUTPUT_TOKEN_PRICE_HEADER};
use crate::db::handlers::Credits;
use crate::db::models::credits::{CreditTransactionCreateDBRequest, CreditTransactionType};
use crate::request_logging::models::{AiRequest, AiResponse, ChatCompletionChunk};
use outlet::{RequestData, ResponseData};
use outlet_postgres::SerializationError;
use rust_decimal::{prelude::ToPrimitive, Decimal};
use serde_json::Value;
use sqlx::PgPool;
use std::fmt;
use std::str;
use tracing::{debug, error, instrument, warn};
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
    pub input_price_per_token: Option<rust_decimal::Decimal>,
    pub output_price_per_token: Option<rust_decimal::Decimal>,
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
            input_price_per_token: response_data.headers.get(ONWARDS_INPUT_TOKEN_PRICE_HEADER).and_then(|vals| {
                vals.first()
                    .and_then(|bytes| str::from_utf8(bytes).ok())
                    .and_then(|s| s.parse::<rust_decimal::Decimal>().ok())
            }),
            output_price_per_token: response_data.headers.get(ONWARDS_OUTPUT_TOKEN_PRICE_HEADER).and_then(|vals| {
                vals.first()
                    .and_then(|bytes| str::from_utf8(bytes).ok())
                    .and_then(|s| s.parse::<rust_decimal::Decimal>().ok())
            }),
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
    let provider_name = if let Some(model_name) = &metrics.request_model {
        sqlx::query!(
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
        .map(|row| {
            row.provider_url
                .as_ref()
                .and_then(|url| map_url_to_otel_provider(url))
                .map(|s| s.to_string())
                .or(row.provider_name)
        })
    } else {
        None
    }
    .flatten();

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
        input_price_per_token: metrics.input_price_per_token,
        output_price_per_token: metrics.output_price_per_token,
        server_address: metrics.server_address.clone(),
        server_port: metrics.server_port,
        provider_name,
    };

    // Insert the analytics record and get the ID
    let analytics_id = sqlx::query_scalar!(
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
        RETURNING id
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
    .fetch_one(pool)
    .await?;

    // =======================================================================
    // Deduct credits for API usage if applicable
    // =======================================================================
    // Skip credit deduction for Playground access, Auth::None will also be implicitly skipped as they'll have no user id
    if !matches!(auth, Auth::Playground { .. }) {
        // Deduct credits if applicable (user_id present and at least one price is set)
        if let (Some(user_id), Some(model)) = (row.user_id, &row.request_model) {
            // Check if at least one price is set
            if row.input_price_per_token.is_some() || row.output_price_per_token.is_some() {
                // Calculate total cost - use zero for missing prices
                let input_cost = Decimal::from(row.prompt_tokens) * row.input_price_per_token.unwrap_or(rust_decimal::Decimal::ZERO);
                let output_cost = Decimal::from(row.completion_tokens) * row.output_price_per_token.unwrap_or(rust_decimal::Decimal::ZERO);
                let total_cost = input_cost + output_cost;

                // Round to 2 decimal places to match database schema (DECIMAL(12, 2))
                // This prevents precision issues when storing fractional cent amounts
                let total_cost = total_cost.round_dp(2);

                if total_cost > rust_decimal::Decimal::ZERO {
                    let mut conn = pool.acquire().await?;
                    let mut credits = Credits::new(&mut conn);

                    // Get user balance to check for negative balance warning
                    match credits.get_user_balance(user_id).await {
                        Ok(balance) => {
                            // Warn if this will result in negative balance
                            if balance < total_cost {
                                warn!(
                                    user_id = %user_id,
                                    current_balance = %balance,
                                    cost = %total_cost,
                                    "API usage will result in negative balance"
                                );
                            }

                            // Create usage transaction referencing the analytics record
                            match credits
                                .create_transaction(&CreditTransactionCreateDBRequest {
                                    user_id,
                                    transaction_type: CreditTransactionType::Usage,
                                    amount: total_cost,
                                    source_id: analytics_id.to_string(),
                                    description: Some(format!(
                                        "API usage: {} ({} input + {} output tokens)",
                                        model, row.prompt_tokens, row.completion_tokens
                                    )),
                                })
                                .await
                            {
                                Ok(result) => {
                                    debug!(
                                        user_id = %user_id,
                                        transaction_id = %result.id,
                                        amount = %total_cost,
                                        balance_after = %result.balance_after,
                                        model = %model,
                                        "Credits deducted for API usage"
                                    );
                                    crate::metrics::record_credit_deduction(
                                        &user_id.to_string(),
                                        model,
                                        total_cost.to_f64().unwrap_or(0.0),
                                    );
                                }
                                Err(e) => {
                                    tracing::error!(
                                        error = %e,
                                        correlation_id = %row.correlation_id,
                                        user_id = %user_id,
                                        "Failed to create credit transaction for API usage"
                                    );
                                    crate::metrics::record_credit_deduction_error();
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                error = %e,
                                correlation_id = %row.correlation_id,
                                user_id = %user_id,
                                "Failed to get user balance for credit deduction"
                            );
                            crate::metrics::record_credit_deduction_error();
                        }
                    }
                }
            }
        }
    }

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

    // ===== Credit Deduction Tests =====
    // Tests for the credit deduction functionality in store_analytics_record

    mod credit_deduction_tests {
        use super::super::*;
        use crate::api::models::users::Role;
        use crate::db::handlers::api_keys::ApiKeys;
        use crate::db::handlers::credits::Credits;
        use crate::db::handlers::Repository;
        use crate::db::models::api_keys::{ApiKeyCreateDBRequest, ApiKeyPurpose};
        use crate::db::models::credits::{CreditTransactionCreateDBRequest, CreditTransactionType};
        use crate::test_utils::create_test_user;
        use rust_decimal::Decimal;
        use std::str::FromStr;
        use uuid::Uuid;

        /// Helper: Create a test user with an initial credit balance
        async fn setup_user_with_balance(pool: &sqlx::PgPool, balance: Decimal) -> Uuid {
            let user = create_test_user(pool, Role::StandardUser).await;
            let user_id = user.id;

            if balance > Decimal::ZERO {
                // Create an initial AdminGrant transaction to set balance
                let mut conn = pool.acquire().await.expect("Failed to acquire connection");
                let mut credits = Credits::new(&mut conn);

                credits
                    .create_transaction(&CreditTransactionCreateDBRequest {
                        user_id,
                        transaction_type: CreditTransactionType::AdminGrant,
                        amount: balance,
                        source_id: "test_setup".to_string(),
                        description: Some("Initial test balance".to_string()),
                    })
                    .await
                    .expect("Failed to create initial balance");
            }

            user_id
        }

        /// Helper: Create test UsageMetrics for testing
        fn create_test_usage_metrics(
            input_price: Option<Decimal>,
            output_price: Option<Decimal>,
            prompt_tokens: i64,
            completion_tokens: i64,
        ) -> UsageMetrics {
            UsageMetrics {
                instance_id: Uuid::new_v4(),
                correlation_id: 12345,
                timestamp: chrono::Utc::now(),
                method: "POST".to_string(),
                uri: "/v1/chat/completions".to_string(),
                request_model: Some("gpt-4".to_string()),
                response_model: Some("gpt-4".to_string()),
                status_code: 200,
                duration_ms: 100,
                duration_to_first_byte_ms: Some(50),
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
                response_type: "chat_completion".to_string(),
                server_address: "api.openai.com".to_string(),
                server_port: 443,
                input_price_per_token: input_price,
                output_price_per_token: output_price,
            }
        }

        /// Helper: Create test Auth for API key with valid user
        async fn create_test_auth_for_user(pool: &sqlx::PgPool, user_id: Uuid) -> Auth {
            // Create an API key for this user
            let mut conn = pool.acquire().await.expect("Failed to acquire connection");
            let mut api_key_repo = ApiKeys::new(&mut conn);
            let key = api_key_repo
                .create(&ApiKeyCreateDBRequest {
                    user_id,
                    name: "Test API key".to_string(),
                    description: None,
                    purpose: ApiKeyPurpose::Inference,
                    requests_per_second: None,
                    burst_size: None,
                })
                .await
                .unwrap();

            Auth::ApiKey { bearer_token: key.secret }
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_credit_deduction_successful(pool: sqlx::PgPool) {
            // Setup: User with $10.00 balance
            let initial_balance = Decimal::from_str("10.00").unwrap();
            let user_id = setup_user_with_balance(&pool, initial_balance).await;
            let auth = create_test_auth_for_user(&pool, user_id).await;

            // Pricing: 0.0001 per 1K input tokens, 0.0003 per 1K output tokens
            let input_price = Decimal::from_str("0.00001").unwrap();
            let output_price = Decimal::from_str("0.00003").unwrap();

            // Usage: 1000 input tokens, 500 output tokens
            // Calculated cost: (1000 * 0.00001) + (500 * 0.00003) = 0.01 + 0.015 = 0.025
            // Rounded cost: 0.025 rounds to 0.02 (banker's rounding to 2 decimal places)
            let metrics = create_test_usage_metrics(Some(input_price), Some(output_price), 1000, 500);

            // Execute
            let result = store_analytics_record(&pool, &metrics, &auth).await;
            assert!(result.is_ok(), "store_analytics_record should succeed");

            // Verify: Balance should be deducted
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            let final_balance = credits.get_user_balance(user_id).await.unwrap();

            // Expected cost is rounded to 2 decimal places (0.025 -> 0.02)
            let expected_cost = Decimal::from_str("0.02").unwrap();
            let expected_balance = initial_balance - expected_cost;
            assert_eq!(final_balance, expected_balance, "Balance should be deducted correctly");

            // Verify: Transaction was created
            let transactions = credits.list_user_transactions(user_id, 0, 10).await.unwrap();
            let usage_tx = transactions.iter().find(|tx| tx.transaction_type == CreditTransactionType::Usage);
            assert!(usage_tx.is_some(), "Usage transaction should be created");

            let usage_tx = usage_tx.unwrap();
            assert_eq!(
                usage_tx.amount, expected_cost,
                "Transaction amount should be rounded to 2 decimal places"
            );
            assert_eq!(usage_tx.balance_after, expected_balance, "Balance after should match");
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_skip_deduction_when_user_id_none(pool: sqlx::PgPool) {
            // Setup: Auth::None (no user)
            let auth = Auth::None;
            let metrics = create_test_usage_metrics(
                Some(Decimal::from_str("0.00001").unwrap()),
                Some(Decimal::from_str("0.00003").unwrap()),
                1000,
                500,
            );

            // Execute
            let result = store_analytics_record(&pool, &metrics, &auth).await;
            assert!(result.is_ok(), "Should succeed even without user_id");

            // Verify: No transactions should be created
            // (We can't check this directly without querying all transactions, but the test passing means no error occurred)
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_skip_deduction_when_cost_zero(pool: sqlx::PgPool) {
            // Setup: User with balance, but zero tokens
            let user_id = setup_user_with_balance(&pool, Decimal::from_str("10.00").unwrap()).await;
            let auth = create_test_auth_for_user(&pool, user_id).await;
            let metrics = create_test_usage_metrics(
                Some(Decimal::from_str("0.00001").unwrap()),
                Some(Decimal::from_str("0.00003").unwrap()),
                0, // Zero tokens
                0,
            );

            // Execute
            let result = store_analytics_record(&pool, &metrics, &auth).await;
            assert!(result.is_ok(), "Should succeed with zero cost");

            // Verify: Balance should not change (no Usage transaction)
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            let balance = credits.get_user_balance(user_id).await.unwrap();
            assert_eq!(balance, Decimal::from_str("10.00").unwrap(), "Balance should not change");

            let transactions = credits.list_user_transactions(user_id, 0, 10).await.unwrap();
            let usage_tx = transactions.iter().find(|tx| tx.transaction_type == CreditTransactionType::Usage);
            assert!(usage_tx.is_none(), "No Usage transaction should be created for zero cost");
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_insufficient_balance_still_accrues_usage_transaction(pool: sqlx::PgPool) {
            // Setup: User with insufficient balance (0.01)
            let initial_balance = Decimal::from_str("0.01").unwrap();
            let user_id = setup_user_with_balance(&pool, initial_balance).await;
            let auth = create_test_auth_for_user(&pool, user_id).await;

            // Usage that costs more than balance (0.025 calculated, rounds to 0.02)
            let input_price = Decimal::from_str("0.00001").unwrap();
            let output_price = Decimal::from_str("0.00003").unwrap();
            let metrics = create_test_usage_metrics(Some(input_price), Some(output_price), 1000, 500);

            // Execute
            let result = store_analytics_record(&pool, &metrics, &auth).await;
            // Analytics record should still be created successfully
            assert!(result.is_ok(), "Analytics record should be created even if credit deduction fails");

            // Verify: Balance should go negative as used more than available
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            let final_balance = credits.get_user_balance(user_id).await.unwrap();

            // Expected cost is rounded to 2 decimal places (0.025 -> 0.02)
            let expected_cost = Decimal::from_str("0.02").unwrap();
            let expected_balance = initial_balance - expected_cost;
            assert_eq!(final_balance, expected_balance, "Balance should reflect overdraft");
            // Verify: Usage transaction was created
            let transactions = credits.list_user_transactions(user_id, 0, 10).await.unwrap();
            let usage_tx = transactions.iter().find(|tx| tx.transaction_type == CreditTransactionType::Usage);
            assert!(
                usage_tx.is_some(),
                "Usage transaction should be created even with insufficient balance"
            );
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_cost_calculation_precision(pool: sqlx::PgPool) {
            // Setup: User with balance
            let user_id = setup_user_with_balance(&pool, Decimal::from_str("10.00").unwrap()).await;
            let auth = create_test_auth_for_user(&pool, user_id).await;

            // Test various token counts and pricing scenarios
            // Note: Expected costs are rounded to 2 decimal places (banker's rounding)
            let test_cases = vec![
                // (input_tokens, output_tokens, input_price_per_token, output_price_per_token, expected_cost_rounded)
                (1500, 750, "0.00001", "0.00003", "0.04"), // (1500*0.00001) + (750*0.00003) = 0.0375 -> rounds to 0.04
                (100, 50, "0.000005", "0.000015", "0.00"), // (100*0.000005) + (50*0.000015) = 0.00125 -> rounds to 0.00
                (1, 1, "0.01", "0.02", "0.03"),            // (1*0.01) + (1*0.02) = 0.03 (exact)
            ];

            for (input_tokens, output_tokens, input_price_str, output_price_str, expected_cost_str) in test_cases {
                let input_price = Decimal::from_str(input_price_str).unwrap();
                let output_price = Decimal::from_str(output_price_str).unwrap();
                let expected_cost = Decimal::from_str(expected_cost_str).unwrap();

                let metrics = create_test_usage_metrics(Some(input_price), Some(output_price), input_tokens, output_tokens);

                let balance_before = {
                    let mut conn = pool.acquire().await.unwrap();
                    let mut credits = Credits::new(&mut conn);
                    credits.get_user_balance(user_id).await.unwrap()
                };

                let result = store_analytics_record(&pool, &metrics, &auth).await;
                assert!(result.is_ok(), "store_analytics_record should succeed");

                let balance_after = {
                    let mut conn = pool.acquire().await.unwrap();
                    let mut credits = Credits::new(&mut conn);
                    credits.get_user_balance(user_id).await.unwrap()
                };

                let actual_cost = balance_before - balance_after;
                assert_eq!(
                    actual_cost, expected_cost,
                    "Cost calculation should be precise for tokens={}/{}, prices={}/{}",
                    input_tokens, output_tokens, input_price_str, output_price_str
                );
            }
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_skip_deduction_when_model_missing(pool: sqlx::PgPool) {
            // Setup: User with balance
            let user_id = setup_user_with_balance(&pool, Decimal::from_str("10.00").unwrap()).await;
            let auth = create_test_auth_for_user(&pool, user_id).await;

            // Create metrics with pricing but no model
            let mut metrics = create_test_usage_metrics(
                Some(Decimal::from_str("0.00001").unwrap()),
                Some(Decimal::from_str("0.00003").unwrap()),
                1000,
                500,
            );
            metrics.request_model = None; // Remove model

            // Execute
            let result = store_analytics_record(&pool, &metrics, &auth).await;
            assert!(result.is_ok(), "Should succeed even without model");

            // Verify: Balance should not change
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            let balance = credits.get_user_balance(user_id).await.unwrap();
            assert_eq!(
                balance,
                Decimal::from_str("10.00").unwrap(),
                "Balance should not change without model"
            );
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_playground_users_not_charged(pool: sqlx::PgPool) {
            // Setup: User with balance (accessed via Playground/SSO)
            let initial_balance = Decimal::from_str("10.00").unwrap();
            let user = create_test_user(&pool, Role::StandardUser).await;
            let user_id = user.id;

            // Grant initial balance
            let mut conn = pool.acquire().await.expect("Failed to acquire connection");
            let mut credits = Credits::new(&mut conn);
            credits
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id,
                    transaction_type: CreditTransactionType::AdminGrant,
                    amount: initial_balance,
                    source_id: "test_setup".to_string(),
                    description: Some("Initial test balance".to_string()),
                })
                .await
                .expect("Failed to create initial balance");

            // Create Playground auth (SSO via X-Doubleword-User header)
            let auth = Auth::Playground {
                user_email: user.email.clone(),
            };

            // Create metrics with pricing and tokens (would normally cost money)
            let input_price = Decimal::from_str("0.00001").unwrap();
            let output_price = Decimal::from_str("0.00003").unwrap();
            let metrics = create_test_usage_metrics(Some(input_price), Some(output_price), 1000, 500);

            // Execute
            let result = store_analytics_record(&pool, &metrics, &auth).await;
            assert!(result.is_ok(), "Analytics record should be created");

            // Verify: Balance should NOT change (Playground users aren't charged)
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            let final_balance = credits.get_user_balance(user_id).await.unwrap();
            assert_eq!(
                final_balance, initial_balance,
                "Playground users should not be charged for API usage"
            );

            // Verify: No Usage transaction created
            let transactions = credits.list_user_transactions(user_id, 0, 10).await.unwrap();
            let usage_tx = transactions.iter().find(|tx| tx.transaction_type == CreditTransactionType::Usage);
            assert!(usage_tx.is_none(), "No Usage transaction should be created for Playground users");

            // Verify: Analytics record WAS created (tracking, just not billing)
            let analytics_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM http_analytics WHERE user_id = $1")
                .bind(user_id)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(analytics_count, 1, "Analytics record should be created for tracking");
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_concurrent_requests_with_consistent_pricing(pool: sqlx::PgPool) {
            // Test that multiple concurrent requests are each charged according to their
            // own captured pricing, even if pricing changes between requests

            let user_id = setup_user_with_balance(&pool, Decimal::from_str("100.00").unwrap()).await;
            let auth = create_test_auth_for_user(&pool, user_id).await;

            // Spawn 10 concurrent requests with DIFFERENT pricing
            // (simulating pricing changes between requests being processed)
            let mut handles = vec![];

            for i in 0..10 {
                let pool_clone = pool.clone();
                let auth_clone = auth.clone();

                // Each request has different pricing that results in distinct costs after rounding to 2 decimals
                // Use prices that yield costs like: 0.01, 0.02, 0.03, ..., 0.10 (all different after rounding)
                // For 100 input tokens, we want total costs of 0.01, 0.02, etc.
                let target_cost = (i + 1) as i64; // 1, 2, 3, ..., 10
                let input_price = Decimal::new(target_cost, 4); // 0.0001, 0.0002, ..., 0.0010
                let output_price = Decimal::ZERO; // Only charge for input to keep it simple

                let handle = tokio::task::spawn(async move {
                    let metrics = create_test_usage_metrics(Some(input_price), Some(output_price), 100, 0);
                    store_analytics_record(&pool_clone, &metrics, &auth_clone).await
                });

                handles.push(handle);
            }

            // Wait for all requests to complete
            for handle in handles {
                handle.await.expect("Task panicked").expect("store_analytics_record failed");
            }

            // Verify: All transactions were created successfully
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            let transactions = credits.list_user_transactions(user_id, 0, 20).await.unwrap();

            let usage_transactions: Vec<_> = transactions
                .iter()
                .filter(|tx| tx.transaction_type == CreditTransactionType::Usage)
                .collect();

            assert_eq!(usage_transactions.len(), 10, "Should have 10 usage transactions, one per request");

            // Verify: Each transaction has different amount (because pricing was different)
            let mut amounts: Vec<_> = usage_transactions.iter().map(|tx| tx.amount).collect();
            amounts.sort();
            amounts.dedup();

            assert_eq!(amounts.len(), 10, "Each request should have been charged at its own pricing");

            // Verify: Final balance is correct (sum of all deductions)
            let final_balance = credits.get_user_balance(user_id).await.unwrap();
            let total_deducted = Decimal::from_str("100.00").unwrap() - final_balance;

            assert!(
                final_balance < Decimal::from_str("100.00").unwrap(),
                "Balance should have decreased"
            );
            assert!(total_deducted > Decimal::ZERO, "Some credits should have been deducted");

            // With 10 requests at ~0.00010-0.00029 per token, 100 input + 50 output tokens each
            // Rough calculation: avg price ~0.00015 input, ~0.00025 output
            // Per request: (100 * 0.00015) + (50 * 0.00025) = 0.015 + 0.0125 = 0.0275
            // Total for 10: ~0.275
            // Allow reasonable range
            assert!(
                total_deducted < Decimal::from_str("1.0").unwrap(),
                "Total deducted ({}) should be less than $1.00",
                total_deducted
            );
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_credit_deduction_with_only_input_price(pool: sqlx::PgPool) {
            // Test that cost is calculated correctly when only input price is set
            let user_id = setup_user_with_balance(&pool, Decimal::from_str("10.00").unwrap()).await;
            let auth = create_test_auth_for_user(&pool, user_id).await;

            // Set only input price, output price is None
            let input_price = Decimal::from_str("0.00001").unwrap();
            let metrics = create_test_usage_metrics(Some(input_price), None, 1000, 500);

            let result = store_analytics_record(&pool, &metrics, &auth).await;
            assert!(result.is_ok(), "store_analytics_record should succeed");

            // Verify credit deduction
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            let final_balance = credits.get_user_balance(user_id).await.unwrap();

            // Expected cost: only input tokens counted: 1000 * 0.00001 = 0.01
            // Output tokens (500) should be ignored since no output price
            let expected_cost = Decimal::from_str("0.01").unwrap();
            let expected_balance = Decimal::from_str("10.00").unwrap() - expected_cost;
            assert_eq!(final_balance, expected_balance, "Balance should only deduct for input tokens");

            // Verify transaction amount
            let transactions = credits.list_user_transactions(user_id, 0, 10).await.unwrap();
            let usage_tx = transactions
                .iter()
                .find(|tx| tx.transaction_type == CreditTransactionType::Usage)
                .expect("Usage transaction should exist");

            assert_eq!(usage_tx.amount, expected_cost, "Transaction should only charge for input tokens");
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_credit_deduction_with_only_output_price(pool: sqlx::PgPool) {
            // Test that cost is calculated correctly when only output price is set
            let user_id = setup_user_with_balance(&pool, Decimal::from_str("10.00").unwrap()).await;
            let auth = create_test_auth_for_user(&pool, user_id).await;

            // Set only output price, input price is None
            let output_price = Decimal::from_str("0.00003").unwrap();
            let metrics = create_test_usage_metrics(None, Some(output_price), 1000, 500);

            let result = store_analytics_record(&pool, &metrics, &auth).await;
            assert!(result.is_ok(), "store_analytics_record should succeed");

            // Verify credit deduction
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            let final_balance = credits.get_user_balance(user_id).await.unwrap();

            // Expected cost: only output tokens counted: 500 * 0.00003 = 0.015, rounds to 0.02
            // Input tokens (1000) should be ignored since no input price
            let expected_cost = Decimal::from_str("0.02").unwrap();
            let expected_balance = Decimal::from_str("10.00").unwrap() - expected_cost;
            assert_eq!(final_balance, expected_balance, "Balance should only deduct for output tokens");

            // Verify transaction amount
            let transactions = credits.list_user_transactions(user_id, 0, 10).await.unwrap();
            let usage_tx = transactions
                .iter()
                .find(|tx| tx.transaction_type == CreditTransactionType::Usage)
                .expect("Usage transaction should exist");

            assert_eq!(usage_tx.amount, expected_cost, "Transaction should only charge for output tokens");
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_credit_deduction_with_no_pricing(pool: sqlx::PgPool) {
            // Test that no deduction occurs when neither price is set
            let initial_balance = Decimal::from_str("10.00").unwrap();
            let user_id = setup_user_with_balance(&pool, initial_balance).await;
            let auth = create_test_auth_for_user(&pool, user_id).await;

            // Both prices are None
            let metrics = create_test_usage_metrics(None, None, 1000, 500);

            let result = store_analytics_record(&pool, &metrics, &auth).await;
            assert!(result.is_ok(), "store_analytics_record should succeed");

            // Verify NO credit deduction occurred
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            let final_balance = credits.get_user_balance(user_id).await.unwrap();

            assert_eq!(final_balance, initial_balance, "Balance should remain unchanged with no pricing");

            // Verify NO Usage transaction was created
            let transactions = credits.list_user_transactions(user_id, 0, 10).await.unwrap();
            let usage_tx = transactions.iter().find(|tx| tx.transaction_type == CreditTransactionType::Usage);

            assert!(usage_tx.is_none(), "No Usage transaction should be created when pricing is None");

            // Verify analytics record WAS still created (for tracking)
            let analytics_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM http_analytics WHERE user_id = $1")
                .bind(user_id)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(analytics_count, 1, "Analytics record should still be created");
        }
    }
}
