//! HTTP client abstraction for forwarding requests to upstream services
//!
//! This module provides a unified interface for making HTTP requests, allowing
//! different client implementations (hyper, mock clients for testing, etc.) to
//! be used interchangeably throughout the proxy.
use std::time::Duration;

use async_trait::async_trait;
use axum::response::IntoResponse;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};

pub type HyperClient = Client<
    hyper_tls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    axum::body::Body,
>;

#[async_trait]
pub trait HttpClient: std::fmt::Debug {
    async fn request(
        &self,
        req: axum::extract::Request,
    ) -> Result<axum::response::Response, Box<dyn std::error::Error + Send + Sync>>;
}

#[async_trait]
impl HttpClient for HyperClient {
    async fn request(
        &self,
        req: axum::extract::Request,
    ) -> Result<axum::response::Response, Box<dyn std::error::Error + Send + Sync>> {
        self.request(req)
            .await
            .map(|res| res.into_response())
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }
}

/// Create a hyper HTTP client with connection pooling configured
///
/// Connection pooling reuses HTTP connections instead of creating new ones for each request,
/// dramatically reducing file descriptor usage and eliminating TIME_WAIT connection accumulation.
///
/// See README for performance tuning guidance based on deployment scenarios.
pub fn create_hyper_client(
    pool_max_idle_per_host: usize,
    pool_idle_timeout_secs: u64,
) -> HyperClient {
    let mut http_connector = hyper_util::client::legacy::connect::HttpConnector::new();

    // Allow HTTPS URIs (HttpConnector enforces HTTP-only by default)
    http_connector.enforce_http(false);

    // Send TCP keepalive probes to detect dead connections.
    // After 60s idle, send a probe every 15s (Linux default); give up after 3
    // failures (Linux default). This keeps conntrack entries alive and detects
    // zombies within ~105s.
    http_connector.set_keepalive(Some(Duration::from_secs(60)));

    let https = hyper_tls::HttpsConnector::new_with_connector(http_connector);

    tracing::info!(
        "Creating HTTP client with connection pool: max_idle_per_host={}, idle_timeout={}s, tcp_keepalive=60s",
        pool_max_idle_per_host,
        pool_idle_timeout_secs
    );

    Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(pool_idle_timeout_secs))
        .pool_max_idle_per_host(pool_max_idle_per_host)
        .pool_timer(hyper_util::rt::TokioTimer::new())
        .build(https)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_hyper_client_accepts_https_uris() {
        // Test that create_hyper_client() produces a client that accepts HTTPS URIs
        // Bug: HttpConnector with enforce_http=true (default) rejects HTTPS schemes
        // This test uses a mock server to avoid external network dependencies

        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Start a local mock server
        let mock_server = MockServer::start().await;

        // Configure mock to return 200 OK
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let client = create_hyper_client(10, 60);

        // Create a request with an HTTPS URI to the mock server
        // Note: wiremock gives us an HTTP URL, but we can test HTTPS URI parsing separately
        let http_uri: hyper::Uri = format!("{}/test", mock_server.uri()).parse().unwrap();
        let http_request = axum::extract::Request::builder()
            .uri(http_uri)
            .method("GET")
            .body(axum::body::Body::empty())
            .unwrap();

        // First verify HTTP works
        let result = client.request(http_request).await;
        assert!(result.is_ok(), "HTTP request should work");

        // Now test that HTTPS URIs are accepted (won't connect, but shouldn't be rejected for scheme)
        // With enforce_http=true, this would fail immediately with "invalid URI" or similar
        // With enforce_http=false, it will fail with connection error (expected - no HTTPS server)
        let https_uri: hyper::Uri = "https://localhost:1/test".parse().unwrap();
        let https_request = axum::extract::Request::builder()
            .uri(https_uri)
            .method("GET")
            .body(axum::body::Body::empty())
            .unwrap();

        let result = client.request(https_request).await;

        // Should fail with connection error (no server at localhost:1), NOT scheme rejection
        // If enforce_http=true, hyper would reject the HTTPS scheme before attempting connection
        if let Err(e) = result {
            let error_string = e.to_string().to_lowercase();
            // Connection errors are expected and valid
            // Scheme/URI errors indicate enforce_http is blocking HTTPS
            assert!(
                !error_string.contains("invalid uri") && !error_string.contains("scheme"),
                "Client rejected HTTPS URI at scheme level (enforce_http not disabled): {}",
                e
            );
        }
        // If it somehow succeeds, that's also fine (means HTTPS worked)
    }
}
