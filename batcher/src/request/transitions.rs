use std::sync::Arc;

use tokio::sync::Mutex;

use crate::{error::Result, http::HttpClient, manager::Storage};

use super::types::{Canceled, Claimed, Completed, DaemonId, Failed, Pending, Processing, Request};

impl Request<Pending> {
    pub async fn claim<S: Storage + ?Sized>(
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

    pub async fn cancel<S: Storage + ?Sized>(self, storage: &S) -> Result<Request<Canceled>> {
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
    pub async fn unclaim<S: Storage + ?Sized>(self, storage: &S) -> Result<Request<Pending>> {
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

    pub async fn cancel<S: Storage + ?Sized>(self, storage: &S) -> Result<Request<Canceled>> {
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
    pub async fn retry<S: Storage + ?Sized>(
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
    /// The `should_retry` predicate determines whether a response should be considered
    /// a failure (and thus eligible for retry) or a success.
    ///
    /// Returns:
    /// - `Ok(completed_request)` if the HTTP request succeeded
    /// - `Err(failed_request)` if the HTTP request failed or should be retried
    pub async fn complete<S: Storage + ?Sized, F>(
        self,
        storage: &S,
        should_retry: F,
    ) -> Result<std::result::Result<Request<Completed>, Request<Failed>>>
    where
        F: Fn(&crate::http::HttpResponse) -> bool,
    {
        // Await the result from the channel (lock the mutex to access the receiver)
        let result = {
            let mut rx = self.state.result_rx.lock().await;
            rx.recv().await
        };

        match result {
            Some(Ok(http_response)) => {
                // Check if this response should be retried
                if should_retry(&http_response) {
                    // Treat as failure for retry purposes
                    let failed_state = Failed {
                        error: format!(
                            "HTTP request returned retriable status code: {}",
                            http_response.status
                        ),
                        failed_at: chrono::Utc::now(),
                        retry_attempt: self.state.retry_attempt,
                    };
                    let request = Request {
                        data: self.data,
                        state: failed_state,
                    };
                    storage.persist(&request).await?;
                    Ok(Err(request))
                } else {
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

    pub async fn cancel<S: Storage + ?Sized>(self, storage: &S) -> Result<Request<Canceled>> {
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
