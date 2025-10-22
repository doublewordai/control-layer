//! Database access layer for the probes monitoring system.
//!
//! This module provides the `ProbeManager` struct which manages:
//! - CRUD operations for probes
//! - Probe execution and result storage
//! - Statistics calculation
//! - Background scheduling of active probes

use crate::errors::Error as AppError;
use crate::probes::executor::{ProbeExecutionContext, ProbeExecutor};
use crate::probes::models::{CreateProbe, Probe, ProbeExecution, ProbeResult, ProbeStatistics};
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Main interface for interacting with the probes monitoring system.
///
/// The `ProbeManager` struct manages probe scheduling and database operations.
/// It maintains a collection of background tasks that execute probes at
/// their configured intervals.
#[derive(Clone)]
pub struct ProbeManager {
    schedulers: Arc<RwLock<HashMap<Uuid, JoinHandle<()>>>>,
}

impl Default for ProbeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ProbeManager {
    /// Create a new `ProbeManager` instance.
    ///
    /// This creates an empty scheduler collection. Call `initialize_schedulers()`
    /// to start background tasks for all active probes.
    pub fn new() -> Self {
        Self {
            schedulers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Initialize schedulers for all active probes in the database.
    ///
    /// This should be called on application startup to resume monitoring
    /// for all probes that were active when the service last shutdown.
    pub async fn initialize_schedulers(&self, pool: &PgPool) -> Result<(), AppError> {
        let probes = Self::list_active_probes(pool).await?;

        for probe in probes {
            self.start_scheduler(pool.clone(), probe.id).await?;
        }

        Ok(())
    }

    /// Start a scheduler for a probe
    pub async fn start_scheduler(&self, pool: PgPool, probe_id: Uuid) -> Result<(), AppError> {
        // Check if scheduler already exists
        {
            let schedulers = self.schedulers.read().await;
            if schedulers.contains_key(&probe_id) {
                return Ok(());
            }
        }

        // Spawn the scheduler task
        let handle = tokio::spawn(async move {
            // Check when the probe last executed to avoid immediate execution on restart
            let _should_delay = match Self::get_recent_results(&pool, probe_id, 1).await {
                Ok(results) => {
                    if let Some(last_result) = results.first() {
                        let probe = match Self::get_probe(&pool, probe_id).await {
                            Ok(p) => p,
                            Err(e) => {
                                tracing::error!("Error fetching probe {}: {}", probe_id, e);
                                return;
                            }
                        };

                        let now = chrono::Utc::now();
                        let time_since_last = now - last_result.executed_at;
                        let interval = chrono::Duration::seconds(probe.interval_seconds as i64);

                        if time_since_last < interval {
                            // Calculate how long to wait until next scheduled execution
                            let wait_duration = interval - time_since_last;
                            let wait_secs = wait_duration.num_seconds().max(0) as u64;

                            tracing::info!(
                                "Probe {} last executed {}s ago, waiting {}s until next execution",
                                probe.name,
                                time_since_last.num_seconds(),
                                wait_secs
                            );

                            tokio::time::sleep(tokio::time::Duration::from_secs(wait_secs)).await;
                            false // Don't delay again
                        } else {
                            tracing::info!(
                                "Probe {} last executed {}s ago (>{}s interval), executing immediately",
                                probe.name,
                                time_since_last.num_seconds(),
                                probe.interval_seconds
                            );
                            true // Execute immediately
                        }
                    } else {
                        true // No previous results, execute immediately
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Error checking last execution for probe {}: {}, will execute immediately",
                        probe_id,
                        e
                    );
                    true
                }
            };

            loop {
                // Get the probe to check if it's still active and get the interval
                let probe = match Self::get_probe(&pool, probe_id).await {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!("Error fetching probe {}: {}", probe_id, e);
                        break;
                    }
                };

                // If probe is not active, stop the scheduler
                if !probe.active {
                    tracing::info!("Probe {} is not active, stopping scheduler", probe.name);
                    break;
                }

                // Execute the probe
                match Self::execute_probe(&pool, probe_id).await {
                    Ok(result) => {
                        if result.success {
                            tracing::debug!(
                                "Probe {} executed successfully in {}ms",
                                probe.name,
                                result.response_time_ms.unwrap_or(0)
                            );
                        } else {
                            tracing::warn!(
                                "Probe {} execution failed: {:?}",
                                probe.name,
                                result.error_message
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!("Error executing probe {}: {}", probe.name, e);
                    }
                }

                // Sleep for the configured interval
                tokio::time::sleep(tokio::time::Duration::from_secs(
                    probe.interval_seconds as u64,
                ))
                .await;
            }

            tracing::info!("Scheduler for probe {} has stopped", probe_id);
        });

        // Store the handle
        let mut schedulers = self.schedulers.write().await;
        schedulers.insert(probe_id, handle);

        Ok(())
    }

    /// Stop a scheduler for a probe
    pub async fn stop_scheduler(&self, probe_id: Uuid) -> Result<(), AppError> {
        let mut schedulers = self.schedulers.write().await;

        if let Some(handle) = schedulers.remove(&probe_id) {
            handle.abort();
            tracing::info!("Stopped scheduler for probe {}", probe_id);
        }

        Ok(())
    }

    /// Check if a scheduler is running for a probe
    pub async fn is_scheduler_running(&self, probe_id: Uuid) -> bool {
        let schedulers = self.schedulers.read().await;
        schedulers.contains_key(&probe_id)
    }

    /// Create a new probe and start its scheduler.
    ///
    /// The probe is created as active by default and a background scheduler
    /// is started immediately to execute the probe at its configured interval.
    pub async fn create_probe(&self, pool: &PgPool, probe: CreateProbe) -> Result<Probe, AppError> {
        let result = sqlx::query_as::<_, Probe>(
            r#"
            INSERT INTO probes (name, deployment_id, interval_seconds, active)
            VALUES ($1, $2, $3, true)
            RETURNING *
            "#,
        )
        .bind(&probe.name)
        .bind(&probe.deployment_id)
        .bind(probe.interval_seconds)
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create probe: {}", e))?;

        // Auto-start scheduler for new probes
        self.start_scheduler(pool.clone(), result.id).await?;

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

    /// Get a probe by name
    pub async fn get_probe_by_name(pool: &PgPool, name: &str) -> Result<Probe, AppError> {
        let probe = sqlx::query_as::<_, Probe>(
            r#"
            SELECT * FROM probes WHERE name = $1
            "#,
        )
        .bind(name)
        .fetch_optional(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to fetch probe by name: {}", e))?
        .ok_or_else(|| AppError::NotFound {
            resource: "Probe".to_string(),
            id: name.to_string(),
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
    pub async fn get_deployment_statuses(
        pool: &PgPool,
        deployment_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, (Option<Uuid>, bool, Option<i32>, Option<chrono::DateTime<chrono::Utc>>, Option<bool>, Option<f64>)>, AppError> {
        if deployment_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        // Get probes for these deployments with their latest result
        let rows = sqlx::query(
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
        )
        .bind(deployment_ids)
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to fetch deployment statuses: {}", e))?;

        let mut result = std::collections::HashMap::new();

        for row in rows {
            let deployment_id: Uuid = row.get("deployment_id");
            let probe_id: Uuid = row.get("probe_id");
            let active: bool = row.get("active");
            let interval_seconds: i32 = row.get("interval_seconds");
            let last_check: Option<chrono::DateTime<chrono::Utc>> = row.get("last_check");
            let last_success: Option<bool> = row.get("last_success");

            // Calculate 24h uptime for this probe
            let uptime_24h = if active {
                Self::calculate_uptime_percentage(pool, probe_id, chrono::Duration::hours(24)).await.ok()
            } else {
                None
            };

            result.insert(
                deployment_id,
                (Some(probe_id), active, Some(interval_seconds), last_check, last_success, uptime_24h),
            );
        }

        Ok(result)
    }

    /// Calculate uptime percentage for a probe over a time period
    async fn calculate_uptime_percentage(
        pool: &PgPool,
        probe_id: Uuid,
        duration: chrono::Duration,
    ) -> Result<f64, AppError> {
        let since = chrono::Utc::now() - duration;

        let row = sqlx::query(
            r#"
            SELECT
                COUNT(*) as total,
                COUNT(*) FILTER (WHERE success = true) as successful
            FROM probe_results
            WHERE probe_id = $1 AND executed_at >= $2
            "#,
        )
        .bind(probe_id)
        .bind(since)
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to calculate uptime: {}", e))?;

        let total: i64 = row.get("total");
        let successful: i64 = row.get("successful");

        if total == 0 {
            return Ok(100.0); // No data = assume operational
        }

        Ok((successful as f64 / total as f64) * 100.0)
    }

    /// Activate a probe and start its scheduler
    pub async fn activate_probe(&self, pool: &PgPool, id: Uuid) -> Result<Probe, AppError> {
        let probe = sqlx::query_as::<_, Probe>(
            r#"
            UPDATE probes SET active = true WHERE id = $1 RETURNING *
            "#,
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to activate probe: {}", e))?;

        // Start the scheduler
        self.start_scheduler(pool.clone(), id).await?;

        Ok(probe)
    }

    /// Deactivate a probe and stop its scheduler
    pub async fn deactivate_probe(&self, pool: &PgPool, id: Uuid) -> Result<Probe, AppError> {
        let probe = sqlx::query_as::<_, Probe>(
            r#"
            UPDATE probes SET active = false WHERE id = $1 RETURNING *
            "#,
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to deactivate probe: {}", e))?;

        // Stop the scheduler
        self.stop_scheduler(id).await?;

        Ok(probe)
    }

    /// Update a probe's configuration
    pub async fn update_probe(&self, pool: &PgPool, id: Uuid, interval_seconds: Option<i32>) -> Result<Probe, AppError> {
        let probe = Self::get_probe(pool, id).await?;
        let was_active = probe.active;

        // Update the probe
        let updated_probe = sqlx::query_as::<_, Probe>(
            r#"
            UPDATE probes
            SET interval_seconds = COALESCE($2, interval_seconds)
            WHERE id = $1
            RETURNING *
            "#,
        )
        .bind(id)
        .bind(interval_seconds)
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to update probe: {}", e))?;

        // If the probe was active and interval changed, restart the scheduler
        if was_active && interval_seconds.is_some() {
            self.stop_scheduler(id).await?;
            self.start_scheduler(pool.clone(), id).await?;
        }

        Ok(updated_probe)
    }

    /// Delete a probe
    pub async fn delete_probe(&self, pool: &PgPool, id: Uuid) -> Result<(), AppError> {
        // Stop the scheduler first
        self.stop_scheduler(id).await?;

        sqlx::query(
            r#"
            DELETE FROM probes WHERE id = $1
            "#,
        )
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to delete probe: {}", e))?;

        Ok(())
    }

    /// Test a probe configuration without creating it
    pub async fn test_probe(pool: &PgPool, deployment_id: Uuid) -> Result<ProbeResult, AppError> {
        // Fetch deployment and endpoint details
        let context = sqlx::query(
            r#"
            SELECT
                d.model_name,
                d.type as model_type,
                ie.url as endpoint_url,
                ie.api_key
            FROM deployed_models d
            JOIN inference_endpoints ie ON d.hosted_on = ie.id
            WHERE d.id = $1
            "#,
        )
        .bind(deployment_id)
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to fetch deployment for test: {}", e))?;

        let model_name: String = context.try_get("model_name")
            .map_err(|e| anyhow::anyhow!("Failed to get model_name: {}", e))?;
        let model_type_str: Option<String> = context.try_get("model_type").ok();
        let endpoint_url: String = context.try_get("endpoint_url")
            .map_err(|e| anyhow::anyhow!("Failed to get endpoint_url: {}", e))?;
        let api_key: Option<String> = context.try_get("api_key").ok();

        // Parse model type - default to Chat if not specified
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
            None => crate::db::models::deployments::ModelType::Chat, // Default to Chat
        };

        let execution_context = ProbeExecutionContext {
            probe_id: Uuid::nil(), // Use nil UUID for test probes
            model_name,
            model_type,
            endpoint_url,
            api_key,
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
    pub async fn execute_probe(pool: &PgPool, id: Uuid) -> Result<ProbeResult, AppError> {
        // Note: We allow executing inactive probes manually via "Run Now"
        let _probe = Self::get_probe(pool, id).await?;

        // Fetch deployment and endpoint details
        let context = sqlx::query(
            r#"
            SELECT
                p.id as probe_id,
                d.model_name,
                d.type as model_type,
                ie.url as endpoint_url,
                ie.api_key
            FROM probes p
            JOIN deployed_models d ON p.deployment_id = d.id
            JOIN inference_endpoints ie ON d.hosted_on = ie.id
            WHERE p.id = $1
            "#,
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to fetch probe execution context: {}", e))?;

        let probe_id: Uuid = context.try_get("probe_id")
            .map_err(|e| anyhow::anyhow!("Failed to get probe_id: {}", e))?;
        let model_name: String = context.try_get("model_name")
            .map_err(|e| anyhow::anyhow!("Failed to get model_name: {}", e))?;
        let model_type_str: Option<String> = context.try_get("model_type").ok();
        let endpoint_url: String = context.try_get("endpoint_url")
            .map_err(|e| anyhow::anyhow!("Failed to get endpoint_url: {}", e))?;
        let api_key: Option<String> = context.try_get("api_key").ok();

        // Parse model type - default to Chat if not specified
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
            None => crate::db::models::deployments::ModelType::Chat, // Default to Chat
        };

        let execution_context = ProbeExecutionContext {
            probe_id,
            model_name,
            model_type,
            endpoint_url,
            api_key,
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

        let results = sql_query.fetch_all(pool).await
            .map_err(|e| anyhow::anyhow!("Failed to fetch probe results: {}", e))?;

        Ok(results)
    }

    /// Get the last N results for a probe
    pub async fn get_recent_results(
        pool: &PgPool,
        probe_id: Uuid,
        limit: i64,
    ) -> Result<Vec<ProbeResult>, AppError> {
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
        let mut query = String::from(
            r#"
            SELECT
                COUNT(*) as total,
                COUNT(*) FILTER (WHERE success = true) as successful,
                COUNT(*) FILTER (WHERE success = false) as failed,
                AVG(response_time_ms) FILTER (WHERE success = true) as avg_time,
                MIN(response_time_ms) FILTER (WHERE success = true) as min_time,
                MAX(response_time_ms) FILTER (WHERE success = true) as max_time,
                PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY response_time_ms) FILTER (WHERE success = true) as p50,
                PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY response_time_ms) FILTER (WHERE success = true) as p95,
                PERCENTILE_CONT(0.99) WITHIN GROUP (ORDER BY response_time_ms) FILTER (WHERE success = true) as p99,
                MAX(executed_at) as last_execution,
                MAX(executed_at) FILTER (WHERE success = true) as last_success,
                MAX(executed_at) FILTER (WHERE success = false) as last_failure
            FROM probe_results
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

        let mut sql_query = sqlx::query(&query).bind(probe_id);

        if let Some(start) = start_time {
            sql_query = sql_query.bind(start);
        }

        if let Some(end) = end_time {
            sql_query = sql_query.bind(end);
        }

        let row = sql_query.fetch_one(pool).await
            .map_err(|e| anyhow::anyhow!("Failed to fetch probe statistics: {}", e))?;

        let total: i64 = row.try_get("total")
            .map_err(|e| anyhow::anyhow!("Failed to get total: {}", e))?;
        let successful: i64 = row.try_get("successful")
            .map_err(|e| anyhow::anyhow!("Failed to get successful: {}", e))?;
        let failed: i64 = row.try_get("failed")
            .map_err(|e| anyhow::anyhow!("Failed to get failed: {}", e))?;
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
            avg_response_time_ms: row.try_get("avg_time").ok(),
            min_response_time_ms: row.try_get("min_time").ok(),
            max_response_time_ms: row.try_get("max_time").ok(),
            p50_response_time_ms: row.try_get("p50").ok(),
            p95_response_time_ms: row.try_get("p95").ok(),
            p99_response_time_ms: row.try_get("p99").ok(),
            last_execution: row.try_get("last_execution").ok(),
            last_success: row.try_get("last_success").ok(),
            last_failure: row.try_get("last_failure").ok(),
        })
    }
}
