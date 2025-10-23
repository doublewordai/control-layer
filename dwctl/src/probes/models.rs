//! Data models for the probes monitoring system.
//!
//! This module contains all the core data structures used for monitoring
//! API endpoints, including probes, probe results, and statistics.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

/// A probe configuration that monitors a deployed model.
///
/// Probes reference a deployment which contains the model name, model type,
/// and the endpoint it's hosted on. All configuration is derived from the
/// deployment.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Probe {
    /// Unique identifier for the probe
    pub id: Uuid,
    /// Human-readable name for the probe
    pub name: String,
    /// Reference to the deployment (model) being monitored
    pub deployment_id: Uuid,
    /// How often to execute the probe, in seconds
    pub interval_seconds: i32,
    /// Whether the probe is currently active and should be scheduled
    pub active: bool,
    /// When the probe was created
    pub created_at: DateTime<Utc>,
    /// When the probe was last updated
    pub updated_at: DateTime<Utc>,
}

/// Request payload for creating a new probe.
///
/// Created probes are automatically activated and start executing on their
/// configured interval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateProbe {
    /// Human-readable name for the probe
    pub name: String,
    /// Reference to the deployment (model) to monitor
    pub deployment_id: Uuid,
    /// How often to execute the probe, in seconds
    pub interval_seconds: i32,
}

/// A stored result from executing a probe.
///
/// Results are persisted to the database and used to calculate statistics
/// and visualize probe history.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ProbeResult {
    /// Unique identifier for this result
    pub id: Uuid,
    /// ID of the probe that was executed
    pub probe_id: Uuid,
    /// When the probe was executed
    pub executed_at: DateTime<Utc>,
    /// Whether the probe execution succeeded
    pub success: bool,
    /// Response time in milliseconds (if successful)
    pub response_time_ms: Option<i32>,
    /// HTTP status code (if received)
    pub status_code: Option<i32>,
    /// Error message (if failed)
    pub error_message: Option<String>,
    /// Full API response data as JSON
    pub response_data: Option<serde_json::Value>,
    /// Additional metadata about the execution
    pub metadata: Option<serde_json::Value>,
}

/// In-memory representation of a probe execution before it's stored.
///
/// This is the result of running a probe, which gets converted to a
/// `ProbeResult` when persisted to the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeExecution {
    /// ID of the probe that was executed
    pub probe_id: Uuid,
    /// Whether the probe execution succeeded
    pub success: bool,
    /// Response time in milliseconds
    pub response_time_ms: i32,
    /// HTTP status code (if received)
    pub status_code: Option<i32>,
    /// Error message (if failed)
    pub error_message: Option<String>,
    /// Full API response data as JSON
    pub response_data: Option<serde_json::Value>,
    /// Additional metadata about the execution
    pub metadata: Option<serde_json::Value>,
}

/// Aggregated statistics for a probe over a time period.
///
/// Statistics are calculated from stored probe results and include
/// percentile metrics for response times.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeStatistics {
    /// Total number of probe executions
    pub total_executions: i64,
    /// Number of successful executions
    pub successful_executions: i64,
    /// Number of failed executions
    pub failed_executions: i64,
    /// Success rate as a percentage (0-100)
    pub success_rate: f64,
    /// Average response time in milliseconds (successful probes only)
    pub avg_response_time_ms: Option<f64>,
    /// Minimum response time in milliseconds
    pub min_response_time_ms: Option<i32>,
    /// Maximum response time in milliseconds
    pub max_response_time_ms: Option<i32>,
    /// 50th percentile (median) response time
    pub p50_response_time_ms: Option<f64>,
    /// 95th percentile response time
    pub p95_response_time_ms: Option<f64>,
    /// 99th percentile response time
    pub p99_response_time_ms: Option<f64>,
    /// Timestamp of the most recent execution
    pub last_execution: Option<DateTime<Utc>>,
    /// Timestamp of the most recent successful execution
    pub last_success: Option<DateTime<Utc>>,
    /// Timestamp of the most recent failed execution
    pub last_failure: Option<DateTime<Utc>>,
}

impl Default for ProbeStatistics {
    fn default() -> Self {
        Self {
            total_executions: 0,
            successful_executions: 0,
            failed_executions: 0,
            success_rate: 0.0,
            avg_response_time_ms: None,
            min_response_time_ms: None,
            max_response_time_ms: None,
            p50_response_time_ms: None,
            p95_response_time_ms: None,
            p99_response_time_ms: None,
            last_execution: None,
            last_success: None,
            last_failure: None,
        }
    }
}
