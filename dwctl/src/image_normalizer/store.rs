//! Object-store trait and implementations for normalised image bytes.
//!
//! Three implementations:
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
//! - [`S3CompatStore`] — any S3-compatible store reached via a custom
//!   endpoint (Cloudflare R2, MinIO, Backblaze B2, AWS S3). Uses static
//!   access-key credentials and local SigV4 presigning.
//!
//! Production deployments use [`GcsStore`] or [`S3CompatStore`]; the
//! in-memory store is meant for `cargo test` and local `cargo run`
//! workflows where no bucket is configured.
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use url::Url;

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

    /// True if `url` already points at an object in THIS store — i.e. a URL
    /// we previously signed. Used to avoid re-ingesting and re-signing our
    /// own signed URLs: doing so would (a) waste a round-trip re-fetching an
    /// image we already host and (b) clobber a longer upstream TTL (e.g. the
    /// batch dispatch TTL) with whatever TTL the re-signing path uses.
    ///
    /// Default `false` (treat nothing as ours, always ingest) preserves the
    /// prior behaviour for backends that don't override it.
    fn owns_url(&self, _url: &str) -> bool {
        false
    }
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

    fn owns_url(&self, url: &str) -> bool {
        // We hand out `{base_url}/{hex}?expires=...` URLs. Match the parsed
        // origin (scheme + host + port) exactly and require the candidate path
        // to extend our base path at a `/` boundary — so neither a different
        // host nor a sibling prefix (`/dw-img2`, `/dw-img.evil`) can be
        // mistaken for one of ours.
        let (Ok(base), Ok(u)) = (Url::parse(&self.base_url), Url::parse(url)) else {
            return false;
        };
        if base.scheme() != u.scheme() || base.host_str() != u.host_str() || base.port_or_known_default() != u.port_or_known_default() {
            return false;
        }
        let prefix = base.path().trim_end_matches('/');
        match u.path().strip_prefix(prefix) {
            Some(rest) => rest.is_empty() || rest.starts_with('/'),
            None => false,
        }
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

    fn owns_url(&self, url: &str) -> bool {
        // V4 signed URLs are path-style:
        // `https://storage.googleapis.com/{bucket}/images/...`. Parse and match
        // the host *exactly* (so a suffix-spoof like
        // `storage.googleapis.com.evil.com` is rejected) and look for our
        // bucket + content-addressed `images/` prefix on the path only (so it
        // can't be smuggled in via the query string).
        let Ok(u) = Url::parse(url) else {
            return false;
        };
        u.scheme() == "https"
            && u.host_str() == Some("storage.googleapis.com")
            && u.path().starts_with(&format!("/{}/images/", self.bucket))
    }
}

// ========================== S3CompatStore =================================

/// SigV4 presigning has a hard 7-day ceiling. Clamp any requested TTL to
/// just under it so a misconfigured `dispatch_ttl` can't make presigning
/// fail at runtime.
const S3_MAX_PRESIGN: Duration = Duration::from_secs(7 * 24 * 60 * 60 - 60);

/// S3-compatible object store backend (Cloudflare R2, MinIO, Backblaze B2,
/// AWS S3) reached via a custom endpoint.
///
/// Unlike [`GcsStore`], this uses static access-key credentials and *local*
/// SigV4 presigning — no `signBlob` round-trip and no Workload Identity. The
/// credentials are supplied at construction (read from the environment by
/// [`from_config`](super::from_config), never from the serializable config),
/// so they cannot leak via a config dump.
///
/// The `aws_sdk_s3::Client` is cheap to build (no network at construction),
/// so it is created eagerly in [`S3CompatStore::new`].
pub struct S3CompatStore {
    bucket: String,
    /// The configured endpoint (e.g. `https://<acct>.r2.cloudflarestorage.com`),
    /// trailing slash trimmed. Parsed in [`owns_url`] to match the host of our
    /// own presigned URLs — exactly for path-style, or as `{bucket}.{host}` for
    /// virtual-hosted addressing.
    endpoint: String,
    client: aws_sdk_s3::Client,
}

impl S3CompatStore {
    pub fn new(
        bucket: impl Into<String>,
        endpoint_url: impl Into<String>,
        region: impl Into<String>,
        force_path_style: bool,
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
    ) -> Self {
        let creds =
            aws_credential_types::Credentials::new(access_key_id.into(), secret_access_key.into(), None, None, "dwctl-image-normalizer");
        let endpoint = endpoint_url.into();
        let s3_config = aws_sdk_s3::config::Builder::new()
            .region(aws_sdk_s3::config::Region::new(region.into()))
            .credentials_provider(creds)
            .endpoint_url(endpoint.clone())
            .force_path_style(force_path_style)
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .build();
        Self {
            bucket: bucket.into(),
            endpoint: endpoint.trim_end_matches('/').to_string(),
            client: aws_sdk_s3::Client::from_conf(s3_config),
        }
    }

    /// Object key shape for a given token. Mirrors [`GcsStore::key`] so the
    /// two backends are interchangeable for the same content hash.
    fn key(token: ImageToken) -> String {
        let hex = token.to_hex();
        format!("images/{}/{}/{}", &hex[..2], &hex[2..4], hex)
    }
}

#[async_trait]
impl ImageStore for S3CompatStore {
    async fn put(&self, token: ImageToken, mime: &str, bytes: Bytes) -> Result<bool, StoreError> {
        // Idempotency: short-circuit if the object already exists.
        if self.exists(token).await? {
            return Ok(false);
        }
        let key = Self::key(token);
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .content_type(mime)
            .body(aws_sdk_s3::primitives::ByteStream::from(bytes))
            .send()
            .await
            .map_err(|e| StoreError::Backend(format!("S3 put {key}: {}", e.into_service_error())))?;
        Ok(true)
    }

    async fn sign(&self, token: ImageToken, ttl: Duration) -> Result<SignedImageUrl, StoreError> {
        let key = Self::key(token);
        let ttl = ttl.min(S3_MAX_PRESIGN);
        let presign = aws_sdk_s3::presigning::PresigningConfig::expires_in(ttl)
            .map_err(|e| StoreError::Backend(format!("S3 presign config: {e}")))?;
        let req = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .presigned(presign)
            .await
            .map_err(|e| StoreError::Backend(format!("S3 sign {key}: {}", e.into_service_error())))?;
        let expires_at = Utc::now() + ChronoDuration::from_std(ttl).unwrap_or(ChronoDuration::seconds(900));
        Ok(SignedImageUrl {
            url: req.uri().to_string(),
            expires_at,
        })
    }

    async fn read(&self, token: ImageToken) -> Result<(String, Bytes), StoreError> {
        let key = Self::key(token);
        let resp = self.client.get_object().bucket(&self.bucket).key(&key).send().await.map_err(|e| {
            let svc = e.into_service_error();
            if svc.is_no_such_key() {
                StoreError::NotFound
            } else {
                StoreError::Backend(format!("S3 read {key}: {svc}"))
            }
        })?;
        let mime = resp.content_type().unwrap_or("application/octet-stream").to_string();
        let bytes = resp
            .body
            .collect()
            .await
            .map_err(|e| StoreError::Backend(format!("S3 read body {key}: {e}")))?
            .into_bytes();
        Ok((mime, bytes))
    }

    async fn exists(&self, token: ImageToken) -> Result<bool, StoreError> {
        let key = Self::key(token);
        // Typed 404 → false; any other error (auth, network, server) bubbles
        // up so a misconfiguration can't be misread as a missing object
        // (which would otherwise trigger a needless re-upload).
        match self.client.head_object().bucket(&self.bucket).key(&key).send().await {
            Ok(_) => Ok(true),
            Err(e) => {
                let svc = e.into_service_error();
                if svc.is_not_found() {
                    Ok(false)
                } else {
                    Err(StoreError::Backend(format!("S3 exists {key}: {svc}")))
                }
            }
        }
    }

    fn owns_url(&self, url: &str) -> bool {
        // Presigned URLs from this store are either path-style
        // (`{endpoint}/{bucket}/images/{hex}/{hex}/{sha}?X-Amz-...`) or, when
        // `force_path_style` is off, virtual-hosted
        // (`https://{bucket}.{endpoint-host}/images/...`). Parse both endpoint
        // and candidate and match the host *exactly* — rejecting suffix-spoofs
        // like `…r2.cloudflarestorage.com.evil.com` — then look for the
        // content-addressed `images/` prefix on the path only (never the query).
        let (Ok(ep), Ok(u)) = (Url::parse(&self.endpoint), Url::parse(url)) else {
            return false;
        };
        if u.scheme() != ep.scheme() {
            return false;
        }
        let (Some(ep_host), Some(u_host)) = (ep.host_str(), u.host_str()) else {
            return false;
        };
        // Path-style: same host[:port], `/{bucket}/images/...`.
        if u_host == ep_host && u.port_or_known_default() == ep.port_or_known_default() {
            return u.path().starts_with(&format!("/{}/images/", self.bucket));
        }
        // Virtual-hosted: `{bucket}.{endpoint-host}`, `/images/...`.
        if u_host == format!("{}.{}", self.bucket, ep_host) {
            return u.path().starts_with("/images/");
        }
        false
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

    #[test]
    fn s3_key_uses_two_level_prefix() {
        let key = S3CompatStore::key(tok(0xab));
        assert!(key.starts_with("images/ab/ab/abab"));
    }

    #[test]
    fn s3_compat_store_builds_client() {
        // Construction is offline (no network): proves the trimmed
        // aws-sdk-s3 feature set is enough to build a custom-endpoint,
        // path-style client with static credentials.
        let s = S3CompatStore::new(
            "imgs",
            "https://example.r2.cloudflarestorage.com",
            "auto",
            true,
            "AKIDEXAMPLE",
            "secret",
        );
        assert_eq!(s.bucket, "imgs");
    }

    #[test]
    fn s3_owns_url_matches_own_presigned_urls_only() {
        let s = S3CompatStore::new(
            "images-bucket",
            "https://acct.r2.cloudflarestorage.com",
            "auto",
            true,
            "AKIDEXAMPLE",
            "secret",
        );
        let hex = tok(0xab).to_hex();
        // Our own path-style presigned URL → owned.
        let ours = format!(
            "https://acct.r2.cloudflarestorage.com/images-bucket/images/{}/{}/{}?x-id=GetObject&X-Amz-Signature=deadbeef",
            &hex[..2],
            &hex[2..4],
            hex
        );
        assert!(s.owns_url(&ours));
        // Same bucket+key path but a DIFFERENT (external) host → not ours,
        // so it still gets normalised rather than forwarded raw.
        let spoofed = format!("https://evil.example.com/images-bucket/images/{}/{}/{}", &hex[..2], &hex[2..4], hex);
        assert!(!s.owns_url(&spoofed));
        // Host suffix-spoof: our endpoint host as a *prefix* of an
        // attacker-controlled host must not match (exact-host parsing).
        assert!(!s.owns_url("https://acct.r2.cloudflarestorage.com.evil.com/images-bucket/images/ab/cd/abcd"));
        // The owned key prefix smuggled into the query string, on a foreign
        // host → not ours (we only inspect the parsed path).
        assert!(!s.owns_url("https://evil.example.com/x?next=/images-bucket/images/ab/cd/abcd"));
        // Arbitrary external image URL → not ours.
        assert!(!s.owns_url("https://example.com/cat.png"));
        // A data: URI is never ours.
        assert!(!s.owns_url("data:image/png;base64,AAAA"));
    }

    #[test]
    fn s3_owns_url_matches_virtual_hosted_urls() {
        // With `force_path_style = false`, presigned URLs are virtual-hosted:
        // `https://{bucket}.{endpoint-host}/images/...`.
        let s = S3CompatStore::new(
            "images-bucket",
            "https://acct.r2.cloudflarestorage.com",
            "auto",
            false,
            "AKIDEXAMPLE",
            "secret",
        );
        assert!(s.owns_url("https://images-bucket.acct.r2.cloudflarestorage.com/images/ab/cd/abcd?X-Amz-Signature=deadbeef"));
        // A different bucket label on the host → not ours.
        assert!(!s.owns_url("https://other.acct.r2.cloudflarestorage.com/images/ab/cd/abcd"));
        // Bucket as a suffix-spoof of an external host → not ours.
        assert!(!s.owns_url("https://images-bucket.acct.r2.cloudflarestorage.com.evil.com/images/ab/cd/abcd"));
    }

    #[test]
    fn s3_owns_url_endpoint_trailing_slash_trimmed() {
        let s = S3CompatStore::new(
            "imgs",
            "https://acct.r2.cloudflarestorage.com/",
            "auto",
            true,
            "AKIDEXAMPLE",
            "secret",
        );
        assert_eq!(s.endpoint, "https://acct.r2.cloudflarestorage.com");
        assert!(s.owns_url("https://acct.r2.cloudflarestorage.com/imgs/images/ab/cd/abcd"));
    }

    #[test]
    fn memory_store_owns_url() {
        let s = MemoryStore::new().with_base_url("http://test.local/img");
        assert!(s.owns_url("http://test.local/img/abcd?expires=123"));
        assert!(!s.owns_url("https://example.com/cat.png"));
        // Sibling prefixes sharing the leading characters must NOT match —
        // the path has to extend `/img` at a `/` boundary.
        assert!(!s.owns_url("http://test.local/img2/abcd"));
        assert!(!s.owns_url("http://test.local/img.evil/abcd"));
        // Same host + path but a different scheme → not ours.
        assert!(!s.owns_url("https://test.local/img/abcd"));
        // Same path on a different host → not ours.
        assert!(!s.owns_url("http://evil.test/img/abcd"));
    }

    #[test]
    fn gcs_owns_url_matches_bucket_and_key_prefix() {
        let s = GcsStore::new("images-bucket", "europe-west4");
        assert!(s.owns_url("https://storage.googleapis.com/images-bucket/images/ab/cd/abcd?X-Goog-Signature=x"));
        assert!(!s.owns_url("https://storage.googleapis.com/other-bucket/images/ab/cd/abcd"));
        assert!(!s.owns_url("https://example.com/cat.png"));
        // Host suffix-spoof: `storage.googleapis.com` as a prefix of an
        // attacker host must not match (exact-host parsing).
        assert!(!s.owns_url("https://storage.googleapis.com.evil.com/images-bucket/images/ab/cd/abcd"));
        // Our key prefix smuggled into the query on a foreign host → not ours.
        assert!(!s.owns_url("https://evil.example.com/x?u=/images-bucket/images/ab/cd/abcd"));
        // Plain http (signed URLs are always https) → not ours.
        assert!(!s.owns_url("http://storage.googleapis.com/images-bucket/images/ab/cd/abcd"));
    }

    #[tokio::test]
    async fn s3_presign_caps_ttl_at_seven_days() {
        // A wildly oversized TTL must not blow past the SigV4 7-day ceiling;
        // it is clamped before PresigningConfig validation, so signing the
        // (offline) presigned URL succeeds rather than erroring.
        let s = S3CompatStore::new(
            "imgs",
            "https://example.r2.cloudflarestorage.com",
            "auto",
            true,
            "AKIDEXAMPLE",
            "secret",
        );
        let signed = s
            .sign(tok(5), Duration::from_secs(30 * 24 * 60 * 60))
            .await
            .expect("presign with clamped ttl should succeed");
        assert!(signed.url.starts_with("https://example.r2.cloudflarestorage.com"));
        // Expiry reflects the clamp, not the requested 30 days.
        assert!(signed.expires_at <= Utc::now() + ChronoDuration::days(7));
    }
}
