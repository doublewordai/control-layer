//! Database access layer for the probes monitoring system.
//!
//! This module provides the `ProbeManager` struct which handles all database
//! operations for probes, including CRUD operations, probe execution, and
//! statistics calculation.
//!
//! Background scheduling is handled separately by the `ProbeScheduler`.

use crate::api::models::probes::{CreateProbe, ProbeStatistics, UpdateProbeRequest};
use crate::db::models::probes::{Probe, ProbeExecution, ProbeResult};
use crate::errors::Error as AppError;
use crate::probes::executor::{ProbeExecutionContext, ProbeExecutor};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// Deployment status information returned by bulk status queries.
///
/// Tuple contains: (probe_id, active, interval_seconds, last_check, last_success, uptime_24h)
type DeploymentStatus = (
    Option<Uuid>,
    bool,
    Option<i32>,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<bool>,
    Option<f64>,
);

/// Map of deployment IDs to their status information.
type DeploymentStatusMap = std::collections::HashMap<Uuid, DeploymentStatus>;

/// Database access layer for probes.
///
/// This provides pure database operations for probes. Background scheduling
/// is handled by the separate `ProbeScheduler` which reads probe state from
/// the database.
pub struct ProbeManager;

impl ProbeManager {
    /// Create a new probe
    pub async fn create_probe(pool: &PgPool, probe: CreateProbe) -> Result<Probe, AppError> {
        let result = sqlx::query_as::<_, Probe>(
            r#"
            INSERT INTO probes (name, deployment_id, interval_seconds, active, http_method, request_path, request_body)
            VALUES ($1, $2, $3, true, $4, $5, $6)
            RETURNING *
            "#,
        )
        .bind(&probe.name)
        .bind(probe.deployment_id)
        .bind(probe.interval_seconds)
        .bind(&probe.http_method)
        .bind(&probe.request_path)
        .bind(&probe.request_body)
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create probe: {}", e))?;

        Ok(result)
    }

    /// Get a probe by ID
    pub async fn get_probe(pool: &PgPool, id: Uuid) -> Result<Probe, AppError> {
        let probe = sqlx::query_as::<_, Probe>(
            r#"
            SELECT * FROM probes WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to fetch probe: {}", e))?
        .ok_or_else(|| AppError::NotFound {
            resource: "Probe".to_string(),
            id: id.to_string(),
        })?;

        Ok(probe)
    }

    /// List all probes
    pub async fn list_probes(pool: &PgPool) -> Result<Vec<Probe>, AppError> {
        let probes = sqlx::query_as::<_, Probe>(
            r#"
            SELECT * FROM probes ORDER BY created_at DESC
            "#,
        )
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to list probes: {}", e))?;

        Ok(probes)
    }

    /// List active probes
    pub async fn list_active_probes(pool: &PgPool) -> Result<Vec<Probe>, AppError> {
        let probes = sqlx::query_as::<_, Probe>(
            r#"
            SELECT * FROM probes WHERE active = true ORDER BY created_at DESC
            "#,
        )
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to list active probes: {}", e))?;

        Ok(probes)
    }

    /// Get probe status for multiple deployments (bulk operation)
    /// Returns a map of deployment_id -> (probe_id, active, interval_seconds, last_check, last_success, uptime_24h)
    #[tracing::instrument(skip(pool, deployment_ids), fields(count = deployment_ids.len()), err)]
    pub async fn get_deployment_statuses(pool: &PgPool, deployment_ids: &[Uuid]) -> Result<DeploymentStatusMap, AppError> {
        if deployment_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        // Get probes for these deployments with their latest result
        let rows = sqlx::query!(
            r#"
            SELECT
                p.deployment_id,
                p.id as probe_id,
                p.active,
                p.interval_seconds,
                pr.executed_at as last_check,
                pr.success as last_success
            FROM probes p
            LEFT JOIN LATERAL (
                SELECT executed_at, success
                FROM probe_results
                WHERE probe_id = p.id
                ORDER BY executed_at DESC
                LIMIT 1
            ) pr ON true
            WHERE p.deployment_id = ANY($1)
            "#,
            deployment_ids
        )
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to fetch deployment statuses: {}", e))?;

        // Collect all active probe IDs for bulk uptime calculation
        let active_probe_ids: Vec<Uuid> = rows.iter().filter(|row| row.active).map(|row| row.probe_id).collect();

        // Calculate uptime for all active probes in one query
        let uptime_map = if !active_probe_ids.is_empty() {
            Self::calculate_uptime_percentages_bulk(pool, &active_probe_ids, chrono::Duration::hours(24))
                .await
                .unwrap_or_else(|_| std::collections::HashMap::new())
        } else {
            std::collections::HashMap::new()
        };

        let mut result = std::collections::HashMap::new();

        for row in rows {
            let deployment_id = row.deployment_id;
            let probe_id = row.probe_id;
            let active = row.active;
            let interval_seconds = row.interval_seconds;
            let last_check = row.last_check;
            let last_success = row.last_success;

            // Get uptime from the bulk-calculated map (only for active probes)
            let uptime_24h = if active { uptime_map.get(&probe_id).copied() } else { None };

            result.insert(
                deployment_id,
                (Some(probe_id), active, Some(interval_seconds), last_check, last_success, uptime_24h),
            );
        }

        Ok(result)
    }

    /// Calculate uptime percentages for multiple probes in bulk
    async fn calculate_uptime_percentages_bulk(
        pool: &PgPool,
        probe_ids: &[Uuid],
        duration: chrono::Duration,
    ) -> Result<std::collections::HashMap<Uuid, f64>, AppError> {
        if probe_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let since = chrono::Utc::now() - duration;

        let rows = sqlx::query!(
            r#"
            SELECT
                probe_id,
                COUNT(*) as total,
                COUNT(*) FILTER (WHERE success = true) as successful
            FROM probe_results
            WHERE probe_id = ANY($1) AND executed_at >= $2
            GROUP BY probe_id
            "#,
            probe_ids,
            since
        )
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to calculate bulk uptime: {}", e))?;

        let mut uptime_map = std::collections::HashMap::new();

        for row in rows {
            let total = row.total.unwrap_or(0);
            let successful = row.successful.unwrap_or(0);

            let uptime = if total == 0 {
                100.0 // No data = assume operational
            } else {
                (successful as f64 / total as f64) * 100.0
            };

            uptime_map.insert(row.probe_id, uptime);
        }

        // For probes with no results, assume 100% uptime
        for probe_id in probe_ids {
            uptime_map.entry(*probe_id).or_insert(100.0);
        }

        Ok(uptime_map)
    }

    /// Activate a probe
    pub async fn activate_probe(pool: &PgPool, id: Uuid) -> Result<Probe, AppError> {
        let probe = sqlx::query_as::<_, Probe>(
            r#"
            UPDATE probes SET active = true WHERE id = $1 RETURNING *
            "#,
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to activate probe: {}", e))?;

        Ok(probe)
    }

    /// Deactivate a probe
    pub async fn deactivate_probe(pool: &PgPool, id: Uuid) -> Result<Probe, AppError> {
        let probe = sqlx::query_as::<_, Probe>(
            r#"
            UPDATE probes SET active = false WHERE id = $1 RETURNING *
            "#,
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to deactivate probe: {}", e))?;

        Ok(probe)
    }

    /// Update a probe's configuration
    pub async fn update_probe(pool: &PgPool, id: Uuid, update: UpdateProbeRequest) -> Result<Probe, AppError> {
        let updated_probe = sqlx::query_as::<_, Probe>(
            r#"
            UPDATE probes
            SET interval_seconds = COALESCE($2, interval_seconds),
                http_method = COALESCE($3, http_method),
                request_path = COALESCE($4, request_path),
                request_body = COALESCE($5, request_body)
            WHERE id = $1
            RETURNING *
            "#,
        )
        .bind(id)
        .bind(update.interval_seconds)
        .bind(update.http_method)
        .bind(update.request_path)
        .bind(update.request_body)
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to update probe: {}", e))?;

        Ok(updated_probe)
    }

    /// Delete a probe
    pub async fn delete_probe(pool: &PgPool, id: Uuid) -> Result<(), AppError> {
        sqlx::query!(
            r#"
            DELETE FROM probes WHERE id = $1
            "#,
            id
        )
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to delete probe: {}", e))?;

        Ok(())
    }

    /// Test a probe configuration without creating it
    pub async fn test_probe(
        pool: &PgPool,
        deployment_id: Uuid,
        config: &crate::config::Config,
        http_method: Option<String>,
        request_path: Option<String>,
        request_body: Option<serde_json::Value>,
    ) -> Result<ProbeResult, AppError> {
        // Fetch deployment details - use alias to route through control layer
        let context = sqlx::query!(
            r#"
            SELECT
                d.alias,
                d.type as model_type,
                ak.secret as system_api_key
            FROM deployed_models d
            CROSS JOIN api_keys ak
            WHERE d.id = $1 AND ak.id = '00000000-0000-0000-0000-000000000000'::uuid
            "#,
            deployment_id
        )
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to fetch deployment for test: {}", e))?;

        let model_name = context.alias;
        let model_type_str = context.model_type;
        let system_api_key = context.system_api_key;

        // Route through control layer's normal AI proxy (not admin path)
        let endpoint_url = format!("http://localhost:{}/ai", config.port);
        let api_key = Some(system_api_key);

        // Parse model type - use auto-detection if not specified
        let model_type = match model_type_str.as_deref() {
            Some(t) => match t.to_uppercase().as_str() {
                "CHAT" => crate::db::models::deployments::ModelType::Chat,
                "EMBEDDINGS" => crate::db::models::deployments::ModelType::Embeddings,
                _ => {
                    return Err(AppError::BadRequest {
                        message: format!("Unknown model type: {}", t),
                    });
                }
            },
            None => crate::db::models::deployments::ModelType::detect_from_name(&model_name),
        };

        let execution_context = ProbeExecutionContext {
            probe_id: Uuid::nil(), // Use nil UUID for test probes
            model_name,
            model_type,
            endpoint_url,
            api_key,
            http_method: http_method.unwrap_or_else(|| "POST".to_string()),
            request_path,
            request_body,
        };

        let executor = ProbeExecutor::new();
        let mut execution = executor.execute(execution_context).await?;

        // Override probe_id to deployment_id for the test result
        execution.probe_id = deployment_id;

        // Return the result without storing it
        Ok(ProbeResult {
            id: Uuid::new_v4(),
            probe_id: deployment_id,
            executed_at: Utc::now(),
            success: execution.success,
            response_time_ms: Some(execution.response_time_ms),
            status_code: execution.status_code,
            error_message: execution.error_message,
            response_data: execution.response_data,
            metadata: execution.metadata,
        })
    }

    /// Execute a probe and store the result
    pub async fn execute_probe(pool: &PgPool, id: Uuid, config: &crate::config::Config) -> Result<ProbeResult, AppError> {
        // Note: We allow executing inactive probes manually via "Run Now"
        let _probe = Self::get_probe(pool, id).await?;

        // Fetch deployment details and probe configuration - use alias to route through control layer
        let context = sqlx::query!(
            r#"
            SELECT
                p.id as probe_id,
                p.http_method,
                p.request_path,
                p.request_body,
                d.alias,
                d.type as model_type,
                ak.secret as system_api_key
            FROM probes p
            JOIN deployed_models d ON p.deployment_id = d.id
            CROSS JOIN api_keys ak
            WHERE p.id = $1 AND ak.id = '00000000-0000-0000-0000-000000000000'::uuid
            "#,
            id
        )
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to fetch probe execution context: {}", e))?;

        let probe_id = context.probe_id;
        let http_method = context.http_method;
        let request_path = context.request_path;
        let request_body = context.request_body;
        let model_name = context.alias;
        let model_type_str = context.model_type;
        let system_api_key = context.system_api_key;

        // Route through control layer's normal AI proxy (not admin path)
        let endpoint_url = format!("http://localhost:{}/ai", config.port);
        let api_key = Some(system_api_key);

        // Parse model type - use auto-detection if not specified
        let model_type = match model_type_str.as_deref() {
            Some(t) => match t.to_uppercase().as_str() {
                "CHAT" => crate::db::models::deployments::ModelType::Chat,
                "EMBEDDINGS" => crate::db::models::deployments::ModelType::Embeddings,
                _ => {
                    return Err(AppError::BadRequest {
                        message: format!("Unknown model type: {}", t),
                    });
                }
            },
            None => crate::db::models::deployments::ModelType::detect_from_name(&model_name),
        };

        let execution_context = ProbeExecutionContext {
            probe_id,
            model_name,
            model_type,
            endpoint_url,
            api_key,
            http_method,
            request_path,
            request_body,
        };

        let executor = ProbeExecutor::new();
        let execution = executor.execute(execution_context).await?;

        let result = Self::store_result(pool, execution).await?;

        Ok(result)
    }

    /// Store a probe execution result
    async fn store_result(pool: &PgPool, execution: ProbeExecution) -> Result<ProbeResult, AppError> {
        let result = sqlx::query_as::<_, ProbeResult>(
            r#"
            INSERT INTO probe_results
            (probe_id, success, response_time_ms, status_code, error_message, response_data, metadata)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            RETURNING *
            "#,
        )
        .bind(execution.probe_id)
        .bind(execution.success)
        .bind(execution.response_time_ms)
        .bind(execution.status_code)
        .bind(execution.error_message)
        .bind(execution.response_data)
        .bind(execution.metadata)
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to store probe result: {}", e))?;

        Ok(result)
    }

    /// Get probe results with optional filters
    pub async fn get_probe_results(
        pool: &PgPool,
        probe_id: Uuid,
        start_time: Option<DateTime<Utc>>,
        end_time: Option<DateTime<Utc>>,
        limit: Option<i64>,
    ) -> Result<Vec<ProbeResult>, AppError> {
        let mut query = String::from(
            r#"
            SELECT * FROM probe_results
            WHERE probe_id = $1
            "#,
        );

        let mut param_count = 1;

        if start_time.is_some() {
            param_count += 1;
            query.push_str(&format!(" AND executed_at >= ${}", param_count));
        }

        if end_time.is_some() {
            param_count += 1;
            query.push_str(&format!(" AND executed_at <= ${}", param_count));
        }

        query.push_str(" ORDER BY executed_at DESC");

        if limit.is_some() {
            param_count += 1;
            query.push_str(&format!(" LIMIT ${}", param_count));
        }

        let mut sql_query = sqlx::query_as::<_, ProbeResult>(&query).bind(probe_id);

        if let Some(start) = start_time {
            sql_query = sql_query.bind(start);
        }

        if let Some(end) = end_time {
            sql_query = sql_query.bind(end);
        }

        if let Some(lim) = limit {
            sql_query = sql_query.bind(lim);
        }

        let results = sql_query
            .fetch_all(pool)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to fetch probe results: {}", e))?;

        Ok(results)
    }

    /// Get the last N results for a probe
    pub async fn get_recent_results(pool: &PgPool, probe_id: Uuid, limit: i64) -> Result<Vec<ProbeResult>, AppError> {
        Self::get_probe_results(pool, probe_id, None, None, Some(limit)).await
    }

    /// Calculate aggregated statistics for a probe over a time period.
    ///
    /// Computes success rates, response time percentiles, and execution counts
    /// from stored probe results.
    pub async fn get_statistics(
        pool: &PgPool,
        probe_id: Uuid,
        start_time: Option<DateTime<Utc>>,
        end_time: Option<DateTime<Utc>>,
    ) -> Result<ProbeStatistics, AppError> {
        // Define a struct to hold the common query result
        #[derive(sqlx::FromRow)]
        struct StatsRow {
            total: Option<i64>,
            successful: Option<i64>,
            failed: Option<i64>,
            avg_time: Option<f64>,
            min_time: Option<i32>,
            max_time: Option<i32>,
            p50: Option<f64>,
            p95: Option<f64>,
            p99: Option<f64>,
            last_execution: Option<DateTime<Utc>>,
            last_success: Option<DateTime<Utc>>,
            last_failure: Option<DateTime<Utc>>,
        }

        let row = match (start_time, end_time) {
            (None, None) => sqlx::query_as!(
                StatsRow,
                r#"
                    SELECT
                        COUNT(*) as total,
                        COUNT(*) FILTER (WHERE success = true) as successful,
                        COUNT(*) FILTER (WHERE success = false) as failed,
                        (AVG(response_time_ms) FILTER (WHERE success = true))::float8 as avg_time,
                        MIN(response_time_ms) FILTER (WHERE success = true) as min_time,
                        MAX(response_time_ms) FILTER (WHERE success = true) as max_time,
                        (PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY response_time_ms) FILTER (WHERE success = true))::float8 as p50,
                        (PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY response_time_ms) FILTER (WHERE success = true))::float8 as p95,
                        (PERCENTILE_CONT(0.99) WITHIN GROUP (ORDER BY response_time_ms) FILTER (WHERE success = true))::float8 as p99,
                        MAX(executed_at) as last_execution,
                        MAX(executed_at) FILTER (WHERE success = true) as last_success,
                        MAX(executed_at) FILTER (WHERE success = false) as last_failure
                    FROM probe_results
                    WHERE probe_id = $1
                    "#,
                probe_id
            )
            .fetch_one(pool)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to fetch probe statistics: {}", e))?,
            (Some(start), None) => sqlx::query_as!(
                StatsRow,
                r#"
                    SELECT
                        COUNT(*) as total,
                        COUNT(*) FILTER (WHERE success = true) as successful,
                        COUNT(*) FILTER (WHERE success = false) as failed,
                        (AVG(response_time_ms) FILTER (WHERE success = true))::float8 as avg_time,
                        MIN(response_time_ms) FILTER (WHERE success = true) as min_time,
                        MAX(response_time_ms) FILTER (WHERE success = true) as max_time,
                        (PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY response_time_ms) FILTER (WHERE success = true))::float8 as p50,
                        (PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY response_time_ms) FILTER (WHERE success = true))::float8 as p95,
                        (PERCENTILE_CONT(0.99) WITHIN GROUP (ORDER BY response_time_ms) FILTER (WHERE success = true))::float8 as p99,
                        MAX(executed_at) as last_execution,
                        MAX(executed_at) FILTER (WHERE success = true) as last_success,
                        MAX(executed_at) FILTER (WHERE success = false) as last_failure
                    FROM probe_results
                    WHERE probe_id = $1 AND executed_at >= $2
                    "#,
                probe_id,
                start
            )
            .fetch_one(pool)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to fetch probe statistics: {}", e))?,
            (None, Some(end)) => sqlx::query_as!(
                StatsRow,
                r#"
                    SELECT
                        COUNT(*) as total,
                        COUNT(*) FILTER (WHERE success = true) as successful,
                        COUNT(*) FILTER (WHERE success = false) as failed,
                        (AVG(response_time_ms) FILTER (WHERE success = true))::float8 as avg_time,
                        MIN(response_time_ms) FILTER (WHERE success = true) as min_time,
                        MAX(response_time_ms) FILTER (WHERE success = true) as max_time,
                        (PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY response_time_ms) FILTER (WHERE success = true))::float8 as p50,
                        (PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY response_time_ms) FILTER (WHERE success = true))::float8 as p95,
                        (PERCENTILE_CONT(0.99) WITHIN GROUP (ORDER BY response_time_ms) FILTER (WHERE success = true))::float8 as p99,
                        MAX(executed_at) as last_execution,
                        MAX(executed_at) FILTER (WHERE success = true) as last_success,
                        MAX(executed_at) FILTER (WHERE success = false) as last_failure
                    FROM probe_results
                    WHERE probe_id = $1 AND executed_at <= $2
                    "#,
                probe_id,
                end
            )
            .fetch_one(pool)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to fetch probe statistics: {}", e))?,
            (Some(start), Some(end)) => sqlx::query_as!(
                StatsRow,
                r#"
                    SELECT
                        COUNT(*) as total,
                        COUNT(*) FILTER (WHERE success = true) as successful,
                        COUNT(*) FILTER (WHERE success = false) as failed,
                        (AVG(response_time_ms) FILTER (WHERE success = true))::float8 as avg_time,
                        MIN(response_time_ms) FILTER (WHERE success = true) as min_time,
                        MAX(response_time_ms) FILTER (WHERE success = true) as max_time,
                        (PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY response_time_ms) FILTER (WHERE success = true))::float8 as p50,
                        (PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY response_time_ms) FILTER (WHERE success = true))::float8 as p95,
                        (PERCENTILE_CONT(0.99) WITHIN GROUP (ORDER BY response_time_ms) FILTER (WHERE success = true))::float8 as p99,
                        MAX(executed_at) as last_execution,
                        MAX(executed_at) FILTER (WHERE success = true) as last_success,
                        MAX(executed_at) FILTER (WHERE success = false) as last_failure
                    FROM probe_results
                    WHERE probe_id = $1 AND executed_at >= $2 AND executed_at <= $3
                    "#,
                probe_id,
                start,
                end
            )
            .fetch_one(pool)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to fetch probe statistics: {}", e))?,
        };

        let total = row.total.unwrap_or(0);
        let successful = row.successful.unwrap_or(0);
        let failed = row.failed.unwrap_or(0);
        let success_rate = if total > 0 {
            (successful as f64 / total as f64) * 100.0
        } else {
            0.0
        };

        Ok(ProbeStatistics {
            total_executions: total,
            successful_executions: successful,
            failed_executions: failed,
            success_rate,
            avg_response_time_ms: row.avg_time,
            min_response_time_ms: row.min_time,
            max_response_time_ms: row.max_time,
            p50_response_time_ms: row.p50,
            p95_response_time_ms: row.p95,
            p99_response_time_ms: row.p99,
            last_execution: row.last_execution,
            last_success: row.last_success,
            last_failure: row.last_failure,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;

    async fn setup_test_deployment(pool: &PgPool) -> Uuid {
        // Generate unique names using UUID
        let unique_id = Uuid::new_v4();
        let endpoint_name = format!("test-endpoint-{}", unique_id);
        let model_name = format!("test-model-{}", unique_id);

        // Create test endpoint
        let endpoint_id = sqlx::query_scalar!(
            "INSERT INTO inference_endpoints (name, url, created_by) VALUES ($1, $2, $3) RETURNING id",
            endpoint_name,
            "http://localhost:8080",
            Uuid::nil()
        )
        .fetch_one(pool)
        .await
        .unwrap();

        // Create test deployment
        sqlx::query_scalar!(
            "INSERT INTO deployed_models (model_name, alias, type, hosted_on, created_by) VALUES ($1, $2, $3, $4, $5) RETURNING id",
            model_name.clone(),
            model_name,
            "chat" as _,
            endpoint_id,
            Uuid::nil()
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn test_create_and_get_probe(pool: PgPool) {
        let deployment_id = setup_test_deployment(&pool).await;

        let probe_create = CreateProbe {
            name: "Test Probe".to_string(),
            deployment_id,
            interval_seconds: 60,
            http_method: "POST".to_string(),
            request_path: None,
            request_body: None,
        };

        let created = ProbeManager::create_probe(&pool, probe_create).await.unwrap();

        assert_eq!(created.name, "Test Probe");
        assert_eq!(created.deployment_id, deployment_id);
        assert_eq!(created.interval_seconds, 60);
        assert!(created.active); // New probes are active by default

        // Test get_probe
        let fetched = ProbeManager::get_probe(&pool, created.id).await.unwrap();
        assert_eq!(fetched.id, created.id);
        assert_eq!(fetched.name, created.name);
    }

    #[sqlx::test]
    async fn test_list_probes(pool: PgPool) {
        // Create separate deployment for each probe
        for i in 0..3 {
            let deployment_id = setup_test_deployment(&pool).await;
            ProbeManager::create_probe(
                &pool,
                CreateProbe {
                    name: format!("Probe {}", i),
                    deployment_id,
                    interval_seconds: 60,
                    http_method: "POST".to_string(),
                    request_path: None,
                    request_body: None,
                },
            )
            .await
            .unwrap();
        }

        let probes = ProbeManager::list_probes(&pool).await.unwrap();
        assert_eq!(probes.len(), 3);
    }

    #[sqlx::test]
    async fn test_list_active_probes(pool: PgPool) {
        // Create separate deployments for each probe
        let deployment_id1 = setup_test_deployment(&pool).await;
        let deployment_id2 = setup_test_deployment(&pool).await;

        // Create probes
        let probe1 = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Active Probe".to_string(),
                deployment_id: deployment_id1,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        let probe2 = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Inactive Probe".to_string(),
                deployment_id: deployment_id2,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        // Deactivate one probe
        ProbeManager::deactivate_probe(&pool, probe2.id).await.unwrap();

        let active = ProbeManager::list_active_probes(&pool).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, probe1.id);
    }

    #[sqlx::test]
    async fn test_activate_deactivate_probe(pool: PgPool) {
        let deployment_id = setup_test_deployment(&pool).await;

        let probe = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Test Probe".to_string(),
                deployment_id,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        assert!(probe.active);

        // Deactivate
        let deactivated = ProbeManager::deactivate_probe(&pool, probe.id).await.unwrap();
        assert!(!deactivated.active);

        // Reactivate
        let activated = ProbeManager::activate_probe(&pool, probe.id).await.unwrap();
        assert!(activated.active);
    }

    #[sqlx::test]
    async fn test_update_probe(pool: PgPool) {
        let deployment_id = setup_test_deployment(&pool).await;

        let probe = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Test Probe".to_string(),
                deployment_id,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        // Update interval
        let updated = ProbeManager::update_probe(
            &pool,
            probe.id,
            UpdateProbeRequest {
                interval_seconds: Some(120),
                http_method: None,
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(updated.interval_seconds, 120);

        // Update with None should keep existing value
        let unchanged = ProbeManager::update_probe(
            &pool,
            probe.id,
            UpdateProbeRequest {
                interval_seconds: None,
                http_method: None,
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(unchanged.interval_seconds, 120);
    }

    #[sqlx::test]
    async fn test_delete_probe(pool: PgPool) {
        let deployment_id = setup_test_deployment(&pool).await;

        let probe = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Test Probe".to_string(),
                deployment_id,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        ProbeManager::delete_probe(&pool, probe.id).await.unwrap();

        // Verify it's deleted
        let result = ProbeManager::get_probe(&pool, probe.id).await;
        assert!(result.is_err());
    }

    #[sqlx::test]
    async fn test_get_deployment_statuses(pool: PgPool) {
        let deployment_id = setup_test_deployment(&pool).await;

        let probe = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Test Probe".to_string(),
                deployment_id,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        let statuses = ProbeManager::get_deployment_statuses(&pool, &[deployment_id]).await.unwrap();

        assert_eq!(statuses.len(), 1);
        let (probe_id, active, interval, _, _, _) = statuses.get(&deployment_id).unwrap();
        assert_eq!(*probe_id, Some(probe.id));
        assert!(*active);
        assert_eq!(*interval, Some(60));
    }

    #[sqlx::test]
    async fn test_get_statistics_empty(pool: PgPool) {
        let deployment_id = setup_test_deployment(&pool).await;

        let probe = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Test Probe".to_string(),
                deployment_id,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        let stats = ProbeManager::get_statistics(&pool, probe.id, None, None).await.unwrap();

        assert_eq!(stats.total_executions, 0);
        assert_eq!(stats.successful_executions, 0);
        assert_eq!(stats.failed_executions, 0);
        assert_eq!(stats.success_rate, 0.0);
    }

    #[sqlx::test]
    async fn test_get_probe_results_empty(pool: PgPool) {
        let deployment_id = setup_test_deployment(&pool).await;

        let probe = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Test Probe".to_string(),
                deployment_id,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        let results = ProbeManager::get_probe_results(&pool, probe.id, None, None, None).await.unwrap();
        assert_eq!(results.len(), 0);
    }

    #[sqlx::test]
    async fn test_probe_notify_trigger(pool: PgPool) {
        use sqlx::postgres::PgListener;
        use tokio::time::{Duration, timeout};

        let deployment_id = setup_test_deployment(&pool).await;

        // Create a listener for probe changes
        let mut listener = PgListener::connect_with(&pool).await.unwrap();
        listener.listen("probe_changes").await.unwrap();

        // Test INSERT notification
        let probe = ProbeManager::create_probe(
            &pool,
            CreateProbe {
                name: "Test Probe".to_string(),
                deployment_id,
                interval_seconds: 60,
                http_method: "POST".to_string(),
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        // Wait for and verify INSERT notification
        let notification = timeout(Duration::from_secs(2), listener.recv())
            .await
            .expect("Timeout waiting for INSERT notification")
            .unwrap();

        let payload: serde_json::Value = serde_json::from_str(notification.payload()).unwrap();
        assert_eq!(payload["action"], "INSERT");
        assert_eq!(payload["probe_id"], probe.id.to_string());
        assert_eq!(payload["active"], true);

        // Test UPDATE notification
        ProbeManager::update_probe(
            &pool,
            probe.id,
            UpdateProbeRequest {
                interval_seconds: Some(120),
                http_method: None,
                request_path: None,
                request_body: None,
            },
        )
        .await
        .unwrap();

        let notification = timeout(Duration::from_secs(2), listener.recv())
            .await
            .expect("Timeout waiting for UPDATE notification")
            .unwrap();

        let payload: serde_json::Value = serde_json::from_str(notification.payload()).unwrap();
        assert_eq!(payload["action"], "UPDATE");
        assert_eq!(payload["probe_id"], probe.id.to_string());
        assert_eq!(payload["active"], true);

        // Test DELETE notification
        ProbeManager::delete_probe(&pool, probe.id).await.unwrap();

        let notification = timeout(Duration::from_secs(2), listener.recv())
            .await
            .expect("Timeout waiting for DELETE notification")
            .unwrap();

        let payload: serde_json::Value = serde_json::from_str(notification.payload()).unwrap();
        assert_eq!(payload["action"], "DELETE");
        assert_eq!(payload["probe_id"], probe.id.to_string());
        assert_eq!(payload["active"], true);
    }
}
