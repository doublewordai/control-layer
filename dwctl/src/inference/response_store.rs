//! Response storage trait for the OpenAI Responses lifecycle.
//!
//! Relocated from onwards (COR-536): onwards no longer knows about Responses, so
//! the trait its edge translator + storage need lives in dwctl. Only the two
//! methods dwctl actually uses are kept - `store` (persist a produced Responses
//! object) and `get_context` (read a prior turn for `previous_response_id`
//! hydration). The Fusillade-backed implementation is in [`super::store`].

use std::fmt;

use async_trait::async_trait;
use serde_json::Value;

/// Error type for response store operations.
#[derive(Debug, Clone)]
pub enum StoreError {
    /// Response not found with the given id.
    NotFound(String),
    /// Storage backend error.
    StorageError(String),
    /// Serialization/deserialization error.
    SerializationError(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::NotFound(id) => write!(f, "Response not found: {id}"),
            StoreError::StorageError(msg) => write!(f, "Storage error: {msg}"),
            StoreError::SerializationError(msg) => write!(f, "Serialization error: {msg}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// Response lifecycle storage for the Responses API.
#[async_trait]
pub trait ResponseStore: Send + Sync {
    /// Persist a completed Responses object and return its id.
    async fn store(&self, response: &Value) -> Result<String, StoreError>;

    /// Retrieve a prior response's context (for `previous_response_id` hydration).
    async fn get_context(&self, response_id: &str) -> Result<Option<Value>, StoreError>;
}

/// No-op store: returns a generated id and never resolves context. Used where a
/// store is structurally required but no persistence is wanted (e.g. tests).
#[derive(Debug, Clone, Default)]
pub struct NoOpResponseStore;

#[async_trait]
impl ResponseStore for NoOpResponseStore {
    async fn store(&self, _response: &Value) -> Result<String, StoreError> {
        Ok("noop".to_string())
    }
    async fn get_context(&self, _response_id: &str) -> Result<Option<Value>, StoreError> {
        Ok(None)
    }
}
