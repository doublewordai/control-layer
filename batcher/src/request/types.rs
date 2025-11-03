//! Core types for the batching system.
//!
//! This module defines the type-safe request lifecycle using the typestate pattern.
//! Each request progresses through distinct states, enforced at compile time.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, Mutex};
use tokio::task::AbortHandle;
use uuid::Uuid;

use crate::error::Result;
use crate::http::HttpResponse;

/// Marker trait for valid request states.
///
/// This trait enables the typestate pattern, ensuring that operations
/// are only performed on requests in valid states.
pub trait RequestState: Send + Sync {}

/// A request to be processed by the batcher system.
///
/// Uses the typestate pattern to ensure type-safe state transitions.
/// The generic parameter `T` represents the current state of the request.
///
/// # Example
/// ```ignore
/// let pending_request = Request {
///     state: Pending {},
///     data: request_data,
/// };
/// // Can only call operations valid for Pending state
/// ```
#[derive(Debug, Clone)]
pub struct Request<T: RequestState> {
    /// The current state of the request.
    pub state: T,
    /// The user-supplied request data.
    pub data: RequestData,
}

/// User-supplied data for a request to be processed by the batcher system.
///
/// This contains all the information needed to make an HTTP request
/// to a target API endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestData {
    /// The ID with which the request was submitted.
    pub id: RequestId,

    /// The base URL of the target endpoint (e.g., "https://api.openai.com")
    pub endpoint: String,

    /// HTTP method (e.g., "POST", "GET")
    pub method: String,

    /// The path portion of the URL (e.g., "/v1/chat/completions")
    pub path: String,

    /// The request body as a JSON string
    pub body: String,

    /// Model identifier - used as a demux key for routing and concurrency control.
    ///
    /// This is somewhat duplicative (it's also in the body), but materializing
    /// it here provides more flexibility for routing and resource management.
    pub model: String,
}

/// Configuration and system-supplied context for how a request should be processed.
///
/// These parameters control retry behavior, timeouts, and authentication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestContext {
    /// Maximum number of retry attempts before giving up
    pub max_retries: u32,

    /// Base backoff duration in milliseconds (will be exponentially increased)
    pub backoff_ms: u64,

    /// Factor by which the backoff_ms is increased with each retry
    pub backoff_factor: u64,

    /// Maximum backoff time in milliseconds
    pub max_backoff_ms: u64,

    /// Timeout for each individual request attempt in milliseconds
    pub timeout_ms: u64,

    /// API key, sent in Authorization: Bearer header. Separate from the request.
    pub api_key: String,
}

// ============================================================================
// Request States
// ============================================================================

/// Request is waiting to be processed.
///
/// This is the initial state for all newly submitted requests.
#[derive(Debug, Clone)]
pub struct Pending {}

impl RequestState for Pending {}

/// Request has been claimed by a daemon but not yet actively executing.
///
/// This intermediate state helps track which daemon is responsible for
/// processing the request.
#[derive(Debug, Clone)]
pub struct Claimed {
    pub daemon_id: DaemonId,
    pub claimed_at: DateTime<Utc>,
}

impl RequestState for Claimed {}

/// Request is currently being processed by a daemon (i.e., HTTP request in flight).
#[derive(Debug, Clone)]
pub struct Processing {
    pub daemon_id: DaemonId,
    pub claimed_at: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
    /// Channel receiver for the HTTP request result (wrapped in Arc<Mutex<>> for Sync)
    pub result_rx: Arc<Mutex<mpsc::Receiver<Result<HttpResponse>>>>,
    /// Handle to abort the in-flight HTTP request
    pub abort_handle: AbortHandle,
}

impl RequestState for Processing {}

/// Request completed successfully.
#[derive(Debug, Clone)]
pub struct Completed {
    pub response_status: u16,
    pub response_body: String,
    pub claimed_at: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
}

impl RequestState for Completed {}

/// Request failed after exhausting retries.
#[derive(Debug, Clone)]
pub struct Failed {
    pub error: String,
    pub failed_at: DateTime<Utc>,
}

impl RequestState for Failed {}

/// Request was canceled by the caller.
#[derive(Debug, Clone)]
pub struct Canceled {
    pub canceled_at: DateTime<Utc>,
}

impl RequestState for Canceled {}

/// Unique identifier for a request in the system.
pub type RequestId = Uuid;

pub type DaemonId = Uuid;

// ============================================================================
// Unified Request Representation
// ============================================================================

/// Enum that can hold a request in any state.
///
/// This is used for storage and API responses where we need to handle
/// requests uniformly regardless of their current state.
#[derive(Debug, Clone)]
pub enum AnyRequest {
    Pending(Request<Pending>),
    Claimed(Request<Claimed>),
    Processing(Request<Processing>),
    Completed(Request<Completed>),
    Failed(Request<Failed>),
    Canceled(Request<Canceled>),
}

impl AnyRequest {
    /// Get the request ID regardless of state.
    pub fn id(&self) -> RequestId {
        match self {
            AnyRequest::Pending(r) => r.data.id,
            AnyRequest::Claimed(r) => r.data.id,
            AnyRequest::Processing(r) => r.data.id,
            AnyRequest::Completed(r) => r.data.id,
            AnyRequest::Failed(r) => r.data.id,
            AnyRequest::Canceled(r) => r.data.id,
        }
    }

    /// Check if this request is in the Pending state.
    pub fn is_pending(&self) -> bool {
        matches!(self, AnyRequest::Pending(_))
    }

    /// Check if this request is in a terminal state (Completed, Failed, or Canceled).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            AnyRequest::Completed(_) | AnyRequest::Failed(_) | AnyRequest::Canceled(_)
        )
    }

    /// Try to extract as a Pending request.
    pub fn as_pending(&self) -> Option<&Request<Pending>> {
        match self {
            AnyRequest::Pending(r) => Some(r),
            _ => None,
        }
    }

    /// Try to take as a Pending request, consuming self.
    pub fn into_pending(self) -> Option<Request<Pending>> {
        match self {
            AnyRequest::Pending(r) => Some(r),
            _ => None,
        }
    }
}

// Conversion traits for going from typed Request to AnyRequest

impl From<Request<Pending>> for AnyRequest {
    fn from(r: Request<Pending>) -> Self {
        AnyRequest::Pending(r)
    }
}

impl From<Request<Claimed>> for AnyRequest {
    fn from(r: Request<Claimed>) -> Self {
        AnyRequest::Claimed(r)
    }
}

impl From<Request<Processing>> for AnyRequest {
    fn from(r: Request<Processing>) -> Self {
        AnyRequest::Processing(r)
    }
}

impl From<Request<Completed>> for AnyRequest {
    fn from(r: Request<Completed>) -> Self {
        AnyRequest::Completed(r)
    }
}

impl From<Request<Failed>> for AnyRequest {
    fn from(r: Request<Failed>) -> Self {
        AnyRequest::Failed(r)
    }
}

impl From<Request<Canceled>> for AnyRequest {
    fn from(r: Request<Canceled>) -> Self {
        AnyRequest::Canceled(r)
    }
}
