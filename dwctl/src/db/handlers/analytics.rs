//! Database queries for request analytics and aggregation.

use chrono::{DateTime, Duration, Timelike, Utc};
use moka::future::Cache;
use once_cell::sync::Lazy;
use sqlx::{FromRow, PgPool};
use std::collections::HashMap;
use tracing::instrument;

use crate::{
    api::models::{
        batches::BatchAnalytics,
        deployments::{ModelMetrics, ModelTimeSeriesPoint},
        requests::{
            AnalyticsEntry, HttpAnalyticsFilter, ModelBreakdownEntry, ModelUsage, ModelUserUsageResponse, RequestsAggregateResponse,
            StatusCodeBreakdown, TimeSeriesPoint, UserUsage,
        },
    },
    db::errors::Result,
};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use uuid::Uuid;

/// Global cache for model metrics (60 second TTL)
static METRICS_CACHE: Lazy<Cache<String, HashMap<String, ModelMetrics>>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(100)
        .time_to_live(std::time::Duration::from_secs(60))
        .build()
});

/// Time granularity for analytics queries
#[derive(Debug, Clone, Copy)]
pub enum TimeGranularity {
    /// 10-minute intervals
    TenMinutes,
    /// 1-hour intervals
    Hour,
}

/// Time series data from analytics query
#[derive(FromRow)]
struct TimeSeriesRow {
    pub timestamp: Option<DateTime<Utc>>,
    pub requests_count: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub avg_latency_ms: Option<f64>,
    pub p95_latency_ms: Option<f64>,
    pub p99_latency_ms: Option<f64>,
}

/// Status code breakdown from analytics query
#[derive(FromRow)]
struct StatusCodeRow {
    pub status_code: Option<i32>,
    pub status_count: Option<i64>,
}

/// Model usage data from analytics query
#[derive(FromRow)]
struct ModelUsageRow {
    pub model_name: Option<String>,
    pub model_count: Option<i64>,
    pub model_avg_latency_ms: Option<f64>,
}

/// Total requests count
#[derive(FromRow)]
struct TotalRequestsRow {
    pub total_requests: Option<i64>,
}

/// Model metrics aggregation from analytics query
#[derive(FromRow)]
struct ModelMetricsRow {
    pub model: Option<String>,
    pub total_requests: Option<i64>,
    pub avg_latency_ms: Option<f64>,
    pub total_input_tokens: Option<i64>,
    pub total_output_tokens: Option<i64>,
    pub last_active_at: Option<DateTime<Utc>>,
}

/// Time series data for bulk queries (includes model name)
#[derive(FromRow)]
struct BulkTimeSeriesRow {
    pub model: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
    pub requests_count: Option<i64>,
}

/// Get total request count
#[instrument(skip(db), err)]
async fn get_total_requests(
    db: &PgPool,
    time_range_start: DateTime<Utc>,
    time_range_end: DateTime<Utc>,
    model_filter: Option<&str>,
) -> Result<i64> {
    let total_requests = if let Some(model) = model_filter {
        sqlx::query_as!(
            TotalRequestsRow,
            "SELECT COUNT(*) as total_requests FROM http_analytics WHERE timestamp >= $1 AND timestamp <= $2 AND model = $3",
            time_range_start,
            time_range_end,
            model
        )
        .fetch_one(db)
        .await?
        .total_requests
        .unwrap_or(0)
    } else {
        sqlx::query_as!(
            TotalRequestsRow,
            "SELECT COUNT(*) as total_requests FROM http_analytics WHERE timestamp >= $1 AND timestamp <= $2",
            time_range_start,
            time_range_end
        )
        .fetch_one(db)
        .await?
        .total_requests
        .unwrap_or(0)
    };
    Ok(total_requests)
}

/// Get time series data with configurable granularity
#[instrument(skip(db), err)]
async fn get_time_series(
    db: &PgPool,
    time_range_start: DateTime<Utc>,
    time_range_end: DateTime<Utc>,
    model_filter: Option<&str>,
    granularity: TimeGranularity,
) -> Result<Vec<TimeSeriesPoint>> {
    match granularity {
        TimeGranularity::Hour => get_time_series_hourly(db, time_range_start, time_range_end, model_filter).await,
        TimeGranularity::TenMinutes => get_time_series_ten_minutes(db, time_range_start, time_range_end, model_filter).await,
    }
}

/// Get time series data with hourly granularity (existing implementation)
#[instrument(skip(db), err)]
async fn get_time_series_hourly(
    db: &PgPool,
    time_range_start: DateTime<Utc>,
    time_range_end: DateTime<Utc>,
    model_filter: Option<&str>,
) -> Result<Vec<TimeSeriesPoint>> {
    let rows = if let Some(model) = model_filter {
        sqlx::query_as!(
            TimeSeriesRow,
            r#"
            SELECT
                date_trunc('hour', timestamp) as timestamp,
                COUNT(*) as requests_count,
                COALESCE(SUM(prompt_tokens), 0)::bigint as input_tokens,
                COALESCE(SUM(completion_tokens), 0)::bigint as output_tokens,
                AVG(duration_ms)::float8 as avg_latency_ms,
                PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY duration_ms)::float8 as p95_latency_ms,
                PERCENTILE_CONT(0.99) WITHIN GROUP (ORDER BY duration_ms)::float8 as p99_latency_ms
            FROM http_analytics
            WHERE timestamp >= $1 AND timestamp <= $2 AND model = $3
            GROUP BY date_trunc('hour', timestamp)
            ORDER BY timestamp
            "#,
            time_range_start,
            time_range_end,
            model
        )
        .fetch_all(db)
        .await?
    } else {
        sqlx::query_as!(
            TimeSeriesRow,
            r#"
            SELECT
                date_trunc('hour', timestamp) as timestamp,
                COUNT(*) as requests_count,
                COALESCE(SUM(prompt_tokens), 0)::bigint as input_tokens,
                COALESCE(SUM(completion_tokens), 0)::bigint as output_tokens,
                AVG(duration_ms)::float8 as avg_latency_ms,
                PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY duration_ms)::float8 as p95_latency_ms,
                PERCENTILE_CONT(0.99) WITHIN GROUP (ORDER BY duration_ms)::float8 as p99_latency_ms
            FROM http_analytics
            WHERE timestamp >= $1 AND timestamp <= $2
            GROUP BY date_trunc('hour', timestamp)
            ORDER BY timestamp
            "#,
            time_range_start,
            time_range_end
        )
        .fetch_all(db)
        .await?
    };

    let time_series = rows
        .into_iter()
        .filter_map(|row| {
            row.timestamp.map(|timestamp| TimeSeriesPoint {
                timestamp,
                duration_minutes: 60,
                requests: row.requests_count.unwrap_or(0),
                input_tokens: row.input_tokens.unwrap_or(0),
                output_tokens: row.output_tokens.unwrap_or(0),
                avg_latency_ms: row.avg_latency_ms,
                p95_latency_ms: row.p95_latency_ms,
                p99_latency_ms: row.p99_latency_ms,
            })
        })
        .collect();

    // Fill in missing hourly intervals with zero values
    let filled_time_series = fill_missing_intervals(time_series, time_range_start, time_range_end);

    Ok(filled_time_series)
}

/// Get time series data with 10-minute granularity
#[instrument(skip(db), err)]
async fn get_time_series_ten_minutes(
    db: &PgPool,
    time_range_start: DateTime<Utc>,
    time_range_end: DateTime<Utc>,
    model_filter: Option<&str>,
) -> Result<Vec<TimeSeriesPoint>> {
    let rows = if let Some(model) = model_filter {
        sqlx::query_as!(
            TimeSeriesRow,
            r#"
            SELECT
                date_trunc('hour', timestamp) + INTERVAL '10 minute' * FLOOR(EXTRACT(minute FROM timestamp) / 10) as timestamp,
                COUNT(*) as requests_count,
                COALESCE(SUM(prompt_tokens), 0)::bigint as input_tokens,
                COALESCE(SUM(completion_tokens), 0)::bigint as output_tokens,
                AVG(duration_ms)::float8 as avg_latency_ms,
                PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY duration_ms)::float8 as p95_latency_ms,
                PERCENTILE_CONT(0.99) WITHIN GROUP (ORDER BY duration_ms)::float8 as p99_latency_ms
            FROM http_analytics
            WHERE timestamp >= $1 AND timestamp <= $2 AND model = $3
            GROUP BY date_trunc('hour', timestamp) + INTERVAL '10 minute' * FLOOR(EXTRACT(minute FROM timestamp) / 10)
            ORDER BY timestamp
            "#,
            time_range_start,
            time_range_end,
            model
        )
        .fetch_all(db)
        .await?
    } else {
        sqlx::query_as!(
            TimeSeriesRow,
            r#"
            SELECT
                date_trunc('hour', timestamp) + INTERVAL '10 minute' * FLOOR(EXTRACT(minute FROM timestamp) / 10) as timestamp,
                COUNT(*) as requests_count,
                COALESCE(SUM(prompt_tokens), 0)::bigint as input_tokens,
                COALESCE(SUM(completion_tokens), 0)::bigint as output_tokens,
                AVG(duration_ms)::float8 as avg_latency_ms,
                PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY duration_ms)::float8 as p95_latency_ms,
                PERCENTILE_CONT(0.99) WITHIN GROUP (ORDER BY duration_ms)::float8 as p99_latency_ms
            FROM http_analytics
            WHERE timestamp >= $1 AND timestamp <= $2
            GROUP BY date_trunc('hour', timestamp) + INTERVAL '10 minute' * FLOOR(EXTRACT(minute FROM timestamp) / 10)
            ORDER BY timestamp
            "#,
            time_range_start,
            time_range_end
        )
        .fetch_all(db)
        .await?
    };

    let time_series = rows
        .into_iter()
        .filter_map(|row| {
            row.timestamp.map(|timestamp| TimeSeriesPoint {
                timestamp,
                duration_minutes: 10, // 10-minute intervals
                requests: row.requests_count.unwrap_or(0),
                input_tokens: row.input_tokens.unwrap_or(0),
                output_tokens: row.output_tokens.unwrap_or(0),
                avg_latency_ms: row.avg_latency_ms,
                p95_latency_ms: row.p95_latency_ms,
                p99_latency_ms: row.p99_latency_ms,
            })
        })
        .collect();

    // Fill in missing 10-minute intervals with zero values
    let filled_time_series = fill_missing_intervals_ten_minutes(time_series, time_range_start, time_range_end);

    Ok(filled_time_series)
}

/// Fill in missing hourly intervals with zero values
fn fill_missing_intervals(
    mut time_series: Vec<TimeSeriesPoint>,
    time_range_start: DateTime<Utc>,
    time_range_end: DateTime<Utc>,
) -> Vec<TimeSeriesPoint> {
    // Sort by timestamp to ensure order
    time_series.sort_by_key(|point| point.timestamp);

    // Create a HashMap for quick lookup of existing data points
    let existing_points: HashMap<DateTime<Utc>, &TimeSeriesPoint> = time_series.iter().map(|point| (point.timestamp, point)).collect();

    // Generate all hourly intervals from start time to end time
    let start_hour = time_range_start
        .date_naive()
        .and_hms_opt(time_range_start.hour(), 0, 0)
        .map(|naive| naive.and_utc())
        .unwrap_or(time_range_start);

    let mut filled_series = Vec::new();
    let mut current = start_hour;

    while current <= time_range_end {
        if let Some(existing_point) = existing_points.get(&current) {
            // Use existing data
            filled_series.push((*existing_point).clone());
        } else {
            // Fill with zero values
            filled_series.push(TimeSeriesPoint {
                timestamp: current,
                duration_minutes: 60,
                requests: 0,
                input_tokens: 0,
                output_tokens: 0,
                avg_latency_ms: None,
                p95_latency_ms: None,
                p99_latency_ms: None,
            });
        }

        current += Duration::hours(1);
    }

    filled_series
}

/// Fill missing 10-minute intervals with zero values
fn fill_missing_intervals_ten_minutes(
    mut time_series: Vec<TimeSeriesPoint>,
    time_range_start: DateTime<Utc>,
    time_range_end: DateTime<Utc>,
) -> Vec<TimeSeriesPoint> {
    // Sort by timestamp to ensure order
    time_series.sort_by_key(|point| point.timestamp);

    // Create a HashMap for quick lookup of existing data points
    let existing_points: HashMap<DateTime<Utc>, &TimeSeriesPoint> = time_series.iter().map(|point| (point.timestamp, point)).collect();

    // Generate all 10-minute intervals from start time to end time
    // Round start time down to the nearest 10-minute interval
    let start_ten_minutes = time_range_start
        .date_naive()
        .and_hms_opt(time_range_start.hour(), (time_range_start.minute() / 10) * 10, 0)
        .map(|naive| naive.and_utc())
        .unwrap_or(time_range_start);

    let mut filled_series = Vec::new();
    let mut current = start_ten_minutes;

    while current <= time_range_end {
        if let Some(existing_point) = existing_points.get(&current) {
            // Use existing data
            filled_series.push((*existing_point).clone());
        } else {
            // Fill with zero values
            filled_series.push(TimeSeriesPoint {
                timestamp: current,
                duration_minutes: 10,
                requests: 0,
                input_tokens: 0,
                output_tokens: 0,
                avg_latency_ms: None,
                p95_latency_ms: None,
                p99_latency_ms: None,
            });
        }
        current += Duration::minutes(10);
    }

    filled_series
}

/// Fill missing 10-minute intervals for sparklines (simplified version for ModelTimeSeriesPoint)
fn fill_missing_intervals_for_sparklines(
    mut time_series: Vec<ModelTimeSeriesPoint>,
    time_range_start: DateTime<Utc>,
    time_range_end: DateTime<Utc>,
) -> Vec<ModelTimeSeriesPoint> {
    // Sort by timestamp to ensure order
    time_series.sort_by_key(|point| point.timestamp);

    // Create a HashMap for quick lookup of existing data points
    let existing_points: HashMap<DateTime<Utc>, &ModelTimeSeriesPoint> = time_series.iter().map(|point| (point.timestamp, point)).collect();

    // Generate all 10-minute intervals from start time to end time
    // Round start time down to the nearest 10-minute interval
    let start_ten_minutes = time_range_start
        .date_naive()
        .and_hms_opt(time_range_start.hour(), (time_range_start.minute() / 10) * 10, 0)
        .map(|naive| naive.and_utc())
        .unwrap_or(time_range_start);

    let mut filled_series = Vec::new();
    let mut current = start_ten_minutes;

    while current <= time_range_end {
        if let Some(existing_point) = existing_points.get(&current) {
            // Use existing data
            filled_series.push((*existing_point).clone());
        } else {
            // Fill with zero values
            filled_series.push(ModelTimeSeriesPoint {
                timestamp: current,
                requests: 0,
            });
        }
        current += Duration::minutes(10);
    }

    filled_series
}

/// Get status code breakdown (raw counts, percentages calculated later)
#[instrument(skip(db), err)]
async fn get_status_codes(
    db: &PgPool,
    time_range_start: DateTime<Utc>,
    time_range_end: DateTime<Utc>,
    model_filter: Option<&str>,
) -> Result<Vec<StatusCodeRow>> {
    let rows = if let Some(model) = model_filter {
        sqlx::query_as!(
            StatusCodeRow,
            "SELECT status_code, COUNT(*) as status_count FROM http_analytics WHERE timestamp >= $1 AND timestamp <= $2 AND model = $3 AND status_code IS NOT NULL GROUP BY status_code ORDER BY status_count DESC",
            time_range_start,
            time_range_end,
            model
        )
        .fetch_all(db)
        .await?
    } else {
        sqlx::query_as!(
            StatusCodeRow,
            "SELECT status_code, COUNT(*) as status_count FROM http_analytics WHERE timestamp >= $1 AND timestamp <= $2 AND status_code IS NOT NULL GROUP BY status_code ORDER BY status_count DESC",
            time_range_start,
            time_range_end
        )
        .fetch_all(db)
        .await?
    };

    Ok(rows)
}

/// Get model usage data (raw counts, percentages calculated later)
#[instrument(skip(db), err)]
async fn get_model_usage(db: &PgPool, time_range_start: DateTime<Utc>, time_range_end: DateTime<Utc>) -> Result<Vec<ModelUsageRow>> {
    let rows = sqlx::query_as!(
        ModelUsageRow,
        "SELECT model as model_name, COUNT(*) as model_count, COALESCE(AVG(duration_ms), 0)::float8 as model_avg_latency_ms FROM http_analytics WHERE timestamp >= $1 AND timestamp <= $2 AND model IS NOT NULL GROUP BY model ORDER BY model_count DESC",
        time_range_start,
        time_range_end
    )
    .fetch_all(db)
    .await?;

    Ok(rows)
}

/// Get aggregated analytics data for HTTP requests
#[instrument(skip(db), err)]
pub async fn get_requests_aggregate(
    db: &PgPool,
    time_range_start: DateTime<Utc>,
    time_range_end: DateTime<Utc>,
    model_filter: Option<&str>,
) -> Result<RequestsAggregateResponse> {
    // Execute all queries concurrently
    let (total_requests, time_series, status_code_rows, model_rows) = if model_filter.is_some() {
        // For single model view, don't fetch model breakdown
        let (total_requests, time_series, status_code_rows) = tokio::try_join!(
            get_total_requests(db, time_range_start, time_range_end, model_filter),
            get_time_series(db, time_range_start, time_range_end, model_filter, TimeGranularity::Hour),
            get_status_codes(db, time_range_start, time_range_end, model_filter),
        )?;
        (total_requests, time_series, status_code_rows, Vec::new())
    } else {
        // For all models view, fetch everything
        let (total_requests, time_series, status_code_rows, model_rows) = tokio::try_join!(
            get_total_requests(db, time_range_start, time_range_end, model_filter),
            get_time_series(db, time_range_start, time_range_end, model_filter, TimeGranularity::Hour),
            get_status_codes(db, time_range_start, time_range_end, model_filter),
            get_model_usage(db, time_range_start, time_range_end),
        )?;
        (total_requests, time_series, status_code_rows, model_rows)
    };

    // Convert status code rows to breakdown with percentages
    let status_codes: Vec<StatusCodeBreakdown> = status_code_rows
        .into_iter()
        .filter_map(|row| match (row.status_code, row.status_count) {
            (Some(status_code), Some(status_count)) => Some(StatusCodeBreakdown {
                status: status_code.to_string(),
                count: status_count,
                percentage: if total_requests > 0 {
                    (status_count as f64 * 100.0) / total_requests as f64
                } else {
                    0.0
                },
            }),
            _ => None,
        })
        .collect();

    // Convert model rows to usage with percentages (only if we have model data)
    let models = if !model_rows.is_empty() {
        let models: Vec<ModelUsage> = model_rows
            .into_iter()
            .filter_map(|row| match (row.model_name, row.model_count) {
                (Some(model_name), Some(model_count)) => Some(ModelUsage {
                    model: model_name,
                    count: model_count,
                    percentage: if total_requests > 0 {
                        (model_count as f64 * 100.0) / total_requests as f64
                    } else {
                        0.0
                    },
                    avg_latency_ms: row.model_avg_latency_ms.unwrap_or(0.0),
                }),
                _ => None,
            })
            .collect();
        Some(models)
    } else {
        None
    };

    Ok(RequestsAggregateResponse {
        total_requests,
        model: model_filter.map(|m| m.to_string()),
        status_codes,
        models,
        time_series,
    })
}

/// Get aggregated metrics for one or more models (with 60s cache)
#[instrument(skip(db), err)]
pub async fn get_model_metrics(db: &PgPool, mut model_aliases: Vec<String>) -> Result<HashMap<String, ModelMetrics>> {
    if model_aliases.is_empty() {
        return Ok(HashMap::new());
    }

    // Create cache key by sorting aliases to ensure consistent key regardless of order
    model_aliases.sort();
    let cache_key = model_aliases.join(",");

    // Check cache first
    if let Some(cached) = METRICS_CACHE.get(&cache_key).await {
        tracing::debug!("Cache hit for model metrics");
        return Ok(cached);
    }

    // Cache miss - execute query
    tracing::debug!("Cache miss for model metrics, executing query");
    let result = get_model_metrics_impl(db, model_aliases.clone()).await?;

    // Store in cache
    METRICS_CACHE.insert(cache_key, result.clone()).await;

    Ok(result)
}

/// Internal implementation of get_model_metrics (not cached)
async fn get_model_metrics_impl(db: &PgPool, model_aliases: Vec<String>) -> Result<HashMap<String, ModelMetrics>> {
    // Initialize all models with zero metrics (for models with no activity)
    let mut metrics_map: HashMap<String, ModelMetrics> = HashMap::new();
    for alias in &model_aliases {
        metrics_map.insert(
            alias.clone(),
            ModelMetrics {
                avg_latency_ms: None,
                total_requests: 0,
                total_input_tokens: 0,
                total_output_tokens: 0,
                last_active_at: None,
                time_series: None, // Will be filled in later
            },
        );
    }

    // Get basic metrics for models that have activity
    let metrics_rows = sqlx::query_as!(
        ModelMetricsRow,
        r#"
        SELECT
            model,
            COUNT(*) as total_requests,
            AVG(duration_ms)::float8 as avg_latency_ms,
            COALESCE(SUM(prompt_tokens), 0)::bigint as total_input_tokens,
            COALESCE(SUM(completion_tokens), 0)::bigint as total_output_tokens,
            MAX(timestamp) as last_active_at
        FROM http_analytics
        WHERE model = ANY($1)
        GROUP BY model
        "#,
        &model_aliases
    )
    .fetch_all(db)
    .await?;

    // Update metrics for models that have actual data
    for row in metrics_rows {
        if let Some(model) = row.model
            && let Some(metrics) = metrics_map.get_mut(&model)
        {
            metrics.avg_latency_ms = row.avg_latency_ms;
            metrics.total_requests = row.total_requests.unwrap_or(0);
            metrics.total_input_tokens = row.total_input_tokens.unwrap_or(0);
            metrics.total_output_tokens = row.total_output_tokens.unwrap_or(0);
            metrics.last_active_at = row.last_active_at;
        }
    }

    // Get time series data for sparklines (last 2 hours in 10-minute intervals) for all models in one query
    let now = Utc::now();
    let two_hours_ago = now - Duration::hours(2);

    let time_series_rows = sqlx::query_as!(
        BulkTimeSeriesRow,
        r#"
        SELECT
            model,
            date_trunc('hour', timestamp) + INTERVAL '10 minute' * FLOOR(EXTRACT(minute FROM timestamp) / 10) as timestamp,
            COUNT(*) as requests_count
        FROM http_analytics
        WHERE model = ANY($1)
            AND timestamp >= $2
            AND timestamp <= $3
        GROUP BY model, date_trunc('hour', timestamp) + INTERVAL '10 minute' * FLOOR(EXTRACT(minute FROM timestamp) / 10)
        ORDER BY model, timestamp
        "#,
        &model_aliases,
        two_hours_ago,
        now
    )
    .fetch_all(db)
    .await;

    // Process time series data if successful
    if let Ok(rows) = time_series_rows {
        // Group time series points by model
        let mut model_time_series: HashMap<String, Vec<ModelTimeSeriesPoint>> = HashMap::new();
        for row in rows {
            if let (Some(model), Some(timestamp)) = (row.model, row.timestamp) {
                model_time_series.entry(model).or_default().push(ModelTimeSeriesPoint {
                    timestamp,
                    requests: row.requests_count.unwrap_or(0),
                });
            }
        }

        // Fill in missing intervals for all models (including those with no activity)
        for model in &model_aliases {
            if let Some(metrics) = metrics_map.get_mut(model) {
                let time_series = model_time_series.get(model).cloned().unwrap_or_default();
                // Fill gaps with zero values for consistent sparklines
                let filled_time_series = fill_missing_intervals_for_sparklines(time_series, two_hours_ago, now);
                metrics.time_series = Some(filled_time_series);
            }
        }
    } else {
        tracing::warn!("Failed to fetch bulk time series data for models");
    }

    Ok(metrics_map)
}

/// User usage data from analytics query
#[derive(FromRow)]
struct UserUsageRow {
    pub user_id: Option<uuid::Uuid>,
    pub user_email: Option<String>,
    pub request_count: Option<i64>,
    pub total_input_tokens: Option<i64>,
    pub total_output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub total_cost: Option<f64>,
    pub last_active_at: Option<DateTime<Utc>>,
}

/// Get usage data grouped by user for a specific model
#[instrument(skip(db), err)]
pub async fn get_model_user_usage(
    db: &PgPool,
    model_alias: &str,
    start_date: DateTime<Utc>,
    end_date: DateTime<Utc>,
) -> Result<ModelUserUsageResponse> {
    // Get user-grouped data (join with users table for email)
    let user_rows = sqlx::query_as!(
        UserUsageRow,
        r#"
        SELECT
            ha.user_id,
            u.email as "user_email?",
            COUNT(*) as request_count,
            COALESCE(SUM(ha.prompt_tokens), 0)::bigint as total_input_tokens,
            COALESCE(SUM(ha.completion_tokens), 0)::bigint as total_output_tokens,
            COALESCE(SUM(ha.total_tokens), 0)::bigint as total_tokens,
            SUM(ha.total_cost)::float8 as total_cost,
            MAX(ha.timestamp) as last_active_at
        FROM http_analytics ha
        LEFT JOIN users u ON u.id = ha.user_id
        WHERE ha.model = $1
            AND ha.timestamp >= $2
            AND ha.timestamp <= $3
            AND ha.user_id IS NOT NULL
        GROUP BY ha.user_id, u.email
        ORDER BY request_count DESC
        "#,
        model_alias,
        start_date,
        end_date
    )
    .fetch_all(db)
    .await?;

    // Get totals (only for authenticated users)
    let totals_row = sqlx::query!(
        r#"
        SELECT
            COUNT(*) as total_requests,
            COALESCE(SUM(total_tokens), 0)::bigint as total_tokens,
            SUM(total_cost)::float8 as total_cost
        FROM http_analytics
        WHERE model = $1
            AND timestamp >= $2
            AND timestamp <= $3
            AND user_id IS NOT NULL
        "#,
        model_alias,
        start_date,
        end_date
    )
    .fetch_one(db)
    .await?;

    // Convert rows to UserUsage
    let users: Vec<UserUsage> = user_rows
        .into_iter()
        .map(|row| UserUsage {
            user_id: row.user_id.map(|id| id.to_string()),
            user_email: row.user_email,
            request_count: row.request_count.unwrap_or(0),
            total_tokens: row.total_tokens.unwrap_or(0),
            input_tokens: row.total_input_tokens.unwrap_or(0),
            output_tokens: row.total_output_tokens.unwrap_or(0),
            total_cost: row.total_cost,
            last_active_at: row.last_active_at,
        })
        .collect();

    Ok(ModelUserUsageResponse {
        model: model_alias.to_string(),
        start_date,
        end_date,
        total_requests: totals_row.total_requests.unwrap_or(0),
        total_tokens: totals_row.total_tokens.unwrap_or(0),
        total_cost: totals_row.total_cost,
        users,
    })
}

/// Get aggregated analytics metrics for a batch given a list of request IDs
#[instrument(skip(pool))]
pub async fn get_batch_analytics(pool: &PgPool, batch_id: &Uuid) -> Result<BatchAnalytics> {
    // Query analytics for these specific request IDs
    let metrics = sqlx::query!(
        r#"
        SELECT
            COUNT(*) as "total_requests!",
            COALESCE(SUM(prompt_tokens), 0) as "total_prompt_tokens!",
            COALESCE(SUM(completion_tokens), 0) as "total_completion_tokens!",
            COALESCE(SUM(total_tokens), 0) as "total_tokens!",
            AVG(duration_ms) as "avg_duration_ms",
            AVG(duration_to_first_byte_ms) as "avg_ttfb_ms",
            SUM((prompt_tokens * COALESCE(input_price_per_token, 0)) +
                (completion_tokens * COALESCE(output_price_per_token, 0))) as "total_cost"
        FROM http_analytics
        WHERE fusillade_batch_id = $1
        "#,
        batch_id
    )
    .fetch_one(pool)
    .await?;

    Ok(BatchAnalytics {
        total_requests: metrics.total_requests,
        total_prompt_tokens: metrics.total_prompt_tokens.to_i64().unwrap_or(0),
        total_completion_tokens: metrics.total_completion_tokens.to_i64().unwrap_or(0),
        total_tokens: metrics.total_tokens.to_i64().unwrap_or(0),
        avg_duration_ms: metrics.avg_duration_ms.and_then(|d| d.to_f64()),
        avg_ttfb_ms: metrics.avg_ttfb_ms.and_then(|d| d.to_f64()),
        total_cost: metrics.total_cost.map(|d| d.to_string()),
    })
}

/// Get aggregated analytics metrics for multiple batches in a single query
#[instrument(skip(pool))]
pub async fn get_batches_analytics_bulk(pool: &PgPool, batch_ids: &[Uuid]) -> Result<HashMap<Uuid, BatchAnalytics>> {
    if batch_ids.is_empty() {
        return Ok(HashMap::new());
    }

    // Query analytics for all batch IDs at once, grouped by batch ID
    let rows = sqlx::query!(
        r#"
        SELECT
            fusillade_batch_id,
            COUNT(*) as "total_requests!",
            COALESCE(SUM(prompt_tokens), 0) as "total_prompt_tokens!",
            COALESCE(SUM(completion_tokens), 0) as "total_completion_tokens!",
            COALESCE(SUM(total_tokens), 0) as "total_tokens!",
            AVG(duration_ms) as "avg_duration_ms",
            AVG(duration_to_first_byte_ms) as "avg_ttfb_ms",
            SUM((prompt_tokens * COALESCE(input_price_per_token, 0)) +
                (completion_tokens * COALESCE(output_price_per_token, 0))) as "total_cost"
        FROM http_analytics
        WHERE fusillade_batch_id = ANY($1)
        GROUP BY fusillade_batch_id
        "#,
        batch_ids
    )
    .fetch_all(pool)
    .await?;

    // Convert rows to HashMap
    let mut result = HashMap::new();
    for row in rows {
        if let Some(batch_id) = row.fusillade_batch_id {
            result.insert(
                batch_id,
                BatchAnalytics {
                    total_requests: row.total_requests,
                    total_prompt_tokens: row.total_prompt_tokens.to_i64().unwrap_or(0),
                    total_completion_tokens: row.total_completion_tokens.to_i64().unwrap_or(0),
                    total_tokens: row.total_tokens.to_i64().unwrap_or(0),
                    avg_duration_ms: row.avg_duration_ms.and_then(|d: Decimal| d.to_f64()),
                    avg_ttfb_ms: row.avg_ttfb_ms.and_then(|d: Decimal| d.to_f64()),
                    total_cost: row.total_cost.map(|d: Decimal| d.to_string()),
                },
            );
        }
    }

    // For batch IDs with no analytics data, insert empty analytics
    for batch_id in batch_ids {
        result.entry(*batch_id).or_insert(BatchAnalytics {
            total_requests: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tokens: 0,
            avg_duration_ms: None,
            avg_ttfb_ms: None,
            total_cost: None,
        });
    }

    Ok(result)
}

/// Row type for http_analytics query
#[derive(FromRow)]
struct HttpAnalyticsRow {
    pub id: i64,
    pub timestamp: DateTime<Utc>,
    pub method: String,
    pub uri: String,
    pub model: Option<String>,
    pub status_code: Option<i32>,
    pub duration_ms: Option<i64>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub response_type: Option<String>,
    pub fusillade_batch_id: Option<Uuid>,
    pub input_price_per_token: Option<Decimal>,
    pub output_price_per_token: Option<Decimal>,
    pub custom_id: Option<String>,
}

/// List HTTP analytics entries with filtering and pagination
#[instrument(skip(pool), err)]
pub async fn list_http_analytics(
    pool: &PgPool,
    skip: i64,
    limit: i64,
    order_desc: bool,
    filters: HttpAnalyticsFilter,
) -> Result<Vec<AnalyticsEntry>> {
    // Wrap custom_id in wildcards for ILIKE substring matching
    let custom_id_pattern = filters.custom_id.as_ref().map(|s| format!("%{}%", s));

    let rows = sqlx::query_as!(
        HttpAnalyticsRow,
        r#"
        SELECT
            id,
            timestamp,
            method,
            uri,
            model,
            status_code,
            duration_ms,
            prompt_tokens,
            completion_tokens,
            total_tokens,
            response_type,
            fusillade_batch_id,
            input_price_per_token,
            output_price_per_token,
            custom_id
        FROM http_analytics
        WHERE
            ($1::timestamptz IS NULL OR timestamp >= $1)
            AND ($2::timestamptz IS NULL OR timestamp <= $2)
            AND ($3::text IS NULL OR model = $3)
            AND ($4::uuid IS NULL OR fusillade_batch_id = $4)
            AND ($5::text IS NULL OR method = $5)
            AND ($6::text IS NULL OR uri LIKE $6)
            AND ($7::int IS NULL OR status_code = $7)
            AND ($8::int IS NULL OR status_code >= $8)
            AND ($9::int IS NULL OR status_code <= $9)
            AND ($10::bigint IS NULL OR duration_ms >= $10)
            AND ($11::bigint IS NULL OR duration_ms <= $11)
            AND ($12::text IS NULL OR custom_id ILIKE $12)
        ORDER BY timestamp DESC
        LIMIT $13
        OFFSET $14
        "#,
        filters.timestamp_after,
        filters.timestamp_before,
        filters.model,
        filters.fusillade_batch_id,
        filters.method,
        filters.uri_pattern,
        filters.status_code,
        filters.status_code_min,
        filters.status_code_max,
        filters.min_duration_ms,
        filters.max_duration_ms,
        custom_id_pattern,
        limit,
        skip,
    )
    .fetch_all(pool)
    .await?;

    // Note: The ORDER BY is hardcoded to DESC in the query above.
    // If we need dynamic ordering, we'd need to use query_as with raw SQL
    // or have two separate queries. For now, we'll handle ASC by reversing.
    let mut entries: Vec<AnalyticsEntry> = rows
        .into_iter()
        .map(|row| AnalyticsEntry {
            id: row.id,
            timestamp: row.timestamp,
            method: row.method,
            uri: row.uri,
            model: row.model,
            status_code: row.status_code,
            duration_ms: row.duration_ms,
            prompt_tokens: row.prompt_tokens,
            completion_tokens: row.completion_tokens,
            total_tokens: row.total_tokens,
            response_type: row.response_type,
            fusillade_batch_id: row.fusillade_batch_id,
            input_price_per_token: row.input_price_per_token.map(|p| p.to_string()),
            output_price_per_token: row.output_price_per_token.map(|p| p.to_string()),
            custom_id: row.custom_id,
        })
        .collect();

    // If ascending order requested, reverse the results
    if !order_desc {
        entries.reverse();
    }

    Ok(entries)
}

/// Row type for batch aggregate stats from pre-aggregated table
#[derive(FromRow)]
struct BatchCountRow {
    pub total_batch_count: Option<i64>,
    pub avg_requests_per_batch: Option<Decimal>,
    pub total_cost: Option<Decimal>,
}

/// Row type for per-model breakdown query
#[derive(FromRow)]
struct ModelBreakdownRow {
    pub model: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cost: Option<Decimal>,
    pub request_count: Option<i64>,
}

/// Get batch-level metrics from pre-aggregated batch_aggregates table.
/// Returns (batch_count, avg_requests_per_batch, total_cost).
/// Batch count and avg can't be derived from per-model breakdown
/// (batches may span multiple models). Cost is already aggregated here.
#[instrument(skip(pool), err)]
pub async fn get_user_batch_counts(pool: &PgPool, user_id: Uuid) -> Result<(i64, f64, String)> {
    let row = sqlx::query_as!(
        BatchCountRow,
        r#"
        SELECT
            COUNT(*) as total_batch_count,
            COALESCE(AVG(transaction_count), 0) as avg_requests_per_batch,
            COALESCE(SUM(total_amount), 0) as total_cost
        FROM batch_aggregates
        WHERE user_id = $1
        "#,
        user_id
    )
    .fetch_one(pool)
    .await?;

    Ok((
        row.total_batch_count.unwrap_or(0),
        row.avg_requests_per_batch.and_then(|d| d.to_f64()).unwrap_or(0.0),
        row.total_cost.map(|d| d.to_string()).unwrap_or_else(|| "0".to_string()),
    ))
}

/// Incrementally aggregate new http_analytics rows into the user_model_usage summary table.
/// Uses a cursor to track the last processed id, so only new rows are scanned.
#[instrument(skip(pool), err)]
pub async fn refresh_user_model_usage(pool: &PgPool) -> Result<()> {
    let mut tx = pool.begin().await?;

    let cursor: i64 = sqlx::query_scalar!(
        "SELECT last_processed_id FROM user_model_usage_cursor WHERE id = TRUE FOR UPDATE"
    )
    .fetch_one(&mut *tx)
    .await?;

    let new_max: Option<i64> = sqlx::query_scalar!(
        r#"
        SELECT MAX(id) FROM http_analytics
        WHERE id > $1 AND user_id IS NOT NULL AND model IS NOT NULL AND fusillade_batch_id IS NOT NULL
        "#,
        cursor
    )
    .fetch_one(&mut *tx)
    .await?;

    let Some(new_max) = new_max else {
        return Ok(());
    };

    sqlx::query!(
        r#"
        INSERT INTO user_model_usage (user_id, model, input_tokens, output_tokens, cost, request_count)
        SELECT user_id,
               model,
               COALESCE(SUM(prompt_tokens), 0),
               COALESCE(SUM(completion_tokens), 0),
               COALESCE(SUM(total_cost), 0),
               COUNT(*)
        FROM http_analytics
        WHERE id > $1 AND id <= $2
              AND user_id IS NOT NULL AND model IS NOT NULL AND fusillade_batch_id IS NOT NULL
        GROUP BY user_id, model
        ON CONFLICT (user_id, model)
        DO UPDATE SET
            input_tokens = user_model_usage.input_tokens + EXCLUDED.input_tokens,
            output_tokens = user_model_usage.output_tokens + EXCLUDED.output_tokens,
            cost = user_model_usage.cost + EXCLUDED.cost,
            request_count = user_model_usage.request_count + EXCLUDED.request_count,
            updated_at = NOW()
        "#,
        cursor,
        new_max
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query!(
        "UPDATE user_model_usage_cursor SET last_processed_id = $1, updated_at = NOW() WHERE id = TRUE",
        new_max
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// Estimate what the user's total cost would have been at realtime tariff rates.
/// Joins user_model_usage → deployed_models (by alias) → model_tariffs (realtime, active).
/// Models with no matching realtime tariff are excluded from the estimate.
#[instrument(skip(pool), err)]
pub async fn estimate_realtime_cost(pool: &PgPool, user_id: Uuid) -> Result<String> {
    let cost: Option<Decimal> = sqlx::query_scalar!(
        r#"
        SELECT SUM(
            u.input_tokens::NUMERIC * t.input_price_per_token +
            u.output_tokens::NUMERIC * t.output_price_per_token
        ) as "cost"
        FROM user_model_usage u
        JOIN deployed_models dm ON dm.alias = u.model
        JOIN model_tariffs t ON t.deployed_model_id = dm.id
            AND t.api_key_purpose = 'realtime'
            AND t.valid_until IS NULL
        WHERE u.user_id = $1
        "#,
        user_id
    )
    .fetch_one(pool)
    .await?;

    Ok(cost.map(|d| d.to_string()).unwrap_or_else(|| "0".to_string()))
}

/// Get per-model breakdown from the pre-aggregated user_model_usage table.
/// Totals (tokens, cost, request count) are derived from these results by the handler.
#[instrument(skip(pool), err)]
pub async fn get_user_model_breakdown(pool: &PgPool, user_id: Uuid) -> Result<Vec<ModelBreakdownEntry>> {
    let rows = sqlx::query_as!(
        ModelBreakdownRow,
        r#"
        SELECT model,
               input_tokens,
               output_tokens,
               cost,
               request_count
        FROM user_model_usage
        WHERE user_id = $1
        ORDER BY request_count DESC
        "#,
        user_id
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            row.model.map(|model| ModelBreakdownEntry {
                model,
                input_tokens: row.input_tokens.unwrap_or(0),
                output_tokens: row.output_tokens.unwrap_or(0),
                cost: row.cost.map(|d| d.to_string()).unwrap_or_else(|| "0".to_string()),
                request_count: row.request_count.unwrap_or(0),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use sqlx::PgPool;

    #[test]
    fn test_fill_missing_intervals_empty_input() {
        let time_series = vec![];
        let start_time = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let end_time = start_time + Duration::hours(24);
        let result = fill_missing_intervals(time_series, start_time, end_time);

        // Even with empty input, should fill intervals with zero values
        assert!(!result.is_empty());
        // All points should have zero values
        assert!(result.iter().all(|p| p.requests == 0));
        assert!(result.iter().all(|p| p.input_tokens == 0));
        assert!(result.iter().all(|p| p.output_tokens == 0));
        assert!(result.iter().all(|p| p.avg_latency_ms.is_none()));
    }

    #[test]
    fn test_fill_missing_intervals_single_point() {
        let start_time = Utc.with_ymd_and_hms(2024, 1, 1, 10, 0, 0).unwrap();
        let end_time = start_time + Duration::hours(24);
        let time_series = vec![TimeSeriesPoint {
            timestamp: start_time,
            duration_minutes: 60,
            requests: 5,
            input_tokens: 100,
            output_tokens: 50,
            avg_latency_ms: Some(200.0),
            p95_latency_ms: Some(300.0),
            p99_latency_ms: Some(400.0),
        }];

        let result = fill_missing_intervals(time_series, start_time, end_time);

        // Should have data from start_time to current hour
        assert!(!result.is_empty());
        assert_eq!(result[0].timestamp, start_time);
        assert_eq!(result[0].requests, 5);
    }

    #[test]
    fn test_fill_missing_intervals_gaps() {
        let start_time = Utc.with_ymd_and_hms(2024, 1, 1, 10, 0, 0).unwrap();
        let point1_time = start_time;
        let point2_time = start_time + Duration::hours(3); // Skip 2 hours

        let time_series = vec![
            TimeSeriesPoint {
                timestamp: point1_time,
                duration_minutes: 60,
                requests: 5,
                input_tokens: 100,
                output_tokens: 50,
                avg_latency_ms: Some(200.0),
                p95_latency_ms: Some(300.0),
                p99_latency_ms: Some(400.0),
            },
            TimeSeriesPoint {
                timestamp: point2_time,
                duration_minutes: 60,
                requests: 3,
                input_tokens: 60,
                output_tokens: 30,
                avg_latency_ms: Some(150.0),
                p95_latency_ms: Some(250.0),
                p99_latency_ms: Some(350.0),
            },
        ];

        let end_time = start_time + Duration::hours(24);
        let result = fill_missing_intervals(time_series, start_time, end_time);

        // Should fill in the gaps with zero values
        let first_gap = result.iter().find(|p| p.timestamp == start_time + Duration::hours(1));
        assert!(first_gap.is_some());
        let gap_point = first_gap.unwrap();
        assert_eq!(gap_point.requests, 0);
        assert_eq!(gap_point.input_tokens, 0);
        assert_eq!(gap_point.output_tokens, 0);
        assert_eq!(gap_point.avg_latency_ms, None);
        assert_eq!(gap_point.p95_latency_ms, None);
        assert_eq!(gap_point.p99_latency_ms, None);
    }

    #[test]
    fn test_fill_missing_intervals_unsorted_input() {
        let start_time = Utc.with_ymd_and_hms(2024, 1, 1, 10, 0, 0).unwrap();
        let point1_time = start_time + Duration::hours(2);
        let point2_time = start_time;

        let time_series = vec![
            TimeSeriesPoint {
                timestamp: point1_time,
                duration_minutes: 60,
                requests: 3,
                input_tokens: 60,
                output_tokens: 30,
                avg_latency_ms: Some(150.0),
                p95_latency_ms: Some(250.0),
                p99_latency_ms: Some(350.0),
            },
            TimeSeriesPoint {
                timestamp: point2_time,
                duration_minutes: 60,
                requests: 5,
                input_tokens: 100,
                output_tokens: 50,
                avg_latency_ms: Some(200.0),
                p95_latency_ms: Some(300.0),
                p99_latency_ms: Some(400.0),
            },
        ];

        let end_time = start_time + Duration::hours(24);
        let result = fill_missing_intervals(time_series, start_time, end_time);

        // Should handle unsorted input correctly
        let first_point = result.iter().find(|p| p.timestamp == start_time).unwrap();
        assert_eq!(first_point.requests, 5);

        let second_point = result.iter().find(|p| p.timestamp == start_time + Duration::hours(2)).unwrap();
        assert_eq!(second_point.requests, 3);
    }

    #[test]
    fn test_fill_missing_intervals_hour_truncation() {
        // Test that start time is properly truncated to hour boundary
        let start_time = Utc.with_ymd_and_hms(2024, 1, 1, 10, 30, 45).unwrap(); // 10:30:45
        let expected_start = Utc.with_ymd_and_hms(2024, 1, 1, 10, 0, 0).unwrap(); // Should truncate to 10:00:00

        let time_series = vec![TimeSeriesPoint {
            timestamp: expected_start,
            duration_minutes: 60,
            requests: 5,
            input_tokens: 100,
            output_tokens: 50,
            avg_latency_ms: Some(200.0),
            p95_latency_ms: Some(300.0),
            p99_latency_ms: Some(400.0),
        }];

        let end_time = start_time + Duration::hours(24);
        let result = fill_missing_intervals(time_series, start_time, end_time);

        // First point should be at the truncated hour
        assert_eq!(result[0].timestamp, expected_start);
    }

    // Helper function to create test analytics data
    async fn insert_test_analytics_data(
        pool: &PgPool,
        timestamp: DateTime<Utc>,
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
            1i64, // Simple correlation_id for tests
            timestamp,
            status_code,
            duration_ms as i64,
            model,
            prompt_tokens,
            completion_tokens,
            prompt_tokens + completion_tokens // total_tokens
        )
        .execute(pool)
        .await
        .expect("Failed to insert test analytics data");
    }

    #[sqlx::test]
    async fn test_get_total_requests_no_filter(pool: PgPool) {
        let now = Utc::now();
        let one_hour_ago = now - Duration::hours(1);
        let two_hours_ago = now - Duration::hours(2);

        // Insert test data
        insert_test_analytics_data(&pool, one_hour_ago, "gpt-4", 200, 100.0, 50, 25).await;
        insert_test_analytics_data(&pool, one_hour_ago, "claude-3", 200, 150.0, 75, 35).await;
        insert_test_analytics_data(&pool, two_hours_ago, "gpt-4", 400, 200.0, 100, 50).await;

        let result = get_total_requests(&pool, two_hours_ago, now, None).await.unwrap();
        assert_eq!(result, 3);
    }

    #[sqlx::test]
    async fn test_get_total_requests_with_model_filter(pool: PgPool) {
        let now = Utc::now();
        let one_hour_ago = now - Duration::hours(1);

        // Insert test data for different models
        insert_test_analytics_data(&pool, one_hour_ago, "gpt-4", 200, 100.0, 50, 25).await;
        insert_test_analytics_data(&pool, one_hour_ago, "claude-3", 200, 150.0, 75, 35).await;
        insert_test_analytics_data(&pool, one_hour_ago, "gpt-4", 400, 200.0, 100, 50).await;

        let result = get_total_requests(&pool, one_hour_ago, now, Some("gpt-4")).await.unwrap();
        assert_eq!(result, 2);

        let result = get_total_requests(&pool, one_hour_ago, now, Some("claude-3")).await.unwrap();
        assert_eq!(result, 1);

        let result = get_total_requests(&pool, one_hour_ago, now, Some("nonexistent")).await.unwrap();
        assert_eq!(result, 0);
    }

    #[sqlx::test]
    async fn test_get_time_series_basic(pool: PgPool) {
        let base_time = Utc.with_ymd_and_hms(2024, 1, 1, 10, 0, 0).unwrap();
        let hour1 = base_time;
        let hour2 = base_time + Duration::hours(1);

        // Insert data for two different hours
        insert_test_analytics_data(&pool, hour1, "gpt-4", 200, 100.0, 50, 25).await;
        insert_test_analytics_data(&pool, hour1, "gpt-4", 200, 200.0, 75, 35).await;
        insert_test_analytics_data(&pool, hour2, "gpt-4", 200, 150.0, 60, 30).await;

        let result = get_time_series(
            &pool,
            base_time,
            base_time + Duration::hours(24),
            Some("gpt-4"),
            TimeGranularity::Hour,
        )
        .await
        .unwrap();

        // Should have filled in gaps and have data for both hours
        assert!(!result.is_empty());

        // Find the data points for our test hours
        let hour1_point = result.iter().find(|p| p.timestamp == hour1);
        let hour2_point = result.iter().find(|p| p.timestamp == hour2);

        assert!(hour1_point.is_some());
        assert!(hour2_point.is_some());

        let h1 = hour1_point.unwrap();
        assert_eq!(h1.requests, 2);
        assert_eq!(h1.input_tokens, 125); // 50 + 75
        assert_eq!(h1.output_tokens, 60); // 25 + 35

        let h2 = hour2_point.unwrap();
        assert_eq!(h2.requests, 1);
        assert_eq!(h2.input_tokens, 60);
        assert_eq!(h2.output_tokens, 30);
    }

    #[sqlx::test]
    async fn test_get_status_codes(pool: PgPool) {
        let now = Utc::now();
        let one_hour_ago = now - Duration::hours(1);

        // Insert data with different status codes
        insert_test_analytics_data(&pool, one_hour_ago, "gpt-4", 200, 100.0, 50, 25).await;
        insert_test_analytics_data(&pool, one_hour_ago, "gpt-4", 200, 150.0, 75, 35).await;
        insert_test_analytics_data(&pool, one_hour_ago, "gpt-4", 400, 200.0, 100, 50).await;
        insert_test_analytics_data(&pool, one_hour_ago, "claude-3", 500, 250.0, 80, 40).await;

        let result = get_status_codes(&pool, one_hour_ago, now, None).await.unwrap();

        // Should be ordered by count descending
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].status_code, Some(200));
        assert_eq!(result[0].status_count, Some(2));

        // Test with model filter
        let result = get_status_codes(&pool, one_hour_ago, now, Some("gpt-4")).await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].status_code, Some(200));
        assert_eq!(result[0].status_count, Some(2));
        assert_eq!(result[1].status_code, Some(400));
        assert_eq!(result[1].status_count, Some(1));
    }

    #[sqlx::test]
    async fn test_get_model_usage(pool: PgPool) {
        let now = Utc::now();
        let one_hour_ago = now - Duration::hours(1);

        // Insert data for different models
        insert_test_analytics_data(&pool, one_hour_ago, "gpt-4", 200, 100.0, 50, 25).await;
        insert_test_analytics_data(&pool, one_hour_ago, "gpt-4", 200, 200.0, 75, 35).await;
        insert_test_analytics_data(&pool, one_hour_ago, "claude-3", 200, 300.0, 60, 30).await;

        let result = get_model_usage(&pool, one_hour_ago, now).await.unwrap();

        // Should be ordered by count descending
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].model_name, Some("gpt-4".to_string()));
        assert_eq!(result[0].model_count, Some(2));
        assert_eq!(result[0].model_avg_latency_ms, Some(150.0)); // (100 + 200) / 2

        assert_eq!(result[1].model_name, Some("claude-3".to_string()));
        assert_eq!(result[1].model_count, Some(1));
        assert_eq!(result[1].model_avg_latency_ms, Some(300.0));
    }

    #[sqlx::test]
    async fn test_get_requests_aggregate_full_integration(pool: PgPool) {
        let base_time = Utc.with_ymd_and_hms(2024, 1, 1, 10, 0, 0).unwrap();

        // Insert comprehensive test data
        insert_test_analytics_data(&pool, base_time, "gpt-4", 200, 100.0, 50, 25).await;
        insert_test_analytics_data(&pool, base_time, "gpt-4", 200, 200.0, 75, 35).await;
        insert_test_analytics_data(&pool, base_time, "claude-3", 400, 300.0, 60, 30).await;
        insert_test_analytics_data(&pool, base_time + Duration::hours(1), "gpt-4", 500, 150.0, 40, 20).await;

        let result = get_requests_aggregate(&pool, base_time, base_time + Duration::hours(24), None)
            .await
            .unwrap();

        // Verify aggregated response
        assert_eq!(result.total_requests, 4);
        assert!(result.model.is_none());

        // Check status codes
        assert_eq!(result.status_codes.len(), 3);
        let status_200 = result.status_codes.iter().find(|s| s.status == "200").unwrap();
        assert_eq!(status_200.count, 2);
        assert_eq!(status_200.percentage, 50.0);

        // Check models
        assert!(result.models.is_some());
        let models = result.models.as_ref().unwrap();
        assert_eq!(models.len(), 2);

        let gpt4 = models.iter().find(|m| m.model == "gpt-4").unwrap();
        assert_eq!(gpt4.count, 3);
        assert_eq!(gpt4.percentage, 75.0);
        assert_eq!(gpt4.avg_latency_ms, 150.0); // (100 + 200 + 150) / 3

        // Check time series
        assert!(!result.time_series.is_empty());
    }

    #[sqlx::test]
    async fn test_get_requests_aggregate_with_model_filter(pool: PgPool) {
        let base_time = Utc::now() - Duration::hours(2);

        // Insert test data for multiple models
        insert_test_analytics_data(&pool, base_time, "gpt-4", 200, 100.0, 50, 25).await;
        insert_test_analytics_data(&pool, base_time, "claude-3", 400, 300.0, 60, 30).await;

        let result = get_requests_aggregate(&pool, base_time, base_time + Duration::hours(24), Some("gpt-4"))
            .await
            .unwrap();

        assert_eq!(result.total_requests, 1);
        assert_eq!(result.model, Some("gpt-4".to_string()));

        // When filtering by model, models array should be empty
        assert!(result.models.is_none() || result.models.as_ref().unwrap().is_empty());

        // Should only have status codes for the filtered model
        assert_eq!(result.status_codes.len(), 1);
        assert_eq!(result.status_codes[0].status, "200");
    }

    #[sqlx::test]
    async fn test_get_requests_aggregate_empty_database(pool: PgPool) {
        let base_time = Utc::now() - Duration::hours(24);
        let end_time = Utc::now();

        let result = get_requests_aggregate(&pool, base_time, end_time, None).await.unwrap();

        assert_eq!(result.total_requests, 0);
        assert_eq!(result.status_codes.len(), 0);
        assert!(result.models.is_none() || result.models.as_ref().unwrap().is_empty());

        // Time series should still be filled with zero values
        assert!(!result.time_series.is_empty());
        assert!(result.time_series.iter().all(|p| p.requests == 0));
    }

    #[sqlx::test]
    async fn test_percentage_calculations_precision(pool: PgPool) {
        let base_time = Utc::now() - Duration::hours(1);

        // Insert data that will test percentage precision
        for _i in 0..7 {
            insert_test_analytics_data(&pool, base_time, "gpt-4", 200, 100.0, 50, 25).await;
        }
        for _i in 0..3 {
            insert_test_analytics_data(&pool, base_time, "claude-3", 400, 300.0, 60, 30).await;
        }

        let result = get_requests_aggregate(&pool, base_time, Utc::now(), None).await.unwrap();

        assert_eq!(result.total_requests, 10);

        // Check status code percentages
        let status_200 = result.status_codes.iter().find(|s| s.status == "200").unwrap();
        assert_eq!(status_200.percentage, 70.0); // 7/10 * 100

        let status_400 = result.status_codes.iter().find(|s| s.status == "400").unwrap();
        assert_eq!(status_400.percentage, 30.0); // 3/10 * 100

        // Check model percentages
        let models = result.models.as_ref().unwrap();
        let gpt4 = models.iter().find(|m| m.model == "gpt-4").unwrap();
        assert_eq!(gpt4.percentage, 70.0);

        let claude3 = models.iter().find(|m| m.model == "claude-3").unwrap();
        assert_eq!(claude3.percentage, 30.0);
    }

    // Test analytics data with batch parameters
    struct TestBatchAnalyticsData<'a> {
        fusillade_batch_id: Uuid,
        fusillade_request_id: Option<Uuid>,
        timestamp: DateTime<Utc>,
        model: &'a str,
        status_code: i32,
        duration_ms: f64,
        duration_to_first_byte_ms: Option<f64>,
        prompt_tokens: i64,
        completion_tokens: i64,
        input_price_per_token: Option<f64>,
        output_price_per_token: Option<f64>,
    }

    // Helper function to insert analytics data with fusillade_request_id
    async fn insert_test_analytics_with_batch_id(pool: &PgPool, data: TestBatchAnalyticsData<'_>) {
        use rust_decimal::Decimal;
        use uuid::Uuid;

        sqlx::query!(
            r#"
            INSERT INTO http_analytics (
                instance_id, correlation_id, timestamp, uri, method, status_code,
                duration_ms, duration_to_first_byte_ms, model, prompt_tokens,
                completion_tokens, total_tokens, fusillade_batch_id, fusillade_request_id,
                input_price_per_token, output_price_per_token
            ) VALUES ($1, $2, $3, '/ai/chat/completions', 'POST', $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
            "#,
            Uuid::new_v4(),
            1i64,
            data.timestamp,
            data.status_code,
            data.duration_ms as i64,
            data.duration_to_first_byte_ms.map(|d| d as i64),
            data.model,
            data.prompt_tokens,
            data.completion_tokens,
            data.prompt_tokens + data.completion_tokens,
            data.fusillade_batch_id,
            data.fusillade_request_id,
            data.input_price_per_token.map(Decimal::from_f64_retain).flatten(),
            data.output_price_per_token.map(Decimal::from_f64_retain).flatten(),
        )
        .execute(pool)
        .await
        .expect("Failed to insert test analytics data with batch ID");
    }

    #[sqlx::test]
    async fn test_get_batch_analytics_single_request(pool: PgPool) {
        let batch_id = Uuid::new_v4();
        let now = Utc::now();

        // Insert a single analytics record
        insert_test_analytics_with_batch_id(
            &pool,
            TestBatchAnalyticsData {
                fusillade_batch_id: batch_id,
                fusillade_request_id: Some(Uuid::new_v4()),
                timestamp: now,
                model: "gpt-4",
                status_code: 200,
                duration_ms: 150.0,
                duration_to_first_byte_ms: Some(50.0),
                prompt_tokens: 100,
                completion_tokens: 50,
                input_price_per_token: Some(0.00001),  // $0.00001 per token
                output_price_per_token: Some(0.00003), // $0.00003 per token
            },
        )
        .await;

        let result = get_batch_analytics(&pool, &batch_id).await.unwrap();

        assert_eq!(result.total_requests, 1);
        assert_eq!(result.total_prompt_tokens, 100);
        assert_eq!(result.total_completion_tokens, 50);
        assert_eq!(result.total_tokens, 150);
        assert_eq!(result.avg_duration_ms, Some(150.0));
        assert_eq!(result.avg_ttfb_ms, Some(50.0));

        // Cost = (100 * 0.00001) + (50 * 0.00003) = 0.001 + 0.0015 = 0.0025
        let cost = result.total_cost.unwrap();
        let cost_f64: f64 = cost.parse().unwrap();
        assert!((cost_f64 - 0.0025).abs() < 0.00001);
    }

    #[sqlx::test]
    async fn test_get_batch_analytics_multiple_requests(pool: PgPool) {
        let batch_id = Uuid::new_v4();
        let now = Utc::now();

        // Insert multiple analytics records for the same batch
        insert_test_analytics_with_batch_id(
            &pool,
            TestBatchAnalyticsData {
                fusillade_batch_id: batch_id,
                fusillade_request_id: Some(Uuid::new_v4()),
                timestamp: now,
                model: "gpt-4",
                status_code: 200,
                duration_ms: 100.0,
                duration_to_first_byte_ms: Some(30.0),
                prompt_tokens: 50,
                completion_tokens: 25,
                input_price_per_token: Some(0.00001),
                output_price_per_token: Some(0.00003),
            },
        )
        .await;

        insert_test_analytics_with_batch_id(
            &pool,
            TestBatchAnalyticsData {
                fusillade_batch_id: batch_id,
                fusillade_request_id: Some(Uuid::new_v4()),
                timestamp: now,
                model: "gpt-4",
                status_code: 200,
                duration_ms: 200.0,
                duration_to_first_byte_ms: Some(70.0),
                prompt_tokens: 100,
                completion_tokens: 50,
                input_price_per_token: Some(0.00001),
                output_price_per_token: Some(0.00003),
            },
        )
        .await;

        insert_test_analytics_with_batch_id(
            &pool,
            TestBatchAnalyticsData {
                fusillade_batch_id: batch_id,
                fusillade_request_id: Some(Uuid::new_v4()),
                timestamp: now,
                model: "claude-3",
                status_code: 200,
                duration_ms: 150.0,
                duration_to_first_byte_ms: Some(40.0),
                prompt_tokens: 75,
                completion_tokens: 35,
                input_price_per_token: Some(0.00002),
                output_price_per_token: Some(0.00004),
            },
        )
        .await;

        let result = get_batch_analytics(&pool, &batch_id).await.unwrap();

        assert_eq!(result.total_requests, 3);
        assert_eq!(result.total_prompt_tokens, 225); // 50 + 100 + 75
        assert_eq!(result.total_completion_tokens, 110); // 25 + 50 + 35
        assert_eq!(result.total_tokens, 335); // 75 + 150 + 110

        // Average duration: (100 + 200 + 150) / 3 = 150.0
        assert_eq!(result.avg_duration_ms, Some(150.0));

        // Average TTFB: (30 + 70 + 40) / 3 = 46.666...
        let avg_ttfb = result.avg_ttfb_ms.unwrap();
        assert!((avg_ttfb - 46.666666666666664).abs() < 0.0001);

        // Total cost calculation:
        // req1: (50 * 0.00001) + (25 * 0.00003) = 0.0005 + 0.00075 = 0.00125
        // req2: (100 * 0.00001) + (50 * 0.00003) = 0.001 + 0.0015 = 0.0025
        // req3: (75 * 0.00002) + (35 * 0.00004) = 0.0015 + 0.0014 = 0.0029
        // Total: 0.00125 + 0.0025 + 0.0029 = 0.00665
        let cost = result.total_cost.unwrap();
        let cost_f64: f64 = cost.parse().unwrap();
        assert!((cost_f64 - 0.00665).abs() < 0.00001);
    }

    #[sqlx::test]
    async fn test_get_batch_analytics_nonexistent_batch_id(pool: PgPool) {
        let nonexistent_batch_id = Uuid::new_v4();

        let result = get_batch_analytics(&pool, &nonexistent_batch_id).await.unwrap();

        assert_eq!(result.total_requests, 0);
        assert_eq!(result.total_prompt_tokens, 0);
        assert_eq!(result.total_completion_tokens, 0);
        assert_eq!(result.total_tokens, 0);
        assert_eq!(result.avg_duration_ms, None);
        assert_eq!(result.avg_ttfb_ms, None);
        assert_eq!(result.total_cost, None);
    }

    #[sqlx::test]
    async fn test_get_batch_analytics_filters_by_batch_id(pool: PgPool) {
        let batch_id_1 = Uuid::new_v4();
        let batch_id_2 = Uuid::new_v4();
        let now = Utc::now();

        // Insert records for batch 1
        insert_test_analytics_with_batch_id(
            &pool,
            TestBatchAnalyticsData {
                fusillade_batch_id: batch_id_1,
                fusillade_request_id: Some(Uuid::new_v4()),
                timestamp: now,
                model: "gpt-4",
                status_code: 200,
                duration_ms: 100.0,
                duration_to_first_byte_ms: Some(30.0),
                prompt_tokens: 50,
                completion_tokens: 25,
                input_price_per_token: Some(0.00001),
                output_price_per_token: Some(0.00003),
            },
        )
        .await;

        // Insert records for batch 2 (should not be included)
        insert_test_analytics_with_batch_id(
            &pool,
            TestBatchAnalyticsData {
                fusillade_batch_id: batch_id_2,
                fusillade_request_id: Some(Uuid::new_v4()),
                timestamp: now,
                model: "gpt-4",
                status_code: 200,
                duration_ms: 200.0,
                duration_to_first_byte_ms: Some(40.0),
                prompt_tokens: 100,
                completion_tokens: 50,
                input_price_per_token: Some(0.00001),
                output_price_per_token: Some(0.00003),
            },
        )
        .await;

        // Query only for batch 1
        let result = get_batch_analytics(&pool, &batch_id_1).await.unwrap();

        // Should only return data for batch 1
        assert_eq!(result.total_requests, 1);
        assert_eq!(result.total_prompt_tokens, 50);
        assert_eq!(result.total_completion_tokens, 25);
        assert_eq!(result.total_tokens, 75);
        assert_eq!(result.avg_duration_ms, Some(100.0));
    }

    #[sqlx::test]
    async fn test_get_batch_analytics_missing_optional_fields(pool: PgPool) {
        let batch_id = Uuid::new_v4();
        let now = Utc::now();

        // Insert record without TTFB and pricing data
        insert_test_analytics_with_batch_id(
            &pool,
            TestBatchAnalyticsData {
                fusillade_batch_id: batch_id,
                fusillade_request_id: Some(Uuid::new_v4()),
                timestamp: now,
                model: "gpt-4",
                status_code: 200,
                duration_ms: 100.0,
                duration_to_first_byte_ms: None, // No TTFB
                prompt_tokens: 50,
                completion_tokens: 25,
                input_price_per_token: None,  // No input price
                output_price_per_token: None, // No output price
            },
        )
        .await;

        let result = get_batch_analytics(&pool, &batch_id).await.unwrap();

        assert_eq!(result.total_requests, 1);
        assert_eq!(result.total_prompt_tokens, 50);
        assert_eq!(result.total_completion_tokens, 25);
        assert_eq!(result.total_tokens, 75);
        assert_eq!(result.avg_duration_ms, Some(100.0));
        assert_eq!(result.avg_ttfb_ms, None); // Should be None when no TTFB data

        // Cost should be 0 when no pricing data
        let cost = result.total_cost.unwrap();
        let cost_f64: f64 = cost.parse().unwrap();
        assert_eq!(cost_f64, 0.0);
    }

    #[sqlx::test]
    async fn test_get_batch_analytics_multiple_requests_same_batch(pool: PgPool) {
        let batch_id = Uuid::new_v4();
        let other_batch_id = Uuid::new_v4();
        let now = Utc::now();

        // Insert multiple records for the same batch with different request IDs
        insert_test_analytics_with_batch_id(
            &pool,
            TestBatchAnalyticsData {
                fusillade_batch_id: batch_id,
                fusillade_request_id: Some(Uuid::new_v4()),
                timestamp: now,
                model: "gpt-4",
                status_code: 200,
                duration_ms: 100.0,
                duration_to_first_byte_ms: Some(30.0),
                prompt_tokens: 50,
                completion_tokens: 25,
                input_price_per_token: Some(0.00001),
                output_price_per_token: Some(0.00003),
            },
        )
        .await;

        insert_test_analytics_with_batch_id(
            &pool,
            TestBatchAnalyticsData {
                fusillade_batch_id: batch_id,
                fusillade_request_id: Some(Uuid::new_v4()),
                timestamp: now,
                model: "gpt-4",
                status_code: 200,
                duration_ms: 150.0,
                duration_to_first_byte_ms: Some(45.0),
                prompt_tokens: 75,
                completion_tokens: 30,
                input_price_per_token: Some(0.00001),
                output_price_per_token: Some(0.00003),
            },
        )
        .await;

        // Insert record for a different batch (should not be included)
        insert_test_analytics_with_batch_id(
            &pool,
            TestBatchAnalyticsData {
                fusillade_batch_id: other_batch_id,
                fusillade_request_id: Some(Uuid::new_v4()),
                timestamp: now,
                model: "gpt-4",
                status_code: 200,
                duration_ms: 200.0,
                duration_to_first_byte_ms: Some(40.0),
                prompt_tokens: 100,
                completion_tokens: 50,
                input_price_per_token: Some(0.00001),
                output_price_per_token: Some(0.00003),
            },
        )
        .await;

        // Query only for the first batch
        let result = get_batch_analytics(&pool, &batch_id).await.unwrap();

        // Should only include the two requests from the first batch, not the other batch
        assert_eq!(result.total_requests, 2);
        assert_eq!(result.total_prompt_tokens, 125); // 50 + 75
        assert_eq!(result.total_completion_tokens, 55); // 25 + 30
        assert_eq!(result.avg_duration_ms, Some(125.0)); // (100 + 150) / 2
    }

    #[sqlx::test]
    async fn test_get_batches_analytics_bulk_empty_input(pool: PgPool) {
        // Empty batch IDs should return empty HashMap
        let result = get_batches_analytics_bulk(&pool, &[]).await.unwrap();
        assert!(result.is_empty());
    }

    #[sqlx::test]
    async fn test_get_batches_analytics_bulk_multiple_batches(pool: PgPool) {
        let batch_id_1 = Uuid::new_v4();
        let batch_id_2 = Uuid::new_v4();
        let batch_id_3 = Uuid::new_v4(); // This one will have no analytics
        let now = Utc::now();

        // Insert analytics for batch 1
        insert_test_analytics_with_batch_id(
            &pool,
            TestBatchAnalyticsData {
                fusillade_batch_id: batch_id_1,
                fusillade_request_id: Some(Uuid::new_v4()),
                timestamp: now,
                model: "gpt-4",
                status_code: 200,
                duration_ms: 100.0,
                duration_to_first_byte_ms: Some(20.0),
                prompt_tokens: 50,
                completion_tokens: 25,
                input_price_per_token: Some(0.00001),
                output_price_per_token: Some(0.00003),
            },
        )
        .await;

        // Insert analytics for batch 2 (two requests)
        insert_test_analytics_with_batch_id(
            &pool,
            TestBatchAnalyticsData {
                fusillade_batch_id: batch_id_2,
                fusillade_request_id: Some(Uuid::new_v4()),
                timestamp: now,
                model: "gpt-3.5-turbo",
                status_code: 200,
                duration_ms: 50.0,
                duration_to_first_byte_ms: Some(10.0),
                prompt_tokens: 100,
                completion_tokens: 50,
                input_price_per_token: Some(0.000001),
                output_price_per_token: Some(0.000002),
            },
        )
        .await;
        insert_test_analytics_with_batch_id(
            &pool,
            TestBatchAnalyticsData {
                fusillade_batch_id: batch_id_2,
                fusillade_request_id: Some(Uuid::new_v4()),
                timestamp: now,
                model: "gpt-3.5-turbo",
                status_code: 200,
                duration_ms: 60.0,
                duration_to_first_byte_ms: Some(15.0),
                prompt_tokens: 200,
                completion_tokens: 100,
                input_price_per_token: Some(0.000001),
                output_price_per_token: Some(0.000002),
            },
        )
        .await;

        // Query bulk analytics for all three batches
        let result = get_batches_analytics_bulk(&pool, &[batch_id_1, batch_id_2, batch_id_3])
            .await
            .unwrap();

        // Should have entries for all three batches
        assert_eq!(result.len(), 3);

        // Batch 1 should have 1 request
        let analytics_1 = result.get(&batch_id_1).unwrap();
        assert_eq!(analytics_1.total_requests, 1);
        assert_eq!(analytics_1.total_prompt_tokens, 50);
        assert_eq!(analytics_1.total_completion_tokens, 25);

        // Batch 2 should have 2 requests aggregated
        let analytics_2 = result.get(&batch_id_2).unwrap();
        assert_eq!(analytics_2.total_requests, 2);
        assert_eq!(analytics_2.total_prompt_tokens, 300); // 100 + 200
        assert_eq!(analytics_2.total_completion_tokens, 150); // 50 + 100
        assert_eq!(analytics_2.avg_duration_ms, Some(55.0)); // (50 + 60) / 2

        // Batch 3 should have zero counts (no analytics data)
        let analytics_3 = result.get(&batch_id_3).unwrap();
        assert_eq!(analytics_3.total_requests, 0);
        assert_eq!(analytics_3.total_prompt_tokens, 0);
        assert_eq!(analytics_3.total_completion_tokens, 0);
        assert!(analytics_3.avg_duration_ms.is_none());
    }
}
