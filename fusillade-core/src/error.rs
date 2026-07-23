//! Error types for the batching system.

use thiserror::Error;

use crate::types::{AttemptId, RequestId};

/// Result type alias using the fusillade error type.
pub type Result<T> = std::result::Result<T, FusilladeError>;

/// Main error type for the batching system.
#[derive(Error, Debug)]
pub enum FusilladeError {
    /// Request not found
    #[error("Request not found: {0}")]
    RequestNotFound(RequestId),

    /// Request exists but is not in the expected state for the requested
    /// operation (e.g., completing an already-completed or failed request).
    ///
    /// Distinct from [`RequestNotFound`] so callers can be properly idempotent
    /// against concurrent writers — e.g. a complete-then-complete race where
    /// the second caller should treat "already completed" as success rather
    /// than synthesizing a new row.
    #[error("Request {id} is in state '{current_state}', expected one of: {expected}")]
    RequestStateConflict {
        id: RequestId,
        current_state: String,
        expected: &'static str,
    },

    /// The in-memory daemon execution no longer owns the persisted request.
    #[error("Request {id} attempt {attempt_id} lost ownership")]
    RequestAttemptLost {
        id: RequestId,
        attempt_id: AttemptId,
    },

    /// A transient infrastructure failure while applying an attempt-aware
    /// persistence operation.
    ///
    /// Reserved for storage adapters after positive classification of a
    /// transient database or persistence-infrastructure failure. This must
    /// never represent validation, serialization, transformation, admission
    /// closure, or programming errors.
    #[error("Request attempt persistence infrastructure failure during {operation}")]
    AttemptPersistenceInfrastructure {
        operation: &'static str,
        #[source]
        source: anyhow::Error,
    },

    /// Cancelled request
    #[error("Request cancelled: {0}")]
    RequestCancelled(RequestId),

    /// Daemon is shutting down
    #[error("Daemon is shutting down")]
    Shutdown,

    /// Request is in an invalid state for the requested operation
    #[error("Invalid state transition: request {0} is in state '{1}', expected '{2}'")]
    InvalidState(RequestId, String, String),

    /// Validation error (e.g., invalid file format, missing required fields)
    #[error("Validation error: {0}")]
    ValidationError(String),

    /// HTTP client error.
    #[error("HTTP request failed: {0}")]
    HttpClient(String),

    /// HTTP request builder error.
    #[error("HTTP request builder failed: {0}")]
    HttpRequestBuilder(String),

    /// HTTP client timeout.
    #[error("HTTP request timed out: {0}")]
    HttpClientTimeout(String),

    /// Timed out waiting for response headers + first body chunk (time-to-first-token).
    /// Only used for streaming requests. Handles servers (like vLLM) that return
    /// headers immediately but queue the request before producing tokens.
    #[error("First chunk timeout: {0}")]
    FirstChunkTimeout(String),

    /// The request body upload made no progress for the configured stall window.
    /// Distinguishes send-phase hangs (a wedged connection or a stalled write)
    /// from a slow-to-respond upstream, which is governed by first_chunk_timeout:
    /// uploading a request should take seconds even when the answer takes hours.
    #[error("Upload stall timeout: {0}")]
    UploadStallTimeout(String),

    /// Timed out waiting for the next chunk of response body tokens (streaming only)
    #[error("Tokens timeout: {0}")]
    TokensTimeout(String),

    /// Timed out waiting for the entire response body to complete (streaming only).
    /// Fires when the total body read exceeds body_timeout.
    #[error("Body timeout: {0}")]
    BodyTimeout(String),

    /// Serialization/deserialization error
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// General error from anyhow
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Helper functions for serializing and deserializing errors to/from JSON.
///
/// These are used to store error information in the database in a structured format.
/// TODO: What's the point of this module? Thisi s just serde logic right? Why can't we just use
/// serde_json
pub mod error_serialization {
    use anyhow::Error;
    use serde::{Deserialize, Serialize};

    /// Serialized error format that preserves error message and source chain.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct SerializedError {
        /// The main error message
        pub message: String,
        /// Chain of source errors, if any
        pub sources: Vec<String>,
    }

    /// Serializes an anyhow::Error to a JSON string.
    ///
    /// Preserves the error message and the chain of source errors.
    pub fn serialize_error(error: &Error) -> String {
        let serialized = SerializedError {
            message: error.to_string(),
            sources: error.chain().skip(1).map(|e| e.to_string()).collect(),
        };
        serde_json::to_string(&serialized).unwrap_or_else(|_| {
            format!(
                r#"{{"message":"{}","sources":[]}}"#,
                error.to_string().replace('"', "\\\"")
            )
        })
    }

    /// Deserializes an error from a JSON string.
    ///
    /// Returns an anyhow::Error with the original message.
    pub fn deserialize_error(json: &str) -> Error {
        match serde_json::from_str::<SerializedError>(json) {
            Ok(serialized) => {
                let mut error_msg = serialized.message;
                if !serialized.sources.is_empty() {
                    error_msg.push_str("\nCaused by:\n");
                    for (i, source) in serialized.sources.iter().enumerate() {
                        error_msg.push_str(&format!("  {}: {}\n", i + 1, source));
                    }
                }
                anyhow::anyhow!(error_msg)
            }
            Err(_) => {
                // Fallback: treat the entire string as an error message
                anyhow::anyhow!("Deserialization failed: {}", json)
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_serialize_deserialize_simple_error() {
            let error = anyhow::anyhow!("Test error");
            let serialized = serialize_error(&error);
            let deserialized = deserialize_error(&serialized);
            assert_eq!(error.to_string(), deserialized.to_string());
        }

        #[test]
        fn test_serialize_deserialize_with_context() {
            let error = anyhow::anyhow!("Root cause")
                .context("Middle context")
                .context("Top context");
            let serialized = serialize_error(&error);
            let deserialized = deserialize_error(&serialized);
            // The deserialized error should contain the full chain
            assert!(deserialized.to_string().contains("Top context"));
            assert!(deserialized.to_string().contains("Middle context"));
            assert!(deserialized.to_string().contains("Root cause"));
        }
    }
}
