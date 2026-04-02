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
use crate::connections::sync::{
    ActivateBatchInput, IngestFileInput, SyncConnectionInput, build_activate_batch_job,
    build_ingest_file_job, build_sync_connection_job,
};

/// Shared state available to all task step closures.
///
/// Generic over pool provider so it works with both `DbPools` (production)
/// and `TestDbPools` (tests).
#[derive(Clone)]
pub struct TaskState<P: PoolProvider + Clone = sqlx_pool_router::DbPools> {
    pub request_manager: Arc<PostgresRequestManager<P, fusillade::ReqwestHttpClient>>,
    /// dwctl database pool — for querying connections, sync_operations, sync_entries.
    /// Separate from request_manager which uses the fusillade database.
    pub dwctl_pool: PgPool,
    /// Encryption key for decrypting connection credentials inside jobs.
    pub encryption_key: Option<Vec<u8>>,
    /// Reference to the IngestFileJob so SyncConnectionJob can enqueue it.
    pub ingest_file_job: Option<Arc<Job<IngestFileInput, TaskState<P>>>>,
    /// Reference to the ActivateBatchJob so IngestFileJob can enqueue it.
    pub activate_batch_job: Option<Arc<Job<ActivateBatchInput, TaskState<P>>>>,
    /// Reference to the CreateBatchInput job so ActivateBatchJob can enqueue populate.
    pub create_batch_job: Option<Arc<Job<CreateBatchInput, TaskState<P>>>>,
}

/// Manages underway jobs and worker lifecycle.
///
/// Built once at startup, stored in `AppState`. Handlers use it to enqueue
/// work; the worker processes jobs in the background.
pub struct TaskRunner<P: PoolProvider + Clone + 'static = sqlx_pool_router::DbPools> {
    pub create_batch_job: Job<CreateBatchInput, TaskState<P>>,
    pub sync_connection_job: Job<SyncConnectionInput, TaskState<P>>,
    pub ingest_file_job: Job<IngestFileInput, TaskState<P>>,
    pub activate_batch_job: Job<ActivateBatchInput, TaskState<P>>,
}

impl<P: PoolProvider + Clone + Send + Sync + 'static> TaskRunner<P> {
    /// Build the task runner, registering all job types.
    ///
    /// Call [`start`] to begin processing.
    pub async fn new(pool: PgPool, mut state: TaskState<P>) -> Result<Self> {
        // First pass: build jobs so we can get Arc references for cross-job enqueueing.
        let ingest_arc = Arc::new(build_ingest_file_job(pool.clone(), state.clone()).await?);
        let activate_arc = Arc::new(build_activate_batch_job(pool.clone(), state.clone()).await?);
        let create_batch_arc = Arc::new(build_create_batch_job(pool.clone(), state.clone()).await?);

        // Wire up cross-references so jobs can enqueue each other.
        state.ingest_file_job = Some(ingest_arc.clone());
        state.activate_batch_job = Some(activate_arc.clone());
        state.create_batch_job = Some(create_batch_arc.clone());

        // Second pass: rebuild with cross-references wired up.
        let create_batch_job = build_create_batch_job(pool.clone(), state.clone()).await?;
        let sync_connection_job = build_sync_connection_job(pool.clone(), state.clone()).await?;
        let ingest_file_job = build_ingest_file_job(pool.clone(), state.clone()).await?;
        let activate_batch_job = build_activate_batch_job(pool.clone(), state).await?;

        Ok(Self {
            create_batch_job,
            sync_connection_job,
            ingest_file_job,
            activate_batch_job,
        })
    }

    /// Start the underway worker with the given shutdown token.
    ///
    /// The worker stops when the token is cancelled — either explicitly via
    /// [`BackgroundServices::shutdown`] or automatically via its `DropGuard`.
    /// Interrupted tasks are retried on next startup (state tracked in Postgres).
    pub fn start(&self, shutdown_token: CancellationToken) -> Vec<tokio::task::JoinHandle<()>> {
        let mut handles = Vec::new();

        // Create batch worker
        let mut create_batch_worker = self.create_batch_job.worker();
        create_batch_worker.set_shutdown_token(shutdown_token.clone());
        handles.push(tokio::spawn(async move {
            if let Err(e) = create_batch_worker.run().await {
                tracing::error!(error = %e, "Create-batch worker error");
            }
        }));

        // Sync connection worker
        let mut sync_worker = self.sync_connection_job.worker();
        sync_worker.set_shutdown_token(shutdown_token.clone());
        handles.push(tokio::spawn(async move {
            if let Err(e) = sync_worker.run().await {
                tracing::error!(error = %e, "Sync-connection worker error");
            }
        }));

        // Ingest file worker
        let mut ingest_worker = self.ingest_file_job.worker();
        ingest_worker.set_shutdown_token(shutdown_token.clone());
        handles.push(tokio::spawn(async move {
            if let Err(e) = ingest_worker.run().await {
                tracing::error!(error = %e, "Ingest-file worker error");
            }
        }));

        // Activate batch worker
        let mut activate_worker = self.activate_batch_job.worker();
        activate_worker.set_shutdown_token(shutdown_token);
        handles.push(tokio::spawn(async move {
            if let Err(e) = activate_worker.run().await {
                tracing::error!(error = %e, "Activate-batch worker error");
            }
        }));

        handles
    }
}
