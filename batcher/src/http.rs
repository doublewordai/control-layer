//! HTTP client abstraction for making requests.
//!
//! This module defines the `HttpClient` trait to abstract HTTP request execution,
//! enabling testability with mock implementations.

use crate::error::Result;
use crate::types::RequestData;
use async_trait::async_trait;
use std::time::Duration;

/// Response from an HTTP request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status code
    pub status: u16,
    /// Response body as a string
    pub body: String,
}

/// Trait for executing HTTP requests.
///
/// This abstraction allows for different implementations (production vs. testing)
/// and makes the daemon processing logic testable without making real HTTP calls.
///
/// # Example
/// ```ignore
/// let client = ReqwestHttpClient::new();
/// let response = client.execute(&request_data, "api-key", 5000).await?;
/// println!("Status: {}, Body: {}", response.status, response.body);
/// ```
#[async_trait]
pub trait HttpClient: Send + Sync + Clone {
    /// Execute an HTTP request.
    ///
    /// # Arguments
    /// * `request` - The request data containing endpoint, method, path, and body
    /// * `api_key` - API key to include in Authorization: Bearer header
    /// * `timeout_ms` - Request timeout in milliseconds
    ///
    /// # Errors
    /// Returns an error if:
    /// - The request fails due to network issues
    /// - The request times out
    /// - The URL is invalid
    async fn execute(
        &self,
        request: &RequestData,
        api_key: &str,
        timeout_ms: u64,
    ) -> Result<HttpResponse>;
}

// ============================================================================
// Production Implementation using reqwest
// ============================================================================

/// Production HTTP client using reqwest.
///
/// This implementation makes real HTTP requests to external endpoints.
#[derive(Clone)]
pub struct ReqwestHttpClient {
    client: reqwest::Client,
}

impl ReqwestHttpClient {
    /// Create a new reqwest-based HTTP client.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for ReqwestHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HttpClient for ReqwestHttpClient {
    async fn execute(
        &self,
        request: &RequestData,
        api_key: &str,
        timeout_ms: u64,
    ) -> Result<HttpResponse> {
        let url = format!("{}{}", request.endpoint, request.path);

        let req = self
            .client
            .request(
                request.method.parse().map_err(|e| {
                    anyhow::anyhow!("Invalid HTTP method '{}': {}", request.method, e)
                })?,
                &url,
            )
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .body(request.body.clone())
            .timeout(Duration::from_millis(timeout_ms));

        let response = req.send().await?;
        let status = response.status().as_u16();
        let body = response.text().await?;

        Ok(HttpResponse { status, body })
    }
}

// ============================================================================
// Test/Mock Implementation
// ============================================================================

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

/// Mock HTTP client for testing.
///
/// Allows configuring predetermined responses for specific requests without
/// making actual HTTP calls.
///
/// # Example
/// ```ignore
/// let mock = MockHttpClient::new();
/// mock.add_response(
///     "POST /v1/chat/completions",
///     HttpResponse {
///         status: 200,
///         body: r#"{"result": "success"}"#.to_string(),
///     },
/// );
/// ```
#[derive(Clone)]
pub struct MockHttpClient {
    responses: Arc<Mutex<HashMap<String, Vec<Result<HttpResponse>>>>>,
    calls: Arc<Mutex<Vec<MockCall>>>,
}

/// Record of a call made to the mock HTTP client.
#[derive(Debug, Clone)]
pub struct MockCall {
    pub method: String,
    pub endpoint: String,
    pub path: String,
    pub body: String,
    pub api_key: String,
    pub timeout_ms: u64,
}

impl MockHttpClient {
    /// Create a new mock HTTP client.
    pub fn new() -> Self {
        Self {
            responses: Arc::new(Mutex::new(HashMap::new())),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Add a predetermined response for a specific method and path.
    ///
    /// The key is formatted as "{method} {path}". Multiple responses can be
    /// added for the same key - they will be returned in FIFO order.
    pub fn add_response(&self, key: &str, response: Result<HttpResponse>) {
        self.responses
            .lock()
            .entry(key.to_string())
            .or_default()
            .push(response);
    }

    /// Get all calls that have been made to this mock client.
    pub fn get_calls(&self) -> Vec<MockCall> {
        self.calls.lock().clone()
    }

    /// Clear all recorded calls.
    pub fn clear_calls(&self) {
        self.calls.lock().clear();
    }

    /// Get the number of calls made.
    pub fn call_count(&self) -> usize {
        self.calls.lock().len()
    }
}

impl Default for MockHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HttpClient for MockHttpClient {
    async fn execute(
        &self,
        request: &RequestData,
        api_key: &str,
        timeout_ms: u64,
    ) -> Result<HttpResponse> {
        // Record this call
        self.calls.lock().push(MockCall {
            method: request.method.clone(),
            endpoint: request.endpoint.clone(),
            path: request.path.clone(),
            body: request.body.clone(),
            api_key: api_key.to_string(),
            timeout_ms,
        });

        // Look up the response
        let key = format!("{} {}", request.method, request.path);
        let mut responses = self.responses.lock();

        if let Some(response_queue) = responses.get_mut(&key) {
            if !response_queue.is_empty() {
                return response_queue.remove(0);
            }
        }

        // No response configured - return a default error
        Err(crate::error::BatcherError::Other(anyhow::anyhow!(
            "No mock response configured for {} {}",
            request.method,
            request.path
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_client_basic() {
        let mock = MockHttpClient::new();
        mock.add_response(
            "POST /test",
            Ok(HttpResponse {
                status: 200,
                body: "success".to_string(),
            }),
        );

        let request = RequestData {
            id: uuid::Uuid::new_v4(),
            endpoint: "https://api.example.com".to_string(),
            method: "POST".to_string(),
            path: "/test".to_string(),
            body: "{}".to_string(),
            model: "test-model".to_string(),
        };

        let response = mock.execute(&request, "test-key", 5000).await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "success");

        // Verify call was recorded
        let calls = mock.get_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, "POST");
        assert_eq!(calls[0].path, "/test");
        assert_eq!(calls[0].api_key, "test-key");
    }

    #[tokio::test]
    async fn test_mock_client_multiple_responses() {
        let mock = MockHttpClient::new();
        mock.add_response(
            "GET /status",
            Ok(HttpResponse {
                status: 200,
                body: "first".to_string(),
            }),
        );
        mock.add_response(
            "GET /status",
            Ok(HttpResponse {
                status: 200,
                body: "second".to_string(),
            }),
        );

        let request = RequestData {
            id: uuid::Uuid::new_v4(),
            endpoint: "https://api.example.com".to_string(),
            method: "GET".to_string(),
            path: "/status".to_string(),
            body: "".to_string(),
            model: "test-model".to_string(),
        };

        let response1 = mock.execute(&request, "key", 5000).await.unwrap();
        assert_eq!(response1.body, "first");

        let response2 = mock.execute(&request, "key", 5000).await.unwrap();
        assert_eq!(response2.body, "second");

        assert_eq!(mock.call_count(), 2);
    }

    #[tokio::test]
    async fn test_mock_client_no_response() {
        let mock = MockHttpClient::new();

        let request = RequestData {
            id: uuid::Uuid::new_v4(),
            endpoint: "https://api.example.com".to_string(),
            method: "POST".to_string(),
            path: "/unknown".to_string(),
            body: "{}".to_string(),
            model: "test-model".to_string(),
        };

        let result = mock.execute(&request, "key", 5000).await;
        assert!(result.is_err());
    }
}
