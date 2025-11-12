//! Models for the Daemons API.
//!
//! This exposes daemon status information for monitoring.

use serde::{Deserialize, Serialize};

/// Status of a daemon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DaemonStatus {
    Initializing,
    Running,
    Dead,
}

/// Statistics tracked for a daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStats {
    /// Total number of requests processed
    pub requests_processed: u64,
    /// Total number of requests that failed
    pub requests_failed: u64,
    /// Current number of requests being processed
    pub requests_in_flight: usize,
}

/// Response model for a daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonResponse {
    /// Unique ID of the daemon
    pub id: String,
    /// Current status
    pub status: DaemonStatus,
    /// Hostname where daemon is running
    pub hostname: String,
    /// Process ID
    pub pid: i32,
    /// Version string
    pub version: String,
    /// When the daemon started
    pub started_at: i64,
    /// Last heartbeat timestamp (for running daemons)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_heartbeat: Option<i64>,
    /// When the daemon stopped (for dead daemons)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<i64>,
    /// Statistics
    pub stats: DaemonStats,
    /// Configuration snapshot (includes heartbeat_interval_ms and other config)
    pub config: serde_json::Value,
}

/// Query parameters for listing daemons.
#[derive(Debug, Deserialize)]
pub struct ListDaemonsQuery {
    /// Filter by status
    #[serde(default)]
    pub status: Option<DaemonStatus>,
}

/// Response for listing daemons.
#[derive(Debug, Serialize)]
pub struct ListDaemonsResponse {
    pub daemons: Vec<DaemonResponse>,
}
