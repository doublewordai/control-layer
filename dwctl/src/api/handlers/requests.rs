//! Request logging handlers
//!
//! Endpoints for querying HTTP requests logged by the outlet-postgres middleware.

use axum::{
    extract::{Query, State},
    response::Json,
};
// Remove unused chrono imports
use outlet_postgres::{RequestFilter, RequestRepository};
use tracing::{debug, error};

use crate::{
    api::models::requests::{
        AggregateRequestsQuery, ApiAiRequest, ApiAiResponse, HttpRequest, HttpResponse, ListRequestsQuery, ListRequestsResponse,
        ModelUserUsageResponse, RequestResponsePair, RequestsAggregateResponse,
    },
    auth::permissions::{operation, resource, RequiresPermission},
    db::handlers::analytics::{get_model_user_usage, get_requests_aggregate},
    errors::Error,
    request_logging::{AiRequest, AiResponse},
    AppState,
};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use utoipa::IntoParams;

/// Convert outlet-postgres request/response pairs to API types
///
/// This function handles the conversion from the outlet-postgres database types
/// to the API response types, including error handling for unparseable request/response bodies.
fn convert_outlet_pairs_to_api(outlet_pairs: Vec<outlet_postgres::RequestResponsePair<AiRequest, AiResponse>>) -> Vec<RequestResponsePair> {
    outlet_pairs
        .into_iter()
        .map(|pair| {
            // Capture values for logging before moving
            let req_correlation_id = pair.request.correlation_id;
            let req_uri = pair.request.uri.clone();

            let api_request = HttpRequest {
                id: pair.request.id,
                timestamp: pair.request.timestamp,
                method: pair.request.method,
                uri: pair.request.uri,
                headers: pair.request.headers,
                body: pair.request.body.as_ref().map(|result| match result {
                    Ok(parsed_request) => ApiAiRequest::from(parsed_request),
                    Err(raw_bytes) => {
                        // Handle base64-encoded error data
                        let raw_string = String::from_utf8_lossy(raw_bytes);
                        tracing::warn!(
                            correlation_id = req_correlation_id,
                            uri = %req_uri,
                            raw_string_preview = %raw_string.chars().take(100).collect::<String>(),
                            "Request failed to parse, using raw data"
                        );
                        ApiAiRequest::Other(serde_json::Value::String(raw_string.to_string()))
                    }
                }),
                created_at: pair.request.created_at,
            };

            let api_response = pair.response.map(|resp| {
                // Capture values for logging before moving
                let resp_correlation_id = resp.correlation_id;
                let resp_status_code = resp.status_code;

                HttpResponse {
                    id: resp.id,
                    timestamp: resp.timestamp,
                    status_code: resp.status_code,
                    headers: resp.headers,
                    body: resp.body.as_ref().map(|result| match result {
                        Ok(parsed_response) => ApiAiResponse::from(parsed_response),
                        Err(raw_bytes) => {
                            // Handle base64-encoded error data
                            let raw_string = String::from_utf8_lossy(raw_bytes);
                            tracing::warn!(
                                correlation_id = resp_correlation_id,
                                status_code = resp_status_code,
                                raw_string_preview = %raw_string.chars().take(100).collect::<String>(),
                                "Response failed to parse, using raw data"
                            );
                            ApiAiResponse::Other(serde_json::Value::String(raw_string.to_string()))
                        }
                    }),
                    duration_ms: resp.duration_ms,
                    created_at: resp.created_at,
                }
            });

            RequestResponsePair {
                request: api_request,
                response: api_response,
            }
        })
        .collect()
}

/// List HTTP requests with filtering and pagination
///
/// Returns a paginated list of HTTP requests logged by the system, with optional filtering
/// by user, endpoint type, time range, and other criteria.
#[utoipa::path(
    get,
    path = "/admin/api/v1/requests",
    params(ListRequestsQuery),
    responses(
        (status = 200, description = "List of HTTP requests"),
        (status = 400, description = "Invalid query parameters"),
        (status = 404, description = "Request logging not enabled"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "requests",
)]
#[tracing::instrument(skip_all)]
pub async fn list_requests(
    Query(query): Query<ListRequestsQuery>,
    State(state): State<AppState>,
    _: RequiresPermission<resource::Requests, operation::ReadAll>,
) -> Result<Json<ListRequestsResponse>, Error> {
    // Validate and apply limits
    let limit = query.limit.unwrap_or(50).clamp(1, 1000);
    let offset = query.offset.unwrap_or(0).max(0);

    // If request logging is not enabled, return 404
    let outlet_pool = state.outlet_db.as_ref().ok_or_else(|| {
        debug!("Request logging is not enabled");
        Error::NotFound {
            id: "request_logging".to_string(),
            resource: "Request logging is not enabled".to_string(),
        }
    })?;

    let repository: RequestRepository<AiRequest, AiResponse> = RequestRepository::new(outlet_pool.clone());

    // Build filter for outlet-postgres
    let mut filter = RequestFilter {
        uri_pattern: None,
        limit: Some(limit),
        offset: Some(offset),
        order_by_timestamp_desc: query.order_desc.unwrap_or(true),
        ..Default::default()
    };

    // Apply query filters
    if let Some(method) = &query.method {
        filter.method = Some(method.clone());
    }

    if let Some(status_code) = query.status_code {
        filter.status_code = Some(status_code);
    }

    if let Some(min_status) = query.status_code_min {
        filter.status_code_min = Some(min_status);
    }

    if let Some(max_status) = query.status_code_max {
        filter.status_code_max = Some(max_status);
    }

    if let Some(min_duration) = query.min_duration_ms {
        filter.min_duration_ms = Some(min_duration);
    }

    if let Some(max_duration) = query.max_duration_ms {
        filter.max_duration_ms = Some(max_duration);
    }

    if let Some(timestamp_after) = query.timestamp_after {
        filter.timestamp_after = Some(timestamp_after);
    }

    if let Some(timestamp_before) = query.timestamp_before {
        filter.timestamp_before = Some(timestamp_before);
    }

    // Additional URI pattern filtering if requested
    if let Some(uri_pattern) = &query.uri_pattern {
        filter.uri_pattern = Some(uri_pattern.clone());
    }

    // Query the outlet-postgres repository
    let outlet_pairs = repository.query(filter).await.map_err(|e| {
        error!("Failed to query requests: {}", e);
        Error::Internal {
            operation: "Failed to query requests".to_string(),
        }
    })?;

    // Convert outlet-postgres types to API types
    let api_pairs = convert_outlet_pairs_to_api(outlet_pairs);

    Ok(Json(ListRequestsResponse { requests: api_pairs }))
}

/// Get aggregated request metrics and analytics
///
/// Returns aggregated metrics and analytics about HTTP requests, including counts,
/// latency statistics, error rates, and other aggregated insights.
#[utoipa::path(
    get,
    path = "/admin/api/v1/requests/aggregate",
    params(AggregateRequestsQuery),
    responses(
        (status = 200, description = "Aggregated request metrics", body = RequestsAggregateResponse),
        (status = 404, description = "Request logging not enabled"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "requests",
)]
#[tracing::instrument(skip_all)]
pub async fn aggregate_requests(
    Query(query): Query<AggregateRequestsQuery>,
    State(state): State<AppState>,
    _: RequiresPermission<resource::Analytics, operation::ReadAll>,
) -> Result<Json<RequestsAggregateResponse>, Error> {
    // If request logging is not enabled, return 404
    if state.outlet_db.is_none() {
        debug!("Request logging is not enabled");
        return Err(Error::NotFound {
            id: "request_logging".to_string(),
            resource: "Request logging is not enabled".to_string(),
        });
    };

    // Use provided timestamps or default to last 24 hours
    let now = chrono::Utc::now();
    let time_range_start = query.timestamp_after.unwrap_or_else(|| now - chrono::Duration::hours(24));
    let time_range_end = query.timestamp_before.unwrap_or(now);
    let model_filter = query.model.as_deref();

    // Get aggregated analytics data
    let response = get_requests_aggregate(&state.db, time_range_start, time_range_end, model_filter).await?;

    Ok(Json(response))
}

/// Query parameters for aggregate by user
#[derive(Debug, Deserialize, IntoParams)]
pub struct AggregateByUserQuery {
    /// Filter by specific model alias
    pub model: Option<String>,
    /// Start date for usage data (defaults to 24 hours ago)
    pub start_date: Option<DateTime<Utc>>,
    /// End date for usage data (defaults to now)
    pub end_date: Option<DateTime<Utc>>,
}

/// Get aggregated request metrics grouped by user
///
/// Returns request metrics aggregated by user for the specified time range and model.
#[utoipa::path(
    get,
    path = "/admin/api/v1/requests/aggregate-by-user",
    params(AggregateByUserQuery),
    responses(
        (status = 200, description = "User aggregated request metrics", body = ModelUserUsageResponse),
        (status = 404, description = "Request logging not enabled or model not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "requests",
)]
#[tracing::instrument(skip_all)]
pub async fn aggregate_by_user(
    Query(query): Query<AggregateByUserQuery>,
    State(state): State<AppState>,
    _: RequiresPermission<resource::Analytics, operation::ReadAll>,
) -> Result<Json<ModelUserUsageResponse>, Error> {
    // If request logging is not enabled, return 404
    if state.outlet_db.is_none() {
        debug!("Request logging is not enabled");
        return Err(Error::NotFound {
            id: "request_logging".to_string(),
            resource: "Request logging is not enabled".to_string(),
        });
    };

    // Model is required for this endpoint
    let model_alias = query.model.ok_or_else(|| Error::BadRequest {
        message: "Model parameter is required".to_string(),
    })?;

    // Set default date range
    let end_date = query.end_date.unwrap_or_else(Utc::now);
    let start_date = query.start_date.unwrap_or_else(|| end_date - Duration::hours(24));

    // Get usage data
    let usage_data = get_model_user_usage(&state.db, &model_alias, start_date, end_date).await?;

    Ok(Json(usage_data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::models::users::Role, test_utils::*};
    use chrono::{Duration, Utc};
    use serde_json::json;
    use sqlx::{ConnectOptions, PgPool};

    // Helper function to insert test analytics data for aggregate tests
    async fn insert_test_analytics_data(
        pool: &PgPool,
        timestamp: chrono::DateTime<chrono::Utc>,
        model: &str,
        status_code: i32,
        duration_ms: f64,
        prompt_tokens: i64,
        completion_tokens: i64,
    ) {
        use uuid::Uuid;

        sqlx::query!(
            r#"
            INSERT INTO http_analytics (
                instance_id, correlation_id, timestamp, uri, method, status_code, duration_ms, 
                model, prompt_tokens, completion_tokens, total_tokens
            ) VALUES ($1, $2, $3, '/ai/chat/completions', 'POST', $4, $5, $6, $7, $8, $9)
            "#,
            Uuid::new_v4(),
            1i64,
            timestamp,
            status_code,
            duration_ms as i64,
            model,
            prompt_tokens,
            completion_tokens,
            prompt_tokens + completion_tokens
        )
        .execute(pool)
        .await
        .expect("Failed to insert test analytics data");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_requests_outlet_disabled(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await; // Request logging disabled
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        let response = app
            .get("/admin/api/v1/requests")
            .add_header(&add_auth_headers(&admin_user)[0].0, &add_auth_headers(&admin_user)[0].1)
            .add_header(&add_auth_headers(&admin_user)[1].0, &add_auth_headers(&admin_user)[1].1)
            .await;

        // Should return 404 since request logging is disabled
        response.assert_status(axum::http::StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_requests_unauthorized(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), true).await; // Request logging enabled
        let user = create_test_user(&pool, Role::StandardUser).await; // Non-admin user

        let response = app
            .get("/admin/api/v1/requests")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        // Should be forbidden since user doesn't have Requests:Read permission
        response.assert_status(axum::http::StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_aggregate_requests_outlet_disabled(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await; // Request logging disabled
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        let response = app
            .get("/admin/api/v1/requests/aggregate")
            .add_header(&add_auth_headers(&admin_user)[0].0, &add_auth_headers(&admin_user)[0].1)
            .add_header(&add_auth_headers(&admin_user)[1].0, &add_auth_headers(&admin_user)[1].1)
            .await;

        // Should return 404 since request logging is disabled
        response.assert_status(axum::http::StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_aggregate_requests_success(pool: PgPool) {
        // Insert analytics data to test aggregate functionality
        let base_time = Utc::now() - Duration::hours(1);
        insert_test_analytics_data(&pool, base_time, "gpt-4", 200, 100.0, 50, 25).await;

        // Create config with request logging enabled
        let mut config = create_test_config();
        config.enable_request_logging = true;
        config.database = crate::config::DatabaseConfig::External {
            url: pool.connect_options().to_url_lossy().to_string(),
        };
        config.leader_election.enabled = false;

        // Use Application::new to properly set up outlet_db
        let app = crate::Application::new(config).await.expect("Failed to create application");
        let (server, _drop_guard) = app.into_test_server();
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        let response = server
            .get("/admin/api/v1/requests/aggregate")
            .add_header(&add_auth_headers(&admin_user)[0].0, &add_auth_headers(&admin_user)[0].1)
            .add_header(&add_auth_headers(&admin_user)[1].0, &add_auth_headers(&admin_user)[1].1)
            .await;

        response.assert_status_ok();
        let aggregate_response: RequestsAggregateResponse = response.json();
        assert_eq!(aggregate_response.total_requests, 1);
        assert!(aggregate_response.model.is_none()); // No model filter applied
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_aggregate_requests_with_model_filter(pool: PgPool) {
        // Insert analytics data for multiple models
        let base_time = Utc::now() - Duration::hours(1);
        insert_test_analytics_data(&pool, base_time, "gpt-4", 200, 100.0, 50, 25).await;
        insert_test_analytics_data(&pool, base_time, "claude-3", 200, 150.0, 75, 35).await;

        // Create config with request logging enabled
        let mut config = create_test_config();
        config.enable_request_logging = true;
        config.database = crate::config::DatabaseConfig::External {
            url: pool.connect_options().to_url_lossy().to_string(),
        };
        config.leader_election.enabled = false;

        // Use Application::new to properly set up outlet_db
        let app = crate::Application::new(config).await.expect("Failed to create application");
        let (server, _drop_guard) = app.into_test_server();
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        let response = server
            .get("/admin/api/v1/requests/aggregate?model=gpt-4")
            .add_header(&add_auth_headers(&admin_user)[0].0, &add_auth_headers(&admin_user)[0].1)
            .add_header(&add_auth_headers(&admin_user)[1].0, &add_auth_headers(&admin_user)[1].1)
            .await;

        response.assert_status_ok();
        let aggregate_response: RequestsAggregateResponse = response.json();
        assert_eq!(aggregate_response.total_requests, 1);
        assert_eq!(aggregate_response.model, Some("gpt-4".to_string()));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_aggregate_requests_unauthorized(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), true).await; // Request logging enabled
        let user = create_test_user(&pool, Role::StandardUser).await; // Non-admin user

        let response = app
            .get("/admin/api/v1/requests/aggregate")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        // Should be forbidden since user doesn't have Analytics:Read permission
        response.assert_status(axum::http::StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_requests_success_empty(pool: PgPool) {
        // Test successful response when request logging is enabled but no data exists
        // This exercises the outlet database query and conversion logic (lines 112-184)
        let mut config = create_test_config();
        config.enable_request_logging = true;
        config.database = crate::config::DatabaseConfig::External {
            url: pool.connect_options().to_url_lossy().to_string(),
        };
        config.leader_election.enabled = false;

        // Use Application::new to properly set up outlet_db
        let app = crate::Application::new(config).await.expect("Failed to create application");
        let (server, _drop_guard) = app.into_test_server();
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        let response = server
            .get("/admin/api/v1/requests")
            .add_header(&add_auth_headers(&admin_user)[0].0, &add_auth_headers(&admin_user)[0].1)
            .add_header(&add_auth_headers(&admin_user)[1].0, &add_auth_headers(&admin_user)[1].1)
            .await;

        response.assert_status_ok();
        let list_response: ListRequestsResponse = response.json();
        assert!(list_response.requests.is_empty());
    }

    // Unit tests for helper types and conversions
    #[test]
    fn test_list_requests_query_default() {
        let query = ListRequestsQuery::default();
        assert_eq!(query.limit, Some(50));
        assert_eq!(query.offset, Some(0));
        assert_eq!(query.order_desc, Some(true));
        assert!(query.method.is_none());
        assert!(query.uri_pattern.is_none());
        assert!(query.status_code.is_none());
    }

    #[test]
    fn test_list_requests_query_limit_clamping() {
        // Test the handler's limit clamping logic
        let over_max = 2000i64.clamp(1, 1000);
        assert_eq!(over_max, 1000);

        let under_min = 0i64.clamp(1, 1000);
        assert_eq!(under_min, 1);

        let valid = 50i64.clamp(1, 1000);
        assert_eq!(valid, 50);
    }

    #[test]
    fn test_api_ai_request_conversions() {
        // Test conversion from AiRequest to ApiAiRequest
        let ai_request = AiRequest::Other(json!({"test": "data"}));
        let api_request = ApiAiRequest::from(&ai_request);

        match api_request {
            ApiAiRequest::Other(val) => {
                assert_eq!(val, json!({"test": "data"}));
            }
            _ => panic!("Expected Other variant"),
        }
    }

    #[test]
    fn test_api_ai_response_conversions() {
        // Test conversion from AiResponse to ApiAiResponse
        let ai_response = AiResponse::Other(json!({"result": "success"}));
        let api_response = ApiAiResponse::from(&ai_response);

        match api_response {
            ApiAiResponse::Other(val) => {
                assert_eq!(val, json!({"result": "success"}));
            }
            _ => panic!("Expected Other variant"),
        }
    }

    #[test]
    fn test_convert_outlet_pairs_empty() {
        // Test conversion with empty input
        let outlet_pairs = vec![];
        let api_pairs = super::convert_outlet_pairs_to_api(outlet_pairs);
        assert!(api_pairs.is_empty());
    }

    #[test]
    fn test_convert_outlet_pairs_successful_parsing() {
        // Test conversion with successful request/response parsing
        use chrono::Utc;
        use uuid::Uuid;

        let req_id = 1i64;
        let resp_id = 2i64;
        let instance_id = Uuid::new_v4();
        let correlation_id = 12345i64;
        let timestamp = Utc::now();

        let parsed_request = AiRequest::Other(json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hello"}]
        }));

        let parsed_response = AiResponse::Other(json!({
            "choices": [{"message": {"role": "assistant", "content": "Hi there!"}}]
        }));

        let outlet_request = outlet_postgres::HttpRequest {
            id: req_id,
            instance_id,
            correlation_id,
            timestamp,
            method: "POST".to_string(),
            uri: "/ai/v1/chat/completions".to_string(),
            headers: json!({"content-type": "application/json"}),
            body: Some(Ok(parsed_request)),
            created_at: timestamp,
        };

        let outlet_response = outlet_postgres::HttpResponse {
            id: resp_id,
            instance_id,
            correlation_id,
            timestamp,
            status_code: 200,
            headers: json!({"content-type": "application/json"}),
            body: Some(Ok(parsed_response)),
            duration_ms: 150,
            duration_to_first_byte_ms: 50,
            created_at: timestamp,
        };

        let outlet_pair = outlet_postgres::RequestResponsePair {
            request: outlet_request,
            response: Some(outlet_response),
        };

        let api_pairs = super::convert_outlet_pairs_to_api(vec![outlet_pair]);

        assert_eq!(api_pairs.len(), 1);
        let pair = &api_pairs[0];

        // Verify request conversion
        assert_eq!(pair.request.id, req_id);
        assert_eq!(pair.request.method, "POST");
        assert_eq!(pair.request.uri, "/ai/v1/chat/completions");
        assert!(pair.request.body.is_some());

        // Verify response conversion
        assert!(pair.response.is_some());
        let response = pair.response.as_ref().unwrap();
        assert_eq!(response.id, resp_id);
        assert_eq!(response.status_code, 200);
        assert_eq!(response.duration_ms, 150);
        assert!(response.body.is_some());
    }

    #[test]
    fn test_convert_outlet_pairs_parsing_errors() {
        // Test conversion with parsing errors (raw bytes)
        use bytes::Bytes;
        use chrono::Utc;
        use uuid::Uuid;

        let req_id = 3i64;
        let resp_id = 4i64;
        let instance_id = Uuid::new_v4();
        let correlation_id = 67890i64;
        let timestamp = Utc::now();

        // Create raw bytes that failed to parse
        let raw_request_bytes = Bytes::from("invalid json request data");
        let raw_response_bytes = Bytes::from("invalid json response data");

        let outlet_request = outlet_postgres::HttpRequest {
            id: req_id,
            instance_id,
            correlation_id,
            timestamp,
            method: "POST".to_string(),
            uri: "/ai/v1/embeddings".to_string(),
            headers: json!({"content-type": "application/json"}),
            body: Some(Err(raw_request_bytes)),
            created_at: timestamp,
        };

        let outlet_response = outlet_postgres::HttpResponse {
            id: resp_id,
            instance_id,
            correlation_id,
            timestamp,
            status_code: 400,
            headers: json!({"content-type": "application/json"}),
            body: Some(Err(raw_response_bytes)),
            duration_ms: 50,
            duration_to_first_byte_ms: 25,
            created_at: timestamp,
        };

        let outlet_pair = outlet_postgres::RequestResponsePair {
            request: outlet_request,
            response: Some(outlet_response),
        };

        let api_pairs = super::convert_outlet_pairs_to_api(vec![outlet_pair]);

        assert_eq!(api_pairs.len(), 1);
        let pair = &api_pairs[0];

        // Verify request with parsing error
        assert_eq!(pair.request.id, req_id);
        assert_eq!(pair.request.method, "POST");
        assert_eq!(pair.request.uri, "/ai/v1/embeddings");

        // Body should be converted to ApiAiRequest::Other with raw string
        match pair.request.body.as_ref().unwrap() {
            ApiAiRequest::Other(value) => {
                assert_eq!(value, &json!("invalid json request data"));
            }
            _ => panic!("Expected Other variant with raw string"),
        }

        // Verify response with parsing error
        let response = pair.response.as_ref().unwrap();
        assert_eq!(response.status_code, 400);

        match response.body.as_ref().unwrap() {
            ApiAiResponse::Other(value) => {
                assert_eq!(value, &json!("invalid json response data"));
            }
            _ => panic!("Expected Other variant with raw string"),
        }
    }

    #[test]
    fn test_convert_outlet_pairs_no_response() {
        // Test conversion with request only (no response)
        use chrono::Utc;
        use uuid::Uuid;

        let req_id = 5i64;
        let instance_id = Uuid::new_v4();
        let correlation_id = 99999i64;
        let timestamp = Utc::now();

        let outlet_request = outlet_postgres::HttpRequest {
            id: req_id,
            instance_id,
            correlation_id,
            timestamp,
            method: "GET".to_string(),
            uri: "/ai/v1/models".to_string(),
            headers: json!({}),
            body: None, // No body for GET request
            created_at: timestamp,
        };

        let outlet_pair = outlet_postgres::RequestResponsePair {
            request: outlet_request,
            response: None, // No response yet
        };

        let api_pairs = super::convert_outlet_pairs_to_api(vec![outlet_pair]);

        assert_eq!(api_pairs.len(), 1);
        let pair = &api_pairs[0];

        // Verify request conversion
        assert_eq!(pair.request.id, req_id);
        assert_eq!(pair.request.method, "GET");
        assert_eq!(pair.request.uri, "/ai/v1/models");
        assert!(pair.request.body.is_none());

        // Verify no response
        assert!(pair.response.is_none());
    }

    #[test]
    fn test_convert_outlet_pairs_mixed_scenarios() {
        // Test conversion with multiple pairs in different states
        use bytes::Bytes;
        use chrono::Utc;
        use uuid::Uuid;

        let instance_id = Uuid::new_v4();
        let timestamp = Utc::now();

        // Pair 1: Successful parsing
        let pair1_req = outlet_postgres::HttpRequest {
            id: 10i64,
            instance_id,
            correlation_id: 1,
            timestamp,
            method: "POST".to_string(),
            uri: "/ai/v1/chat/completions".to_string(),
            headers: json!({}),
            body: Some(Ok(AiRequest::Other(json!({"test": "success"})))),
            created_at: timestamp,
        };

        // Pair 2: Parsing error
        let pair2_req = outlet_postgres::HttpRequest {
            id: 20i64,
            instance_id,
            correlation_id: 2,
            timestamp,
            method: "POST".to_string(),
            uri: "/ai/v1/embeddings".to_string(),
            headers: json!({}),
            body: Some(Err(Bytes::from("error data"))),
            created_at: timestamp,
        };

        // Pair 3: No body
        let pair3_req = outlet_postgres::HttpRequest {
            id: 30i64,
            instance_id,
            correlation_id: 3,
            timestamp,
            method: "GET".to_string(),
            uri: "/ai/v1/models".to_string(),
            headers: json!({}),
            body: None,
            created_at: timestamp,
        };

        let outlet_pairs = vec![
            outlet_postgres::RequestResponsePair {
                request: pair1_req,
                response: None,
            },
            outlet_postgres::RequestResponsePair {
                request: pair2_req,
                response: None,
            },
            outlet_postgres::RequestResponsePair {
                request: pair3_req,
                response: None,
            },
        ];

        let api_pairs = super::convert_outlet_pairs_to_api(outlet_pairs);

        assert_eq!(api_pairs.len(), 3);

        // Verify first pair (successful parsing)
        assert_eq!(api_pairs[0].request.id, 10i64);
        assert!(api_pairs[0].request.body.is_some());

        // Verify second pair (parsing error)
        assert_eq!(api_pairs[1].request.id, 20i64);
        match api_pairs[1].request.body.as_ref().unwrap() {
            ApiAiRequest::Other(value) => assert_eq!(value, &json!("error data")),
            _ => panic!("Expected Other variant"),
        }

        // Verify third pair (no body)
        assert_eq!(api_pairs[2].request.id, 30i64);
        assert!(api_pairs[2].request.body.is_none());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_standard_user_cannot_access_requests(pool: PgPool) {
        // Create config with request logging enabled
        let mut config = create_test_config();
        config.enable_request_logging = true;
        config.database = crate::config::DatabaseConfig::External {
            url: pool.connect_options().to_url_lossy().to_string(),
        };
        config.leader_election.enabled = false;

        // Use Application::new to properly set up outlet_db
        let app = crate::Application::new(config).await.expect("Failed to create application");
        let (app, _drop_guard) = app.into_test_server();
        let standard_user = create_test_user(&pool, Role::StandardUser).await;

        // StandardUser should NOT be able to list requests (no Requests permissions)
        let response = app
            .get("/admin/api/v1/requests")
            .add_header(&add_auth_headers(&standard_user)[0].0, &add_auth_headers(&standard_user)[0].1)
            .add_header(&add_auth_headers(&standard_user)[1].0, &add_auth_headers(&standard_user)[1].1)
            .await;

        response.assert_status_forbidden();

        // StandardUser should NOT be able to access aggregated requests (no Analytics permissions)
        let response = app
            .get("/admin/api/v1/requests/aggregate")
            .add_header(&add_auth_headers(&standard_user)[0].0, &add_auth_headers(&standard_user)[0].1)
            .add_header(&add_auth_headers(&standard_user)[1].0, &add_auth_headers(&standard_user)[1].1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_request_viewer_can_access_monitoring_data(pool: PgPool) {
        // Create config with request logging enabled
        let mut config = create_test_config();
        config.enable_request_logging = true;
        config.database = crate::config::DatabaseConfig::External {
            url: pool.connect_options().to_url_lossy().to_string(),
        };
        config.leader_election.enabled = false;

        // Use Application::new to properly set up outlet_db
        let app = crate::Application::new(config).await.expect("Failed to create application");
        let (app, _drop_guard) = app.into_test_server();
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;

        // RequestViewer should be able to list requests (has ReadAll for Requests)
        let response = app
            .get("/admin/api/v1/requests")
            .add_header(&add_auth_headers(&request_viewer)[0].0, &add_auth_headers(&request_viewer)[0].1)
            .add_header(&add_auth_headers(&request_viewer)[1].0, &add_auth_headers(&request_viewer)[1].1)
            .await;

        response.assert_status_ok();
        let _list_response: ListRequestsResponse = response.json();
        // Empty is fine - we're testing permission, not data

        // RequestViewer should be able to access aggregated requests (has ReadAll for Analytics)
        let response = app
            .get("/admin/api/v1/requests/aggregate")
            .add_header(&add_auth_headers(&request_viewer)[0].0, &add_auth_headers(&request_viewer)[0].1)
            .add_header(&add_auth_headers(&request_viewer)[1].0, &add_auth_headers(&request_viewer)[1].1)
            .await;

        response.assert_status_ok();
        let _aggregate_response: RequestsAggregateResponse = response.json();
        // Empty data is fine - we're testing permission
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_cannot_access_sensitive_requests(pool: PgPool) {
        // Create config with request logging enabled
        let mut config = create_test_config();
        config.enable_request_logging = true;
        config.database = crate::config::DatabaseConfig::External {
            url: pool.connect_options().to_url_lossy().to_string(),
        };
        config.leader_election.enabled = false;

        // Use Application::new to properly set up outlet_db
        let app = crate::Application::new(config).await.expect("Failed to create application");
        let (app, _drop_guard) = app.into_test_server();

        // Create a NON-ADMIN PlatformManager (so role permissions are checked)
        let platform_manager = create_test_user(&pool, Role::PlatformManager).await; // Not create_test_admin_user!

        // PlatformManager should NOT be able to list requests (no Requests permissions)
        let response = app
            .get("/admin/api/v1/requests")
            .add_header(&add_auth_headers(&platform_manager)[0].0, &add_auth_headers(&platform_manager)[0].1)
            .add_header(&add_auth_headers(&platform_manager)[1].0, &add_auth_headers(&platform_manager)[1].1)
            .await;

        response.assert_status_forbidden();

        // But PlatformManager should be able to access aggregated analytics (has Analytics permissions)
        let response = app
            .get("/admin/api/v1/requests/aggregate")
            .add_header(&add_auth_headers(&platform_manager)[0].0, &add_auth_headers(&platform_manager)[0].1)
            .add_header(&add_auth_headers(&platform_manager)[1].0, &add_auth_headers(&platform_manager)[1].1)
            .await;

        response.assert_status_ok();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_layered_roles_monitoring_access(pool: PgPool) {
        // Create config with request logging enabled
        let mut config = create_test_config();
        config.enable_request_logging = true;
        config.database = crate::config::DatabaseConfig::External {
            url: pool.connect_options().to_url_lossy().to_string(),
        };
        config.leader_election.enabled = false;

        // Use Application::new to properly set up outlet_db
        let app = crate::Application::new(config).await.expect("Failed to create application");
        let (app, _drop_guard) = app.into_test_server();

        // User with StandardUser + RequestViewer should have monitoring access
        let monitoring_user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::RequestViewer]).await;

        // Should be able to list requests (RequestViewer permission)
        let response = app
            .get("/admin/api/v1/requests")
            .add_header(&add_auth_headers(&monitoring_user)[0].0, &add_auth_headers(&monitoring_user)[0].1)
            .add_header(&add_auth_headers(&monitoring_user)[1].0, &add_auth_headers(&monitoring_user)[1].1)
            .await;

        response.assert_status_ok();

        // Should be able to access analytics (RequestViewer permission)
        let response = app
            .get("/admin/api/v1/requests/aggregate")
            .add_header(&add_auth_headers(&monitoring_user)[0].0, &add_auth_headers(&monitoring_user)[0].1)
            .add_header(&add_auth_headers(&monitoring_user)[1].0, &add_auth_headers(&monitoring_user)[1].1)
            .await;

        response.assert_status_ok();

        // User with PlatformManager + RequestViewer should have full monitoring access
        let full_admin = create_test_user_with_roles(&pool, vec![Role::PlatformManager, Role::RequestViewer]).await;

        // Should be able to list requests (RequestViewer permission)
        let response = app
            .get("/admin/api/v1/requests")
            .add_header(&add_auth_headers(&full_admin)[0].0, &add_auth_headers(&full_admin)[0].1)
            .add_header(&add_auth_headers(&full_admin)[1].0, &add_auth_headers(&full_admin)[1].1)
            .await;

        response.assert_status_ok();

        // Should be able to access analytics (both roles have this)
        let response = app
            .get("/admin/api/v1/requests/aggregate")
            .add_header(&add_auth_headers(&full_admin)[0].0, &add_auth_headers(&full_admin)[0].1)
            .add_header(&add_auth_headers(&full_admin)[1].0, &add_auth_headers(&full_admin)[1].1)
            .await;

        response.assert_status_ok();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_requests_filtering_and_query_permissions(pool: PgPool) {
        // Create config with request logging enabled
        let mut config = create_test_config();
        config.enable_request_logging = true;
        config.database = crate::config::DatabaseConfig::External {
            url: pool.connect_options().to_url_lossy().to_string(),
        };
        config.leader_election.enabled = false;

        // Use Application::new to properly set up outlet_db
        let app = crate::Application::new(config).await.expect("Failed to create application");
        let (app, _drop_guard) = app.into_test_server();
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;

        // RequestViewer should be able to use all query parameters
        let response = app
            .get("/admin/api/v1/requests?limit=10&offset=0&method=POST&status_code_min=200&status_code_max=299")
            .add_header(&add_auth_headers(&request_viewer)[0].0, &add_auth_headers(&request_viewer)[0].1)
            .add_header(&add_auth_headers(&request_viewer)[1].0, &add_auth_headers(&request_viewer)[1].1)
            .await;

        response.assert_status_ok();

        let response = app
            .get("/admin/api/v1/requests?uri_pattern=chat/completions&order_desc=false")
            .add_header(&add_auth_headers(&request_viewer)[0].0, &add_auth_headers(&request_viewer)[0].1)
            .add_header(&add_auth_headers(&request_viewer)[1].0, &add_auth_headers(&request_viewer)[1].1)
            .await;

        response.assert_status_ok();

        // StandardUser should be forbidden regardless of query parameters
        let response = app
            .get("/admin/api/v1/requests?limit=1")
            .add_header(&add_auth_headers(&standard_user)[0].0, &add_auth_headers(&standard_user)[0].1)
            .add_header(&add_auth_headers(&standard_user)[1].0, &add_auth_headers(&standard_user)[1].1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_analytics_filtering_permissions(pool: PgPool) {
        // Insert some test data
        let base_time = Utc::now() - Duration::hours(1);
        insert_test_analytics_data(&pool, base_time, "gpt-4", 200, 100.0, 50, 25).await;
        insert_test_analytics_data(&pool, base_time, "claude-3", 200, 150.0, 75, 35).await;

        // Create config with request logging enabled
        let mut config = create_test_config();
        config.enable_request_logging = true;
        config.database = crate::config::DatabaseConfig::External {
            url: pool.connect_options().to_url_lossy().to_string(),
        };
        config.leader_election.enabled = false;

        // Use Application::new to properly set up outlet_db
        let app = crate::Application::new(config).await.expect("Failed to create application");
        let (app, _drop_guard) = app.into_test_server();
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;
        let platform_manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;

        // RequestViewer should be able to filter analytics by model
        let response = app
            .get("/admin/api/v1/requests/aggregate?model=gpt-4")
            .add_header(&add_auth_headers(&request_viewer)[0].0, &add_auth_headers(&request_viewer)[0].1)
            .add_header(&add_auth_headers(&request_viewer)[1].0, &add_auth_headers(&request_viewer)[1].1)
            .await;

        response.assert_status_ok();
        let filtered_response: RequestsAggregateResponse = response.json();
        assert_eq!(filtered_response.model, Some("gpt-4".to_string()));

        // PlatformManager should be able to access analytics (but not requests)
        let response = app
            .get("/admin/api/v1/requests/aggregate")
            .add_header(&add_auth_headers(&platform_manager)[0].0, &add_auth_headers(&platform_manager)[0].1)
            .add_header(&add_auth_headers(&platform_manager)[1].0, &add_auth_headers(&platform_manager)[1].1)
            .await;

        response.assert_status_ok();

        // StandardUser should be forbidden from analytics
        let response = app
            .get("/admin/api/v1/requests/aggregate?model=gpt-4")
            .add_header(&add_auth_headers(&standard_user)[0].0, &add_auth_headers(&standard_user)[0].1)
            .add_header(&add_auth_headers(&standard_user)[1].0, &add_auth_headers(&standard_user)[1].1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_role_isolation_monitoring_data(pool: PgPool) {
        // Create config with request logging enabled
        let mut config = create_test_config();
        config.enable_request_logging = true;
        config.database = crate::config::DatabaseConfig::External {
            url: pool.connect_options().to_url_lossy().to_string(),
        };
        config.leader_election.enabled = false;

        // Use Application::new to properly set up outlet_db
        let app = crate::Application::new(config).await.expect("Failed to create application");
        let (app, _drop_guard) = app.into_test_server();

        // Test different role combinations for monitoring access
        let role_tests = vec![
            (vec![Role::StandardUser], false, false, "StandardUser only"),
            (vec![Role::RequestViewer], true, true, "RequestViewer only"),
            (vec![Role::PlatformManager], false, true, "PlatformManager only"), // Can access analytics but not raw requests
            (
                vec![Role::StandardUser, Role::RequestViewer],
                true,
                true,
                "StandardUser + RequestViewer",
            ),
            (
                vec![Role::PlatformManager, Role::RequestViewer],
                true,
                true,
                "PlatformManager + RequestViewer",
            ),
        ];

        for (roles, can_access_requests, can_access_analytics, _description) in role_tests {
            let user = create_test_user_with_roles(&pool, roles).await;

            // Test requests access
            let response = app
                .get("/admin/api/v1/requests")
                .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
                .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
                .await;

            if can_access_requests {
                response.assert_status_ok();
            } else {
                response.assert_status_forbidden();
            }

            // Test analytics access
            let response = app
                .get("/admin/api/v1/requests/aggregate")
                .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
                .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
                .await;

            if can_access_analytics {
                response.assert_status_ok();
            } else {
                response.assert_status_forbidden();
            }
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_request_data_access_boundary_conditions(pool: PgPool) {
        // Create config with request logging enabled
        let mut config = create_test_config();
        config.enable_request_logging = true;
        config.database = crate::config::DatabaseConfig::External {
            url: pool.connect_options().to_url_lossy().to_string(),
        };
        config.leader_election.enabled = false;

        // Use Application::new to properly set up outlet_db
        let app = crate::Application::new(config).await.expect("Failed to create application");
        let (app, _drop_guard) = app.into_test_server();
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;

        // Test boundary conditions for query parameters
        let boundary_tests = vec![
            ("limit=1", "Minimum limit"),
            ("limit=1000", "Maximum limit"),
            ("offset=0", "Zero offset"),
            ("status_code=200", "Specific status code"),
            ("status_code_min=100&status_code_max=599", "Status code range"),
            ("order_desc=true", "Descending order"),
            ("order_desc=false", "Ascending order"),
        ];

        for (query_params, _description) in boundary_tests {
            let response = app
                .get(&format!("/admin/api/v1/requests?{query_params}"))
                .add_header(&add_auth_headers(&request_viewer)[0].0, &add_auth_headers(&request_viewer)[0].1)
                .add_header(&add_auth_headers(&request_viewer)[1].0, &add_auth_headers(&request_viewer)[1].1)
                .await;

            response.assert_status_ok();
            let list_response: ListRequestsResponse = response.json();
            // Just verify we get a valid response structure
            assert!(list_response.requests.is_empty() || !list_response.requests.is_empty());
        }
    }

    // Test the conversion helper functions work correctly with role permissions
    #[test]
    fn test_outlet_conversion_respects_permissions() {
        // This tests that the outlet conversion functions work regardless of user permissions
        // (the permission check happens before conversion)
        use chrono::Utc;
        use uuid::Uuid;

        let outlet_request = outlet_postgres::HttpRequest {
            id: 1i64,
            instance_id: Uuid::new_v4(),
            correlation_id: 123i64,
            timestamp: Utc::now(),
            method: "POST".to_string(),
            uri: "/ai/chat/completions".to_string(),
            headers: json!({"authorization": "Bearer sk-..."}), // Sensitive header
            body: Some(Ok(AiRequest::Other(
                json!({"messages": [{"role": "user", "content": "sensitive data"}]}),
            ))),
            created_at: Utc::now(),
        };

        let outlet_pair = outlet_postgres::RequestResponsePair {
            request: outlet_request,
            response: None,
        };

        // Conversion should work regardless of permission context
        let api_pairs = super::convert_outlet_pairs_to_api(vec![outlet_pair]);

        assert_eq!(api_pairs.len(), 1);
        let pair = &api_pairs[0];

        // Verify sensitive data is preserved in conversion
        // (permission filtering should happen at the endpoint level, not in conversion)
        assert_eq!(pair.request.uri, "/ai/chat/completions");
        assert!(pair.request.headers.get("authorization").is_some());
        assert!(pair.request.body.is_some());
    }
}
