//! Daemon for processing batched requests.

use crate::{Request, RequestContext, RequestId, RequestStatus, Result};
use chrono::Utc;
use dashmap::DashMap;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

/// A unique identifier for a daemon instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DaemonId(Uuid);

impl DaemonId {
    /// Create a new random daemon ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Get the underlying UUID.
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }

    /// Convert to a short, readable string format.
    pub fn to_short_string(&self) -> String {
        let hex = format!("{:x}", self.0.as_u128());
        format!("daemon_{}", &hex[..8])
    }
}

impl Default for DaemonId {
    fn default() -> Self {
        Self::new()
    }
}

impl From<Uuid> for DaemonId {
    fn from(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

impl std::fmt::Display for DaemonId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_short_string())
    }
}

/// Response from an upstream HTTP request.
#[derive(Debug, Clone)]
pub struct Response {
    /// HTTP status code
    pub status: u16,
    /// Response body
    pub body: String,
}

/// An update event for a request, emitted when the status changes.
#[derive(Debug, Clone)]
pub struct RequestUpdate {
    /// The ID of the request that was updated
    pub request_id: RequestId,
    /// The new status of the request
    pub status: RequestStatus,
}

/// Operations that the daemon needs to interact with a batcher.
///
/// These operations are separate from the main `Batcher` trait because they're
/// intended for internal daemon use, not for external clients.
pub trait DaemonOperations: Send + Sync {
    /// Poll for pending requests for a specific model and atomically claim them.
    ///
    /// Returns up to `limit` pending requests for the given model. If `model` is None,
    /// returns requests for any model. Only returns requests that are ready to be
    /// processed (i.e., not waiting for retry backoff).
    ///
    /// This method should atomically transition the returned requests from Pending to
    /// PendingProcessing, marking them with the given daemon_id and acquisition timestamp. This
    /// prevents race conditions where multiple daemons might try to process the same request.
    fn poll_pending(
        &self,
        model: Option<&str>,
        limit: usize,
        daemon_id: &str,
    ) -> impl Future<Output = Result<Vec<(RequestId, Request, RequestContext)>>> + Send;

    /// Update the status of a request.
    ///
    /// This is used by the daemon to transition requests through their lifecycle:
    /// Pending -> Processing -> Completed/Failed
    fn update_status(
        &self,
        id: RequestId,
        status: RequestStatus,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// Configuration for the daemon.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// How often to poll for new requests (in milliseconds)
    pub poll_interval_ms: u64,

    /// Maximum number of requests to fetch per poll per model
    pub batch_size: usize,

    /// Maximum number of in-flight requests per model
    pub max_in_flight_per_model: usize,

    /// Set of models this daemon is responsible for
    /// If empty, the daemon handles all models
    pub models: Vec<String>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: 1000,
            batch_size: 10,
            max_in_flight_per_model: 10,
            models: vec![], // Empty means handle all models
        }
    }
}

/// The main daemon that processes batched requests.
pub struct Daemon<T>
where
    T: DaemonOperations + 'static,
{
    batcher: Arc<T>,
    config: DaemonConfig,
    http_client: reqwest::Client,
    updates_tx: broadcast::Sender<RequestUpdate>,
    daemon_id: DaemonId,
    /// Tracks the number of in-flight requests per model
    in_flight_per_model: Arc<DashMap<String, usize>>,
}

impl<T> Daemon<T>
where
    T: DaemonOperations + 'static,
{
    /// Create a new daemon instance.
    ///
    /// The broadcast channel can buffer up to 1000 updates. If a receiver falls behind,
    /// older updates may be dropped.
    pub fn new(batcher: Arc<T>, config: DaemonConfig) -> Self {
        let (updates_tx, _) = broadcast::channel(1000); // Buffer 1000 updates

        Self {
            batcher,
            config,
            http_client: reqwest::Client::new(),
            updates_tx,
            daemon_id: DaemonId::new(),
            in_flight_per_model: Arc::new(DashMap::new()),
        }
    }

    /// Subscribe to request updates.
    ///
    /// Returns a stream that will emit `RequestUpdate` events as requests change status.
    ///
    /// # Arguments
    ///
    /// * `request_ids` - If `Some(ids)`, only emit updates for the specified requests.
    ///                   If `None`, emit updates for all requests (useful for monitoring).
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Subscribe to specific requests
    /// let stream = daemon.subscribe(Some(vec![id1, id2]));
    ///
    /// // Subscribe to all requests
    /// let stream = daemon.subscribe(None);
    /// ```
    pub fn subscribe(
        &self,
        request_ids: Option<Vec<RequestId>>,
    ) -> Pin<Box<dyn Stream<Item = RequestUpdate> + Send>> {
        let rx = self.updates_tx.subscribe();

        match request_ids {
            Some(ids) => {
                // Filter by specific request IDs
                let stream = BroadcastStream::new(rx).filter_map(move |result| match result {
                    Ok(update) if ids.contains(&update.request_id) => Some(update),
                    _ => None,
                });
                Box::pin(stream)
            }
            None => {
                // Pass through all updates
                let stream = BroadcastStream::new(rx).filter_map(|result| result.ok());
                Box::pin(stream)
            }
        }
    }

    /// Spawn the daemon as a background task.
    ///
    /// Returns a JoinHandle that can be used to await completion or cancel the daemon.
    /// This method takes `self` by value, consuming the daemon.
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(async move {
            self.run().await;
        })
    }

    /// Spawn the daemon as a background task from an Arc.
    ///
    /// This is useful when you need to retain access to the daemon (e.g., for registering callbacks).
    /// Returns a JoinHandle that can be used to await completion or cancel the daemon.
    pub fn spawn_arc(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            self.run().await;
        })
    }

    /// Main run loop for the daemon.
    #[instrument(skip(self), fields(id = %self.daemon_id))]
    async fn run(&self) {
        info!(
            "Daemon started with models: {:?}",
            if self.config.models.is_empty() {
                "all".to_string()
            } else {
                format!("{:?}", self.config.models)
            }
        );

        let mut join_set = tokio::task::JoinSet::new();

        loop {
            // Clean up completed tasks
            while let Some(result) = join_set.try_join_next() {
                if let Err(e) = result {
                    error!("Task panicked: {}", e);
                }
            }

            // Determine which models to poll
            let models_to_poll: Vec<String> = if self.config.models.is_empty() {
                // If no models configured, poll once with None (all models)
                vec![]
            } else {
                self.config.models.clone()
            };

            // Poll each model based on available capacity
            let daemon_id_str = self.daemon_id.to_string();

            if models_to_poll.is_empty() {
                // Poll for all models
                let capacity = self.config.batch_size;
                match self.batcher.poll_pending(None, capacity, &daemon_id_str).await {
                    Ok(requests) => {
                        if !requests.is_empty() {
                            debug!(
                                "Polled {} pending requests across all models",
                                requests.len()
                            );
                            self.spawn_requests(&mut join_set, requests);
                        }
                    }
                    Err(e) => {
                        error!("Error polling for requests: {}", e);
                    }
                }
            } else {
                // Poll each configured model
                for model in &models_to_poll {
                    let in_flight = self.in_flight_per_model.get(model).map(|c| *c).unwrap_or(0);
                    let capacity = self
                        .config
                        .max_in_flight_per_model
                        .saturating_sub(in_flight);

                    if capacity > 0 {
                        let limit = capacity.min(self.config.batch_size);
                        match self.batcher.poll_pending(Some(model), limit, &daemon_id_str).await {
                            Ok(requests) => {
                                if !requests.is_empty() {
                                    debug!(
                                        "Polled {} pending requests for model {}",
                                        requests.len(),
                                        model
                                    );
                                    self.spawn_requests(&mut join_set, requests);
                                }
                            }
                            Err(e) => {
                                error!("Error polling for requests for model {}: {}", model, e);
                            }
                        }
                    }
                }
            }

            // Sleep before next poll
            tokio::time::sleep(Duration::from_millis(self.config.poll_interval_ms)).await;
        }
    }

    /// Spawn requests as concurrent tasks.
    fn spawn_requests(
        &self,
        join_set: &mut tokio::task::JoinSet<()>,
        requests: Vec<(RequestId, Request, RequestContext)>,
    ) {
        for (id, request, context) in requests {
            let model = request.model.clone();

            // Increment in-flight counter
            *self.in_flight_per_model.entry(model.clone()).or_insert(0) += 1;

            // Clone Arc references for the spawned task
            let batcher = self.batcher.clone();
            let http_client = self.http_client.clone();
            let updates_tx = self.updates_tx.clone();
            let daemon_id = self.daemon_id;
            let in_flight = self.in_flight_per_model.clone();

            join_set.spawn(async move {
                // Process the request
                Self::process_request(
                    id,
                    request,
                    context,
                    batcher,
                    http_client,
                    updates_tx,
                    daemon_id,
                )
                .await;

                // Decrement the in-flight counter for this model
                if let Some(mut count) = in_flight.get_mut(&model) {
                    if *count > 0 {
                        *count -= 1;
                    }
                }
            });
        }
    }

    /// Process a single request.
    #[instrument(skip(batcher, http_client, updates_tx, context), fields(id = %id, model = %request.model))]
    async fn process_request(
        id: RequestId,
        request: Request,
        context: RequestContext,
        batcher: Arc<T>,
        http_client: reqwest::Client,
        updates_tx: broadcast::Sender<RequestUpdate>,
        daemon_id: DaemonId,
    ) {
        debug!("Processing request");

        // Helper to update status and broadcast
        let update_and_broadcast = |status: RequestStatus| async {
            if let Err(e) = batcher.update_status(id, status.clone()).await {
                error!("Failed to update status: {}", e);
                return Err(e);
            }
            let _ = updates_tx.send(RequestUpdate {
                request_id: id,
                status,
            });
            Ok(())
        };

        // Transition from PendingProcessing to Processing (HTTP request starting)
        let processing_status = RequestStatus::Processing {
            daemon_id: daemon_id.to_string(),
            acquired_at: Utc::now(),
        };

        if update_and_broadcast(processing_status).await.is_err() {
            return;
        }

        // Send HTTP request
        let result = Self::send_http_request(&request, &context, &http_client).await;

        // Update status based on result
        match result {
            Ok(response) => {
                debug!(
                    "Request completed successfully with status {}",
                    response.status
                );

                let completed_status = RequestStatus::Completed {
                    response_status: response.status,
                    response_body: response.body.clone(),
                    completed_at: Utc::now(),
                };

                let _ = update_and_broadcast(completed_status).await;
            }
            Err(e) => {
                warn!("Request failed: {}", e);

                // For now, mark as failed immediately (retry logic in Phase 7)
                let failed_status = RequestStatus::Failed {
                    error: e.to_string(),
                    retry_count: 0,
                    failed_at: Utc::now(),
                };

                let _ = update_and_broadcast(failed_status).await;
            }
        }
    }

    /// Send an HTTP request to the upstream service.
    #[instrument(skip(http_client, request, context), fields(url = %request.url(), method = %request.method))]
    async fn send_http_request(
        request: &Request,
        context: &RequestContext,
        http_client: &reqwest::Client,
    ) -> Result<Response> {
        debug!("Sending HTTP request");

        let url = request.url();
        let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|e| {
            crate::BatcherError::InvalidRequest(format!("Invalid HTTP method: {}", e))
        })?;

        let mut http_request = http_client
            .request(method.clone(), &url)
            .timeout(Duration::from_millis(context.timeout_ms));

        // Only add Authorization header if api_key is not empty
        if !request.api_key.is_empty() {
            http_request =
                http_request.header("Authorization", format!("Bearer {}", request.api_key));
        }

        // Only add body and Content-Type for methods that support body
        if method != reqwest::Method::GET && method != reqwest::Method::HEAD {
            if !request.body.is_empty() {
                http_request = http_request
                    .header("Content-Type", "application/json")
                    .body(request.body.clone());
            }
        }

        let response = http_request.send().await?;

        let status = response.status().as_u16();
        let body = response.text().await?;

        Ok(Response { status, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Batcher, InMemoryBatcher, Request, RequestContext};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn test_daemon_config_defaults() {
        let config = DaemonConfig::default();
        assert_eq!(config.poll_interval_ms, 1000);
        assert_eq!(config.batch_size, 10);
        assert_eq!(config.max_in_flight_per_model, 10);
        assert!(config.models.is_empty());
    }

    #[test]
    fn test_response_creation() {
        let response = Response {
            status: 200,
            body: r#"{"result": "success"}"#.to_string(),
        };
        assert_eq!(response.status, 200);
        assert!(response.body.contains("success"));
    }

    fn create_test_request(endpoint: &str) -> Request {
        Request {
            endpoint: endpoint.to_string(),
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            body: r#"{"model": "gpt-4", "messages": []}"#.to_string(),
            api_key: "test-key-123".to_string(),
            model: "gpt-4".to_string(),
        }
    }

    #[tokio::test]
    async fn test_daemon_processes_request() {
        // Start mock HTTP server
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("Authorization", "Bearer test-key-123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-123",
                "choices": [{"message": {"content": "Hello!"}}]
            })))
            .mount(&mock_server)
            .await;

        // Create batcher and daemon
        let batcher = Arc::new(InMemoryBatcher::new());
        let config = DaemonConfig {
            poll_interval_ms: 50, // Fast polling for tests
            batch_size: 10,
            max_in_flight_per_model: 10,
            models: vec![],
        };
        let daemon = Daemon::new(batcher.clone(), config);

        // Submit a request
        let request = create_test_request(&mock_server.uri());
        let context = RequestContext::default();
        let ids = batcher
            .submit_requests(vec![(request, context)])
            .await
            .unwrap();
        let request_id = ids[0];

        // Spawn daemon
        let handle = daemon.spawn();

        // Wait a bit for processing
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Check status
        let statuses = batcher.get_status(vec![request_id]).await.unwrap();
        assert!(matches!(statuses[0].1, RequestStatus::Completed { .. }));

        // Get response details
        if let RequestStatus::Completed {
            response_status,
            response_body,
            ..
        } = &statuses[0].1
        {
            assert_eq!(*response_status, 200);
            assert!(response_body.contains("chatcmpl-123"));
        }

        // Cleanup
        handle.abort();
    }

    #[tokio::test]
    async fn test_daemon_stream_updates() {
        use tokio_stream::StreamExt;

        // Start mock HTTP server
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "result": "success"
            })))
            .mount(&mock_server)
            .await;

        // Create batcher and daemon
        let batcher = Arc::new(InMemoryBatcher::new());
        let config = DaemonConfig {
            poll_interval_ms: 50,
            batch_size: 10,
            max_in_flight_per_model: 10,
            models: vec![],
        };
        let daemon = Arc::new(Daemon::new(batcher.clone(), config));

        // Submit a request
        let request = create_test_request(&mock_server.uri());
        let context = RequestContext::default();
        let ids = batcher
            .submit_requests(vec![(request, context)])
            .await
            .unwrap();
        let request_id = ids[0];

        // Subscribe to updates
        let mut update_stream = daemon.subscribe(Some(ids.clone()));

        // Spawn daemon
        let handle = daemon.clone().spawn_arc();

        // Collect updates
        let mut updates = Vec::new();
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(update) = update_stream.next().await {
                updates.push(update.clone());
                if update.status.is_terminal() {
                    break;
                }
            }
        })
        .await;

        assert!(result.is_ok(), "Should receive updates");
        assert!(!updates.is_empty(), "Should have at least one update");

        // Check we got Processing and Completed updates
        let has_processing = updates
            .iter()
            .any(|u| matches!(u.status, RequestStatus::Processing { .. }));
        let has_completed = updates
            .iter()
            .any(|u| matches!(u.status, RequestStatus::Completed { .. }));

        assert!(has_processing, "Should have Processing update");
        assert!(has_completed, "Should have Completed update");

        // Check the completed update has correct data
        let completed = updates
            .iter()
            .find(|u| matches!(u.status, RequestStatus::Completed { .. }))
            .unwrap();

        if let RequestStatus::Completed {
            response_status,
            response_body,
            ..
        } = &completed.status
        {
            assert_eq!(response_status, &200);
            assert!(response_body.contains("success"));
        }

        // Cleanup
        handle.abort();
    }

    #[tokio::test]
    async fn test_daemon_handles_http_error() {
        // Start mock HTTP server that returns error
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
            .mount(&mock_server)
            .await;

        // Create batcher and daemon
        let batcher = Arc::new(InMemoryBatcher::new());
        let config = DaemonConfig {
            poll_interval_ms: 50,
            batch_size: 10,
            max_in_flight_per_model: 10,
            models: vec![],
        };
        let daemon = Daemon::new(batcher.clone(), config);

        // Submit a request
        let request = create_test_request(&mock_server.uri());
        let context = RequestContext::default();
        let ids = batcher
            .submit_requests(vec![(request, context)])
            .await
            .unwrap();
        let request_id = ids[0];

        // Spawn daemon
        let handle = daemon.spawn();

        // Wait for processing
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Check status - should be completed with 500 status
        let statuses = batcher.get_status(vec![request_id]).await.unwrap();

        // The request completes with the HTTP error status (500), not Failed status
        // because the HTTP request succeeded (didn't timeout/network error)
        if let RequestStatus::Completed {
            response_status, ..
        } = statuses[0].1
        {
            assert_eq!(response_status, 500);
        } else {
            panic!("Expected Completed status, got: {:?}", statuses[0].1);
        }

        // Cleanup
        handle.abort();
    }

    #[tokio::test]
    async fn test_daemon_processes_multiple_requests() {
        // Start mock HTTP server
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "result": "ok"
            })))
            .mount(&mock_server)
            .await;

        // Create batcher and daemon
        let batcher = Arc::new(InMemoryBatcher::new());
        let config = DaemonConfig {
            poll_interval_ms: 50,
            batch_size: 10,
            max_in_flight_per_model: 10,
            models: vec![],
        };
        let daemon = Daemon::new(batcher.clone(), config);

        // Submit multiple requests
        let requests = vec![
            (
                create_test_request(&mock_server.uri()),
                RequestContext::default(),
            ),
            (
                create_test_request(&mock_server.uri()),
                RequestContext::default(),
            ),
            (
                create_test_request(&mock_server.uri()),
                RequestContext::default(),
            ),
        ];
        let ids = batcher.submit_requests(requests).await.unwrap();

        // Spawn daemon
        let handle = daemon.spawn();

        // Wait for processing
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Check all statuses
        let statuses = batcher.get_status(ids).await.unwrap();
        for (_, status) in statuses {
            assert!(matches!(status, RequestStatus::Completed { .. }));
        }

        // Cleanup
        handle.abort();
    }
}
