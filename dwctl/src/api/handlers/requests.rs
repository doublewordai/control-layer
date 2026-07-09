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
            RequestsAggregateResponse, UsageDateQuery, UserBatchUsageResponse,
        },
        users::CurrentUser,
    },
    auth::permissions::{RequiresPermission, operation, resource},
    db::handlers::analytics::{
        get_model_user_usage, get_realtime_tariffs, get_requests_aggregate, get_user_batch_count_for_range, get_user_batch_counts,
        get_user_model_breakdown, get_user_model_breakdown_for_range, list_http_analytics, refresh_user_model_usage_daily,
    },
    db::handlers::credits::Credits,
    errors::Error,
};
use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use utoipa::IntoParams;

/// Cache key: (user_id, optional start-day-timestamp, optional end-day-timestamp).
type UsageCacheKey = (Uuid, Option<i64>, Option<i64>);

/// Unified cache for user usage data (60-minute TTL).
/// All-time requests use (user_id, None, None). Date-filtered requests truncate
/// timestamps to midnight UTC so the same preset always hits cache.
static USAGE_CACHE: Lazy<Cache<UsageCacheKey, UserBatchUsageResponse>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(5_000)
        .time_to_live(std::time::Duration::from_secs(3600))
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

/// Get batch usage metrics for the current caller or a related user/org
///
/// Returns batch usage including total tokens, costs, request/batch counts,
/// and per-model breakdown. The default target is:
///
/// - the caller's `active_organization` when set (so a member who has switched
///   into an org context sees the org's aggregate rather than their personal
///   usage), or
/// - the caller's own user id otherwise.
///
/// Callers can pass `user_id` to drill into a specific subject:
/// - their own id (always allowed),
/// - their active org's id (always allowed for org members),
/// - another member of their active org (only allowed for owners/admins of
///   the org).
///
/// When `start_date` and/or `end_date` are provided, queries http_analytics directly
/// for the given range (capped at 180 days). Without date params, returns all-time
/// stats from pre-aggregated tables. Both paths use a shared 60-minute cache,
/// scoped per target user.
#[utoipa::path(
    get,
    path = "/admin/api/v1/usage",
    params(UsageDateQuery),
    responses(
        (status = 200, description = "User batch usage metrics", body = UserBatchUsageResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Caller is not allowed to view this user's usage"),
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
    Query(query): Query<UsageDateQuery>,
    State(state): State<AppState<P>>,
    current_user: CurrentUser,
) -> Result<Json<UserBatchUsageResponse>, Error> {
    let has_dates = query.start_date.is_some() || query.end_date.is_some();
    let refresh = query.refresh.unwrap_or(false);

    // Resolve the target user *before* building the cache key — different
    // targets must never share a cache entry.
    let target_user_id = resolve_usage_target(&state, &current_user, query.user_id).await?;

    // Build cache key: truncate dates to midnight UTC so preset windows always hit cache.
    // Skip cache for ranges under 30 days — the data moves too fast to cache usefully.
    let (cache_key, use_cache) = if has_dates {
        let end_date = query.end_date.unwrap_or_else(Utc::now);
        let start_date = query.start_date.unwrap_or_else(|| end_date - Duration::days(180));
        let span = end_date - start_date;
        let truncate = |dt: DateTime<Utc>| dt.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp();
        (
            (target_user_id, Some(truncate(start_date)), Some(truncate(end_date))),
            span.num_days() >= 30,
        )
    } else {
        ((target_user_id, None, None), true)
    };

    if refresh {
        USAGE_CACHE.invalidate(&cache_key).await;
    } else if use_cache && let Some(cached) = USAGE_CACHE.get(&cache_key).await {
        return Ok(Json(cached));
    }

    // Two paths: all-time uses fast pre-aggregated tables, date-filtered
    // queries http_analytics directly (bounded by covering index).
    let (batch_count, by_model, tariffs) = if has_dates {
        let end_date = query.end_date.unwrap_or_else(Utc::now);
        let start = query.start_date.unwrap_or_else(|| end_date - Duration::days(180));
        let max_start = end_date - Duration::days(180);
        let start_date = if start < max_start { max_start } else { start };

        tokio::try_join!(
            get_user_batch_count_for_range(state.db.read(), target_user_id, start_date, end_date),
            get_user_model_breakdown_for_range(state.db.read(), target_user_id, start_date, end_date),
            get_realtime_tariffs(state.db.read()),
        )?
    } else {
        // All-time usage combines two pre-aggregated tables:
        //
        // 1. `user_model_usage_daily` — per-day rollup incrementally folded from
        //    http_analytics by the usage-refresh daemon (cursor-based forward-fill).
        //    Tokens, cost, and request count are additive across days, so the all-time
        //    total is a SUM over every day for the user. Reads rely on the daemon's
        //    forward-fill (eventually consistent, sub-second under load); only ?refresh=true
        //    forces a synchronous fold before reading.
        //
        // 2. `batch_aggregates` — needed for batch *count* because counting
        //    distinct batches is NOT additive. A single batch's analytics
        //    rows can land in two different refresh windows (some rows
        //    processed in window N, the rest in window N+1), so an
        //    incremental COUNT(DISTINCT batch_id) per window would
        //    double-count that batch. Time-windowed queries avoid this by
        //    counting distinct IDs over a fixed timestamp range, but
        //    all-time has no fixed range — the cursor moves forward.
        //    `batch_aggregates` only contains completed batches (all rows
        //    written), so a simple COUNT(*) is safe.
        if refresh {
            refresh_user_model_usage_daily(state.db.write()).await?;
            let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
            Credits::new(&mut conn).aggregate_user_batches(target_user_id).await?;
        }
        let (batch_stats, by_model, tariffs) = tokio::try_join!(
            get_user_batch_counts(state.db.read(), target_user_id),
            get_user_model_breakdown(state.db.read(), target_user_id),
            get_realtime_tariffs(state.db.read()),
        )?;
        (batch_stats.0, by_model, tariffs)
    };

    let total_cost = by_model
        .iter()
        .fold(Decimal::ZERO, |acc, e| acc + e.cost.parse::<Decimal>().unwrap_or(Decimal::ZERO))
        .to_string();
    let total_requests: i64 = by_model.iter().map(|e| e.request_count).sum();
    let avg_requests_per_batch = if batch_count > 0 {
        total_requests as f64 / batch_count as f64
    } else {
        0.0
    };

    let mut total_input_tokens: i64 = 0;
    let mut total_output_tokens: i64 = 0;
    let mut total_request_count: i64 = 0;
    let mut estimated_realtime_cost = Decimal::ZERO;
    for entry in &by_model {
        total_input_tokens += entry.input_tokens;
        total_output_tokens += entry.output_tokens;
        total_request_count += entry.request_count;
        if let Some(&(input_price, output_price)) = tariffs.get(&entry.model) {
            estimated_realtime_cost += Decimal::from(entry.input_tokens) * input_price + Decimal::from(entry.output_tokens) * output_price;
        }
    }

    let usage = UserBatchUsageResponse {
        total_input_tokens,
        total_output_tokens,
        total_request_count,
        total_batch_count: batch_count,
        avg_requests_per_batch,
        total_cost,
        estimated_realtime_cost: estimated_realtime_cost.to_string(),
        by_model,
    };

    if use_cache {
        USAGE_CACHE.insert(cache_key, usage.clone()).await;
    }

    Ok(Json(usage))
}

/// Resolve the `user_id` the `/usage` endpoint should aggregate against, with
/// authorization. Mirrors the rules described on the route handler:
///
/// - When `requested` is `None`, fall back to the caller's active organization
///   (so org-context callers see org-wide usage by default) and then to the
///   caller's own id.
/// - When `requested == current_user.id` or `requested == active_organization`
///   (any role inside that org), pass it through.
/// - Otherwise the request is only honored when the caller is an owner or
///   admin of their active organization *and* the requested id is a member
///   of that organization — that's the per-member drill-down case.
async fn resolve_usage_target<P: PoolProvider>(
    state: &AppState<P>,
    current_user: &CurrentUser,
    requested: Option<crate::types::UserId>,
) -> Result<crate::types::UserId, Error> {
    let default_target = current_user.active_organization.unwrap_or(current_user.id);
    let Some(requested) = requested else {
        return Ok(default_target);
    };

    // Self is always allowed — no DB round-trip needed.
    if requested == current_user.id {
        return Ok(requested);
    }

    // Every remaining authorisation path requires the caller to be in an org
    // context. Without one we can short-circuit to 403 before acquiring a
    // DB connection (avoids a pool round-trip on every malformed `user_id`
    // request from a personal-context caller).
    let Some(active_org) = current_user.active_organization else {
        return Err(unauthorized_usage_target_error(requested));
    };

    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = crate::db::handlers::Organizations::new(&mut conn);
    let actor_role = repo.get_user_org_role(current_user.id, active_org).await?;
    // Non-members of the active org get the same 403 as a personal-context
    // caller — they're outside the org's authority surface entirely.
    if actor_role.is_none() {
        return Err(unauthorized_usage_target_error(requested));
    }

    // The active org itself is queryable by any of its members.
    if requested == active_org {
        return Ok(requested);
    }

    // Per-member drill-down: requires owner/admin role on the active org
    // *and* the requested id to actually be a member of it.
    let target_role = repo.get_user_org_role(requested, active_org).await?;
    if matches!(actor_role.as_deref(), Some("owner" | "admin")) && target_role.is_some() {
        return Ok(requested);
    }

    Err(unauthorized_usage_target_error(requested))
}

/// Single source of truth for the 403 a caller gets when they request a
/// `user_id` they aren't allowed to read. The permission label points at
/// `Resource::Requests` + `Operation::ReadAll` — the closest analogue to
/// "read someone else's request history", which is what the per-member
/// drill-down case actually is. The previous `ReadOwn` label made the
/// log line look like a normal self-read failure, which it isn't.
fn unauthorized_usage_target_error(requested: crate::types::UserId) -> Error {
    Error::InsufficientPermissions {
        required: crate::types::Permission::Allow(crate::types::Resource::Requests, crate::types::Operation::ReadAll),
        action: crate::types::Operation::ReadAll,
        resource: format!("Usage for user {requested}"),
    }
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

    /// Helper: insert an http_analytics row attributed to a specific user_id.
    /// Used by usage-scope tests to seed distinct totals for an org vs a member.
    async fn insert_analytics_for_user(pool: &PgPool, user_id: uuid::Uuid, prompt_tokens: i64, completion_tokens: i64) {
        let correlation_id = CORRELATION_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
        sqlx::query!(
            r#"
            INSERT INTO http_analytics (
                instance_id, correlation_id, timestamp, uri, method, status_code, duration_ms,
                model, prompt_tokens, completion_tokens, total_tokens, user_id
            ) VALUES ($1, $2, $3, '/ai/chat/completions', 'POST', 200, 100, 'gpt-4', $4, $5, $6, $7)
            "#,
            uuid::Uuid::new_v4(),
            correlation_id,
            chrono::Utc::now() - chrono::Duration::minutes(5),
            prompt_tokens,
            completion_tokens,
            prompt_tokens + completion_tokens,
            user_id,
        )
        .execute(pool)
        .await
        .expect("Failed to insert test analytics row");
    }

    /// Build a `/usage` URL with the standard one-hour-ago window and an
    /// optional `user_id`. Centralised so the per-test cases describe what
    /// they're checking rather than the date-format ceremony (chrono's
    /// `to_rfc3339()` emits `+00:00`, where the `+` is decoded as a space
    /// by the query-string parser — the `Z` suffix avoids it).
    fn build_usage_url(user_id: Option<uuid::Uuid>) -> String {
        let start = (Utc::now() - Duration::hours(1)).format("%Y-%m-%dT%H:%M:%SZ");
        let end = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        match user_id {
            Some(id) => format!("/admin/api/v1/usage?start_date={start}&end_date={end}&user_id={id}"),
            None => format!("/admin/api/v1/usage?start_date={start}&end_date={end}"),
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_usage_defaults_to_active_organization(pool: PgPool) {
        // Org-context callers should see the org's aggregate by default,
        // not their personal usage.
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let org = crate::test::utils::create_test_org(&pool, user.id).await;

        // Distinct seed amounts so we can verify which subject the handler hit.
        insert_analytics_for_user(&pool, user.id, 100, 50).await;
        insert_analytics_for_user(&pool, org.id, 1000, 500).await;

        let org_cookie = format!("dw_active_org={}", org.id);
        let auth = add_auth_headers(&user);

        let response = app
            .get(&build_usage_url(None))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .add_header("cookie", &org_cookie)
            .await;

        response.assert_status_ok();
        let body: UserBatchUsageResponse = response.json();
        assert_eq!(
            body.total_input_tokens, 1000,
            "should return the org's tokens, not the human user's"
        );
        assert_eq!(body.total_output_tokens, 500);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_usage_user_id_filter_to_self(pool: PgPool) {
        // Callers can always pass their own id to override the org-default
        // and see their personal usage instead.
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let org = crate::test::utils::create_test_org(&pool, user.id).await;

        insert_analytics_for_user(&pool, user.id, 100, 50).await;
        insert_analytics_for_user(&pool, org.id, 1000, 500).await;

        let org_cookie = format!("dw_active_org={}", org.id);
        let auth = add_auth_headers(&user);

        let response = app
            .get(&build_usage_url(Some(user.id)))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .add_header("cookie", &org_cookie)
            .await;

        response.assert_status_ok();
        let body: UserBatchUsageResponse = response.json();
        assert_eq!(body.total_input_tokens, 100);
        assert_eq!(body.total_output_tokens, 50);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_usage_admin_can_filter_to_member(pool: PgPool) {
        // Org admins / owners can drill into a specific member's id.
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let member = create_test_user(&pool, Role::StandardUser).await;
        let org = crate::test::utils::create_test_org(&pool, owner.id).await;
        crate::test::utils::add_org_member(&pool, org.id, member.id, "member").await;

        insert_analytics_for_user(&pool, member.id, 25, 25).await;

        let org_cookie = format!("dw_active_org={}", org.id);
        let auth = add_auth_headers(&owner);

        let response = app
            .get(&build_usage_url(Some(member.id)))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .add_header("cookie", &org_cookie)
            .await;

        response.assert_status_ok();
        let body: UserBatchUsageResponse = response.json();
        assert_eq!(body.total_input_tokens, 25);
        assert_eq!(body.total_output_tokens, 25);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_usage_non_admin_cannot_filter_to_other_member(pool: PgPool) {
        // Regular members can't see another member's usage even within the
        // same org — the handler must 403.
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let member_a = create_test_user(&pool, Role::StandardUser).await;
        let member_b = create_test_user(&pool, Role::StandardUser).await;
        let org = crate::test::utils::create_test_org(&pool, owner.id).await;
        crate::test::utils::add_org_member(&pool, org.id, member_a.id, "member").await;
        crate::test::utils::add_org_member(&pool, org.id, member_b.id, "member").await;

        let org_cookie = format!("dw_active_org={}", org.id);
        let auth = add_auth_headers(&member_a);

        let response = app
            .get(&build_usage_url(Some(member_b.id)))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .add_header("cookie", &org_cookie)
            .await;

        response.assert_status(axum::http::StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_usage_personal_context_other_user_id_forbidden(pool: PgPool) {
        // Personal-context callers (no `active_organization`) shouldn't be
        // able to request a `user_id` other than their own. This case also
        // covers the short-circuit early-return that avoids a DB acquire
        // for malformed requests from non-org callers.
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let bob = create_test_user(&pool, Role::StandardUser).await;

        let auth = add_auth_headers(&alice);
        let response = app
            .get(&build_usage_url(Some(bob.id)))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::FORBIDDEN);
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

    /// Insert an `http_analytics` row with an explicit `user_id`. The
    /// general `insert_test_analytics` helper above intentionally leaves
    /// `user_id` NULL because most tests don't care about ownership; the
    /// usage handler does care, so we need a variant that sets it.
    async fn insert_test_analytics_for_user(
        pool: &PgPool,
        user_id: uuid::Uuid,
        timestamp: chrono::DateTime<chrono::Utc>,
        model: &str,
        prompt_tokens: i64,
        completion_tokens: i64,
    ) {
        let correlation_id = CORRELATION_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
        sqlx::query!(
            r#"
            INSERT INTO http_analytics (
                instance_id, correlation_id, timestamp, uri, method, status_code, duration_ms,
                model, prompt_tokens, completion_tokens, total_tokens, user_id
            ) VALUES ($1, $2, $3, '/ai/chat/completions', 'POST', 200, 100, $4, $5, $6, $7, $8)
            "#,
            uuid::Uuid::new_v4(),
            correlation_id,
            timestamp,
            model,
            prompt_tokens,
            completion_tokens,
            prompt_tokens + completion_tokens,
            user_id,
        )
        .execute(pool)
        .await
        .expect("Failed to insert test analytics data with user_id");
    }

    /// Regression test for org-context scoping in the `/admin/api/v1/usage`
    /// handler. The handler previously read `current_user.id` directly for
    /// every query and cache key, so usage figures never changed when the
    /// user switched into an org — they always saw their personal numbers.
    ///
    /// We exercise the date-filtered path because it reads `http_analytics`
    /// directly (with `WHERE user_id = $1`), avoiding the need to run the
    /// background `refresh_user_model_usage` / `aggregate_user_batches`
    /// jobs against the test pool. Two analytics rows are seeded — one for
    /// the PM, one for the org — and the test asserts the response only
    /// contains the model attributed to the active context.
    #[sqlx::test]
    #[test_log::test]
    async fn test_get_usage_pm_in_org_context_scopes_to_org(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;

        let pm_user = create_test_user_with_roles(&pool, vec![Role::PlatformManager, Role::StandardUser, Role::BatchAPIUser]).await;
        let org = create_test_org(&pool, pm_user.id).await;
        let auth = add_auth_headers(&pm_user);

        // Seed two analytics rows with distinct (user_id, model) pairs so
        // we can tell which context is "winning" just by reading off the
        // single model alias in by_model.
        let one_hour_ago = Utc::now() - Duration::hours(1);
        insert_test_analytics_for_user(&pool, pm_user.id, one_hour_ago, "personal-model", 50, 25).await;
        insert_test_analytics_for_user(&pool, org.id, one_hour_ago, "org-model", 100, 50).await;

        // Date-filtered request — bypasses the all-time pre-aggregated path
        // so we don't depend on the cursor-driven refresh job running.
        // Use the `Z`-suffix RFC3339 form so the querystring needs no
        // percent-encoding ("+" in numeric-offset RFC3339 timestamps would
        // be misread as a space by the form parser).
        use chrono::SecondsFormat;
        let start = (one_hour_ago - Duration::minutes(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
        let end = (Utc::now() + Duration::minutes(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
        let url = format!("/admin/api/v1/usage?start_date={}&end_date={}", start, end);

        // PM in personal context → personal-model only.
        let personal_resp = app
            .get(&url)
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;
        personal_resp.assert_status_ok();
        let personal_body: serde_json::Value = personal_resp.json();
        let personal_models = personal_body["by_model"].as_array().expect("by_model array");
        assert_eq!(
            personal_models.len(),
            1,
            "personal context should see only the human user's analytics",
        );
        assert_eq!(personal_models[0]["model"], "personal-model");

        // PM in org context → org-model only. This is the contract that
        // regressed before the fix; if `get_usage` ever stops threading
        // `active_organization` through `target_user_id` this fails.
        let org_cookie = format!("dw_active_org={}", org.id);
        let org_resp = app
            .get(&url)
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .add_header("cookie", &org_cookie)
            .await;
        org_resp.assert_status_ok();
        let org_body: serde_json::Value = org_resp.json();
        let org_models = org_body["by_model"].as_array().expect("by_model array");
        assert_eq!(
            org_models.len(),
            1,
            "org context should see only the org's analytics, not the human user's",
        );
        assert_eq!(org_models[0]["model"], "org-model");
    }
}
