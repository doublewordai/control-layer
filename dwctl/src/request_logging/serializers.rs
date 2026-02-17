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
use crate::request_logging::models::{AiRequest, AiResponse, ChatCompletionChunk, ParsedAIRequest};
use outlet::{RequestData, ResponseData};
use outlet_postgres::SerializationError;
use serde_json::Value;
use std::fmt;
use std::str;
use tracing::instrument;
use uuid::Uuid;

use super::utils;

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

/// Complete row structure for http_analytics table.
///
/// This struct mirrors the `http_analytics` database schema. Some fields are used by
/// `MetricsRecorder::record_from_analytics()` for Prometheus metrics, while others
/// exist to maintain parity with the database schema (populated but not read in Rust).
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields maintain schema parity; only subset read by MetricsRecorder
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
    /// Batch completion window (priority): "1h", "24h", etc.
    ///
    /// This is recorded as an empty string (`""`) for non-batch requests rather than
    /// using `None`/`NULL`. The empty-string sentinel is intentional so that
    /// Prometheus metrics can be filtered with a simple `batch_sla=""` label
    /// selector, at the cost of a small increase in label cardinality.
    pub batch_sla: String,
    /// The request_source from batch metadata (e.g., "api", "frontend").
    /// Empty string for non-batch requests or when not provided.
    pub batch_request_source: String,
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

#[cfg(test)]
mod tests {
    use super::{UsageMetrics, parse_ai_request, parse_ai_response};
    use crate::request_logging::models::{AiRequest, AiResponse};
    use async_openai::types::chat::{CreateChatCompletionResponse, CreateChatCompletionStreamResponse};
    use async_openai::types::completions::CreateCompletionResponse;
    use async_openai::types::embeddings::{CreateBase64EmbeddingResponse, CreateEmbeddingResponse, EmbeddingUsage};
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
            usage: Some(async_openai::types::chat::CompletionUsage {
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
            usage: Some(async_openai::types::chat::CompletionUsage {
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
            usage: Some(async_openai::types::chat::CompletionUsage {
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
}
