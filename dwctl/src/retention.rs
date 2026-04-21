use crate::{Config, config::RetentionConfig};

/// Internal batch metadata key storing artifact retention TTL in whole seconds.
pub(crate) const RETENTION_TTL_METADATA_KEY: &str = fusillade::RETENTION_TTL_METADATA_KEY;

/// Returns the configured default batch artifact TTL in seconds, if any.
pub(crate) fn default_batch_artifact_ttl_seconds(config: &Config) -> Option<i64> {
    config.background_services.retention.batch_artifacts_default_ttl_seconds()
}

/// Convert control-layer retention config into Fusillade daemon retention settings.
pub(crate) fn apply_to_fusillade_config(retention: &RetentionConfig, daemon_config: &mut fusillade::daemon::DaemonConfig) {
    daemon_config.retention_interval_ms = if retention.enabled {
        retention.sweep_interval.as_millis().min(u64::MAX as u128) as u64
    } else {
        0
    };
    daemon_config.retention_batch_size = retention.batch_size.max(1);
}
