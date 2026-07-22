//! Batching system for HTTP requests with retry logic and concurrency control.
//!
//! This crate provides 'managers' that accept submitted HTTP requests, and provides an API for
//! checking their status over time. Behind the scenes, a daemon processes these requests in
//! batches, retrying failed requests with exponential backoff and enforcing concurrency limits
//!
//! Batching system with PostgreSQL storage and background daemon for processing requests.

pub mod bg_errors;
pub mod daemon;
pub mod http;
pub mod manager;
pub mod processor;
pub mod transform;

pub use fusillade_core::{batch, error, request, response_step};

// Re-export commonly used types
pub use daemon::{Daemon, DaemonConfig, DaemonMode, ModelEscalationConfig};
pub use fusillade_core::batch::*;
pub use fusillade_core::error::{FusilladeError, Result};
pub use fusillade_core::request::*;
pub use fusillade_core::response_step::{
    CreateStepInput, ResponseStep, ResponseStepStore, StepId, StepKind, StepState,
};
pub use http::{
    HttpClient, HttpResponse, MockHttpClient, ReqwestHttpClient, StreamEvent, StreamEventCallback,
    StreamReassembler,
};
#[cfg(feature = "postgres")]
pub use manager::DaemonExecutor;
#[cfg(feature = "postgres")]
pub use manager::PostgresDaemon;
pub use manager::{DaemonStorage, ModelFilter, ModelFilterState, Storage};
pub use processor::{CancellationFuture, DefaultRequestProcessor, RequestProcessor, ShouldRetry};
pub use transform::ResponseTransformer;
