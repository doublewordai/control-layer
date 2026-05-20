//! Configuration for the image normaliser.
//!
//! Loaded as a section of the dwctl `Config` via Figment (YAML + env
//! overrides with `DWCTL_IMAGE_NORMALIZER__*`).
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Top-level image-normaliser configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageNormalizerConfig {
    /// Master switch. When false, the normaliser is replaced by a no-op
    /// at startup and the middleware passes all traffic through unchanged.
    #[serde(default)]
    pub enabled: bool,

    /// Object store backend. Choose `memory` for local development and
    /// integration tests (bytes held in-process only); `gcs` for production.
    #[serde(default)]
    pub backend: BackendConfig,

    /// Hardened-fetcher policy.
    #[serde(default)]
    pub fetcher: FetcherConfig,

    /// Signed-URL TTL policy.
    #[serde(default)]
    pub signing: SigningConfig,
}

/// Object-store backend selection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BackendConfig {
    /// In-process bytes only. For tests and local development. Bytes are
    /// lost on restart; signed URLs are served by the dwctl process itself
    /// via the dashboard image endpoint.
    #[default]
    Memory,
    /// Google Cloud Storage. Requires Workload Identity binding so the
    /// service account can write to the bucket and call `signBlob` for V4
    /// URL signing.
    Gcs {
        bucket: String,
        #[serde(default = "default_gcs_region")]
        region: String,
    },
}

fn default_gcs_region() -> String {
    "europe-west4".to_string()
}

/// Hardened-fetcher policy. All durations expressed in seconds in YAML;
/// `Duration` at runtime via the with-helper.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetcherConfig {
    /// Maximum response body size we will read (bytes). Larger payloads
    /// are aborted with `BadInput`.
    pub max_bytes: u64,

    /// Total per-attempt timeout, in seconds.
    pub timeout_secs: u64,

    /// Maximum number of HTTP 3xx redirects to follow. Each hop is
    /// re-validated against the IP deny-list independently.
    pub max_redirects: u8,

    /// Maximum number of retry attempts on transient errors (timeouts,
    /// connection refused, DNS failure, origin 5xx). Permanent errors
    /// (4xx, MIME mismatch, oversize, allow-list rejection) are never
    /// retried.
    pub max_retries: u8,

    /// Base delay between retries, in milliseconds. Doubled on each attempt
    /// (250ms / 500ms / 1000ms / ...) plus a small random jitter.
    pub retry_base_delay_ms: u64,

    /// MIME types accepted from the upstream `Content-Type`. Lowercase.
    pub allowed_mime: Vec<String>,
}

impl Default for FetcherConfig {
    fn default() -> Self {
        Self {
            max_bytes: 20 * 1024 * 1024,
            timeout_secs: 30,
            max_redirects: 3,
            max_retries: 3,
            retry_base_delay_ms: 250,
            allowed_mime: vec![
                "image/png".to_string(),
                "image/jpeg".to_string(),
                "image/webp".to_string(),
                "image/gif".to_string(),
            ],
        }
    }
}

impl FetcherConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }
    pub fn retry_base_delay(&self) -> Duration {
        Duration::from_millis(self.retry_base_delay_ms)
    }
    pub fn mime_allowed(&self, mime: &str) -> bool {
        let mime = mime.to_ascii_lowercase();
        self.allowed_mime.iter().any(|m| m.eq_ignore_ascii_case(&mime))
    }
}

/// Signed-URL TTL policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SigningConfig {
    /// TTL applied to signed URLs handed to upstream providers from the
    /// realtime path. Covers a single chat completion.
    pub realtime_ttl_secs: u64,
    /// TTL applied to signed URLs handed to upstream providers from the
    /// batch dispatcher. Covers a single dispatch attempt; retries get
    /// fresh URLs.
    pub dispatch_ttl_secs: u64,
    /// TTL applied to signed URLs served via the dashboard image-view
    /// endpoint. Short, since the dashboard re-signs on each page load.
    pub dashboard_ttl_secs: u64,
}

impl Default for SigningConfig {
    fn default() -> Self {
        Self {
            realtime_ttl_secs: 900,    // 15 min
            dispatch_ttl_secs: 1800,   // 30 min — refreshed per dispatch
            dashboard_ttl_secs: 300,   // 5 min
        }
    }
}

impl SigningConfig {
    pub fn realtime_ttl(&self) -> Duration {
        Duration::from_secs(self.realtime_ttl_secs)
    }
    pub fn dispatch_ttl(&self) -> Duration {
        Duration::from_secs(self.dispatch_ttl_secs)
    }
    pub fn dashboard_ttl(&self) -> Duration {
        Duration::from_secs(self.dashboard_ttl_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_disabled_with_memory_backend() {
        let c = ImageNormalizerConfig::default();
        assert!(!c.enabled);
        assert!(matches!(c.backend, BackendConfig::Memory));
    }

    #[test]
    fn fetcher_default_mime_includes_common_image_types() {
        let f = FetcherConfig::default();
        assert!(f.mime_allowed("image/png"));
        assert!(f.mime_allowed("IMAGE/JPEG"));
        assert!(!f.mime_allowed("application/json"));
        assert!(!f.mime_allowed("text/html"));
    }

    #[test]
    fn json_round_trip_with_gcs_backend() {
        // serde_json round-trip is enough to validate the field shapes and
        // the tagged-enum representation. Full YAML loading is exercised by
        // dwctl's broader Figment-based config tests.
        let cfg = ImageNormalizerConfig {
            enabled: true,
            backend: BackendConfig::Gcs {
                bucket: "my-bucket".to_string(),
                region: "europe-west4".to_string(),
            },
            fetcher: FetcherConfig {
                max_bytes: 1_048_576,
                timeout_secs: 10,
                max_redirects: 1,
                max_retries: 0,
                retry_base_delay_ms: 100,
                allowed_mime: vec!["image/png".to_string()],
            },
            signing: SigningConfig {
                realtime_ttl_secs: 60,
                dispatch_ttl_secs: 120,
                dashboard_ttl_secs: 30,
            },
        };
        let json = serde_json::to_string(&cfg).expect("serialize");
        let back: ImageNormalizerConfig = serde_json::from_str(&json).expect("deserialize");
        assert!(back.enabled);
        match back.backend {
            BackendConfig::Gcs { ref bucket, .. } => assert_eq!(bucket, "my-bucket"),
            _ => panic!("expected gcs backend"),
        }
        assert_eq!(back.fetcher.max_bytes, 1_048_576);
        assert!(back.fetcher.mime_allowed("image/png"));
        assert!(!back.fetcher.mime_allowed("image/jpeg"));
        assert_eq!(back.signing.realtime_ttl().as_secs(), 60);
    }
}
