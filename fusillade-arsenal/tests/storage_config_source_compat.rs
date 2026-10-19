use std::collections::HashMap;

use fusillade_arsenal::PostgresStorageConfig;

// This integration target is a downstream crate. Keep the exhaustive literal
// shape accepted on origin/main so adding private retention controls cannot
// become a source-breaking public struct-field addition.
#[test]
fn origin_main_storage_config_literal_still_compiles() {
    let config = PostgresStorageConfig {
        pending_request_counts_timeout_ms: 60_000,
        max_concurrent_state_writes: 64,
        batch_metadata_fields: vec!["id".to_owned()],
        claim_timeout_ms: 60_000,
        processing_timeout_ms: 600_000,
        stale_daemon_threshold_ms: 30_000,
        unclaim_batch_size: 100,
        service_tier_completion_windows_ms: HashMap::new(),
        default_completion_window_ms: 86_400_000,
        claim_ramp_exponent: 0.56,
        urgency_weight: 0.0,
        batch_claim_require_live: false,
        background_concurrency_limit: 0,
        leaks_per_window: 60.0,
        model_filters_keep_per_model: 50,
        model_filters_retention_ms: 604_800_000,
    };

    assert_eq!(config.claim_timeout_ms, 60_000);
}
