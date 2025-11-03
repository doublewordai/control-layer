//! In-memory implementation of RequestManager.
//!
//! This implementation combines in-memory storage with the daemon to provide
//! a complete batching system suitable for testing and single-process deployments.
//!
use crate::request::AnyRequest;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::Stream;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::daemon::{Daemon, DaemonConfig};
use crate::error::Result;
use crate::http::HttpClient;
use crate::request::{Pending, Request, RequestId};
use crate::storage::{in_memory::InMemoryStorage, Storage};

use super::RequestManager;

/// In-memory implementation of the RequestManager trait.
///
/// This manager uses in-memory storage and runs a daemon for processing requests.
/// It's suitable for testing and single-process deployments where persistence isn't required.
///
/// # Example
/// ```ignore
/// use batcher::{InMemoryRequestManager, ReqwestHttpClient, DaemonConfig};
///
/// let http_client = Arc::new(ReqwestHttpClient::new());
/// let config = DaemonConfig::default();
/// let manager = InMemoryRequestManager::new(http_client, config);
///
/// // Start processing
/// let handle = manager.run()?;
///
/// // Submit requests
/// manager.submit_requests(vec![(request, context)]).await?;
/// ```
pub struct InMemoryRequestManager<H: HttpClient> {
    storage: Arc<InMemoryStorage>,
    http_client: Arc<H>,
    config: DaemonConfig,
    status_tx: broadcast::Sender<AnyRequest>,
}

impl<H: HttpClient + 'static> InMemoryRequestManager<H> {
    /// Create a new in-memory request manager.
    ///
    /// # Arguments
    /// * `http_client` - HTTP client for making requests
    /// * `config` - Daemon configuration (batch size, concurrency limits, etc.)
    pub fn new(http_client: Arc<H>, config: DaemonConfig) -> Self {
        // Larger buffer to handle batch operations without lagging
        // Each request goes through ~4 state transitions (Pending->Claimed->Processing->Completed/Failed)
        let (status_tx, _) = broadcast::channel(10000);

        Self {
            storage: Arc::new(InMemoryStorage::with_status_updates(status_tx.clone())),
            http_client,
            config,
            status_tx,
        }
    }

    /// Create with default daemon configuration.
    pub fn with_defaults(http_client: Arc<H>) -> Self {
        Self::new(http_client, DaemonConfig::default())
    }
}

#[async_trait]
impl<H: HttpClient + 'static> RequestManager for InMemoryRequestManager<H> {
    #[tracing::instrument(skip(self, requests), fields(count = requests.len()))]
    async fn submit_requests(&self, requests: Vec<Request<Pending>>) -> Result<Vec<Result<()>>> {
        tracing::info!(count = requests.len(), "Submitting batch of requests");

        let mut results = Vec::new();

        for request in requests {
            let result = self.storage.submit(request).await;
            results.push(result);
        }

        let successful = results.iter().filter(|r| r.is_ok()).count();
        tracing::info!(
            successful = successful,
            total = results.len(),
            "Batch submission complete"
        );

        Ok(results)
    }

    #[tracing::instrument(skip(self, ids), fields(count = ids.len()))]
    async fn cancel_requests(&self, ids: Vec<RequestId>) -> Result<Vec<Result<()>>> {
        tracing::info!(count = ids.len(), "Cancelling requests");

        let mut results = Vec::new();

        for id in ids {
            // Get the request from storage
            let get_results = self.storage.get_requests(vec![id]).await?;
            let request_result = get_results.into_iter().next().unwrap();

            let result = match request_result {
                Ok(any_request) => match any_request {
                    AnyRequest::Pending(req) => {
                        req.cancel(&*self.storage).await?;
                        Ok(())
                    }
                    AnyRequest::Claimed(req) => {
                        req.cancel(&*self.storage).await?;
                        Ok(())
                    }
                    AnyRequest::Processing(req) => {
                        req.cancel(&*self.storage).await?;
                        Ok(())
                    }
                    AnyRequest::Completed(_) | AnyRequest::Failed(_) | AnyRequest::Canceled(_) => {
                        Err(crate::error::BatcherError::InvalidState(
                            id,
                            "terminal state".to_string(),
                            "cancellable state".to_string(),
                        ))
                    }
                },
                Err(e) => Err(e),
            };

            results.push(result);
        }

        Ok(results)
    }

    async fn get_status(&self, ids: Vec<RequestId>) -> Result<Vec<Result<AnyRequest>>> {
        self.storage.get_requests(ids).await
    }

    fn get_status_updates(
        &self,
        _id_filter: Option<Vec<RequestId>>,
    ) -> std::pin::Pin<Box<dyn Stream<Item = Result<Result<AnyRequest>>> + Send>> {
        // Subscribe to the broadcast channel
        let mut rx = self.status_tx.subscribe();

        // Convert the receiver into a stream
        Box::pin(async_stream::stream! {
            loop {
                match rx.recv().await {
                    Ok(request) => yield Ok(Ok(request)),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Missed n messages due to slow consumer
                        tracing::info!(lagged_count = n, "Status update stream lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Channel closed, end the stream
                        break;
                    }
                }
            }
        })
    }

    fn run(&self) -> Result<JoinHandle<Result<()>>> {
        tracing::info!("Starting request manager daemon");

        let daemon = Arc::new(Daemon::new(
            self.storage.clone(),
            self.http_client.clone(),
            self.config.clone(),
        ));

        let handle = tokio::spawn(async move { daemon.run().await });

        tracing::info!("Daemon spawned successfully");

        Ok(handle)
    }
}
