use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::daemon::{Daemon, DaemonConfig, DaemonMode};
use crate::http::{HttpClient, ReqwestHttpClient};
use crate::processor::RequestProcessor;

type PostgresStore<P> = fusillade_arsenal::PostgresRequestManager<P>;

/// PostgreSQL-backed scheduling runtime.
///
/// Database access lives on [`fusillade_arsenal::PostgresRequestManager`].
/// This type only composes that store with an HTTP client and daemon config
/// so the root crate remains focused on scheduling work.
pub struct PostgresDaemon<P, H = ReqwestHttpClient>
where
    P: fusillade_arsenal::PoolProvider,
    H: HttpClient,
{
    storage: Arc<PostgresStore<P>>,
    http_client: Arc<H>,
    config: DaemonConfig,
    processor: OnceLock<Arc<dyn RequestProcessor<PostgresStore<P>, H>>>,
}

impl From<&DaemonConfig> for fusillade_arsenal::PostgresStorageConfig {
    fn from(config: &DaemonConfig) -> Self {
        Self {
            pending_request_counts_timeout_ms: config.pending_request_counts_timeout_ms,
            max_concurrent_state_writes: config.max_concurrent_state_writes,
            batch_metadata_fields: config.batch_metadata_fields.clone(),
            claim_timeout_ms: config.claim_timeout_ms,
            processing_timeout_ms: config.processing_timeout_ms,
            stale_daemon_threshold_ms: config.stale_daemon_threshold_ms,
            unclaim_batch_size: config.unclaim_batch_size,
            service_tier_completion_windows_ms: config.service_tier_completion_windows_ms.clone(),
            default_completion_window_ms: config.default_completion_window_ms,
            claim_ramp_exponent: config.claim_ramp_exponent,
            urgency_weight: config.urgency_weight,
            batch_claim_require_live: config.batch_claim_require_live,
            leaks_per_window: config.leaks_per_window,
            model_filters_keep_per_model: config.model_filters_keep_per_model,
            model_filters_retention_ms: config.model_filters_retention_ms,
        }
    }
}

impl<P> PostgresDaemon<P, ReqwestHttpClient>
where
    P: fusillade_arsenal::PoolProvider,
{
    /// Build a PostgreSQL daemon runtime from an existing Arsenal store and
    /// the default Reqwest HTTP client.
    pub fn from_store(storage: Arc<PostgresStore<P>>, config: DaemonConfig) -> Self {
        let http_client = Arc::new(
            ReqwestHttpClient::new(
                Duration::from_millis(config.first_chunk_timeout_ms),
                Duration::from_millis(config.chunk_timeout_ms),
                Duration::from_millis(config.body_timeout_ms),
                config.streamable_endpoints.clone(),
            )
            .with_upload_stall_timeout(Duration::from_millis(config.upload_stall_timeout_ms))
            .with_upload_chunk_bytes(config.upload_chunk_bytes)
            .with_upload_stall_poll_interval(Duration::from_millis(config.upload_stall_poll_ms)),
        );
        Self::new(storage, http_client, config)
    }

    /// Build a PostgreSQL daemon runtime from pools and daemon config.
    ///
    /// Use [`PostgresDaemon::new`] when the store needs custom Arsenal
    /// configuration, such as database retry cadence.
    pub fn from_pools(pools: P, config: DaemonConfig) -> Self {
        let storage = Arc::new(PostgresStore::new(pools, (&config).into()));
        Self::from_store(storage, config)
    }
}

impl<P, H> PostgresDaemon<P, H>
where
    P: fusillade_arsenal::PoolProvider,
    H: HttpClient + 'static,
{
    pub fn new(storage: Arc<PostgresStore<P>>, http_client: Arc<H>, config: DaemonConfig) -> Self {
        Self {
            storage,
            http_client,
            config,
            processor: OnceLock::new(),
        }
    }

    pub fn with_processor(self, processor: Arc<dyn RequestProcessor<PostgresStore<P>, H>>) -> Self {
        let _ = self.processor.set(processor);
        self
    }

    pub fn set_processor(
        &self,
        processor: Arc<dyn RequestProcessor<PostgresStore<P>, H>>,
    ) -> std::result::Result<(), &'static str> {
        self.processor
            .set(processor)
            .map_err(|_| "processor already set")
    }

    pub fn set_response_transformer(
        &self,
        transformer: Arc<dyn crate::transform::ResponseTransformer>,
    ) -> std::result::Result<(), &'static str> {
        self.storage
            .set_response_transformer(Arc::new(ResponseTransformerAdapter(transformer)))
    }

    pub fn storage(&self) -> &Arc<PostgresStore<P>> {
        &self.storage
    }

    pub fn http_client(&self) -> &Arc<H> {
        &self.http_client
    }

    pub fn config(&self) -> &DaemonConfig {
        &self.config
    }

    pub fn processor(&self) -> Option<&Arc<dyn RequestProcessor<PostgresStore<P>, H>>> {
        self.processor.get()
    }

    pub fn run(
        self: Arc<Self>,
        shutdown_token: CancellationToken,
    ) -> crate::Result<JoinHandle<crate::Result<()>>> {
        let mode = self.config.mode;
        self.run_with_mode(shutdown_token, mode)
    }

    pub fn run_with_mode(
        self: Arc<Self>,
        shutdown_token: CancellationToken,
        mode: DaemonMode,
    ) -> crate::Result<JoinHandle<crate::Result<()>>> {
        tracing::info!(?mode, "Starting PostgreSQL scheduling daemon");

        let mut daemon = Daemon::new(
            self.storage.clone(),
            self.http_client.clone(),
            self.config.clone(),
            shutdown_token,
        );
        if let Some(processor) = self.processor.get().cloned() {
            daemon = daemon.with_processor(processor);
        }
        let daemon = Arc::new(daemon);

        let handle = tokio::spawn(async move { daemon.run_with_mode(mode).await });

        tracing::info!("PostgreSQL scheduling daemon spawned successfully");

        Ok(handle)
    }
}

#[async_trait]
impl<P, H> super::DaemonExecutor<H> for PostgresDaemon<P, H>
where
    P: fusillade_arsenal::PoolProvider,
    H: HttpClient + 'static,
{
    fn http_client(&self) -> &Arc<H> {
        self.http_client()
    }

    fn config(&self) -> &DaemonConfig {
        self.config()
    }

    fn run_with_mode(
        self: Arc<Self>,
        shutdown_token: CancellationToken,
        mode: DaemonMode,
    ) -> crate::Result<JoinHandle<crate::Result<()>>> {
        PostgresDaemon::run_with_mode(self, shutdown_token, mode)
    }
}

struct ResponseTransformerAdapter(Arc<dyn crate::transform::ResponseTransformer>);

#[async_trait]
impl fusillade_arsenal::ResponseTransformer for ResponseTransformerAdapter {
    async fn transform(
        &self,
        request: &crate::request::RequestData,
        body: &str,
    ) -> crate::Result<String> {
        self.0.transform(request, body).await
    }
}
