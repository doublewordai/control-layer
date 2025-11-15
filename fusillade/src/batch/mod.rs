//! File and batch types for grouping requests.
//!
//! This module defines types for:
//! - Files: Collections of request templates
//! - Request templates: Mutable request definitions
//! - Batches: Execution triggers for files
//!
//! See request/ for the types for requests, since they have their logic more tightly coupled to
//! their models.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

/// Unique identifier for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileId(pub Uuid);

impl From<Uuid> for FileId {
    fn from(uuid: Uuid) -> Self {
        FileId(uuid)
    }
}

impl std::ops::Deref for FileId {
    type Target = Uuid;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Display for FileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", &self.0.to_string()[..8])
    }
}

/// Unique identifier for a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct BatchId(pub Uuid);

impl From<Uuid> for BatchId {
    fn from(uuid: Uuid) -> Self {
        BatchId(uuid)
    }
}

impl std::ops::Deref for BatchId {
    type Target = Uuid;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Display for BatchId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", &self.0.to_string()[..8])
    }
}

/// Unique identifier for a request template.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct TemplateId(pub Uuid);

impl From<Uuid> for TemplateId {
    fn from(uuid: Uuid) -> Self {
        TemplateId(uuid)
    }
}

impl std::ops::Deref for TemplateId {
    type Target = Uuid;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Display for TemplateId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", &self.0.to_string()[..8])
    }
}

/// Purpose for which a file was created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    /// File contains batch API request templates
    Batch,
    /// Virtual file that streams batch output (completed requests)
    BatchOutput,
    /// Virtual file that streams batch errors (failed requests)
    BatchError,
}

/// Type of batch output file for lookup purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFileType {
    /// Output file containing completed requests
    Output,
    /// Error file containing failed requests
    Error,
}

impl fmt::Display for Purpose {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Purpose::Batch => write!(f, "batch"),
            Purpose::BatchOutput => write!(f, "batch_output"),
            Purpose::BatchError => write!(f, "batch_error"),
        }
    }
}

impl FromStr for Purpose {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "batch" => Ok(Purpose::Batch),
            "batch_output" => Ok(Purpose::BatchOutput),
            "batch_error" => Ok(Purpose::BatchError),
            _ => Err(format!("Invalid purpose: {}", s)),
        }
    }
}

/// File status tracking lifecycle and health.
///
/// Status tracks the file's lifecycle:
/// - `Processed`: Successfully uploaded and parsed into templates (users can access)
/// - `Error`: Failed to process during upload (only visible to admins with SystemAccess)
/// - `Deleted`: Soft-deleted by user (metadata retained for audit, only visible to admins)
/// - `Expired`: Past its expiration date (metadata retained for audit, only visible to admins)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    /// File was successfully processed and templates created
    Processed,
    /// File processing failed (see error_message for details)
    Error,
    /// File was soft-deleted by user (metadata retained for audit)
    Deleted,
    /// File has passed its expiration date
    Expired,
}

impl fmt::Display for FileStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileStatus::Processed => write!(f, "processed"),
            FileStatus::Error => write!(f, "error"),
            FileStatus::Deleted => write!(f, "deleted"),
            FileStatus::Expired => write!(f, "expired"),
        }
    }
}

impl FromStr for FileStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "processed" => Ok(FileStatus::Processed),
            "error" => Ok(FileStatus::Error),
            "deleted" => Ok(FileStatus::Deleted),
            "expired" => Ok(FileStatus::Expired),
            _ => Err(format!("Invalid file status: {}", s)),
        }
    }
}

/// A file containing a collection of request templates.
#[derive(Debug, Clone, Serialize)]
pub struct File {
    pub id: FileId,
    pub name: String,
    pub description: Option<String>,
    pub size_bytes: i64,
    pub status: FileStatus,
    pub error_message: Option<String>,
    pub purpose: Option<Purpose>,
    pub expires_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub uploaded_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A request template defining how to make a request.
///
/// Templates are mutable, but requests snapshot the template state
/// at execution time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestTemplate {
    pub id: TemplateId,
    pub file_id: FileId,
    pub custom_id: Option<String>, // OpenAI Batch API custom identifier
    pub endpoint: String,
    pub method: String,
    pub path: String,
    pub body: String,
    pub model: String,
    pub api_key: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Input for creating a new request template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct RequestTemplateInput {
    pub custom_id: Option<String>, // OpenAI Batch API custom identifier
    pub endpoint: String,
    pub method: String,
    pub path: String,
    pub body: String,
    pub model: String,
    pub api_key: String,
}

/// Batch output item - represents a completed request in OpenAI format.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct BatchOutputItem {
    /// Request ID
    pub id: String,
    /// Custom ID from the original request
    pub custom_id: Option<String>,
    /// Response details
    pub response: BatchResponseDetails,
    /// Error (should be null for successful responses)
    pub error: Option<serde_json::Value>,
}

/// Batch error item - represents a failed request in OpenAI format.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct BatchErrorItem {
    /// Request ID
    pub id: String,
    /// Custom ID from the original request
    pub custom_id: Option<String>,
    /// Response (should be null for errors)
    pub response: Option<serde_json::Value>,
    /// Error details
    pub error: BatchErrorDetails,
}

/// Response details for a batch output item.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct BatchResponseDetails {
    /// HTTP status code
    pub status_code: i16,
    /// Request ID from the upstream API
    pub request_id: Option<String>,
    /// Response body (e.g., ChatCompletion, Embedding, etc.)
    pub body: serde_json::Value,
}

/// Error details for a batch error item.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct BatchErrorDetails {
    /// Error code
    pub code: Option<String>,
    /// Error message
    pub message: String,
}

/// Enum for different types of file content that can be streamed.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum FileContentItem {
    /// Request template (for input files with purpose='batch')
    Template(RequestTemplateInput),
    /// Batch output (for output files with purpose='batch_output')
    Output(BatchOutputItem),
    /// Batch error (for error files with purpose='batch_error')
    Error(BatchErrorItem),
}

/// Metadata for creating a file from a stream
#[derive(Debug, Clone, Default, Serialize)]
pub struct FileMetadata {
    pub filename: Option<String>,
    pub description: Option<String>,
    pub purpose: Option<String>,
    pub expires_after_anchor: Option<String>,
    pub expires_after_seconds: Option<i64>,
    pub size_bytes: Option<i64>,
    pub uploaded_by: Option<String>,
}

/// Filter parameters for listing files
#[derive(Debug, Clone, Default)]
pub struct FileFilter {
    /// Filter by user who uploaded the file
    /// TODO: We use a string here, because this crate is decoupled from the dwctl one which uses a
    /// UUID. Is this fine? This just needs to be a unique identifier per user.
    pub uploaded_by: Option<String>,
    /// Filter by file status (processed, error, deleted, expired)
    pub status: Option<String>,
    /// Filter by purpose
    pub purpose: Option<String>,
    /// Cursor for pagination (file ID to start after)
    pub after: Option<FileId>,
    /// Maximum number of results to return
    pub limit: Option<usize>,
    /// Sort order (true = ascending, false = descending)
    pub ascending: bool,
}

/// Items that can be yielded from a file upload stream
#[derive(Debug, Clone, Serialize)]
pub enum FileStreamItem {
    /// File metadata (should be first item in stream)
    Metadata(FileMetadata),
    /// A request template parsed from JSONL
    Template(RequestTemplateInput),
    /// An error occurred during parsing
    Error(String),
}

/// Input parameters for creating a new batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchInput {
    /// The file containing request templates
    pub file_id: FileId,
    /// The API endpoint to use for all requests (e.g., "/v1/chat/completions")
    pub endpoint: String,
    /// Completion window (e.g., "24h")
    pub completion_window: String,
    /// Optional metadata key-value pairs (OpenAI allows up to 16 pairs)
    pub metadata: Option<serde_json::Value>,
    /// User who created this batch (for ownership tracking)
    pub created_by: Option<String>,
}

/// A batch represents one execution of all of a file's templates.
#[derive(Debug, Clone, Serialize)]
pub struct Batch {
    pub id: BatchId,
    pub file_id: FileId,
    pub created_at: DateTime<Utc>,
    /// Metadata key-value pairs (OpenAI allows up to 16 pairs)
    pub metadata: Option<serde_json::Value>,
    /// Completion window (e.g., "24h")
    pub completion_window: String,
    /// The API endpoint to use for all requests (e.g., "/v1/chat/completions")
    pub endpoint: String,
    /// File ID containing the successful results
    pub output_file_id: Option<FileId>,
    /// File ID containing the error results
    pub error_file_id: Option<FileId>,
    /// User who created this batch
    pub created_by: Option<String>,
    /// When the batch will expire (created_at + completion_window)
    pub expires_at: Option<DateTime<Utc>>,
    /// When batch cancellation was initiated
    pub cancelling_at: Option<DateTime<Utc>>,
    /// Batch-level errors (validation errors, system errors, etc.)
    pub errors: Option<serde_json::Value>,

    /// Status fields
    pub total_requests: i64,
    pub pending_requests: i64,
    pub in_progress_requests: i64,
    pub completed_requests: i64,
    pub failed_requests: i64,
    pub canceled_requests: i64,
    pub requests_started_at: Option<DateTime<Utc>>,

    /// Terminal state timestamps (set once when batch enters that state)
    pub finalizing_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
    pub cancelled_at: Option<DateTime<Utc>>,
}

/// Status information for a batch, computed from its executions.
#[derive(Debug, Clone, Serialize)]
pub struct BatchStatus {
    pub batch_id: BatchId,
    pub file_id: FileId,
    pub file_name: String,
    pub total_requests: i64,
    pub pending_requests: i64,
    pub in_progress_requests: i64,
    pub completed_requests: i64,
    pub failed_requests: i64,
    pub canceled_requests: i64,
    pub started_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl BatchStatus {
    /// Check if the batch has finished (all requests in terminal state).
    pub fn is_finished(&self) -> bool {
        self.completed_requests + self.failed_requests + self.canceled_requests
            == self.total_requests
    }

    /// Check if the batch is still running.
    pub fn is_running(&self) -> bool {
        !self.is_finished()
    }

    /// Get OpenAI-compatible status string.
    /// Maps internal state to OpenAI's status values:
    /// - "validating" - batch just created, no requests started yet
    /// - "in_progress" - batch is being processed
    /// - "finalizing" - nearly all requests done (95%+ complete)
    /// - "completed" - all requests in terminal state and at least one succeeded
    /// - "failed" - all requests in terminal state and all failed
    /// - "cancelled" - all requests cancelled
    pub fn openai_status(&self) -> &'static str {
        if self.total_requests == 0 {
            return "validating";
        }

        let terminal_count =
            self.completed_requests + self.failed_requests + self.canceled_requests;

        if terminal_count == 0 {
            // Nothing has started yet
            "validating"
        } else if terminal_count == self.total_requests {
            // All done - determine outcome
            if self.canceled_requests == self.total_requests {
                "cancelled"
            } else if self.completed_requests == 0 {
                "failed"
            } else {
                "completed"
            }
        } else if terminal_count as f64 / self.total_requests as f64 >= 0.95 {
            // Nearly done (95%+)
            "finalizing"
        } else {
            // In progress
            "in_progress"
        }
    }
}
