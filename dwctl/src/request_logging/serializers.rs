//! Request and response serialization for AI proxy analytics.
//!
//! This module provides the serialization layer between the [outlet] request logging
//! middleware and the analytics database. It parses incoming AI requests, extracts
//! usage metrics from responses, records analytics data, and handles credit deduction.
//!
//! # Request Path
//!
//! When [outlet] intercepts an incoming request, it calls [`parse_ai_request`] to parse
//! the JSON body into an [`AiRequest`] variant (ChatCompletions, Completions, Embeddings,
//! or Other). This happens synchronously before the request is forwarded upstream.
//!
//! # Response Path
//!
//! After the upstream response completes, [outlet] calls the response serializer.
//! This is split into two phases:
//!
//! **Inline** (in the serializer closure):
//! 1. Parse response body via [`parse_ai_response`] (handles JSON, SSE streams, compression)
//! 2. Extract [`UsageMetrics`] (tokens, model, duration)
//! 3. Extract auth info from headers
//! 4. Return parsed [`AiResponse`] to outlet
//!
//! **Fire-and-forget** (spawned via `tokio::spawn`):
//! 1. Lookup API key → user_id, email
//! 2. Lookup model tariffs → price per token
//! 3. Write [`HttpAnalyticsRow`] to `http_analytics` table
//! 4. Deduct credits (if 2xx status and pricing configured)
//! 5. Record Prometheus metrics
//!
//! The spawned task runs independently - outlet doesn't wait for it.
//!
//! # Credit Deduction
//!
//! Credits are deducted based on token usage and model-specific pricing. The serializer
//! looks up the model's tariffs (input/output price per token) and creates a credit
//! transaction for each successful request. Failed requests (non-2xx status codes) do
//! not incur charges.
//!
//! [outlet]: https://github.com/doublewordai/outlet

use crate::config::Config;
use crate::db::errors::Result as DbResult;
use crate::db::handlers::Credits;
use crate::db::models::credits::{CreditTransactionCreateDBRequest, CreditTransactionType};
use crate::request_logging::models::{AiRequest, AiResponse, ChatCompletionChunk, ParsedAIRequest};
use crate::request_logging::utils::{extract_header_as_string, extract_header_as_uuid};
use metrics::{counter, histogram};
use outlet::{RequestData, ResponseData};
use outlet_postgres::SerializationError;
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde_json::Value;
use sqlx::PgPool;
use std::fmt;
use std::str;
use tracing::{Instrument, debug, error, info_span, instrument, warn};
use uuid::Uuid;

use super::utils;

/// Access source types for analytics tracking
#[derive(Clone, Debug)]
pub enum AccessSource {
    ApiKey,
    UnknownApiKey,
    Unauthenticated,
}

impl fmt::Display for AccessSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccessSource::ApiKey => write!(f, "api_key"),
            AccessSource::UnknownApiKey => write!(f, "unknown_api_key"),
            AccessSource::Unauthenticated => write!(f, "unauthenticated"),
        }
    }
}

/// Authentication information extracted from request headers
#[derive(Clone)]
pub enum Auth {
    /// API key access (Authorization: Bearer <key>)
    ApiKey { bearer_token: String },
    /// No authentication found
    None,
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
    pub fusillade_batch_id: Option<Uuid>,
    pub fusillade_request_id: Option<Uuid>,
    pub custom_id: Option<String>,
    /// Request origin: "api", "frontend", or "fusillade"
    pub request_origin: String,
    /// Batch SLA completion window: "1h", "24h", etc.
    ///
    /// This is recorded as an empty string (`""`) for non-batch requests rather than
    /// using `None`/`NULL`. The empty-string sentinel is intentional so that
    /// Prometheus metrics can be filtered with a simple `batch_sla=""` label
    /// selector, at the cost of a small increase in label cardinality.
    pub batch_sla: String,
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
#[instrument(skip_all)]
pub fn parse_ai_request(request_data: &RequestData) -> Result<ParsedAIRequest, SerializationError> {
    let headers = request_data
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.iter().map(|b| String::from_utf8_lossy(b).to_string()).collect()))
        .collect();

    let bytes = match &request_data.body {
        Some(body) => body.as_ref(),
        None => {
            return Ok({
                ParsedAIRequest {
                    headers,
                    request: AiRequest::Other(Value::Null),
                }
            });
        }
    };

    let body_str = String::from_utf8_lossy(bytes);

    if body_str.trim().is_empty() {
        return Ok({
            ParsedAIRequest {
                headers,
                request: AiRequest::Other(Value::Null),
            }
        });
    }

    match serde_json::from_str(&body_str) {
        Ok(request) => Ok(ParsedAIRequest { headers, request }),
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
#[instrument(skip_all)]
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
        Ok(parsed_request) => match parsed_request.request {
            AiRequest::ChatCompletions(chat_req) if chat_req.stream.unwrap_or(false) => utils::parse_streaming_response(&body_str),
            AiRequest::Completions(completion_req) if completion_req.stream.unwrap_or(false) => utils::parse_streaming_response(&body_str),
            _ => utils::parse_non_streaming_response(&body_str),
        },
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
    #[instrument(skip_all, name = "extract_usage_metrics")]
    pub fn extract(
        instance_id: Uuid,
        request_data: &RequestData,
        response_data: &ResponseData,
        parsed_response: &AiResponse,
        config: &Config,
    ) -> Self {
        // Extract model from request
        let request_model = match parse_ai_request(request_data) {
            Ok(parsed_request) => match parsed_request.request {
                AiRequest::ChatCompletions(req) => Some(req.model),
                AiRequest::Completions(req) => Some(req.model),
                AiRequest::Embeddings(req) => Some(req.model),
                _ => None,
            },
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
    #[instrument(skip_all, name = "extract_auth")]
    pub fn from_request(request_data: &RequestData, _config: &Config) -> Self {
        // Check for API key in Authorization header
        if let Some(auth_header) = Self::get_header_value(request_data, "authorization")
            && let Some(bearer_token) = auth_header.strip_prefix("Bearer ")
        {
            return Auth::ApiKey {
                bearer_token: bearer_token.to_string(),
            };
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
#[instrument(skip(pool, request_data))]
pub async fn store_analytics_record(
    pool: &PgPool,
    metrics: &UsageMetrics,
    auth: &Auth,
    request_data: &RequestData,
) -> DbResult<HttpAnalyticsRow> {
    // Extract fusillade ID headers if present
    let fusillade_batch_id = extract_header_as_uuid(request_data, "x-fusillade-batch-id");
    let fusillade_request_id = extract_header_as_uuid(request_data, "x-fusillade-request-id");
    let custom_id = extract_header_as_string(request_data, "x-fusillade-custom-id");

    // Extract batch metadata headers for tariff pricing
    let batch_created_at = extract_header_as_string(request_data, "x-fusillade-batch-created-at");
    let batch_completion_window = extract_header_as_string(request_data, "x-fusillade-batch-completion-window");

    // Parse batch created_at timestamp if available, otherwise use metrics.timestamp
    let pricing_timestamp = if let Some(created_at_str) = &batch_created_at {
        created_at_str.parse::<chrono::DateTime<chrono::Utc>>().unwrap_or_else(|e| {
            warn!(
                "Failed to parse x-fusillade-batch-created-at header '{}': {}. Falling back to metrics.timestamp",
                created_at_str, e
            );
            metrics.timestamp
        })
    } else {
        metrics.timestamp
    };

    // Extract user information and API key purpose based on auth type
    let (user_id, user_email, access_source, api_key_purpose) = match auth {
        Auth::ApiKey { bearer_token } => {
            // Try to get user ID, email, and purpose from API key
            use crate::db::handlers::api_keys::ApiKeys;
            let mut conn = pool.acquire().await?;
            {
                let mut repo = ApiKeys::new(&mut conn);
                match repo.get_user_info_by_secret(bearer_token).await? {
                    Some((user_id, email, purpose)) => (Some(user_id), Some(email), AccessSource::ApiKey, Some(purpose)),
                    None => {
                        warn!("Unknown API key used");
                        (None, None, AccessSource::UnknownApiKey, None)
                    }
                }
            }
        }
        Auth::None => (None, None, AccessSource::Unauthenticated, None),
    };

    // Get tariff pricing and provider name
    // Use request_model for lookup since that's what the user specified
    let (tariff_pricing, provider_name) = if let Some(model_name) = &metrics.request_model {
        let model_info = sqlx::query!(
            r#"
            SELECT
                dm.id as model_id,
                ie.name as "provider_name?",
                ie.url as "provider_url?"
            FROM deployed_models dm
            LEFT JOIN inference_endpoints ie ON dm.hosted_on = ie.id
            WHERE dm.alias = $1
            "#,
            model_name
        )
        .fetch_optional(pool)
        .instrument(info_span!("fetch_model_info"))
        .await?;

        if let Some(model_info) = model_info {
            // Use the tariff repository to get pricing at timestamp
            use crate::db::handlers::Tariffs;
            let mut conn = pool.acquire().await?;
            let mut tariffs_repo = Tariffs::new(&mut conn);

            let tariff_pricing = tariffs_repo
                .get_pricing_at_timestamp_with_fallback(
                    model_info.model_id,
                    api_key_purpose.as_ref(),
                    &crate::db::models::api_keys::ApiKeyPurpose::Realtime,
                    pricing_timestamp,
                    batch_completion_window.as_deref(),
                )
                .await?;

            let provider_name = model_info
                .provider_url
                .as_ref()
                .and_then(|url| map_url_to_otel_provider(url))
                .map(|s| s.to_string())
                .or(model_info.provider_name);

            debug!(
                "Tariff pricing for model '{}' with purpose '{:?}': {:?}",
                model_name, api_key_purpose, tariff_pricing
            );

            (tariff_pricing, provider_name)
        } else {
            // Model exists in request but not found in deployed_models
            // Log analytics without pricing to preserve request data
            warn!(
                model_name = %model_name,
                "Model not found in deployed_models table - logging analytics without pricing"
            );
            (None, None)
        }
    } else {
        (None, None)
    };

    // Use tariff pricing if available
    let (input_price, output_price) = tariff_pricing
        .map(|(input, output)| (Some(input), Some(output)))
        .unwrap_or((None, None));

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
        input_price_per_token: input_price,
        output_price_per_token: output_price,
        server_address: metrics.server_address.clone(),
        server_port: metrics.server_port,
        provider_name,
        fusillade_batch_id,
        fusillade_request_id,
        custom_id,
        request_origin: match (&api_key_purpose, &fusillade_batch_id) {
            // Any explicit fusillade batch ID takes precedence
            (_, Some(_)) => "fusillade".to_string(),
            // Batch API keys without an explicit fusillade_batch_id are still considered fusillade
            (Some(crate::db::models::api_keys::ApiKeyPurpose::Batch), None) => "fusillade".to_string(),
            // Playground keys map to the frontend origin
            (Some(crate::db::models::api_keys::ApiKeyPurpose::Playground), _) => "frontend".to_string(),
            // Everything else is treated as generic API usage
            _ => "api".to_string(),
        },
        batch_sla: batch_completion_window.clone().unwrap_or_default(),
    };

    // Insert the analytics record and get the ID
    let analytics_id = sqlx::query_scalar!(
        r#"
        INSERT INTO http_analytics (
            instance_id, correlation_id, timestamp, method, uri, model,
            status_code, duration_ms, duration_to_first_byte_ms, prompt_tokens, completion_tokens,
            total_tokens, response_type, user_id, user_email, access_source,
            input_price_per_token, output_price_per_token, fusillade_batch_id, fusillade_request_id, custom_id,
            request_origin, batch_sla
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23)
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
            output_price_per_token = EXCLUDED.output_price_per_token,
            fusillade_batch_id = EXCLUDED.fusillade_batch_id,
            fusillade_request_id = EXCLUDED.fusillade_request_id,
            custom_id = EXCLUDED.custom_id,
            request_origin = EXCLUDED.request_origin,
            batch_sla = EXCLUDED.batch_sla
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
        row.output_price_per_token,
        row.fusillade_batch_id,
        row.fusillade_request_id,
        row.custom_id,
        row.request_origin,
        row.batch_sla
    )
    .fetch_one(pool)
    .instrument(info_span!("insert_http_analytics"))
    .await?;

    // =======================================================================
    // Deduct credits for API usage if applicable
    // =======================================================================
    // Deduct credits if applicable (user_id present and at least one price is set)
    if let (Some(user_id), Some(model)) = (row.user_id, &row.request_model) {
        tracing::trace!(
            user_id = %user_id,
            model = %model,
            "Checking if credit deduction is applicable"
        );

        // skip deduction if originated from system user
        if user_id != Uuid::nil() {
            tracing::trace!(user_id = %user_id, "User is not system user, checking pricing");

            // Check if at least one price is set
            if row.input_price_per_token.is_some() || row.output_price_per_token.is_some() {
                // Calculate total cost - use zero for missing prices
                let input_cost = Decimal::from(row.prompt_tokens) * row.input_price_per_token.unwrap_or(rust_decimal::Decimal::ZERO);
                let output_cost = Decimal::from(row.completion_tokens) * row.output_price_per_token.unwrap_or(rust_decimal::Decimal::ZERO);
                let total_cost = input_cost + output_cost;

                tracing::debug!(
                    user_id = %user_id,
                    model = %model,
                    prompt_tokens = row.prompt_tokens,
                    completion_tokens = row.completion_tokens,
                    input_price_per_token = ?row.input_price_per_token,
                    output_price_per_token = ?row.output_price_per_token,
                    input_cost = %input_cost,
                    output_cost = %output_cost,
                    total_cost = %total_cost,
                    "Calculated API usage cost"
                );

                if total_cost > rust_decimal::Decimal::ZERO {
                    tracing::trace!(
                        user_id = %user_id,
                        total_cost = %total_cost,
                        "Cost is greater than zero, proceeding with credit deduction"
                    );

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
                            // Include fusillade_batch_id to enable fast batch grouping in transactions list
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
                                    fusillade_batch_id: row.fusillade_batch_id,
                                })
                                .await
                            {
                                Ok(result) => {
                                    debug!(
                                        user_id = %user_id,
                                        transaction_id = %result.id,
                                        amount = %total_cost,
                                        model = %model,
                                        "Credits deducted for API usage"
                                    );
                                    let cents = (total_cost.to_f64().unwrap_or(0.0) * 100.0).round() as u64;
                                    counter!(
                                        "dwctl_credits_deducted_total",
                                        "user_id" => user_id.to_string(),
                                        "model" => model.to_string()
                                    )
                                    .increment(cents);
                                }
                                Err(e) => {
                                    tracing::error!(
                                        error = %e,
                                        correlation_id = %row.correlation_id,
                                        user_id = %user_id,
                                        "Failed to create credit transaction for API usage"
                                    );
                                    counter!("dwctl_credits_deduction_errors_total").increment(1);
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
                            counter!("dwctl_credits_deduction_errors_total").increment(1);
                        }
                    }
                } else {
                    tracing::trace!(
                        user_id = %user_id,
                        total_cost = %total_cost,
                        "Skipping credit deduction: cost is zero"
                    );
                }
            } else {
                tracing::trace!(
                    user_id = %user_id,
                    model = %model,
                    "Skipping credit deduction: no pricing configured"
                );
            }
        } else {
            tracing::trace!(
                user_id = %user_id,
                "Skipping credit deduction: system user (UUID::nil)"
            );
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
    ///
    /// # Analytics Storage
    /// Analytics are stored for ALL responses, including error responses (4xx, 5xx).
    /// Even if the response cannot be parsed as a valid AI response, the request
    /// metadata (status code, duration, model, user, etc.) is still recorded.
    pub fn create_serializer(self) -> impl Fn(&RequestData, &ResponseData) -> Result<AiResponse, SerializationError> + Send + Sync {
        move |request_data: &RequestData, response_data: &ResponseData| {
            let serializer_span = info_span!(
                "response_serializer",
                correlation_id = request_data.correlation_id,
                status = %response_data.status
            );
            let _guard = serializer_span.enter();

            // Try to parse the response - may fail for error responses (4xx, 5xx)
            let parse_result = parse_ai_response(request_data, response_data);

            // Use parsed response for metrics, or fallback to Other for error responses
            let metrics_response = match &parse_result {
                Ok(response) => response.clone(),
                Err(_) => AiResponse::Other(Value::Null),
            };

            // Basic metrics - extracted regardless of parse success
            // This captures status_code, duration, model from request, etc.
            let metrics = UsageMetrics::extract(self.instance_id, request_data, response_data, &metrics_response, &self.config);

            // Auth information
            let auth = Auth::from_request(request_data, &self.config);

            // Clone data for async processing
            let pool_clone = self.pool.clone();
            let metrics_recorder_clone = self.metrics_recorder.clone();
            let request_data_clone = request_data.clone();
            let correlation_id = request_data.correlation_id;

            // The write to the analytics table and metrics recording
            // This runs for ALL responses, including errors
            let async_span = info_span!("analytics_storage", correlation_id = correlation_id);
            tokio::spawn(
                async move {
                    // Store to database - this enriches with user/pricing data and returns complete row
                    let result = store_analytics_record(&pool_clone, &metrics, &auth, &request_data_clone).await;

                    // Record analytics processing lag regardless of success/failure
                    // This measures time from response completion to storage attempt completion
                    let total_ms = chrono::Utc::now().signed_duration_since(metrics.timestamp).num_milliseconds();
                    let lag_ms = total_ms - metrics.duration_ms;
                    histogram!("dwctl_analytics_lag_seconds").record(lag_ms as f64 / 1000.0);

                    match result {
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
                }
                .instrument(async_span),
            );

            // Return the parse result - outlet-postgres will handle SerializationError
            // by storing the fallback_data (base64 encoded response)
            parse_result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{UsageMetrics, parse_ai_request, parse_ai_response};
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

        match result.request {
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

        match result.request {
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

        match result.request {
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

        match result.request {
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

        match result.request {
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
            &crate::test::utils::create_test_config(),
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
            &crate::test::utils::create_test_config(),
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
            &crate::test::utils::create_test_config(),
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
            &crate::test::utils::create_test_config(),
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
            &crate::test::utils::create_test_config(),
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
            &crate::test::utils::create_test_config(),
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

    // ===== Fusillade Request ID Tests =====
    // Tests for extracting and serializing the X-Fusillade-Request-Id header
    // Note: Unit tests for extract_fusillade_request_id are in utils.rs

    // ===== Credit Deduction Tests =====
    // Tests for the credit deduction functionality in store_analytics_record

    mod credit_deduction_tests {
        use super::super::*;
        use crate::api::models::transactions::TransactionFilters;
        use crate::api::models::users::Role;
        use crate::db::handlers::Repository;
        use crate::db::handlers::api_keys::ApiKeys;
        use crate::db::handlers::credits::Credits;
        use crate::db::models::api_keys::{ApiKeyCreateDBRequest, ApiKeyPurpose};
        use crate::db::models::credits::{CreditTransactionCreateDBRequest, CreditTransactionType};
        use crate::db::models::tariffs::TariffCreateDBRequest;
        use crate::test::utils::create_test_user;
        use crate::types::DeploymentId;
        use rust_decimal::Decimal;
        use std::str::FromStr;
        use uuid::Uuid;

        /// Fixture: Create a test model with endpoint (no tariff)
        /// Returns the model's deployment ID
        async fn create_test_model(pool: &sqlx::PgPool, model_name: &str) -> DeploymentId {
            use crate::db::handlers::{Deployments, InferenceEndpoints};
            use crate::db::models::{deployments::DeploymentCreateDBRequest, inference_endpoints::InferenceEndpointCreateDBRequest};

            let user = create_test_user(pool, Role::StandardUser).await;

            // Create endpoint
            let mut conn = pool.acquire().await.unwrap();
            let mut endpoints_repo = InferenceEndpoints::new(&mut conn);
            let endpoint = endpoints_repo
                .create(&InferenceEndpointCreateDBRequest {
                    created_by: user.id,
                    name: format!("test-endpoint-{}", Uuid::new_v4()),
                    description: None,
                    url: url::Url::from_str("https://api.test.com").unwrap(),
                    api_key: None,
                    model_filter: None,
                    auth_header_name: Some("Authorization".to_string()),
                    auth_header_prefix: Some("Bearer ".to_string()),
                })
                .await
                .unwrap();

            // Create deployment
            let mut conn = pool.acquire().await.unwrap();
            let mut deployments_repo = Deployments::new(&mut conn);
            let deployment = deployments_repo
                .create(&DeploymentCreateDBRequest {
                    created_by: user.id,
                    model_name: model_name.to_string(),
                    alias: model_name.to_string(),
                    description: None,
                    model_type: None,
                    capabilities: None,
                    hosted_on: Some(endpoint.id),
                    requests_per_second: None,
                    burst_size: None,
                    capacity: None,
                    batch_capacity: None,
                    provider_pricing: None,
                    // Composite model fields (regular model = not composite)
                    is_composite: false,
                    lb_strategy: None,
                    fallback_enabled: None,
                    fallback_on_rate_limit: None,
                    fallback_on_status: None,
                    sanitize_responses: true,
                })
                .await
                .unwrap();

            deployment.id
        }

        /// Helper: Create or replace a tariff for a model
        /// Takes model ID directly, no name lookups
        ///
        /// # Arguments
        /// * `api_key_purpose` - Defaults to Realtime if not specified
        /// * `valid_from` - Defaults to NOW if not specified
        #[allow(clippy::too_many_arguments)]
        async fn setup_tariff(
            pool: &sqlx::PgPool,
            deployed_model_id: DeploymentId,
            tariff_name: &str,
            input_price_per_token: Decimal,
            output_price_per_token: Decimal,
            api_key_purpose: Option<ApiKeyPurpose>,
            valid_from: Option<chrono::DateTime<chrono::Utc>>,
        ) {
            use crate::db::handlers::Tariffs;

            // Default to Realtime if not specified
            let purpose = api_key_purpose.or(Some(ApiKeyPurpose::Realtime));

            let mut conn = pool.acquire().await.unwrap();
            let mut tariffs_repo = Tariffs::new(&mut conn);

            // Close existing tariffs for this model to avoid duplicates
            let current_tariffs = tariffs_repo.list_current_by_model(deployed_model_id).await.unwrap();
            let tariff_ids: Vec<_> = current_tariffs
                .into_iter()
                .filter(|t| t.name == tariff_name && t.api_key_purpose == purpose)
                .map(|t| t.id)
                .collect();

            if !tariff_ids.is_empty() {
                tariffs_repo.close_tariffs_batch(&tariff_ids).await.unwrap();
            }

            // Default completion_window to "24h" for batch tariffs (required by DB constraint)
            let completion_window = if purpose == Some(ApiKeyPurpose::Batch) {
                Some("24h".to_string())
            } else {
                None
            };

            let tariff = TariffCreateDBRequest {
                deployed_model_id,
                name: tariff_name.to_string(),
                input_price_per_token,
                output_price_per_token,
                api_key_purpose: purpose,
                completion_window,
                valid_from,
            };
            tariffs_repo.create(&tariff).await.unwrap();
        }

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
                        fusillade_batch_id: None,
                    })
                    .await
                    .expect("Failed to create initial balance");
            }

            user_id
        }

        /// Helper: Create test UsageMetrics for testing
        fn create_test_usage_metrics(prompt_tokens: i64, completion_tokens: i64) -> UsageMetrics {
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
            }
        }

        /// Helper: Create dummy RequestData for tests
        fn create_test_request_data() -> RequestData {
            create_test_request_data_with_headers(None)
        }

        fn create_test_request_data_with_headers(fusillade_request_id: Option<Uuid>) -> RequestData {
            use bytes::Bytes;
            use std::collections::HashMap;
            let mut headers = HashMap::new();

            if let Some(request_id) = fusillade_request_id {
                headers.insert("x-fusillade-request-id".to_string(), vec![Bytes::from(request_id.to_string())]);
            }

            RequestData {
                correlation_id: 12345,
                timestamp: std::time::SystemTime::now(),
                method: axum::http::Method::POST,
                uri: "/v1/chat/completions".parse().unwrap(),
                headers,
                body: None,
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
                    purpose: ApiKeyPurpose::Realtime,
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
            use crate::api::models::transactions::TransactionFilters;

            // Setup: Create test model and tariff
            let model_id = create_test_model(&pool, "gpt-4").await;
            let input_price = Decimal::from_str("0.00001").unwrap();
            let output_price = Decimal::from_str("0.00003").unwrap();
            setup_tariff(&pool, model_id, "batch", input_price, output_price, None, None).await;

            // Setup: User with $10.00 balance
            let initial_balance = Decimal::from_str("10.00").unwrap();
            let user_id = setup_user_with_balance(&pool, initial_balance).await;
            let auth = create_test_auth_for_user(&pool, user_id).await;

            // Usage: 1000 input tokens, 500 output tokens
            // Calculated cost: (1000 * 0.00001) + (500 * 0.00003) = 0.01 + 0.015 = 0.025
            let metrics = create_test_usage_metrics(1000, 500);

            // Execute
            let request_data = create_test_request_data();
            let result = store_analytics_record(&pool, &metrics, &auth, &request_data).await;
            assert!(result.is_ok(), "store_analytics_record should succeed");

            // Verify: Balance should be deducted
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            let final_balance = credits.get_user_balance(user_id).await.unwrap();

            // Expected cost with full precision (DECIMAL(12, 8))
            let expected_cost = Decimal::from_str("0.025").unwrap();
            let expected_balance = initial_balance - expected_cost;
            assert_eq!(final_balance, expected_balance, "Balance should be deducted correctly");

            // Verify: Transaction was created
            let transactions = credits
                .list_user_transactions(user_id, 0, 10, &TransactionFilters::default())
                .await
                .unwrap();
            let usage_tx = transactions.iter().find(|tx| tx.transaction_type == CreditTransactionType::Usage);
            assert!(usage_tx.is_some(), "Usage transaction should be created");

            let usage_tx = usage_tx.unwrap();
            assert_eq!(usage_tx.amount, expected_cost, "Transaction amount should preserve full precision");
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_skip_deduction_when_user_id_none(pool: sqlx::PgPool) {
            // Setup: Model with tariff pricing
            let model_id = create_test_model(&pool, "gpt-4").await;
            setup_tariff(
                &pool,
                model_id,
                "batch",
                Decimal::from_str("0.00001").unwrap(),
                Decimal::from_str("0.00003").unwrap(),
                None,
                None,
            )
            .await;

            // Setup: Auth::None (no user)
            let auth = Auth::None;
            let metrics = create_test_usage_metrics(1000, 500);

            // Execute
            let request_data = create_test_request_data();
            let result = store_analytics_record(&pool, &metrics, &auth, &request_data).await;
            assert!(result.is_ok(), "Should succeed even without user_id");

            // Verify: No transactions should be created
            // (We can't check this directly without querying all transactions, but the test passing means no error occurred)
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_skip_deduction_when_cost_zero(pool: sqlx::PgPool) {
            // Setup: Model with tariff pricing
            let model_id = create_test_model(&pool, "gpt-4").await;
            setup_tariff(
                &pool,
                model_id,
                "batch",
                Decimal::from_str("0.00001").unwrap(),
                Decimal::from_str("0.00003").unwrap(),
                None,
                None,
            )
            .await;

            // Setup: User with balance, but zero tokens
            let user_id = setup_user_with_balance(&pool, Decimal::from_str("10.00").unwrap()).await;
            let auth = create_test_auth_for_user(&pool, user_id).await;
            let metrics = create_test_usage_metrics(
                0, // Zero tokens
                0,
            );

            // Execute
            let request_data = create_test_request_data();
            let result = store_analytics_record(&pool, &metrics, &auth, &request_data).await;
            assert!(result.is_ok(), "Should succeed with zero cost");

            // Verify: Balance should not change (no Usage transaction)
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            let balance = credits.get_user_balance(user_id).await.unwrap();
            assert_eq!(balance, Decimal::from_str("10.00").unwrap(), "Balance should not change");

            let transactions = credits
                .list_user_transactions(user_id, 0, 10, &TransactionFilters::default())
                .await
                .unwrap();
            let usage_tx = transactions.iter().find(|tx| tx.transaction_type == CreditTransactionType::Usage);
            assert!(usage_tx.is_none(), "No Usage transaction should be created for zero cost");
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_insufficient_balance_still_accrues_usage_transaction(pool: sqlx::PgPool) {
            // Setup: Model with tariff pricing
            let input_price = Decimal::from_str("0.00001").unwrap();
            let output_price = Decimal::from_str("0.00003").unwrap();
            let model_id = create_test_model(&pool, "gpt-4").await;
            setup_tariff(&pool, model_id, "batch", input_price, output_price, None, None).await;

            // Setup: User with insufficient balance (0.01)
            let initial_balance = Decimal::from_str("0.01").unwrap();
            let user_id = setup_user_with_balance(&pool, initial_balance).await;
            let auth = create_test_auth_for_user(&pool, user_id).await;

            // Usage that costs more than balance (0.025 calculated)
            let metrics = create_test_usage_metrics(1000, 500);

            // Execute
            let request_data = create_test_request_data();
            let result = store_analytics_record(&pool, &metrics, &auth, &request_data).await;
            // Analytics record should still be created successfully
            assert!(result.is_ok(), "Analytics record should be created even if credit deduction fails");

            // Verify: Balance should go negative as used more than available
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            let final_balance = credits.get_user_balance(user_id).await.unwrap();

            // Expected cost with full precision
            let expected_cost = Decimal::from_str("0.025").unwrap();
            let expected_balance = initial_balance - expected_cost;
            assert_eq!(final_balance, expected_balance, "Balance should reflect overdraft");
            // Verify: Usage transaction was created
            let transactions = credits
                .list_user_transactions(user_id, 0, 10, &TransactionFilters::default())
                .await
                .unwrap();
            let usage_tx = transactions.iter().find(|tx| tx.transaction_type == CreditTransactionType::Usage);
            assert!(
                usage_tx.is_some(),
                "Usage transaction should be created even with insufficient balance"
            );
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_cost_calculation_precision(pool: sqlx::PgPool) {
            // Setup: Model
            let model_id = create_test_model(&pool, "gpt-4").await;

            // Setup: User with balance
            let user_id = setup_user_with_balance(&pool, Decimal::from_str("10.00").unwrap()).await;
            let auth = create_test_auth_for_user(&pool, user_id).await;

            // Test various token counts and pricing scenarios
            // Note: Expected costs preserve full precision (DECIMAL(12, 8))
            let test_cases = vec![
                // (input_tokens, output_tokens, input_price_per_token, output_price_per_token, expected_cost)
                (1500, 750, "0.00001", "0.00003", "0.0375"), // (1500*0.00001) + (750*0.00003) = 0.0375
                (100, 50, "0.000005", "0.000015", "0.00125"), // (100*0.000005) + (50*0.000015) = 0.00125
                (1, 1, "0.01", "0.02", "0.03"),              // (1*0.01) + (1*0.02) = 0.03 (exact)
            ];

            for (input_tokens, output_tokens, input_price_str, output_price_str, expected_cost_str) in test_cases {
                // Setup model with this specific pricing
                let input_price = Decimal::from_str(input_price_str).unwrap();
                let output_price = Decimal::from_str(output_price_str).unwrap();
                setup_tariff(&pool, model_id, "batch", input_price, output_price, None, None).await;

                let expected_cost = Decimal::from_str(expected_cost_str).unwrap();

                let metrics = create_test_usage_metrics(input_tokens, output_tokens);

                let balance_before = {
                    let mut conn = pool.acquire().await.unwrap();
                    let mut credits = Credits::new(&mut conn);
                    credits.get_user_balance(user_id).await.unwrap()
                };

                let request_data = create_test_request_data();
                let result = store_analytics_record(&pool, &metrics, &auth, &request_data).await;
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
            let mut metrics = create_test_usage_metrics(1000, 500);
            metrics.request_model = None; // Remove model

            // Execute
            let request_data = create_test_request_data();
            let result = store_analytics_record(&pool, &metrics, &auth, &request_data).await;
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
        async fn test_system_user_are_not_charged(pool: sqlx::PgPool) {
            // Setup: Model with tariff pricing
            let model_id = create_test_model(&pool, "gpt-4").await;
            let input_price = Decimal::from_str("0.00001").unwrap();
            let output_price = Decimal::from_str("0.00003").unwrap();
            setup_tariff(&pool, model_id, "batch", input_price, output_price, None, None).await;

            // Setup: User with balance (accessed via Playground/SSO)
            let initial_balance = Decimal::from_str("10.00").unwrap();
            let user_id = Uuid::nil();

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
                    fusillade_batch_id: None,
                })
                .await
                .expect("Failed to create initial balance");

            // Create Playground auth (SSO via X-Doubleword-User header)
            let auth = create_test_auth_for_user(&pool, user_id).await;

            // Create metrics with pricing and tokens (would normally cost money)
            let metrics = create_test_usage_metrics(1000, 500);

            // Execute
            let request_data = create_test_request_data();
            let result = store_analytics_record(&pool, &metrics, &auth, &request_data).await;
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
            let transactions = credits
                .list_user_transactions(user_id, 0, 10, &TransactionFilters::default())
                .await
                .unwrap();
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
            // Test that multiple concurrent requests are all processed successfully
            // and charged at the same pricing (concurrent access with shared tariff)

            let model_id = create_test_model(&pool, "gpt-4").await;
            let user_id = setup_user_with_balance(&pool, Decimal::from_str("100.00").unwrap()).await;
            let auth = create_test_auth_for_user(&pool, user_id).await;

            // Setup a single tariff upfront (before spawning any tasks)
            // Use an explicit valid_from in the past to avoid timing issues
            let input_price = Decimal::from_str("0.0001").unwrap();
            let output_price = Decimal::ZERO;

            setup_tariff(
                &pool,
                model_id,
                "batch",
                input_price,
                output_price,
                Some(ApiKeyPurpose::Realtime),
                Some(chrono::Utc::now() - chrono::Duration::seconds(5)),
            )
            .await;

            // Spawn 10 concurrent requests all using the same tariff
            let mut handles = vec![];

            for i in 0..10 {
                let pool_clone = pool.clone();
                let auth_clone = auth.clone();

                let handle = tokio::task::spawn(async move {
                    // Use unique correlation_id for each request to avoid conflicts
                    let mut metrics = create_test_usage_metrics(100, 0);
                    metrics.correlation_id = 12345 + i;
                    let mut request_data = create_test_request_data();
                    request_data.correlation_id = 12345 + (i as u64);
                    store_analytics_record(&pool_clone, &metrics, &auth_clone, &request_data).await
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
            let transactions = credits
                .list_user_transactions(user_id, 0, 20, &TransactionFilters::default())
                .await
                .unwrap();

            let usage_transactions: Vec<_> = transactions
                .iter()
                .filter(|tx| tx.transaction_type == CreditTransactionType::Usage)
                .collect();

            assert_eq!(usage_transactions.len(), 10, "Should have 10 usage transactions, one per request");

            // Verify: All transactions have the same amount (consistent pricing)
            let amounts: Vec<_> = usage_transactions.iter().map(|tx| tx.amount).collect();
            let expected_cost = Decimal::new(1, 2); // 100 tokens * 0.0001 = 0.01
            for amount in &amounts {
                assert_eq!(amount, &expected_cost, "All requests should be charged at the same rate");
            }

            // Debug: Print all transactions to see the chain
            println!("\n=== All Transactions (most recent first) ===");
            for tx in &transactions {
                println!("  {:?} | amount: {}", tx.transaction_type, tx.amount);
            }

            // Verify: Final balance is correct (10 requests * 0.01 each = 0.10 total)
            let final_balance = credits.get_user_balance(user_id).await.unwrap();
            let expected_final = Decimal::from_str("99.90").unwrap(); // 100.00 - 0.10

            // Debug: Show what we got vs what we expected
            println!("\nFinal balance: {} (expected: {})", final_balance, expected_final);
            println!("Total transactions: {}", transactions.len());
            println!("Usage transactions: {}", usage_transactions.len());

            assert_eq!(
                final_balance, expected_final,
                "Balance should be 99.90 after 10 requests at 0.01 each"
            );
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_credit_deduction_with_only_input_price(pool: sqlx::PgPool) {
            // Test that cost is calculated correctly when only input price is set
            let model_id = create_test_model(&pool, "gpt-4").await;
            let input_price = Decimal::from_str("0.00001").unwrap();
            setup_tariff(&pool, model_id, "batch", input_price, Decimal::ZERO, None, None).await;

            let user_id = setup_user_with_balance(&pool, Decimal::from_str("10.00").unwrap()).await;
            let auth = create_test_auth_for_user(&pool, user_id).await;

            let metrics = create_test_usage_metrics(1000, 500);

            let request_data = create_test_request_data();
            let result = store_analytics_record(&pool, &metrics, &auth, &request_data).await;
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
            let transactions = credits
                .list_user_transactions(user_id, 0, 10, &TransactionFilters::default())
                .await
                .unwrap();
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
            let model_id = create_test_model(&pool, "gpt-4").await;
            let output_price = Decimal::from_str("0.00003").unwrap();
            setup_tariff(&pool, model_id, "batch", Decimal::ZERO, output_price, None, None).await;

            let user_id = setup_user_with_balance(&pool, Decimal::from_str("10.00").unwrap()).await;
            let auth = create_test_auth_for_user(&pool, user_id).await;

            let metrics = create_test_usage_metrics(1000, 500);

            let request_data = create_test_request_data();
            let result = store_analytics_record(&pool, &metrics, &auth, &request_data).await;
            assert!(result.is_ok(), "store_analytics_record should succeed");

            // Verify credit deduction
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            let final_balance = credits.get_user_balance(user_id).await.unwrap();

            // Expected cost: only output tokens counted: 500 * 0.00003 = 0.015
            // Input tokens (1000) should be ignored since no input price
            let expected_cost = Decimal::from_str("0.015").unwrap();
            let expected_balance = Decimal::from_str("10.00").unwrap() - expected_cost;
            assert_eq!(final_balance, expected_balance, "Balance should only deduct for output tokens");

            // Verify transaction amount
            let transactions = credits
                .list_user_transactions(user_id, 0, 10, &TransactionFilters::default())
                .await
                .unwrap();
            let usage_tx = transactions
                .iter()
                .find(|tx| tx.transaction_type == CreditTransactionType::Usage)
                .expect("Usage transaction should exist");

            assert_eq!(usage_tx.amount, expected_cost, "Transaction should only charge for output tokens");
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_credit_deduction_with_no_pricing(pool: sqlx::PgPool) {
            // Test that no deduction occurs when neither price is set (both zero)
            let model_id = create_test_model(&pool, "gpt-4").await;
            setup_tariff(&pool, model_id, "batch", Decimal::ZERO, Decimal::ZERO, None, None).await;

            let initial_balance = Decimal::from_str("10.00").unwrap();
            let user_id = setup_user_with_balance(&pool, initial_balance).await;
            let auth = create_test_auth_for_user(&pool, user_id).await;

            let metrics = create_test_usage_metrics(1000, 500);

            let request_data = create_test_request_data();
            let result = store_analytics_record(&pool, &metrics, &auth, &request_data).await;
            assert!(result.is_ok(), "store_analytics_record should succeed");

            // Verify NO credit deduction occurred
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            let final_balance = credits.get_user_balance(user_id).await.unwrap();

            assert_eq!(final_balance, initial_balance, "Balance should remain unchanged with no pricing");

            // Verify NO Usage transaction was created
            let transactions = credits
                .list_user_transactions(user_id, 0, 10, &TransactionFilters::default())
                .await
                .unwrap();
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

        #[sqlx::test]
        #[test_log::test]
        async fn test_fusillade_request_id_stored_correctly(pool: sqlx::PgPool) {
            // Setup: Model with tariff pricing
            let model_id = create_test_model(&pool, "gpt-4").await;
            let input_price = Decimal::from_str("0.00001").unwrap();
            let output_price = Decimal::from_str("0.00003").unwrap();
            setup_tariff(&pool, model_id, "batch", input_price, output_price, None, None).await;

            // Setup: User with balance
            let initial_balance = Decimal::from_str("10.00").unwrap();
            let user_id = setup_user_with_balance(&pool, initial_balance).await;
            let auth = create_test_auth_for_user(&pool, user_id).await;

            let metrics = create_test_usage_metrics(100, 50);

            // Create request with fusillade request ID
            let fusillade_request_id = Uuid::new_v4();
            let request_data = create_test_request_data_with_headers(Some(fusillade_request_id));

            // Execute
            let result = store_analytics_record(&pool, &metrics, &auth, &request_data).await;
            assert!(result.is_ok(), "store_analytics_record should succeed");

            // Verify: fusillade_request_id is stored in the analytics record
            let stored_record = sqlx::query!(
                r#"
                SELECT fusillade_request_id
                FROM http_analytics
                WHERE correlation_id = $1
                "#,
                request_data.correlation_id as i64
            )
            .fetch_one(&pool)
            .await
            .unwrap();

            assert_eq!(
                stored_record.fusillade_request_id,
                Some(fusillade_request_id),
                "Fusillade request ID should be stored in analytics record"
            );
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_fusillade_request_id_null_when_missing(pool: sqlx::PgPool) {
            // Setup: Model with tariff pricing
            let model_id = create_test_model(&pool, "gpt-4").await;
            let input_price = Decimal::from_str("0.00001").unwrap();
            let output_price = Decimal::from_str("0.00003").unwrap();
            setup_tariff(&pool, model_id, "batch", input_price, output_price, None, None).await;

            // Setup: User with balance
            let initial_balance = Decimal::from_str("10.00").unwrap();
            let user_id = setup_user_with_balance(&pool, initial_balance).await;
            let auth = create_test_auth_for_user(&pool, user_id).await;

            let metrics = create_test_usage_metrics(100, 50);

            // Create request without fusillade request ID
            let request_data = create_test_request_data_with_headers(None);

            // Execute
            let result = store_analytics_record(&pool, &metrics, &auth, &request_data).await;
            assert!(result.is_ok(), "store_analytics_record should succeed");

            // Verify: fusillade_request_id is NULL in the analytics record
            let stored_record = sqlx::query!(
                r#"
                SELECT fusillade_request_id
                FROM http_analytics
                WHERE correlation_id = $1
                "#,
                request_data.correlation_id as i64
            )
            .fetch_one(&pool)
            .await
            .unwrap();

            assert_eq!(
                stored_record.fusillade_request_id, None,
                "Fusillade request ID should be NULL when header is not present"
            );
        }

        /// Helper: Create test Auth for API key with specific purpose
        async fn create_test_auth_for_user_with_purpose(pool: &sqlx::PgPool, user_id: Uuid, purpose: ApiKeyPurpose) -> Auth {
            let mut conn = pool.acquire().await.expect("Failed to acquire connection");
            let mut api_key_repo = ApiKeys::new(&mut conn);
            let key = api_key_repo
                .create(&ApiKeyCreateDBRequest {
                    user_id,
                    name: format!("Test API key - {:?}", purpose),
                    description: None,
                    purpose,
                    requests_per_second: None,
                    burst_size: None,
                })
                .await
                .unwrap();

            Auth::ApiKey { bearer_token: key.secret }
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_tariff_different_prices_for_batch_and_realtime(pool: sqlx::PgPool) {
            // Test that a deployment can have different tariffs for batch and realtime,
            // and requests are charged at the correct price based on API key purpose

            // Setup: Create model
            let model_id = create_test_model(&pool, "gpt-4-turbo").await;

            // Setup: Create different tariffs for batch and realtime
            let batch_input_price = Decimal::from_str("0.00005").unwrap(); // $0.00005 per token
            let batch_output_price = Decimal::from_str("0.00010").unwrap(); // $0.00010 per token
            setup_tariff(
                &pool,
                model_id,
                "batch_pricing",
                batch_input_price,
                batch_output_price,
                Some(ApiKeyPurpose::Batch),
                None,
            )
            .await;

            let realtime_input_price = Decimal::from_str("0.00010").unwrap(); // $0.00010 per token (2x batch)
            let realtime_output_price = Decimal::from_str("0.00020").unwrap(); // $0.00020 per token (2x batch)
            setup_tariff(
                &pool,
                model_id,
                "realtime_pricing",
                realtime_input_price,
                realtime_output_price,
                Some(ApiKeyPurpose::Realtime),
                None,
            )
            .await;

            // Setup: User with balance
            let user_id = setup_user_with_balance(&pool, Decimal::from_str("100.00").unwrap()).await;

            // Create API keys with different purposes
            let batch_auth = create_test_auth_for_user_with_purpose(&pool, user_id, ApiKeyPurpose::Batch).await;
            let realtime_auth = create_test_auth_for_user_with_purpose(&pool, user_id, ApiKeyPurpose::Realtime).await;

            // Test batch request (1000 input tokens, 500 output tokens)
            let mut batch_metrics = create_test_usage_metrics(1000, 500);
            batch_metrics.request_model = Some("gpt-4-turbo".to_string());
            batch_metrics.response_model = Some("gpt-4-turbo".to_string());
            let batch_request_data = create_test_request_data();
            let result = store_analytics_record(&pool, &batch_metrics, &batch_auth, &batch_request_data).await;
            assert!(result.is_ok(), "Batch request should succeed");

            // Expected batch cost: (1000 * 0.00005) + (500 * 0.00010) = 0.05 + 0.05 = 0.10
            let expected_batch_cost = Decimal::from_str("0.10").unwrap();

            // Test realtime request (1000 input tokens, 500 output tokens)
            let mut realtime_metrics = create_test_usage_metrics(1000, 500);
            realtime_metrics.correlation_id = 12346; // Different correlation ID
            realtime_metrics.request_model = Some("gpt-4-turbo".to_string());
            realtime_metrics.response_model = Some("gpt-4-turbo".to_string());
            let mut realtime_request_data = create_test_request_data();
            realtime_request_data.correlation_id = 12346;
            let result = store_analytics_record(&pool, &realtime_metrics, &realtime_auth, &realtime_request_data).await;
            assert!(result.is_ok(), "Realtime request should succeed");

            // Expected realtime cost: (1000 * 0.00010) + (500 * 0.00020) = 0.10 + 0.10 = 0.20
            let expected_realtime_cost = Decimal::from_str("0.20").unwrap();

            // Verify: Check transactions
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            let transactions = credits
                .list_user_transactions(user_id, 0, 10, &TransactionFilters::default())
                .await
                .unwrap();

            let usage_transactions: Vec<_> = transactions
                .iter()
                .filter(|tx| tx.transaction_type == CreditTransactionType::Usage)
                .collect();

            assert_eq!(usage_transactions.len(), 2, "Should have 2 usage transactions");

            // Most recent transaction should be the realtime one (transactions are ordered newest first)
            assert_eq!(
                usage_transactions[0].amount, expected_realtime_cost,
                "Realtime request should be charged at realtime price"
            );
            assert_eq!(
                usage_transactions[1].amount, expected_batch_cost,
                "Batch request should be charged at batch price"
            );

            // Verify: Final balance
            let final_balance = credits.get_user_balance(user_id).await.unwrap();
            let expected_balance = Decimal::from_str("100.00").unwrap() - expected_batch_cost - expected_realtime_cost;
            assert_eq!(
                final_balance, expected_balance,
                "Balance should reflect both batch and realtime charges"
            );

            // Verify: Analytics records have correct pricing stored
            let batch_analytics = sqlx::query!(
                r#"
                SELECT input_price_per_token, output_price_per_token
                FROM http_analytics
                WHERE correlation_id = $1
                "#,
                batch_request_data.correlation_id as i64
            )
            .fetch_one(&pool)
            .await
            .unwrap();

            assert_eq!(batch_analytics.input_price_per_token.unwrap(), batch_input_price);
            assert_eq!(batch_analytics.output_price_per_token.unwrap(), batch_output_price);

            let realtime_analytics = sqlx::query!(
                r#"
                SELECT input_price_per_token, output_price_per_token
                FROM http_analytics
                WHERE correlation_id = $1
                "#,
                realtime_request_data.correlation_id as i64
            )
            .fetch_one(&pool)
            .await
            .unwrap();

            assert_eq!(realtime_analytics.input_price_per_token.unwrap(), realtime_input_price);
            assert_eq!(realtime_analytics.output_price_per_token.unwrap(), realtime_output_price);
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_tariff_update_new_requests_use_updated_pricing(pool: sqlx::PgPool) {
            // Test that when tariffs are updated, new requests use the updated pricing

            // Setup: Create model with initial tariff
            let model_id = create_test_model(&pool, "claude-3-opus").await;
            let initial_input_price = Decimal::from_str("0.00010").unwrap();
            let initial_output_price = Decimal::from_str("0.00020").unwrap();
            setup_tariff(
                &pool,
                model_id,
                "standard_pricing",
                initial_input_price,
                initial_output_price,
                Some(ApiKeyPurpose::Realtime),
                None,
            )
            .await;

            // Setup: User with balance
            let user_id = setup_user_with_balance(&pool, Decimal::from_str("100.00").unwrap()).await;
            let auth = create_test_auth_for_user(&pool, user_id).await;

            // Request 1: Use initial pricing (1000 input, 500 output)
            let mut metrics1 = create_test_usage_metrics(1000, 500);
            metrics1.request_model = Some("claude-3-opus".to_string());
            metrics1.response_model = Some("claude-3-opus".to_string());
            let request_data1 = create_test_request_data();
            let result = store_analytics_record(&pool, &metrics1, &auth, &request_data1).await;
            assert!(result.is_ok(), "First request should succeed");

            // Expected cost with initial pricing: (1000 * 0.00010) + (500 * 0.00020) = 0.10 + 0.10 = 0.20
            let expected_cost1 = Decimal::from_str("0.20").unwrap();

            // Update: Change tariff pricing (50% reduction)
            let updated_input_price = Decimal::from_str("0.00005").unwrap();
            let updated_output_price = Decimal::from_str("0.00010").unwrap();
            setup_tariff(
                &pool,
                model_id,
                "standard_pricing",
                updated_input_price,
                updated_output_price,
                Some(ApiKeyPurpose::Realtime),
                None,
            )
            .await;

            // Request 2: Use updated pricing (1000 input, 500 output)
            let mut metrics2 = create_test_usage_metrics(1000, 500);
            metrics2.correlation_id = 12346;
            metrics2.request_model = Some("claude-3-opus".to_string());
            metrics2.response_model = Some("claude-3-opus".to_string());
            let mut request_data2 = create_test_request_data();
            request_data2.correlation_id = 12346;
            let result = store_analytics_record(&pool, &metrics2, &auth, &request_data2).await;
            assert!(result.is_ok(), "Second request should succeed");

            // Expected cost with updated pricing: (1000 * 0.00005) + (500 * 0.00010) = 0.05 + 0.05 = 0.10
            let expected_cost2 = Decimal::from_str("0.10").unwrap();

            // Verify: Check transactions
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            let transactions = credits
                .list_user_transactions(user_id, 0, 10, &TransactionFilters::default())
                .await
                .unwrap();

            let usage_transactions: Vec<_> = transactions
                .iter()
                .filter(|tx| tx.transaction_type == CreditTransactionType::Usage)
                .collect();

            assert_eq!(usage_transactions.len(), 2, "Should have 2 usage transactions");

            // Most recent transaction (index 0) should use updated pricing
            assert_eq!(
                usage_transactions[0].amount, expected_cost2,
                "Second request should use updated pricing (0.10)"
            );
            // Older transaction (index 1) should use original pricing
            assert_eq!(
                usage_transactions[1].amount, expected_cost1,
                "First request should use original pricing (0.20)"
            );

            // Verify: Final balance reflects both charges
            let final_balance = credits.get_user_balance(user_id).await.unwrap();
            let expected_balance = Decimal::from_str("100.00").unwrap() - expected_cost1 - expected_cost2;
            assert_eq!(
                final_balance, expected_balance,
                "Balance should reflect charges at both old and new pricing"
            );

            // Verify: Analytics records store the correct pricing
            let analytics1 = sqlx::query!(
                r#"
                SELECT input_price_per_token, output_price_per_token
                FROM http_analytics
                WHERE correlation_id = $1
                "#,
                request_data1.correlation_id as i64
            )
            .fetch_one(&pool)
            .await
            .unwrap();

            assert_eq!(analytics1.input_price_per_token.unwrap(), initial_input_price);
            assert_eq!(analytics1.output_price_per_token.unwrap(), initial_output_price);

            let analytics2 = sqlx::query!(
                r#"
                SELECT input_price_per_token, output_price_per_token
                FROM http_analytics
                WHERE correlation_id = $1
                "#,
                request_data2.correlation_id as i64
            )
            .fetch_one(&pool)
            .await
            .unwrap();

            assert_eq!(analytics2.input_price_per_token.unwrap(), updated_input_price);
            assert_eq!(analytics2.output_price_per_token.unwrap(), updated_output_price);
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_tariff_fallback_to_realtime_when_batch_tariff_missing(pool: sqlx::PgPool) {
            // Test that batch requests fall back to realtime pricing when no batch tariff exists

            // Setup: Create model with ONLY realtime tariff (no batch tariff)
            let model_id = create_test_model(&pool, "gpt-4").await;
            let realtime_input_price = Decimal::from_str("0.00015").unwrap();
            let realtime_output_price = Decimal::from_str("0.00030").unwrap();
            setup_tariff(
                &pool,
                model_id,
                "realtime_pricing",
                realtime_input_price,
                realtime_output_price,
                Some(ApiKeyPurpose::Realtime),
                None,
            )
            .await;

            // Setup: User with balance and batch API key
            let user_id = setup_user_with_balance(&pool, Decimal::from_str("100.00").unwrap()).await;
            let batch_auth = create_test_auth_for_user_with_purpose(&pool, user_id, ApiKeyPurpose::Batch).await;

            // Test: Make a batch request (should fall back to realtime pricing)
            let metrics = create_test_usage_metrics(1000, 500);
            let request_data = create_test_request_data();
            let result = store_analytics_record(&pool, &metrics, &batch_auth, &request_data).await;
            assert!(result.is_ok(), "Batch request should succeed with fallback pricing");

            // Expected cost using realtime pricing: (1000 * 0.00015) + (500 * 0.00030) = 0.15 + 0.15 = 0.30
            let expected_cost = Decimal::from_str("0.30").unwrap();

            // Verify: Transaction amount uses realtime pricing
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            let transactions = credits
                .list_user_transactions(user_id, 0, 10, &TransactionFilters::default())
                .await
                .unwrap();

            let usage_tx = transactions
                .iter()
                .find(|tx| tx.transaction_type == CreditTransactionType::Usage)
                .expect("Usage transaction should exist");

            assert_eq!(usage_tx.amount, expected_cost, "Batch request should fall back to realtime pricing");

            // Verify: Analytics record stores realtime pricing (not batch)
            let analytics = sqlx::query!(
                r#"
                SELECT input_price_per_token, output_price_per_token
                FROM http_analytics
                WHERE correlation_id = $1
                "#,
                request_data.correlation_id as i64
            )
            .fetch_one(&pool)
            .await
            .unwrap();

            assert_eq!(
                analytics.input_price_per_token.unwrap(),
                realtime_input_price,
                "Should store realtime input price"
            );
            assert_eq!(
                analytics.output_price_per_token.unwrap(),
                realtime_output_price,
                "Should store realtime output price"
            );
        }

        #[sqlx::test]
        #[test_log::test]
        async fn test_tariff_playground_has_separate_pricing(pool: sqlx::PgPool) {
            // Test that playground requests use their own pricing (separate from realtime/batch)

            // Setup: Create model with tariffs for all three purposes
            let model_id = create_test_model(&pool, "gpt-3.5-turbo").await;

            // Realtime: Standard pricing
            setup_tariff(
                &pool,
                model_id,
                "realtime_pricing",
                Decimal::from_str("0.00100").unwrap(),
                Decimal::from_str("0.00200").unwrap(),
                Some(ApiKeyPurpose::Realtime),
                None,
            )
            .await;

            // Batch: Discounted pricing (50% off)
            setup_tariff(
                &pool,
                model_id,
                "batch_pricing",
                Decimal::from_str("0.00050").unwrap(),
                Decimal::from_str("0.00100").unwrap(),
                Some(ApiKeyPurpose::Batch),
                None,
            )
            .await;

            // Playground: Free pricing (for testing/demos)
            setup_tariff(
                &pool,
                model_id,
                "playground_pricing",
                Decimal::ZERO,
                Decimal::ZERO,
                Some(ApiKeyPurpose::Playground),
                None,
            )
            .await;

            // Setup: User with balance
            let user_id = setup_user_with_balance(&pool, Decimal::from_str("100.00").unwrap()).await;

            // Create API keys for each purpose
            let realtime_auth = create_test_auth_for_user_with_purpose(&pool, user_id, ApiKeyPurpose::Realtime).await;
            let batch_auth = create_test_auth_for_user_with_purpose(&pool, user_id, ApiKeyPurpose::Batch).await;
            let playground_auth = create_test_auth_for_user_with_purpose(&pool, user_id, ApiKeyPurpose::Playground).await;

            // Make requests with each API key type (100 input, 50 output)
            let mut metrics1 = create_test_usage_metrics(100, 50);
            metrics1.correlation_id = 11111;
            metrics1.request_model = Some("gpt-3.5-turbo".to_string());
            metrics1.response_model = Some("gpt-3.5-turbo".to_string());
            let mut request_data1 = create_test_request_data();
            request_data1.correlation_id = 11111;
            store_analytics_record(&pool, &metrics1, &realtime_auth, &request_data1)
                .await
                .unwrap();

            let mut metrics2 = create_test_usage_metrics(100, 50);
            metrics2.correlation_id = 22222;
            metrics2.request_model = Some("gpt-3.5-turbo".to_string());
            metrics2.response_model = Some("gpt-3.5-turbo".to_string());
            let mut request_data2 = create_test_request_data();
            request_data2.correlation_id = 22222;
            store_analytics_record(&pool, &metrics2, &batch_auth, &request_data2).await.unwrap();

            let mut metrics3 = create_test_usage_metrics(100, 50);
            metrics3.correlation_id = 33333;
            metrics3.request_model = Some("gpt-3.5-turbo".to_string());
            metrics3.response_model = Some("gpt-3.5-turbo".to_string());
            let mut request_data3 = create_test_request_data();
            request_data3.correlation_id = 33333;
            store_analytics_record(&pool, &metrics3, &playground_auth, &request_data3)
                .await
                .unwrap();

            // Verify: Check transaction amounts
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            let transactions = credits
                .list_user_transactions(user_id, 0, 10, &TransactionFilters::default())
                .await
                .unwrap();

            let usage_transactions: Vec<_> = transactions
                .iter()
                .filter(|tx| tx.transaction_type == CreditTransactionType::Usage)
                .collect();

            // Only 2 usage transactions should exist (playground is free, so no transaction)
            assert_eq!(
                usage_transactions.len(),
                2,
                "Should have 2 usage transactions (realtime and batch only)"
            );

            // Expected costs:
            // Realtime: (100 * 0.00100) + (50 * 0.00200) = 0.10 + 0.10 = 0.20
            // Batch: (100 * 0.00050) + (50 * 0.00100) = 0.05 + 0.05 = 0.10
            // Playground: 0 (free)
            let expected_realtime_cost = Decimal::from_str("0.20").unwrap();
            let expected_batch_cost = Decimal::from_str("0.10").unwrap();

            // Transactions are ordered newest first, so: playground (no tx), batch, realtime
            assert_eq!(usage_transactions[0].amount, expected_batch_cost);
            assert_eq!(usage_transactions[1].amount, expected_realtime_cost);

            // Verify: Final balance (should only deduct realtime + batch, not playground)
            let final_balance = credits.get_user_balance(user_id).await.unwrap();
            let expected_balance = Decimal::from_str("100.00").unwrap() - expected_realtime_cost - expected_batch_cost;
            assert_eq!(
                final_balance, expected_balance,
                "Balance should only reflect realtime and batch charges"
            );
        }
    }
}
