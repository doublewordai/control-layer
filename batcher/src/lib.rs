//! Batching system for HTTP requests with retry logic and concurrency control.
//!
//! This crate provides 'managers' that accept submitted HTTP requests, and provides an API for
//! checking their status over time. Behind the scenes, a daemon processes these requests in
//! batches, retrying failed requests with exponential backoff and enforcing concurrency limits
//!
//! # Example
//! ```no_run
//! use batcher::{
//!     InMemoryRequestManager, RequestManager, ReqwestHttpClient,
//!     DaemonConfig, Request, RequestData, RequestId, Pending
//! };
//! use std::sync::Arc;
//! use uuid::Uuid;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // Create HTTP client and request manager
//!     let http_client = Arc::new(ReqwestHttpClient::new());
//!     let config = DaemonConfig::default();
//!     let manager = Arc::new(InMemoryRequestManager::new(http_client, config));
//!
//!     // Start the daemon
//!     let _daemon_handle = manager.run()?;
//!
//!     // Create and submit a request
//!     let request = Request {
//!         state: Pending {
//!             retry_attempt: 0,
//!             not_before: None,
//!         },
//!         data: RequestData {
//!             id: RequestId::from(Uuid::new_v4()),
//!             endpoint: "https://api.example.com".to_string(),
//!             method: "POST".to_string(),
//!             path: "/v1/completions".to_string(),
//!             body: r#"{"prompt": "Hello"}"#.to_string(),
//!             model: "gpt-4".to_string(),
//!             api_key: "your-api-key".to_string(),
//!         },
//!     };
//!
//!     let results = manager.submit_requests(vec![request]).await?;
//!     let request_id = results.into_iter().next().unwrap()?;
//!
//!     // Check status
//!     let statuses = manager.get_status(vec![request_id]).await?;
//!     println!("Status: {:?}", statuses);
//!
//!     Ok(())
//! }
//! ```

pub mod daemon;
pub mod error;
pub mod http;
pub mod manager;
pub mod request;
pub mod storage;

// Re-export commonly used types at the crate root for convenience.
// This allows users to write `use batcher::InMemoryRequestManager` instead of
// `use batcher::manager::in_memory::InMemoryRequestManager`, simplifying the API.
// These types form the public interface that most users will interact with:
// - Core traits (RequestManager, Storage, HttpClient)
// - Main implementations (InMemoryRequestManager, InMemoryStorage, ReqwestHttpClient)
// - Configuration (DaemonConfig)
// - Request types and states (Request, RequestData, Pending, Processing, etc.)
// - Error handling (BatcherError, Result)
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
