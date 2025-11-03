use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A unique identifier for a request in the batcher system.
///
/// Uses a short, readable format like "req_abc123xy" instead of full UUIDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestId(Uuid);

impl RequestId {
    /// Create a new random request ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Get the underlying UUID.
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }

    /// Convert to a short, readable string format.
    ///
    /// Takes the first 8 hex characters of the UUID and formats as "req_xxxxxxxx".
    pub fn to_short_string(&self) -> String {
        let hex = format!("{:x}", self.0.as_u128());
        format!("req_{}", &hex[..8])
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

impl From<Uuid> for RequestId {
    fn from(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

impl From<RequestId> for Uuid {
    fn from(id: RequestId) -> Self {
        id.0
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_short_string())
    }
}

/// A request to be processed by the batcher system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// The base URL of the target endpoint (e.g., "https://api.openai.com")
    pub endpoint: String,

    /// HTTP method (e.g., "POST", "GET")
    pub method: String,

    /// The path portion of the URL (e.g., "/v1/chat/completions")
    pub path: String,

    /// The request body as a JSON string
    pub body: String,

    /// API key for authentication with the upstream service
    pub api_key: String,

    /// Model identifier - used as a demux key for routing and concurrency control
    pub model: String,
}

impl Request {
    /// Get the full URL for this request.
    pub fn url(&self) -> String {
        format!("{}{}", self.endpoint, self.path)
    }
}

/// Configuration context for how a request should be processed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestContext {
    /// Maximum number of retry attempts before giving up
    pub max_retries: u32,

    /// Base backoff duration in milliseconds (will be exponentially increased)
    pub backoff_ms: u64,

    /// Timeout for each individual request attempt in milliseconds
    pub timeout_ms: u64,
}

impl Default for RequestContext {
    fn default() -> Self {
        Self {
            max_retries: 3,
            backoff_ms: 1000,    // 1 second base backoff
            timeout_ms: 30000,   // 30 second timeout
        }
    }
}

impl RequestContext {
    /// Create a new context with custom settings.
    pub fn new(max_retries: u32, backoff_ms: u64, timeout_ms: u64) -> Self {
        Self {
            max_retries,
            backoff_ms,
            timeout_ms,
        }
    }

    /// Calculate the backoff duration for a given retry attempt.
    /// Uses exponential backoff: backoff_ms * 2^retry_count
    pub fn calculate_backoff(&self, retry_count: u32) -> std::time::Duration {
        let backoff_ms = self.backoff_ms * 2u64.pow(retry_count);
        std::time::Duration::from_millis(backoff_ms)
    }
}

/// The current status of a request in the system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RequestStatus {
    /// Request is waiting to be processed
    Pending,

    /// Request has been claimed by a daemon but not yet actively executing
    PendingProcessing {
        /// ID of the daemon that claimed this request
        daemon_id: String,
        /// When the request was acquired
        acquired_at: DateTime<Utc>,
    },

    /// Request is currently being processed by a daemon (HTTP request in flight)
    Processing {
        /// ID of the daemon processing this request
        daemon_id: String,
        /// When the request was acquired for processing
        acquired_at: DateTime<Utc>,
    },

    /// Request completed successfully
    Completed {
        /// HTTP response status code
        response_status: u16,
        /// Response body
        response_body: String,
        /// When the request completed
        completed_at: DateTime<Utc>,
    },

    /// Request failed after exhausting retries
    Failed {
        /// Error message describing the failure
        error: String,
        /// Number of retry attempts made
        retry_count: u32,
        /// When the request was marked as failed
        failed_at: DateTime<Utc>,
    },

    /// Request was canceled by the caller
    Canceled {
        /// When the request was canceled
        canceled_at: DateTime<Utc>,
    },
}

impl RequestStatus {
    /// Check if this status represents a terminal state (completed, failed, or canceled).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            RequestStatus::Completed { .. }
                | RequestStatus::Failed { .. }
                | RequestStatus::Canceled { .. }
        )
    }

    /// Check if this status represents an active state (pending, pending_processing, or processing).
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            RequestStatus::Pending
                | RequestStatus::PendingProcessing { .. }
                | RequestStatus::Processing { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_url() {
        let request = Request {
            endpoint: "https://api.example.com".to_string(),
            method: "POST".to_string(),
            path: "/v1/chat".to_string(),
            body: "{}".to_string(),
            api_key: "test".to_string(),
            model: "gpt-4".to_string(),
        };

        assert_eq!(request.url(), "https://api.example.com/v1/chat");
    }

    #[test]
    fn test_backoff_calculation() {
        let context = RequestContext::default();

        // Retry 0: 1000ms * 2^0 = 1000ms
        assert_eq!(context.calculate_backoff(0).as_millis(), 1000);

        // Retry 1: 1000ms * 2^1 = 2000ms
        assert_eq!(context.calculate_backoff(1).as_millis(), 2000);

        // Retry 2: 1000ms * 2^2 = 4000ms
        assert_eq!(context.calculate_backoff(2).as_millis(), 4000);

        // Retry 3: 1000ms * 2^3 = 8000ms
        assert_eq!(context.calculate_backoff(3).as_millis(), 8000);
    }

    #[test]
    fn test_status_terminal() {
        assert!(!RequestStatus::Pending.is_terminal());
        assert!(!RequestStatus::PendingProcessing {
            daemon_id: "test".to_string(),
            acquired_at: Utc::now(),
        }
        .is_terminal());
        assert!(!RequestStatus::Processing {
            daemon_id: "test".to_string(),
            acquired_at: Utc::now(),
        }
        .is_terminal());

        assert!(RequestStatus::Completed {
            response_status: 200,
            response_body: "{}".to_string(),
            completed_at: Utc::now(),
        }
        .is_terminal());

        assert!(RequestStatus::Failed {
            error: "timeout".to_string(),
            retry_count: 3,
            failed_at: Utc::now(),
        }
        .is_terminal());

        assert!(RequestStatus::Canceled {
            canceled_at: Utc::now()
        }
        .is_terminal());
    }

    #[test]
    fn test_status_active() {
        assert!(RequestStatus::Pending.is_active());
        assert!(RequestStatus::PendingProcessing {
            daemon_id: "test".to_string(),
            acquired_at: Utc::now(),
        }
        .is_active());
        assert!(RequestStatus::Processing {
            daemon_id: "test".to_string(),
            acquired_at: Utc::now(),
        }
        .is_active());

        assert!(!RequestStatus::Completed {
            response_status: 200,
            response_body: "{}".to_string(),
            completed_at: Utc::now(),
        }
        .is_active());
    }
}
