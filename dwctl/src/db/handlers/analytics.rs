//! Database queries for request analytics and aggregation.

use chrono::{DateTime, Duration, Timelike, Utc};
use sqlx::{FromRow, PgPool};
use std::collections::HashMap;
use tracing::instrument;

use crate::{
    api::models::{
        deployments::{ModelMetrics, ModelTimeSeriesPoint},
        requests::{ModelUsage, ModelUserUsageResponse, RequestsAggregateResponse, StatusCodeBreakdown, TimeSeriesPoint, UserUsage},
    },
    db::errors::Result,
};

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
    pub total_requests: Option<i64>,
    pub avg_latency_ms: Option<f64>,
    pub total_input_tokens: Option<i64>,
    pub total_output_tokens: Option<i64>,
    pub last_active_at: Option<DateTime<Utc>>,
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

/// Get aggregated metrics for a specific model
#[instrument(skip(db), err)]
pub async fn get_model_metrics(db: &PgPool, model_alias: &str) -> Result<ModelMetrics> {
    // Get basic metrics
    let row = sqlx::query_as!(
        ModelMetricsRow,
        r#"
        SELECT
            COUNT(*) as total_requests,
            AVG(duration_ms)::float8 as avg_latency_ms,
            COALESCE(SUM(prompt_tokens), 0)::bigint as total_input_tokens,
            COALESCE(SUM(completion_tokens), 0)::bigint as total_output_tokens,
            MAX(timestamp) as last_active_at
        FROM http_analytics
        WHERE model = $1
        "#,
        model_alias
    )
    .fetch_one(db)
    .await?;

    // Get time series data for sparklines (last 2 hours in 10-minute intervals)
    let now = Utc::now();
    let two_hours_ago = now - Duration::hours(2);
    let sparkline_data = match get_time_series(db, two_hours_ago, now, Some(model_alias), TimeGranularity::TenMinutes).await {
        Ok(time_series_data) => {
            let sparkline_points: Vec<ModelTimeSeriesPoint> = time_series_data
                .into_iter()
                .map(|point| ModelTimeSeriesPoint {
                    timestamp: point.timestamp,
                    requests: point.requests,
                })
                .collect();
            Some(sparkline_points)
        }
        Err(_) => {
            // If time series fails, still return metrics without sparkline
            tracing::warn!("Failed to fetch time series data for model {}", model_alias);
            None
        }
    };

    Ok(ModelMetrics {
        avg_latency_ms: row.avg_latency_ms,
        total_requests: row.total_requests.unwrap_or(0),
        total_input_tokens: row.total_input_tokens.unwrap_or(0),
        total_output_tokens: row.total_output_tokens.unwrap_or(0),
        last_active_at: row.last_active_at,
        time_series: sparkline_data,
    })
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
    // Get user-grouped data (only rows with both user_id and user_email)
    let user_rows = sqlx::query_as!(
        UserUsageRow,
        r#"
        SELECT
            user_id,
            user_email,
            COUNT(*) as request_count,
            COALESCE(SUM(prompt_tokens), 0)::bigint as total_input_tokens,
            COALESCE(SUM(completion_tokens), 0)::bigint as total_output_tokens,
            COALESCE(SUM(total_tokens), 0)::bigint as total_tokens,
            SUM(total_cost)::float8 as total_cost,
            MAX(timestamp) as last_active_at
        FROM http_analytics
        WHERE model = $1
            AND timestamp >= $2
            AND timestamp <= $3
            AND user_id IS NOT NULL
            AND user_email IS NOT NULL
        GROUP BY user_id, user_email
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
            AND user_email IS NOT NULL
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
}
