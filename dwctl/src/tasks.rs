//! Background task processing via underway.
//!
//! This module provides a task runner that manages underway jobs for deferred
//! work. Job definitions live alongside their handlers (e.g. batch population
//! is defined in `api::handlers::batches`); this module wires them together.

use std::sync::{Arc, OnceLock, Weak};

use anyhow::Result;
use fusillade::PostgresRequestManager;
use sqlx::PgPool;
use sqlx_pool_router::PoolProvider;
use tokio_util::sync::CancellationToken;
use underway::Job;

use crate::api::handlers::batches::{CascadeBatchStateInput, CreateBatchInput, build_cascade_batch_state_job, build_create_batch_job};
use crate::connections::sync::{
    ActivateBatchInput, IngestFileInput, SyncConnectionInput, build_activate_batch_job, build_ingest_file_job, build_sync_connection_job,
};
use crate::responses::jobs::{CompleteResponseInput, CreateResponseInput, build_complete_response_job, build_create_response_job};

/// A lazily-initialized, shared job reference using `Weak` to avoid reference
/// cycles. Each `Job` owns a cloned `TaskState`, and `TaskState` holds `Weak`
/// references back to the jobs. The `TaskRunner` keeps the strong `Arc`s alive.
type WeakJobRef<I, P> = Arc<OnceLock<Weak<Job<I, TaskState<P>>>>>;

/// Shared state available to all task step closures.
///
/// Generic over pool provider so it works with both `DbPools` (production)
/// and `TestDbPools` (tests).
///
/// Cross-job references use `Weak` to break reference cycles: TaskRunner holds
/// strong `Arc<Job>` references, TaskState holds `Weak` references that are
/// upgraded when enqueueing.
#[derive(Clone)]
pub struct TaskState<P: PoolProvider + Clone = sqlx_pool_router::DbPools> {
    pub request_manager: Arc<PostgresRequestManager<P, fusillade::ReqwestHttpClient>>,
    /// dwctl database pool — for querying connections, sync_operations, sync_entries.
    pub dwctl_pool: PgPool,
    /// Shared config for capacity checks and other runtime configuration.
    pub config: crate::SharedConfig,
    /// Encryption key for decrypting connection credentials inside jobs.
    pub encryption_key: Option<Vec<u8>>,
    /// Weak reference to the IngestFileJob so SyncConnectionJob can enqueue it.
    pub ingest_file_job: WeakJobRef<IngestFileInput, P>,
    /// Weak reference to the ActivateBatchJob so IngestFileJob can enqueue it.
    pub activate_batch_job: WeakJobRef<ActivateBatchInput, P>,
    /// Weak reference to the CreateBatchInput job so ActivateBatchJob can enqueue populate.
    pub create_batch_job: WeakJobRef<CreateBatchInput, P>,
    /// Weak reference to the CascadeBatchState job so cancel/delete handlers can enqueue cleanup.
    pub cascade_batch_state_job: WeakJobRef<CascadeBatchStateInput, P>,
}

impl<P: PoolProvider + Clone> TaskState<P> {
    /// Get the ingest file job. Returns Err if not initialized or TaskRunner was dropped.
    pub fn get_ingest_file_job(&self) -> anyhow::Result<Arc<Job<IngestFileInput, TaskState<P>>>> {
        self.ingest_file_job
            .get()
            .ok_or_else(|| anyhow::anyhow!("ingest_file_job not initialized"))?
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("ingest_file_job dropped (TaskRunner gone)"))
    }

    /// Get the activate batch job.
    pub fn get_activate_batch_job(&self) -> anyhow::Result<Arc<Job<ActivateBatchInput, TaskState<P>>>> {
        self.activate_batch_job
            .get()
            .ok_or_else(|| anyhow::anyhow!("activate_batch_job not initialized"))?
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("activate_batch_job dropped (TaskRunner gone)"))
    }

    /// Get the create batch job.
    pub fn get_create_batch_job(&self) -> anyhow::Result<Arc<Job<CreateBatchInput, TaskState<P>>>> {
        self.create_batch_job
            .get()
            .ok_or_else(|| anyhow::anyhow!("create_batch_job not initialized"))?
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("create_batch_job dropped (TaskRunner gone)"))
    }

    /// Get the cascade batch state job.
    pub fn get_cascade_batch_state_job(&self) -> anyhow::Result<Arc<Job<CascadeBatchStateInput, TaskState<P>>>> {
        self.cascade_batch_state_job
            .get()
            .ok_or_else(|| anyhow::anyhow!("cascade_batch_state_job not initialized"))?
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("cascade_batch_state_job dropped (TaskRunner gone)"))
    }
}

/// Manages underway jobs and worker lifecycle.
///
/// Holds the strong `Arc<Job>` references that keep jobs alive. TaskState
/// holds `Weak` references back, so dropping TaskRunner cleans up properly.
pub struct TaskRunner<P: PoolProvider + Clone + 'static = sqlx_pool_router::DbPools> {
    pub create_batch_job: Arc<Job<CreateBatchInput, TaskState<P>>>,
    /// `None` when `cascade_batch_state_workers` is 0 (avoids opening
    /// PgListener connections during Job construction in test environments).
    pub cascade_batch_state_job: Option<Arc<Job<CascadeBatchStateInput, TaskState<P>>>>,
    pub sync_connection_job: Arc<Job<SyncConnectionInput, TaskState<P>>>,
    pub ingest_file_job: Arc<Job<IngestFileInput, TaskState<P>>>,
    pub activate_batch_job: Arc<Job<ActivateBatchInput, TaskState<P>>>,
    pub create_response_job: Arc<Job<CreateResponseInput, TaskState<P>>>,
    pub complete_response_job: Arc<Job<CompleteResponseInput, TaskState<P>>>,
}

impl<P: PoolProvider + Clone + Send + Sync + 'static> TaskRunner<P> {
    /// Build the task runner, registering all job types.
    ///
    /// All jobs share a single TaskState with `Weak` cross-references that are
    /// set after construction, breaking the reference cycle.
    pub async fn new(pool: PgPool, state: TaskState<P>, task_config: &crate::config::TaskWorkersConfig) -> Result<Self> {
        let create_batch_job = Arc::new(build_create_batch_job(pool.clone(), state.clone()).await?);
        let cascade_batch_state_job = if task_config.cascade_batch_state_workers > 0 {
            let job = Arc::new(build_cascade_batch_state_job(pool.clone(), state.clone()).await?);
            state
                .cascade_batch_state_job
                .set(Arc::downgrade(&job))
                .map_err(|_| anyhow::anyhow!("cascade_batch_state_job OnceLock already set"))?;
            Some(job)
        } else {
            None
        };
        let ingest_file_job = Arc::new(build_ingest_file_job(pool.clone(), state.clone()).await?);
        let activate_batch_job = Arc::new(build_activate_batch_job(pool.clone(), state.clone()).await?);
        let create_response_job = Arc::new(build_create_response_job(pool.clone(), state.clone()).await?);
        let complete_response_job = Arc::new(build_complete_response_job(pool.clone(), state.clone()).await?);
        let sync_connection_job = Arc::new(build_sync_connection_job(pool, state.clone()).await?);

        // Wire weak cross-references. All jobs share the same Arc<OnceLock>,
        // so setting them here makes them visible to every cloned TaskState.
        state
            .ingest_file_job
            .set(Arc::downgrade(&ingest_file_job))
            .map_err(|_| anyhow::anyhow!("ingest_file_job OnceLock already set — double initialization"))?;
        state
            .activate_batch_job
            .set(Arc::downgrade(&activate_batch_job))
            .map_err(|_| anyhow::anyhow!("activate_batch_job OnceLock already set — double initialization"))?;
        state
            .create_batch_job
            .set(Arc::downgrade(&create_batch_job))
            .map_err(|_| anyhow::anyhow!("create_batch_job OnceLock already set — double initialization"))?;

        Ok(Self {
            create_batch_job,
            cascade_batch_state_job,
            sync_connection_job,
            ingest_file_job,
            activate_batch_job,
            create_response_job,
            complete_response_job,
        })
    }

    /// Start the underway workers with the given shutdown token and config.
    ///
    /// Task workers (create-batch, cascade-batch-state) always run.
    /// Sync workers (discovery, ingest, activate) are gated by `sync_config.enabled`.
    pub fn start(
        &self,
        shutdown_token: CancellationToken,
        task_config: &crate::config::TaskWorkersConfig,
        sync_config: &crate::config::SyncWorkersConfig,
    ) -> Vec<(&'static str, tokio::task::JoinHandle<()>)> {
        let mut handles: Vec<(&'static str, tokio::task::JoinHandle<()>)> = Vec::new();

        // Batch creation workers — handles both API-triggered and
        // sync-triggered batch population. Always at least 1: without a
        // worker, enqueued batch populations hang indefinitely.
        let create_batch_workers = task_config.create_batch_workers.max(1);
        for i in 0..create_batch_workers {
            let mut worker = self.create_batch_job.worker();
            worker.set_shutdown_token(shutdown_token.clone());
            handles.push((
                "create-batch-worker",
                tokio::spawn(async move {
                    if let Err(e) = worker.run().await {
                        tracing::error!(error = %e, worker = i, "Create-batch worker error");
                    }
                }),
            ));
        }

        // Cascade-batch-state workers — updates child request states after
        // a batch is cancelled/deleted.
        if let Some(ref job) = self.cascade_batch_state_job {
            for i in 0..task_config.cascade_batch_state_workers {
                let mut worker = job.worker();
                worker.set_shutdown_token(shutdown_token.clone());
                handles.push((
                    "cascade-batch-state-worker",
                    tokio::spawn(async move {
                        if let Err(e) = worker.run().await {
                            tracing::error!(error = %e, worker = i, "Cascade-batch-state worker error");
                        }
                    }),
                ));
            }
        }

        // Response lifecycle workers — handle create-response and complete-response jobs
        // from the responses middleware and outlet handler.
        if task_config.response_workers > 0 {
            for _ in 0..task_config.response_workers {
                let mut worker = self.create_response_job.worker();
                worker.set_shutdown_token(shutdown_token.clone());
                handles.push((
                    "create-response-worker",
                    tokio::spawn(async move {
                        if let Err(e) = worker.run().await {
                            tracing::error!(error = %e, "Create-response worker error");
                        }
                    }),
                ));
            }
            for _ in 0..task_config.response_workers {
                let mut worker = self.complete_response_job.worker();
                worker.set_shutdown_token(shutdown_token.clone());
                handles.push((
                    "complete-response-worker",
                    tokio::spawn(async move {
                        if let Err(e) = worker.run().await {
                            tracing::error!(error = %e, "Complete-response worker error");
                        }
                    }),
                ));
            }
        }

        if !sync_config.enabled {
            tracing::info!("Sync workers disabled on this instance");
            return handles;
        }

        // Sync discovery workers (0 = disabled)
        for i in 0..sync_config.discovery_workers {
            let mut worker = self.sync_connection_job.worker();
            worker.set_shutdown_token(shutdown_token.clone());
            handles.push((
                "sync-discovery-worker",
                tokio::spawn(async move {
                    if let Err(e) = worker.run().await {
                        tracing::error!(error = %e, worker = i, "Sync-connection worker error");
                    }
                }),
            ));
        }

        // File ingestion workers (0 = disabled)
        for i in 0..sync_config.ingest_workers {
            let mut worker = self.ingest_file_job.worker();
            worker.set_shutdown_token(shutdown_token.clone());
            handles.push((
                "ingest-file-worker",
                tokio::spawn(async move {
                    if let Err(e) = worker.run().await {
                        tracing::error!(error = %e, worker = i, "Ingest-file worker error");
                    }
                }),
            ));
        }

        // Batch activation workers (0 = disabled)
        for i in 0..sync_config.activate_workers {
            let mut worker = self.activate_batch_job.worker();
            worker.set_shutdown_token(shutdown_token.clone());
            handles.push((
                "activate-batch-worker",
                tokio::spawn(async move {
                    if let Err(e) = worker.run().await {
                        tracing::error!(error = %e, worker = i, "Activate-batch worker error");
                    }
                }),
            ));
        }

        tracing::info!(
            discovery_workers = sync_config.discovery_workers,
            ingest_workers = sync_config.ingest_workers,
            activate_workers = sync_config.activate_workers,
            "Sync workers started"
        );

        handles
    }
}
