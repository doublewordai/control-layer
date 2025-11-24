//! Core types for the batching system.
//!
//! This module defines the type-safe request lifecycle using the typestate pattern.
//! Each request progresses through distinct states, enforced at compile time.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::{Mutex, mpsc};
use tokio::task::AbortHandle;
use uuid::Uuid;

use crate::batch::{BatchId, TemplateId};
use crate::error::Result;
use crate::http::HttpResponse;

/// Marker trait for valid request states.
///
/// This trait enables the typestate pattern, ensuring that operations
/// are only performed on requests in valid states.
pub trait RequestState: Send + Sync {}

/// A request to be processed by the fusillade system.
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
#[derive(Debug, Clone, Serialize)]
pub struct Request<T: RequestState> {
    /// The current state of the request.
    pub state: T,
    /// The user-supplied request data.
    pub data: RequestData,
}

/// User-supplied data for a request to be processed by the fusillade system.
///
/// This contains all the information needed to make an HTTP request
/// to a target API endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestData {
    /// The ID with which the request was submitted.
    pub id: RequestId,

    /// The batch this execution belongs to
    pub batch_id: BatchId,

    /// The template this execution was created from
    pub template_id: TemplateId,

    /// Optional custom ID provided by the user for tracking
    pub custom_id: Option<String>,

    /// The base URL of the target endpoint (e.g., <https://api.openai.com>)
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

    /// API key for authentication (sent in Authorization: Bearer header)
    pub api_key: String,
}

// ============================================================================
// Request States
// ============================================================================

/// Request is waiting to be processed.
///
/// This is the initial state for all newly submitted requests.
#[derive(Debug, Clone, Serialize)]
pub struct Pending {
    /// Number of times this request has been attempted (0 = first attempt)
    pub retry_attempt: u32,

    /// Earliest time this request can be claimed (for exponential backoff)
    /// None means it can be claimed immediately
    pub not_before: Option<DateTime<Utc>>,
}

impl RequestState for Pending {}

/// Request has been claimed by a daemon but not yet actively executing.
///
/// This intermediate state helps track which daemon is responsible for
/// processing the request.
#[derive(Debug, Clone, Serialize)]
pub struct Claimed {
    pub daemon_id: DaemonId,
    pub claimed_at: DateTime<Utc>,
    /// Number of times this request has been attempted (carried over from Pending)
    pub retry_attempt: u32,
}

impl RequestState for Claimed {}

/// Request is currently being processed by a daemon (i.e., HTTP request in flight).
#[derive(Debug, Clone, Serialize)]
pub struct Processing {
    pub daemon_id: DaemonId,
    pub claimed_at: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
    /// Number of times this request has been attempted (carried over from Claimed)
    pub retry_attempt: u32,
    /// Channel receiver for the HTTP request result (wrapped in Arc<Mutex<>> for Sync)
    #[serde(skip)]
    pub result_rx: Arc<Mutex<mpsc::Receiver<Result<HttpResponse>>>>,
    /// Handle to abort the in-flight HTTP request
    #[serde(skip)]
    pub abort_handle: AbortHandle,
}

impl RequestState for Processing {}

/// Request completed successfully.
#[derive(Debug, Clone, Serialize)]
pub struct Completed {
    pub response_status: u16,
    pub response_body: String,
    pub claimed_at: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
}

impl RequestState for Completed {}

/// Reason why a request failed.
///
/// This enum distinguishes between different types of failures to determine
/// whether a request should be retried.
#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "details")]
pub enum FailureReason {
    /// HTTP request returned a status code that should be retried (e.g., 429, 500, 503).
    /// These are transient errors that may succeed on retry.
    RetriableHttpStatus { status: u16, body: String },

    /// HTTP request returned a client error status code that should not be retried (e.g., 400, 404).
    /// These indicate problems with the request itself that won't be fixed by retrying.
    NonRetriableHttpStatus { status: u16, body: String },

    /// Network error, timeout, or other transport-level failure.
    /// These are transient infrastructure issues that should be retried.
    NetworkError { error: String },

    /// The HTTP task terminated unexpectedly (panic or crash).
    /// This should be retried as it may be a transient issue.
    TaskTerminated,
}

impl FailureReason {
    /// Returns true if this failure reason indicates the request should be retried.
    pub fn is_retriable(&self) -> bool {
        match self {
            FailureReason::RetriableHttpStatus { .. } => true,
            FailureReason::NonRetriableHttpStatus { .. } => false,
            FailureReason::NetworkError { .. } => true,
            FailureReason::TaskTerminated => true,
        }
    }

    /// Returns a human-readable error message for this failure reason.
    pub fn to_error_message(&self) -> String {
        match self {
            FailureReason::RetriableHttpStatus { status, body } => {
                format!(
                    "HTTP request returned retriable status code: {} - {}",
                    status, body
                )
            }
            FailureReason::NonRetriableHttpStatus { status, body } => {
                format!(
                    "HTTP request returned error status code: {} - {}",
                    status, body
                )
            }
            FailureReason::NetworkError { error } => {
                format!("Network error: {}", error)
            }
            FailureReason::TaskTerminated => "HTTP task terminated unexpectedly".to_string(),
        }
    }
}

/// Request failed after exhausting retries.
#[derive(Debug, Clone, Serialize)]
pub struct Failed {
    pub reason: FailureReason,
    pub failed_at: DateTime<Utc>,
    /// Number of times this request has been attempted when it failed
    pub retry_attempt: u32,
}

impl RequestState for Failed {}

/// Request was canceled by the caller.
#[derive(Debug, Clone, Serialize)]
pub struct Canceled {
    pub canceled_at: DateTime<Utc>,
}

impl RequestState for Canceled {}

/// Unique identifier for a request in the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct RequestId(pub Uuid);

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Display only first 8 characters for readability in logs
        write!(f, "{}", &self.0.to_string()[..8])
    }
}

impl From<Uuid> for RequestId {
    fn from(uuid: Uuid) -> Self {
        RequestId(uuid)
    }
}

impl std::ops::Deref for RequestId {
    type Target = Uuid;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Unique identifier for a daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct DaemonId(pub Uuid);

impl std::fmt::Display for DaemonId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Display only first 8 characters for readability in logs
        write!(f, "{}", &self.0.to_string()[..8])
    }
}

impl From<Uuid> for DaemonId {
    fn from(uuid: Uuid) -> Self {
        DaemonId(uuid)
    }
}

impl std::ops::Deref for DaemonId {
    type Target = Uuid;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

// ============================================================================
// Unified Request Representation
// ============================================================================

/// Result of completing a processing request.
///
/// After a request finishes processing, it transitions to one of three
/// terminal states: Completed (success), Failed (error/retry), or Canceled.
#[derive(Debug)]
pub enum RequestCompletionResult {
    /// The HTTP request completed successfully.
    Completed(Request<Completed>),
    /// The HTTP request failed and may be retried.
    Failed(Request<Failed>),
    /// The request was canceled before completion.
    Canceled(Request<Canceled>),
}

/// Enum that can hold a request in any state.
///
/// This is used for storage and API responses where we need to handle
/// requests uniformly regardless of their current state.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", content = "request")]
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

    /// Get the variant name of the current state.
    pub fn variant(&self) -> &'static str {
        match self {
            AnyRequest::Pending(_) => "Pending",
            AnyRequest::Claimed(_) => "Claimed",
            AnyRequest::Processing(_) => "Processing",
            AnyRequest::Completed(_) => "Completed",
            AnyRequest::Failed(_) => "Failed",
            AnyRequest::Canceled(_) => "Canceled",
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
