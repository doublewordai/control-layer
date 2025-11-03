//! PostgreSQL implementation of RequestManager.
//!
//! This implementation combines PostgreSQL storage with the daemon to provide
//! a production-ready batching system with persistent storage and real-time updates.

use crate::request::AnyRequest;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::Stream;
use sqlx::postgres::PgPool;
use tokio::task::JoinHandle;

use crate::daemon::{Daemon, DaemonConfig};
use crate::error::{BatcherError, Result};
use crate::http::HttpClient;
use crate::request::{Pending, Request, RequestId};
use crate::storage::postgres::PostgresStorage;
use crate::storage::Storage;

use super::RequestManager;

/// PostgreSQL implementation of the RequestManager trait.
///
/// This manager uses PostgreSQL for persistent storage and runs a daemon for processing requests.
/// It leverages Postgres LISTEN/NOTIFY for real-time status updates.
///
/// # Example
/// ```ignore
/// use batcher::{PostgresRequestManager, ReqwestHttpClient, DaemonConfig};
/// use sqlx::PgPool;
///
/// let pool = PgPool::connect("postgresql://localhost/batcher").await?;
/// let http_client = Arc::new(ReqwestHttpClient::new());
/// let config = DaemonConfig::default();
/// let manager = PostgresRequestManager::new(pool, http_client, config).await?;
///
/// // Start processing
/// let handle = manager.run()?;
///
/// // Submit requests
/// manager.submit_requests(vec![request]).await?;
/// ```
pub struct PostgresRequestManager<H: HttpClient> {
    storage: Arc<PostgresStorage>,
    http_client: Arc<H>,
    config: DaemonConfig,
}

impl<H: HttpClient + 'static> PostgresRequestManager<H> {
    /// Create a new PostgreSQL request manager.
    ///
    /// # Arguments
    /// * `pool` - PostgreSQL connection pool
    /// * `http_client` - HTTP client for making requests
    /// * `config` - Daemon configuration (batch size, concurrency limits, etc.)
    pub fn new(pool: PgPool, http_client: Arc<H>, config: DaemonConfig) -> Self {
        let storage = Arc::new(PostgresStorage::new(pool));

        Self {
            storage,
            http_client,
            config,
        }
    }

    /// Create with default daemon configuration.
    pub fn with_defaults(pool: PgPool, http_client: Arc<H>) -> Self {
        Self::new(pool, http_client, DaemonConfig::default())
    }

    /// Get a reference to the underlying storage.
    pub fn storage(&self) -> &Arc<PostgresStorage> {
        &self.storage
    }
}

#[async_trait]
impl<H: HttpClient + 'static> RequestManager for PostgresRequestManager<H> {
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
        id_filter: Option<Vec<RequestId>>,
    ) -> std::pin::Pin<Box<dyn Stream<Item = Result<Result<AnyRequest>>> + Send>> {
        let storage = self.storage.clone();

        Box::pin(async_stream::stream! {
            // Create a listener for Postgres NOTIFY events
            let mut listener = match storage.create_listener().await {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to create listener");
                    yield Err(e);
                    return;
                }
            };

            // Listen on the request_updates channel
            if let Err(e) = listener.listen("request_updates").await {
                tracing::error!(error = %e, "Failed to listen on request_updates channel");
                yield Err(BatcherError::Other(anyhow::anyhow!("Failed to listen: {}", e)));
                return;
            }

            tracing::info!("Listening for request updates");

            loop {
                match listener.recv().await {
                    Ok(notification) => {
                        // Parse the JSON payload
                        let payload = notification.payload();

                        // The payload contains: { "id": "...", "state": "...", "updated_at": "..." }
                        // We need to parse the ID and fetch the full request from storage
                        let parsed: serde_json::Result<serde_json::Value> = serde_json::from_str(payload);

                        match parsed {
                            Ok(json) => {
                                if let Some(id_str) = json.get("id").and_then(|v| v.as_str()) {
                                    // Parse the UUID
                                    if let Ok(uuid) = uuid::Uuid::parse_str(id_str) {
                                        let request_id = RequestId(uuid);

                                        // Apply filter if specified
                                        if let Some(ref filter) = id_filter {
                                            if !filter.contains(&request_id) {
                                                // Skip this update - not in filter
                                                continue;
                                            }
                                        }

                                        // Fetch the full request from storage
                                        match storage.get_requests(vec![request_id]).await {
                                            Ok(results) => {
                                                if let Some(result) = results.into_iter().next() {
                                                    yield Ok(result);
                                                }
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    error = %e,
                                                    request_id = %request_id,
                                                    "Failed to fetch request after notification"
                                                );
                                                yield Err(e);
                                            }
                                        }
                                    } else {
                                        tracing::warn!(id_str = id_str, "Failed to parse UUID from notification");
                                    }
                                } else {
                                    tracing::warn!(payload = payload, "Notification payload missing 'id' field");
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    payload = payload,
                                    "Failed to parse notification payload"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Error receiving notification");
                        yield Err(BatcherError::Other(anyhow::anyhow!("Notification error: {}", e)));
                        // Don't return - keep trying to receive notifications
                    }
                }
            }
        })
    }

    fn run(&self) -> Result<JoinHandle<Result<()>>> {
        tracing::info!("Starting PostgreSQL request manager daemon");

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::ReqwestHttpClient;
    use crate::request::RequestData;
    use uuid::Uuid;

    async fn create_test_pool() -> PgPool {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
        PgPool::connect(&database_url)
            .await
            .expect("Failed to connect to test database")
    }

    #[tokio::test]
    #[ignore] // Run with: cargo test --features postgres -- --ignored
    async fn test_submit_and_get_status() {
        let pool = create_test_pool().await;
        let http_client = Arc::new(ReqwestHttpClient::new());
        let manager = PostgresRequestManager::with_defaults(pool, http_client);

        let request_id = RequestId(Uuid::new_v4());
        let request = Request {
            state: Pending {
                retry_attempt: 0,
                not_before: None,
            },
            data: RequestData {
                id: request_id,
                endpoint: "https://api.example.com".to_string(),
                method: "POST".to_string(),
                path: "/v1/test".to_string(),
                body: r#"{"key": "value"}"#.to_string(),
                model: "test-model".to_string(),
                api_key: "test-key".to_string(),
            },
        };

        let results = manager.submit_requests(vec![request]).await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_ok());

        let status_results = manager.get_status(vec![request_id]).await.unwrap();
        assert_eq!(status_results.len(), 1);
        assert!(status_results[0].is_ok());

        let status = status_results[0].as_ref().unwrap();
        assert!(status.is_pending());
    }
}
