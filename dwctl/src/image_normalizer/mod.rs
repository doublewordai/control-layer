//! Image-input normalisation for `/v1/chat/completions` and `/v1/responses`.
//!
//! ## What this module does
//!
//! Replaces user-supplied image references in inference request bodies with
//! references to bytes we control. Two flavours of input:
//!
//! - HTTP(S) URLs (always normalised — closes the original SSRF exposure
//!   from forwarding arbitrary URLs to upstream model providers).
//! - `data:` URIs (normalised only when the calling user has opted into
//!   the full image-privacy mode — saves the inflight bandwidth cost of
//!   re-sending the same bytes on every request, and keeps the
//!   user's raw bytes from reaching providers in the request body).
//!
//! Two-stage substitution:
//!
//! 1. **Ingest** ([`ImageNormalizer::ingest`]) — fetch (HTTP) or decode
//!    (data URI), hash the bytes, store in our object store keyed by
//!    SHA-256, return an opaque [`ImageToken`]. Idempotent on content.
//! 2. **Sign** ([`ImageNormalizer::sign`]) — exchange a token for a
//!    short-lived signed URL ready to hand to an upstream provider.
//!
//! Realtime requests are single-stage (sign immediately at middleware
//! time, ~15min TTL because the request completes in seconds). Batch
//! requests are two-stage: ingest at file upload (token stored in the DB)
//! and sign just before each dispatch attempt (~30min TTL per attempt, so
//! retries get fresh URLs and the leak window per attempt is bounded).
//!
//! ## Module layout
//!
//! - [`config`] — Figment-loaded config section.
//! - [`fetcher`] — hardened reqwest fetcher with DNS pinning, IP
//!   deny-list, redirect re-validation, MIME / size caps, retries.
//! - [`ip_filter`] — pure IP deny-list predicate.
//! - [`token`] — opaque `dw-img://{sha256}` token format.
//! - [`data_uri`] — minimal `data:` URI decoder.
//! - [`walker`] — body-traversal helpers for both endpoint shapes and
//!   for both ingest-time substitution and dispatch-time JIT signing.
//! - [`store`] — object-store trait + in-memory impl (for tests / local
//!   dev) and a GCS-backed impl scaffold (full wiring pending).
use async_trait::async_trait;
use bytes::Bytes;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;

pub mod config;
pub mod data_uri;
pub mod fetcher;
pub mod ip_filter;
pub mod store;
pub mod token;
pub mod walker;

pub use config::{BackendConfig, FetcherConfig, ImageNormalizerConfig, SigningConfig};
pub use store::{ImageStore, MemoryStore, SignedImageUrl, StoreError};
pub use token::{ImageToken, TokenParseError};
pub use walker::Mode;

/// Input to [`ImageNormalizer::ingest`].
#[derive(Debug, Clone)]
pub enum ImageInput {
    /// An `http://` / `https://` URL to fetch.
    HttpUrl(String),
    /// An already-decoded data URI payload.
    DataUri(String),
}

/// Top-level errors from the normaliser.
#[derive(Debug, thiserror::Error)]
pub enum NormalizeError {
    #[error("bad input: {0}")]
    BadInput(String),
    #[error("the provided image URL could not be retrieved: {0}; ensure it is publicly accessible and does not require authentication")]
    Unfetchable(String),
    #[error("fetch failed: {0}")]
    FetchFailed(String),
    #[error("transient failure: {0}")]
    Transient(String),
    #[error("store failed: {0}")]
    StoreFailed(String),
    #[error("token not found in store")]
    NotFound,
}

impl From<fetcher::FetchError> for NormalizeError {
    fn from(e: fetcher::FetchError) -> Self {
        match e {
            fetcher::FetchError::BadInput(m) => NormalizeError::BadInput(m),
            fetcher::FetchError::Unfetchable(m) => NormalizeError::Unfetchable(m),
            fetcher::FetchError::FetchFailed(m) => NormalizeError::FetchFailed(m),
            fetcher::FetchError::Transient(m) => NormalizeError::Transient(m),
        }
    }
}

impl From<data_uri::DataUriError> for NormalizeError {
    fn from(e: data_uri::DataUriError) -> Self {
        NormalizeError::BadInput(e.to_string())
    }
}

impl From<StoreError> for NormalizeError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound => NormalizeError::NotFound,
            StoreError::Backend(m) => NormalizeError::StoreFailed(m),
            StoreError::Unimplemented => NormalizeError::StoreFailed("backend not implemented".into()),
        }
    }
}

/// Outcome of a successful [`ImageNormalizer::ingest`] call. Carries
/// not just the content-hash token but also the resolved mime type and
/// byte length, so callers can record useful metadata in `image_access`
/// without re-reading the object from the store.
#[derive(Debug, Clone)]
pub struct IngestResult {
    pub token: ImageToken,
    pub mime: String,
    pub bytes_len: u64,
}

/// The normaliser interface. Hand to middleware and the dispatcher; pick
/// the implementation at startup based on [`ImageNormalizerConfig::enabled`]
/// and [`BackendConfig`].
#[async_trait]
pub trait ImageNormalizer: Send + Sync {
    /// Fetch (or decode) `input`, ensure bytes are in the store, return
    /// the [`IngestResult`] (token + resolved metadata).
    async fn ingest(&self, input: ImageInput) -> Result<IngestResult, NormalizeError>;

    /// Generate a fresh signed URL for `token` with TTL `ttl`.
    async fn sign(&self, token: ImageToken, ttl: Duration) -> Result<SignedImageUrl, NormalizeError>;

    /// Read bytes for `token` directly. Used by the dashboard image
    /// endpoint (after authorisation).
    async fn read(&self, token: ImageToken) -> Result<(String, Bytes), NormalizeError>;

    /// True if `url` already points at an object in our own store (a URL we
    /// previously signed). Callers use this to avoid re-ingesting/re-signing
    /// an already-normalised URL — which would waste a re-fetch and clobber a
    /// longer upstream TTL (e.g. the batch dispatch TTL) with a shorter one.
    /// Default `false` preserves prior always-ingest behaviour.
    fn owns_url(&self, _url: &str) -> bool {
        false
    }
}

/// No-op normaliser used when `config.enabled = false`. Surfaces an error
/// from every call so a misconfigured middleware can't silently strip
/// substitution — keeps the security posture predictable.
pub struct DisabledNormalizer;

#[async_trait]
impl ImageNormalizer for DisabledNormalizer {
    async fn ingest(&self, _input: ImageInput) -> Result<IngestResult, NormalizeError> {
        Err(NormalizeError::BadInput("image normalisation is disabled".into()))
    }
    async fn sign(&self, _token: ImageToken, _ttl: Duration) -> Result<SignedImageUrl, NormalizeError> {
        Err(NormalizeError::BadInput("image normalisation is disabled".into()))
    }
    async fn read(&self, _token: ImageToken) -> Result<(String, Bytes), NormalizeError> {
        Err(NormalizeError::BadInput("image normalisation is disabled".into()))
    }
}

/// Concrete normaliser that composes a [`fetcher::ImageFetcher`] with an
/// [`ImageStore`].
pub struct DefaultImageNormalizer<S: ImageStore> {
    fetcher: fetcher::ImageFetcher,
    store: Arc<S>,
}

impl<S: ImageStore> DefaultImageNormalizer<S> {
    pub fn new(fetcher_cfg: FetcherConfig, store: Arc<S>) -> Self {
        Self {
            fetcher: fetcher::ImageFetcher::new(fetcher_cfg),
            store,
        }
    }
}

#[async_trait]
impl<S: ImageStore + 'static> ImageNormalizer for DefaultImageNormalizer<S> {
    async fn ingest(&self, input: ImageInput) -> Result<IngestResult, NormalizeError> {
        let (mime, bytes) = match input {
            ImageInput::HttpUrl(url) => {
                let fetched = self.fetcher.fetch(&url).await?;
                (fetched.mime, fetched.bytes)
            }
            ImageInput::DataUri(uri) => {
                let decoded = data_uri::parse(&uri)?;
                // Enforce the same size and MIME policy as the HTTP fetch
                // path — a `data:` URI must not bypass the normaliser's
                // content limits (an oversized payload is a memory/storage
                // DoS, and a non-image MIME would otherwise be stored and
                // signed for a downstream provider).
                let len = decoded.bytes.len() as u64;
                if len > self.fetcher.max_bytes() {
                    return Err(NormalizeError::BadInput(format!(
                        "data: URI payload {len} bytes exceeds cap {}",
                        self.fetcher.max_bytes()
                    )));
                }
                if !self.fetcher.mime_allowed(&decoded.mime) {
                    return Err(NormalizeError::BadInput(format!("mime not allowed: {}", decoded.mime)));
                }
                (decoded.mime, Bytes::from(decoded.bytes))
            }
        };
        let bytes_len = bytes.len() as u64;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let digest = hasher.finalize();
        let mut sha = [0u8; 32];
        sha.copy_from_slice(&digest);
        let token = ImageToken(sha);

        // exists() short-circuit avoids re-uploading dedup hits.
        if !self.store.exists(token).await? {
            self.store.put(token, &mime, bytes).await?;
        }
        Ok(IngestResult { token, mime, bytes_len })
    }

    async fn sign(&self, token: ImageToken, ttl: Duration) -> Result<SignedImageUrl, NormalizeError> {
        Ok(self.store.sign(token, ttl).await?)
    }

    async fn read(&self, token: ImageToken) -> Result<(String, Bytes), NormalizeError> {
        Ok(self.store.read(token).await?)
    }

    fn owns_url(&self, url: &str) -> bool {
        self.store.owns_url(url)
    }
}

/// Build a normaliser from config. Returns a boxed trait object so callers
/// can hold it as `Arc<dyn ImageNormalizer>` regardless of backend choice.
///
/// The returned `Arc` is **the single shared instance** for the process —
/// callers must hold and clone it (e.g. from `AppState`), not re-build it
/// per request. Rebuilding per request would re-init the GCS client + ADC
/// signer on every dashboard image load, which hammers the GCP metadata
/// server and creates a new mTLS connection each time.
///
/// Returns an error if `enabled = true` but no backend is configured —
/// silently falling back to `MemoryStore` in production would lose bytes
/// on restart and across replicas.
pub fn from_config(cfg: &ImageNormalizerConfig) -> Result<Arc<dyn ImageNormalizer>, anyhow::Error> {
    if !cfg.enabled {
        return Ok(Arc::new(DisabledNormalizer));
    }
    let backend = cfg
        .backend
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("image_normalizer.enabled = true but image_normalizer.backend is not set"))?;
    Ok(match backend {
        BackendConfig::Memory => {
            tracing::warn!(
                "image_normalizer enabled with the in-memory backend: stored image bytes are lost on \
                 restart and are not shared across replicas — use gcs or s3_compatible in production"
            );
            let store = Arc::new(MemoryStore::new());
            Arc::new(DefaultImageNormalizer::new(cfg.fetcher.clone(), store))
        }
        BackendConfig::Gcs { bucket, region } => {
            let store = Arc::new(store::GcsStore::new(bucket.clone(), region.clone()));
            Arc::new(DefaultImageNormalizer::new(cfg.fetcher.clone(), store))
        }
        BackendConfig::S3Compatible {
            bucket,
            endpoint_url,
            region,
            force_path_style,
        } => {
            // Credentials are sourced from the environment (not the
            // serializable config) so they can't leak via a config dump.
            //
            // NB: these are deliberately NOT prefixed `DWCTL_`. The config
            // loader maps every `DWCTL_`-prefixed variable onto a config
            // field and rejects unknown ones, so a `DWCTL_`-prefixed secret
            // name would fail startup. Plain (unprefixed) names are ignored
            // by the config loader and read directly here.
            let access_key_id = std::env::var("IMAGE_NORMALIZER_S3_ACCESS_KEY_ID").map_err(|_| {
                anyhow::anyhow!(
                    "image_normalizer.backend.type = s3_compatible requires the \
                     IMAGE_NORMALIZER_S3_ACCESS_KEY_ID environment variable"
                )
            })?;
            let secret_access_key = std::env::var("IMAGE_NORMALIZER_S3_SECRET_ACCESS_KEY").map_err(|_| {
                anyhow::anyhow!(
                    "image_normalizer.backend.type = s3_compatible requires the \
                     IMAGE_NORMALIZER_S3_SECRET_ACCESS_KEY environment variable"
                )
            })?;
            let store = Arc::new(store::S3CompatStore::new(
                bucket.clone(),
                endpoint_url.clone(),
                region.clone(),
                *force_path_style,
                access_key_id,
                secret_access_key,
            ));
            Arc::new(DefaultImageNormalizer::new(cfg.fetcher.clone(), store))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1x1 transparent PNG, base64-encoded as a data URI.
    const TINY_PNG_DATA_URI: &str =
        "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";

    #[tokio::test]
    async fn ingest_data_uri_then_sign_round_trip() {
        let store = Arc::new(MemoryStore::new());
        let n = DefaultImageNormalizer::new(FetcherConfig::default(), store.clone());

        let result = n.ingest(ImageInput::DataUri(TINY_PNG_DATA_URI.to_string())).await.unwrap();
        let token = result.token;
        assert_eq!(result.mime, "image/png");
        assert!(result.bytes_len > 0, "bytes_len should be the actual decoded length, got 0");

        // dedup: ingesting the same URI again yields the same token and
        // does not duplicate the stored bytes.
        let result_again = n.ingest(ImageInput::DataUri(TINY_PNG_DATA_URI.to_string())).await.unwrap();
        assert_eq!(token, result_again.token);
        assert_eq!(result.bytes_len, result_again.bytes_len);

        // sign returns a usable URL with the token hex baked in.
        let signed = n.sign(token, Duration::from_secs(60)).await.unwrap();
        assert!(signed.url.contains(&token.to_hex()));

        // read returns the original bytes back.
        let (mime, bytes) = n.read(token).await.unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[tokio::test]
    async fn ingest_bad_data_uri_returns_bad_input() {
        let store = Arc::new(MemoryStore::new());
        let n = DefaultImageNormalizer::new(FetcherConfig::default(), store);
        let err = n.ingest(ImageInput::DataUri("data:image/png,raw".to_string())).await.unwrap_err();
        assert!(matches!(err, NormalizeError::BadInput(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn ingest_oversized_data_uri_is_rejected() {
        // A `data:` URI larger than the configured cap must be refused the
        // same way the HTTP fetch path refuses an oversized body — a data
        // URI must not be a way to bypass `max_bytes`.
        let store = Arc::new(MemoryStore::new());
        let cfg = FetcherConfig {
            max_bytes: 8,
            ..FetcherConfig::default()
        };
        let n = DefaultImageNormalizer::new(cfg, store);
        let err = n.ingest(ImageInput::DataUri(TINY_PNG_DATA_URI.to_string())).await.unwrap_err();
        assert!(matches!(err, NormalizeError::BadInput(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn ingest_data_uri_with_disallowed_mime_is_rejected() {
        // A non-image `data:` URI decodes cleanly but must still be refused —
        // the MIME allow-list applies to data URIs, not just HTTP fetches.
        let store = Arc::new(MemoryStore::new());
        let n = DefaultImageNormalizer::new(FetcherConfig::default(), store);
        let err = n
            .ingest(ImageInput::DataUri("data:text/html;base64,PGgxPmhpPC9oMT4=".to_string()))
            .await
            .unwrap_err();
        assert!(matches!(err, NormalizeError::BadInput(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn disabled_normalizer_errors_predictably() {
        let n = DisabledNormalizer;
        let err = n.ingest(ImageInput::DataUri(TINY_PNG_DATA_URI.to_string())).await.unwrap_err();
        assert!(matches!(err, NormalizeError::BadInput(_)));
    }

    #[test]
    fn from_config_disabled_returns_disabled_normalizer() {
        let cfg = ImageNormalizerConfig::default();
        // Smoke check: ensure we got *something*; the disabled-error
        // behaviour is exercised in `disabled_normalizer_errors_predictably`.
        let _: Arc<dyn ImageNormalizer> = from_config(&cfg).expect("disabled config must build cleanly");
    }

    #[test]
    fn from_config_memory_backend_when_enabled() {
        let cfg = ImageNormalizerConfig {
            enabled: true,
            backend: Some(BackendConfig::Memory),
            fetcher: FetcherConfig::default(),
            signing: SigningConfig::default(),
        };
        let _: Arc<dyn ImageNormalizer> = from_config(&cfg).expect("memory backend must build");
    }

    #[test]
    fn from_config_enabled_without_backend_errors() {
        let cfg = ImageNormalizerConfig {
            enabled: true,
            backend: None,
            fetcher: FetcherConfig::default(),
            signing: SigningConfig::default(),
        };
        // Manual match because the Ok arm holds `Arc<dyn ImageNormalizer>`
        // which doesn't implement Debug (required by `expect_err`).
        match from_config(&cfg) {
            Ok(_) => panic!("enabled + no backend must error"),
            Err(e) => {
                let msg = e.to_string();
                assert!(msg.contains("backend"), "error should mention 'backend': {msg}");
            }
        }
    }
}
