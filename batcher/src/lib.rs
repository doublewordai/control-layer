//! Batching system for HTTP requests with retry logic and concurrency control.
//!
//! This crate provides a request batching system that:
//! - Accepts HTTP requests for processing
//! - Manages request lifecycle with type-safe state transitions
//! - Implements retry logic with exponential backoff
//! - Enforces per-model concurrency limits
//! - Provides real-time status updates
//!
//! # Example
//! ```ignore
//! use batcher::{InMemoryRequestManager, RequestManager, ReqwestHttpClient};
//!
//! let http_client = Arc::new(ReqwestHttpClient::new());
//! let manager = InMemoryRequestManager::new(http_client);
//!
//! // Start the daemon
//! let handle = manager.run()?;
//!
//! // Submit requests
//! let ids = manager.submit_requests(vec![(request, context)]).await?;
//!
//! // Check status
//! let statuses = manager.get_status(ids).await?;
//! ```

pub mod daemon;
pub mod error;
pub mod http;
pub mod manager;
pub mod request;
pub mod storage;

// Re-export commonly used types
pub use daemon::{Daemon, DaemonConfig};
pub use error::{BatcherError, Result};
pub use http::{HttpClient, HttpResponse, MockHttpClient, ReqwestHttpClient};
pub use manager::in_memory::InMemoryRequestManager;
pub use manager::RequestManager;
pub use request::*;
pub use storage::in_memory::InMemoryStorage;
pub use storage::Storage;
