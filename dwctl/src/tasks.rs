//! Background task processing via underway.
//!
//! This module provides a task runner that manages underway jobs for deferred
//! work. Job definitions live alongside their handlers (e.g. batch population
//! is defined in `api::handlers::batches`); this module wires them together.

use std::sync::Arc;

use anyhow::Result;
use fusillade::PostgresRequestManager;
use sqlx::PgPool;
use sqlx_pool_router::PoolProvider;
use tokio_util::sync::CancellationToken;
use underway::Job;

use crate::api::handlers::batches::{CreateBatchInput, build_create_batch_job};

/// Shared state available to all task step closures.
///
/// Generic over pool provider so it works with both `DbPools` (production)
/// and `TestDbPools` (tests).
#[derive(Clone)]
pub struct TaskState<P: PoolProvider + Clone = sqlx_pool_router::DbPools> {
    pub request_manager: Arc<PostgresRequestManager<P, fusillade::ReqwestHttpClient>>,
}

/// Manages underway jobs and worker lifecycle.
///
/// Built once at startup, stored in `AppState`. Handlers use it to enqueue
/// work; the worker processes jobs in the background.
pub struct TaskRunner<P: PoolProvider + Clone + 'static = sqlx_pool_router::DbPools> {
    pub create_batch_job: Job<CreateBatchInput, TaskState<P>>,
}

impl<P: PoolProvider + Clone + Send + Sync + 'static> TaskRunner<P> {
    /// Build the task runner, registering all job types.
    ///
    /// Call [`start`] to begin processing.
    pub async fn new(pool: PgPool, state: TaskState<P>) -> Result<Self> {
        let create_batch_job = build_create_batch_job(pool, state).await?;
        Ok(Self { create_batch_job })
    }

    /// Start the underway worker with the given shutdown token.
    ///
    /// The worker stops when the token is cancelled — either explicitly via
    /// [`BackgroundServices::shutdown`] or automatically via its `DropGuard`.
    /// Interrupted tasks are retried on next startup (state tracked in Postgres).
    pub fn start(&self, shutdown_token: CancellationToken) {
        let mut worker = self.create_batch_job.worker();
        worker.set_shutdown_token(shutdown_token);

        tokio::spawn(async move {
            if let Err(e) = worker.run().await {
                tracing::error!(error = %e, "Underway worker error");
            }
        });
    }

}
