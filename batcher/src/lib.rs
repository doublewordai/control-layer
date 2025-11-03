//! Batching system for HTTP requests with retry logic and concurrency control.
//!
//! This crate provides 'managers' that accept submitted HTTP requests, and provides an API for
//! checking their status over time. Behind the scenes, a daemon processes these requests in
//! batches, retrying failed requests with exponential backoff and enforcing concurrency limits
//!
//! // TODO: Make this example runnable
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
// TODO: This isn't very clean - why are these specifically reexported at this top level?
pub use daemon::{Daemon, DaemonConfig};
pub use error::{BatcherError, Result};
pub use http::{HttpClient, HttpResponse, MockHttpClient, ReqwestHttpClient};
pub use manager::in_memory::InMemoryRequestManager;
pub use manager::RequestManager;
pub use request::*;
pub use storage::in_memory::InMemoryStorage;
pub use storage::Storage;

// Postgres-specific exports (only available with "postgres" feature)
#[cfg(feature = "postgres")]
pub use manager::postgres::PostgresRequestManager;
#[cfg(feature = "postgres")]
pub use storage::postgres::PostgresStorage;
