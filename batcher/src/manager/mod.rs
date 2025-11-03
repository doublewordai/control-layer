//! Main trait for the batching system.
//!
//! This module defines the `RequestManager` trait, which provides the interface
//! for submitting, canceling, and checking the status of batched requests.

use crate::error::Result;
use crate::request::{AnyRequest, Pending, Request, RequestId};
use async_trait::async_trait;
use futures::stream::Stream;
use std::pin::Pin;
use tokio::task::JoinHandle;

pub mod in_memory;

/// Main trait for the batching system.
///
/// This trait defines the interface for submitting, canceling, and checking the status
/// of batched requests. Implementations can be backed by different storage mechanisms
/// (e.g., in-memory, PostgreSQL).
///
/// # Example
/// ```ignore
/// let manager = PostgresRequestManager::new(pool).await?;
///
/// // Submit a batch of requests
/// let requests = vec![(pending_request, request_context)];
/// let ids = manager.submit_requests(requests).await?;
///
/// // Check status
/// let statuses = manager.get_status(ids.clone()).await?;
///
/// // Cancel if needed
/// manager.cancel_requests(ids).await?;
/// ```
#[async_trait]
pub trait RequestManager: Send + Sync {
    /// Submit requests for processing.
    ///
    /// Users submit IDs in the request data. These should be used in the other methods.
    /// API keys and other request-specific data should be included in the RequestData.
    /// Retry behavior is configured at the daemon level.
    async fn submit_requests(&self, requests: Vec<Request<Pending>>) -> Result<Vec<Result<()>>>;

    /// Cancel one or more pending or processing requests.
    ///
    /// Requests that have already completed or failed cannot be canceled.
    /// This is a best-effort operation - some requests may have already been processed.
    ///
    /// Returns a result for each request ID indicating whether cancellation succeeded.
    ///
    /// # Errors
    /// Individual cancellation results may fail if:
    /// - Request ID doesn't exist
    /// - Request is already in a terminal state (completed/failed)
    async fn cancel_requests(&self, ids: Vec<RequestId>) -> Result<Vec<Result<()>>>;

    /// Get the current status of one or more requests.
    ///
    /// Returns the status for each requested ID. If a request ID doesn't exist,
    /// an error will be returned for that specific request.
    ///
    /// # Errors
    /// Individual status results may fail if:
    /// - Request ID doesn't exist
    /// - Database error occurs
    async fn get_status(&self, ids: Vec<RequestId>) -> Result<Vec<Result<AnyRequest>>>;

    /// Get a stream of updates for a set of requests.
    ///
    /// Returns requests as each request changes status.
    /// `None` for `id_filter` implies getting all status updates.
    ///
    /// The stream continues indefinitely, emitting updates as they occur.
    /// The outer Result represents stream-level errors (e.g., connection loss),
    /// while the inner Result represents per-update errors.
    ///
    /// # Example
    /// ```ignore
    /// let mut updates = manager.get_status_updates(Some(vec![request_id]));
    /// while let Some(result) = updates.next().await {
    ///     match result {
    ///         Ok(Ok(request)) => println!("Request {} updated: {:?}", request.id(), request),
    ///         Ok(Err(e)) => eprintln!("Update error: {}", e),
    ///         Err(e) => {
    ///             eprintln!("Stream error: {}", e);
    ///             break;
    ///         }
    ///     }
    /// }
    /// ```
    fn get_status_updates(
        &self,
        id_filter: Option<Vec<RequestId>>,
    ) -> Pin<Box<dyn Stream<Item = Result<Result<AnyRequest>>> + Send>>;

    /// Run the Request Manager daemon thread.
    ///
    /// This spawns a background task responsible for actually doing the work of moving
    /// requests from one state to another, and broadcasting those status changes.
    ///
    /// The daemon will:
    /// - Claim pending requests
    /// - Execute HTTP requests
    /// - Handle retries with exponential backoff
    /// - Update request statuses
    /// - Respect per-model concurrency limits
    ///
    /// # Errors
    /// Returns an error if the daemon fails to start.
    ///
    /// # Example
    /// ```ignore
    /// let handle = manager.run()?;
    ///
    /// // Do work...
    ///
    /// // Shutdown gracefully (implementation-specific)
    /// handle.abort();
    /// ```
    fn run(&self) -> Result<JoinHandle<Result<()>>>;
}
