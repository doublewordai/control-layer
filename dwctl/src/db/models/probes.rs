use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;

/// A probe configuration that monitors a deployed model.
///
/// Probes reference a deployment which contains the model name, model type,
/// and the endpoint it's hosted on. All configuration is derived from the
/// deployment.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow, ToSchema)]
pub struct Probe {
    /// Unique identifier for the probe
    #[schema(value_type = String, format = "uuid")]
    pub id: Uuid,
    /// Human-readable name for the probe
    pub name: String,
    /// Reference to the deployment (model) being monitored
    #[schema(value_type = String, format = "uuid")]
    pub deployment_id: Uuid,
    /// How often to execute the probe, in seconds
    pub interval_seconds: i32,
    /// Whether the probe is currently active and should be scheduled
    pub active: bool,
    /// HTTP method to use for the probe request (e.g., GET, POST)
    pub http_method: String,
    /// Path to append to the endpoint URL (e.g., /v1/chat/completions)
    pub request_path: Option<String>,
    /// JSON body to send with the probe request
    pub request_body: Option<serde_json::Value>,
    /// When the probe was created
    #[schema(value_type = String, format = "date-time")]
    pub created_at: DateTime<Utc>,
    /// When the probe was last updated
    #[schema(value_type = String, format = "date-time")]
    pub updated_at: DateTime<Utc>,
}

/// A stored result from executing a probe.
///
/// Results are persisted to the database and used to calculate statistics
/// and visualize probe history.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow, ToSchema)]
pub struct ProbeResult {
    /// Unique identifier for this result
    #[schema(value_type = String, format = "uuid")]
    pub id: Uuid,
    /// ID of the probe that was executed
    #[schema(value_type = String, format = "uuid")]
    pub probe_id: Uuid,
    /// When the probe was executed
    #[schema(value_type = String, format = "date-time")]
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
