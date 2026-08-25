use std::collections::HashMap;
use std::sync::Arc;

use fusillade::daemon::default_should_retry;
use fusillade::{DaemonConfig, DaemonMode};

// This integration target is a downstream crate. Keep the exhaustive literal
// shape accepted on origin/main so additive maintenance controls cannot become
// a source-breaking public struct-field addition.
#[test]
fn origin_main_daemon_config_literal_still_compiles() {
    let config = DaemonConfig {
        mode: DaemonMode::Both,
        claim_batch_size: 100,
        model_concurrency_limits: Arc::new(dashmap::DashMap::new()),
        model_escalations: Arc::new(dashmap::DashMap::new()),
        inject_deadline_priority: false,
        background_concurrency_limit: 0,
        claim_interval_ms: 1_000,
        batch_claim_size: 0,
        batch_claim_batch_size: 4,
        batch_claim_require_live: false,
        batch_claim_interval_ms: 0,
        claim_loop_max_consecutive_failures: 10,
        claim_query_timeout_ms: 180_000,
        max_concurrent_state_writes: 64,
        max_retries: Some(1_000),
        stop_before_deadline_ms: Some(0),
        backoff_ms: 1_000,
        backoff_factor: 2,
        max_backoff_ms: 10_000,
        upload_stall_timeout_ms: 60_000,
        upload_chunk_bytes: 64 * 1_024,
        upload_stall_poll_ms: 100,
        first_chunk_timeout_ms: 540_000,
        chunk_timeout_ms: 540_000,
        body_timeout_ms: 60_000,
        status_log_interval_ms: Some(2_000),
        heartbeat_interval_ms: 5_000,
        adaptive_concurrency: false,
        adaptive_growth_factor: 1.05,
        adaptive_cut_factor: 0.5,
        memory_gate_high_fraction: 0.85,
        memory_gate_low_fraction: 0.75,
        memory_gate_release_in_flight_fraction: 0.25,
        should_retry: Arc::new(default_should_retry),
        additional_retryable_statuses: vec![499],
        claim_timeout_ms: 60_000,
        processing_timeout_ms: 600_000,
        pending_request_counts_timeout_ms: 60_000,
        stale_daemon_threshold_ms: 30_000,
        unclaim_batch_size: 100,
        cancellation_poll_interval_ms: 5_000,
        batch_metadata_fields: vec!["id".to_owned()],
        purge_interval_ms: 600_000,
        purge_batch_size: 1_000,
        purge_throttle_ms: 100,
        batch_archive_sweep_enabled: false,
        batch_archive_sweep_interval_ms: 5_000,
        batch_archive_sweep_moves_per_tick: 4,
        batch_archive_sweep_dwell_secs: 0.0,
        batch_archive_cancel_grace_secs: 600.0,
        batch_archive_backfill_enabled: false,
        batch_archive_backfill_interval_ms: 1_000,
        batch_archive_backfill_moves_per_tick: 4,
        batch_archive_backfill_concurrency: 1,
        batch_archive_partitions_weeks_ahead: 4,
        batch_finalizer_enabled: true,
        batch_finalizer_interval_ms: 10_000,
        batch_finalizer_cancelled_grace_secs: 3_600.0,
        batch_finalizer_cancelled_per_tick: 50,
        throughput_log_interval_ms: Some(60_000),
        streamable_endpoints: Vec::new(),
        urgency_weight: 0.0,
        service_tier_completion_windows_ms: HashMap::from([("flex".to_owned(), 3_600_000)]),
        default_completion_window_ms: 86_400_000,
        claim_ramp_exponent: 0.56,
        leaks_per_window: 60.0,
        model_filters_keep_per_model: 50,
        model_filters_retention_ms: 604_800_000,
    };

    assert_eq!(config.claim_batch_size, 100);
}
