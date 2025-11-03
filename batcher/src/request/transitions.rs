use std::sync::Arc;

use tokio::sync::Mutex;

use crate::{
    error::Result,
    http::HttpClient,
    storage::Storage,
};

use super::types::{
    Canceled, Claimed, Completed, DaemonId, Failed, Pending,
    Processing, Request, RequestContext,
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
