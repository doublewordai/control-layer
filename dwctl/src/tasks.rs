//! Background task processing via underway.
//!
//! This module provides a task runner that manages underway jobs for deferred
//! work. Job definitions live alongside their handlers (e.g. batch population
//! is defined in `api::handlers::batches`); this module wires them together.

use std::sync::{Arc, OnceLock};

use anyhow::Result;
use fusillade::PostgresRequestManager;
use sqlx::PgPool;
use sqlx_pool_router::PoolProvider;
use tokio_util::sync::CancellationToken;
use underway::Job;

use crate::api::handlers::batches::{CreateBatchInput, build_create_batch_job};
use crate::connections::sync::{
    ActivateBatchInput, IngestFileInput, SyncConnectionInput, build_activate_batch_job, build_ingest_file_job, build_sync_connection_job,
};

/// Shared state available to all task step closures.
///
/// Generic over pool provider so it works with both `DbPools` (production)
/// and `TestDbPools` (tests).
///
/// A lazily-initialized, shared job reference. Set once after all jobs are
/// built; visible to every cloned `TaskState` via the shared `Arc<OnceLock>`.
type JobRef<I, P> = Arc<OnceLock<Arc<Job<I, TaskState<P>>>>>;

/// Cross-job references use `Arc<OnceLock<...>>` so they can be set once after
/// all jobs are built, and all cloned TaskState instances see the same value.
#[derive(Clone)]
pub struct TaskState<P: PoolProvider + Clone = sqlx_pool_router::DbPools> {
    pub request_manager: Arc<PostgresRequestManager<P, fusillade::ReqwestHttpClient>>,
    /// dwctl database pool — for querying connections, sync_operations, sync_entries.
    pub dwctl_pool: PgPool,
    /// Encryption key for decrypting connection credentials inside jobs.
    pub encryption_key: Option<Vec<u8>>,
    /// Reference to the IngestFileJob so SyncConnectionJob can enqueue it.
    pub ingest_file_job: JobRef<IngestFileInput, P>,
    /// Reference to the ActivateBatchJob so IngestFileJob can enqueue it.
    pub activate_batch_job: JobRef<ActivateBatchInput, P>,
    /// Reference to the CreateBatchInput job so ActivateBatchJob can enqueue populate.
    pub create_batch_job: JobRef<CreateBatchInput, P>,
}

impl<P: PoolProvider + Clone> TaskState<P> {
    /// Get the ingest file job, panics if not initialized.
    pub fn get_ingest_file_job(&self) -> &Arc<Job<IngestFileInput, TaskState<P>>> {
        self.ingest_file_job.get().expect("ingest_file_job not initialized")
    }

    /// Get the activate batch job, panics if not initialized.
    pub fn get_activate_batch_job(&self) -> &Arc<Job<ActivateBatchInput, TaskState<P>>> {
        self.activate_batch_job.get().expect("activate_batch_job not initialized")
    }

    /// Get the create batch job, panics if not initialized.
    pub fn get_create_batch_job(&self) -> &Arc<Job<CreateBatchInput, TaskState<P>>> {
        self.create_batch_job.get().expect("create_batch_job not initialized")
    }
}

/// Manages underway jobs and worker lifecycle.
///
/// Each job is stored as `Arc<Job<...>>` so the same instance is used for
/// both enqueueing (via cross-job references in TaskState) and running workers.
pub struct TaskRunner<P: PoolProvider + Clone + 'static = sqlx_pool_router::DbPools> {
    pub create_batch_job: Arc<Job<CreateBatchInput, TaskState<P>>>,
    pub sync_connection_job: Arc<Job<SyncConnectionInput, TaskState<P>>>,
    pub ingest_file_job: Arc<Job<IngestFileInput, TaskState<P>>>,
    pub activate_batch_job: Arc<Job<ActivateBatchInput, TaskState<P>>>,
}

impl<P: PoolProvider + Clone + Send + Sync + 'static> TaskRunner<P> {
    /// Build the task runner, registering all job types.
    ///
    /// All jobs share a single TaskState with `Arc<OnceLock>` cross-references
    /// that are set after construction, so every cloned copy sees the same jobs.
    pub async fn new(pool: PgPool, state: TaskState<P>) -> Result<Self> {
        // Build all jobs with the shared state. The OnceLock fields are empty
        // at this point, but all jobs hold Arc clones of the same OnceLocks.
        let create_batch_job = Arc::new(build_create_batch_job(pool.clone(), state.clone()).await?);
        let ingest_file_job = Arc::new(build_ingest_file_job(pool.clone(), state.clone()).await?);
        let activate_batch_job = Arc::new(build_activate_batch_job(pool.clone(), state.clone()).await?);
        let sync_connection_job = Arc::new(build_sync_connection_job(pool, state.clone()).await?);

        // Wire cross-references. Because all jobs share the same Arc<OnceLock>,
        // setting them here makes them visible to every cloned TaskState.
        if state.ingest_file_job.set(ingest_file_job.clone()).is_err() {
            panic!("ingest_file_job OnceLock already set — double initialization");
        }
        if state.activate_batch_job.set(activate_batch_job.clone()).is_err() {
            panic!("activate_batch_job OnceLock already set — double initialization");
        }
        if state.create_batch_job.set(create_batch_job.clone()).is_err() {
            panic!("create_batch_job OnceLock already set — double initialization");
        }

        Ok(Self {
            create_batch_job,
            sync_connection_job,
            ingest_file_job,
            activate_batch_job,
        })
    }

    /// Start the underway worker with the given shutdown token.
    pub fn start(&self, shutdown_token: CancellationToken) -> Vec<tokio::task::JoinHandle<()>> {
        let mut handles = Vec::new();

        let mut create_batch_worker = self.create_batch_job.worker();
        create_batch_worker.set_shutdown_token(shutdown_token.clone());
        handles.push(tokio::spawn(async move {
            if let Err(e) = create_batch_worker.run().await {
                tracing::error!(error = %e, "Create-batch worker error");
            }
        }));

        let mut sync_worker = self.sync_connection_job.worker();
        sync_worker.set_shutdown_token(shutdown_token.clone());
        handles.push(tokio::spawn(async move {
            if let Err(e) = sync_worker.run().await {
                tracing::error!(error = %e, "Sync-connection worker error");
            }
        }));

        let mut ingest_worker = self.ingest_file_job.worker();
        ingest_worker.set_shutdown_token(shutdown_token.clone());
        handles.push(tokio::spawn(async move {
            if let Err(e) = ingest_worker.run().await {
                tracing::error!(error = %e, "Ingest-file worker error");
            }
        }));

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
