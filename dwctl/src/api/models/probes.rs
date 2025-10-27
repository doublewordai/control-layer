use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// Request payload for creating a new probe.
///
/// Created probes are automatically activated and start executing on their
/// configured interval.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateProbe {
    /// Human-readable name for the probe
    pub name: String,
    /// Reference to the deployment (model) to monitor
    #[schema(value_type = String, format = "uuid")]
    pub deployment_id: Uuid,
    /// How often to execute the probe, in seconds
    pub interval_seconds: i32,
}

/// Query parameters for filtering probes
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct ProbesQuery {
    /// Filter by probe status: "active" to show only active probes, omit to show all
    pub status: Option<String>,
}

/// Query parameters for filtering probe results
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct ResultsQuery {
    /// Start time for filtering results
    #[param(value_type = Option<String>, format = "date-time")]
    #[schema(value_type = Option<String>, format = "date-time")]
    pub start_time: Option<DateTime<Utc>>,
    /// End time for filtering results
    #[param(value_type = Option<String>, format = "date-time")]
    #[schema(value_type = Option<String>, format = "date-time")]
    pub end_time: Option<DateTime<Utc>>,
    /// Maximum number of results to return
    pub limit: Option<i64>,
}

/// Query parameters for probe statistics
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct StatsQuery {
    /// Start time for statistics calculation
    #[param(value_type = Option<String>, format = "date-time")]
    #[schema(value_type = Option<String>, format = "date-time")]
    pub start_time: Option<DateTime<Utc>>,
    /// End time for statistics calculation
    #[param(value_type = Option<String>, format = "date-time")]
    #[schema(value_type = Option<String>, format = "date-time")]
    pub end_time: Option<DateTime<Utc>>,
}

/// Request payload for updating a probe
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateProbeRequest {
    /// Update probe execution interval in seconds
    pub interval_seconds: Option<i32>,
}

/// Aggregated statistics for a probe over a time period.
///
/// Statistics are calculated from stored probe results and include
/// percentile metrics for response times.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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
    #[schema(value_type = Option<String>, format = "date-time")]
    pub last_execution: Option<DateTime<Utc>>,
    /// Timestamp of the most recent successful execution
    #[schema(value_type = Option<String>, format = "date-time")]
    pub last_success: Option<DateTime<Utc>>,
    /// Timestamp of the most recent failed execution
    #[schema(value_type = Option<String>, format = "date-time")]
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
