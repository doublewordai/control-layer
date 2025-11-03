//! Daemon for processing batched requests with per-model concurrency control.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::error::Result;
use crate::http::HttpClient;
use crate::request::{DaemonId, RequestContext};
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

    /// Default request context (timeout, retries, etc.)
    pub default_context: RequestContext,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            claim_batch_size: 100,
            default_model_concurrency: 10,
            model_concurrency_limits: HashMap::new(),
            claim_interval_ms: 100,
            default_context: RequestContext {
                max_retries: 3,
                backoff_ms: 100,
                backoff_factor: 2,
                max_backoff_ms: 10000,
                timeout_ms: 30000,
                api_key: String::new(),
            },
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

            tracing::debug!(claimed_count = claimed.len(), "Claimed requests from storage");

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
                            let context = self.config.default_context.clone();

                            join_set.spawn(async move {
                                // Permit is held for the duration of this task
                                let _permit = permit;

                                tracing::info!(request_id = %request_id, "Processing request");

                                // Process the request
                                let processing = request.process(
                                    http_client,
                                    context,
                                    storage.as_ref()
                                ).await?;

                                // Wait for completion
                                match processing.complete(storage.as_ref()).await? {
                                    Ok(_completed) => {
                                        tracing::info!(request_id = %request_id, "Request completed successfully");
                                    }
                                    Err(_failed) => {
                                        tracing::warn!(request_id = %request_id, "Request failed");
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
                            tokio::spawn(async move {
                                if let Err(e) = request.unclaim(storage.as_ref()).await {
                                    tracing::error!(
                                        request_id = %request_id,
                                        error = %e,
                                        "Failed to unclaim request"
                                    );
                                }
                            });
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
            state: Pending {},
            data: RequestData {
                id: RequestId::from(uuid::Uuid::new_v4()),
                endpoint: "https://api.example.com".to_string(),
                method: "POST".to_string(),
                path: "/v1/test".to_string(),
                body: "{}".to_string(),
                model: "test-model".to_string(),
            },
        };

        storage
            .submit(
                request,
                RequestContext {
                    max_retries: 3,
                    backoff_ms: 100,
                    backoff_factor: 2,
                    max_backoff_ms: 10000,
                    timeout_ms: 30000,
                    api_key: "test-key".to_string(),
                },
            )
            .await
            .unwrap();

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
