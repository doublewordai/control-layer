//! Configuration for the image normaliser.
//!
//! Loaded as a section of the dwctl `Config` via Figment (YAML + env
//! overrides with `DWCTL_IMAGE_NORMALIZER__*`).
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Top-level image-normaliser configuration.
///
/// Default = disabled with no backend; safe for builds and tests where
/// the feature is off. When `enabled` is set to true, `backend` MUST be
/// configured — see `from_config` for the explicit error.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageNormalizerConfig {
    /// Master switch. When false, the normaliser is replaced by a no-op
    /// at startup and the middleware passes all traffic through unchanged.
    #[serde(default)]
    pub enabled: bool,

    /// Object store backend. Choose `memory` for local development and
    /// integration tests (bytes held in-process only); `gcs` for production.
    /// Required when `enabled` is true; there is intentionally no default
    /// so a misconfigured prod deployment can't silently fall back to
    /// in-process memory (which loses bytes on restart and across replicas).
    pub backend: Option<BackendConfig>,

    /// Hardened-fetcher policy.
    #[serde(default)]
    pub fetcher: FetcherConfig,

    /// Signed-URL TTL policy.
    #[serde(default)]
    pub signing: SigningConfig,
}

/// Object-store backend selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BackendConfig {
    /// In-process bytes only. For tests and local development. Bytes are
    /// lost on restart; signed URLs are served by the dwctl process itself
    /// via the dashboard image endpoint. Intentionally NOT the default —
    /// operators must opt in explicitly (and never use this in production).
    Memory,
    /// Google Cloud Storage. Requires Workload Identity binding so the
    /// service account can write to the bucket and call `signBlob` for V4
    /// URL signing.
    Gcs {
        bucket: String,
        #[serde(default = "default_gcs_region")]
        region: String,
    },
    /// Any S3-compatible object store reached via a custom endpoint —
    /// Cloudflare R2, MinIO, Backblaze B2, or AWS S3 itself. Uses static
    /// access-key credentials and local SigV4 presigning (no signBlob /
    /// Workload Identity needed).
    ///
    /// Credentials are intentionally NOT part of this (serializable)
    /// config — they are read from the environment at startup so they
    /// can't leak via a config dump (note: unprefixed, NOT `DWCTL_`, so
    /// the config loader leaves them alone):
    ///   - `IMAGE_NORMALIZER_S3_ACCESS_KEY_ID`
    ///   - `IMAGE_NORMALIZER_S3_SECRET_ACCESS_KEY`
    ///
    /// Example (Cloudflare R2):
    ///   type: s3_compatible
    ///   bucket: doubleword-images
    ///   endpoint_url: https://<account_id>.r2.cloudflarestorage.com
    ///   region: auto
    S3Compatible {
        bucket: String,
        /// Full endpoint URL of the S3-compatible service.
        endpoint_url: String,
        /// Region label. R2 uses `auto`; AWS S3 uses e.g. `us-east-1`.
        #[serde(default = "default_s3_region")]
        region: String,
        /// Use path-style addressing (`endpoint/bucket/key`) rather than
        /// virtual-hosted (`bucket.endpoint/key`). Required by most
        /// S3-compatible endpoints (R2, MinIO); defaults to true.
        #[serde(default = "default_true")]
        force_path_style: bool,
    },
}

fn default_gcs_region() -> String {
    "europe-west4".to_string()
}

fn default_s3_region() -> String {
    // R2 ignores region but the SDK requires one; "auto" is R2's convention.
    "auto".to_string()
}

fn default_true() -> bool {
    true
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
    /// Optional override for the TTL applied to signed URLs handed to
    /// upstream providers from the batch dispatcher.
    ///
    /// When `None` (default), the TTL is **derived from the batch daemon's
    /// `processing_timeout_ms`** at startup: `processing_timeout +
    /// dispatch_ttl_headroom_secs` (default headroom: 5 min). This ensures
    /// the URL is always valid for at least one full dispatch attempt, plus
    /// a margin for clock skew / processing pauses — the URL itself can
    /// never be the cause of a dispatch failure.
    ///
    /// Set explicitly to a `Some(secs)` only if you have a specific
    /// reason to deviate from the derived value (e.g., very long-running
    /// upstream calls with a much shorter URL leak window requirement).
    pub dispatch_ttl_secs: Option<u64>,
    /// Headroom added on top of `processing_timeout_ms` when deriving the
    /// dispatch TTL. Ignored if `dispatch_ttl_secs` is set.
    ///
    /// 5 minutes is sufficient regardless of how long a batch runs because
    /// the URL is **re-signed on every dispatch attempt** (the JIT-signing
    /// step in `DwctlRequestProcessor::process` runs per claim). A single
    /// attempt is bounded by `processing_timeout_ms`, so a URL valid for
    /// `processing_timeout + headroom` always outlives the attempt that
    /// created it; a retry gets a brand-new URL with a fresh full TTL. The
    /// headroom therefore only needs to cover the gap between signing and
    /// the actual HTTP send within one attempt, plus clock skew — not the
    /// cumulative duration of multiple retries or the whole batch window.
    pub dispatch_ttl_headroom_secs: u64,
    /// TTL applied to signed URLs served via the dashboard image-view
    /// endpoint. Short, since the dashboard re-signs on each page load.
    pub dashboard_ttl_secs: u64,
}

impl Default for SigningConfig {
    fn default() -> Self {
        Self {
            realtime_ttl_secs: 900,          // 15 min
            dispatch_ttl_secs: None,         // derive from processing_timeout_ms
            dispatch_ttl_headroom_secs: 300, // +5 min headroom on derived dispatch TTL
            dashboard_ttl_secs: 300,         // 5 min
        }
    }
}

impl SigningConfig {
    pub fn realtime_ttl(&self) -> Duration {
        Duration::from_secs(self.realtime_ttl_secs)
    }

    /// Resolve the dispatch TTL. If `dispatch_ttl_secs` is set, that
    /// value is used verbatim; otherwise it is derived from
    /// `processing_timeout` + `dispatch_ttl_headroom_secs`.
    pub fn dispatch_ttl(&self, processing_timeout: Duration) -> Duration {
        match self.dispatch_ttl_secs {
            Some(s) => Duration::from_secs(s),
            None => processing_timeout + Duration::from_secs(self.dispatch_ttl_headroom_secs),
        }
    }

    pub fn dashboard_ttl(&self) -> Duration {
        Duration::from_secs(self.dashboard_ttl_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_disabled_with_no_backend() {
        let c = ImageNormalizerConfig::default();
        assert!(!c.enabled);
        assert!(c.backend.is_none(), "no default backend — must be set explicitly when enabled");
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
            backend: Some(BackendConfig::Gcs {
                bucket: "my-bucket".to_string(),
                region: "europe-west4".to_string(),
            }),
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
                dispatch_ttl_secs: Some(120),
                dispatch_ttl_headroom_secs: 300,
                dashboard_ttl_secs: 30,
            },
        };
        let json = serde_json::to_string(&cfg).expect("serialize");
        let back: ImageNormalizerConfig = serde_json::from_str(&json).expect("deserialize");
        assert!(back.enabled);
        match back.backend {
            Some(BackendConfig::Gcs { ref bucket, .. }) => assert_eq!(bucket, "my-bucket"),
            _ => panic!("expected gcs backend"),
        }
        assert_eq!(back.fetcher.max_bytes, 1_048_576);
        assert!(back.fetcher.mime_allowed("image/png"));
        assert!(!back.fetcher.mime_allowed("image/jpeg"));
        assert_eq!(back.signing.realtime_ttl().as_secs(), 60);
    }

    #[test]
    fn dispatch_ttl_explicit_override_used_verbatim() {
        let signing = SigningConfig {
            realtime_ttl_secs: 900,
            dispatch_ttl_secs: Some(3600),
            dispatch_ttl_headroom_secs: 300,
            dashboard_ttl_secs: 300,
        };
        // Caller's processing_timeout is irrelevant when an explicit
        // override is set.
        assert_eq!(signing.dispatch_ttl(Duration::from_secs(60)).as_secs(), 3600);
        assert_eq!(signing.dispatch_ttl(Duration::from_secs(99999)).as_secs(), 3600);
    }

    #[test]
    fn dispatch_ttl_derived_from_processing_timeout_plus_headroom() {
        let signing = SigningConfig::default(); // headroom 300s
        assert_eq!(signing.dispatch_ttl(Duration::from_secs(600)).as_secs(), 900);
        assert_eq!(signing.dispatch_ttl(Duration::from_secs(1800)).as_secs(), 2100);
    }

    #[test]
    fn dispatch_ttl_headroom_is_configurable() {
        let signing = SigningConfig {
            realtime_ttl_secs: 900,
            dispatch_ttl_secs: None,
            dispatch_ttl_headroom_secs: 60,
            dashboard_ttl_secs: 300,
        };
        assert_eq!(signing.dispatch_ttl(Duration::from_secs(600)).as_secs(), 660);
    }
}
