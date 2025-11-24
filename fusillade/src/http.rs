//! HTTP client abstraction for making requests.
//!
//! This module defines the `HttpClient` trait to abstract HTTP request execution,
//! enabling testability with mock implementations.

use crate::error::Result;
use crate::types::RequestData;
use async_trait::async_trait;
use std::time::Duration;

/// Response from an HTTP request.
/// TODO: How will we deal with streaming responses? Right now we buffer the whole response before
/// writing it back
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
    /// TODO: Why have this and default
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
    // TODO: document
    #[tracing::instrument(skip(self, request, api_key), fields(request_id = %request.id, method = %request.method, model = %request.model))]
    async fn execute(
        &self,
        request: &RequestData,
        api_key: &str,
        timeout_ms: u64,
    ) -> Result<HttpResponse> {
        let url = format!("{}{}", request.endpoint, request.path);

        tracing::debug!(
            url = %url,
            timeout_ms = timeout_ms,
            "Executing HTTP request"
        );

        let mut req = self
            .client
            .request(
                request.method.parse().map_err(|e| {
                    tracing::error!(method = %request.method, error = %e, "Invalid HTTP method");
                    anyhow::anyhow!("Invalid HTTP method '{}': {}", request.method, e)
                })?,
                &url,
            )
            .timeout(Duration::from_millis(timeout_ms));

        // Only add Authorization header if api_key is not empty
        if !api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", api_key));
            tracing::trace!(request_id = %request.id, "Added Authorization header");
        }

        // Only add body and Content-Type for methods that support a body
        let method_upper = request.method.to_uppercase();
        if method_upper != "GET"
            && method_upper != "HEAD"
            && method_upper != "DELETE"
            && !request.body.is_empty()
        {
            req = req
                .header("Content-Type", "application/json")
                .body(request.body.clone());
            tracing::trace!(
                request_id = %request.id,
                body_len = request.body.len(),
                "Added request body"
            );
        }

        let response = req.send().await.map_err(|e| {
            tracing::error!(
                request_id = %request.id,
                url = %url,
                error = %e,
                "HTTP request failed"
            );
            e
        })?;

        let status = response.status().as_u16();
        let body = response.text().await?;

        tracing::info!(
            request_id = %request.id,
            status = status,
            response_len = body.len(),
            "HTTP request completed"
        );

        Ok(HttpResponse { status, body })
    }
}

// ============================================================================
// Test/Mock Implementation
// ============================================================================

// TODO: this should be a separate file within an http/ module.
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::oneshot;

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
    responses: Arc<Mutex<HashMap<String, Vec<MockResponse>>>>,
    calls: Arc<Mutex<Vec<MockCall>>>,
    in_flight: Arc<AtomicUsize>,
}

/// A mock response that can optionally wait for a trigger before completing.
enum MockResponse {
    /// Immediate response
    Immediate(Result<HttpResponse>),
    /// Response that waits for a trigger signal before completing
    Triggered {
        response: Result<HttpResponse>,
        trigger: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    },
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
            in_flight: Arc::new(AtomicUsize::new(0)),
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
            .push(MockResponse::Immediate(response));
    }

    /// Add a response that will wait for a manual trigger before completing.
    ///
    /// Returns a sender that when triggered (by sending `()` or dropping) will
    /// cause the HTTP request to complete with the given response.
    ///
    /// # Example
    /// ```ignore
    /// let trigger = mock.add_response_with_trigger(
    ///     "POST /test",
    ///     Ok(HttpResponse { status: 200, body: "ok".to_string() })
    /// );
    /// // ... request is now blocked waiting ...
    /// trigger.send(()).unwrap(); // Now it completes
    /// ```
    pub fn add_response_with_trigger(
        &self,
        key: &str,
        response: Result<HttpResponse>,
    ) -> oneshot::Sender<()> {
        let (tx, rx) = oneshot::channel();
        self.responses
            .lock()
            .entry(key.to_string())
            .or_default()
            .push(MockResponse::Triggered {
                response,
                trigger: Arc::new(Mutex::new(Some(rx))),
            });
        tx
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

    /// Get the number of requests currently in-flight (executing).
    ///
    /// This is useful for testing cancellation - if a request is aborted,
    /// the in-flight count will decrease.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }
}

impl Default for MockHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HttpClient for MockHttpClient {
    // TODO: document
    async fn execute(
        &self,
        request: &RequestData,
        api_key: &str,
        timeout_ms: u64,
    ) -> Result<HttpResponse> {
        // Increment in-flight counter
        self.in_flight.fetch_add(1, Ordering::SeqCst);

        // Guard to ensure we decrement even if cancelled/panicked
        let in_flight = self.in_flight.clone();
        let _guard = InFlightGuard { in_flight };

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
        let mock_response = {
            let mut responses = self.responses.lock();
            if let Some(response_queue) = responses.get_mut(&key) {
                if !response_queue.is_empty() {
                    Some(response_queue.remove(0))
                } else {
                    None
                }
            } else {
                None
            }
        };

        match mock_response {
            Some(MockResponse::Immediate(response)) => response,
            Some(MockResponse::Triggered { response, trigger }) => {
                // Wait for the trigger signal before returning the response
                let rx = {
                    let mut trigger_guard = trigger.lock();
                    trigger_guard.take()
                };

                if let Some(rx) = rx {
                    // Wait for trigger (ignore the result - we proceed either way)
                    let _ = rx.await;
                }

                response
            }
            None => {
                // No response configured - return a default error
                Err(crate::error::FusilladeError::Other(anyhow::anyhow!(
                    "No mock response configured for {} {}",
                    request.method,
                    request.path
                )))
            }
        }
    }
}

/// Guard that decrements the in-flight counter when dropped.
/// This ensures the counter is decremented even if the task is cancelled or panics.
struct InFlightGuard {
    in_flight: Arc<AtomicUsize>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RequestId;

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
            id: RequestId::from(uuid::Uuid::new_v4()),
            batch_id: crate::batch::BatchId::from(uuid::Uuid::new_v4()),
            template_id: crate::batch::TemplateId::from(uuid::Uuid::new_v4()),
            custom_id: None,
            endpoint: "https://api.example.com".to_string(),
            method: "POST".to_string(),
            path: "/test".to_string(),
            body: "{}".to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
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
            id: RequestId::from(uuid::Uuid::new_v4()),
            batch_id: crate::batch::BatchId::from(uuid::Uuid::new_v4()),
            template_id: crate::batch::TemplateId::from(uuid::Uuid::new_v4()),
            custom_id: None,
            endpoint: "https://api.example.com".to_string(),
            method: "GET".to_string(),
            path: "/status".to_string(),
            body: "".to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
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
            id: RequestId::from(uuid::Uuid::new_v4()),
            batch_id: crate::batch::BatchId::from(uuid::Uuid::new_v4()),
            template_id: crate::batch::TemplateId::from(uuid::Uuid::new_v4()),
            custom_id: None,
            endpoint: "https://api.example.com".to_string(),
            method: "POST".to_string(),
            path: "/unknown".to_string(),
            body: "{}".to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
        };

        let result = mock.execute(&request, "key", 5000).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_mock_client_with_trigger() {
        let mock = MockHttpClient::new();

        let trigger = mock.add_response_with_trigger(
            "POST /test",
            Ok(HttpResponse {
                status: 200,
                body: "triggered".to_string(),
            }),
        );

        let request = RequestData {
            id: RequestId::from(uuid::Uuid::new_v4()),
            batch_id: crate::batch::BatchId::from(uuid::Uuid::new_v4()),
            template_id: crate::batch::TemplateId::from(uuid::Uuid::new_v4()),
            custom_id: None,
            endpoint: "https://api.example.com".to_string(),
            method: "POST".to_string(),
            path: "/test".to_string(),
            body: "{}".to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
        };

        // Spawn the request execution (it will block waiting for trigger)
        let mock_clone = mock.clone();
        let handle = tokio::spawn(async move { mock_clone.execute(&request, "key", 5000).await });

        // Give it a moment to start executing
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Verify it hasn't completed yet
        assert!(!handle.is_finished());

        // Now trigger the response
        trigger.send(()).unwrap();

        // Wait for completion
        let response = handle.await.unwrap().unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "triggered");
    }
}
