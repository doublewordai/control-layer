use thiserror::Error;

/// Result type for batcher operations.
pub type Result<T> = std::result::Result<T, BatcherError>;

/// Errors that can occur in the batcher system.
#[derive(Debug, Error)]
pub enum BatcherError {
    /// Database operation failed
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    /// Request not found
    #[error("Request not found: {0}")]
    RequestNotFound(String),

    /// Invalid request parameters
    #[error("Invalid request: {0}")]
    InvalidRequest(String),

    /// HTTP request failed
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON serialization/deserialization failed
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Request was canceled
    #[error("Request canceled: {0}")]
    Canceled(String),

    /// Internal error
    #[error("Internal error: {0}")]
    Internal(String),
}
