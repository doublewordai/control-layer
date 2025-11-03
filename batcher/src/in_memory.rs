//! In-memory implementation of the Batcher trait for testing.

use crate::{
    Batcher, BatcherError, DaemonOperations, Request, RequestContext, RequestId, RequestStatus,
    Result,
};
use chrono::Utc;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// An in-memory storage entry for a request.
#[derive(Debug, Clone)]
struct RequestEntry {
    #[allow(dead_code)] // Will be used in future phases for request processing
    request: Request,
    #[allow(dead_code)] // Will be used in future phases for retry logic
    context: RequestContext,
    status: RequestStatus,
}

/// In-memory implementation of the Batcher trait.
///
/// This implementation stores all requests in memory using a HashMap and is primarily
/// intended for testing purposes. It provides immediate "success" responses for
/// submitted requests.
#[derive(Debug, Clone)]
pub struct InMemoryBatcher {
    storage: Arc<Mutex<HashMap<RequestId, RequestEntry>>>,
}

impl InMemoryBatcher {
    /// Create a new in-memory batcher.
    pub fn new() -> Self {
        Self {
            storage: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get the number of requests currently stored.
    pub fn len(&self) -> usize {
        self.storage.lock().unwrap().len()
    }

    /// Check if the storage is empty.
    pub fn is_empty(&self) -> bool {
        self.storage.lock().unwrap().is_empty()
    }

    /// Clear all stored requests (useful for testing).
    pub fn clear(&self) {
        self.storage.lock().unwrap().clear();
    }
}

impl Default for InMemoryBatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Batcher for InMemoryBatcher {
    fn submit_requests(
        &self,
        requests: Vec<(Request, RequestContext)>,
    ) -> impl std::future::Future<Output = Result<Vec<RequestId>>> + Send {
        let storage = self.storage.clone();

        async move {
            let mut storage = storage.lock().unwrap();
            let mut ids = Vec::with_capacity(requests.len());

            for (request, context) in requests {
                let id = RequestId::new();
                let entry = RequestEntry {
                    request,
                    context,
                    status: RequestStatus::Pending,
                };
                storage.insert(id, entry);
                ids.push(id);
            }

            Ok(ids)
        }
    }

    fn cancel_requests(
        &self,
        ids: Vec<RequestId>,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        let storage = self.storage.clone();

        async move {
            let mut storage = storage.lock().unwrap();

            for id in ids {
                if let Some(entry) = storage.get_mut(&id) {
                    // Only cancel if the request is still active
                    if entry.status.is_active() {
                        entry.status = RequestStatus::Canceled {
                            canceled_at: Utc::now(),
                        };
                    }
                } else {
                    return Err(BatcherError::RequestNotFound(id.to_string()));
                }
            }

            Ok(())
        }
    }

    fn get_status(
        &self,
        ids: Vec<RequestId>,
    ) -> impl std::future::Future<Output = Result<Vec<(RequestId, RequestStatus)>>> + Send {
        let storage = self.storage.clone();

        async move {
            let storage = storage.lock().unwrap();
            let mut results = Vec::with_capacity(ids.len());

            for id in ids {
                if let Some(entry) = storage.get(&id) {
                    results.push((id, entry.status.clone()));
                } else {
                    return Err(BatcherError::RequestNotFound(id.to_string()));
                }
            }

            Ok(results)
        }
    }
}

impl DaemonOperations for InMemoryBatcher {
    fn poll_pending(
        &self,
        model: Option<&str>,
        limit: usize,
        daemon_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<(RequestId, Request, RequestContext)>>> + Send
    {
        let storage = self.storage.clone();
        let model = model.map(|s| s.to_string());
        let daemon_id = daemon_id.to_string();

        async move {
            let mut storage = storage.lock().unwrap();
            let mut results = Vec::new();

            for (id, entry) in storage.iter_mut() {
                if matches!(entry.status, RequestStatus::Pending) {
                    // Filter by model if specified
                    if let Some(ref target_model) = model {
                        if &entry.request.model != target_model {
                            continue;
                        }
                    }

                    // Atomically claim this request by transitioning to PendingProcessing
                    entry.status = RequestStatus::PendingProcessing {
                        daemon_id: daemon_id.clone(),
                        acquired_at: chrono::Utc::now(),
                    };

                    results.push((*id, entry.request.clone(), entry.context.clone()));
                    if results.len() >= limit {
                        break;
                    }
                }
            }

            Ok(results)
        }
    }

    fn update_status(
        &self,
        id: RequestId,
        status: RequestStatus,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        let storage = self.storage.clone();

        async move {
            let mut storage = storage.lock().unwrap();

            if let Some(entry) = storage.get_mut(&id) {
                entry.status = status;
                Ok(())
            } else {
                Err(BatcherError::RequestNotFound(id.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_request() -> Request {
        Request {
            endpoint: "https://api.example.com".to_string(),
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            body: r#"{"model": "gpt-4"}"#.to_string(),
            api_key: "sk-test123".to_string(),
            model: "gpt-4".to_string(),
        }
    }

    #[tokio::test]
    async fn test_submit_single_request() {
        let batcher = InMemoryBatcher::new();
        let request = create_test_request();
        let context = RequestContext::default();

        let ids = batcher
            .submit_requests(vec![(request, context)])
            .await
            .unwrap();

        assert_eq!(ids.len(), 1);
        assert_eq!(batcher.len(), 1);
    }

    #[tokio::test]
    async fn test_submit_multiple_requests() {
        let batcher = InMemoryBatcher::new();
        let requests = vec![
            (create_test_request(), RequestContext::default()),
            (create_test_request(), RequestContext::default()),
            (create_test_request(), RequestContext::default()),
        ];

        let ids = batcher.submit_requests(requests).await.unwrap();

        assert_eq!(ids.len(), 3);
        assert_eq!(batcher.len(), 3);

        // All IDs should be unique
        let unique_ids: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique_ids.len(), 3);
    }

    #[tokio::test]
    async fn test_get_status() {
        let batcher = InMemoryBatcher::new();
        let request = create_test_request();
        let context = RequestContext::default();

        let ids = batcher
            .submit_requests(vec![(request, context)])
            .await
            .unwrap();

        let statuses = batcher.get_status(ids.clone()).await.unwrap();

        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].0, ids[0]);
        assert!(matches!(statuses[0].1, RequestStatus::Pending));
    }

    #[tokio::test]
    async fn test_get_status_multiple() {
        let batcher = InMemoryBatcher::new();
        let requests = vec![
            (create_test_request(), RequestContext::default()),
            (create_test_request(), RequestContext::default()),
        ];

        let ids = batcher.submit_requests(requests).await.unwrap();
        let statuses = batcher.get_status(ids.clone()).await.unwrap();

        assert_eq!(statuses.len(), 2);
        for (id, status) in &statuses {
            assert!(ids.contains(id));
            assert!(matches!(status, RequestStatus::Pending));
        }
    }

    #[tokio::test]
    async fn test_get_status_not_found() {
        let batcher = InMemoryBatcher::new();
        let fake_id = RequestId::new();

        let result = batcher.get_status(vec![fake_id]).await;

        assert!(matches!(result, Err(BatcherError::RequestNotFound(_))));
    }

    #[tokio::test]
    async fn test_cancel_request() {
        let batcher = InMemoryBatcher::new();
        let request = create_test_request();
        let context = RequestContext::default();

        let ids = batcher
            .submit_requests(vec![(request, context)])
            .await
            .unwrap();

        batcher.cancel_requests(ids.clone()).await.unwrap();

        let statuses = batcher.get_status(ids).await.unwrap();
        assert!(matches!(
            statuses[0].1,
            RequestStatus::Canceled { .. }
        ));
    }

    #[tokio::test]
    async fn test_cancel_multiple_requests() {
        let batcher = InMemoryBatcher::new();
        let requests = vec![
            (create_test_request(), RequestContext::default()),
            (create_test_request(), RequestContext::default()),
            (create_test_request(), RequestContext::default()),
        ];

        let ids = batcher.submit_requests(requests).await.unwrap();
        batcher.cancel_requests(ids.clone()).await.unwrap();

        let statuses = batcher.get_status(ids).await.unwrap();
        for (_, status) in statuses {
            assert!(matches!(status, RequestStatus::Canceled { .. }));
        }
    }

    #[tokio::test]
    async fn test_cancel_not_found() {
        let batcher = InMemoryBatcher::new();
        let fake_id = RequestId::new();

        let result = batcher.cancel_requests(vec![fake_id]).await;

        assert!(matches!(result, Err(BatcherError::RequestNotFound(_))));
    }

    #[tokio::test]
    async fn test_clear() {
        let batcher = InMemoryBatcher::new();
        let requests = vec![
            (create_test_request(), RequestContext::default()),
            (create_test_request(), RequestContext::default()),
        ];

        batcher.submit_requests(requests).await.unwrap();
        assert_eq!(batcher.len(), 2);

        batcher.clear();
        assert_eq!(batcher.len(), 0);
        assert!(batcher.is_empty());
    }

    #[tokio::test]
    async fn test_concurrent_submissions() {
        let batcher = InMemoryBatcher::new();
        let batcher_clone1 = batcher.clone();
        let batcher_clone2 = batcher.clone();

        let handle1 = tokio::spawn(async move {
            let requests = vec![
                (create_test_request(), RequestContext::default()),
                (create_test_request(), RequestContext::default()),
            ];
            batcher_clone1.submit_requests(requests).await.unwrap()
        });

        let handle2 = tokio::spawn(async move {
            let requests = vec![
                (create_test_request(), RequestContext::default()),
                (create_test_request(), RequestContext::default()),
            ];
            batcher_clone2.submit_requests(requests).await.unwrap()
        });

        let ids1 = handle1.await.unwrap();
        let ids2 = handle2.await.unwrap();

        assert_eq!(ids1.len(), 2);
        assert_eq!(ids2.len(), 2);
        assert_eq!(batcher.len(), 4);

        // All IDs should be unique
        let all_ids: Vec<_> = ids1.into_iter().chain(ids2.into_iter()).collect();
        let unique_ids: std::collections::HashSet<_> = all_ids.iter().collect();
        assert_eq!(unique_ids.len(), 4);
    }

    // Tests for DaemonOperations trait

    #[tokio::test]
    async fn test_poll_pending() {
        let batcher = InMemoryBatcher::new();
        let requests = vec![
            (create_test_request(), RequestContext::default()),
            (create_test_request(), RequestContext::default()),
            (create_test_request(), RequestContext::default()),
        ];

        batcher.submit_requests(requests).await.unwrap();

        let pending = batcher.poll_pending(None, 10, "test-daemon").await.unwrap();
        assert_eq!(pending.len(), 3);
    }

    #[tokio::test]
    async fn test_poll_pending_with_limit() {
        let batcher = InMemoryBatcher::new();
        let requests = vec![
            (create_test_request(), RequestContext::default()),
            (create_test_request(), RequestContext::default()),
            (create_test_request(), RequestContext::default()),
        ];

        batcher.submit_requests(requests).await.unwrap();

        let pending = batcher.poll_pending(None, 2, "test-daemon").await.unwrap();
        assert_eq!(pending.len(), 2);
    }

    #[tokio::test]
    async fn test_poll_pending_filters_non_pending() {
        let batcher = InMemoryBatcher::new();
        let requests = vec![
            (create_test_request(), RequestContext::default()),
            (create_test_request(), RequestContext::default()),
        ];

        let ids = batcher.submit_requests(requests).await.unwrap();

        // Cancel one request
        batcher.cancel_requests(vec![ids[0]]).await.unwrap();

        // Only one should be pending
        let pending = batcher.poll_pending(None, 10, "test-daemon").await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, ids[1]);
    }

    #[tokio::test]
    async fn test_update_status() {
        let batcher = InMemoryBatcher::new();
        let requests = vec![(create_test_request(), RequestContext::default())];

        let ids = batcher.submit_requests(requests).await.unwrap();
        let id = ids[0];

        // Update to Processing
        batcher
            .update_status(
                id,
                RequestStatus::Processing {
                    daemon_id: "daemon-1".to_string(),
                    acquired_at: Utc::now(),
                },
            )
            .await
            .unwrap();

        let statuses = batcher.get_status(vec![id]).await.unwrap();
        assert!(matches!(statuses[0].1, RequestStatus::Processing { .. }));

        // Update to Completed
        batcher
            .update_status(
                id,
                RequestStatus::Completed {
                    response_status: 200,
                    response_body: "{}".to_string(),
                    completed_at: Utc::now(),
                },
            )
            .await
            .unwrap();

        let statuses = batcher.get_status(vec![id]).await.unwrap();
        assert!(matches!(statuses[0].1, RequestStatus::Completed { .. }));
    }

    #[tokio::test]
    async fn test_update_status_not_found() {
        let batcher = InMemoryBatcher::new();
        let fake_id = RequestId::new();

        let result = batcher
            .update_status(fake_id, RequestStatus::Pending)
            .await;

        assert!(matches!(result, Err(BatcherError::RequestNotFound(_))));
    }
}
