//! Object-store trait and implementations for normalised image bytes.
//!
//! Two implementations:
//!
//! - [`MemoryStore`] — an in-process `HashMap<Sha256, Bytes>` used by tests
//!   and local development. Signed URLs returned here are local
//!   `http://host/dw-img/{hex}` URLs whose `?expires=` parameter is
//!   advisory only (the store does not enforce expiry on read — the
//!   memory backend is intended for `cargo test` and local `cargo run`).
//! - [`GcsStore`] — uploads to Google Cloud Storage via
//!   `google-cloud-storage` and returns V4 signed URLs via the IAM
//!   `signBlob` API (Workload-Identity-friendly: no on-disk private
//!   key required).
//!
//! Production deployments use [`GcsStore`]; the in-memory store is meant
//! for `cargo test` and local `cargo run` workflows where no GCS bucket
//! is configured.
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
        let url = format!(
            "{}/{}?expires={}",
            self.base_url.trim_end_matches('/'),
            token.to_hex(),
            expires_at.timestamp()
        );
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
/// Uses Application Default Credentials (Workload Identity in production)
/// for both the object IO operations (write_object / read_object) and for
/// V4 signed URL generation. Signing piggybacks on the IAM `signBlob` API,
/// so no on-disk private key is required — the GCP SA bound by WI just
/// needs `roles/iam.serviceAccountTokenCreator` on itself and
/// `roles/storage.objectAdmin` on the bucket.
///
/// The client + signer are constructed lazily on first use via `tokio::sync::OnceCell`
/// so that the dwctl binary can boot even when GCS auth isn't yet available
/// (useful in CI / local dev where the `image_normalizer.enabled` flag is
/// off).
pub struct GcsStore {
    pub bucket: String,
    pub region: String,
    client_cell: tokio::sync::OnceCell<google_cloud_storage::client::Storage>,
    signer_cell: tokio::sync::OnceCell<google_cloud_auth::signer::Signer>,
}

impl GcsStore {
    pub fn new(bucket: impl Into<String>, region: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            region: region.into(),
            client_cell: tokio::sync::OnceCell::new(),
            signer_cell: tokio::sync::OnceCell::new(),
        }
    }

    /// Object key shape for a given token. Two-level prefix keeps listing
    /// fan-out small on dense buckets.
    pub(crate) fn key(token: ImageToken) -> String {
        let hex = token.to_hex();
        format!("images/{}/{}/{}", &hex[..2], &hex[2..4], hex)
    }

    /// `projects/_/buckets/<bucket>` — the resource form `SignedUrlBuilder` wants.
    fn bucket_resource(&self) -> String {
        format!("projects/_/buckets/{}", self.bucket)
    }

    async fn client(&self) -> Result<&google_cloud_storage::client::Storage, StoreError> {
        self.client_cell
            .get_or_try_init(|| async {
                google_cloud_storage::client::Storage::builder()
                    .build()
                    .await
                    .map_err(|e| StoreError::Backend(format!("GCS client init: {e}")))
            })
            .await
    }

    async fn signer(&self) -> Result<&google_cloud_auth::signer::Signer, StoreError> {
        self.signer_cell
            .get_or_try_init(|| async {
                google_cloud_auth::credentials::Builder::default()
                    .build_signer()
                    .map_err(|e| StoreError::Backend(format!("ADC signer init: {e}")))
            })
            .await
    }
}

#[async_trait]
impl ImageStore for GcsStore {
    async fn put(&self, token: ImageToken, mime: &str, bytes: Bytes) -> Result<bool, StoreError> {
        // Idempotency: short-circuit if the object already exists.
        if self.exists(token).await? {
            return Ok(false);
        }
        let client = self.client().await?;
        let key = Self::key(token);
        client
            .write_object(self.bucket_resource(), &key, bytes)
            .set_content_type(mime)
            .send_buffered()
            .await
            .map_err(|e| StoreError::Backend(format!("GCS put {key}: {e}")))?;
        Ok(true)
    }

    async fn sign(&self, token: ImageToken, ttl: Duration) -> Result<SignedImageUrl, StoreError> {
        let signer = self.signer().await?;
        let key = Self::key(token);
        let url = google_cloud_storage::builder::storage::SignedUrlBuilder::for_object(self.bucket_resource(), &key)
            .with_method(google_cloud_storage::http::Method::GET)
            .with_expiration(ttl)
            .sign_with(signer)
            .await
            .map_err(|e| StoreError::Backend(format!("GCS sign {key}: {e}")))?;
        let expires_at = Utc::now() + ChronoDuration::from_std(ttl).unwrap_or(ChronoDuration::seconds(900));
        Ok(SignedImageUrl { url, expires_at })
    }

    async fn read(&self, token: ImageToken) -> Result<(String, Bytes), StoreError> {
        let client = self.client().await?;
        let key = Self::key(token);
        let mut resp = client
            .read_object(self.bucket_resource(), &key)
            .send()
            .await
            .map_err(|e| StoreError::Backend(format!("GCS read {key}: {e}")))?;
        let mime = resp.object().content_type.clone();
        let mut bytes_vec: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.next().await {
            let chunk = chunk.map_err(|e| StoreError::Backend(format!("GCS read body: {e}")))?;
            bytes_vec.extend_from_slice(&chunk);
        }
        Ok((mime, Bytes::from(bytes_vec)))
    }

    async fn exists(&self, token: ImageToken) -> Result<bool, StoreError> {
        let client = self.client().await?;
        let key = Self::key(token);
        // The smallest GET we can do — start a read; if it succeeds, the
        // object exists. We immediately drop the response without reading
        // the body. Typed 404 → false; any other error (auth failure,
        // network, server error) → bubble up so misconfiguration can't be
        // misread as a missing object (which would trigger a re-upload).
        match client.read_object(self.bucket_resource(), &key).send().await {
            Ok(_) => Ok(true),
            Err(e) => match e.http_status_code() {
                Some(404) => Ok(false),
                _ => Err(StoreError::Backend(format!("GCS exists {key}: {e}"))),
            },
        }
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

    #[test]
    fn gcs_bucket_resource_format() {
        let s = GcsStore::new("my-bucket", "europe-west4");
        assert_eq!(s.bucket_resource(), "projects/_/buckets/my-bucket");
    }
}
