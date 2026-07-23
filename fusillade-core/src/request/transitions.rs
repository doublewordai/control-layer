//! State transitions for batch requests using the typestate pattern.
//!
//! This module implements state transitions for HTTP batch requests using Rust's
//! type system to enforce valid state transitions at compile time. Each request
//! state is represented as a distinct type parameter on `Request<State>`.
//!
//! # Typestate Pattern
//!
//! The typestate pattern leverages Rust's type system to make invalid states
//! unrepresentable. A `Request<Pending>` can only call methods available for
//! pending requests, and transitions return different types:
//!
//! ```text
//! Request<Pending> ──claim()──> Request<Claimed> ──process()──> Request<Processing>
//!       │                             │                               │
//!       │                             │                               └──complete()──> Request<Completed>
//!       │                             │                               └──complete()──> Request<Failed>
//!       └──cancel()──> Request<Canceled>                              └──cancel()────> Request<Canceled>
//!                              │
//!                              └──unclaim()─> Request<Pending>
//!
//! Request<Failed> ──retry()──> Request<Pending>  (if retries remain)
//!                 ──retry()──> None              (if max retries reached)
//! ```
//!
//! # State Lifecycle
//!
//! ## 1. Pending → Claimed
//!
//! A daemon claims a pending request for processing:
//! - Records which daemon claimed it
//! - Sets claimed_at timestamp
//! - Preserves retry attempt count
//!
//! ## 2. Claimed → Processing
//!
//! The daemon starts executing the HTTP request:
//! - Spawns an async task to make the HTTP call
//! - Creates a channel to receive the result
//! - Provides an abort handle for cancellation
//!
//! ## 3. Processing → Completed or Failed
//!
//! The HTTP request completes:
//! - **Success**: Transitions to `Completed` with response body
//! - **Failure**: Transitions to `Failed` with error message
//! - **Retriable**: HTTP succeeded but status code indicates retry (e.g., 429, 500)
//!
//! ## 4. Failed → Pending (Retry)
//!
//! Failed requests can be retried with exponential backoff:
//! - Increments retry_attempt counter
//! - Calculates backoff delay: `backoff_ms * (factor ^ attempt)`
//! - Sets not_before timestamp to delay retry
//! - Returns `None` if max retries exceeded
//!
//! ## 5. Any State → Canceled
//!
//! Requests can be canceled from most states:
//! - `Pending`: Simply marks as canceled
//! - `Claimed`: Releases claim and cancels
//! - `Processing`: Aborts the in-flight HTTP request
//!
//! # Retry Configuration
//!
//! Exponential backoff and retry limits are configured via [`RetryConfig`]:
//!
//! ```rust
//! # use fusillade_core::request::transitions::RetryConfig;
//! let config = RetryConfig {
//!     max_retries: Some(1000),
//!     stop_before_deadline_ms: Some(900_000),
//!     backoff_ms: 1000,         // Start with 1 second
//!     backoff_factor: 2,        // Double each time (1s, 2s, 4s)
//!     max_backoff_ms: 60000,    // Cap at 60 seconds
//! };
//! ```
//!
//! # Example Workflow
//!
//! ```ignore
//! // Daemon claims a pending request
//! let pending: Request<Pending> = storage.next_pending().await?;
//! let claimed = pending.claim(daemon_id, &storage).await?;
//!
//! // Start processing
//! let processing = claimed.process(http_client, &storage).await?;
//!
//! // Wait for completion
//! let result = processing.complete(&storage, |resp| resp.status >= 500).await?;
//!
//! match result {
//!     Ok(completed) => println!("Success: {}", completed.state.response_status),
//!     Err(failed) => {
//!         // Attempt retry with backoff
//!         if let Some(retrying) = failed.retry(retry_attempt, config, &storage).await? {
//!             println!("Retrying request...");
//!         } else {
//!             println!("Max retries exceeded");
//!         }
//!     }
//! }
//! ```

use std::sync::Arc;

use tokio::sync::{Mutex, oneshot};
use tracing::Instrument;

use crate::{FusilladeError, error::Result, manager::RequestTransitionStorage};

use super::types::{
    AttemptId, Canceled, Claimed, Completed, DaemonId, Failed, FailureReason, HttpResponse,
    Pending, Processing, Request, RequestCompletionResult, RequestState,
};

/// Reason for cancelling a request.
#[derive(Debug, Clone, Copy)]
pub enum CancellationReason {
    /// User-initiated cancellation (should persist Canceled state).
    User,
    /// Daemon shutdown (abort HTTP but don't persist state change).
    Shutdown,
}

fn fresh_attempt_id() -> AttemptId {
    AttemptId::from(uuid::Uuid::new_v4())
}

fn validate_attempt_authority(attempt_id: AttemptId) -> Result<()> {
    if attempt_id.is_nil() {
        return Err(FusilladeError::ValidationError(
            "nil attempt ID cannot authorize a state transition".to_string(),
        ));
    }
    Ok(())
}

async fn persist_owned_transition<S, T>(
    storage: &S,
    request: &Request<T>,
    attempt_id: AttemptId,
) -> Result<()>
where
    S: RequestTransitionStorage + ?Sized,
    T: RequestState + Clone,
    super::types::AnyRequest: From<Request<T>>,
{
    validate_attempt_authority(attempt_id)?;
    if !storage.persist_attempt(request, attempt_id).await? {
        return Err(FusilladeError::RequestAttemptLost {
            id: request.data.id,
            attempt_id,
        });
    }
    Ok(())
}

fn prepare_processing_request<Fut>(
    request: Request<Claimed>,
    response_fut: Fut,
) -> (Request<Processing>, oneshot::Sender<()>)
where
    Fut: std::future::Future<Output = Result<HttpResponse>> + Send + 'static,
{
    let (dispatch_tx, dispatch_rx) = oneshot::channel();
    let (tx, rx) = tokio::sync::mpsc::channel(1);

    // Spawn now so Processing can own an abort handle, but keep the upstream
    // future completely unpolled until durable processing admission succeeds.
    let current_span = tracing::Span::current();
    let task_handle = tokio::spawn(
        async move {
            if dispatch_rx.await.is_err() {
                return;
            }
            tokio::select! {
                // Dropping Request<Processing> closes the result receiver.
                // Selecting this branch drops response_fut and propagates
                // cancellation to the underlying HTTP request.
                _ = tx.closed() => {}
                result = response_fut => {
                    let _ = tx.send(result).await;
                }
            }
        }
        .instrument(current_span),
    );

    let processing = Request {
        data: request.data,
        state: Processing {
            daemon_id: request.state.daemon_id,
            attempt_id: request.state.attempt_id,
            claimed_at: request.state.claimed_at,
            started_at: chrono::Utc::now(),
            retry_attempt: request.state.retry_attempt,
            batch_expires_at: request.state.batch_expires_at,
            result_rx: Arc::new(Mutex::new(rx)),
            abort_handle: task_handle.abort_handle(),
        },
    };

    (processing, dispatch_tx)
}

fn finish_processing_admission(
    mut request: Request<Processing>,
    dispatch_tx: oneshot::Sender<()>,
    attempt_id: AttemptId,
    persistence: Result<bool>,
) -> Result<Request<Processing>> {
    if request.state.attempt_id != attempt_id {
        request.state.abort_handle.abort();
        return Err(FusilladeError::ValidationError(
            "processing request attempt does not match persistence fence".to_string(),
        ));
    }

    match persistence {
        Ok(true) => {
            // Storage admission may have waited behind a database limiter.
            // Align in-memory timeout accounting with actual dispatch; the
            // storage implementation owns the durable post-admission stamp.
            request.state.started_at = chrono::Utc::now();
            if dispatch_tx.send(()).is_err() {
                request.state.abort_handle.abort();
                return Err(FusilladeError::Other(anyhow::anyhow!(
                    "HTTP dispatch task terminated before request processing began"
                )));
            }
            Ok(request)
        }
        Ok(false) => {
            request.state.abort_handle.abort();
            Err(FusilladeError::RequestAttemptLost {
                id: request.data.id,
                attempt_id,
            })
        }
        Err(error) => {
            request.state.abort_handle.abort();
            Err(error)
        }
    }
}

fn failure_reason_from_http_error(error: FusilladeError) -> FailureReason {
    match error {
        FusilladeError::HttpRequestBuilder(error) => FailureReason::RequestBuilderError { error },
        FusilladeError::HttpClientTimeout(error)
        | FusilladeError::FirstChunkTimeout(error)
        | FusilladeError::UploadStallTimeout(error)
        | FusilladeError::TokensTimeout(error)
        | FusilladeError::BodyTimeout(error) => FailureReason::Timeout { error },
        error => FailureReason::NetworkError {
            error: crate::error::error_serialization::serialize_error(&error.into()),
        },
    }
}

impl Request<Pending> {
    pub async fn claim<S: RequestTransitionStorage + ?Sized>(
        self,
        daemon_id: DaemonId,
        storage: &S,
    ) -> Result<Request<Claimed>> {
        let attempt_id = fresh_attempt_id();
        let request = Request {
            data: self.data,
            state: Claimed {
                daemon_id,
                attempt_id,
                claimed_at: chrono::Utc::now(),
                retry_attempt: self.state.retry_attempt, // Carry over retry attempt
                batch_expires_at: self.state.batch_expires_at, // Carry over batch deadline
                // This single-row claim path does not run the leaky-bucket gate.
                leak: None,
            },
        };
        persist_owned_transition(storage, &request, attempt_id).await?;
        Ok(request)
    }

    pub async fn cancel<S: RequestTransitionStorage + ?Sized>(
        self,
        storage: &S,
    ) -> Result<Request<Canceled>> {
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
    pub async fn unclaim<S: RequestTransitionStorage + ?Sized>(
        self,
        storage: &S,
    ) -> Result<Request<Pending>> {
        let attempt_id = self.state.attempt_id;
        validate_attempt_authority(attempt_id)?;
        let request = Request {
            data: self.data,
            state: Pending {
                retry_attempt: self.state.retry_attempt, // Preserve retry attempt
                not_before: None,                        // Can be claimed immediately
                batch_expires_at: self.state.batch_expires_at, // Carry over batch deadline
            },
        };
        persist_owned_transition(storage, &request, attempt_id).await?;
        Ok(request)
    }

    pub async fn cancel<S: RequestTransitionStorage + ?Sized>(
        self,
        storage: &S,
    ) -> Result<Request<Canceled>> {
        let attempt_id = self.state.attempt_id;
        validate_attempt_authority(attempt_id)?;
        let request = Request {
            data: self.data,
            state: Canceled {
                canceled_at: chrono::Utc::now(),
            },
        };
        persist_owned_transition(storage, &request, attempt_id).await?;
        Ok(request)
    }

    pub async fn process<S, Fut>(
        self,
        storage: &S,
        response_fut: Fut,
    ) -> Result<Request<Processing>>
    where
        S: RequestTransitionStorage + ?Sized,
        Fut: std::future::Future<Output = Result<HttpResponse>> + Send + 'static,
    {
        let attempt_id = self.state.attempt_id;
        validate_attempt_authority(attempt_id)?;
        let (request, dispatch_tx) = prepare_processing_request(self, response_fut);
        let persistence = storage.persist_attempt(&request, attempt_id).await;
        finish_processing_admission(request, dispatch_tx, attempt_id, persistence)
    }
}

/// Configuration for retry behavior.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_retries: Option<u32>,
    pub stop_before_deadline_ms: Option<i64>,
    pub backoff_ms: u64,
    pub backoff_factor: u64,
    pub max_backoff_ms: u64,
}

impl Request<Failed> {
    /// Attempt to retry this failed request.
    ///
    /// If retries are available, transitions the request back to Pending with:
    /// - Incremented retry_attempt
    /// - Calculated not_before timestamp for exponential backoff
    ///
    /// If no retries remain, returns None and the request stays Failed.
    ///
    /// The retry logic considers:
    /// - max_retries: Hard cap on total retry attempts
    /// - stop_before_deadline_ms: Deadline-aware retry (stops before batch expiration)
    pub fn can_retry(
        self,
        retry_attempt: u32,
        config: RetryConfig,
    ) -> std::result::Result<Request<Pending>, Box<Self>> {
        // Calculate exponential backoff: backoff_ms * (backoff_factor ^ retry_attempt)
        let backoff_duration = {
            let exponential = config
                .backoff_ms
                .saturating_mul(config.backoff_factor.saturating_pow(retry_attempt));
            exponential.min(config.max_backoff_ms)
        };

        let now = chrono::Utc::now();
        let not_before = now + chrono::Duration::milliseconds(backoff_duration as i64);

        if let Some(max_retries) = config.max_retries
            && retry_attempt >= max_retries
        {
            return Err(Box::new(self));
        }

        // Determine the effective deadline (with or without buffer)
        let effective_deadline = if let Some(stop_before_deadline_ms) =
            config.stop_before_deadline_ms
        {
            self.state.batch_expires_at - chrono::Duration::milliseconds(stop_before_deadline_ms)
        } else {
            // No buffer configured - use the actual deadline
            self.state.batch_expires_at
        };

        // Check if the next retry would start before the effective deadline
        if not_before >= effective_deadline {
            return Err(Box::new(self));
        }

        // state_transition span emitted by caller after persist

        let request = Request {
            data: self.data,
            state: Pending {
                retry_attempt: retry_attempt + 1,
                not_before: Some(not_before),
                batch_expires_at: self.state.batch_expires_at,
            },
        };

        Ok(request)
    }
}

impl Request<Processing> {
    /// Wait for the HTTP request to complete.
    ///
    /// This method awaits the result from the spawned HTTP task and transitions
    /// the request to one of three terminal states: `Completed`, `Failed`, or `Canceled`.
    ///
    /// The `should_retry` predicate determines whether a response should be considered
    /// a failure (and thus eligible for retry) or a success.
    ///
    /// The `cancellation` future allows external cancellation of the request. It should
    /// resolve to a `CancellationReason`:
    /// - `CancellationReason::User`: User-initiated cancellation (persists Canceled state)
    /// - `CancellationReason::Shutdown`: Daemon shutdown (aborts HTTP but doesn't persist)
    ///
    /// Returns:
    /// - `RequestCompletionResult::Completed` if the HTTP request succeeded
    /// - `RequestCompletionResult::Failed` if the HTTP request failed or should be retried
    /// - `RequestCompletionResult::Canceled` if the request was canceled by user
    /// - `Err(FusilladeError::Shutdown)` if the daemon is shutting down
    pub async fn complete<S, F, Fut>(
        self,
        storage: &S,
        should_retry: F,
        cancellation: Fut,
    ) -> Result<RequestCompletionResult>
    where
        S: RequestTransitionStorage + ?Sized,
        F: Fn(&HttpResponse) -> bool,
        Fut: std::future::Future<Output = CancellationReason>,
    {
        let attempt_id = self.state.attempt_id;
        validate_attempt_authority(attempt_id)?;

        // Await the result from the channel (lock the mutex to access the receiver)
        // We use an enum to track whether we got a result or cancellation so we can
        // drop the mutex guard before calling self.cancel()
        enum Outcome {
            Result(Option<std::result::Result<HttpResponse, FusilladeError>>),
            Canceled(CancellationReason),
        }

        let outcome = {
            let mut rx = self.state.result_rx.lock().await;

            tokio::select! {
                // Wait for the HTTP request to finish processing
                result = rx.recv() => Outcome::Result(result),
                // Handle cancellation
                reason = cancellation => Outcome::Canceled(reason),
            }
        };

        // Handle cancellation outside the mutex guard
        let result = match outcome {
            Outcome::Canceled(CancellationReason::User) => {
                // User cancellation: abort HTTP task but don't persist state change.
                // The batch's cancelling_at flag causes these requests to be counted
                // as canceled in queries, so no individual UPDATE is needed.
                self.state.abort_handle.abort();
                let canceled = Request {
                    data: self.data,
                    state: Canceled {
                        canceled_at: chrono::Utc::now(),
                    },
                };
                return Ok(RequestCompletionResult::Canceled(canceled));
            }
            Outcome::Canceled(CancellationReason::Shutdown) => {
                // Shutdown: abort HTTP task but don't persist state change
                // Request stays in Processing state and will be reclaimed later
                self.state.abort_handle.abort();
                return Err(FusilladeError::Shutdown);
            }
            Outcome::Result(result) => result,
        };

        match result {
            Some(Ok(http_response)) => {
                // Check if this is an error response (4xx or 5xx)
                let is_error = http_response.status >= 400;

                // Check if this response should be retried
                if should_retry(&http_response) {
                    // Treat as failure for retry purposes
                    let failed_state = Failed {
                        reason: FailureReason::RetriableHttpStatus {
                            status: http_response.status,
                            body: http_response.body.clone(),
                        },
                        failed_at: chrono::Utc::now(),
                        retry_attempt: self.state.retry_attempt,
                        batch_expires_at: self.state.batch_expires_at,
                        routed_model: self.data.model.clone(),
                    };
                    let request = Request {
                        data: self.data,
                        state: failed_state,
                    };
                    Ok(RequestCompletionResult::Failed(request))
                } else if is_error {
                    // Non-retriable error (e.g., 4xx client errors)
                    // Mark as failed but don't retry
                    let failed_state = Failed {
                        reason: FailureReason::NonRetriableHttpStatus {
                            status: http_response.status,
                            body: http_response.body.clone(),
                        },
                        failed_at: chrono::Utc::now(),
                        retry_attempt: self.state.retry_attempt,
                        batch_expires_at: self.state.batch_expires_at,
                        routed_model: self.data.model.clone(),
                    };
                    let request = Request {
                        data: self.data,
                        state: failed_state,
                    };
                    persist_owned_transition(storage, &request, attempt_id).await?;
                    Ok(RequestCompletionResult::Failed(request))
                } else {
                    // HTTP request completed successfully
                    let completed_state = Completed {
                        response_status: http_response.status,
                        response_body: http_response.body,
                        claimed_at: self.state.claimed_at,
                        started_at: self.state.started_at,
                        completed_at: chrono::Utc::now(),
                        routed_model: self.data.model.clone(),
                    };
                    let request = Request {
                        data: self.data,
                        state: completed_state,
                    };
                    persist_owned_transition(storage, &request, attempt_id).await?;
                    Ok(RequestCompletionResult::Completed(request))
                }
            }
            Some(Err(e)) => {
                let reason = failure_reason_from_http_error(e);

                let failed_state = Failed {
                    reason,
                    failed_at: chrono::Utc::now(),
                    retry_attempt: self.state.retry_attempt,
                    batch_expires_at: self.state.batch_expires_at,
                    routed_model: self.data.model.clone(),
                };
                let request = Request {
                    data: self.data,
                    state: failed_state,
                };
                if !request.state.reason.is_retriable() {
                    persist_owned_transition(storage, &request, attempt_id).await?;
                }
                Ok(RequestCompletionResult::Failed(request))
            }
            None => {
                // Channel closed - task died without sending a result
                let failed_state = Failed {
                    reason: FailureReason::TaskTerminated,
                    failed_at: chrono::Utc::now(),
                    retry_attempt: self.state.retry_attempt,
                    batch_expires_at: self.state.batch_expires_at,
                    routed_model: self.data.model.clone(),
                };
                let request = Request {
                    data: self.data,
                    state: failed_state,
                };
                persist_owned_transition(storage, &request, attempt_id).await?;
                Ok(RequestCompletionResult::Failed(request))
            }
        }
    }

    pub async fn cancel<S: RequestTransitionStorage + ?Sized>(
        self,
        storage: &S,
    ) -> Result<Request<Canceled>> {
        // Abort the in-flight HTTP request
        self.state.abort_handle.abort();
        let attempt_id = self.state.attempt_id;
        validate_attempt_authority(attempt_id)?;

        let request = Request {
            data: self.data,
            state: Canceled {
                canceled_at: chrono::Utc::now(),
            },
        };
        persist_owned_transition(storage, &request, attempt_id).await?;
        Ok(request)
    }
}

#[cfg(test)]
mod attempt_transition_tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use chrono::Duration;
    use tokio::time::{sleep, timeout};
    use uuid::Uuid;

    use super::*;
    use crate::batch::TemplateId;
    use crate::manager::RequestTransitionStorage;
    use crate::request::{AnyRequest, AttemptId, RequestData, RequestId};

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct PersistRecord {
        target: &'static str,
        argument: AttemptId,
        embedded: Option<AttemptId>,
    }

    struct RecordingTransitionStorage {
        records: StdMutex<Vec<PersistRecord>>,
        applied: bool,
        fail: bool,
    }

    impl RecordingTransitionStorage {
        fn applied() -> Self {
            Self {
                records: StdMutex::new(Vec::new()),
                applied: true,
                fail: false,
            }
        }

        fn lost() -> Self {
            Self {
                records: StdMutex::new(Vec::new()),
                applied: false,
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                records: StdMutex::new(Vec::new()),
                applied: false,
                fail: true,
            }
        }

        fn records(&self) -> Vec<PersistRecord> {
            self.records.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl RequestTransitionStorage for RecordingTransitionStorage {
        async fn persist<T: RequestState + Clone>(
            &self,
            _request: &Request<T>,
        ) -> Result<Option<RequestId>>
        where
            AnyRequest: From<Request<T>>,
        {
            Ok(None)
        }

        async fn persist_attempt<T: RequestState + Clone>(
            &self,
            request: &Request<T>,
            attempt_id: AttemptId,
        ) -> Result<bool>
        where
            AnyRequest: From<Request<T>>,
        {
            if self.fail {
                return Err(FusilladeError::Other(anyhow::anyhow!(
                    "recording storage failure"
                )));
            }
            let request = AnyRequest::from(request.clone());
            let (target, embedded) = match request {
                AnyRequest::Pending(_) => ("pending", None),
                AnyRequest::Claimed(req) => ("claimed", Some(req.state.attempt_id)),
                AnyRequest::Processing(req) => ("processing", Some(req.state.attempt_id)),
                AnyRequest::Completed(_) => ("completed", None),
                AnyRequest::Failed(_) => ("failed", None),
                AnyRequest::Canceled(_) => ("canceled", None),
            };
            self.records.lock().unwrap().push(PersistRecord {
                target,
                argument: attempt_id,
                embedded,
            });
            Ok(self.applied)
        }
    }

    fn claimed_request(attempt_id: AttemptId) -> Request<Claimed> {
        Request {
            data: RequestData {
                id: RequestId(Uuid::new_v4()),
                batch_id: None,
                template_id: TemplateId(Uuid::new_v4()),
                custom_id: None,
                endpoint: "http://example.test".to_string(),
                method: "POST".to_string(),
                path: "/v1/chat/completions".to_string(),
                body: "{}".to_string(),
                model: "test-model".to_string(),
                api_key: "test-key".to_string(),
                created_by: "test-owner".to_string(),
                batch_metadata: HashMap::new(),
            },
            state: Claimed {
                daemon_id: DaemonId(Uuid::new_v4()),
                attempt_id,
                claimed_at: chrono::Utc::now(),
                retry_attempt: 0,
                batch_expires_at: chrono::Utc::now() + Duration::hours(1),
                leak: None,
            },
        }
    }

    fn pending_request() -> Request<Pending> {
        let claimed = claimed_request(AttemptId(Uuid::new_v4()));
        Request {
            data: claimed.data,
            state: Pending {
                retry_attempt: 0,
                not_before: None,
                batch_expires_at: chrono::Utc::now() + Duration::hours(1),
            },
        }
    }

    #[tokio::test]
    async fn public_claim_passes_one_exact_generated_attempt_to_storage() {
        let storage = RecordingTransitionStorage::applied();

        let claimed = pending_request()
            .claim(DaemonId(Uuid::new_v4()), &storage)
            .await
            .unwrap();

        assert!(!claimed.state.attempt_id.is_nil());
        assert_eq!(
            storage.records(),
            vec![PersistRecord {
                target: "claimed",
                argument: claimed.state.attempt_id,
                embedded: Some(claimed.state.attempt_id),
            }]
        );
    }

    #[tokio::test]
    async fn public_claimed_transitions_forward_the_embedded_attempt() {
        let storage = RecordingTransitionStorage::applied();
        let attempt_id = AttemptId(Uuid::new_v4());

        claimed_request(attempt_id).unclaim(&storage).await.unwrap();
        claimed_request(attempt_id).cancel(&storage).await.unwrap();
        let processing = claimed_request(attempt_id)
            .process(&storage, std::future::pending::<Result<HttpResponse>>())
            .await
            .unwrap();
        drop(processing);

        assert_eq!(
            storage.records(),
            vec![
                PersistRecord {
                    target: "pending",
                    argument: attempt_id,
                    embedded: None,
                },
                PersistRecord {
                    target: "canceled",
                    argument: attempt_id,
                    embedded: None,
                },
                PersistRecord {
                    target: "processing",
                    argument: attempt_id,
                    embedded: Some(attempt_id),
                },
            ]
        );
    }

    #[tokio::test]
    async fn public_processing_terminal_paths_forward_the_processing_attempt() {
        let storage = RecordingTransitionStorage::applied();
        let attempt_id = AttemptId(Uuid::new_v4());

        let completed = claimed_request(attempt_id)
            .process(&storage, async {
                Ok(HttpResponse {
                    status: 200,
                    body: "ok".to_string(),
                })
            })
            .await
            .unwrap()
            .complete(
                &storage,
                |_| false,
                std::future::pending::<CancellationReason>(),
            )
            .await
            .unwrap();
        assert!(matches!(completed, RequestCompletionResult::Completed(_)));

        let failed = claimed_request(attempt_id)
            .process(&storage, async {
                Ok(HttpResponse {
                    status: 400,
                    body: "bad".to_string(),
                })
            })
            .await
            .unwrap()
            .complete(
                &storage,
                |_| false,
                std::future::pending::<CancellationReason>(),
            )
            .await
            .unwrap();
        assert!(matches!(failed, RequestCompletionResult::Failed(_)));

        claimed_request(attempt_id)
            .process(&storage, std::future::pending::<Result<HttpResponse>>())
            .await
            .unwrap()
            .cancel(&storage)
            .await
            .unwrap();

        let records = storage.records();
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.target, "completed" | "failed" | "canceled"))
                .cloned()
                .collect::<Vec<_>>(),
            vec![
                PersistRecord {
                    target: "completed",
                    argument: attempt_id,
                    embedded: None,
                },
                PersistRecord {
                    target: "failed",
                    argument: attempt_id,
                    embedded: None,
                },
                PersistRecord {
                    target: "canceled",
                    argument: attempt_id,
                    embedded: None,
                },
            ]
        );
    }

    #[tokio::test]
    async fn every_public_attempt_boundary_maps_storage_loss_to_attempt_lost() {
        let lost = RecordingTransitionStorage::lost();
        let attempt_id = AttemptId(Uuid::new_v4());

        let claim_error = pending_request()
            .claim(DaemonId(Uuid::new_v4()), &lost)
            .await
            .unwrap_err();
        assert!(matches!(
            claim_error,
            FusilladeError::RequestAttemptLost { .. }
        ));
        for error in [
            claimed_request(attempt_id)
                .unclaim(&lost)
                .await
                .unwrap_err(),
            claimed_request(attempt_id).cancel(&lost).await.unwrap_err(),
            claimed_request(attempt_id)
                .process(&lost, std::future::pending::<Result<HttpResponse>>())
                .await
                .unwrap_err(),
        ] {
            assert!(matches!(
                error,
                FusilladeError::RequestAttemptLost {
                    attempt_id: lost_attempt,
                    ..
                } if lost_attempt == attempt_id
            ));
        }

        let admitted = RecordingTransitionStorage::applied();
        let completion_error = claimed_request(attempt_id)
            .process(&admitted, async {
                Ok(HttpResponse {
                    status: 200,
                    body: "ok".to_string(),
                })
            })
            .await
            .unwrap()
            .complete(
                &lost,
                |_| false,
                std::future::pending::<CancellationReason>(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            completion_error,
            FusilladeError::RequestAttemptLost {
                attempt_id: lost_attempt,
                ..
            } if lost_attempt == attempt_id
        ));

        let failure_error = claimed_request(attempt_id)
            .process(&admitted, async {
                Ok(HttpResponse {
                    status: 400,
                    body: "bad".to_string(),
                })
            })
            .await
            .unwrap()
            .complete(
                &lost,
                |_| false,
                std::future::pending::<CancellationReason>(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            failure_error,
            FusilladeError::RequestAttemptLost {
                attempt_id: lost_attempt,
                ..
            } if lost_attempt == attempt_id
        ));

        let cancel_error = claimed_request(attempt_id)
            .process(&admitted, std::future::pending::<Result<HttpResponse>>())
            .await
            .unwrap()
            .cancel(&lost)
            .await
            .unwrap_err();
        assert!(matches!(
            cancel_error,
            FusilladeError::RequestAttemptLost {
                attempt_id: lost_attempt,
                ..
            } if lost_attempt == attempt_id
        ));
    }

    #[tokio::test]
    async fn public_processing_storage_error_never_polls_upstream_future() {
        let storage = RecordingTransitionStorage::failing();
        let polled = Arc::new(AtomicBool::new(false));
        let polled_by_future = polled.clone();

        let error = claimed_request(AttemptId(Uuid::new_v4()))
            .process(&storage, async move {
                polled_by_future.store(true, Ordering::SeqCst);
                Ok(HttpResponse {
                    status: 200,
                    body: "must not run".to_string(),
                })
            })
            .await
            .unwrap_err();

        assert!(matches!(error, FusilladeError::Other(_)));
        tokio::task::yield_now().await;
        assert!(!polled.load(Ordering::SeqCst));
    }

    #[test]
    fn fresh_attempt_ids_are_non_nil_and_unique() {
        let attempts: HashSet<_> = (0..64).map(|_| fresh_attempt_id()).collect();

        assert_eq!(attempts.len(), 64);
        assert!(attempts.iter().all(|attempt_id| !attempt_id.is_nil()));
    }

    #[test]
    fn nil_attempt_cannot_authorize_a_transition() {
        let error = validate_attempt_authority(AttemptId(Uuid::nil())).unwrap_err();

        assert!(matches!(error, FusilladeError::ValidationError(_)));
        assert!(error.to_string().contains("nil"));
    }

    #[tokio::test]
    async fn lost_processing_admission_never_polls_the_response_future() {
        let attempt_id = fresh_attempt_id();
        let polled = Arc::new(AtomicBool::new(false));
        let response_fut = {
            let polled = polled.clone();
            std::future::poll_fn(move |_| {
                polled.store(true, Ordering::SeqCst);
                std::task::Poll::<Result<HttpResponse>>::Pending
            })
        };
        let (request, dispatch_tx) =
            prepare_processing_request(claimed_request(attempt_id), response_fut);
        let request_id = request.data.id;

        let error =
            finish_processing_admission(request, dispatch_tx, attempt_id, Ok(false)).unwrap_err();

        assert!(matches!(
            error,
            FusilladeError::RequestAttemptLost {
                id,
                attempt_id: lost_attempt,
            } if id == request_id && lost_attempt == attempt_id
        ));
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            !polled.load(Ordering::SeqCst),
            "ownership loss must not open the dispatch gate"
        );
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn dropping_processing_request_drops_the_response_future() {
        let attempt_id = fresh_attempt_id();
        let started = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let response_fut = {
            let started = started.clone();
            let dropped = dropped.clone();
            async move {
                let _drop_flag = DropFlag(dropped);
                started.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
                unreachable!()
            }
        };
        let (request, dispatch_tx) =
            prepare_processing_request(claimed_request(attempt_id), response_fut);
        let abort_handle = request.state.abort_handle.clone();
        let processing =
            finish_processing_admission(request, dispatch_tx, attempt_id, Ok(true)).unwrap();

        timeout(std::time::Duration::from_secs(1), async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("response future never started");

        drop(processing);

        let dropped_in_time = timeout(std::time::Duration::from_secs(1), async {
            while !dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await;
        if dropped_in_time.is_err() {
            abort_handle.abort();
        }
        dropped_in_time.expect("dropping Request<Processing> left the response future running");
    }

    #[tokio::test]
    async fn processing_start_is_refreshed_after_durable_admission() {
        let attempt_id = fresh_attempt_id();
        let (request, dispatch_tx) = prepare_processing_request(
            claimed_request(attempt_id),
            std::future::pending::<Result<HttpResponse>>(),
        );
        let before_admission = request.state.started_at;
        let abort_handle = request.state.abort_handle.clone();

        sleep(std::time::Duration::from_millis(2)).await;
        let processing =
            finish_processing_admission(request, dispatch_tx, attempt_id, Ok(true)).unwrap();

        assert!(processing.state.started_at > before_admission);
        abort_handle.abort();
    }

    #[test]
    fn upload_stall_remains_a_retriable_timeout() {
        let reason = failure_reason_from_http_error(FusilladeError::UploadStallTimeout(
            "upload stalled".to_string(),
        ));

        assert_eq!(
            reason,
            FailureReason::Timeout {
                error: "upload stalled".to_string(),
            }
        );
        assert!(reason.is_retriable());
    }
}
