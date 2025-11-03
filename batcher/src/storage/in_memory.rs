//! In-memory storage implementation for requests.
//!
//! This implementation stores all requests in memory using concurrent data structures.
//! It's suitable for testing and single-process deployments. Requests are lost on restart.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::error::{BatcherError, Result};
use crate::request::*;

use super::Storage;

/// Stored request with its context.
#[derive(Clone)]
struct StoredRequest {
    request: AnyRequest,
    #[allow(dead_code)] // Context stored for future use (e.g., retry logic, monitoring)
    context: RequestContext,
}

/// In-memory implementation of the Storage trait.
///
/// Stores all requests in a concurrent HashMap and validates state transitions
/// to prevent race conditions when multiple daemons operate concurrently.
///
/// # Example
/// ```ignore
/// let storage = InMemoryStorage::new();
///
/// // Submit a pending request
/// let request = Request {
///     state: Pending { info: PendingInfo {} },
///     data: request_data,
/// };
/// storage.persist(&request).await?;
///
/// // Try to claim it
/// let claimed = request.claim(daemon_id, &storage).await?;
/// ```
#[derive(Clone)]
pub struct InMemoryStorage {
    requests: Arc<RwLock<HashMap<RequestId, StoredRequest>>>,
}

impl InMemoryStorage {
    /// Create a new in-memory storage.
    pub fn new() -> Self {
        Self {
            requests: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl Storage for InMemoryStorage {
    async fn submit(&self, request: Request<Pending>, context: RequestContext) -> Result<()> {
        let request_id = request.data.id;

        let mut requests = self.requests.write();

        // Check if request already exists
        if requests.contains_key(&request_id) {
            return Err(BatcherError::InvalidState(
                request_id,
                "exists".to_string(),
                "new".to_string(),
            ));
        }

        requests.insert(
            request_id,
            StoredRequest {
                request: request.into(),
                context,
            },
        );
        Ok(())
    }

    async fn claim_requests(
        &self,
        limit: usize,
        daemon_id: DaemonId,
    ) -> Result<Vec<Request<Claimed>>> {
        let mut requests = self.requests.write();
        let now = chrono::Utc::now();

        let pending_ids: Vec<RequestId> = requests
            .iter()
            .filter(|(_, stored)| stored.request.is_pending())
            .take(limit)
            .map(|(id, _)| *id)
            .collect();

        let mut claimed_requests = Vec::new();

        for id in pending_ids {
            if let Some(stored) = requests.get_mut(&id) {
                // Extract the pending request and transition to claimed
                if let Some(pending_req) = stored.request.as_pending() {
                    let claimed_req = Request {
                        state: Claimed {
                            daemon_id,
                            claimed_at: now,
                        },
                        data: pending_req.data.clone(),
                    };

                    // Update storage
                    stored.request = claimed_req.clone().into();

                    // Return the claimed request
                    claimed_requests.push(claimed_req);
                }
            }
        }

        Ok(claimed_requests)
    }

    async fn persist<T: RequestState + Clone>(&self, request: &Request<T>) -> Result<()>
    where
        AnyRequest: From<Request<T>>,
    {
        let request_id = request.data.id;

        let mut requests = self.requests.write();

        if let Some(existing) = requests.get_mut(&request_id) {
            // Don't overwrite terminal states (idempotency protection)
            if existing.request.is_terminal() {
                return Err(BatcherError::InvalidState(
                    request_id,
                    "terminal state".to_string(),
                    "modifiable state".to_string(),
                ));
            }

            // Update the stored request
            existing.request = request.clone().into();
            Ok(())
        } else {
            Err(BatcherError::RequestNotFound(request_id))
        }
    }

    async fn view_pending_requests(
        &self,
        limit: usize,
        _daemon_id: Option<DaemonId>,
    ) -> Result<Vec<Request<Pending>>> {
        let requests = self.requests.read();

        let pending: Vec<Request<Pending>> = requests
            .values()
            .filter_map(|stored| stored.request.as_pending().cloned())
            .take(limit)
            .collect();

        Ok(pending)
    }

    async fn get_requests(&self, ids: Vec<RequestId>) -> Result<Vec<Result<AnyRequest>>> {
        let requests = self.requests.read();

        let results = ids
            .into_iter()
            .map(|id| {
                requests
                    .get(&id)
                    .map(|stored| stored.request.clone())
                    .ok_or_else(|| BatcherError::RequestNotFound(id))
            })
            .collect();

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request_data(id: RequestId) -> RequestData {
        RequestData {
            id,
            endpoint: "https://api.example.com".to_string(),
            method: "POST".to_string(),
            path: "/v1/test".to_string(),
            body: r#"{"test": true}"#.to_string(),
            model: "test-model".to_string(),
        }
    }

    fn sample_context() -> RequestContext {
        RequestContext {
            max_retries: 3,
            backoff_ms: 100,
            backoff_factor: 2,
            max_backoff_ms: 10000,
            timeout_ms: 30000,
            api_key: "test-key".to_string(),
        }
    }

    #[tokio::test]
    async fn test_submit_new_pending_request() {
        let storage = InMemoryStorage::new();
        let id = uuid::Uuid::new_v4();

        let request = Request {
            state: Pending {},
            data: sample_request_data(id),
        };

        let result = storage.submit(request, sample_context()).await;
        assert!(result.is_ok());

        // Verify it's stored
        let pending = storage.view_pending_requests(10, None).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].data.id, id);
    }

    #[tokio::test]
    async fn test_claim_requests_atomically() {
        let storage = InMemoryStorage::new();
        let daemon1 = uuid::Uuid::new_v4();
        let daemon2 = uuid::Uuid::new_v4();

        // Submit two pending requests
        let id1 = uuid::Uuid::new_v4();
        let id2 = uuid::Uuid::new_v4();

        let request1 = Request {
            state: Pending {},
            data: sample_request_data(id1),
        };
        storage.submit(request1, sample_context()).await.unwrap();

        let request2 = Request {
            state: Pending {},
            data: sample_request_data(id2),
        };
        storage.submit(request2, sample_context()).await.unwrap();

        // Daemon 1 claims both
        let claimed = storage.claim_requests(10, daemon1).await.unwrap();
        assert_eq!(claimed.len(), 2);
        assert_eq!(claimed[0].state.daemon_id, daemon1);
        assert_eq!(claimed[1].state.daemon_id, daemon1);

        // Daemon 2 tries to claim - should get nothing
        let claimed2 = storage.claim_requests(10, daemon2).await.unwrap();
        assert_eq!(claimed2.len(), 0);
    }

    #[tokio::test]
    async fn test_unclaim_and_reclaim() {
        let storage = InMemoryStorage::new();
        let daemon_id = uuid::Uuid::new_v4();
        let id = uuid::Uuid::new_v4();

        // Submit a pending request
        let request = Request {
            state: Pending {},
            data: sample_request_data(id),
        };
        storage.submit(request, sample_context()).await.unwrap();

        // Claim it
        let claimed = storage.claim_requests(1, daemon_id).await.unwrap();
        assert_eq!(claimed.len(), 1);

        // Unclaim it
        let unclaimed = Request {
            state: Pending {},
            data: sample_request_data(id),
        };
        storage.persist(&unclaimed).await.unwrap();

        // Should be claimable again
        let reclaimed = storage.claim_requests(1, daemon_id).await.unwrap();
        assert_eq!(reclaimed.len(), 1);
    }

    #[tokio::test]
    async fn test_persist_updates_state() {
        let storage = InMemoryStorage::new();
        let daemon_id = uuid::Uuid::new_v4();
        let id = uuid::Uuid::new_v4();

        // Submit and claim a request
        let request = Request {
            state: Pending {},
            data: sample_request_data(id),
        };
        storage.submit(request, sample_context()).await.unwrap();

        let claimed = storage.claim_requests(1, daemon_id).await.unwrap();
        assert_eq!(claimed.len(), 1);

        // Transition to completed
        let completed = Request {
            state: Completed {
                response_status: 200,
                response_body: "OK".to_string(),
                claimed_at: chrono::Utc::now(),
                started_at: chrono::Utc::now(),
                completed_at: chrono::Utc::now(),
            },
            data: sample_request_data(id),
        };
        storage.persist(&completed).await.unwrap();

        // Should no longer be pending
        let pending = storage.view_pending_requests(10, None).await.unwrap();
        assert_eq!(pending.len(), 0);
    }

    #[tokio::test]
    async fn test_cancel_pending_request() {
        let storage = InMemoryStorage::new();
        let id = uuid::Uuid::new_v4();

        // Submit a pending request
        let request = Request {
            state: Pending {},
            data: sample_request_data(id),
        };
        storage.submit(request.clone(), sample_context()).await.unwrap();

        // Cancel it
        let canceled = request.cancel(&storage).await.unwrap();
        assert_eq!(canceled.data.id, id);

        // Should no longer be pending
        let pending = storage.view_pending_requests(10, None).await.unwrap();
        assert_eq!(pending.len(), 0);

        // Should be retrievable as canceled
        let retrieved = storage.get_requests(vec![id]).await.unwrap();
        assert!(matches!(
            retrieved[0].as_ref().unwrap(),
            crate::request::AnyRequest::Canceled(_)
        ));
    }

    #[tokio::test]
    async fn test_cancel_claimed_request() {
        let storage = InMemoryStorage::new();
        let daemon_id = uuid::Uuid::new_v4();
        let id = uuid::Uuid::new_v4();

        // Submit and claim a request
        let request = Request {
            state: Pending {},
            data: sample_request_data(id),
        };
        storage.submit(request, sample_context()).await.unwrap();

        let claimed = storage.claim_requests(1, daemon_id).await.unwrap();
        assert_eq!(claimed.len(), 1);

        // Cancel it
        let canceled = claimed[0].clone().cancel(&storage).await.unwrap();
        assert_eq!(canceled.data.id, id);

        // Should be retrievable as canceled
        let retrieved = storage.get_requests(vec![id]).await.unwrap();
        assert!(matches!(
            retrieved[0].as_ref().unwrap(),
            crate::request::AnyRequest::Canceled(_)
        ));
    }
}
