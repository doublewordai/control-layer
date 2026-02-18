//! Request logging handlers
//!
//! Endpoints for querying HTTP analytics data from the http_analytics table.

use axum::{
    extract::{Query, State},
    response::Json,
};
use moka::future::Cache;
use once_cell::sync::Lazy;
use sqlx_pool_router::PoolProvider;
use uuid::Uuid;

use crate::{
    AppState,
    api::models::{
        requests::{
            AggregateRequestsQuery, HttpAnalyticsFilter, ListAnalyticsResponse, ListRequestsQuery, ModelUserUsageResponse,
            RequestsAggregateResponse, UserBatchUsageResponse,
        },
        users::CurrentUser,
    },
    auth::permissions::{RequiresPermission, operation, resource},
    db::handlers::analytics::{
        estimate_realtime_cost, get_model_user_usage, get_requests_aggregate, get_user_batch_counts,
        get_user_model_breakdown, list_http_analytics, refresh_user_model_usage,
    },
    errors::Error,
};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use utoipa::IntoParams;

/// Cache for user usage data (60 second TTL, keyed by user ID)
static USAGE_CACHE: Lazy<Cache<Uuid, UserBatchUsageResponse>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(1_000)
        .time_to_live(std::time::Duration::from_secs(60))
        .build()
});

/// List HTTP analytics entries with filtering and pagination
///
/// Returns a paginated list of HTTP analytics entries from the http_analytics table,
/// with optional filtering by model, batch ID, status code, duration, and time range.
#[utoipa::path(
    get,
    path = "/admin/api/v1/requests",
    params(ListRequestsQuery),
    responses(
        (status = 200, description = "List of analytics entries", body = ListAnalyticsResponse),
        (status = 400, description = "Invalid query parameters"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "requests",
)]
#[tracing::instrument(skip_all)]
pub async fn list_requests<P: PoolProvider>(
    Query(query): Query<ListRequestsQuery>,
    State(state): State<AppState<P>>,
    _: RequiresPermission<resource::Requests, operation::ReadAll>,
) -> Result<Json<ListAnalyticsResponse>, Error> {
    // Validate and apply limits
    let (skip, limit) = query.pagination.params();

    // Build filter from query parameters
    let filter = HttpAnalyticsFilter {
        method: query.method,
        uri_pattern: query.uri_pattern,
        status_code: query.status_code,
        status_code_min: query.status_code_min,
        status_code_max: query.status_code_max,
        min_duration_ms: query.min_duration_ms,
        max_duration_ms: query.max_duration_ms,
        timestamp_after: query.timestamp_after,
        timestamp_before: query.timestamp_before,
        model: query.model,
        fusillade_batch_id: query.fusillade_batch_id,
        custom_id: query.custom_id,
    };

    // Query the http_analytics table - use read replica for analytics
    let entries = list_http_analytics(state.db.read(), skip, limit, query.order_desc.unwrap_or(true), filter).await?;

    Ok(Json(ListAnalyticsResponse { entries }))
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
        (status = 500, description = "Internal server error"),
    ),
    tag = "requests",
)]
#[tracing::instrument(skip_all)]
pub async fn aggregate_requests<P: PoolProvider>(
    Query(query): Query<AggregateRequestsQuery>,
    State(state): State<AppState<P>>,
    _: RequiresPermission<resource::Analytics, operation::ReadAll>,
) -> Result<Json<RequestsAggregateResponse>, Error> {
    // Use provided timestamps or default to last 24 hours
    let now = chrono::Utc::now();
    let time_range_start = query.timestamp_after.unwrap_or_else(|| now - chrono::Duration::hours(24));
    let time_range_end = query.timestamp_before.unwrap_or(now);
    let model_filter = query.model.as_deref();

    // Get aggregated analytics data from http_analytics table - use read replica for analytics
    let response = get_requests_aggregate(state.db.read(), time_range_start, time_range_end, model_filter).await?;

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
        (status = 400, description = "Model parameter is required"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "requests",
)]
#[tracing::instrument(skip_all)]
pub async fn aggregate_by_user<P: PoolProvider>(
    Query(query): Query<AggregateByUserQuery>,
    State(state): State<AppState<P>>,
    _: RequiresPermission<resource::Analytics, operation::ReadAll>,
) -> Result<Json<ModelUserUsageResponse>, Error> {
    // Model is required for this endpoint
    let model_alias = query.model.ok_or_else(|| Error::BadRequest {
        message: "Model parameter is required".to_string(),
    })?;

    // Set default date range
    let end_date = query.end_date.unwrap_or_else(Utc::now);
    let start_date = query.start_date.unwrap_or_else(|| end_date - Duration::hours(24));

    // Get usage data from http_analytics table - use read replica for analytics
    let usage_data = get_model_user_usage(state.db.read(), &model_alias, start_date, end_date).await?;

    Ok(Json(usage_data))
}

/// Get the current user's batch usage metrics
///
/// Returns all-time batch usage including total tokens, costs, request/batch counts,
/// and per-model breakdown. Only includes batched requests. Any authenticated user
/// can access their own usage data.
#[utoipa::path(
    get,
    path = "/admin/api/v1/usage",
    responses(
        (status = 200, description = "User batch usage metrics", body = UserBatchUsageResponse),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "usage",
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_usage<P: PoolProvider>(
    State(state): State<AppState<P>>,
    current_user: CurrentUser,
) -> Result<Json<UserBatchUsageResponse>, Error> {
    if let Some(cached) = USAGE_CACHE.get(&current_user.id).await {
        return Ok(Json(cached));
    }

    let write_pool = state.db.write();
    refresh_user_model_usage(write_pool).await?;

    let read_pool = state.db.read();
    let ((total_batch_count, avg_requests_per_batch, total_cost), by_model, estimated_realtime_cost) = tokio::try_join!(
        get_user_batch_counts(read_pool, current_user.id),
        get_user_model_breakdown(write_pool, current_user.id),
        estimate_realtime_cost(write_pool, current_user.id),
    )?;

    // Derive token/request totals from per-model breakdown
    let mut total_input_tokens: i64 = 0;
    let mut total_output_tokens: i64 = 0;
    let mut total_request_count: i64 = 0;
    for entry in &by_model {
        total_input_tokens += entry.input_tokens;
        total_output_tokens += entry.output_tokens;
        total_request_count += entry.request_count;
    }

    let usage = UserBatchUsageResponse {
        total_input_tokens,
        total_output_tokens,
        total_request_count,
        total_batch_count,
        avg_requests_per_batch,
        total_cost,
        estimated_realtime_cost,
        by_model,
    };

    USAGE_CACHE.insert(current_user.id, usage.clone()).await;

    Ok(Json(usage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::models::requests::ListAnalyticsResponse, api::models::users::Role, test::utils::*};
    use chrono::{Duration, Utc};
    use sqlx::PgPool;
    use std::sync::atomic::{AtomicI64, Ordering};

    // Atomic counter to ensure unique correlation_ids across tests
    static CORRELATION_ID_COUNTER: AtomicI64 = AtomicI64::new(1);

    // Test analytics data parameters
    struct TestAnalyticsData<'a> {
        timestamp: chrono::DateTime<chrono::Utc>,
        model: &'a str,
        status_code: i32,
        duration_ms: f64,
        prompt_tokens: i64,
        completion_tokens: i64,
        fusillade_batch_id: Option<uuid::Uuid>,
    }

    // Helper function to insert test analytics data
    async fn insert_test_analytics(pool: &PgPool, data: TestAnalyticsData<'_>) {
        use uuid::Uuid;

        let correlation_id = CORRELATION_ID_COUNTER.fetch_add(1, Ordering::SeqCst);

        sqlx::query!(
            r#"
            INSERT INTO http_analytics (
                instance_id, correlation_id, timestamp, uri, method, status_code, duration_ms,
                model, prompt_tokens, completion_tokens, total_tokens, fusillade_batch_id
            ) VALUES ($1, $2, $3, '/ai/chat/completions', 'POST', $4, $5, $6, $7, $8, $9, $10)
            "#,
            Uuid::new_v4(),
            correlation_id,
            data.timestamp,
            data.status_code,
            data.duration_ms as i64,
            data.model,
            data.prompt_tokens,
            data.completion_tokens,
            data.prompt_tokens + data.completion_tokens,
            data.fusillade_batch_id
        )
        .execute(pool)
        .await
        .expect("Failed to insert test analytics data");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_requests_unauthorized(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

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
    async fn test_list_requests_success_empty(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::RequestViewer).await;

        let response = app
            .get("/admin/api/v1/requests")
            .add_header(&add_auth_headers(&admin_user)[0].0, &add_auth_headers(&admin_user)[0].1)
            .add_header(&add_auth_headers(&admin_user)[1].0, &add_auth_headers(&admin_user)[1].1)
            .await;

        response.assert_status_ok();
        let list_response: ListAnalyticsResponse = response.json();
        assert!(list_response.entries.is_empty());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_requests_with_data(pool: PgPool) {
        // Insert test data
        let base_time = Utc::now() - Duration::hours(1);
        insert_test_analytics(
            &pool,
            TestAnalyticsData {
                timestamp: base_time,
                model: "gpt-4",
                status_code: 200,
                duration_ms: 100.0,
                prompt_tokens: 50,
                completion_tokens: 25,
                fusillade_batch_id: None,
            },
        )
        .await;
        insert_test_analytics(
            &pool,
            TestAnalyticsData {
                timestamp: base_time,
                model: "claude-3",
                status_code: 200,
                duration_ms: 150.0,
                prompt_tokens: 75,
                completion_tokens: 35,
                fusillade_batch_id: None,
            },
        )
        .await;

        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::RequestViewer).await;

        let response = app
            .get("/admin/api/v1/requests")
            .add_header(&add_auth_headers(&admin_user)[0].0, &add_auth_headers(&admin_user)[0].1)
            .add_header(&add_auth_headers(&admin_user)[1].0, &add_auth_headers(&admin_user)[1].1)
            .await;

        response.assert_status_ok();
        let list_response: ListAnalyticsResponse = response.json();
        assert_eq!(list_response.entries.len(), 2);

        // Verify entries have expected fields
        for entry in &list_response.entries {
            assert!(entry.model.is_some());
            assert_eq!(entry.status_code, Some(200));
            assert!(entry.duration_ms.is_some());
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_requests_with_model_filter(pool: PgPool) {
        // Insert test data for multiple models
        let base_time = Utc::now() - Duration::hours(1);
        insert_test_analytics(
            &pool,
            TestAnalyticsData {
                timestamp: base_time,
                model: "gpt-4",
                status_code: 200,
                duration_ms: 100.0,
                prompt_tokens: 50,
                completion_tokens: 25,
                fusillade_batch_id: None,
            },
        )
        .await;
        insert_test_analytics(
            &pool,
            TestAnalyticsData {
                timestamp: base_time,
                model: "gpt-4",
                status_code: 200,
                duration_ms: 120.0,
                prompt_tokens: 60,
                completion_tokens: 30,
                fusillade_batch_id: None,
            },
        )
        .await;
        insert_test_analytics(
            &pool,
            TestAnalyticsData {
                timestamp: base_time,
                model: "claude-3",
                status_code: 200,
                duration_ms: 150.0,
                prompt_tokens: 75,
                completion_tokens: 35,
                fusillade_batch_id: None,
            },
        )
        .await;

        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::RequestViewer).await;

        // Filter by model
        let response = app
            .get("/admin/api/v1/requests?model=gpt-4")
            .add_header(&add_auth_headers(&admin_user)[0].0, &add_auth_headers(&admin_user)[0].1)
            .add_header(&add_auth_headers(&admin_user)[1].0, &add_auth_headers(&admin_user)[1].1)
            .await;

        response.assert_status_ok();
        let list_response: ListAnalyticsResponse = response.json();
        assert_eq!(list_response.entries.len(), 2);
        assert!(list_response.entries.iter().all(|e| e.model.as_deref() == Some("gpt-4")));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_requests_with_batch_filter(pool: PgPool) {
        use uuid::Uuid;

        let batch_id = Uuid::new_v4();
        let other_batch_id = Uuid::new_v4();
        let base_time = Utc::now() - Duration::hours(1);

        // Insert data for different batches
        insert_test_analytics(
            &pool,
            TestAnalyticsData {
                timestamp: base_time,
                model: "gpt-4",
                status_code: 200,
                duration_ms: 100.0,
                prompt_tokens: 50,
                completion_tokens: 25,
                fusillade_batch_id: Some(batch_id),
            },
        )
        .await;
        insert_test_analytics(
            &pool,
            TestAnalyticsData {
                timestamp: base_time,
                model: "gpt-4",
                status_code: 200,
                duration_ms: 120.0,
                prompt_tokens: 60,
                completion_tokens: 30,
                fusillade_batch_id: Some(batch_id),
            },
        )
        .await;
        insert_test_analytics(
            &pool,
            TestAnalyticsData {
                timestamp: base_time,
                model: "claude-3",
                status_code: 200,
                duration_ms: 150.0,
                prompt_tokens: 75,
                completion_tokens: 35,
                fusillade_batch_id: Some(other_batch_id),
            },
        )
        .await;
        insert_test_analytics(
            &pool,
            TestAnalyticsData {
                timestamp: base_time,
                model: "claude-3",
                status_code: 200,
                duration_ms: 160.0,
                prompt_tokens: 80,
                completion_tokens: 40,
                fusillade_batch_id: None,
            },
        )
        .await;

        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::RequestViewer).await;

        // Filter by batch ID
        let response = app
            .get(&format!("/admin/api/v1/requests?fusillade_batch_id={}", batch_id))
            .add_header(&add_auth_headers(&admin_user)[0].0, &add_auth_headers(&admin_user)[0].1)
            .add_header(&add_auth_headers(&admin_user)[1].0, &add_auth_headers(&admin_user)[1].1)
            .await;

        response.assert_status_ok();
        let list_response: ListAnalyticsResponse = response.json();
        assert_eq!(list_response.entries.len(), 2);
        assert!(list_response.entries.iter().all(|e| e.fusillade_batch_id == Some(batch_id)));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_aggregate_requests_success(pool: PgPool) {
        // Insert analytics data to test aggregate functionality
        let base_time = Utc::now() - Duration::hours(1);
        insert_test_analytics(
            &pool,
            TestAnalyticsData {
                timestamp: base_time,
                model: "gpt-4",
                status_code: 200,
                duration_ms: 100.0,
                prompt_tokens: 50,
                completion_tokens: 25,
                fusillade_batch_id: None,
            },
        )
        .await;

        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        let response = app
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
        insert_test_analytics(
            &pool,
            TestAnalyticsData {
                timestamp: base_time,
                model: "gpt-4",
                status_code: 200,
                duration_ms: 100.0,
                prompt_tokens: 50,
                completion_tokens: 25,
                fusillade_batch_id: None,
            },
        )
        .await;
        insert_test_analytics(
            &pool,
            TestAnalyticsData {
                timestamp: base_time,
                model: "claude-3",
                status_code: 200,
                duration_ms: 150.0,
                prompt_tokens: 75,
                completion_tokens: 35,
                fusillade_batch_id: None,
            },
        )
        .await;

        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        let response = app
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
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let response = app
            .get("/admin/api/v1/requests/aggregate")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        // Should be forbidden since user doesn't have Analytics:Read permission
        response.assert_status(axum::http::StatusCode::FORBIDDEN);
    }

    // Unit tests for helper types
    #[test]
    fn test_list_requests_query_default() {
        let query = ListRequestsQuery::default();
        assert_eq!(query.pagination.skip(), 0);
        assert_eq!(query.pagination.limit(), 10); // DEFAULT_LIMIT from pagination module
        assert_eq!(query.order_desc, Some(true));
        assert!(query.method.is_none());
        assert!(query.uri_pattern.is_none());
        assert!(query.status_code.is_none());
        assert!(query.model.is_none());
        assert!(query.fusillade_batch_id.is_none());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_standard_user_cannot_access_requests(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
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
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;

        // RequestViewer should be able to list requests (has ReadAll for Requests)
        let response = app
            .get("/admin/api/v1/requests")
            .add_header(&add_auth_headers(&request_viewer)[0].0, &add_auth_headers(&request_viewer)[0].1)
            .add_header(&add_auth_headers(&request_viewer)[1].0, &add_auth_headers(&request_viewer)[1].1)
            .await;

        response.assert_status_ok();
        let _list_response: ListAnalyticsResponse = response.json();

        // RequestViewer should be able to access aggregated requests (has ReadAll for Analytics)
        let response = app
            .get("/admin/api/v1/requests/aggregate")
            .add_header(&add_auth_headers(&request_viewer)[0].0, &add_auth_headers(&request_viewer)[0].1)
            .add_header(&add_auth_headers(&request_viewer)[1].0, &add_auth_headers(&request_viewer)[1].1)
            .await;

        response.assert_status_ok();
        let _aggregate_response: RequestsAggregateResponse = response.json();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_cannot_access_raw_requests(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_user(&pool, Role::PlatformManager).await;

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
    async fn test_requests_query_parameters(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;

        // Test various query parameter combinations
        let test_queries = vec![
            "limit=10&skip=0",
            "method=POST",
            "status_code=200",
            "status_code_min=200&status_code_max=299",
            "order_desc=true",
            "order_desc=false",
            "model=gpt-4",
        ];

        for query in test_queries {
            let response = app
                .get(&format!("/admin/api/v1/requests?{}", query))
                .add_header(&add_auth_headers(&request_viewer)[0].0, &add_auth_headers(&request_viewer)[0].1)
                .add_header(&add_auth_headers(&request_viewer)[1].0, &add_auth_headers(&request_viewer)[1].1)
                .await;

            response.assert_status_ok();
            let _: ListAnalyticsResponse = response.json();
        }
    }
}
