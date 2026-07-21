//! Daemon runtime traits.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::task::JoinHandle;

use crate::error::Result;
use crate::http::HttpClient;

#[cfg(feature = "postgres")]
mod postgres;
#[cfg(feature = "postgres")]
pub use postgres::PostgresDaemon;

pub use fusillade_core::manager::{
    ArchiveOutcome, DaemonStorage, ModelFilter, ModelFilterState, Storage,
};

/// Daemon executor trait for runtime orchestration.
///
/// This trait handles only the daemon lifecycle: spawning the background
/// worker that processes requests. Durable data operations live on
/// [`Storage`] in `fusillade-core`.
#[async_trait]
pub trait DaemonExecutor<H: HttpClient>: Send + Sync {
    /// Get a reference to the HTTP client.
    fn http_client(&self) -> &Arc<H>;

    /// Get a reference to the daemon configuration.
    fn config(&self) -> &crate::daemon::DaemonConfig;

    /// Run the daemon thread.
    fn run(
        self: Arc<Self>,
        shutdown_token: tokio_util::sync::CancellationToken,
    ) -> Result<JoinHandle<Result<()>>> {
        let mode = self.config().mode;
        self.run_with_mode(shutdown_token, mode)
    }

    /// Run the daemon thread with an explicit mode.
    fn run_with_mode(
        self: Arc<Self>,
        shutdown_token: tokio_util::sync::CancellationToken,
        mode: crate::daemon::DaemonMode,
    ) -> Result<JoinHandle<Result<()>>>;
}
