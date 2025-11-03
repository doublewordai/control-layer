use std::future::Future;

use crate::error::Result;
use crate::request::{AnyRequest, Claimed, DaemonId, Pending, Request, RequestId, RequestState};

pub mod in_memory;

#[cfg(feature = "postgres")]
pub mod postgres;

/// Storage trait for persisting and querying requests.
///
/// This trait provides atomic operations for request lifecycle management.
/// The type system ensures valid state transitions, so implementations don't
/// need to validate them.
pub trait Storage: Send + Sync {
    /// Submit a new pending request to storage with its processing context.
    ///
    /// This is used for initial request submission. The request must be in `Pending` state.
    ///
    /// # Errors
    /// - If a request with the same ID already exists
    fn submit(&self, request: Request<Pending>) -> impl Future<Output = Result<()>> + Send;

    /// Atomically claim pending requests for processing.
    ///
    /// This operation transitions requests from `Pending` to `Claimed` state atomically,
    /// preventing race conditions when multiple daemons operate concurrently.
    ///
    /// # Arguments
    /// - `limit` - Maximum number of requests to claim
    /// - `daemon_id` - ID of the daemon claiming these requests
    ///
    /// # Returns
    /// Vector of successfully claimed requests. May return fewer than `limit` if
    /// insufficient pending requests are available.
    fn claim_requests(
        &self,
        limit: usize,
        daemon_id: DaemonId,
    ) -> impl Future<Output = Result<Vec<Request<Claimed>>>> + Send;

    /// Update an existing request's state in storage.
    ///
    /// The type system ensures valid state transitions, so this method just
    /// persists the new state without validation.
    ///
    /// # Errors
    /// - `RequestNotFound` - if the request doesn't exist
    fn persist<T: RequestState + Clone>(
        &self,
        request: &Request<T>,
    ) -> impl Future<Output = Result<()>> + Send
    where
        AnyRequest: From<Request<T>>;

    /// View the pending requests in the storage (read-only).
    ///
    /// This is a non-mutating query, useful for monitoring. For claiming requests,
    /// use `claim_requests` instead.
    ///
    /// # Arguments
    /// - `limit` - Maximum number of requests to return
    /// - `daemon_id` - Optional filter (implementation-specific)
    fn view_pending_requests(
        &self,
        limit: usize,
        daemon_id: Option<DaemonId>,
    ) -> impl Future<Output = Result<Vec<Request<Pending>>>> + Send;

    /// Get requests by IDs.
    ///
    /// Returns the current request (in whatever state) for each requested ID.
    ///
    /// # Arguments
    /// - `ids` - Vector of request IDs to retrieve
    ///
    /// # Returns
    /// Vector of Results - one for each ID. If a request doesn't exist,
    /// that entry will be an error.
    fn get_requests(
        &self,
        ids: Vec<RequestId>,
    ) -> impl Future<Output = Result<Vec<Result<AnyRequest>>>> + Send;
}
