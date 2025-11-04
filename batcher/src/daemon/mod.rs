//! Daemon for processing batched requests with per-model concurrency control.
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::error::Result;
use crate::http::HttpClient;
use crate::request::DaemonId;
use crate::storage::Storage;

/// Configuration for the daemon.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Maximum number of requests to claim in each iteration
    pub claim_batch_size: usize,

    /// Default concurrency limit per model
    pub default_model_concurrency: usize,

    /// Per-model concurrency overrides
    pub model_concurrency_limits: HashMap<String, usize>,

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
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            claim_batch_size: 100,
            default_model_concurrency: 10,
            model_concurrency_limits: HashMap::new(),
            claim_interval_ms: 1000,
            max_retries: 5,
            backoff_ms: 1000,
            backoff_factor: 2,
            max_backoff_ms: 10000,
            timeout_ms: 600000,
            status_log_interval_ms: Some(2000), // Log every 5 seconds by default
        }
    }
}

/// Daemon that processes batched requests.
///
/// The daemon continuously claims pending requests from storage, enforces
/// per-model concurrency limits, and dispatches requests for execution.
pub struct Daemon<S, H>
where
    S: Storage,
    H: HttpClient,
{
    daemon_id: DaemonId,
    storage: Arc<S>,
    http_client: Arc<H>,
    config: DaemonConfig,
    semaphores: Arc<RwLock<HashMap<String, Arc<Semaphore>>>>,
    requests_in_flight: Arc<AtomicUsize>,
}

impl<S, H> Daemon<S, H>
where
    S: Storage + 'static,
    H: HttpClient + 'static,
{
    /// Create a new daemon.
    pub fn new(storage: Arc<S>, http_client: Arc<H>, config: DaemonConfig) -> Self {
        Self {
            daemon_id: DaemonId::from(uuid::Uuid::new_v4()),
            storage,
            http_client,
            config,
            semaphores: Arc::new(RwLock::new(HashMap::new())),
            requests_in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Get or create a semaphore for a model.
    fn get_semaphore(&self, model: &str) -> Arc<Semaphore> {
        let mut semaphores = self.semaphores.write();

        semaphores
            .entry(model.to_string())
            .or_insert_with(|| {
                let limit = self
                    .config
                    .model_concurrency_limits
                    .get(model)
                    .copied()
                    .unwrap_or(self.config.default_model_concurrency);
                Arc::new(Semaphore::new(limit))
            })
            .clone()
    }

    /// Try to acquire a permit for a model (non-blocking).
    fn try_acquire_permit(&self, model: &str) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let semaphore = self.get_semaphore(model);
        semaphore.clone().try_acquire_owned().ok()
    }

    /// Run the daemon loop.
    ///
    /// This continuously claims and processes requests until an error occurs
    /// or the task is cancelled.
    #[tracing::instrument(skip(self), fields(daemon_id = %self.daemon_id))]
    pub async fn run(self: Arc<Self>) -> Result<()> {
        tracing::info!("Daemon starting main processing loop");

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

        let mut join_set: JoinSet<Result<()>> = JoinSet::new();

        loop {
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

            // Claim a batch of pending requests
            let claimed = self
                .storage
                .claim_requests(self.config.claim_batch_size, self.daemon_id)
                .await?;

            if claimed.is_empty() {
                tracing::trace!("No pending requests, sleeping");
                // No pending requests, sleep and retry
                tokio::time::sleep(Duration::from_millis(self.config.claim_interval_ms)).await;
                continue;
            }

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
                    match self.try_acquire_permit(&model) {
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

                                // Wait for completion
                                match processing.complete(storage.as_ref()).await? {
                                    Ok(_completed) => {
                                        tracing::info!(request_id = %request_id, "Request completed successfully");
                                    }
                                    Err(failed) => {
                                        let retry_attempt = failed.state.retry_attempt;
                                        tracing::warn!(
                                            request_id = %request_id,
                                            retry_attempt,
                                            "Request failed, attempting retry"
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
                                                tracing::warn!(
                                                    request_id = %request_id,
                                                    retry_attempt,
                                                    "Request failed permanently (no retries remaining)"
                                                );
                                            }
                                        }
                                    }
                                }

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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::MockHttpClient;
    use crate::request::*;
    use crate::storage::in_memory::InMemoryStorage;

    #[tokio::test]
    async fn test_daemon_processes_requests() {
        let storage = Arc::new(InMemoryStorage::new());
        let http_client = Arc::new(MockHttpClient::new());

        // Set up a mock response
        http_client.add_response(
            "POST /v1/test",
            Ok(crate::http::HttpResponse {
                status: 200,
                body: "OK".to_string(),
            }),
        );

        // Submit a pending request
        let request = Request {
            state: Pending {
                retry_attempt: 0,
                not_before: None,
            },
            data: RequestData {
                id: RequestId::from(uuid::Uuid::new_v4()),
                endpoint: "https://api.example.com".to_string(),
                method: "POST".to_string(),
                path: "/v1/test".to_string(),
                body: "{}".to_string(),
                model: "test-model".to_string(),
                api_key: "test-key".to_string(),
            },
        };

        storage.submit(request).await.unwrap();

        // Create and run daemon
        let daemon = Arc::new(Daemon::new(
            storage.clone(),
            http_client,
            DaemonConfig::default(),
        ));

        // Run daemon for a short time
        let daemon_handle = tokio::spawn({
            let daemon = daemon.clone();
            async move { daemon.run().await }
        });

        // Give it time to process
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Stop the daemon
        daemon_handle.abort();

        // Verify no more pending requests
        let pending = storage.view_pending_requests(10, None).await.unwrap();
        assert_eq!(pending.len(), 0);
    }
}
