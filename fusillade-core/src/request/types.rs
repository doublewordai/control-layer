//! Core types for the batching system.
//!
//! This module defines the type-safe request lifecycle using the typestate pattern.
//! Each request progresses through distinct states, enforced at compile time.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc};
use tokio::task::AbortHandle;
use uuid::Uuid;

use crate::batch::{BatchId, TemplateId};
use crate::error::Result;

/// Response from an upstream request execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response body as a string.
    pub body: String,
}

/// Database state for filtering and querying requests.
///
/// This enum represents the string values stored in the database's `state` column.
/// It's used for filtering operations like SLA monitoring and escalation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "sqlx-postgres", derive(sqlx::Type))]
#[cfg_attr(
    feature = "sqlx-postgres",
    sqlx(type_name = "text", rename_all = "lowercase")
)]
pub enum RequestStateFilter {
    Pending,
    Claimed,
    Processing,
    Completed,
    Failed,
    Canceled,
}

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

    /// The batch this execution belongs to. `None` for batchless responses
    /// (created via `create_realtime` / `create_flex`).
    pub batch_id: Option<BatchId>,

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

    /// User who created the batch this request belongs to.
    /// Used by the daemon for per-user fair scheduling.
    #[serde(default)]
    pub created_by: String,

    /// Batch metadata fields to be sent as headers (x-fusillade-COLUMN-NAME)
    #[serde(skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub batch_metadata: std::collections::HashMap<String, String>,
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

    /// When the batch expires (used for deadline-aware retry logic)
    pub batch_expires_at: DateTime<Utc>,
}

impl RequestState for Pending {}

/// Leaky-bucket stamp for a request claimed via Source B (the not-live,
/// before-ramp trickle). Carried on [`Claimed`] as `Some` iff the row was
/// leaked; the daemon stamps `next_token_at = now + window_secs /
/// leaks_per_window` for the `(created_by, window_class, model)` bucket. `None`
/// for full-capacity (Source A) claims, which consume no token. The model is
/// read from the request itself, so it is not carried on the stamp.
#[derive(Debug, Clone, Serialize)]
pub struct LeakStamp {
    /// The bucket's window-class (a batch's `completion_window`, or a batchless
    /// row's `service_tier` defaulting to `'default'`).
    pub window_class: String,
    /// The request's completion-window length in seconds (`W`); the daemon
    /// divides it by `leaks_per_window` to get the leak interval.
    pub window_secs: f64,
}

/// Request has been claimed by a daemon but not yet actively executing.
///
/// This intermediate state helps track which daemon is responsible for
/// processing the request.
#[derive(Debug, Clone, Serialize)]
pub struct Claimed {
    pub daemon_id: DaemonId,
    /// Unique identity for this claim. A new value is assigned every time the
    /// request moves from pending to claimed, even when the daemon is unchanged.
    pub attempt_id: AttemptId,
    pub claimed_at: DateTime<Utc>,
    /// Number of times this request has been attempted (carried over from Pending)
    pub retry_attempt: u32,
    /// When the batch expires (carried over from Pending)
    pub batch_expires_at: DateTime<Utc>,
    /// Set iff this row was claimed via the leaky bucket (Source B); tells the
    /// daemon which `(user, window-class, model)` bucket to stamp (the model is
    /// read from the request's `data.model`). `None` for full-capacity claims.
    pub leak: Option<LeakStamp>,
}

impl RequestState for Claimed {}

/// Request is currently being processed by a daemon (i.e., HTTP request in flight).
#[derive(Debug, Clone, Serialize)]
pub struct Processing {
    pub daemon_id: DaemonId,
    /// The exact claim that owns this in-flight upstream execution.
    pub attempt_id: AttemptId,
    pub claimed_at: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
    /// Number of times this request has been attempted (carried over from Claimed)
    pub retry_attempt: u32,
    /// When the batch expires (carried over from Claimed)
    pub batch_expires_at: DateTime<Utc>,
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
    /// The model that was actually used (may differ from template if escalated)
    pub routed_model: String,
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

    /// HTTP request returned a client error status code that should not be retried (e.g., 400).
    /// These indicate problems with the request itself that won't be fixed by retrying.
    NonRetriableHttpStatus { status: u16, body: String },

    /// Network error or other transport-level failure (excluding timeouts).
    /// These are transient infrastructure issues that should be retried.
    NetworkError { error: String },

    /// Request timed out before receiving a response.
    /// These are transient and should be retried.
    Timeout { error: String },

    /// The HTTP task terminated unexpectedly (panic or crash).
    /// This should be retried as it may be a transient issue.
    TaskTerminated,

    /// Failed to construct the HTTP request before it could be sent.
    /// This includes invalid header values, malformed URLs, or other builder errors.
    /// These are data errors that will never succeed on retry.
    RequestBuilderError { error: String },

    /// The request's batch reached a terminal state (cancelled, failed, or expired)
    /// before this request could be processed. Not retriable.
    BatchTerminated,
}

impl FailureReason {
    /// Returns true if this failure reason indicates the request should be retried.
    pub fn is_retriable(&self) -> bool {
        match self {
            FailureReason::RetriableHttpStatus { .. } => true,
            FailureReason::NonRetriableHttpStatus { .. } => false,
            FailureReason::NetworkError { .. } => true,
            FailureReason::Timeout { .. } => true,
            FailureReason::TaskTerminated => true,
            FailureReason::RequestBuilderError { .. } => false,
            FailureReason::BatchTerminated => false,
        }
    }

    /// Returns a short, stable label for use in metrics.
    pub fn metric_label(&self) -> &'static str {
        match self {
            FailureReason::RetriableHttpStatus { .. } => "retriable_http_status",
            FailureReason::NonRetriableHttpStatus { .. } => "non_retriable_http_status",
            FailureReason::NetworkError { .. } => "network_error",
            FailureReason::Timeout { .. } => "timeout",
            FailureReason::TaskTerminated => "task_terminated",
            FailureReason::RequestBuilderError { .. } => "builder_error",
            FailureReason::BatchTerminated => "batch_terminated",
        }
    }

    /// Returns the HTTP status code as a string for metrics, or empty for non-HTTP failures.
    pub fn status_code_label(&self) -> String {
        match self {
            FailureReason::RetriableHttpStatus { status, .. }
            | FailureReason::NonRetriableHttpStatus { status, .. } => status.to_string(),
            _ => String::new(),
        }
    }

    /// Returns a human-readable error message for this failure reason.
    ///
    /// ZDR: the upstream response `body` is intentionally NOT included — provider
    /// error bodies can echo prompt/response content, and this message is emitted
    /// to logs at the daemon's retry/failure sites. Only the status and the body
    /// length are reported.
    pub fn to_error_message(&self) -> String {
        match self {
            FailureReason::RetriableHttpStatus { status, body } => {
                format!(
                    "HTTP request returned retriable status code: {} ({}-byte body omitted)",
                    status,
                    body.len()
                )
            }
            FailureReason::NonRetriableHttpStatus { status, body } => {
                format!(
                    "HTTP request returned error status code: {} ({}-byte body omitted)",
                    status,
                    body.len()
                )
            }
            FailureReason::NetworkError { error } => {
                format!("Network error: {}", error)
            }
            FailureReason::Timeout { error } => {
                format!("Request timed out: {}", error)
            }
            FailureReason::TaskTerminated => "HTTP task terminated unexpectedly".to_string(),
            FailureReason::BatchTerminated => {
                "Request was not processed because its batch reached a terminal state".to_string()
            }
            FailureReason::RequestBuilderError { error } => {
                format!("Failed to build HTTP request: {}", error)
            }
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
    /// When the batch expires (carried over from Processing)
    pub batch_expires_at: DateTime<Utc>,
    /// The model that was actually used (may differ from template if escalated)
    pub routed_model: String,
}

impl RequestState for Failed {}

/// Request was canceled by the caller.
#[derive(Debug, Clone, Serialize)]
pub struct Canceled {
    pub canceled_at: DateTime<Utc>,
}

impl RequestState for Canceled {}

/// Target state for cascading a batch's terminal state to its in-flight requests.
///
/// Used by `cascade_batch_state_to_requests` to transition pending/claimed/processing
/// requests into the appropriate terminal state, updating the correct timestamp column
/// for each variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CascadeTargetState {
    /// Cascade to canceled — sets `canceled_at`.
    Canceled,
    /// Cascade to failed — sets `failed_at`.
    Failed,
}

impl CascadeTargetState {
    pub fn as_str(&self) -> &'static str {
        match self {
            CascadeTargetState::Canceled => "canceled",
            CascadeTargetState::Failed => "failed",
        }
    }
}

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

/// Unique identifier for one daemon execution attempt.
///
/// Unlike [`DaemonId`], this changes on every claim and therefore distinguishes
/// overlapping executions started by the same daemon. A nil value may be used
/// only when reconstructing legacy database rows for inspection; it must never
/// authorize a state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct AttemptId(pub Uuid);

impl std::fmt::Display for AttemptId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl From<Uuid> for AttemptId {
    fn from(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

impl std::ops::Deref for AttemptId {
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

    /// Get the request data regardless of state.
    pub fn data(&self) -> &RequestData {
        match self {
            AnyRequest::Pending(r) => &r.data,
            AnyRequest::Claimed(r) => &r.data,
            AnyRequest::Processing(r) => &r.data,
            AnyRequest::Completed(r) => &r.data,
            AnyRequest::Failed(r) => &r.data,
            AnyRequest::Canceled(r) => &r.data,
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

#[cfg(test)]
mod attempt_id_tests {
    use super::*;

    #[test]
    fn display_uses_the_full_uuid() {
        let uuid = Uuid::parse_str("12345678-1234-4567-89ab-1234567890ab").unwrap();
        let attempt_id = AttemptId::from(uuid);

        assert_eq!(attempt_id.to_string(), uuid.to_string());
        assert_eq!(*attempt_id, uuid);
    }
}

#[cfg(test)]
mod zdr_logging_tests {
    use super::*;

    /// ZDR (COR-498): the human-readable error string fed to the daemon's
    /// retry/failure logs must never contain the upstream provider response body.
    #[test]
    fn to_error_message_omits_provider_body() {
        const SENTINEL: &str = "ZDR-SENTINEL-PROVIDER-BODY-7c1e";

        for reason in [
            FailureReason::RetriableHttpStatus {
                status: 503,
                body: format!("{{\"completion\":\"{SENTINEL}\"}}"),
            },
            FailureReason::NonRetriableHttpStatus {
                status: 400,
                body: format!("{{\"error\":\"{SENTINEL}\"}}"),
            },
        ] {
            let msg = reason.to_error_message();
            assert!(
                !msg.contains(SENTINEL),
                "provider response body leaked into error message: {msg}"
            );
            // The status must still be present so the log stays diagnostic.
            assert!(
                msg.contains(&reason.status_code_label()),
                "expected status code in message, got: {msg}"
            );
            // And the message must be non-empty (daemon asserts this).
            assert!(!msg.is_empty());
        }
    }
}
