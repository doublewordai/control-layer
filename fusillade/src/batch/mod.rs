//! File and batch types for grouping requests.
//!
//! This module defines types for:
//! - Files: Collections of request templates
//! - Request templates: Mutable request definitions
//! - Batches: Execution triggers for files

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

/// Unique identifier for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
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

/// A file containing a collection of request templates.
#[derive(Debug, Clone, Serialize)]
pub struct File {
    pub id: FileId,
    pub name: String,
    pub description: Option<String>,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestTemplateInput {
    pub endpoint: String,
    pub method: String,
    pub path: String,
    pub body: String,
    pub model: String,
    pub api_key: String,
}

/// A batch represents one execution of all of a file's templates.
#[derive(Debug, Clone, Serialize)]
pub struct Batch {
    pub id: BatchId,
    pub file_id: FileId,
    pub created_at: DateTime<Utc>,
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
    pub last_updated_at: Option<DateTime<Utc>>,
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
}
