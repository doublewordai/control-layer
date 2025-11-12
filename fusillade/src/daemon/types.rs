//! Daemon state types using the typestate pattern.
//!
//! This module defines type-safe daemon lifecycle management. Each daemon
//! progresses through distinct states, enforced at compile time.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::request::DaemonId;

/// Marker trait for valid daemon states.
pub trait DaemonState: Send + Sync {}

/// A daemon instance in the fusillade system.
///
/// Uses the typestate pattern to ensure type-safe state transitions.
/// The generic parameter `T` represents the current state of the daemon.
#[derive(Debug, Clone, Serialize)]
pub struct DaemonRecord<T: DaemonState> {
    /// The current state of the daemon.
    pub state: T,
    /// The daemon metadata.
    pub data: DaemonData,
}

/// Immutable daemon metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DaemonData {
    /// Unique identifier for this daemon instance.
    pub id: DaemonId,
    /// Hostname where the daemon is running.
    pub hostname: String,
    /// Process ID of the daemon.
    pub pid: i32,
    /// Version string (e.g., git commit hash or semver).
    pub version: String,
    /// Snapshot of daemon configuration (for audit trail).
    pub config_snapshot: serde_json::Value,
}

// ============================================================================
// Daemon States
// ============================================================================

/// Daemon is initializing (registered but not yet started processing).
#[derive(Debug, Clone, Serialize)]
pub struct Initializing {
    pub started_at: DateTime<Utc>,
}

impl DaemonState for Initializing {}

/// Daemon is actively running and processing requests.
#[derive(Debug, Clone, Serialize)]
pub struct Running {
    pub started_at: DateTime<Utc>,
    pub last_heartbeat: DateTime<Utc>,
    pub stats: DaemonStats,
}

impl DaemonState for Running {}

/// Daemon has shut down (terminal state).
#[derive(Debug, Clone, Serialize)]
pub struct Dead {
    pub started_at: DateTime<Utc>,
    pub stopped_at: DateTime<Utc>,
    pub final_stats: DaemonStats,
}

impl DaemonState for Dead {}

/// Statistics tracked for each daemon.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DaemonStats {
    /// Total number of requests successfully processed.
    pub requests_processed: u64,
    /// Total number of requests that failed.
    pub requests_failed: u64,
    /// Current number of requests being processed.
    pub requests_in_flight: usize,
}

// ============================================================================
// Unified Daemon Representation
// ============================================================================

/// Enum that can hold a daemon in any state.
///
/// This is used for storage and API responses where we need to handle
/// daemons uniformly regardless of their current state.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", content = "daemon")]
pub enum AnyDaemonRecord {
    Initializing(DaemonRecord<Initializing>),
    Running(DaemonRecord<Running>),
    Dead(DaemonRecord<Dead>),
}

impl AnyDaemonRecord {
    /// Get the daemon ID regardless of state.
    pub fn id(&self) -> DaemonId {
        match self {
            AnyDaemonRecord::Initializing(d) => d.data.id,
            AnyDaemonRecord::Running(d) => d.data.id,
            AnyDaemonRecord::Dead(d) => d.data.id,
        }
    }

    /// Check if this daemon is in a terminal state (Dead).
    pub fn is_terminal(&self) -> bool {
        matches!(self, AnyDaemonRecord::Dead(_))
    }

    /// Get the daemon status enum.
    pub fn status(&self) -> DaemonStatus {
        match self {
            AnyDaemonRecord::Initializing(_) => DaemonStatus::Initializing,
            AnyDaemonRecord::Running(_) => DaemonStatus::Running,
            AnyDaemonRecord::Dead(_) => DaemonStatus::Dead,
        }
    }
}

/// Daemon status enum for filtering queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonStatus {
    Initializing,
    Running,
    Dead,
}

impl DaemonStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            DaemonStatus::Initializing => "initializing",
            DaemonStatus::Running => "running",
            DaemonStatus::Dead => "dead",
        }
    }
}

impl std::str::FromStr for DaemonStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "initializing" => Ok(DaemonStatus::Initializing),
            "running" => Ok(DaemonStatus::Running),
            "dead" => Ok(DaemonStatus::Dead),
            _ => Err(format!("Invalid daemon status: {}", s)),
        }
    }
}

// Conversion traits for going from typed DaemonRecord to AnyDaemonRecord

impl From<DaemonRecord<Initializing>> for AnyDaemonRecord {
    fn from(d: DaemonRecord<Initializing>) -> Self {
        AnyDaemonRecord::Initializing(d)
    }
}

impl From<DaemonRecord<Running>> for AnyDaemonRecord {
    fn from(d: DaemonRecord<Running>) -> Self {
        AnyDaemonRecord::Running(d)
    }
}

impl From<DaemonRecord<Dead>> for AnyDaemonRecord {
    fn from(d: DaemonRecord<Dead>) -> Self {
        AnyDaemonRecord::Dead(d)
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Get the current hostname.
pub fn get_hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Get the current process ID.
pub fn get_pid() -> i32 {
    std::process::id() as i32
}

/// Get a version string (currently returns "dev", can be replaced with git hash or semver).
pub fn get_version() -> String {
    option_env!("GIT_HASH")
        .or(option_env!("CARGO_PKG_VERSION"))
        .unwrap_or("dev")
        .to_string()
}
