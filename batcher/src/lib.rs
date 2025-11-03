pub mod daemon;
pub mod error;
pub mod in_memory;
pub mod types;

pub use daemon::{Daemon, DaemonConfig, DaemonId, DaemonOperations, RequestUpdate, Response};
pub use error::{BatcherError, Result};
pub use in_memory::InMemoryBatcher;
pub use types::{Request, RequestContext, RequestId, RequestStatus};

use std::future::Future;

/// Main trait for the batcher system.
///
/// This trait defines the interface for submitting, canceling, and checking the status
/// of batched requests. Implementations can be backed by different storage mechanisms
/// (e.g., in-memory, PostgreSQL).
pub trait Batcher: Send + Sync {
    /// Submit a batch of requests for processing.
    ///
    /// Each request is paired with a context that defines retry behavior and other
    /// processing parameters.
    ///
    /// Returns a unique ID for each submitted request, in the same order as the input.
    fn submit_requests(
        &self,
        requests: Vec<(Request, RequestContext)>,
    ) -> impl Future<Output = Result<Vec<RequestId>>> + Send;

    /// Cancel one or more pending or processing requests.
    ///
    /// Requests that have already completed or failed cannot be canceled.
    /// This is a best-effort operation - some requests may have already been processed.
    fn cancel_requests(&self, ids: Vec<RequestId>) -> impl Future<Output = Result<()>> + Send;

    /// Get the current status of one or more requests.
    ///
    /// Returns the status for each requested ID. If a request ID doesn't exist,
    /// an error will be returned.
    fn get_status(
        &self,
        ids: Vec<RequestId>,
    ) -> impl Future<Output = Result<Vec<(RequestId, RequestStatus)>>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_serialization() {
        let request = Request {
            endpoint: "https://api.example.com".to_string(),
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            body: r#"{"model": "gpt-4"}"#.to_string(),
            api_key: "sk-test123".to_string(),
            model: "gpt-4".to_string(),
        };

        let json = serde_json::to_string(&request).unwrap();
        let deserialized: Request = serde_json::from_str(&json).unwrap();

        assert_eq!(request, deserialized);
    }

    #[test]
    fn test_request_context_defaults() {
        let context = RequestContext::default();
        assert_eq!(context.max_retries, 3);
        assert_eq!(context.backoff_ms, 1000);
        assert_eq!(context.timeout_ms, 30000);
    }

    #[test]
    fn test_request_id_roundtrip() {
        let id = RequestId::new();
        let uuid_val = id.as_uuid();
        let id2 = RequestId::from(uuid_val);
        assert_eq!(id, id2);
    }

    #[test]
    fn test_request_status_types() {
        use RequestStatus::*;

        // Verify we can construct all status variants
        let _pending = Pending;
        let _pending_processing = PendingProcessing {
            daemon_id: "test".to_string(),
            acquired_at: chrono::Utc::now(),
        };
        let _processing = Processing {
            daemon_id: "test".to_string(),
            acquired_at: chrono::Utc::now(),
        };
        let _completed = Completed {
            response_status: 200,
            response_body: "{}".to_string(),
            completed_at: chrono::Utc::now(),
        };
        let _failed = Failed {
            error: "test error".to_string(),
            retry_count: 3,
            failed_at: chrono::Utc::now(),
        };
        let _canceled = Canceled {
            canceled_at: chrono::Utc::now(),
        };
    }
}
