//! PostgreSQL storage for the Fusillade scheduling daemon.

use std::future::Future;
use std::time::Duration;

use fusillade_core::FusilladeError;

mod db;
pub mod postgres;
#[path = "response_step.rs"]
pub mod postgres_response_step;
pub mod transform;
mod utils;

pub use fusillade_core::manager::{
    ArchiveOutcome, DaemonStorage, ModelFilter, ModelFilterState, Storage,
};
pub use fusillade_core::request::AnyRequest;
pub use fusillade_core::response_step;
pub use postgres::{BatchInsertStrategy, PoolProvider, PostgresRequestManager, TestDbPools};
pub use postgres_response_step::PostgresResponseStepManager;
pub use transform::ResponseTransformer;

pub mod batch {
    pub use fusillade_core::batch::*;
}

pub mod daemon {
    pub use crate::PostgresStorageConfig as DaemonConfig;
    pub use fusillade_core::daemon_record::*;
}

pub mod manager {
    pub use fusillade_core::manager::*;
}

pub mod error {
    pub use fusillade_core::error::*;
}

pub mod request {
    pub use fusillade_core::request::*;
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PostgresStorageConfig {
    #[serde(default = "default_pending_request_counts_timeout_ms")]
    pub pending_request_counts_timeout_ms: u64,
    /// Maximum number of request state transitions that may write to Postgres
    /// concurrently. Set to `0` to disable only the write-class limit; the
    /// shared response-traffic budget still applies.
    #[serde(default = "default_max_concurrent_state_writes")]
    pub max_concurrent_state_writes: usize,
    /// Maximum number of consolidated response detail reads that may run
    /// concurrently. Set to `0` to disable only the read-class limit; the
    /// shared response-traffic budget still applies.
    #[serde(default = "default_max_concurrent_response_reads")]
    pub max_concurrent_response_reads: usize,
    #[serde(default = "default_batch_metadata_fields")]
    pub batch_metadata_fields: Vec<String>,
    pub claim_timeout_ms: u64,
    pub processing_timeout_ms: u64,
    pub stale_daemon_threshold_ms: u64,
    pub unclaim_batch_size: usize,
    #[serde(default = "default_service_tier_completion_windows_ms")]
    pub service_tier_completion_windows_ms: std::collections::HashMap<String, u64>,
    #[serde(default = "default_completion_window_ms")]
    pub default_completion_window_ms: u64,
    #[serde(default = "default_claim_ramp_exponent")]
    pub claim_ramp_exponent: f64,
    #[serde(default)]
    pub urgency_weight: f64,
    #[serde(default)]
    pub batch_claim_require_live: bool,
    #[serde(default = "default_leaks_per_window")]
    pub leaks_per_window: f64,
    #[serde(default = "default_model_filters_keep_per_model")]
    pub model_filters_keep_per_model: i64,
    #[serde(default = "default_model_filters_retention_ms")]
    pub model_filters_retention_ms: u64,
}

fn default_batch_metadata_fields() -> Vec<String> {
    vec![
        "id".to_string(),
        "endpoint".to_string(),
        "created_at".to_string(),
        "completion_window".to_string(),
    ]
}

fn default_service_tier_completion_windows_ms() -> std::collections::HashMap<String, u64> {
    std::collections::HashMap::from([("flex".to_string(), 3_600_000)])
}

fn default_completion_window_ms() -> u64 {
    86_400_000
}

fn default_pending_request_counts_timeout_ms() -> u64 {
    60_000
}

fn default_max_concurrent_state_writes() -> usize {
    64
}

fn default_max_concurrent_response_reads() -> usize {
    8
}

fn default_claim_ramp_exponent() -> f64 {
    0.56
}

fn default_leaks_per_window() -> f64 {
    60.0
}

fn default_model_filters_keep_per_model() -> i64 {
    50
}

fn default_model_filters_retention_ms() -> u64 {
    604_800_000
}

impl Default for PostgresStorageConfig {
    fn default() -> Self {
        Self {
            pending_request_counts_timeout_ms: default_pending_request_counts_timeout_ms(),
            max_concurrent_state_writes: default_max_concurrent_state_writes(),
            max_concurrent_response_reads: default_max_concurrent_response_reads(),
            batch_metadata_fields: default_batch_metadata_fields(),
            claim_timeout_ms: 60_000,
            processing_timeout_ms: 600_000,
            stale_daemon_threshold_ms: 30_000,
            unclaim_batch_size: 100,
            service_tier_completion_windows_ms: default_service_tier_completion_windows_ms(),
            default_completion_window_ms: default_completion_window_ms(),
            claim_ramp_exponent: default_claim_ramp_exponent(),
            urgency_weight: 0.0,
            batch_claim_require_live: false,
            leaks_per_window: default_leaks_per_window(),
            model_filters_keep_per_model: default_model_filters_keep_per_model(),
            model_filters_retention_ms: default_model_filters_retention_ms(),
        }
    }
}

/// Retry cadence for transient database failures.
///
/// Each entry is the delay before the next retry. An empty cadence disables
/// retries and preserves the first error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbRetryConfig {
    retry_delays: Vec<Duration>,
}

impl DbRetryConfig {
    pub fn new(retry_delays: Vec<Duration>) -> Self {
        Self { retry_delays }
    }

    pub fn fixed(retries: usize, delay: Duration) -> Self {
        Self {
            retry_delays: vec![delay; retries],
        }
    }

    pub fn disabled() -> Self {
        Self::new(Vec::new())
    }

    pub fn retry_delays(&self) -> &[Duration] {
        &self.retry_delays
    }
}

impl Default for DbRetryConfig {
    fn default() -> Self {
        Self::fixed(3, Duration::from_millis(50))
    }
}

pub async fn retry_transient_db_errors<T, Op, Fut>(
    config: &DbRetryConfig,
    mut operation: Op,
) -> fusillade_core::Result<T>
where
    Op: FnMut() -> Fut,
    Fut: Future<Output = fusillade_core::Result<T>>,
{
    for delay in config.retry_delays() {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) if is_retryable_db_error(&error) => {
                if !delay.is_zero() {
                    tokio::time::sleep(*delay).await;
                }
            }
            Err(error) => return Err(error),
        }
    }

    operation().await
}

pub fn is_retryable_db_error(error: &FusilladeError) -> bool {
    is_retryable_db_error_message(&error.to_string())
}

pub fn is_retryable_db_error_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("pool timed out while waiting for an open connection")
        || message.contains("pooltimedout")
        || message.contains("connection pool timed out")
}

/// Fusillade Arsenal database migrator.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Get the Fusillade Arsenal database migrator.
///
/// Returns a migrator that can be run against a PostgreSQL pool.
pub fn migrator() -> &'static sqlx::migrate::Migrator {
    &MIGRATOR
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_write_concurrency_defaults_when_missing() {
        let mut serialized = serde_json::to_value(PostgresStorageConfig::default()).unwrap();
        serialized
            .as_object_mut()
            .unwrap()
            .remove("max_concurrent_state_writes");

        let decoded: PostgresStorageConfig = serde_json::from_value(serialized).unwrap();
        let reencoded = serde_json::to_value(decoded).unwrap();

        assert_eq!(reencoded["max_concurrent_state_writes"], 64);
    }

    #[test]
    fn state_write_concurrency_explicit_value_round_trips() {
        let mut serialized = serde_json::to_value(PostgresStorageConfig::default()).unwrap();
        serialized["max_concurrent_state_writes"] = serde_json::json!(17);

        let decoded: PostgresStorageConfig = serde_json::from_value(serialized).unwrap();
        let reencoded = serde_json::to_value(decoded).unwrap();

        assert_eq!(reencoded["max_concurrent_state_writes"], 17);
    }

    #[test]
    fn response_read_concurrency_defaults_when_missing() {
        let mut serialized = serde_json::to_value(PostgresStorageConfig::default()).unwrap();
        serialized
            .as_object_mut()
            .unwrap()
            .remove("max_concurrent_response_reads");

        let decoded: PostgresStorageConfig = serde_json::from_value(serialized).unwrap();
        assert_eq!(decoded.max_concurrent_response_reads, 8);
    }

    #[test]
    fn response_read_concurrency_explicit_value_round_trips() {
        let mut serialized = serde_json::to_value(PostgresStorageConfig::default()).unwrap();
        serialized["max_concurrent_response_reads"] = serde_json::json!(3);

        let decoded: PostgresStorageConfig = serde_json::from_value(serialized).unwrap();
        assert_eq!(decoded.max_concurrent_response_reads, 3);
    }
}
