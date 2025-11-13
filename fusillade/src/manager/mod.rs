//! Main traits for the batching system.
//!
//! This module defines the `Storage` and `RequestManager` traits, which provide the interface
//! for persisting requests, creating files, launching batches, and checking execution status.

use crate::batch::{
    Batch, BatchId, BatchInput, BatchStatus, File, FileContentItem, FileFilter, FileId,
    FileStreamItem, OutputFileType, RequestTemplateInput,
};
use crate::daemon::{AnyDaemonRecord, DaemonRecord, DaemonState, DaemonStatus};
use crate::error::Result;
use crate::http::HttpClient;
use crate::request::{AnyRequest, Claimed, DaemonId, Request, RequestId, RequestState};
use async_trait::async_trait;
use futures::stream::Stream;
use std::pin::Pin;
use std::sync::Arc;
use tokio::task::JoinHandle;

#[cfg(feature = "postgres")]
pub mod postgres;

/// Storage trait for persisting and querying requests.
///
/// This trait provides atomic operations for request lifecycle management.
/// The type system ensures valid state transitions, so implementations don't
/// need to validate them.
#[async_trait]
pub trait Storage: Send + Sync {
    /// Create a new file with templates.
    async fn create_file(
        &self,
        name: String,
        description: Option<String>,
        templates: Vec<RequestTemplateInput>,
    ) -> Result<FileId>;

    /// Create a new file with templates from a stream.
    ///
    /// The stream yields FileStreamItem which can be either:
    /// - Metadata: File metadata (can appear anywhere, will be accumulated)
    /// - Template: Request templates (processed as they arrive)
    async fn create_file_stream<S: Stream<Item = FileStreamItem> + Send + Unpin>(
        &self,
        stream: S,
    ) -> Result<FileId>;

    /// Get a file by ID.
    async fn get_file(&self, file_id: FileId) -> Result<File>;

    /// List files with optional filtering.
    async fn list_files(&self, filter: FileFilter) -> Result<Vec<File>>;

    /// Get all content for a file.
    async fn get_file_content(&self, file_id: FileId) -> Result<Vec<FileContentItem>>;

    /// Stream file content.
    /// Returns different content types based on the file's purpose:
    /// - Regular files (purpose='batch'): RequestTemplateInput
    /// - Batch output files (purpose='batch_output'): BatchOutputItem
    /// - Batch error files (purpose='batch_error'): BatchErrorItem
    ///
    /// The offset parameter allows skipping the first N lines (0-indexed).
    fn get_file_content_stream(
        &self,
        file_id: FileId,
        offset: usize,
    ) -> Pin<Box<dyn Stream<Item = Result<FileContentItem>> + Send>>;

    /// Delete a file (cascades to batches and executions).
    async fn delete_file(&self, file_id: FileId) -> Result<()>;

    /// Create a batch from a file's current templates.
    /// This will spawn requests in the Pending state for all templates in the file.
    async fn create_batch(&self, input: BatchInput) -> Result<Batch>;

    /// Get a batch by ID.
    async fn get_batch(&self, batch_id: BatchId) -> Result<Batch>;

    /// Get batch status.
    async fn get_batch_status(&self, batch_id: BatchId) -> Result<BatchStatus>;

    /// List all batches for a file.
    async fn list_file_batches(&self, file_id: FileId) -> Result<Vec<BatchStatus>>;

    /// List batches with optional filtering by creator and cursor-based pagination.
    /// Returns batches sorted by created_at DESC.
    /// The `after` parameter is a cursor for pagination (returns batches created before this ID).
    async fn list_batches(
        &self,
        created_by: Option<String>,
        after: Option<BatchId>,
        limit: i64,
    ) -> Result<Vec<Batch>>;

    /// Get a batch by its output or error file ID.
    /// Uses indexed lookup on output_file_id or error_file_id.
    async fn get_batch_by_output_file_id(
        &self,
        file_id: FileId,
        file_type: OutputFileType,
    ) -> Result<Option<Batch>>;

    /// Get all requests for a batch.
    async fn get_batch_requests(&self, batch_id: BatchId) -> Result<Vec<AnyRequest>>;

    /// Cancel all pending/in-progress requests for a batch.
    async fn cancel_batch(&self, batch_id: BatchId) -> Result<()>;

    /// The following methods are defined specifically for requests - i.e. independent of the
    /// files/batches they belong to.
    ///
    /// Cancel one or more individual pending or in-progress requests.
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
    #[tracing::instrument(skip(self, ids), fields(count = ids.len()))]
    async fn cancel_requests(&self, ids: Vec<RequestId>) -> Result<Vec<Result<()>>> {
        tracing::info!(count = ids.len(), "Cancelling requests");

        let mut results = Vec::new();

        for id in ids {
            // Get the request from storage
            let get_results = self.get_requests(vec![id]).await?;
            let request_result = get_results.into_iter().next().unwrap();

            let result = match request_result {
                Ok(any_request) => match any_request {
                    AnyRequest::Pending(req) => {
                        req.cancel(self).await?;
                        Ok(())
                    }
                    AnyRequest::Claimed(req) => {
                        req.cancel(self).await?;
                        Ok(())
                    }
                    AnyRequest::Processing(req) => {
                        req.cancel(self).await?;
                        Ok(())
                    }
                    AnyRequest::Completed(_) | AnyRequest::Failed(_) | AnyRequest::Canceled(_) => {
                        Err(crate::error::FusilladeError::InvalidState(
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

    /// Get in progress requests by IDs.
    async fn get_requests(&self, ids: Vec<RequestId>) -> Result<Vec<Result<AnyRequest>>>;

    // These methods are used by the DaemonExecutor for pulling requests, and then persisting their
    // states as they iterate through them

    /// Atomically claim pending requests for processing.
    async fn claim_requests(
        &self,
        limit: usize,
        daemon_id: DaemonId,
    ) -> Result<Vec<Request<Claimed>>>;

    /// Update an existing request's state in storage.
    async fn persist<T: RequestState + Clone>(&self, request: &Request<T>) -> Result<()>
    where
        AnyRequest: From<Request<T>>;
}

/// Daemon lifecycle persistence.
///
/// This trait provides storage operations for tracking daemon state,
/// including registration, heartbeat updates, and graceful shutdown.
#[async_trait]
pub trait DaemonStorage: Send + Sync {
    /// Persist daemon state update.
    ///
    /// This is a low-level method used by state transition methods.
    /// The type parameter `T` ensures type-safe state transitions.
    async fn persist_daemon<T: DaemonState + Clone>(&self, record: &DaemonRecord<T>) -> Result<()>
    where
        AnyDaemonRecord: From<DaemonRecord<T>>;

    /// Get daemon by ID.
    ///
    /// Returns an `AnyDaemonRecord` which can hold the daemon in any state.
    async fn get_daemon(&self, daemon_id: DaemonId) -> Result<AnyDaemonRecord>;

    /// List all daemons with optional status filter.
    ///
    /// If `status_filter` is `None`, returns all daemons regardless of status.
    /// Otherwise, returns only daemons matching the specified status.
    async fn list_daemons(
        &self,
        status_filter: Option<DaemonStatus>,
    ) -> Result<Vec<AnyDaemonRecord>>;
}

/// Daemon executor trait for runtime orchestration.
///
/// This trait handles only the daemon lifecycle - spawning the background worker
/// that processes requests. All data operations are on the Storage trait.
#[async_trait]
pub trait DaemonExecutor<H: HttpClient>: Storage + Send + Sync {
    /// Get a reference to the HTTP client.
    fn http_client(&self) -> &Arc<H>;

    /// Get a reference to the daemon configuration.
    fn config(&self) -> &crate::daemon::DaemonConfig;

    /// Run the daemon thread.
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
    /// let manager = Arc::new(manager);
    /// let handle = manager.run()?;
    ///
    /// // Do work...
    ///
    /// // Shutdown gracefully (implementation-specific)
    /// handle.abort();
    /// ```
    fn run(
        self: Arc<Self>,
        shutdown_token: tokio_util::sync::CancellationToken,
    ) -> Result<JoinHandle<Result<()>>>;

    // File and Batch Management methods are inherited from the Storage trait
}
