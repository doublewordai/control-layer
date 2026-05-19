//! Object-store trait and implementations for normalised image bytes.
//!
//! Two implementations:
//!
//! - [`MemoryStore`] — an in-process `HashMap<Sha256, Bytes>` used by tests
//!   and local development. Signed URLs returned here are local
//!   `http://host/dw-img/{hex}` URLs that the dashboard image endpoint or
//!   integration-test fixtures can resolve.
//! - [`GcsStore`] — uploads to Google Cloud Storage and returns V4
//!   signed URLs via the IAM `signBlob` API.
//!
//! Production deployments use [`GcsStore`]; the in-memory store is meant for
//! `cargo test` and local `cargo run` workflows where no GCS bucket is
//! configured.
//!
//! NOTE (GCS implementation): the GCS-backed store is currently scaffolded
//! and returns `Unimplemented` from `put` / `sign`. Wiring it up requires
//! choosing a crate combination for Workload-Identity-based V4 signing
//! (likely `cloud-storage` or `google-cloud-storage` plus `gcp_auth` for
//! the IAM `signBlob` dance). This is a focused follow-up — the trait
//! seam is stable, so the rest of the system can be built and tested
//! against `MemoryStore` first.
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use super::token::ImageToken;

/// Stored image metadata + a signed URL pointing at the bytes.
#[derive(Debug, Clone)]
pub struct SignedImageUrl {
    pub url: String,
    pub expires_at: DateTime<Utc>,
}

/// Errors that can come out of an object-store backend.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("object not found in store")]
    NotFound,
    #[error("object-store backend error: {0}")]
    Backend(String),
    #[error("not implemented for this backend")]
    Unimplemented,
}

/// Object-store backend for normalised image bytes. Content-addressed by
/// SHA-256.
#[async_trait]
pub trait ImageStore: Send + Sync {
    /// Idempotently store `bytes` under `token`. If the object already
    /// exists, this is a no-op and returns `Ok(false)`. Otherwise stores
    /// and returns `Ok(true)`.
    async fn put(&self, token: ImageToken, mime: &str, bytes: Bytes) -> Result<bool, StoreError>;

    /// Generate a short-lived signed URL pointing at the bytes for `token`.
    async fn sign(&self, token: ImageToken, ttl: Duration) -> Result<SignedImageUrl, StoreError>;

    /// Read the bytes for `token` directly. Used by the dashboard
    /// image-view path. Caller is responsible for authorisation.
    async fn read(&self, token: ImageToken) -> Result<(String, Bytes), StoreError>;

    /// True if an object with this token already exists. Cheap check used
    /// by the ingest path to skip uploads on dedup hits.
    async fn exists(&self, token: ImageToken) -> Result<bool, StoreError>;
}

// ============================ MemoryStore =================================

/// In-process store. Bytes held in a `Mutex<HashMap>` keyed by SHA-256.
///
/// Signed URLs returned here are `http://{base}/dw-img/{hex}?expires={ts}`
/// where `base` is configured via [`MemoryStore::with_base_url`]. The
/// dashboard image endpoint (or test fixtures) resolve these.
pub struct MemoryStore {
    inner: Mutex<HashMap<ImageToken, (String, Bytes)>>,
    base_url: String,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            base_url: "http://localhost/dw-img".to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

#[async_trait]
impl ImageStore for MemoryStore {
    async fn put(&self, token: ImageToken, mime: &str, bytes: Bytes) -> Result<bool, StoreError> {
        let mut map = self.inner.lock().expect("MemoryStore mutex poisoned");
        if map.contains_key(&token) {
            return Ok(false);
        }
        map.insert(token, (mime.to_string(), bytes));
        Ok(true)
    }

    async fn sign(&self, token: ImageToken, ttl: Duration) -> Result<SignedImageUrl, StoreError> {
        let map = self.inner.lock().expect("MemoryStore mutex poisoned");
        if !map.contains_key(&token) {
            return Err(StoreError::NotFound);
        }
        let expires_at = Utc::now() + ChronoDuration::from_std(ttl).unwrap_or(ChronoDuration::seconds(900));
        let url = format!("{}/{}?expires={}", self.base_url.trim_end_matches('/'), token.to_hex(), expires_at.timestamp());
        Ok(SignedImageUrl { url, expires_at })
    }

    async fn read(&self, token: ImageToken) -> Result<(String, Bytes), StoreError> {
        let map = self.inner.lock().expect("MemoryStore mutex poisoned");
        map.get(&token).cloned().ok_or(StoreError::NotFound)
    }

    async fn exists(&self, token: ImageToken) -> Result<bool, StoreError> {
        let map = self.inner.lock().expect("MemoryStore mutex poisoned");
        Ok(map.contains_key(&token))
    }
}

// ============================ GcsStore ====================================

/// Google Cloud Storage backend.
///
/// **Not yet wired up.** Requires:
/// - A GCS crate for uploads (e.g. `google-cloud-storage` or `cloud-storage`)
/// - V4 URL signing via the IAM `signBlob` API (Workload Identity has no
///   on-disk private key, so the `gcp_auth` + IAM signing approach is the
///   right one).
///
/// The struct + trait impl are scaffolded so that:
/// - The config layer can refer to it and rule selection works at startup.
/// - The trait seam is final: when the GCS wiring lands, no caller needs
///   to change.
pub struct GcsStore {
    pub bucket: String,
    pub region: String,
}

impl GcsStore {
    pub fn new(bucket: impl Into<String>, region: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            region: region.into(),
        }
    }

    /// Object key shape for a given token. The two-level prefix keeps
    /// listing fan-out small on dense buckets. Used once the put/sign
    /// methods are wired up.
    #[allow(dead_code)]
    pub(crate) fn key(token: ImageToken) -> String {
        let hex = token.to_hex();
        // Two-level prefix for object-listing fan-out, common dedup pattern.
        format!("images/{}/{}/{}", &hex[..2], &hex[2..4], hex)
    }
}

#[async_trait]
impl ImageStore for GcsStore {
    async fn put(&self, _token: ImageToken, _mime: &str, _bytes: Bytes) -> Result<bool, StoreError> {
        Err(StoreError::Unimplemented)
    }

    async fn sign(&self, _token: ImageToken, _ttl: Duration) -> Result<SignedImageUrl, StoreError> {
        Err(StoreError::Unimplemented)
    }

    async fn read(&self, _token: ImageToken) -> Result<(String, Bytes), StoreError> {
        Err(StoreError::Unimplemented)
    }

    async fn exists(&self, _token: ImageToken) -> Result<bool, StoreError> {
        Err(StoreError::Unimplemented)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(b: u8) -> ImageToken {
        ImageToken([b; 32])
    }

    #[tokio::test]
    async fn memory_store_dedups_puts() {
        let s = MemoryStore::new();
        let first = s.put(tok(1), "image/png", Bytes::from_static(b"hello")).await.unwrap();
        let second = s.put(tok(1), "image/png", Bytes::from_static(b"hello")).await.unwrap();
        assert!(first);
        assert!(!second);
    }

    #[tokio::test]
    async fn memory_store_sign_returns_url_with_token_hex() {
        let s = MemoryStore::new().with_base_url("http://test.local/img");
        s.put(tok(7), "image/png", Bytes::from_static(b"x")).await.unwrap();
        let signed = s.sign(tok(7), Duration::from_secs(60)).await.unwrap();
        let token_hex = tok(7).to_hex();
        assert!(signed.url.contains(&token_hex));
        assert!(signed.url.starts_with("http://test.local/img/"));
        assert!(signed.expires_at > Utc::now());
    }

    #[tokio::test]
    async fn memory_store_sign_missing_object_returns_not_found() {
        let s = MemoryStore::new();
        let err = s.sign(tok(99), Duration::from_secs(60)).await.unwrap_err();
        assert!(matches!(err, StoreError::NotFound));
    }

    #[tokio::test]
    async fn memory_store_exists_round_trip() {
        let s = MemoryStore::new();
        assert!(!s.exists(tok(2)).await.unwrap());
        s.put(tok(2), "image/png", Bytes::from_static(b"x")).await.unwrap();
        assert!(s.exists(tok(2)).await.unwrap());
    }

    #[tokio::test]
    async fn memory_store_read_returns_bytes_and_mime() {
        let s = MemoryStore::new();
        s.put(tok(3), "image/jpeg", Bytes::from_static(b"jpegbytes")).await.unwrap();
        let (mime, bytes) = s.read(tok(3)).await.unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(bytes.as_ref(), b"jpegbytes");
    }

    #[test]
    fn gcs_key_uses_two_level_prefix() {
        let key = GcsStore::key(tok(0xab));
        assert!(key.starts_with("images/ab/ab/abab"));
    }

    #[tokio::test]
    async fn gcs_store_methods_return_unimplemented() {
        // Scaffolded only — exercised here so the test suite flags any
        // accidental wiring before the proper implementation lands.
        let s = GcsStore::new("test-bucket", "europe-west4");
        assert!(matches!(s.put(tok(1), "image/png", Bytes::new()).await, Err(StoreError::Unimplemented)));
        assert!(matches!(s.sign(tok(1), Duration::from_secs(60)).await, Err(StoreError::Unimplemented)));
        assert!(matches!(s.read(tok(1)).await, Err(StoreError::Unimplemented)));
        assert!(matches!(s.exists(tok(1)).await, Err(StoreError::Unimplemented)));
    }
}
