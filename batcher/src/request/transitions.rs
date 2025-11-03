use std::sync::Arc;

use tokio::sync::Mutex;

use crate::{error::Result, http::HttpClient, storage::Storage};

use super::types::{Canceled, Claimed, Completed, DaemonId, Failed, Pending, Processing, Request};

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
                retry_attempt: self.state.retry_attempt, // Carry over retry attempt
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
            state: Pending {
                retry_attempt: self.state.retry_attempt, // Preserve retry attempt
                not_before: None,                        // Can be claimed immediately
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

    pub async fn process<H: HttpClient + 'static, S: Storage>(
        self,
        http_client: H,
        timeout_ms: u64,
        storage: &S,
    ) -> Result<Request<Processing>> {
        let request_data = self.data.clone();
        let api_key = request_data.api_key.clone();

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
            retry_attempt: self.state.retry_attempt, // Carry over retry attempt
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

/// Configuration for retry behavior.
#[derive(Debug, Clone, Copy)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub backoff_ms: u64,
    pub backoff_factor: u64,
    pub max_backoff_ms: u64,
}

impl From<&crate::daemon::DaemonConfig> for RetryConfig {
    fn from(config: &crate::daemon::DaemonConfig) -> Self {
        RetryConfig {
            max_retries: config.max_retries,
            backoff_ms: config.backoff_ms,
            backoff_factor: config.backoff_factor,
            max_backoff_ms: config.max_backoff_ms,
        }
    }
}

impl Request<Failed> {
    /// Attempt to retry this failed request.
    ///
    /// If retries are available, transitions the request back to Pending with:
    /// - Incremented retry_attempt
    /// - Calculated not_before timestamp for exponential backoff
    ///
    /// If no retries remain, returns None and the request stays Failed.
    pub async fn retry<S: Storage>(
        self,
        retry_attempt: u32,
        config: RetryConfig,
        storage: &S,
    ) -> Result<Option<Request<Pending>>> {
        // Check if we have retries left
        if retry_attempt >= config.max_retries {
            tracing::debug!(
                request_id = %self.data.id,
                retry_attempt,
                max_retries = config.max_retries,
                "No retries remaining, request remains failed"
            );
            return Ok(None);
        }

        // Calculate exponential backoff: backoff_ms * (backoff_factor ^ retry_attempt)
        let backoff_duration = {
            let exponential = config
                .backoff_ms
                .saturating_mul(config.backoff_factor.saturating_pow(retry_attempt));
            exponential.min(config.max_backoff_ms)
        };

        let not_before =
            chrono::Utc::now() + chrono::Duration::milliseconds(backoff_duration as i64);

        tracing::info!(
            request_id = %self.data.id,
            retry_attempt = retry_attempt + 1,
            backoff_ms = backoff_duration,
            not_before = %not_before,
            "Retrying failed request with exponential backoff"
        );

        let request = Request {
            data: self.data,
            state: Pending {
                retry_attempt: retry_attempt + 1,
                not_before: Some(not_before),
            },
        };

        storage.persist(&request).await?;
        Ok(Some(request))
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
                    retry_attempt: self.state.retry_attempt,
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
                    retry_attempt: self.state.retry_attempt,
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
        request::types::{DaemonId, Pending, Request, RequestData, RequestId},
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
            api_key: "test-key".to_string(),
        }
    }

    const TEST_TIMEOUT_MS: u64 = 30000;

    #[tokio::test]
    async fn test_pending_to_claimed_transition() {
        // Setup
        let storage = InMemoryStorage::new();
        let request_id = RequestId::from(Uuid::new_v4());
        let daemon_id = DaemonId::from(Uuid::new_v4());

        let pending_request = Request {
            state: Pending {
                retry_attempt: 0,
                not_before: None,
            },
            data: sample_request_data(request_id),
        };

        // Submit the pending request first
        storage.submit(pending_request.clone()).await.unwrap();

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
            state: Pending {
                retry_attempt: 0,
                not_before: None,
            },
            data: sample_request_data(request_id),
        };

        // Submit and claim the request
        storage.submit(pending_request.clone()).await.unwrap();
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
            .process(mock_client, TEST_TIMEOUT_MS, &storage)
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
            state: Pending {
                retry_attempt: 0,
                not_before: None,
            },
            data: sample_request_data(request_id),
        };

        // Submit, claim, and process the request
        storage.submit(pending_request.clone()).await.unwrap();
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
            .process(mock_client, TEST_TIMEOUT_MS, &storage)
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
            state: Pending {
                retry_attempt: 0,
                not_before: None,
            },
            data: sample_request_data(request_id),
        };

        // Submit, claim, and process the request
        storage.submit(pending_request.clone()).await.unwrap();
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
            .process(mock_client, TEST_TIMEOUT_MS, &storage)
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
            state: Pending {
                retry_attempt: 0,
                not_before: None,
            },
            data: sample_request_data(request_id),
        };

        // Submit, claim, and process the request
        storage.submit(pending_request.clone()).await.unwrap();
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
            .process(mock_client.clone(), TEST_TIMEOUT_MS, &storage)
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
            state: Pending {
                retry_attempt: 0,
                not_before: None,
            },
            data: sample_request_data(request_id),
        };

        // Submit the pending request
        storage.submit(pending_request.clone()).await.unwrap();

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
            state: Pending {
                retry_attempt: 0,
                not_before: None,
            },
            data: sample_request_data(request_id),
        };

        // Submit and claim the request
        storage.submit(pending_request.clone()).await.unwrap();
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
            state: Pending {
                retry_attempt: 0,
                not_before: None,
            },
            data: sample_request_data(request_id),
        };

        // Submit and claim the request
        storage.submit(pending_request.clone()).await.unwrap();
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

    #[tokio::test]
    async fn test_failed_to_pending_retry_transition() {
        // Setup
        let storage = InMemoryStorage::new();
        let request_id = RequestId::from(Uuid::new_v4());

        let failed_request = Request {
            state: crate::request::types::Failed {
                error: "Network timeout".to_string(),
                failed_at: chrono::Utc::now(),
                retry_attempt: 0,
            },
            data: sample_request_data(request_id),
        };

        // Submit the failed request first (so it exists in storage)
        // We need to submit as pending first, then update to failed
        let pending = Request {
            state: Pending {
                retry_attempt: 0,
                not_before: None,
            },
            data: sample_request_data(request_id),
        };
        storage.submit(pending).await.unwrap();
        storage.persist(&failed_request).await.unwrap();

        let retry_config = super::RetryConfig {
            max_retries: 3,
            backoff_ms: 100,
            backoff_factor: 2,
            max_backoff_ms: 10000,
        };

        // Act: Retry the failed request
        let before_retry = chrono::Utc::now();
        let retried = failed_request
            .retry(0, retry_config, &storage)
            .await
            .unwrap();

        // Assert: Should return Some(pending_request) since we have retries left
        assert!(retried.is_some());
        let pending_request = retried.unwrap();

        // Assert: Verify retry_attempt was incremented
        assert_eq!(pending_request.state.retry_attempt, 1);

        // Assert: Verify not_before was set with backoff (100ms * 2^0 = 100ms)
        assert!(pending_request.state.not_before.is_some());
        let not_before = pending_request.state.not_before.unwrap();
        let expected_backoff = chrono::Duration::milliseconds(100);
        let after_retry = chrono::Utc::now();

        // not_before should be approximately now + 100ms
        assert!(not_before > before_retry);
        assert!(not_before < after_retry + expected_backoff + chrono::Duration::seconds(1));

        // Assert: Verify storage was updated to Pending
        let stored_requests = storage.get_requests(vec![request_id]).await.unwrap();
        match &stored_requests[0] {
            Ok(crate::request::types::AnyRequest::Pending(req)) => {
                assert_eq!(req.state.retry_attempt, 1);
                assert!(req.state.not_before.is_some());
            }
            _ => panic!("Expected request to be in Pending state after retry"),
        }
    }

    #[tokio::test]
    async fn test_failed_retry_exponential_backoff() {
        // Setup
        let storage = InMemoryStorage::new();
        let request_id = RequestId::from(Uuid::new_v4());

        // Submit initial pending request
        let pending = Request {
            state: Pending {
                retry_attempt: 0,
                not_before: None,
            },
            data: sample_request_data(request_id),
        };
        storage.submit(pending).await.unwrap();

        let retry_config = super::RetryConfig {
            max_retries: 3,
            backoff_ms: 100,
            backoff_factor: 2,
            max_backoff_ms: 10000,
        };

        // Test retry attempt 0: backoff should be 100ms * 2^0 = 100ms
        let failed_0 = Request {
            state: crate::request::types::Failed {
                error: "Error".to_string(),
                failed_at: chrono::Utc::now(),
                retry_attempt: 0,
            },
            data: sample_request_data(request_id),
        };
        storage.persist(&failed_0).await.unwrap();

        let now_0 = chrono::Utc::now();
        let retry_0 = failed_0
            .retry(0, retry_config, &storage)
            .await
            .unwrap()
            .unwrap();
        let backoff_0 = retry_0.state.not_before.unwrap() - now_0;

        // Should be approximately 100ms
        assert!(backoff_0.num_milliseconds() >= 100);
        assert!(backoff_0.num_milliseconds() <= 200); // Allow some slack

        // Test retry attempt 1: backoff should be 100ms * 2^1 = 200ms
        let failed_1 = Request {
            state: crate::request::types::Failed {
                error: "Error".to_string(),
                failed_at: chrono::Utc::now(),
                retry_attempt: 1,
            },
            data: sample_request_data(request_id),
        };
        storage.persist(&failed_1).await.unwrap();

        let now_1 = chrono::Utc::now();
        let retry_1 = failed_1
            .retry(1, retry_config, &storage)
            .await
            .unwrap()
            .unwrap();
        let backoff_1 = retry_1.state.not_before.unwrap() - now_1;

        // Should be approximately 200ms
        assert!(backoff_1.num_milliseconds() >= 200);
        assert!(backoff_1.num_milliseconds() <= 300);

        // Test retry attempt 2: backoff should be 100ms * 2^2 = 400ms
        let failed_2 = Request {
            state: crate::request::types::Failed {
                error: "Error".to_string(),
                failed_at: chrono::Utc::now(),
                retry_attempt: 2,
            },
            data: sample_request_data(request_id),
        };
        storage.persist(&failed_2).await.unwrap();

        let now_2 = chrono::Utc::now();
        let retry_2 = failed_2
            .retry(2, retry_config, &storage)
            .await
            .unwrap()
            .unwrap();
        let backoff_2 = retry_2.state.not_before.unwrap() - now_2;

        // Should be approximately 400ms
        assert!(backoff_2.num_milliseconds() >= 400);
        assert!(backoff_2.num_milliseconds() <= 500);
    }

    #[tokio::test]
    async fn test_failed_retry_exhausted() {
        // Setup
        let storage = InMemoryStorage::new();
        let request_id = RequestId::from(Uuid::new_v4());

        // Submit initial pending request
        let pending = Request {
            state: Pending {
                retry_attempt: 0,
                not_before: None,
            },
            data: sample_request_data(request_id),
        };
        storage.submit(pending).await.unwrap();

        let failed_request = Request {
            state: crate::request::types::Failed {
                error: "Network timeout".to_string(),
                failed_at: chrono::Utc::now(),
                retry_attempt: 3,
            },
            data: sample_request_data(request_id),
        };
        storage.persist(&failed_request).await.unwrap();

        let retry_config = super::RetryConfig {
            max_retries: 3,
            backoff_ms: 100,
            backoff_factor: 2,
            max_backoff_ms: 10000,
        };

        // Act: Try to retry when we've already exhausted retries (retry_attempt = 3, max = 3)
        let result = failed_request
            .retry(3, retry_config, &storage)
            .await
            .unwrap();

        // Assert: Should return None since we've exhausted retries
        assert!(result.is_none());

        // Assert: Request should still be in Failed state
        let stored_requests = storage.get_requests(vec![request_id]).await.unwrap();
        match &stored_requests[0] {
            Ok(crate::request::types::AnyRequest::Failed(req)) => {
                assert_eq!(req.state.retry_attempt, 3);
            }
            _ => panic!("Expected request to remain in Failed state"),
        }
    }

    #[tokio::test]
    async fn test_claim_requests_respects_not_before() {
        // Setup
        let storage = InMemoryStorage::new();
        let daemon_id = DaemonId::from(Uuid::new_v4());

        // Create a request with not_before in the future (should NOT be claimable)
        let future_request_id = RequestId::from(Uuid::new_v4());
        let future_request = Request {
            state: Pending {
                retry_attempt: 1,
                not_before: Some(chrono::Utc::now() + chrono::Duration::seconds(10)),
            },
            data: sample_request_data(future_request_id),
        };
        storage.submit(future_request).await.unwrap();

        // Create a request with not_before in the past (should be claimable)
        let past_request_id = RequestId::from(Uuid::new_v4());
        let past_request = Request {
            state: Pending {
                retry_attempt: 1,
                not_before: Some(chrono::Utc::now() - chrono::Duration::seconds(10)),
            },
            data: sample_request_data(past_request_id),
        };
        storage.submit(past_request).await.unwrap();

        // Create a request with no not_before (should be claimable)
        let immediate_request_id = RequestId::from(Uuid::new_v4());
        let immediate_request = Request {
            state: Pending {
                retry_attempt: 0,
                not_before: None,
            },
            data: sample_request_data(immediate_request_id),
        };
        storage.submit(immediate_request).await.unwrap();

        // Act: Try to claim requests
        let claimed = storage.claim_requests(10, daemon_id).await.unwrap();

        // Assert: Should have claimed 2 requests (past and immediate, but not future)
        assert_eq!(claimed.len(), 2);

        let claimed_ids: Vec<RequestId> = claimed.iter().map(|r| r.data.id).collect();
        assert!(claimed_ids.contains(&past_request_id));
        assert!(claimed_ids.contains(&immediate_request_id));
        assert!(!claimed_ids.contains(&future_request_id));

        // Assert: The future request should still be pending
        let pending = storage.view_pending_requests(10, None).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].data.id, future_request_id);
    }

    #[tokio::test]
    async fn test_retry_attempt_carried_through_transitions() {
        // Setup
        let storage = InMemoryStorage::new();
        let request_id = RequestId::from(Uuid::new_v4());
        let daemon_id = DaemonId::from(Uuid::new_v4());

        // Start with a pending request that has retry_attempt = 2
        let pending_request = Request {
            state: Pending {
                retry_attempt: 2,
                not_before: None,
            },
            data: sample_request_data(request_id),
        };
        storage.submit(pending_request.clone()).await.unwrap();

        // Claim it
        let claimed_request = pending_request.claim(daemon_id, &storage).await.unwrap();
        assert_eq!(claimed_request.state.retry_attempt, 2);

        // Process it
        let mock_client = crate::http::MockHttpClient::new();
        let trigger = mock_client.add_response_with_trigger(
            "POST /v1/test",
            Err(crate::error::BatcherError::Other(anyhow::anyhow!("Error"))),
        );

        let processing_request = claimed_request
            .process(mock_client, TEST_TIMEOUT_MS, &storage)
            .await
            .unwrap();
        assert_eq!(processing_request.state.retry_attempt, 2);

        // Complete it (with failure)
        trigger.send(()).unwrap();
        let outcome = processing_request.complete(&storage).await.unwrap();

        match outcome {
            Err(failed_req) => {
                // Verify retry_attempt was preserved in Failed state
                assert_eq!(failed_req.state.retry_attempt, 2);
            }
            Ok(_) => panic!("Expected failure"),
        }
    }
}
