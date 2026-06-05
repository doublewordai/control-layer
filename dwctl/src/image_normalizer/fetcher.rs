//! Hardened HTTP fetch for user-supplied image URLs.
//!
//! Defence in depth on top of the cluster-level NetworkPolicy egress
//! deny-list. The fetcher itself:
//!
//! - Resolves the hostname's IP and validates against the deny-list
//!   ([`super::ip_filter`]) **before** opening the connection.
//! - Pins DNS for the request by connecting to the resolved IP directly,
//!   removing the DNS-rebinding window.
//! - Manually handles HTTP 3xx redirects, re-validating each hop's URL
//!   and re-resolving DNS. Capped at [`FetcherConfig::max_redirects`].
//! - Bounds the response body with `.take(max_bytes)`.
//! - Verifies the response `Content-Type` against
//!   [`FetcherConfig::allowed_mime`].
//! - Applies a total timeout from [`FetcherConfig::timeout_secs`].
//! - Retries only on transient errors (timeout, connect error, DNS
//!   failure, origin 5xx) with capped exponential backoff. Permanent
//!   errors surface immediately.
//!
//! The fetcher's `reqwest::Client` is constructed without any
//! instrumentation middleware so it can't inadvertently leak trace
//! context to attacker-controlled hosts via the `traceparent` header.
use bytes::Bytes;
use rand::RngExt;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, warn};
use url::Url;

use super::config::FetcherConfig;
use super::ip_filter;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// Caller's URL is unacceptable: bad scheme, denied IP, oversize,
    /// MIME mismatch, redirect-cap exceeded, etc. Never retried.
    #[error("bad input: {0}")]
    BadInput(String),
    /// Non-retryable upstream error (e.g. 4xx). Surfaces immediately.
    #[error("fetch failed: {0}")]
    FetchFailed(String),
    /// Retried up to the configured limit and still failing. Indicates
    /// transient origin or network issues.
    #[error("transient failure after retries: {0}")]
    Transient(String),
}

/// Result of a successful fetch.
#[derive(Debug)]
pub struct FetchedImage {
    pub mime: String,
    pub bytes: Bytes,
}

/// Hardened fetcher. Cheap to clone (shares an inner `Arc`).
#[derive(Clone)]
pub struct ImageFetcher {
    config: Arc<FetcherConfig>,
}

impl ImageFetcher {
    pub fn new(config: FetcherConfig) -> Self {
        Self { config: Arc::new(config) }
    }

    /// Maximum decoded payload size in bytes. Shared cap so the `data:` URI
    /// ingest path can enforce the same limit the HTTP fetch path does.
    pub fn max_bytes(&self) -> u64 {
        self.config.max_bytes
    }

    /// Whether `mime` is in the configured allow-list. Shared so the `data:`
    /// URI ingest path applies the same MIME policy as the HTTP fetch path.
    pub fn mime_allowed(&self, mime: &str) -> bool {
        self.config.mime_allowed(mime)
    }

    /// Fetch the bytes at `url`. Performs retries internally; the returned
    /// error is final.
    pub async fn fetch(&self, url: &str) -> Result<FetchedImage, FetchError> {
        let attempts = self.config.max_retries as usize + 1;
        let mut last_err: Option<FetchError> = None;
        for attempt in 0..attempts {
            if attempt > 0 {
                let base = self.config.retry_base_delay();
                // Exponential backoff: 1x, 2x, 4x, …, saturating at
                // u32::MAX-multiplied for very large `attempt` (saturating
                // arithmetic on Duration prevents overflow further on).
                let multiplier = 1u32.checked_shl(attempt as u32 - 1).unwrap_or(u32::MAX);
                let mut delay = base.saturating_mul(multiplier);
                // Add 0..20% positive jitter to spread retry hammers from
                // many concurrent failures across time. Positive-only
                // (rather than ±10%) so the backoff never undershoots the
                // base delay.
                let jitter_ms = {
                    let mut rng = rand::rng();
                    rng.random_range(0..(delay.as_millis() as u64 / 5 + 1))
                };
                delay = delay.saturating_add(Duration::from_millis(jitter_ms));
                debug!(attempt, ?delay, "image fetcher retry sleeping");
                sleep(delay).await;
            }
            match self.fetch_once(url).await {
                Ok(image) => return Ok(image),
                Err(e @ FetchError::BadInput(_)) => return Err(e),
                Err(e @ FetchError::FetchFailed(_)) => return Err(e),
                Err(e @ FetchError::Transient(_)) => {
                    warn!(error = %e, attempt, "image fetch transient failure");
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| FetchError::Transient("no attempts made".into())))
    }

    async fn fetch_once(&self, original_url: &str) -> Result<FetchedImage, FetchError> {
        // Parse URL and walk redirects manually so each hop is validated.
        let mut url = Url::parse(original_url).map_err(|e| FetchError::BadInput(format!("invalid url: {e}")))?;

        for hop in 0..=self.config.max_redirects {
            if !matches!(url.scheme(), "http" | "https") {
                return Err(FetchError::BadInput(format!("unsupported scheme: {}", url.scheme())));
            }
            let host = url.host_str().ok_or_else(|| FetchError::BadInput("missing host".into()))?;
            let port = url
                .port_or_known_default()
                .ok_or_else(|| FetchError::BadInput("unknown port".into()))?;
            let resolved = resolve_first_allowed(host, port).await?;

            // Build a per-hop reqwest client that:
            //  - manually disables auto-redirects (we walk them ourselves)
            //  - resolves the hostname to the IP we already vetted (DNS pinning)
            //  - applies the total timeout
            //  - carries no instrumentation
            //
            // Rebuilding the client on each redirect hop is deliberate, not an
            // oversight: each hop re-resolves DNS and pins to a freshly-vetted
            // IP, which is what defeats DNS-rebinding across a redirect chain.
            // The cost (a connection pool allocation per hop) is bounded by
            // `max_redirects` (default 3) and dwarfed by the network round-trip,
            // so it's negligible for this path.
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(self.config.timeout())
                .resolve(host, resolved)
                .user_agent("dwctl-image-fetcher/1.0")
                .build()
                .map_err(|e| FetchError::Transient(format!("client build: {e}")))?;

            let resp = match client.get(url.clone()).send().await {
                Ok(r) => r,
                Err(e) if e.is_timeout() || e.is_connect() => {
                    return Err(FetchError::Transient(format!("connect/timeout: {e}")));
                }
                Err(e) => return Err(FetchError::FetchFailed(format!("send: {e}"))),
            };

            let status = resp.status();
            if status.is_redirection() {
                let Some(location) = resp.headers().get(reqwest::header::LOCATION).and_then(|h| h.to_str().ok()) else {
                    return Err(FetchError::FetchFailed(format!("{status} without Location header")));
                };
                let next = url
                    .join(location)
                    .map_err(|e| FetchError::BadInput(format!("bad redirect target: {e}")))?;
                if hop == self.config.max_redirects {
                    return Err(FetchError::BadInput(format!(
                        "too many redirects (cap {})",
                        self.config.max_redirects
                    )));
                }
                debug!(?next, "image fetcher following redirect");
                url = next;
                continue;
            }

            if status.is_server_error() {
                return Err(FetchError::Transient(format!("origin {status}")));
            }
            if !status.is_success() {
                return Err(FetchError::FetchFailed(format!("origin {status}")));
            }

            // Validate Content-Type.
            let mime = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|h| h.to_str().ok())
                .map(|s| s.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
                .unwrap_or_default();
            if mime.is_empty() {
                return Err(FetchError::BadInput("missing Content-Type".into()));
            }
            if !self.config.mime_allowed(&mime) {
                return Err(FetchError::BadInput(format!("mime not allowed: {mime}")));
            }

            // Early reject on advertised Content-Length over cap.
            if let Some(len) = resp.content_length()
                && len > self.config.max_bytes
            {
                return Err(FetchError::BadInput(format!(
                    "content-length {} exceeds cap {}",
                    len, self.config.max_bytes
                )));
            }

            // Read with bounded reader so an upstream lying about length
            // can still be capped.
            let max = self.config.max_bytes as usize;
            let bytes = read_bounded(resp, max).await?;

            return Ok(FetchedImage { mime, bytes });
        }
        // Loop bound prevents falling out here, but keep the compiler happy.
        Err(FetchError::BadInput("redirect cap exceeded".into()))
    }
}

/// Resolve `host:port` and return the first IP that passes the deny-list.
/// Returns `BadInput` if every resolved IP is denied; `Transient` if DNS
/// itself fails.
async fn resolve_first_allowed(host: &str, port: u16) -> Result<SocketAddr, FetchError> {
    let lookup = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| FetchError::Transient(format!("dns resolve {host}: {e}")))?;
    for addr in lookup {
        let ip: IpAddr = addr.ip();
        if !ip_filter::is_denied(ip) {
            return Ok(addr);
        }
    }
    Err(FetchError::BadInput(format!(
        "all resolved addresses for {host} are in the deny-list"
    )))
}

/// Read up to `max` bytes from `resp` and refuse if more remain.
async fn read_bounded(resp: reqwest::Response, max: usize) -> Result<Bytes, FetchError> {
    use futures::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| FetchError::Transient(format!("body read: {e}")))?;
        if buf.len() + chunk.len() > max {
            return Err(FetchError::BadInput(format!("body exceeds cap {max}")));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_unsupported_scheme() {
        let f = ImageFetcher::new(FetcherConfig::default());
        let err = f.fetch("file:///etc/passwd").await.unwrap_err();
        assert!(matches!(err, FetchError::BadInput(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn rejects_loopback_after_dns_resolve() {
        // localhost resolves to 127.0.0.1 / ::1 — both denied by ip_filter.
        let f = ImageFetcher::new(FetcherConfig::default());
        let err = f.fetch("http://localhost:9999/x.png").await.unwrap_err();
        assert!(matches!(err, FetchError::BadInput(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn rejects_link_local_metadata_ip_literal() {
        // 169.254.169.254 is the cloud-metadata IP.
        let f = ImageFetcher::new(FetcherConfig::default());
        let err = f.fetch("http://169.254.169.254/latest/meta-data/").await.unwrap_err();
        assert!(matches!(err, FetchError::BadInput(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn rejects_rfc1918_ip_literal() {
        let f = ImageFetcher::new(FetcherConfig::default());
        let err = f.fetch("http://10.0.0.1/x.png").await.unwrap_err();
        assert!(matches!(err, FetchError::BadInput(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn rejects_malformed_url() {
        let f = ImageFetcher::new(FetcherConfig::default());
        let err = f.fetch("not a url").await.unwrap_err();
        assert!(matches!(err, FetchError::BadInput(_)), "got {err:?}");
    }
}
