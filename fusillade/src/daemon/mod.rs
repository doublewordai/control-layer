//! Daemon for processing batched requests with per-model concurrency control.
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{RwLock, Semaphore};
use tokio::task::JoinSet;

use crate::FusilladeError;
use crate::error::Result;
use crate::http::{HttpClient, HttpResponse};
use crate::manager::{DaemonStorage, Storage};
use crate::request::{DaemonId, RequestCompletionResult};
use crate::types::RequestId;
use futures::StreamExt;

pub mod transitions;
pub mod types;

pub use types::{
    AnyDaemonRecord, DaemonData, DaemonRecord, DaemonState, DaemonStats, DaemonStatus, Dead,
    Initializing, Running,
};

/// Predicate function to determine if a response should be retried.
///
/// Takes an HTTP response and returns true if the request should be retried.
pub type ShouldRetryFn = Arc<dyn Fn(&HttpResponse) -> bool + Send + Sync>;

/// Semaphore entry tracking both the semaphore and its configured limit.
type SemaphoreEntry = (Arc<Semaphore>, usize);

/// Default retry predicate: retry on server errors (5xx), rate limits (429), and timeouts (408).
pub fn default_should_retry(response: &HttpResponse) -> bool {
    response.status >= 500 || response.status == 429 || response.status == 408
}

/// Default function for creating the should_retry Arc
fn default_should_retry_fn() -> ShouldRetryFn {
    Arc::new(default_should_retry)
}

/// Configuration for the daemon.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct DaemonConfig {
    /// Maximum number of requests to claim in each iteration
    pub claim_batch_size: usize,

    /// Default concurrency limit per model
    pub default_model_concurrency: usize,

    /// Per-model concurrency overrides (shared, can be updated dynamically)
    pub model_concurrency_limits: Arc<dashmap::DashMap<String, usize>>,

    /// How long to sleep between claim iterations
    pub claim_interval_ms: u64,

    /// Maximum number of retry attempts before giving up
    pub max_retries: u32,

    /// Base backoff duration in milliseconds (will be exponentially increased)
    pub backoff_ms: u64,

    /// Factor by which the backoff_ms is increased with each retry
    pub backoff_factor: u64,

    /// Maximum backoff time in milliseconds
    pub max_backoff_ms: u64,

    /// Timeout for each individual request attempt in milliseconds
    pub timeout_ms: u64,

    /// Interval for logging daemon status (requests in flight) in milliseconds
    /// Set to None to disable periodic status logging
    pub status_log_interval_ms: Option<u64>,

    /// Interval for sending heartbeats to update daemon status in database (milliseconds)
    pub heartbeat_interval_ms: u64,

    /// Predicate function to determine if a response should be retried.
    /// Defaults to retrying 5xx, 429, and 408 status codes.
    #[serde(skip, default = "default_should_retry_fn")]
    pub should_retry: ShouldRetryFn,

    /// Maximum time a request can stay in "claimed" state before being unclaimed
    /// and returned to pending (milliseconds). This handles daemon crashes.
    pub claim_timeout_ms: u64,

    /// Maximum time a request can stay in "processing" state before being unclaimed
    /// and returned to pending (milliseconds). This handles daemon crashes during execution.
    pub processing_timeout_ms: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            claim_batch_size: 100,
            default_model_concurrency: 10,
            model_concurrency_limits: Arc::new(dashmap::DashMap::new()),
            claim_interval_ms: 1000,
            max_retries: 5,
            backoff_ms: 1000,
            backoff_factor: 2,
            max_backoff_ms: 10000,
            timeout_ms: 600000,
            status_log_interval_ms: Some(2000), // Log every 2 seconds by default
            heartbeat_interval_ms: 10000,       // Heartbeat every 10 seconds by default
            should_retry: Arc::new(default_should_retry),
            claim_timeout_ms: 60000,       // 1 minute
            processing_timeout_ms: 600000, // 10 minutes
        }
    }
}

/// Daemon that processes batched requests.
///
/// The daemon continuously claims pending requests from storage, enforces
/// per-model concurrency limits, and dispatches requests for execution.
pub struct Daemon<S, H>
where
    S: Storage + DaemonStorage,
    H: HttpClient,
{
    daemon_id: DaemonId,
    storage: Arc<S>,
    http_client: Arc<H>,
    config: DaemonConfig,
    semaphores: Arc<RwLock<HashMap<String, SemaphoreEntry>>>,
    requests_in_flight: Arc<AtomicUsize>,
    requests_processed: Arc<AtomicU64>,
    requests_failed: Arc<AtomicU64>,
    shutdown_token: tokio_util::sync::CancellationToken,
    /// Map of request_id -> cancellation token for user-initiated cancellation
    cancellation_tokens: Arc<dashmap::DashMap<RequestId, tokio_util::sync::CancellationToken>>,
}

impl<S, H> Daemon<S, H>
where
    S: Storage + DaemonStorage + 'static,
    H: HttpClient + 'static,
{
    /// Create a new daemon.
    pub fn new(
        storage: Arc<S>,
        http_client: Arc<H>,
        config: DaemonConfig,
        shutdown_token: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            daemon_id: DaemonId::from(uuid::Uuid::new_v4()),
            storage,
            http_client,
            config,
            semaphores: Arc::new(RwLock::new(HashMap::new())),
            requests_in_flight: Arc::new(AtomicUsize::new(0)),
            requests_processed: Arc::new(AtomicU64::new(0)),
            requests_failed: Arc::new(AtomicU64::new(0)),
            shutdown_token,
            cancellation_tokens: Arc::new(dashmap::DashMap::new()),
        }
    }

    /// Get or create a semaphore for a model.
    ///
    /// Automatically adjusts the semaphore's permit count if the configured limit has changed.
    /// For limit increases, adds permits. For decreases, forgets permits (as many as possible).
    /// Note: When decreasing, we can only forget permits that aren't currently held, so the
    /// effective limit may temporarily remain higher until requests complete.
    async fn get_semaphore(&self, model: &str) -> Arc<Semaphore> {
        let current_limit = self
            .config
            .model_concurrency_limits
            .get(model)
            .map(|entry| *entry.value())
            .unwrap_or(self.config.default_model_concurrency);

        let mut semaphores = self.semaphores.write().await;

        let entry = semaphores
            .entry(model.to_string())
            .or_insert_with(|| (Arc::new(Semaphore::new(current_limit)), current_limit));

        let (semaphore, stored_limit) = entry;

        // Check if the limit has changed
        if *stored_limit != current_limit {
            if current_limit > *stored_limit {
                // Limit increased - add permits
                let delta = current_limit - *stored_limit;
                semaphore.add_permits(delta);
                tracing::info!(
                    model = %model,
                    old_limit = *stored_limit,
                    new_limit = current_limit,
                    added_permits = delta,
                    "Increased model concurrency limit"
                );
                *stored_limit = current_limit;
            } else {
                // Limit decreased - forget permits (as many as we can)
                let desired_delta = *stored_limit - current_limit;
                let actual_forgotten = semaphore.forget_permits(desired_delta);

                if actual_forgotten < desired_delta {
                    tracing::warn!(
                        model = %model,
                        old_limit = *stored_limit,
                        target_limit = current_limit,
                        desired_to_forget = desired_delta,
                        actually_forgot = actual_forgotten,
                        held_permits = desired_delta - actual_forgotten,
                        "Decreased model concurrency limit (some permits still held by in-flight requests)"
                    );
                } else {
                    tracing::info!(
                        model = %model,
                        old_limit = *stored_limit,
                        new_limit = current_limit,
                        forgot_permits = actual_forgotten,
                        "Decreased model concurrency limit"
                    );
                }

                // Update to the new effective limit (accounting for unforgettable permits)
                *stored_limit = current_limit + (desired_delta - actual_forgotten);
            }
        }

        semaphore.clone()
    }

    /// Try to acquire a permit for a model (non-blocking).
    async fn try_acquire_permit(&self, model: &str) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let semaphore = self.get_semaphore(model).await;
        semaphore.clone().try_acquire_owned().ok()
    }

    /// Run the daemon loop.
    ///
    /// This continuously claims and processes requests until an error occurs
    /// or the task is cancelled.
    ///
    /// The `cancellation_stream` parameter is an optional stream that yields
    /// `RequestId`s that should be cancelled (e.g., from PostgreSQL LISTEN/NOTIFY).
    #[tracing::instrument(skip(self, cancellation_stream), fields(daemon_id = %self.daemon_id))]
    pub async fn run<CS>(self: Arc<Self>, cancellation_stream: Option<CS>) -> Result<()>
    where
        CS: futures::Stream<Item = RequestId> + Unpin + Send + 'static,
    {
        tracing::info!("Daemon starting main processing loop");

        // Register daemon in database
        let daemon_record = DaemonRecord {
            data: DaemonData {
                id: self.daemon_id,
                hostname: types::get_hostname(),
                pid: types::get_pid(),
                version: types::get_version(),
                config_snapshot: serde_json::to_value(&self.config)
                    .expect("Failed to serialize daemon config"),
            },
            state: Initializing {
                started_at: chrono::Utc::now(),
            },
        };

        let running_record = daemon_record.start(self.storage.as_ref()).await?;
        tracing::info!("Daemon registered in database");

        // Spawn periodic heartbeat task
        let storage = self.storage.clone();
        let requests_in_flight = self.requests_in_flight.clone();
        let requests_processed = self.requests_processed.clone();
        let requests_failed = self.requests_failed.clone();
        let daemon_id = self.daemon_id;
        let heartbeat_interval_ms = self.config.heartbeat_interval_ms;
        let shutdown_signal = self.shutdown_token.clone();

        let heartbeat_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(heartbeat_interval_ms));
            let mut daemon_record = running_record;

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let stats = DaemonStats {
                            requests_processed: requests_processed.load(Ordering::Relaxed),
                            requests_failed: requests_failed.load(Ordering::Relaxed),
                            requests_in_flight: requests_in_flight.load(Ordering::Relaxed),
                        };

                        // Clone the record so we preserve it if heartbeat fails
                        let current = daemon_record.clone();
                        match current.heartbeat(stats, storage.as_ref()).await {
                            Ok(updated) => {
                                daemon_record = updated;
                                tracing::trace!(
                                    daemon_id = %daemon_id,
                                    "Heartbeat sent"
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    daemon_id = %daemon_id,
                                    error = %e,
                                    "Failed to send heartbeat"
                                );
                                // daemon_record stays unchanged on error
                            }
                        }
                    }
                    _ = shutdown_signal.cancelled() => {
                        // Mark daemon as dead on shutdown
                        tracing::info!("Shutting down heartbeat task");
                        if let Err(e) = daemon_record.shutdown(storage.as_ref()).await {
                            tracing::error!(
                                daemon_id = %daemon_id,
                                error = %e,
                                "Failed to mark daemon as dead during shutdown"
                            );
                        }
                        break;
                    }
                }
            }
        });

        // Spawn periodic status logging task if configured
        if let Some(interval_ms) = self.config.status_log_interval_ms {
            let requests_in_flight = self.requests_in_flight.clone();
            let daemon_id = self.daemon_id;
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
                loop {
                    interval.tick().await;
                    let count = requests_in_flight.load(Ordering::Relaxed);
                    tracing::debug!(
                        daemon_id = %daemon_id,
                        requests_in_flight = count,
                        "Daemon status"
                    );
                }
            });
        }

        // Spawn task to process cancellation stream if provided
        if let Some(mut stream) = cancellation_stream {
            let cancellation_tokens = self.cancellation_tokens.clone();
            let shutdown_token = self.shutdown_token.clone();
            tokio::spawn(async move {
                tracing::info!("Cancellation stream listener started");

                loop {
                    tokio::select! {
                        Some(request_id) = stream.next() => {
                            tracing::debug!(request_id = %request_id, "Received cancellation notification");

                            // Look up the cancellation token and cancel it
                            if let Some(entry) = cancellation_tokens.get(&request_id) {
                                entry.value().cancel();
                                tracing::debug!(request_id = %request_id, "Cancelled request token");
                            } else {
                                tracing::debug!(
                                    request_id = %request_id,
                                    "Request not found in active requests (may have already completed)"
                                );
                            }
                        }
                        _ = shutdown_token.cancelled() => {
                            tracing::info!("Shutting down cancellation listener");
                            break;
                        }
                    }
                }
            });
        }

        let mut join_set: JoinSet<Result<()>> = JoinSet::new();

        let run_result = loop {
            // Check for shutdown signal
            if self.shutdown_token.is_cancelled() {
                tracing::info!("Shutdown signal received, stopping daemon");
                break Ok(());
            }

            // Poll for completed tasks (non-blocking)
            while let Some(result) = join_set.try_join_next() {
                match result {
                    Ok(Ok(())) => {
                        tracing::trace!("Task completed successfully");
                    }
                    Ok(Err(e)) => {
                        tracing::error!(error = %e, "Task failed");
                    }
                    Err(join_error) => {
                        tracing::error!(error = %join_error, "Task panicked");
                    }
                }
            }

            tracing::trace!("Sleeping before claiming");
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(self.config.claim_interval_ms)) => {},
                _ = self.shutdown_token.cancelled() => {
                    tracing::info!("Shutdown signal received, stopping daemon");
                    break Ok(());
                }
            }
            // Claim a batch of pending requests
            let claimed = self
                .storage
                .claim_requests(self.config.claim_batch_size, self.daemon_id)
                .await?;

            tracing::debug!(
                claimed_count = claimed.len(),
                "Claimed requests from storage"
            );

            // Group requests by model for better concurrency control visibility
            let mut by_model: HashMap<String, Vec<_>> = HashMap::new();
            for request in claimed {
                by_model
                    .entry(request.data.model.clone())
                    .or_default()
                    .push(request);
            }

            tracing::debug!(
                models = by_model.len(),
                total_requests = by_model.values().map(|v| v.len()).sum::<usize>(),
                "Grouped requests by model"
            );

            // Dispatch requests
            for (model, requests) in by_model {
                tracing::debug!(model = %model, count = requests.len(), "Processing requests for model");

                for request in requests {
                    let request_id = request.data.id;

                    // Try to acquire a semaphore permit for this model
                    match self.try_acquire_permit(&model).await {
                        Some(permit) => {
                            tracing::debug!(
                                request_id = %request_id,
                                model = %model,
                                "Acquired permit, spawning processing task"
                            );

                            // We have capacity - spawn a task
                            let storage = self.storage.clone();
                            let http_client = (*self.http_client).clone();
                            let timeout_ms = self.config.timeout_ms;
                            let retry_config = (&self.config).into();
                            let requests_in_flight = self.requests_in_flight.clone();
                            let requests_processed = self.requests_processed.clone();
                            let requests_failed = self.requests_failed.clone();
                            let should_retry = self.config.should_retry.clone();
                            let shutdown_token = self.shutdown_token.clone();
                            let cancellation_tokens = self.cancellation_tokens.clone();

                            // Create a cancellation token for user-initiated cancellation
                            let user_cancellation_token =
                                tokio_util::sync::CancellationToken::new();

                            // Register the token so the LISTEN task can cancel it
                            cancellation_tokens.insert(request_id, user_cancellation_token.clone());

                            // Increment in-flight counter
                            requests_in_flight.fetch_add(1, Ordering::Relaxed);

                            join_set.spawn(async move {
                                // Permit is held for the duration of this task
                                let _permit = permit;

                                // Ensure we decrement the counter when this task completes
                                let _guard = scopeguard::guard((), |_| {
                                    requests_in_flight.fetch_sub(1, Ordering::Relaxed);
                                });

                                tracing::info!(request_id = %request_id, "Processing request");

                                // Process the request
                                let processing = request.process(
                                    http_client,
                                    timeout_ms,
                                    storage.as_ref()
                                ).await?;

                                // Create cancellation future that resolves to User or Shutdown
                                let cancellation = async {
                                    tokio::select! {
                                        _ = user_cancellation_token.cancelled() => {
                                            crate::request::transitions::CancellationReason::User
                                        }
                                        _ = shutdown_token.cancelled() => {
                                            crate::request::transitions::CancellationReason::Shutdown
                                        }
                                    }
                                };

                                // Wait for completion
                                match processing.complete(storage.as_ref(), |response| {
                                    (should_retry)(response)
                                }, cancellation).await {
                                    Ok(RequestCompletionResult::Completed(_completed)) => {
                                        requests_processed.fetch_add(1, Ordering::Relaxed);
                                        tracing::info!(request_id = %request_id, "Request completed successfully");
                                    }
                                    Ok(RequestCompletionResult::Failed(failed)) => {
                                        let retry_attempt = failed.state.retry_attempt;

                                        // Check if this is a retriable error using the FailureReason
                                        if failed.state.reason.is_retriable() {
                                            tracing::warn!(
                                                request_id = %request_id,
                                                retry_attempt,
                                                error = %failed.state.reason.to_error_message(),
                                                "Request failed with retriable error, attempting retry"
                                            );

                                            // Attempt to retry
                                            match failed.retry(retry_attempt, retry_config, storage.as_ref()).await? {
                                                Some(_pending) => {
                                                    tracing::info!(
                                                        request_id = %request_id,
                                                        retry_attempt = retry_attempt + 1,
                                                        "Request queued for retry"
                                                    );
                                                }
                                                None => {
                                                    requests_failed.fetch_add(1, Ordering::Relaxed);
                                                    tracing::warn!(
                                                        request_id = %request_id,
                                                        retry_attempt,
                                                        "Request failed permanently (no retries remaining)"
                                                    );
                                                }
                                            }
                                        } else {
                                            requests_failed.fetch_add(1, Ordering::Relaxed);
                                            tracing::warn!(
                                                request_id = %request_id,
                                                error = %failed.state.reason.to_error_message(),
                                                "Request failed with non-retriable error, not retrying"
                                            );
                                        }
                                    }
                                    Ok(RequestCompletionResult::Canceled(_canceled)) => {
                                        tracing::debug!(request_id = %request_id, "Request canceled by user");
                                    }
                                    Err(FusilladeError::Shutdown) => {
                                        tracing::info!(request_id = %request_id, "Request aborted due to shutdown");
                                        // Don't count as failed - request will be reclaimed
                                    }
                                    Err(e) => {
                                        // Unexpected error
                                        tracing::error!(request_id = %request_id, error = %e, "Unexpected error processing request");
                                        return Err(e);
                                    }
                                }

                                // Remove the cancellation token from the map
                                cancellation_tokens.remove(&request_id);

                                Ok(())
                            });
                        }
                        None => {
                            tracing::debug!(
                                request_id = %request_id,
                                model = %model,
                                "No capacity available, unclaiming request"
                            );

                            // No capacity for this model - unclaim the request
                            let storage = self.storage.clone();
                            if let Err(e) = request.unclaim(storage.as_ref()).await {
                                tracing::error!(
                                    request_id = %request_id,
                                    error = %e,
                                    "Failed to unclaim request"
                                );
                            };
                        }
                    }
                }
            }
        };

        // Wait for heartbeat task to complete (it will mark daemon as dead)
        tracing::info!("Waiting for heartbeat task to complete");
        if let Err(e) = heartbeat_handle.await {
            tracing::error!(error = %e, "Heartbeat task panicked");
        }

        run_result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{HttpResponse, MockHttpClient};
    use crate::manager::{DaemonExecutor, postgres::PostgresRequestManager};
    use std::time::Duration;

    #[sqlx::test]
    #[test_log::test]
    async fn test_daemon_claims_and_completes_request(pool: sqlx::PgPool) {
        // Setup: Create HTTP client with mock response
        let http_client = Arc::new(MockHttpClient::new());
        http_client.add_response(
            "POST /v1/test",
            Ok(HttpResponse {
                status: 200,
                body: r#"{"result":"success"}"#.to_string(),
            }),
        );

        // Setup: Create manager with fast claim interval (no sleeping)
        let config = DaemonConfig {
            claim_batch_size: 10,
            claim_interval_ms: 10, // Very fast for testing
            default_model_concurrency: 10,
            model_concurrency_limits: Arc::new(dashmap::DashMap::new()),
            max_retries: 3,
            backoff_ms: 100,
            backoff_factor: 2,
            max_backoff_ms: 1000,
            timeout_ms: 5000,
            status_log_interval_ms: None, // Disable status logging in tests
            heartbeat_interval_ms: 10000, // 10 seconds
            should_retry: Arc::new(default_should_retry),
            claim_timeout_ms: 60000,
            processing_timeout_ms: 600000,
        };

        let manager = Arc::new(
            PostgresRequestManager::with_client(pool.clone(), http_client.clone())
                .with_config(config),
        );

        // Setup: Create a file and batch to associate with our request
        let file_id = manager
            .create_file(
                "test-file".to_string(),
                Some("Test file".to_string()),
                vec![crate::RequestTemplateInput {
                    custom_id: None,
                    endpoint: "https://api.example.com".to_string(),
                    method: "POST".to_string(),
                    path: "/v1/test".to_string(),
                    body: r#"{"prompt":"test"}"#.to_string(),
                    model: "test-model".to_string(),
                    api_key: "test-key".to_string(),
                }],
            )
            .await
            .expect("Failed to create file");

        let batch = manager
            .create_batch(crate::batch::BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
            })
            .await
            .expect("Failed to create batch");

        // Get the created request from the batch
        let requests = manager
            .get_batch_requests(batch.id)
            .await
            .expect("Failed to get batch requests");
        assert_eq!(requests.len(), 1);
        let request_id = requests[0].id();

        // Start the daemon
        let shutdown_token = tokio_util::sync::CancellationToken::new();
        manager
            .clone()
            .run(shutdown_token.clone())
            .expect("Failed to start daemon");

        // Poll for completion (with timeout)
        let start = tokio::time::Instant::now();
        let timeout = Duration::from_secs(5);
        let mut completed = false;

        while start.elapsed() < timeout {
            let results = manager
                .get_requests(vec![request_id])
                .await
                .expect("Failed to get request");

            if let Some(Ok(any_request)) = results.first() {
                if any_request.is_terminal() {
                    if let crate::AnyRequest::Completed(req) = any_request {
                        // Verify the request was completed successfully
                        assert_eq!(req.state.response_status, 200);
                        assert_eq!(req.state.response_body, r#"{"result":"success"}"#);
                        completed = true;
                        break;
                    } else {
                        panic!(
                            "Request reached terminal state but was not completed: {:?}",
                            any_request
                        );
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Stop the daemon
        shutdown_token.cancel();

        // Assert that the request completed
        assert!(
            completed,
            "Request did not complete within timeout. Check daemon processing."
        );

        // Verify HTTP client was called exactly once
        assert_eq!(http_client.call_count(), 1);
        let calls = http_client.get_calls();
        assert_eq!(calls[0].method, "POST");
        assert_eq!(calls[0].path, "/v1/test");
        assert_eq!(calls[0].api_key, "test-key");
    }

    #[sqlx::test]
    async fn test_daemon_respects_per_model_concurrency_limits(pool: sqlx::PgPool) {
        // Setup: Create HTTP client with triggered responses
        let http_client = Arc::new(MockHttpClient::new());

        // Add 5 triggered responses for our 5 requests
        let trigger1 = http_client.add_response_with_trigger(
            "POST /v1/test",
            Ok(HttpResponse {
                status: 200,
                body: r#"{"result":"1"}"#.to_string(),
            }),
        );
        let trigger2 = http_client.add_response_with_trigger(
            "POST /v1/test",
            Ok(HttpResponse {
                status: 200,
                body: r#"{"result":"2"}"#.to_string(),
            }),
        );
        let trigger3 = http_client.add_response_with_trigger(
            "POST /v1/test",
            Ok(HttpResponse {
                status: 200,
                body: r#"{"result":"3"}"#.to_string(),
            }),
        );
        let trigger4 = http_client.add_response_with_trigger(
            "POST /v1/test",
            Ok(HttpResponse {
                status: 200,
                body: r#"{"result":"4"}"#.to_string(),
            }),
        );
        let trigger5 = http_client.add_response_with_trigger(
            "POST /v1/test",
            Ok(HttpResponse {
                status: 200,
                body: r#"{"result":"5"}"#.to_string(),
            }),
        );

        // Setup: Create manager with concurrency limit of 2 for "gpt-4"
        let model_concurrency_limits = Arc::new(dashmap::DashMap::new());
        model_concurrency_limits.insert("gpt-4".to_string(), 2);

        let config = DaemonConfig {
            claim_batch_size: 10,
            claim_interval_ms: 10,
            default_model_concurrency: 10,
            model_concurrency_limits,
            max_retries: 3,
            backoff_ms: 100,
            backoff_factor: 2,
            max_backoff_ms: 1000,
            timeout_ms: 5000,
            status_log_interval_ms: None,
            heartbeat_interval_ms: 10000,
            should_retry: Arc::new(default_should_retry),
            claim_timeout_ms: 60000,
            processing_timeout_ms: 600000,
        };

        let manager = Arc::new(
            PostgresRequestManager::with_client(pool.clone(), http_client.clone())
                .with_config(config),
        );

        // Setup: Create a file with 5 templates, all using "gpt-4"
        let file_id = manager
            .create_file(
                "test-file".to_string(),
                Some("Test concurrency limits".to_string()),
                vec![
                    crate::RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/test".to_string(),
                        body: r#"{"prompt":"test1"}"#.to_string(),
                        model: "gpt-4".to_string(),
                        api_key: "test-key".to_string(),
                    },
                    crate::RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/test".to_string(),
                        body: r#"{"prompt":"test2"}"#.to_string(),
                        model: "gpt-4".to_string(),
                        api_key: "test-key".to_string(),
                    },
                    crate::RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/test".to_string(),
                        body: r#"{"prompt":"test3"}"#.to_string(),
                        model: "gpt-4".to_string(),
                        api_key: "test-key".to_string(),
                    },
                    crate::RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/test".to_string(),
                        body: r#"{"prompt":"test4"}"#.to_string(),
                        model: "gpt-4".to_string(),
                        api_key: "test-key".to_string(),
                    },
                    crate::RequestTemplateInput {
                        custom_id: None,
                        endpoint: "https://api.example.com".to_string(),
                        method: "POST".to_string(),
                        path: "/v1/test".to_string(),
                        body: r#"{"prompt":"test5"}"#.to_string(),
                        model: "gpt-4".to_string(),
                        api_key: "test-key".to_string(),
                    },
                ],
            )
            .await
            .expect("Failed to create file");

        let batch = manager
            .create_batch(crate::batch::BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
            })
            .await
            .expect("Failed to create batch");

        // Start the daemon
        let shutdown_token = tokio_util::sync::CancellationToken::new();
        manager
            .clone()
            .run(shutdown_token.clone())
            .expect("Failed to start daemon");

        // Wait for exactly 2 requests to be in-flight (respecting concurrency limit)
        let start = tokio::time::Instant::now();
        let timeout = Duration::from_secs(2);
        let mut reached_limit = false;

        while start.elapsed() < timeout {
            let in_flight = http_client.in_flight_count();
            if in_flight == 2 {
                reached_limit = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(
            reached_limit,
            "Expected exactly 2 requests in-flight, got {}",
            http_client.in_flight_count()
        );

        // Verify exactly 2 are in-flight (not more)
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            http_client.in_flight_count(),
            2,
            "Concurrency limit violated: more than 2 requests in-flight"
        );

        // Trigger completion of first request
        trigger1.send(()).unwrap();

        // Wait for the third request to start
        let start = tokio::time::Instant::now();
        let timeout = Duration::from_secs(2);
        let mut third_started = false;

        while start.elapsed() < timeout {
            if http_client.call_count() >= 3 {
                third_started = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(
            third_started,
            "Third request should have started after first completed"
        );

        // Verify still only 2 in-flight
        assert_eq!(
            http_client.in_flight_count(),
            2,
            "Should maintain concurrency limit of 2"
        );

        // Complete remaining requests to clean up
        trigger2.send(()).unwrap();
        trigger3.send(()).unwrap();
        trigger4.send(()).unwrap();
        trigger5.send(()).unwrap();

        // Wait for all requests to complete
        let start = tokio::time::Instant::now();
        let timeout = Duration::from_secs(5);
        let mut all_completed = false;

        while start.elapsed() < timeout {
            let status = manager
                .get_batch_status(batch.id)
                .await
                .expect("Failed to get batch status");

            if status.completed_requests == 5 {
                all_completed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Stop the daemon
        shutdown_token.cancel();

        assert!(all_completed, "All 5 requests should have completed");

        // Verify all 5 HTTP calls were made
        assert_eq!(http_client.call_count(), 5);
    }

    #[sqlx::test]
    async fn test_daemon_retries_failed_requests(pool: sqlx::PgPool) {
        // Setup: Create HTTP client with failing responses, then success
        let http_client = Arc::new(MockHttpClient::new());

        // First attempt: fails with 500
        http_client.add_response(
            "POST /v1/test",
            Ok(HttpResponse {
                status: 500,
                body: r#"{"error":"internal error"}"#.to_string(),
            }),
        );

        // Second attempt: fails with 503
        http_client.add_response(
            "POST /v1/test",
            Ok(HttpResponse {
                status: 503,
                body: r#"{"error":"service unavailable"}"#.to_string(),
            }),
        );

        // Third attempt: succeeds
        http_client.add_response(
            "POST /v1/test",
            Ok(HttpResponse {
                status: 200,
                body: r#"{"result":"success after retries"}"#.to_string(),
            }),
        );

        // Setup: Create manager with fast backoff for testing
        let config = DaemonConfig {
            claim_batch_size: 10,
            claim_interval_ms: 10,
            default_model_concurrency: 10,
            model_concurrency_limits: Arc::new(dashmap::DashMap::new()),
            max_retries: 5,
            backoff_ms: 10, // Very fast backoff for testing
            backoff_factor: 2,
            max_backoff_ms: 100,
            timeout_ms: 5000,
            status_log_interval_ms: None,
            heartbeat_interval_ms: 10000,
            should_retry: Arc::new(default_should_retry),
            claim_timeout_ms: 60000,
            processing_timeout_ms: 600000,
        };

        let manager = Arc::new(
            PostgresRequestManager::with_client(pool.clone(), http_client.clone())
                .with_config(config),
        );

        // Setup: Create a file and batch
        let file_id = manager
            .create_file(
                "test-file".to_string(),
                Some("Test retry logic".to_string()),
                vec![crate::RequestTemplateInput {
                    custom_id: None,
                    endpoint: "https://api.example.com".to_string(),
                    method: "POST".to_string(),
                    path: "/v1/test".to_string(),
                    body: r#"{"prompt":"test"}"#.to_string(),
                    model: "test-model".to_string(),
                    api_key: "test-key".to_string(),
                }],
            )
            .await
            .expect("Failed to create file");

        let batch = manager
            .create_batch(crate::batch::BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
            })
            .await
            .expect("Failed to create batch");

        let requests = manager
            .get_batch_requests(batch.id)
            .await
            .expect("Failed to get batch requests");
        assert_eq!(requests.len(), 1);
        let request_id = requests[0].id();

        // Start the daemon
        let shutdown_token = tokio_util::sync::CancellationToken::new();
        manager
            .clone()
            .run(shutdown_token.clone())
            .expect("Failed to start daemon");

        // Poll for completion (with timeout)
        let start = tokio::time::Instant::now();
        let timeout = Duration::from_secs(5);
        let mut completed = false;

        while start.elapsed() < timeout {
            let results = manager
                .get_requests(vec![request_id])
                .await
                .expect("Failed to get request");

            if let Some(Ok(any_request)) = results.first() {
                if let crate::AnyRequest::Completed(req) = any_request {
                    // Verify the request eventually completed successfully
                    assert_eq!(req.state.response_status, 200);
                    assert_eq!(
                        req.state.response_body,
                        r#"{"result":"success after retries"}"#
                    );
                    completed = true;
                    break;
                }
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Stop the daemon
        shutdown_token.cancel();

        assert!(completed, "Request should have completed after retries");

        // Verify the request was attempted 3 times (2 failures + 1 success)
        assert_eq!(
            http_client.call_count(),
            3,
            "Expected 3 HTTP calls (2 failed attempts + 1 success)"
        );
    }

    #[sqlx::test]
    async fn test_daemon_dynamically_updates_concurrency_limits(pool: sqlx::PgPool) {
        // Setup: Create HTTP client with triggered responses
        let http_client = Arc::new(MockHttpClient::new());

        // Add 10 triggered responses
        let mut triggers = vec![];
        for i in 1..=10 {
            let trigger = http_client.add_response_with_trigger(
                "POST /v1/test",
                Ok(HttpResponse {
                    status: 200,
                    body: format!(r#"{{"result":"{}"}}"#, i),
                }),
            );
            triggers.push(trigger);
        }

        // Setup: Start with concurrency limit of 2 for "gpt-4"
        let model_concurrency_limits = Arc::new(dashmap::DashMap::new());
        model_concurrency_limits.insert("gpt-4".to_string(), 2);

        let config = DaemonConfig {
            claim_batch_size: 10,
            claim_interval_ms: 10,
            default_model_concurrency: 10,
            model_concurrency_limits: model_concurrency_limits.clone(),
            max_retries: 3,
            backoff_ms: 100,
            backoff_factor: 2,
            max_backoff_ms: 1000,
            timeout_ms: 5000,
            status_log_interval_ms: None,
            heartbeat_interval_ms: 10000,
            should_retry: Arc::new(default_should_retry),
            claim_timeout_ms: 60000,
            processing_timeout_ms: 600000,
        };

        let manager = Arc::new(
            PostgresRequestManager::with_client(pool.clone(), http_client.clone())
                .with_config(config),
        );

        // Setup: Create a file with 10 requests, all using "gpt-4"
        let templates: Vec<_> = (1..=10)
            .map(|i| crate::RequestTemplateInput {
                custom_id: None,
                endpoint: "https://api.example.com".to_string(),
                method: "POST".to_string(),
                path: "/v1/test".to_string(),
                body: format!(r#"{{"prompt":"test{}"}}"#, i),
                model: "gpt-4".to_string(),
                api_key: "test-key".to_string(),
            })
            .collect();

        let file_id = manager
            .create_file(
                "test-file".to_string(),
                Some("Test dynamic limits".to_string()),
                templates,
            )
            .await
            .expect("Failed to create file");

        let batch = manager
            .create_batch(crate::batch::BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
            })
            .await
            .expect("Failed to create batch");

        // Start the daemon
        let shutdown_token = tokio_util::sync::CancellationToken::new();
        manager
            .clone()
            .run(shutdown_token.clone())
            .expect("Failed to start daemon");

        // Wait for exactly 2 requests to be in-flight (initial limit)
        let start = tokio::time::Instant::now();
        let timeout = Duration::from_secs(2);
        let mut reached_initial_limit = false;

        while start.elapsed() < timeout {
            let in_flight = http_client.in_flight_count();
            if in_flight == 2 {
                reached_initial_limit = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(
            reached_initial_limit,
            "Expected exactly 2 requests in-flight with initial limit"
        );

        // Increase the limit to 5
        model_concurrency_limits.insert("gpt-4".to_string(), 5);

        // Wait a bit for the daemon to pick up the new limit
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Complete one request to free up a permit and trigger daemon to check limits
        triggers.remove(0).send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Now we should see up to 5 requests in flight
        let start = tokio::time::Instant::now();
        let timeout = Duration::from_secs(2);
        let mut reached_new_limit = false;

        while start.elapsed() < timeout {
            let in_flight = http_client.in_flight_count();
            if in_flight >= 4 {
                // Should see at least 4-5 in flight with new limit
                reached_new_limit = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(
            reached_new_limit,
            "Expected more requests in-flight after limit increase, got {}",
            http_client.in_flight_count()
        );

        // Now decrease the limit to 3
        model_concurrency_limits.insert("gpt-4".to_string(), 3);

        // Complete remaining requests
        for trigger in triggers {
            trigger.send(()).unwrap();
        }

        // Wait for all requests to complete
        let start = tokio::time::Instant::now();
        let timeout = Duration::from_secs(5);
        let mut all_completed = false;

        while start.elapsed() < timeout {
            let status = manager
                .get_batch_status(batch.id)
                .await
                .expect("Failed to get batch status");

            if status.completed_requests == 10 {
                all_completed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Stop the daemon
        shutdown_token.cancel();

        assert!(all_completed, "All 10 requests should have completed");
        assert_eq!(http_client.call_count(), 10);
    }
}
