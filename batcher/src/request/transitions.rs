use std::sync::Arc;

use tokio::sync::Mutex;

use crate::{error::Result, http::HttpClient, storage::Storage};

use super::types::{
    Canceled, Claimed, Completed, DaemonId, Failed, Pending, Processing, Request, RequestContext,
};

impl Request<Pending> {
    pub async fn claim<S: Storage>(
        self,
        daemon_id: DaemonId,
        storage: &S,
    ) -> Result<Request<Claimed>> {
        let request = Request {
            data: self.data,
            state: Claimed {
                daemon_id,
                claimed_at: chrono::Utc::now(),
            },
        };
        storage.persist(&request).await?;
        Ok(request)
    }

    pub async fn cancel<S: Storage>(self, storage: &S) -> Result<Request<Canceled>> {
        let request = Request {
            data: self.data,
            state: Canceled {
                canceled_at: chrono::Utc::now(),
            },
        };
        storage.persist(&request).await?;
        Ok(request)
    }
}

impl Request<Claimed> {
    pub async fn unclaim<S: Storage>(self, storage: &S) -> Result<Request<Pending>> {
        let request = Request {
            data: self.data,
            state: Pending {},
        };
        storage.persist(&request).await?;
        Ok(request)
    }

    pub async fn cancel<S: Storage>(self, storage: &S) -> Result<Request<Canceled>> {
        let request = Request {
            data: self.data,
            state: Canceled {
                canceled_at: chrono::Utc::now(),
            },
        };
        storage.persist(&request).await?;
        Ok(request)
    }

    pub async fn process<H: HttpClient + 'static, S: Storage>(
        self,
        http_client: H,
        context: RequestContext,
        storage: &S,
    ) -> Result<Request<Processing>> {
        let request_data = self.data.clone();
        let api_key = context.api_key.clone();
        let timeout_ms = context.timeout_ms;

        // Create a channel for the HTTP result
        let (tx, rx) = tokio::sync::mpsc::channel(1);

        // Spawn the HTTP request as an async task
        let task_handle = tokio::spawn(async move {
            let result = http_client
                .execute(&request_data, &api_key, timeout_ms)
                .await;
            let _ = tx.send(result).await; // Ignore send errors (receiver dropped)
        });

        let processing_state = Processing {
            daemon_id: self.state.daemon_id,
            claimed_at: self.state.claimed_at,
            started_at: chrono::Utc::now(),
            result_rx: Arc::new(Mutex::new(rx)),
            abort_handle: task_handle.abort_handle(),
        };

        let request = Request {
            data: self.data,
            state: processing_state,
        };

        // Persist the Processing state so we can cancel it if needed
        // If persist fails, abort the spawned HTTP task
        if let Err(e) = storage.persist(&request).await {
            request.state.abort_handle.abort();
            return Err(e);
        }

        Ok(request)
    }
}

impl Request<Processing> {
    /// Wait for the HTTP request to complete.
    ///
    /// This method awaits the result from the spawned HTTP task and transitions
    /// the request to either `Completed` or `Failed` state.
    ///
    /// Returns:
    /// - `Ok(completed_request)` if the HTTP request succeeded
    /// - `Err(failed_request)` if the HTTP request failed
    pub async fn complete<S: Storage>(
        self,
        storage: &S,
    ) -> Result<std::result::Result<Request<Completed>, Request<Failed>>> {
        // Await the result from the channel (lock the mutex to access the receiver)
        let result = {
            let mut rx = self.state.result_rx.lock().await;
            rx.recv().await
        };

        match result {
            Some(Ok(http_response)) => {
                // HTTP request completed successfully
                let completed_state = Completed {
                    response_status: http_response.status,
                    response_body: http_response.body,
                    claimed_at: self.state.claimed_at,
                    started_at: self.state.started_at,
                    completed_at: chrono::Utc::now(),
                };
                let request = Request {
                    data: self.data,
                    state: completed_state,
                };
                storage.persist(&request).await?;
                Ok(Ok(request))
            }
            Some(Err(e)) => {
                // HTTP request failed
                let failed_state = Failed {
                    error: crate::error::error_serialization::serialize_error(&e.into()),
                    failed_at: chrono::Utc::now(),
                };
                let request = Request {
                    data: self.data,
                    state: failed_state,
                };
                storage.persist(&request).await?;
                Ok(Err(request))
            }
            None => {
                // Channel closed - task died without sending a result
                let failed_state = Failed {
                    error: "HTTP task terminated unexpectedly".to_string(),
                    failed_at: chrono::Utc::now(),
                };
                let request = Request {
                    data: self.data,
                    state: failed_state,
                };
                storage.persist(&request).await?;
                Ok(Err(request))
            }
        }
    }

    pub async fn cancel<S: Storage>(self, storage: &S) -> Result<Request<Canceled>> {
        // Abort the in-flight HTTP request
        self.state.abort_handle.abort();

        let request = Request {
            data: self.data,
            state: Canceled {
                canceled_at: chrono::Utc::now(),
            },
        };
        storage.persist(&request).await?;
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use crate::{
        request::types::{DaemonId, Pending, Request, RequestContext, RequestData, RequestId},
        storage::{in_memory::InMemoryStorage, Storage},
    };

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
    async fn test_pending_to_claimed_transition() {
        // Setup
        let storage = InMemoryStorage::new();
        let request_id = RequestId::from(Uuid::new_v4());
        let daemon_id = DaemonId::from(Uuid::new_v4());

        let pending_request = Request {
            state: Pending {},
            data: sample_request_data(request_id),
        };

        // Submit the pending request first
        storage
            .submit(pending_request.clone(), sample_context())
            .await
            .unwrap();

        // Act: Claim the request
        let claimed_request = pending_request.claim(daemon_id, &storage).await.unwrap();

        // Assert: Verify the state transition
        assert_eq!(claimed_request.state.daemon_id, daemon_id);
        assert_eq!(claimed_request.data.id, request_id);
        assert!(claimed_request.state.claimed_at <= chrono::Utc::now());

        // Assert: Verify storage was updated
        let stored_requests = storage.get_requests(vec![request_id]).await.unwrap();
        assert_eq!(stored_requests.len(), 1);

        // Verify it's now in claimed state
        match &stored_requests[0] {
            Ok(crate::request::types::AnyRequest::Claimed(req)) => {
                assert_eq!(req.state.daemon_id, daemon_id);
                assert_eq!(req.data.id, request_id);
            }
            _ => panic!("Expected request to be in Claimed state"),
        }

        // Assert: Verify it's no longer in pending
        let pending = storage.view_pending_requests(10, None).await.unwrap();
        assert_eq!(pending.len(), 0);
    }

    #[tokio::test]
    async fn test_claimed_to_processing_transition() {
        // Setup
        let storage = InMemoryStorage::new();
        let request_id = RequestId::from(Uuid::new_v4());
        let daemon_id = DaemonId::from(Uuid::new_v4());

        let pending_request = Request {
            state: Pending {},
            data: sample_request_data(request_id),
        };

        // Submit and claim the request
        storage
            .submit(pending_request.clone(), sample_context())
            .await
            .unwrap();
        let claimed_request = pending_request.claim(daemon_id, &storage).await.unwrap();

        // Create a mock HTTP client with a trigger (so we can control when it completes)
        let mock_client = crate::http::MockHttpClient::new();
        let _trigger = mock_client.add_response_with_trigger(
            "POST /v1/test",
            Ok(crate::http::HttpResponse {
                status: 200,
                body: r#"{"result": "success"}"#.to_string(),
            }),
        );

        let claimed_at = claimed_request.state.claimed_at;

        // Act: Process the request
        let processing_request = claimed_request
            .process(mock_client, sample_context(), &storage)
            .await
            .unwrap();

        // Assert: Verify the state transition
        assert_eq!(processing_request.state.daemon_id, daemon_id);
        assert_eq!(processing_request.data.id, request_id);
        assert_eq!(processing_request.state.claimed_at, claimed_at);
        assert!(processing_request.state.started_at <= chrono::Utc::now());

        // Assert: Verify storage was updated to Processing state
        let stored_requests = storage.get_requests(vec![request_id]).await.unwrap();
        assert_eq!(stored_requests.len(), 1);
        match &stored_requests[0] {
            Ok(crate::request::types::AnyRequest::Processing(req)) => {
                assert_eq!(req.state.daemon_id, daemon_id);
                assert_eq!(req.data.id, request_id);
            }
            _ => panic!("Expected request to be in Processing state"),
        }

        // Note: We don't trigger completion here - the HTTP task will be aborted
        // when processing_request is dropped, which is fine for this test
    }

    #[tokio::test]
    async fn test_processing_to_completed_transition() {
        // Setup
        let storage = InMemoryStorage::new();
        let request_id = RequestId::from(Uuid::new_v4());
        let daemon_id = DaemonId::from(Uuid::new_v4());

        let pending_request = Request {
            state: Pending {},
            data: sample_request_data(request_id),
        };

        // Submit, claim, and process the request
        storage
            .submit(pending_request.clone(), sample_context())
            .await
            .unwrap();
        let claimed_request = pending_request.claim(daemon_id, &storage).await.unwrap();

        // Create a mock HTTP client with a trigger
        let mock_client = crate::http::MockHttpClient::new();
        let trigger = mock_client.add_response_with_trigger(
            "POST /v1/test",
            Ok(crate::http::HttpResponse {
                status: 200,
                body: r#"{"result": "success"}"#.to_string(),
            }),
        );

        let processing_request = claimed_request
            .process(mock_client, sample_context(), &storage)
            .await
            .unwrap();

        let claimed_at = processing_request.state.claimed_at;
        let started_at = processing_request.state.started_at;

        // Trigger the HTTP response to complete
        trigger.send(()).unwrap();

        // Act: Complete the request
        let outcome = processing_request.complete(&storage).await.unwrap();

        // Assert: Verify it completed successfully
        match outcome {
            Ok(completed_req) => {
                assert_eq!(completed_req.data.id, request_id);
                assert_eq!(completed_req.state.response_status, 200);
                assert_eq!(
                    completed_req.state.response_body,
                    r#"{"result": "success"}"#
                );
                assert_eq!(completed_req.state.claimed_at, claimed_at);
                assert_eq!(completed_req.state.started_at, started_at);
                assert!(completed_req.state.completed_at >= started_at);
                assert!(completed_req.state.completed_at <= chrono::Utc::now());
            }
            Err(_failed_req) => {
                panic!("Expected completion to succeed");
            }
        }

        // Assert: Verify storage was updated to Completed state
        let stored_requests = storage.get_requests(vec![request_id]).await.unwrap();
        assert_eq!(stored_requests.len(), 1);
        match &stored_requests[0] {
            Ok(crate::request::types::AnyRequest::Completed(req)) => {
                assert_eq!(req.state.response_status, 200);
                assert_eq!(req.data.id, request_id);
            }
            _ => panic!("Expected request to be in Completed state in storage"),
        }
    }

    #[tokio::test]
    async fn test_processing_to_failed_transition() {
        // Setup
        let storage = InMemoryStorage::new();
        let request_id = RequestId::from(Uuid::new_v4());
        let daemon_id = DaemonId::from(Uuid::new_v4());

        let pending_request = Request {
            state: Pending {},
            data: sample_request_data(request_id),
        };

        // Submit, claim, and process the request
        storage
            .submit(pending_request.clone(), sample_context())
            .await
            .unwrap();
        let claimed_request = pending_request.claim(daemon_id, &storage).await.unwrap();

        // Create a mock HTTP client with a trigger that returns an error
        let mock_client = crate::http::MockHttpClient::new();
        let trigger = mock_client.add_response_with_trigger(
            "POST /v1/test",
            Err(crate::error::BatcherError::Other(anyhow::anyhow!(
                "Network timeout"
            ))),
        );

        let processing_request = claimed_request
            .process(mock_client, sample_context(), &storage)
            .await
            .unwrap();

        let started_at = processing_request.state.started_at;

        // Trigger the HTTP error response
        trigger.send(()).unwrap();

        // Act: Complete the request (expecting failure)
        let outcome = processing_request.complete(&storage).await.unwrap();

        // Assert: Verify it failed
        match outcome {
            Ok(_completed_req) => {
                panic!("Expected completion to fail");
            }
            Err(failed_req) => {
                assert_eq!(failed_req.data.id, request_id);
                assert!(failed_req.state.error.contains("Network timeout"));
                assert!(failed_req.state.failed_at >= started_at);
                assert!(failed_req.state.failed_at <= chrono::Utc::now());
            }
        }

        // Assert: Verify storage was updated to Failed state
        let stored_requests = storage.get_requests(vec![request_id]).await.unwrap();
        assert_eq!(stored_requests.len(), 1);
        match &stored_requests[0] {
            Ok(crate::request::types::AnyRequest::Failed(req)) => {
                assert_eq!(req.data.id, request_id);
                assert!(req.state.error.contains("Network timeout"));
            }
            _ => panic!("Expected request to be in Failed state in storage"),
        }
    }

    #[tokio::test]
    async fn test_processing_to_canceled_transition() {
        // Setup
        let storage = InMemoryStorage::new();
        let request_id = RequestId::from(Uuid::new_v4());
        let daemon_id = DaemonId::from(Uuid::new_v4());

        let pending_request = Request {
            state: Pending {},
            data: sample_request_data(request_id),
        };

        // Submit, claim, and process the request
        storage
            .submit(pending_request.clone(), sample_context())
            .await
            .unwrap();
        let claimed_request = pending_request.claim(daemon_id, &storage).await.unwrap();

        // Create a mock HTTP client with a trigger that we'll never trigger
        // This keeps the request in-flight so we can cancel it
        let mock_client = crate::http::MockHttpClient::new();
        let _trigger = mock_client.add_response_with_trigger(
            "POST /v1/test",
            Ok(crate::http::HttpResponse {
                status: 200,
                body: r#"{"result": "success"}"#.to_string(),
            }),
        );

        let processing_request = claimed_request
            .process(mock_client.clone(), sample_context(), &storage)
            .await
            .unwrap();

        // Give the spawned task a moment to start executing
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Verify it's in processing state
        let stored_requests = storage.get_requests(vec![request_id]).await.unwrap();
        match &stored_requests[0] {
            Ok(crate::request::types::AnyRequest::Processing(_)) => {
                // Good - it's processing
            }
            _ => panic!("Expected request to be in Processing state before cancel"),
        }

        // Verify there's 1 in-flight request
        assert_eq!(mock_client.in_flight_count(), 1);

        // Act: Cancel the processing request
        let canceled_request = processing_request.cancel(&storage).await.unwrap();

        // Give the abort a moment to take effect
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Assert: Verify the abort handle worked - in-flight count should be 0
        assert_eq!(
            mock_client.in_flight_count(),
            0,
            "In-flight count should be 0 after abort"
        );

        // Assert: Verify the state transition
        assert_eq!(canceled_request.data.id, request_id);
        assert!(canceled_request.state.canceled_at <= chrono::Utc::now());

        // Assert: Verify storage was updated to Canceled state
        let stored_requests = storage.get_requests(vec![request_id]).await.unwrap();
        assert_eq!(stored_requests.len(), 1);
        match &stored_requests[0] {
            Ok(crate::request::types::AnyRequest::Canceled(req)) => {
                assert_eq!(req.data.id, request_id);
            }
            _ => panic!("Expected request to be in Canceled state in storage"),
        }
    }

    #[tokio::test]
    async fn test_pending_to_canceled_transition() {
        // Setup
        let storage = InMemoryStorage::new();
        let request_id = RequestId::from(Uuid::new_v4());

        let pending_request = Request {
            state: Pending {},
            data: sample_request_data(request_id),
        };

        // Submit the pending request
        storage
            .submit(pending_request.clone(), sample_context())
            .await
            .unwrap();

        // Verify it's in pending state
        let pending = storage.view_pending_requests(10, None).await.unwrap();
        assert_eq!(pending.len(), 1);

        // Act: Cancel the pending request
        let canceled_request = pending_request.cancel(&storage).await.unwrap();

        // Assert: Verify the state transition
        assert_eq!(canceled_request.data.id, request_id);
        assert!(canceled_request.state.canceled_at <= chrono::Utc::now());

        // Assert: Verify storage was updated to Canceled state
        let stored_requests = storage.get_requests(vec![request_id]).await.unwrap();
        assert_eq!(stored_requests.len(), 1);
        match &stored_requests[0] {
            Ok(crate::request::types::AnyRequest::Canceled(req)) => {
                assert_eq!(req.data.id, request_id);
            }
            _ => panic!("Expected request to be in Canceled state in storage"),
        }

        // Assert: Verify it's no longer in pending queue
        let pending = storage.view_pending_requests(10, None).await.unwrap();
        assert_eq!(pending.len(), 0);
    }

    #[tokio::test]
    async fn test_claimed_to_canceled_transition() {
        // Setup
        let storage = InMemoryStorage::new();
        let request_id = RequestId::from(Uuid::new_v4());
        let daemon_id = DaemonId::from(Uuid::new_v4());

        let pending_request = Request {
            state: Pending {},
            data: sample_request_data(request_id),
        };

        // Submit and claim the request
        storage
            .submit(pending_request.clone(), sample_context())
            .await
            .unwrap();
        let claimed_request = pending_request.claim(daemon_id, &storage).await.unwrap();

        // Verify it's in claimed state
        let stored_requests = storage.get_requests(vec![request_id]).await.unwrap();
        match &stored_requests[0] {
            Ok(crate::request::types::AnyRequest::Claimed(req)) => {
                assert_eq!(req.state.daemon_id, daemon_id);
            }
            _ => panic!("Expected request to be in Claimed state before cancel"),
        }

        // Act: Cancel the claimed request
        let canceled_request = claimed_request.cancel(&storage).await.unwrap();

        // Assert: Verify the state transition
        assert_eq!(canceled_request.data.id, request_id);
        assert!(canceled_request.state.canceled_at <= chrono::Utc::now());

        // Assert: Verify storage was updated to Canceled state
        let stored_requests = storage.get_requests(vec![request_id]).await.unwrap();
        assert_eq!(stored_requests.len(), 1);
        match &stored_requests[0] {
            Ok(crate::request::types::AnyRequest::Canceled(req)) => {
                assert_eq!(req.data.id, request_id);
            }
            _ => panic!("Expected request to be in Canceled state in storage"),
        }
    }

    #[tokio::test]
    async fn test_claimed_to_unclaimed_transition() {
        // Setup
        let storage = InMemoryStorage::new();
        let request_id = RequestId::from(Uuid::new_v4());
        let daemon_id = DaemonId::from(Uuid::new_v4());

        let pending_request = Request {
            state: Pending {},
            data: sample_request_data(request_id),
        };

        // Submit and claim the request
        storage
            .submit(pending_request.clone(), sample_context())
            .await
            .unwrap();
        let claimed_request = pending_request.claim(daemon_id, &storage).await.unwrap();

        let claimed_at = claimed_request.state.claimed_at;

        // Act: Unclaim the request (returns to Pending state)
        let unclaimed_request = claimed_request.unclaim(&storage).await.unwrap();

        // Assert: Verify the state transition back to Pending
        assert_eq!(unclaimed_request.data.id, request_id);

        // Assert: Verify storage was updated back to Pending state
        let stored_requests = storage.get_requests(vec![request_id]).await.unwrap();
        assert_eq!(stored_requests.len(), 1);
        match &stored_requests[0] {
            Ok(crate::request::types::AnyRequest::Pending(req)) => {
                assert_eq!(req.data.id, request_id);
            }
            _ => panic!("Expected request to be in Pending state in storage after unclaim"),
        }

        // Assert: Verify it's back in the pending queue
        let pending = storage.view_pending_requests(10, None).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].data.id, request_id);

        // Note: The request loses its claimed_at timestamp when unclaimed
        // This is expected behavior - it becomes a fresh pending request again
        let _ = claimed_at; // Acknowledge we're aware of this
    }
}
