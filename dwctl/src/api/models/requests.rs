//! Request logging and querying API types
//!
//! These types provide a flexible interface for querying HTTP requests logged by the outlet-postgres
//! middleware, with basic enrichment for AI-specific endpoints.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};

use crate::request_logging::{AiRequest, AiResponse};

/// Tagged AI request types for API serialization - provides type discrimination for frontend
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ApiAiRequest {
    ChatCompletions(serde_json::Value),
    Completions(serde_json::Value),
    Embeddings(serde_json::Value),
    Other(serde_json::Value),
}

/// Tagged AI response types for API serialization - provides type discrimination for frontend
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ApiAiResponse {
    ChatCompletions(serde_json::Value),
    ChatCompletionsStream(serde_json::Value),
    Completions(serde_json::Value),
    Embeddings(serde_json::Value),
    Other(serde_json::Value),
}

impl From<&AiRequest> for ApiAiRequest {
    fn from(ai_request: &AiRequest) -> Self {
        match ai_request {
            AiRequest::ChatCompletions(req) => ApiAiRequest::ChatCompletions(serde_json::to_value(req).unwrap_or_default()),
            AiRequest::Completions(req) => ApiAiRequest::Completions(serde_json::to_value(req).unwrap_or_default()),
            AiRequest::Embeddings(req) => ApiAiRequest::Embeddings(serde_json::to_value(req).unwrap_or_default()),
            AiRequest::Other(val) => ApiAiRequest::Other(val.clone()),
        }
    }
}

impl From<&AiResponse> for ApiAiResponse {
    fn from(ai_response: &AiResponse) -> Self {
        match ai_response {
            AiResponse::ChatCompletions(resp) => ApiAiResponse::ChatCompletions(serde_json::to_value(resp).unwrap_or_default()),
            AiResponse::ChatCompletionsStream(chunks) => {
                ApiAiResponse::ChatCompletionsStream(serde_json::to_value(chunks).unwrap_or_default())
            }
            AiResponse::Completions(resp) => ApiAiResponse::Completions(serde_json::to_value(resp).unwrap_or_default()),
            AiResponse::Embeddings(resp) => ApiAiResponse::Embeddings(serde_json::to_value(resp).unwrap_or_default()),
            AiResponse::Base64Embeddings(resp) => ApiAiResponse::Embeddings(serde_json::to_value(resp).unwrap_or_default()),
            AiResponse::Other(val) => ApiAiResponse::Other(val.clone()),
        }
    }
}

/// Query parameters for aggregated request analytics
#[derive(Debug, Deserialize, ToSchema, IntoParams)]
pub struct AggregateRequestsQuery {
    /// Filter by specific model name
    pub model: Option<String>,

    /// Filter requests after this timestamp
    pub timestamp_after: Option<DateTime<Utc>>,

    /// Filter requests before this timestamp
    pub timestamp_before: Option<DateTime<Utc>>,
}

/// Query parameters for listing requests
#[derive(Debug, Deserialize, ToSchema, IntoParams)]
pub struct ListRequestsQuery {
    /// Maximum number of requests to return (default: 50, max: 1000)
    pub limit: Option<i64>,

    /// Number of requests to skip for pagination
    pub offset: Option<i64>,

    /// Filter by HTTP method (GET, POST, etc.)
    pub method: Option<String>,

    /// Filter by URI pattern (supports SQL LIKE patterns with %)
    pub uri_pattern: Option<String>,

    /// Filter by exact status code
    pub status_code: Option<i32>,

    /// Filter by minimum status code (for ranges)
    pub status_code_min: Option<i32>,

    /// Filter by maximum status code (for ranges)  
    pub status_code_max: Option<i32>,

    /// Filter by minimum request duration in milliseconds
    pub min_duration_ms: Option<i64>,

    /// Filter by maximum request duration in milliseconds
    pub max_duration_ms: Option<i64>,

    /// Filter requests after this timestamp
    pub timestamp_after: Option<DateTime<Utc>>,

    /// Filter requests before this timestamp
    pub timestamp_before: Option<DateTime<Utc>>,

    /// Order by timestamp descending (newest first) - default: true
    pub order_desc: Option<bool>,
}

/// API-compatible HTTP request representation
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HttpRequest {
    pub id: i64,
    pub timestamp: DateTime<Utc>,
    pub method: String,
    pub uri: String,
    pub headers: Value,
    pub body: Option<ApiAiRequest>,
    pub created_at: DateTime<Utc>,
}

/// API-compatible HTTP response representation  
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HttpResponse {
    pub id: i64,
    pub timestamp: DateTime<Utc>,
    pub status_code: i32,
    pub headers: Value,
    pub body: Option<ApiAiResponse>,
    pub duration_ms: i64,
    pub created_at: DateTime<Utc>,
}

/// API-compatible request-response pair
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RequestResponsePair {
    pub request: HttpRequest,
    pub response: Option<HttpResponse>,
}

/// Response containing a list of requests and pagination metadata
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ListRequestsResponse {
    /// List of HTTP requests
    pub requests: Vec<RequestResponsePair>,
}

impl Default for ListRequestsQuery {
    fn default() -> Self {
        Self {
            limit: Some(50),
            offset: Some(0),
            method: None,
            uri_pattern: None,
            status_code: None,
            status_code_min: None,
            status_code_max: None,
            min_duration_ms: None,
            max_duration_ms: None,
            timestamp_after: None,
            timestamp_before: None,
            order_desc: Some(true),
        }
    }
}

// ===== AGGREGATE/ANALYTICS RESPONSE TYPES =====

/// Status code breakdown for analytics
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StatusCodeBreakdown {
    pub status: String,
    pub count: i64,
    pub percentage: f64,
}

/// Model usage statistics
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ModelUsage {
    pub model: String,
    pub count: i64,
    pub percentage: f64,
    pub avg_latency_ms: f64,
}

/// User usage statistics for a specific model
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserUsage {
    pub user_id: Option<String>,
    pub user_email: Option<String>,
    pub request_count: i64,
    pub total_tokens: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_cost: Option<f64>,
    pub last_active_at: Option<DateTime<Utc>>,
}

/// Response for model usage grouped by user
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ModelUserUsageResponse {
    pub model: String,
    pub start_date: DateTime<Utc>,
    pub end_date: DateTime<Utc>,
    pub total_requests: i64,
    pub total_tokens: i64,
    pub total_cost: Option<f64>,
    pub users: Vec<UserUsage>,
}

/// Time series data point with combined metrics
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TimeSeriesPoint {
    pub timestamp: DateTime<Utc>,
    pub duration_minutes: i32,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub avg_latency_ms: Option<f64>,
    pub p95_latency_ms: Option<f64>,
    pub p99_latency_ms: Option<f64>,
}

/// Aggregated request analytics response
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RequestsAggregateResponse {
    pub total_requests: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub status_codes: Vec<StatusCodeBreakdown>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<ModelUsage>>,
    pub time_series: Vec<TimeSeriesPoint>,
}
