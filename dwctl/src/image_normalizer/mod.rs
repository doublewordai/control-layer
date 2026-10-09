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
//!    (data URI), hash the bytes, store them, return an opaque
//!    [`ImageToken`]. Objects are keyed by the content hash, or — with
//!    `unique_upload_keys` on — by the content hash plus a random per-ingest
//!    upload ID, so identical bytes never share an object key.
//! 2. **Sign** ([`ImageNormalizer::sign`]) — exchange a token for a
//!    short-lived signed URL ready to hand to an upstream provider.
//!
//! Zero-data-retention requests never put a plaintext image in the store. A
//! ZDR flex request ingests with [`ImageNormalizer::ingest_sealed`], which
//! encrypts the bytes with the request's ZDR key before upload (see
//! [`sealed`]); the edge decrypts at dispatch and inlines a `data:` URI. A
//! realtime ZDR request uses [`ImageNormalizer::load`] to fetch and validate
//! the image without storing it at all.
//!
//! Realtime requests are single-stage (sign immediately at middleware
//! time, ~15min TTL because the request completes in seconds). Queued
//! requests (flex, batch files) are two-stage: ingest at submission (the
//! token is what sits in the DB), and sign when the daemon's dispatch loops
//! back through the edge middleware (dispatch TTL per attempt, so retries
//! get fresh URLs and the leak window per attempt is bounded). Signing
//! happens in the edge middleware — BELOW the prompt-cache layer — on
//! purpose: the cache hashes the stable content-addressed token, never the
//! per-attempt signed URL, so a byte-identical image keeps a prefix chain
//! intact across calls.
//!
//! ## Module layout
//!
//! - [`config`] — Figment-loaded config section.
//! - [`fetcher`] — hardened reqwest fetcher with DNS pinning, IP
//!   deny-list, redirect re-validation, MIME / size caps, retries.
//! - [`ip_filter`] — pure IP deny-list predicate.
//! - [`token`] — opaque `dw-img://{sha256}.{upload_id}` token format (legacy
//!   tokens omit `.{upload_id}`).
//! - [`data_uri`] — minimal `data:` URI decoder.
//! - [`walker`] — body-traversal helpers for both endpoint shapes and
//!   for both ingest-time substitution and dispatch-time JIT signing.
//! - [`sealed`] — encrypted-at-rest envelope for zero-data-retention images.
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
pub mod sealed;
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
    /// A `dw-img://` token was presented by a caller who never submitted
    /// that image (no `image_access` row for their user or organization).
    #[error("image token is not accessible to this caller")]
    Forbidden,
    /// The authorisation / bookkeeping store (`image_access`, the API-key
    /// lookup behind it) could not be reached. Transient and retryable, but
    /// distinct from a failed image FETCH so clients and telemetry are told
    /// what actually failed.
    #[error("image access store unavailable")]
    AccessUnavailable,
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

    /// Fetch (or decode) `input` and apply the same size / MIME policy as
    /// [`ingest`](Self::ingest), WITHOUT storing anything. Returns
    /// `(mime, bytes)`. Used for realtime zero-data-retention requests, whose
    /// images are inlined for the provider instead of being written to the
    /// store.
    async fn load(&self, _input: ImageInput) -> Result<(String, Bytes), NormalizeError> {
        Err(NormalizeError::BadInput("image normalisation is disabled".into()))
    }

    /// Like [`ingest`](Self::ingest), but the bytes are encrypted with `key`
    /// before they reach the store (see [`sealed`]), so the store never holds
    /// the plaintext. Used for zero-data-retention flex requests, with the
    /// request's ZDR key. The token's hash is over the sealed object, not the
    /// plaintext, so the object key cannot confirm a known image either.
    async fn ingest_sealed(&self, _input: ImageInput, _key: &[u8]) -> Result<IngestResult, NormalizeError> {
        Err(NormalizeError::BadInput("image normalisation is disabled".into()))
    }

    /// The raw sealed object for `token`, or `None` if the stored object is a
    /// plaintext image. Decrypt with [`sealed::open`].
    async fn read_sealed(&self, token: ImageToken) -> Result<Option<Bytes>, NormalizeError> {
        let (_, bytes) = self.read(token).await?;
        Ok(sealed::is_sealed(&bytes).then_some(bytes))
    }

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
    /// See [`ImageNormalizerConfig::unique_upload_keys`].
    unique_upload_keys: bool,
}

impl<S: ImageStore> DefaultImageNormalizer<S> {
    pub fn new(fetcher_cfg: FetcherConfig, store: Arc<S>) -> Self {
        Self {
            fetcher: fetcher::ImageFetcher::new(fetcher_cfg),
            store,
            unique_upload_keys: false,
        }
    }

    /// Give every ingest its own object key (see
    /// [`ImageNormalizerConfig::unique_upload_keys`]).
    pub fn with_unique_upload_keys(mut self, enabled: bool) -> Self {
        self.unique_upload_keys = enabled;
        self
    }
}

#[async_trait]
impl<S: ImageStore + 'static> ImageNormalizer for DefaultImageNormalizer<S> {
    async fn ingest(&self, input: ImageInput) -> Result<IngestResult, NormalizeError> {
        let (mime, bytes) = self.load(input).await?;
        let bytes_len = bytes.len() as u64;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let digest = hasher.finalize();
        let mut sha = [0u8; 32];
        sha.copy_from_slice(&digest);
        let token = if self.unique_upload_keys {
            // The content hash stays the image's identity (access grants,
            // prompt cache); the random upload ID gives this upload its own
            // object key, so writes never contend on a shared key and need no
            // existence check — ingest is a single PUT.
            let token = ImageToken::new_unique(sha);
            self.store.put(token, &mime, bytes).await?;
            token
        } else {
            // Content-addressed: reuse the existing object on a dedup hit.
            // exists() also reports an object the bucket lifecycle is about to
            // expire as absent, so the re-upload here resets its age before a
            // batch signs a reference to it.
            let token = ImageToken(sha, None);
            // The check only saves a write: if it fails, the PUT below stores
            // the same bytes under the same key anyway.
            let exists = match self.store.exists(token).await {
                Ok(exists) => exists,
                Err(e) => {
                    tracing::warn!(error = %e, "image existence check failed, uploading instead");
                    false
                }
            };
            if !exists {
                self.store.put(token, &mime, bytes).await?;
            }
            token
        };
        Ok(IngestResult { token, mime, bytes_len })
    }

    async fn load(&self, input: ImageInput) -> Result<(String, Bytes), NormalizeError> {
        let (mime, bytes) = match input {
            ImageInput::HttpUrl(url) => {
                let fetched = self.fetcher.fetch(&url).await?;
                (fetched.mime, fetched.bytes)
            }
            ImageInput::DataUri(uri) => {
                // Bound the encoded payload before decoding it, so an oversized
                // `data:` URI is refused without allocating its decoded size.
                let max_encoded_len = self.fetcher.max_bytes().div_ceil(3).saturating_mul(4);
                if uri
                    .split_once(',')
                    .is_some_and(|(_, payload)| payload.len() as u64 > max_encoded_len)
                {
                    return Err(NormalizeError::BadInput(format!(
                        "data: URI payload exceeds cap {}",
                        self.fetcher.max_bytes()
                    )));
                }
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
        // No image format starts with the sealed-object prefix. Refusing it here
        // keeps that prefix an unambiguous marker: a stored plaintext image can
        // never be mistaken for a sealed one.
        if sealed::is_sealed(&bytes) {
            return Err(NormalizeError::BadInput("payload is not a supported image".into()));
        }
        Ok((mime, bytes))
    }

    async fn ingest_sealed(&self, input: ImageInput, key: &[u8]) -> Result<IngestResult, NormalizeError> {
        let (mime, bytes) = self.load(input).await?;
        let bytes_len = bytes.len() as u64;
        let blob = sealed::seal(key, &mime, &bytes).map_err(|e| NormalizeError::StoreFailed(format!("sealing image failed: {e}")))?;
        let sha: [u8; 32] = Sha256::digest(&blob).into();
        // Always its own object: a sealed object is readable only with this
        // request's key, so it can never be shared with another upload.
        let token = ImageToken::new_unique(sha);
        self.store.put(token, sealed::SEALED_CONTENT_TYPE, Bytes::from(blob)).await?;
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
            Arc::new(DefaultImageNormalizer::new(cfg.fetcher.clone(), store).with_unique_upload_keys(cfg.unique_upload_keys))
        }
        BackendConfig::Gcs { bucket, region } => {
            let store = Arc::new(store::GcsStore::new(bucket.clone(), region.clone()));
            Arc::new(DefaultImageNormalizer::new(cfg.fetcher.clone(), store).with_unique_upload_keys(cfg.unique_upload_keys))
        }
        BackendConfig::S3Compatible {
            bucket,
            endpoint_url,
            region,
            force_path_style,
            reuse_max_age_secs,
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
            let store = Arc::new(
                store::S3CompatStore::new(
                    bucket.clone(),
                    endpoint_url.clone(),
                    region.clone(),
                    *force_path_style,
                    access_key_id,
                    secret_access_key,
                )
                .with_reuse_max_age((*reuse_max_age_secs > 0).then(|| Duration::from_secs(*reuse_max_age_secs))),
            );
            Arc::new(DefaultImageNormalizer::new(cfg.fetcher.clone(), store).with_unique_upload_keys(cfg.unique_upload_keys))
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
        assert_eq!(token.1, None, "unique_upload_keys is off by default: content-addressed token");

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
    async fn ingest_with_unique_upload_keys_stores_each_ingest_separately() {
        let store = Arc::new(MemoryStore::new());
        let n = DefaultImageNormalizer::new(FetcherConfig::default(), store).with_unique_upload_keys(true);
        let a = n.ingest(ImageInput::DataUri(TINY_PNG_DATA_URI.to_string())).await.unwrap().token;
        let b = n.ingest(ImageInput::DataUri(TINY_PNG_DATA_URI.to_string())).await.unwrap().token;
        // Same content hash, distinct upload IDs: two objects with the same bytes.
        assert_eq!(a.0, b.0);
        assert!(a.1.is_some() && b.1.is_some());
        assert_ne!(a, b);
        assert_eq!(n.read(a).await.unwrap().1, n.read(b).await.unwrap().1);
        // Content-addressed tokens for the same image stay readable alongside.
        let legacy = DefaultImageNormalizer::new(FetcherConfig::default(), Arc::new(MemoryStore::new()));
        let t = legacy
            .ingest(ImageInput::DataUri(TINY_PNG_DATA_URI.to_string()))
            .await
            .unwrap()
            .token;
        assert_eq!(t, ImageToken(a.0, None));
    }

    /// A store whose existence check always fails, as during a brief object-store outage.
    struct FailingExistsStore(MemoryStore);

    #[async_trait]
    impl ImageStore for FailingExistsStore {
        async fn put(&self, token: ImageToken, mime: &str, bytes: Bytes) -> Result<bool, StoreError> {
            self.0.put(token, mime, bytes).await
        }
        async fn sign(&self, token: ImageToken, ttl: Duration) -> Result<SignedImageUrl, StoreError> {
            self.0.sign(token, ttl).await
        }
        async fn read(&self, token: ImageToken) -> Result<(String, Bytes), StoreError> {
            self.0.read(token).await
        }
        async fn exists(&self, _token: ImageToken) -> Result<bool, StoreError> {
            Err(StoreError::Backend("HEAD: service unavailable".into()))
        }
    }

    #[tokio::test]
    async fn ingest_uploads_when_existence_check_fails() {
        let n = DefaultImageNormalizer::new(FetcherConfig::default(), Arc::new(FailingExistsStore(MemoryStore::new())));
        let token = n.ingest(ImageInput::DataUri(TINY_PNG_DATA_URI.to_string())).await.unwrap().token;
        assert_eq!(token.1, None);
        assert_eq!(n.read(token).await.unwrap().0, "image/png");
    }

    #[tokio::test]
    async fn ingest_sealed_never_stores_plaintext() {
        let store = Arc::new(MemoryStore::new());
        let n = DefaultImageNormalizer::new(FetcherConfig::default(), store.clone());
        let key = crate::keystore::generate_key();

        let sealed_result = n
            .ingest_sealed(ImageInput::DataUri(TINY_PNG_DATA_URI.to_string()), &key)
            .await
            .unwrap();
        assert_eq!(sealed_result.mime, "image/png");
        assert!(sealed_result.token.1.is_some(), "a sealed object is always its own upload");

        // The only stored object is ciphertext: no PNG signature anywhere in it.
        let objects = store.objects();
        assert_eq!(objects.len(), 1);
        assert!(sealed::is_sealed(&objects[0]));
        assert!(!objects[0].windows(4).any(|w| w == b"\x89PNG"));

        // The token hash is not the plaintext hash, so the object key cannot
        // confirm a known image.
        let plain = DefaultImageNormalizer::new(FetcherConfig::default(), Arc::new(MemoryStore::new()))
            .ingest(ImageInput::DataUri(TINY_PNG_DATA_URI.to_string()))
            .await
            .unwrap();
        assert_ne!(sealed_result.token.0, plain.token.0);

        // Only the key opens it.
        let blob = n.read_sealed(sealed_result.token).await.unwrap().expect("sealed object");
        let (mime, bytes) = sealed::open(&key, &blob).unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert!(sealed::open(&crate::keystore::generate_key(), &blob).is_err());
    }

    #[tokio::test]
    async fn a_payload_that_looks_sealed_is_refused() {
        // An "image" carrying the sealed prefix must never be stored as
        // plaintext, or a ZDR dispatch would try to open it as sealed.
        use base64::Engine as _;
        let mut spoof = sealed::SEALED_MAGIC.to_vec();
        spoof.extend_from_slice(b"not really sealed");
        let uri = format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(&spoof));
        let store = Arc::new(MemoryStore::new());
        let n = DefaultImageNormalizer::new(FetcherConfig::default(), store.clone());
        let err = n.ingest(ImageInput::DataUri(uri)).await.unwrap_err();
        assert!(matches!(err, NormalizeError::BadInput(_)), "got {err:?}");
        assert!(store.objects().is_empty());
    }

    #[tokio::test]
    async fn an_oversized_data_uri_is_refused_before_decoding() {
        let cfg = FetcherConfig {
            max_bytes: 8,
            ..FetcherConfig::default()
        };
        let n = DefaultImageNormalizer::new(cfg, Arc::new(MemoryStore::new()));
        // Not even valid base64: proves the length check runs before the decode.
        let uri = format!("data:image/png;base64,{}", "!".repeat(64));
        let err = n.load(ImageInput::DataUri(uri)).await.unwrap_err();
        match err {
            NormalizeError::BadInput(m) => assert!(m.contains("exceeds cap"), "{m}"),
            other => panic!("got {other:?}"),
        }
        // A payload at the cap still decodes.
        let ok = format!("data:image/png;base64,{}", "AAAAAAAAAAA=");
        assert!(n.load(ImageInput::DataUri(ok)).await.is_ok());
    }

    #[tokio::test]
    async fn read_sealed_is_none_for_plaintext_objects() {
        let n = DefaultImageNormalizer::new(FetcherConfig::default(), Arc::new(MemoryStore::new()));
        let token = n.ingest(ImageInput::DataUri(TINY_PNG_DATA_URI.to_string())).await.unwrap().token;
        assert!(n.read_sealed(token).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn load_validates_without_storing() {
        let store = Arc::new(MemoryStore::new());
        let n = DefaultImageNormalizer::new(FetcherConfig::default(), store.clone());
        let (mime, bytes) = n.load(ImageInput::DataUri(TINY_PNG_DATA_URI.to_string())).await.unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert!(store.objects().is_empty());
        // Same MIME policy as ingest.
        let err = n
            .load(ImageInput::DataUri("data:text/html;base64,PGgxPmhpPC9oMT4=".to_string()))
            .await
            .unwrap_err();
        assert!(matches!(err, NormalizeError::BadInput(_)), "got {err:?}");
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
            unique_upload_keys: false,
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
            unique_upload_keys: false,
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
