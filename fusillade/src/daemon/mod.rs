//! Daemon for processing batched requests with per-model concurrency control.
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

pub mod config;

use futures::FutureExt;
use metrics::{counter, gauge, histogram};
use tokio::task::JoinSet;

use opentelemetry::trace::TraceContextExt;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::FusilladeError;
use crate::batch::BatchId;
use crate::error::Result;
use crate::http::HttpClient;
use crate::manager::{ArchiveOutcome, DaemonStorage, Storage};
use crate::processor::{DefaultRequestProcessor, RequestProcessor};
use crate::request::{
    AnyRequest, AttemptId, Claimed, DaemonId, Failed, FailureReason, Request,
    RequestCompletionResult, RequestData, RequestId, RequestState,
};

pub use config::{
    DaemonConfig, DaemonMode, ModelEscalationConfig, ShouldRetryFn, default_should_retry,
};
pub use fusillade_core::daemon_record::{
    AnyDaemonRecord, DaemonData, DaemonRecord, DaemonState, DaemonStats, DaemonStatus, Dead,
    Initializing, Running,
};

/// Per-user throughput counters, reset after each emission cycle.
struct UserThroughputStats {
    completed: AtomicU64,
    failed: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimLoopKind {
    Request,
    Batch,
}

/// Backoff before retrying a failed claim cycle: exponential in the number of
/// consecutive failures, based on the claim interval, capped at 30s.
fn claim_failure_backoff(consecutive_failures: u32, claim_interval_ms: u64) -> Duration {
    const MAX_BACKOFF_MS: u64 = 30_000;
    let factor = 2u64.saturating_pow(consecutive_failures.min(16));
    Duration::from_millis(
        claim_interval_ms
            .max(100)
            .saturating_mul(factor)
            .min(MAX_BACKOFF_MS),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptWriteResolution {
    Applied,
    LostOwnership,
    RequestMissing,
}

enum TerminalPersistenceResolution {
    OriginalApplied,
    FallbackApplied(Box<Request<Failed>>),
    LostOwnership,
    RequestMissing,
}

fn classify_attempt_write_result(result: Result<bool>) -> Result<AttemptWriteResolution> {
    match result {
        Ok(true) => Ok(AttemptWriteResolution::Applied),
        Ok(false)
        | Err(FusilladeError::RequestAttemptLost { .. })
        | Err(FusilladeError::RequestStateConflict { .. }) => {
            Ok(AttemptWriteResolution::LostOwnership)
        }
        Err(FusilladeError::RequestNotFound(_)) => Ok(AttemptWriteResolution::RequestMissing),
        Err(error) => Err(error),
    }
}

fn attempt_write_backoff(failures: u32) -> Duration {
    Duration::from_millis(
        100u64
            .saturating_mul(2u64.saturating_pow(failures.saturating_sub(1).min(6)))
            .min(5_000),
    )
}

async fn attempt_write_until_resolved<F, Fut>(
    shutdown: &tokio_util::sync::CancellationToken,
    request_id: RequestId,
    attempt_id: AttemptId,
    operation: &'static str,
    mut write: F,
) -> Result<AttemptWriteResolution>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<bool>>,
{
    let mut failures = 0u32;

    loop {
        let result = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Err(FusilladeError::Shutdown),
            result = write() => result,
        };

        match result {
            Err(FusilladeError::AttemptPersistenceInfrastructure { .. }) => {
                failures = failures.saturating_add(1);
                let delay = attempt_write_backoff(failures);
                counter!(
                    "fusillade_request_attempt_write_retries_total",
                    "operation" => operation
                )
                .increment(1);

                if failures.is_power_of_two() {
                    tracing::warn!(
                        %request_id,
                        %attempt_id,
                        operation,
                        failures,
                        retry_after_ms = delay.as_millis(),
                        "Request attempt persistence unavailable; retrying"
                    );
                } else {
                    tracing::debug!(
                        %request_id,
                        %attempt_id,
                        operation,
                        failures,
                        retry_after_ms = delay.as_millis(),
                        "Request attempt persistence still unavailable"
                    );
                }

                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => return Err(FusilladeError::Shutdown),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
            result => return classify_attempt_write_result(result),
        }
    }
}

fn terminal_persistence_fallback(
    data: RequestData,
    retry_attempt: u32,
    batch_expires_at: chrono::DateTime<chrono::Utc>,
    routed_model: String,
) -> Request<Failed> {
    Request {
        data,
        state: Failed {
            reason: FailureReason::RequestBuilderError {
                error: "Request outcome could not be stored".to_string(),
            },
            failed_at: chrono::Utc::now(),
            retry_attempt,
            batch_expires_at,
            routed_model,
        },
    }
}

async fn persist_terminal_attempt<S, T>(
    storage: &S,
    shutdown: &tokio_util::sync::CancellationToken,
    request: &Request<T>,
    fallback: Request<Failed>,
    attempt_id: AttemptId,
    operation: &'static str,
) -> Result<TerminalPersistenceResolution>
where
    S: Storage + ?Sized,
    T: RequestState + Clone,
    AnyRequest: From<Request<T>>,
{
    let request_id = request.data.id;
    match attempt_write_until_resolved(shutdown, request_id, attempt_id, operation, || {
        Storage::persist_attempt::<T>(storage, request, attempt_id)
    })
    .await
    {
        Ok(AttemptWriteResolution::Applied) => Ok(TerminalPersistenceResolution::OriginalApplied),
        Ok(AttemptWriteResolution::LostOwnership) => {
            Ok(TerminalPersistenceResolution::LostOwnership)
        }
        Ok(AttemptWriteResolution::RequestMissing) => {
            Ok(TerminalPersistenceResolution::RequestMissing)
        }
        Err(FusilladeError::Shutdown) => Err(FusilladeError::Shutdown),
        Err(_) => {
            counter!(
                "fusillade_request_terminal_persistence_fallback_total",
                "operation" => operation
            )
            .increment(1);
            tracing::warn!(
                %request_id,
                %attempt_id,
                operation,
                "Terminal outcome could not be prepared; storing redacted failure"
            );

            match attempt_write_until_resolved(
                shutdown,
                request_id,
                attempt_id,
                "terminal_fallback",
                || Storage::persist_attempt::<Failed>(storage, &fallback, attempt_id),
            )
            .await?
            {
                AttemptWriteResolution::Applied => Ok(
                    TerminalPersistenceResolution::FallbackApplied(Box::new(fallback)),
                ),
                AttemptWriteResolution::LostOwnership => {
                    Ok(TerminalPersistenceResolution::LostOwnership)
                }
                AttemptWriteResolution::RequestMissing => {
                    Ok(TerminalPersistenceResolution::RequestMissing)
                }
            }
        }
    }
}

struct TerminalFailureContext<'a> {
    request_id: RequestId,
    batch_id: Option<BatchId>,
    model: &'a str,
    user_id: &'a str,
    completion_window: &'a str,
    processing_start: std::time::Instant,
    batch_expires_at: chrono::DateTime<chrono::Utc>,
    requests_failed: &'a AtomicU64,
    user_throughput: &'a dashmap::DashMap<String, UserThroughputStats>,
}

fn record_terminal_failure(failed: &Request<Failed>, context: TerminalFailureContext<'_>) {
    context.requests_failed.fetch_add(1, Ordering::Relaxed);
    context
        .user_throughput
        .entry(context.user_id.to_string())
        .or_insert_with(|| UserThroughputStats {
            completed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        })
        .failed
        .fetch_add(1, Ordering::Relaxed);
    counter!(
        "fusillade_requests_completed_total",
        "model" => context.model.to_string(),
        "status" => "failed",
        "reason" => failed.state.reason.metric_label(),
        "status_code" => failed.state.reason.status_code_label(),
        "completion_window" => context.completion_window.to_string()
    )
    .increment(1);
    counter!(
        "fusillade_user_requests_completed_total",
        "user" => context.user_id.to_string(),
        "status" => "failed",
        "completion_window" => context.completion_window.to_string()
    )
    .increment(1);
    histogram!(
        "fusillade_request_duration_seconds",
        "model" => context.model.to_string(),
        "status" => "failed"
    )
    .record(context.processing_start.elapsed().as_secs_f64());

    if failed.state.failed_at > context.batch_expires_at {
        counter!(
            "fusillade_requests_completed_after_sla_total",
            "model" => context.model.to_string(),
            "status" => "failed",
            "completion_window" => context.completion_window.to_string()
        )
        .increment(1);
        tracing::warn!(
            request_id = %context.request_id,
            batch_id = ?context.batch_id,
            "Request failed permanently after SLA"
        );
    }
    tracing::warn!(
        request_id = %context.request_id,
        batch_id = ?context.batch_id,
        retry_attempt = failed.state.retry_attempt,
        failure_reason = failed.state.reason.metric_label(),
        "request.terminal_failure"
    );
}

fn claim_loop_kinds_for_mode(
    mode: DaemonMode,
    supports_batch_claims: bool,
) -> Result<Vec<ClaimLoopKind>> {
    match mode {
        DaemonMode::Both => {
            if supports_batch_claims {
                Ok(vec![ClaimLoopKind::Request, ClaimLoopKind::Batch])
            } else {
                Ok(vec![ClaimLoopKind::Request])
            }
        }
        DaemonMode::RequestOnly => Ok(vec![ClaimLoopKind::Request]),
        DaemonMode::BatchOnly => {
            if supports_batch_claims {
                Ok(vec![ClaimLoopKind::Batch])
            } else {
                Err(FusilladeError::Other(anyhow::anyhow!(
                    "batch-only daemon mode requires storage that supports batch claims"
                )))
            }
        }
    }
}

fn reclaim_loop_kind_for_mode(mode: DaemonMode) -> ClaimLoopKind {
    match mode {
        DaemonMode::Both | DaemonMode::RequestOnly => ClaimLoopKind::Request,
        DaemonMode::BatchOnly => ClaimLoopKind::Batch,
    }
}

async fn prepare_claim_capacity<T, Maintenance, Capacity>(
    run_reclaim: bool,
    maintenance: Maintenance,
    capacity: Capacity,
) -> Result<(usize, Option<T>)>
where
    Maintenance: Future<Output = Result<usize>>,
    Capacity: FnOnce() -> Option<T>,
{
    let reclaimed = if run_reclaim { maintenance.await? } else { 0 };
    Ok((reclaimed, capacity()))
}

fn get_hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string())
}

fn get_pid() -> i32 {
    std::process::id() as i32
}

fn get_version() -> String {
    option_env!("GIT_HASH")
        .or(option_env!("CARGO_PKG_VERSION"))
        .unwrap_or("dev")
        .to_string()
}

/// Bound database futures so a silently severed connection surfaces as an
/// error instead of freezing the daemon task until TCP keepalive.
async fn with_query_timeout<T>(
    what: &'static str,
    timeout: Duration,
    fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(timeout, fut).await {
        Ok(result) => result,
        Err(_) => Err(FusilladeError::Other(anyhow::anyhow!(
            "{what} timed out after {}ms; dropping the in-flight DB connection to avoid hanging",
            timeout.as_millis()
        ))),
    }
}

/// Daemon responsible for batchless pending requests.
///
/// This loop owns the leaky-bucket/deadline-ramp policy for async/flex rows.
pub struct RequestDaemon<S, H>
where
    S: Storage + DaemonStorage,
    H: HttpClient,
{
    core: Arc<Daemon<S, H>>,
}

impl<S, H> RequestDaemon<S, H>
where
    S: Storage + DaemonStorage + 'static,
    H: HttpClient + 'static,
{
    fn new(core: Arc<Daemon<S, H>>) -> Self {
        Self { core }
    }

    async fn run(self, run_reclaim: bool) -> Result<()> {
        self.core
            .run_claim_loop(ClaimLoopKind::Request, run_reclaim)
            .await
    }
}

/// Daemon responsible for live-model batch requests.
///
/// This loop selects batches first, then claims rows from those batches. It does
/// not use the request daemon's leaky-bucket fallback.
pub struct BatchDaemon<S, H>
where
    S: Storage + DaemonStorage,
    H: HttpClient,
{
    core: Arc<Daemon<S, H>>,
}

impl<S, H> BatchDaemon<S, H>
where
    S: Storage + DaemonStorage + 'static,
    H: HttpClient + 'static,
{
    fn new(core: Arc<Daemon<S, H>>) -> Self {
        Self { core }
    }

    async fn run(self, run_reclaim: bool) -> Result<()> {
        self.core
            .run_claim_loop(ClaimLoopKind::Batch, run_reclaim)
            .await
    }
}

/// Daemon that processes batched requests.
///
/// The daemon continuously claims pending requests from storage, enforces
/// per-model concurrency limits, and dispatches requests for execution.
pub struct Daemon<S, H>
where
    S: Storage + DaemonStorage,
    H: HttpClient,
{
    daemon_id: DaemonId,
    storage: Arc<S>,
    http_client: Arc<H>,
    config: DaemonConfig,
    /// Per-claim processing hook. Defaults to [`DefaultRequestProcessor`],
    /// which preserves the existing fire-and-store pipeline byte-for-byte.
    /// Override via [`Daemon::with_processor`] to inject custom orchestration
    /// (e.g. multi-step Open Responses loops) without changing any other
    /// daemon behavior.
    processor: Arc<dyn RequestProcessor<S, H>>,
    requests_in_flight: Arc<dashmap::DashMap<String, AtomicUsize>>,
    /// Per-user in-flight request counts across all models, used to prioritise
    /// users with fewer active requests during claim (per-user fair scheduling).
    user_requests_in_flight: Arc<dashmap::DashMap<String, AtomicUsize>>,
    /// Per-`(user, window-class, model)` leaky-bucket state for not-live models.
    /// Each entry's value is `next_token_at`: the earliest `Instant` the bucket
    /// may leak its next request. Before a claim cycle the daemon derives the
    /// cooldown set (triples with `next_token_at > now`) and passes it to
    /// `claim_requests`; after a claim it stamps `next_token_at = now + W /
    /// leaks_per_window` for each leaked row's triple. Stale entries are pruned on
    /// read to bound the map. See `leaks_per_window`.
    leak_buckets: Arc<dashmap::DashMap<(String, String, String), std::time::Instant>>,
    /// Per-user throughput counters for periodic OTel emission.
    user_throughput: Arc<dashmap::DashMap<String, UserThroughputStats>>,
    /// Serializes independent claim loops while they compute available
    /// capacity, claim rows, and increment in-flight counters.
    claim_mutex: Arc<tokio::sync::Mutex<()>>,
    requests_processed: Arc<AtomicU64>,
    requests_failed: Arc<AtomicU64>,
    shutdown_token: tokio_util::sync::CancellationToken,
    /// Map of batch_id -> cancellation token for batch-level cancellation
    /// All requests in a batch share the same cancellation token
    cancellation_tokens: Arc<dashmap::DashMap<BatchId, tokio_util::sync::CancellationToken>>,
}

impl<S, H> Daemon<S, H>
where
    S: Storage + DaemonStorage + 'static,
    H: HttpClient + 'static,
{
    /// Create a new daemon.
    ///
    /// Uses [`DefaultRequestProcessor`] for per-claim processing, preserving
    /// today's pipeline behavior. To inject custom orchestration, chain
    /// [`Daemon::with_processor`] after this.
    pub fn new(
        storage: Arc<S>,
        http_client: Arc<H>,
        config: DaemonConfig,
        shutdown_token: tokio_util::sync::CancellationToken,
    ) -> Self {
        let should_retry = config.retry_predicate();
        let config = DaemonConfig {
            should_retry,
            ..config
        };

        Self {
            daemon_id: DaemonId::from(uuid::Uuid::new_v4()),
            storage,
            http_client,
            config,
            processor: Arc::new(DefaultRequestProcessor),
            requests_in_flight: Arc::new(dashmap::DashMap::new()),
            user_requests_in_flight: Arc::new(dashmap::DashMap::new()),
            leak_buckets: Arc::new(dashmap::DashMap::new()),
            user_throughput: Arc::new(dashmap::DashMap::new()),
            claim_mutex: Arc::new(tokio::sync::Mutex::new(())),
            requests_processed: Arc::new(AtomicU64::new(0)),
            requests_failed: Arc::new(AtomicU64::new(0)),
            shutdown_token,
            cancellation_tokens: Arc::new(dashmap::DashMap::new()),
        }
    }

    /// Override the per-claim processing hook.
    ///
    /// Returns `self` for chained construction:
    ///
    /// ```ignore
    /// let daemon = Daemon::new(storage, http, config, shutdown)
    ///     .with_processor(Arc::new(my_custom_processor));
    /// ```
    ///
    /// The provided processor is invoked once per claimed request in place
    /// of the default fire-and-store path. The daemon continues to own
    /// metrics, cancellation token plumbing, retry persistence, and the
    /// outer processing span.
    pub fn with_processor(mut self, processor: Arc<dyn RequestProcessor<S, H>>) -> Self {
        self.processor = processor;
        self
    }

    fn poll_processing_tasks(join_set: &mut JoinSet<Result<()>>) {
        while let Some(result) = join_set.try_join_next() {
            match result {
                Ok(Ok(())) => {
                    tracing::trace!("Task completed successfully");
                }
                Ok(Err(e)) => {
                    crate::background_error!("task_failed", Error, error = %e, "Task failed");
                }
                Err(join_error) => {
                    crate::background_error!("task_panicked", Critical, error = %join_error, "Task panicked");
                }
            }
        }
    }

    fn available_capacity(&self) -> HashMap<String, usize> {
        self.config
            .model_concurrency_limits
            .iter()
            .filter_map(|entry| {
                let model = entry.key().clone();
                let limit = *entry.value();
                let in_flight = self
                    .requests_in_flight
                    .get(&model)
                    .map(|e| e.value().load(Ordering::Relaxed))
                    .unwrap_or(0);
                let available = limit.saturating_sub(in_flight);
                if available > 0 {
                    Some((model, available))
                } else {
                    None
                }
            })
            .collect()
    }

    fn user_active_counts(&self) -> HashMap<String, usize> {
        self.user_requests_in_flight
            .iter()
            .filter_map(|entry| {
                let count = entry.value().load(Ordering::Relaxed);
                if count > 0 {
                    Some((entry.key().clone(), count))
                } else {
                    None
                }
            })
            .collect()
    }

    fn leak_cooldown(&self) -> std::collections::HashSet<(String, String, String)> {
        let cooldown_now = std::time::Instant::now();
        let mut refilled_buckets: Vec<(String, String, String)> = Vec::new();
        let leak_cooldown: std::collections::HashSet<(String, String, String)> = self
            .leak_buckets
            .iter()
            .filter_map(|entry| {
                if *entry.value() > cooldown_now {
                    Some(entry.key().clone())
                } else {
                    refilled_buckets.push(entry.key().clone());
                    None
                }
            })
            .collect();

        for key in refilled_buckets {
            self.leak_buckets
                .remove_if(&key, |_, next_token_at| *next_token_at <= cooldown_now);
        }

        leak_cooldown
    }

    fn stamp_leaks(&self, claimed: &[Request<Claimed>]) {
        let stamp_now = std::time::Instant::now();
        let leaks_per_window = self.config.leaks_per_window.max(f64::MIN_POSITIVE);
        let mut leaked_count = 0u64;
        for request in claimed {
            if let Some(stamp) = &request.state.leak {
                let interval = std::time::Duration::from_secs_f64(
                    (stamp.window_secs / leaks_per_window).max(0.0),
                );
                let key = (
                    request.data.created_by.clone(),
                    stamp.window_class.clone(),
                    request.data.model.clone(),
                );
                self.leak_buckets.insert(key, stamp_now + interval);
                leaked_count += 1;
            }
        }

        if leaked_count > 0 {
            counter!("fusillade_leaky_bucket_leaks_total").increment(leaked_count);
            tracing::debug!(
                leaked_count,
                "Stamped leaky-bucket tokens for leaked claims"
            );
        }
    }

    async fn run_claim_loop(self: Arc<Self>, kind: ClaimLoopKind, run_reclaim: bool) -> Result<()> {
        let mut join_set: JoinSet<Result<()>> = JoinSet::new();
        let (loop_name, interval_ms) = match kind {
            ClaimLoopKind::Request => ("request_daemon", self.config.claim_interval_ms),
            ClaimLoopKind::Batch => (
                "batch_daemon",
                if self.config.batch_claim_interval_ms == 0 {
                    self.config.claim_interval_ms
                } else {
                    self.config.batch_claim_interval_ms
                },
            ),
        };

        tracing::info!(
            daemon_id = %self.daemon_id,
            loop_name,
            interval_ms,
            "Claim loop started"
        );

        let mut consecutive_claim_failures: u32 = 0;

        loop {
            if self.shutdown_token.is_cancelled() {
                tracing::info!(loop_name, "Shutdown signal received, stopping claim loop");
                break Ok(());
            }

            Self::poll_processing_tasks(&mut join_set);

            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(interval_ms)) => {},
                _ = self.shutdown_token.cancelled() => {
                    tracing::info!(loop_name, "Shutdown signal received, stopping claim loop");
                    break Ok(());
                }
            }

            // Observe shutdown while waiting for the claim mutex — otherwise a
            // loop blocked behind the other daemon's claim would run one more
            // full cycle after shutdown is requested.
            let _claim_guard = tokio::select! {
                guard = self.claim_mutex.lock() => guard,
                _ = self.shutdown_token.cancelled() => {
                    tracing::info!(loop_name, "Shutdown signal received, stopping claim loop");
                    break Ok(());
                }
            };
            let claim_timeout = Duration::from_millis(self.config.claim_query_timeout_ms);
            let claim_result: Result<Option<(Vec<Request<Claimed>>, Duration)>> = tokio::select! {
                result = async {
                    let (reclaimed, available_capacity) = prepare_claim_capacity(
                        run_reclaim,
                        with_query_timeout(
                            "stale request reclaim query",
                            claim_timeout,
                            self.storage.reclaim_stale_requests(),
                        ),
                        || {
                            let capacity = self.available_capacity();
                            (!capacity.is_empty()).then_some(capacity)
                        },
                    )
                    .await?;

                    if reclaimed > 0 {
                        tracing::info!(
                            loop_name,
                            reclaimed,
                            "Reclaimed safely abandoned requests before claiming"
                        );
                    }

                    let Some(available_capacity) = available_capacity else {
                        tracing::trace!(
                            loop_name,
                            "No capacity available for any model, skipping claim"
                        );
                        return Ok(None);
                    };

                    let total_capacity: usize = available_capacity.values().sum();
                    // Dual-emit: keep the legacy unlabeled series alive alongside the
                    // new per-daemon one so existing dashboards/alerts survive the
                    // split (deprecation window).
                    gauge!("fusillade_claim_capacity").set(total_capacity as f64);
                    gauge!("fusillade_claim_capacity", "daemon" => loop_name)
                        .set(total_capacity as f64);

                    let user_active_counts = self.user_active_counts();
                    let leak_cooldown = if kind == ClaimLoopKind::Request {
                        self.leak_cooldown()
                    } else {
                        std::collections::HashSet::new()
                    };

                    let claim_start = std::time::Instant::now();
                    let claimed = match kind {
                        ClaimLoopKind::Request => {
                            with_query_timeout(
                                "batchless claim query",
                                claim_timeout,
                                self.storage.claim_batchless_requests(
                                    self.config.claim_batch_size,
                                    self.daemon_id,
                                    &available_capacity,
                                    &user_active_counts,
                                    &leak_cooldown,
                                ),
                            )
                            .await?
                        }
                        ClaimLoopKind::Batch => {
                            // 0 = inherit the (often deployment-tuned) single-loop cap.
                            let batch_claim_size = if self.config.batch_claim_size == 0 {
                                self.config.claim_batch_size
                            } else {
                                self.config.batch_claim_size
                            };
                            with_query_timeout(
                                "batch claim query",
                                claim_timeout,
                                self.storage.claim_batch_requests(
                                    batch_claim_size,
                                    self.config.batch_claim_batch_size,
                                    self.daemon_id,
                                    &available_capacity,
                                    &user_active_counts,
                                ),
                            )
                            .await?
                        }
                    };

                    Ok(Some((claimed, claim_start.elapsed())))
                } => result,
                _ = self.shutdown_token.cancelled() => {
                    tracing::info!(loop_name, "Shutdown signal received, stopping claim loop");
                    break Ok(());
                }
            };

            let (mut claimed, claim_duration) = match claim_result {
                Ok(Some(claimed)) => {
                    consecutive_claim_failures = 0;
                    claimed
                }
                Ok(None) => {
                    consecutive_claim_failures = 0;
                    continue;
                }
                Err(e) => {
                    drop(_claim_guard);
                    consecutive_claim_failures += 1;
                    counter!("fusillade_claim_loop_errors_total", "daemon" => loop_name)
                        .increment(1);
                    if consecutive_claim_failures >= self.config.claim_loop_max_consecutive_failures
                    {
                        tracing::error!(
                            loop_name,
                            consecutive_claim_failures,
                            error = %e,
                            "Claim loop giving up after repeated consecutive failures"
                        );
                        break Err(e);
                    }

                    let base_interval = Duration::from_millis(interval_ms);
                    let backoff = claim_failure_backoff(consecutive_claim_failures, interval_ms);
                    let retry_delay = base_interval.max(backoff);
                    let backoff_sleep = retry_delay.saturating_sub(base_interval);
                    tracing::warn!(
                        loop_name,
                        consecutive_claim_failures,
                        backoff_ms = retry_delay.as_millis() as u64,
                        sleep_ms = backoff_sleep.as_millis() as u64,
                        error = %e,
                        "Claim failed; backing off before retry"
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(backoff_sleep) => {},
                        _ = self.shutdown_token.cancelled() => {
                            tracing::info!(loop_name, "Shutdown signal received, stopping claim loop");
                            break Ok(());
                        }
                    }
                    continue;
                }
            };
            // Dual-emit legacy unlabeled histograms during the deprecation
            // window (see fusillade_claim_capacity above).
            histogram!("fusillade_claim_duration_seconds").record(claim_duration.as_secs_f64());
            histogram!("fusillade_claim_duration_seconds", "daemon" => loop_name)
                .record(claim_duration.as_secs_f64());
            histogram!("fusillade_claim_size").record(claimed.len() as f64);
            histogram!("fusillade_claim_size", "daemon" => loop_name).record(claimed.len() as f64);

            tracing::debug!(
                loop_name,
                claimed_count = claimed.len(),
                "Claimed requests from storage"
            );

            if kind == ClaimLoopKind::Request {
                self.stamp_leaks(&claimed);
            }

            self.prepare_claimed_requests(&mut claimed);
            self.dispatch_claimed_requests(&mut join_set, claimed);
        }
    }

    fn prepare_claimed_requests(&self, claimed: &mut [Request<Claimed>]) {
        for request in claimed.iter_mut() {
            if let Some(config) = self.config.model_escalations.get(&request.data.model) {
                let time_remaining = request.state.batch_expires_at - chrono::Utc::now();
                if time_remaining.num_seconds() < config.escalation_threshold_seconds {
                    let original_model = request.data.model.clone();
                    request.data.model = config.escalation_model.clone();

                    if let Ok(mut json) =
                        serde_json::from_str::<serde_json::Value>(&request.data.body)
                        && let Some(obj) = json.as_object_mut()
                    {
                        obj.insert(
                            "model".to_string(),
                            serde_json::Value::String(config.escalation_model.clone()),
                        );
                        if let Ok(new_body) = serde_json::to_string(&json) {
                            request.data.body = new_body;
                        }
                    }

                    counter!("fusillade_requests_routed_to_escalation_total", "original_model" => original_model.clone(), "escalation_model" => config.escalation_model.clone()).increment(1);
                    tracing::info!(
                        request_id = %request.data.id,
                        original_model = %original_model,
                        escalation_model = %config.escalation_model,
                        time_remaining_seconds = time_remaining.num_seconds(),
                        threshold_seconds = config.escalation_threshold_seconds,
                        "Routing request to escalation model due to time pressure"
                    );
                }
            }
        }

        if self.config.inject_deadline_priority {
            for request in claimed {
                let priority: i32 = (-request.state.batch_expires_at.timestamp())
                    .clamp(i32::MIN as i64, i32::MAX as i64)
                    as i32;

                if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&request.data.body)
                    && let Some(obj) = json.as_object_mut()
                {
                    let nvext = obj.entry("nvext").or_insert_with(|| serde_json::json!({}));
                    if let Some(nvext_obj) = nvext.as_object_mut() {
                        let agent_hints = nvext_obj
                            .entry("agent_hints")
                            .or_insert_with(|| serde_json::json!({}));
                        if let Some(hints_obj) = agent_hints.as_object_mut() {
                            hints_obj.insert(
                                "priority".to_string(),
                                serde_json::Value::Number(priority.into()),
                            );
                            if let Ok(new_body) = serde_json::to_string(&json) {
                                request.data.body = new_body;
                            }
                        }
                    }
                }
            }
        }
    }

    fn dispatch_claimed_requests(
        self: &Arc<Self>,
        join_set: &mut JoinSet<Result<()>>,
        claimed: Vec<Request<Claimed>>,
    ) {
        let mut by_model: HashMap<String, Vec<_>> = HashMap::new();
        for request in claimed {
            let model = request.data.model.clone();
            by_model.entry(model).or_default().push(request);
        }

        tracing::debug!(
            models = by_model.len(),
            total_requests = by_model.values().map(|v| v.len()).sum::<usize>(),
            "Grouped requests by model"
        );

        for (model, requests) in by_model {
            tracing::debug!(model = %model, count = requests.len(), "Processing requests for model");

            for request in requests {
                let request_id = request.data.id;
                let batch_id = request.data.batch_id;

                tracing::trace!(
                    request_id = %request_id,
                    batch_id = ?batch_id,
                    model = %model,
                    "Spawning processing task"
                );

                let model_clone = model.clone();
                let user_id = request.data.created_by.clone();
                let completion_window = request
                    .data
                    .batch_metadata
                    .get("completion_window")
                    .cloned()
                    .unwrap_or_default();

                // Pickup delay: submission (`created_at`) to first claim — the
                // queue-wait component of submission-epoch TTFT. Re-claims after
                // retries are retry mechanics, not pickup, so only the first
                // attempt records.
                if request.state.retry_attempt == 0
                    && let Some(created_at) = crate::http::submission_time(&request.data)
                {
                    let delay_ms = (request.state.claimed_at - created_at).num_milliseconds();
                    if delay_ms >= 0 {
                        histogram!("fusillade_request_pickup_delay_seconds", "model" => model_clone.clone(), "completion_window" => completion_window.clone())
                            .record(delay_ms as f64 / 1000.0);
                    }
                }
                let storage = self.storage.clone();
                let http_client = (*self.http_client).clone();
                let processor = self.processor.clone();
                let retry_config: crate::request::transitions::RetryConfig = (&self.config).into();
                let requests_in_flight = self.requests_in_flight.clone();
                let user_throughput = self.user_throughput.clone();
                let user_requests_in_flight = self.user_requests_in_flight.clone();
                let requests_processed = self.requests_processed.clone();
                let requests_failed = self.requests_failed.clone();
                let should_retry = self.config.should_retry.clone();
                let shutdown_token = self.shutdown_token.clone();
                let cancellation_tokens = self.cancellation_tokens.clone();

                let batch_cancellation_token = match batch_id {
                    Some(bid) => cancellation_tokens.entry(bid).or_default().clone(),
                    None => tokio_util::sync::CancellationToken::new(),
                };

                requests_in_flight
                    .entry(model_clone.clone())
                    .or_default()
                    .fetch_add(1, Ordering::Relaxed);
                gauge!("fusillade_requests_in_flight", "model" => model_clone.clone())
                    .increment(1.0);

                user_requests_in_flight
                    .entry(user_id.clone())
                    .or_default()
                    .fetch_add(1, Ordering::Relaxed);
                gauge!("fusillade_user_requests_in_flight", "user" => user_id.clone(), "completion_window" => completion_window.clone())
                    .increment(1.0);

                let process_span = tracing::info_span!(
                    parent: tracing::Span::none(),
                    "fusillade.process_request",
                    trace_id = tracing::field::Empty,
                    otel.name = "fusillade.process_request",
                    request_id = %request_id,
                    batch_id = ?batch_id,
                    model = %model,
                    outcome = tracing::field::Empty,
                );

                join_set.spawn(async move {
                    let span = tracing::Span::current();
                    let sc = span.context().span().span_context().clone();
                    if sc.is_valid() {
                        span.record("trace_id", tracing::field::display(sc.trace_id()));
                    }

                    let processing_start = std::time::Instant::now();
                    let model_for_guard = model_clone.clone();
                    let user_for_guard = user_id.clone();
                    let cw_for_guard = completion_window.clone();
                    let in_flight_for_guard = requests_in_flight.clone();
                    let user_in_flight_for_guard = user_requests_in_flight.clone();
                    let _guard = scopeguard::guard((), move |_| {
                        if let Some(counter) = in_flight_for_guard.get(&model_for_guard) {
                            counter.value().fetch_sub(1, Ordering::Relaxed);
                        }
                        gauge!("fusillade_requests_in_flight", "model" => model_for_guard).decrement(1.0);
                        gauge!("fusillade_user_requests_in_flight", "user" => user_for_guard.clone(), "completion_window" => cw_for_guard).decrement(1.0);
                        if let Some(counter) = user_in_flight_for_guard.get(&user_for_guard) {
                            let prev = counter.value().fetch_sub(1, Ordering::Relaxed);
                            drop(counter);
                            if prev == 1 {
                                user_in_flight_for_guard.remove(&user_for_guard);
                            }
                        }
                    });

                    let batch_expires_at = request.state.batch_expires_at;
                    let retry_attempt_at_completion = request.state.retry_attempt;
                    let owning_daemon_id = request.state.daemon_id;
                    let attempt_id = request.state.attempt_id;
                    let recovery_data = request.data.clone();
                    let recovery_shutdown_token = shutdown_token.clone();

                    let cancellation: crate::processor::CancellationFuture = Box::pin(async move {
                        tokio::select! {
                            _ = batch_cancellation_token.cancelled() => {
                                crate::request::transitions::CancellationReason::User
                            }
                            _ = shutdown_token.cancelled() => {
                                crate::request::transitions::CancellationReason::Shutdown
                            }
                        }
                    });

                    let processor_result =
                        std::panic::AssertUnwindSafe(processor.process(
                            request,
                            http_client,
                            storage.as_ref(),
                            should_retry.clone(),
                            cancellation,
                        ))
                        .catch_unwind()
                        .await;

                    let mut processor_failure = None;
                    let completion_result = match processor_result {
                        Ok(Ok(result)) => result,
                        Ok(Err(FusilladeError::Shutdown)) => {
                            tracing::Span::current().record("outcome", "shutdown");
                            return Ok(());
                        }
                        Ok(Err(
                            FusilladeError::RequestAttemptLost { .. }
                            | FusilladeError::RequestNotFound(_)
                            | FusilladeError::RequestStateConflict { .. },
                        )) => {
                            tracing::Span::current().record("outcome", "ownership_lost");
                            counter!(
                                "fusillade_request_attempt_outcomes_total",
                                "model" => model_clone.clone(),
                                "resolution" => "revoked"
                            )
                            .increment(1);
                            tracing::info!(
                                %request_id,
                                ?batch_id,
                                %attempt_id,
                                "Request attempt result discarded after ownership loss"
                            );
                            return Ok(());
                        }
                        Ok(Err(
                            FusilladeError::ValidationError(_)
                            | FusilladeError::Serialization(_)
                            | FusilladeError::HttpRequestBuilder(_)
                            | FusilladeError::InvalidState(..),
                        )) => {
                            tracing::warn!(
                                %request_id,
                                ?batch_id,
                                %attempt_id,
                                "Request processor returned a validation error; terminalizing"
                            );
                            RequestCompletionResult::Failed(Request {
                                data: recovery_data.clone(),
                                state: Failed {
                                    reason: FailureReason::RequestBuilderError {
                                        error: "Request processor rejected the request".to_string(),
                                    },
                                    failed_at: chrono::Utc::now(),
                                    retry_attempt: retry_attempt_at_completion,
                                    batch_expires_at,
                                    routed_model: model_clone.clone(),
                                },
                            })
                        }
                        Ok(Err(_)) => {
                            processor_failure = Some("error");
                            counter!(
                                "fusillade_request_processor_failures_total",
                                "model" => model_clone.clone(),
                                "source" => "error"
                            )
                            .increment(1);
                            tracing::warn!(
                                %request_id,
                                ?batch_id,
                                %attempt_id,
                                "Request processor failed; recovering the exact attempt"
                            );
                            RequestCompletionResult::Failed(Request {
                                data: recovery_data.clone(),
                                state: Failed {
                                    reason: FailureReason::ProcessorError,
                                    failed_at: chrono::Utc::now(),
                                    retry_attempt: retry_attempt_at_completion,
                                    batch_expires_at,
                                    routed_model: model_clone.clone(),
                                },
                            })
                        }
                        Err(_) => {
                            processor_failure = Some("panic");
                            counter!(
                                "fusillade_request_processor_failures_total",
                                "model" => model_clone.clone(),
                                "source" => "panic"
                            )
                            .increment(1);
                            tracing::error!(
                                %request_id,
                                ?batch_id,
                                %attempt_id,
                                "Request processor panicked; recovering the exact attempt"
                            );
                            RequestCompletionResult::Failed(Request {
                                data: recovery_data.clone(),
                                state: Failed {
                                    reason: FailureReason::TaskTerminated,
                                    failed_at: chrono::Utc::now(),
                                    retry_attempt: retry_attempt_at_completion,
                                    batch_expires_at,
                                    routed_model: model_clone.clone(),
                                },
                            })
                        }
                    };

                    match completion_result {
                        RequestCompletionResult::Completed(completed) => {
                            let fallback = terminal_persistence_fallback(
                                recovery_data.clone(),
                                retry_attempt_at_completion,
                                batch_expires_at,
                                model_clone.clone(),
                            );
                            let resolution = match persist_terminal_attempt(
                                storage.as_ref(),
                                &recovery_shutdown_token,
                                &completed,
                                fallback,
                                attempt_id,
                                "terminal_completion",
                            )
                            .await
                            {
                                Ok(resolution) => resolution,
                                Err(FusilladeError::Shutdown) => {
                                    tracing::Span::current().record("outcome", "shutdown");
                                    return Ok(());
                                }
                                Err(error) => return Err(error),
                            };

                            match resolution {
                                TerminalPersistenceResolution::OriginalApplied => {}
                                TerminalPersistenceResolution::FallbackApplied(failed) => {
                                    tracing::Span::current()
                                        .record("outcome", "persistence_fallback");
                                    record_terminal_failure(
                                        &failed,
                                        TerminalFailureContext {
                                            request_id,
                                            batch_id,
                                            model: &model_clone,
                                            user_id: &user_id,
                                            completion_window: &completion_window,
                                            processing_start,
                                            batch_expires_at,
                                            requests_failed: requests_failed.as_ref(),
                                            user_throughput: user_throughput.as_ref(),
                                        },
                                    );
                                    return Ok(());
                                }
                                TerminalPersistenceResolution::LostOwnership
                                | TerminalPersistenceResolution::RequestMissing => {
                                    tracing::Span::current()
                                        .record("outcome", "ownership_lost");
                                    counter!(
                                        "fusillade_request_attempt_outcomes_total",
                                        "model" => model_clone.clone(),
                                        "resolution" => "revoked"
                                    )
                                    .increment(1);
                                    return Ok(());
                                }
                            }

                            tracing::Span::current().record("outcome", "completed");
                            requests_processed.fetch_add(1, Ordering::Relaxed);
                            user_throughput.entry(user_id.clone()).or_insert_with(|| UserThroughputStats {
                                completed: AtomicU64::new(0),
                                failed: AtomicU64::new(0),
                            }).completed.fetch_add(1, Ordering::Relaxed);
                            counter!("fusillade_requests_completed_total", "model" => model_clone.clone(), "status" => "success", "completion_window" => completion_window.clone()).increment(1);
                            counter!("fusillade_user_requests_completed_total", "user" => user_id.clone(), "status" => "success", "completion_window" => completion_window.clone()).increment(1);
                            histogram!("fusillade_request_duration_seconds", "model" => model_clone.clone(), "status" => "success")
                                .record(processing_start.elapsed().as_secs_f64());
                            histogram!("fusillade_retry_attempts_on_success", "model" => model_clone.clone())
                                .record(retry_attempt_at_completion as f64);

                            let completed_at = completed.state.completed_at;
                            let seconds_until_deadline = (batch_expires_at - completed_at).num_milliseconds() as f64 / 1000.0;
                            gauge!("fusillade_request_deadline_margin_seconds", "model" => model_clone.clone(), "status" => "success")
                                .set(seconds_until_deadline);
                            if completed_at > batch_expires_at {
                                counter!("fusillade_requests_completed_after_sla_total", "model" => model_clone.clone(), "status" => "success", "completion_window" => completion_window.clone()).increment(1);
                                tracing::warn!(
                                    request_id = %request_id,
                                    batch_id = ?batch_id,
                                    "Request completed successfully after SLA"
                                );
                            }
                            Ok(())
                        }
                        RequestCompletionResult::Failed(failed) => {
                            tracing::Span::current().record("outcome", "failed");
                            let retry_attempt = failed.state.retry_attempt;
                            let terminal_failed = if failed.state.reason.is_retriable() {
                                match failed.can_retry(retry_attempt, retry_config.clone()) {
                                    Ok(pending) => {
                                        let resolution = match attempt_write_until_resolved(
                                            &recovery_shutdown_token,
                                            request_id,
                                            attempt_id,
                                            if processor_failure.is_some() {
                                                "processor_recovery_retry"
                                            } else {
                                                "retry_reschedule"
                                            },
                                            || {
                                                if processor_failure.is_some() {
                                                    storage.recover_attempt_for_retry(
                                                        request_id,
                                                        owning_daemon_id,
                                                        attempt_id,
                                                        pending.state.retry_attempt,
                                                        pending.state.not_before,
                                                    )
                                                } else {
                                                    storage.reschedule_attempt_for_retry(
                                                        request_id,
                                                        owning_daemon_id,
                                                        attempt_id,
                                                        pending.state.retry_attempt,
                                                        pending.state.not_before,
                                                    )
                                                }
                                            },
                                        )
                                        .await
                                        {
                                            Ok(resolution) => resolution,
                                            Err(FusilladeError::Shutdown) => {
                                                tracing::Span::current()
                                                    .record("outcome", "shutdown");
                                                return Ok(());
                                            }
                                            Err(error) => return Err(error),
                                        };

                                        match resolution {
                                            AttemptWriteResolution::Applied => {
                                                counter!(
                                                    "fusillade_requests_retried_total",
                                                    "model" => model_clone.clone()
                                                )
                                                .increment(1);
                                                if let Some(source) = processor_failure {
                                                    counter!(
                                                        "fusillade_request_processor_recoveries_total",
                                                        "model" => model_clone.clone(),
                                                        "source" => source,
                                                        "resolution" => "rescheduled"
                                                    )
                                                    .increment(1);
                                                }
                                                tracing::info!(
                                                    %request_id,
                                                    ?batch_id,
                                                    %attempt_id,
                                                    retry_attempt = retry_attempt + 1,
                                                    "Request retry returned to ordinary admission"
                                                );
                                            }
                                            AttemptWriteResolution::LostOwnership
                                            | AttemptWriteResolution::RequestMissing => {
                                                counter!(
                                                    "fusillade_requests_retry_lost_ownership_total",
                                                    "model" => model_clone.clone()
                                                )
                                                .increment(1);
                                                tracing::info!(
                                                    %request_id,
                                                    ?batch_id,
                                                    %attempt_id,
                                                    "Request retry skipped after ownership loss"
                                                );
                                            }
                                        }
                                        return Ok(());
                                    }
                                    Err(failed) => *failed,
                                }
                            } else {
                                failed
                            };

                            let fallback = terminal_persistence_fallback(
                                recovery_data.clone(),
                                terminal_failed.state.retry_attempt,
                                terminal_failed.state.batch_expires_at,
                                model_clone.clone(),
                            );
                            let resolution = match persist_terminal_attempt(
                                storage.as_ref(),
                                &recovery_shutdown_token,
                                &terminal_failed,
                                fallback,
                                attempt_id,
                                if processor_failure.is_some() {
                                    "processor_recovery_terminal"
                                } else {
                                    "terminal_failure"
                                },
                            )
                            .await
                            {
                                Ok(resolution) => resolution,
                                Err(FusilladeError::Shutdown) => {
                                    tracing::Span::current().record("outcome", "shutdown");
                                    return Ok(());
                                }
                                Err(error) => return Err(error),
                            };

                            let terminal_failed = match resolution {
                                TerminalPersistenceResolution::OriginalApplied => terminal_failed,
                                TerminalPersistenceResolution::FallbackApplied(fallback) => {
                                    *fallback
                                }
                                TerminalPersistenceResolution::LostOwnership
                                | TerminalPersistenceResolution::RequestMissing => {
                                    tracing::Span::current()
                                        .record("outcome", "ownership_lost");
                                    counter!(
                                        "fusillade_request_attempt_outcomes_total",
                                        "model" => model_clone.clone(),
                                        "resolution" => "revoked"
                                    )
                                    .increment(1);
                                    return Ok(());
                                }
                            };

                            if let Some(source) = processor_failure {
                                counter!(
                                    "fusillade_request_processor_recoveries_total",
                                    "model" => model_clone.clone(),
                                    "source" => source,
                                    "resolution" => "terminal"
                                )
                                .increment(1);
                            }
                            record_terminal_failure(
                                &terminal_failed,
                                TerminalFailureContext {
                                    request_id,
                                    batch_id,
                                    model: &model_clone,
                                    user_id: &user_id,
                                    completion_window: &completion_window,
                                    processing_start,
                                    batch_expires_at,
                                    requests_failed: requests_failed.as_ref(),
                                    user_throughput: user_throughput.as_ref(),
                                },
                            );
                            Ok(())
                        }
                        RequestCompletionResult::Canceled(_canceled) => {
                            tracing::Span::current().record("outcome", "canceled");
                            counter!("fusillade_requests_cancelled_total", "model" => model_clone.clone()).increment(1);
                            // Keep the pre-split counter shape alive so existing
                            // dashboards/alerts on completed_total{status="cancelled"}
                            // don't silently break (deprecation window).
                            counter!("fusillade_requests_completed_total", "model" => model_clone.clone(), "status" => "cancelled", "completion_window" => completion_window.clone()).increment(1);
                            counter!("fusillade_user_requests_completed_total", "user" => user_id.clone(), "status" => "cancelled", "completion_window" => completion_window.clone()).increment(1);
                            Ok(())
                        }
                    }
                }.instrument(process_span));
            }
        }
    }

    /// Run the daemon loop.
    ///
    /// This continuously claims and processes requests until an error occurs
    /// or the task is cancelled.
    ///
    /// The daemon periodically polls for cancelled batches and aborts in-flight requests.
    #[tracing::instrument(name = "fusillade.daemon.run", skip(self), fields(daemon_id = %self.daemon_id))]
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let mode = self.config.mode;
        self.run_with_mode(mode).await
    }

    /// Run the daemon loop with an explicit claim-loop mode.
    ///
    /// This overrides [`DaemonConfig::mode`] for callers that run separate
    /// binaries and want the mode selected outside serialized configuration.
    #[tracing::instrument(name = "fusillade.daemon.run_with_mode", skip(self), fields(daemon_id = %self.daemon_id, mode = ?mode))]
    pub async fn run_with_mode(self: Arc<Self>, mode: DaemonMode) -> Result<()> {
        tracing::info!("Daemon starting main processing loop");

        // Register daemon in database
        let daemon_record = DaemonRecord {
            data: DaemonData {
                id: self.daemon_id,
                hostname: get_hostname(),
                pid: get_pid(),
                version: get_version(),
                config_snapshot: serde_json::to_value(&self.config)
                    .expect("Failed to serialize daemon config"),
            },
            state: Initializing {
                started_at: chrono::Utc::now(),
            },
        };

        let running_record = daemon_record.start(self.storage.as_ref()).await?;
        tracing::info!("Daemon registered in database");
        // Liveness signal for dashboards/alerts: 1 while this daemon's run
        // loop is alive, 0 once it stops being polled for ANY reason —
        // normal shutdown, early `?` error return, panic unwind, or the
        // future being dropped/cancelled (that's why it's a drop guard and
        // not a pair of set() calls: an early return between them would
        // strand a stale up=1 in a still-running process). A daemon dying
        // inside a live pod is otherwise invisible to metrics (observed
        // 2026-07-08: silent claim outage until a human bounced the pod).
        // Originally added in #322, lost in the #323 workspace split —
        // verified absent from prod on 2026-07-15.
        //
        // Labeled by the effective `mode` ARGUMENT, not `self.config.mode`:
        // run_with_mode exists so split-fleet binaries override the config,
        // and per-role labels are what let one role's exit never zero
        // another's signal. Dashboards: `min by (pod) (fusillade_daemon_up)`
        // catches any dead role. A hard process abort can still skip the
        // final scrape, so alerting pairs this with heartbeat-rate
        // (FusilladeDaemonDown family).
        struct LivenessGaugeGuard {
            mode_label: &'static str,
        }
        impl Drop for LivenessGaugeGuard {
            fn drop(&mut self) {
                gauge!("fusillade_daemon_up", "mode" => self.mode_label).set(0.0);
            }
        }
        let mode_label = mode.metric_label();
        gauge!("fusillade_daemon_up", "mode" => mode_label).set(1.0);
        let _liveness_gauge_guard = LivenessGaugeGuard { mode_label };

        // Spawn periodic heartbeat task
        let storage = self.storage.clone();
        let requests_in_flight = self.requests_in_flight.clone();
        let requests_processed = self.requests_processed.clone();
        let requests_failed = self.requests_failed.clone();
        let daemon_id = self.daemon_id;
        let heartbeat_interval_ms = self.config.heartbeat_interval_ms;
        let shutdown_signal = self.shutdown_token.clone();

        let heartbeat_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(heartbeat_interval_ms));
            let mut daemon_record = running_record;

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let stats = DaemonStats {
                            requests_processed: requests_processed.load(Ordering::Relaxed),
                            requests_failed: requests_failed.load(Ordering::Relaxed),
                            requests_in_flight: requests_in_flight.iter().map(|e| e.value().load(Ordering::Relaxed)).sum(),
                        };

                        // Clone the record so we preserve it if heartbeat fails
                        let current = daemon_record.clone();
                        let heartbeat_start = std::time::Instant::now();
                        let heartbeat_timeout =
                            Duration::from_millis(heartbeat_interval_ms.saturating_mul(4));
                        match with_query_timeout(
                            "heartbeat query",
                            heartbeat_timeout,
                            current.heartbeat(stats, storage.as_ref()),
                        )
                        .await
                        {
                            Ok(updated) => {
                                histogram!("fusillade_heartbeat_duration_seconds")
                                    .record(heartbeat_start.elapsed().as_secs_f64());
                                daemon_record = updated;
                                tracing::trace!(
                                    daemon_id = %daemon_id,
                                    "Heartbeat sent"
                                );
                            }
                            Err(e) => {
                                histogram!("fusillade_heartbeat_duration_seconds")
                                    .record(heartbeat_start.elapsed().as_secs_f64());
                                crate::background_error!(
                                    "heartbeat_failed", Error,
                                    daemon_id = %daemon_id,
                                    error = %e,
                                    "Failed to send heartbeat"
                                );
                                // daemon_record stays unchanged on error
                            }
                        }
                    }
                    _ = shutdown_signal.cancelled() => {
                        // Mark daemon as dead on shutdown
                        tracing::info!("Shutting down heartbeat task");
                        if let Err(e) = daemon_record.shutdown(storage.as_ref()).await {
                            crate::background_error!(
                                "shutdown_mark_failed", Error,
                                daemon_id = %daemon_id,
                                error = %e,
                                "Failed to mark daemon as dead during shutdown"
                            );
                        }
                        break;
                    }
                }
            }
        });

        // Spawn periodic status logging task if configured
        if let Some(interval_ms) = self.config.status_log_interval_ms {
            let requests_in_flight = self.requests_in_flight.clone();
            let daemon_id = self.daemon_id;
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
                loop {
                    interval.tick().await;
                    let count: usize = requests_in_flight
                        .iter()
                        .map(|e| e.value().load(Ordering::Relaxed))
                        .sum();
                    tracing::debug!(
                        daemon_id = %daemon_id,
                        requests_in_flight = count,
                        "Daemon status"
                    );
                }
            });
        }

        // Spawn periodic per-user throughput emission task if configured
        if let Some(interval_ms) = self.config.throughput_log_interval_ms {
            let user_throughput = self.user_throughput.clone();
            let user_requests_in_flight = self.user_requests_in_flight.clone();
            let daemon_id = self.daemon_id;
            let shutdown_token = self.shutdown_token.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
                // Skip the immediate first tick to avoid a near-zero window on the first emission
                interval.tick().await;
                let mut last_emission = std::time::Instant::now();

                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            let elapsed = last_emission.elapsed();
                            let window_secs = elapsed.as_secs_f64();

                            // Atomically read-and-reset each user's counters
                            let mut users_to_remove = Vec::new();
                            for entry in user_throughput.iter() {
                                let user_id = entry.key();
                                let completed = entry.value().completed.swap(0, Ordering::Relaxed);
                                let failed = entry.value().failed.swap(0, Ordering::Relaxed);

                                if completed > 0 || failed > 0 {
                                    let in_flight = user_requests_in_flight
                                        .get(user_id)
                                        .map(|e| e.value().load(Ordering::Relaxed))
                                        .unwrap_or(0);
                                    let throughput_rpm = if window_secs > 0.0 {
                                        (completed + failed) as f64 / window_secs * 60.0
                                    } else {
                                        0.0
                                    };

                                    tracing::info!(
                                        daemon_id = %daemon_id,
                                        user = %user_id,
                                        completed = completed,
                                        failed = failed,
                                        in_flight = in_flight,
                                        throughput_rpm = format!("{throughput_rpm:.1}"),
                                        window_seconds = format!("{window_secs:.1}"),
                                        "fusillade.user_throughput"
                                    );
                                } else {
                                    // No activity — mark for eviction
                                    users_to_remove.push(user_id.clone());
                                }
                            }

                            // Evict inactive users to prevent unbounded map growth
                            for user_id in users_to_remove {
                                user_throughput.remove(&user_id);
                            }

                            last_emission = std::time::Instant::now();
                        }
                        _ = shutdown_token.cancelled() => {
                            tracing::debug!(
                                daemon_id = %daemon_id,
                                "Shutting down per-user throughput emission task"
                            );
                            break;
                        }
                    }
                }
            });
        }

        // Spawn periodic batch polling task for finalization and cancellation detection
        // This serves two purposes in one efficient loop:
        // 1. Triggers lazy finalization by fetching batches (computes completion timestamps)
        // 2. Detects cancelled batches and aborts their in-flight requests
        let cancellation_tokens = self.cancellation_tokens.clone();
        let storage = self.storage.clone();
        let shutdown_token = self.shutdown_token.clone();
        let cancellation_poll_interval_ms = self.config.cancellation_poll_interval_ms;
        // Same deadness detector as the claim loops. These queries are bounded,
        // so a timeout strongly suggests a dead/stalled connection or pool
        // acquisition rather than legitimately long work.
        let poll_query_timeout = Duration::from_millis(self.config.claim_query_timeout_ms);

        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_millis(cancellation_poll_interval_ms));
            tracing::info!(
                interval_ms = cancellation_poll_interval_ms,
                "Batch polling started (finalization + cancellation detection)"
            );

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // Get all active batch IDs we're currently processing
                        let active_batch_ids: Vec<BatchId> = cancellation_tokens
                            .iter()
                            .map(|entry| *entry.key())
                            .collect();

                        if active_batch_ids.is_empty() {
                            continue;
                        }

                        let poll_start = std::time::Instant::now();
                        gauge!("fusillade_cancellation_poll_batches_checked")
                            .set(active_batch_ids.len() as f64);

                        // Single bulk query to find which active batches have been cancelled.
                        // If a silently severed connection wedges this poll, cancelled batches
                        // keep spending and finalization stalls, so bound it like claims.
                        match with_query_timeout(
                            "cancellation poll query",
                            poll_query_timeout,
                            storage.get_cancelled_batch_ids(&active_batch_ids),
                        )
                        .await
                        {
                            Ok(cancelled_ids) => {
                                for batch_id in cancelled_ids {
                                    if let Some(entry) = cancellation_tokens.get(&batch_id) {
                                        entry.value().cancel();
                                        counter!("fusillade_batches_cancelled_total").increment(1);
                                        tracing::info!(batch_id = %batch_id, "Cancelled all requests in batch");
                                        drop(entry);
                                        cancellation_tokens.remove(&batch_id);
                                    }
                                }
                            }
                            Err(e) => {
                                // Sustained failure means cancelled batches keep spending and
                                // completed batches never get finalized - error, not warn.
                                crate::background_error!(
                                    "cancellation_poll_failed", Error,
                                    error = %e,
                                    "Failed to check batch cancellation status"
                                );
                            }
                        }

                        histogram!("fusillade_cancellation_poll_duration_seconds")
                            .record(poll_start.elapsed().as_secs_f64());
                    }
                    _ = shutdown_token.cancelled() => {
                        tracing::info!("Shutting down batch polling");
                        break;
                    }
                }
            }
        });

        // Spawn periodic purge task for orphaned rows (right-to-erasure compliance)
        if self.config.purge_interval_ms > 0 {
            let storage = self.storage.clone();
            let shutdown_token = self.shutdown_token.clone();
            let purge_interval_ms = self.config.purge_interval_ms;
            let purge_batch_size = self.config.purge_batch_size;
            let purge_throttle_ms = self.config.purge_throttle_ms;
            let purge_query_timeout = Duration::from_millis(self.config.claim_query_timeout_ms);
            let mf_keep_per_model = self.config.model_filters_keep_per_model;
            let mf_retention_secs = self.config.model_filters_retention_ms as f64 / 1000.0;

            tokio::spawn(async move {
                tracing::info!(
                    interval_ms = purge_interval_ms,
                    batch_size = purge_batch_size,
                    throttle_ms = purge_throttle_ms,
                    "Orphaned row purge task started"
                );

                loop {
                    // Sleep for the configured interval (interruptible by shutdown)
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(purge_interval_ms)) => {},
                        _ = shutdown_token.cancelled() => {
                            tracing::info!("Shutting down purge task");
                            break;
                        }
                    }

                    // Drain orphaned rows in batches
                    loop {
                        match with_query_timeout(
                            "orphan purge query",
                            purge_query_timeout,
                            storage.purge_orphaned_rows(purge_batch_size),
                        )
                        .await
                        {
                            Ok(0) => break,
                            Ok(deleted) => {
                                counter!("fusillade_rows_purged_total").increment(deleted);
                                tracing::debug!(deleted, "Purged orphaned rows");
                                // Throttle between batches to avoid sustained DB load
                                tokio::select! {
                                    _ = tokio::time::sleep(Duration::from_millis(purge_throttle_ms)) => {},
                                    _ = shutdown_token.cancelled() => {
                                        tracing::info!("Shutting down purge task during drain");
                                        return;
                                    }
                                }
                            }
                            Err(e) => {
                                crate::background_error!("purge_failed", Error, error = %e, "Failed to purge orphaned rows");
                                break;
                            }
                        }
                    }

                    // Drain old model_filters events (append-only log), always
                    // keeping the latest events per model + the retention
                    // window so the claim gate never loses current state.
                    loop {
                        match with_query_timeout(
                            "model_filters purge query",
                            purge_query_timeout,
                            storage.purge_model_filter_events(
                                purge_batch_size,
                                mf_keep_per_model,
                                mf_retention_secs,
                            ),
                        )
                        .await
                        {
                            Ok(0) => break,
                            Ok(deleted) => {
                                counter!("fusillade_model_filter_events_purged_total")
                                    .increment(deleted);
                                tracing::debug!(deleted, "Purged old model_filters events");
                                tokio::select! {
                                    _ = tokio::time::sleep(Duration::from_millis(purge_throttle_ms)) => {},
                                    _ = shutdown_token.cancelled() => {
                                        tracing::info!("Shutting down purge task during model_filters drain");
                                        return;
                                    }
                                }
                            }
                            Err(e) => {
                                counter!("fusillade_purge_errors_total").increment(1);
                                tracing::error!(error = %e, "Failed to purge model_filters events");
                                break;
                            }
                        }
                    }
                }
            });
        }

        let mut claim_daemons: JoinSet<Result<()>> = JoinSet::new();
        let supports_batch_claims = self.storage.supports_batch_claims();
        let claim_loop_kinds = claim_loop_kinds_for_mode(mode, supports_batch_claims)?;

        if mode == DaemonMode::Both && !supports_batch_claims {
            tracing::info!(
                daemon_id = %self.daemon_id,
                "Storage backend does not support batch claims; running request-only"
            );
        }

        // ---- Batch-archive maintenance + movers ----
        // Only batch-mode daemons touch the archive. The partition-runway
        // tick ALWAYS runs for them (partitions must exist before anyone
        // flips the move flags); the sweep/backfill movers are config-gated:
        // deploys never move data — only these flags do, and only once every
        // pod in the fleet understands location routing (blue/green
        // invariant, see batches.location column comment).
        let batch_mode = claim_loop_kinds.contains(&ClaimLoopKind::Batch);
        if batch_mode && supports_batch_claims {
            {
                let storage = self.storage.clone();
                let shutdown_token = self.shutdown_token.clone();
                let weeks_ahead = self.config.batch_archive_partitions_weeks_ahead;
                tokio::spawn(async move {
                    loop {
                        match storage.ensure_archive_partitions(weeks_ahead).await {
                            Ok((created, ahead)) => {
                                gauge!("fusillade_archive_partitions_ahead").set(ahead as f64);
                                if created > 0 {
                                    tracing::info!(created, ahead, "Created archive partitions");
                                }
                            }
                            Err(e) => {
                                crate::background_error!("archive_partition_ensure_failed", Error, error = %e, "Failed to ensure archive partitions");
                            }
                        }
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_millis(86_400_000)) => {},
                            _ = shutdown_token.cancelled() => break,
                        }
                    }
                });
            }

            // The sweeper (new terminals) and the historical backfill are the
            // SAME mover on different pacing knobs — one tested code path.
            // Both run OLDEST-first: in steady state the sweeper drains its
            // whole candidate set every few ticks so order is cosmetic; under
            // a backlog, the oldest batches are the least likely to ever be
            // re-read, so early issues have minimal blast radius (same
            // argument as the historical drain).
            for (worker, enabled, interval_ms, per_tick, dwell, concurrency) in [
                (
                    "sweep",
                    self.config.batch_archive_sweep_enabled,
                    self.config.batch_archive_sweep_interval_ms,
                    self.config.batch_archive_sweep_moves_per_tick,
                    self.config.batch_archive_sweep_dwell_secs,
                    1usize,
                ),
                (
                    "backfill",
                    self.config.batch_archive_backfill_enabled,
                    self.config.batch_archive_backfill_interval_ms,
                    self.config.batch_archive_backfill_moves_per_tick,
                    0.0,
                    self.config.batch_archive_backfill_concurrency,
                ),
            ] {
                if !enabled {
                    continue;
                }
                let storage = self.storage.clone();
                let shutdown_token = self.shutdown_token.clone();
                let grace = self.config.batch_archive_cancel_grace_secs;
                tokio::spawn(async move {
                    // Misconfiguration guard: interval 0 would busy-loop the
                    // DB, and per_tick <= 0 defeats "bounded per tick"
                    // (Postgres treats LIMIT -1 as unlimited). Disable the
                    // worker loudly rather than run it dangerously.
                    if interval_ms == 0 || per_tick <= 0 {
                        crate::background_error!(
                            "archive_mover_invalid_config",
                            Error,
                            worker,
                            interval_ms,
                            per_tick,
                            "Batch-archive mover disabled due to invalid config"
                        );
                        return;
                    }
                    tracing::info!(worker, interval_ms, per_tick, "Batch-archive mover started");
                    loop {
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_millis(interval_ms)) => {},
                            _ = shutdown_token.cancelled() => {
                                tracing::info!(worker, "Shutting down archive mover");
                                break;
                            }
                        }

                        // Bounded work per tick (orphan-purge pattern) —
                        // never drain-until-empty, so the mover can never
                        // monopolize its loop or the database.
                        let ids = match storage
                            .list_archivable_batches(per_tick, true, grace, dwell)
                            .await
                        {
                            Ok(ids) => ids,
                            Err(e) => {
                                crate::background_error!("archive_list_failed", Error, error = %e, "Failed to list archivable batches");
                                continue;
                            }
                        };

                        // Moves run in waves of `concurrency`: per-move cost
                        // is dominated by fixed transaction overhead on small
                        // batches, so concurrent moves — safe because the
                        // batch lock is taken SKIP LOCKED — are what raise
                        // throughput. An error stops further waves this tick;
                        // the next tick retries (the queue is the data).
                        let mut abort_tick = false;
                        for wave in ids.chunks(concurrency.max(1)) {
                            if shutdown_token.is_cancelled() {
                                return;
                            }
                            if abort_tick {
                                break;
                            }
                            let results = futures::future::join_all(wave.iter().map(|batch_id| {
                                let storage = storage.clone();
                                async move {
                                    let started = std::time::Instant::now();
                                    let result = storage.archive_batch(*batch_id).await;
                                    // Elapsed measured HERE, inside the future:
                                    // after join_all it would include waiting
                                    // for slower wave-mates.
                                    (*batch_id, started.elapsed(), result)
                                }
                            }))
                            .await;
                            for (batch_id, elapsed, result) in results {
                                match result {
                                    Ok(ArchiveOutcome::Archived { rows }) => {
                                        counter!("fusillade_archive_moves_total", "worker" => worker, "outcome" => "archived").increment(1);
                                        counter!("fusillade_archive_moved_rows_total", "worker" => worker).increment(rows);
                                        histogram!("fusillade_archive_move_duration_seconds", "worker" => worker)
                                            .record(elapsed.as_secs_f64());
                                    }
                                    Ok(outcome) => {
                                        let label = match outcome {
                                            ArchiveOutcome::Archived { .. } => unreachable!(),
                                            ArchiveOutcome::SkippedNotFound => "skipped_not_found",
                                            ArchiveOutcome::SkippedNotLive => "skipped_not_live",
                                            ArchiveOutcome::SkippedNotFrozen => {
                                                "skipped_not_frozen"
                                            }
                                            ArchiveOutcome::SkippedNoPartition => {
                                                "skipped_no_partition"
                                            }
                                            ArchiveOutcome::SkippedResponseSteps => {
                                                "skipped_response_steps"
                                            }
                                            ArchiveOutcome::SkippedRetryRaced => {
                                                "skipped_retry_raced"
                                            }
                                        };
                                        counter!("fusillade_archive_moves_total", "worker" => worker, "outcome" => label).increment(1);
                                        if outcome == ArchiveOutcome::SkippedNoPartition {
                                            // The one alert-worthy skip: the
                                            // partition runway failed. The batch
                                            // stays live and fully served.
                                            crate::background_error!("archive_partition_missing", Error, batch_id = %batch_id, "Archive partition missing for batch bucket");
                                        }
                                    }
                                    Err(e) => {
                                        // One error per tick: a wave-wide DB
                                        // failure would otherwise emit
                                        // `concurrency` copies every tick.
                                        if !abort_tick {
                                            crate::background_error!("archive_move_failed", Error, error = %e, batch_id = %batch_id, "Failed to archive batch");
                                        }
                                        abort_tick = true;
                                    }
                                }
                            }
                        }

                        if let Ok(backlog) = storage.count_archivable_batches(grace).await {
                            gauge!("fusillade_archive_backlog").set(backlog as f64);
                        }
                    }
                });
            }
        }

        let reclaim_loop_kind = reclaim_loop_kind_for_mode(mode);
        for claim_loop_kind in claim_loop_kinds {
            let run_reclaim = claim_loop_kind == reclaim_loop_kind;
            match claim_loop_kind {
                ClaimLoopKind::Request => {
                    let request_daemon = RequestDaemon::new(self.clone());
                    claim_daemons.spawn(async move { request_daemon.run(run_reclaim).await });
                }
                ClaimLoopKind::Batch => {
                    let batch_daemon = BatchDaemon::new(self.clone());
                    claim_daemons.spawn(async move { batch_daemon.run(run_reclaim).await });
                }
            }
        }

        let run_result = loop {
            tokio::select! {
                result = claim_daemons.join_next() => {
                    match result {
                        Some(Ok(Ok(()))) => {
                            if claim_daemons.is_empty() {
                                break Ok(());
                            }
                        }
                        Some(Ok(Err(e))) => {
                            self.shutdown_token.cancel();
                            break Err(e);
                        }
                        Some(Err(join_error)) => {
                            self.shutdown_token.cancel();
                            break Err(FusilladeError::Other(anyhow::anyhow!(
                                "claim daemon task panicked: {}",
                                join_error
                            )));
                        }
                        None => break Ok(()),
                    }
                }
                _ = self.shutdown_token.cancelled() => {
                    tracing::info!("Shutdown signal received, stopping daemon");
                    break Ok(());
                }
            }
        };
        claim_daemons.abort_all();

        // Wait for heartbeat task to complete (it will mark daemon as dead)
        tracing::info!("Waiting for heartbeat task to complete");
        if let Err(e) = heartbeat_handle.await {
            crate::background_error!("heartbeat_task_panicked", Critical, error = %e, "Heartbeat task panicked");
        }

        run_result
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    use super::*;

    #[test]
    fn claim_failure_backoff_grows_exponentially_and_caps() {
        assert_eq!(claim_failure_backoff(1, 1000), Duration::from_millis(2_000));
        assert_eq!(claim_failure_backoff(2, 1000), Duration::from_millis(4_000));
        assert_eq!(claim_failure_backoff(3, 1000), Duration::from_millis(8_000));
        assert_eq!(
            claim_failure_backoff(4, 1000),
            Duration::from_millis(16_000)
        );
        assert_eq!(
            claim_failure_backoff(5, 1000),
            Duration::from_millis(30_000)
        );
        assert_eq!(
            claim_failure_backoff(u32::MAX, 1000),
            Duration::from_millis(30_000)
        );
        assert_eq!(claim_failure_backoff(1, 0), Duration::from_millis(200));
    }

    #[test]
    fn default_claim_loop_failure_tolerance_is_ten() {
        assert_eq!(
            DaemonConfig::default().claim_loop_max_consecutive_failures,
            10
        );
    }

    #[test]
    fn daemon_mode_defaults_to_both_and_roundtrips_through_config() {
        assert_eq!(DaemonConfig::default().mode, DaemonMode::Both);

        let mut config = DaemonConfig::default();
        config.mode = DaemonMode::BatchOnly;

        let json = serde_json::to_value(&config).expect("config should serialize");
        assert_eq!(json["mode"], serde_json::json!("batch_only"));

        let decoded: DaemonConfig =
            serde_json::from_value(json).expect("config should deserialize");
        assert_eq!(decoded.mode, DaemonMode::BatchOnly);
    }

    #[test]
    fn daemon_mode_selects_the_expected_claim_loops() {
        assert_eq!(
            claim_loop_kinds_for_mode(DaemonMode::Both, true).expect("both should be supported"),
            vec![ClaimLoopKind::Request, ClaimLoopKind::Batch]
        );
        assert_eq!(
            claim_loop_kinds_for_mode(DaemonMode::RequestOnly, true)
                .expect("request-only should be supported"),
            vec![ClaimLoopKind::Request]
        );
        assert_eq!(
            claim_loop_kinds_for_mode(DaemonMode::BatchOnly, true)
                .expect("batch-only should be supported"),
            vec![ClaimLoopKind::Batch]
        );
        assert!(
            claim_loop_kinds_for_mode(DaemonMode::BatchOnly, false).is_err(),
            "batch-only mode should fail loudly when storage cannot claim batches"
        );
    }

    #[test]
    fn daemon_mode_nominates_exactly_one_reclaim_loop() {
        for (mode, supports_batch_claims, expected) in [
            (DaemonMode::Both, true, ClaimLoopKind::Request),
            (DaemonMode::Both, false, ClaimLoopKind::Request),
            (DaemonMode::RequestOnly, true, ClaimLoopKind::Request),
            (DaemonMode::BatchOnly, true, ClaimLoopKind::Batch),
        ] {
            let loops = claim_loop_kinds_for_mode(mode, supports_batch_claims).unwrap();
            let nominated = reclaim_loop_kind_for_mode(mode);
            assert_eq!(nominated, expected);
            assert_eq!(
                loops.iter().filter(|kind| **kind == nominated).count(),
                1,
                "{mode:?} must run reclaim from exactly one spawned loop"
            );
        }
    }

    #[tokio::test]
    async fn reclaim_runs_before_zero_capacity_exit() {
        let maintenance_calls = Arc::new(AtomicUsize::new(0));
        let capacity_checked = Arc::new(AtomicBool::new(false));
        let calls = maintenance_calls.clone();
        let checked = capacity_checked.clone();

        let (reclaimed, capacity) = prepare_claim_capacity(
            true,
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(2)
            },
            move || {
                assert_eq!(
                    maintenance_calls.load(Ordering::SeqCst),
                    1,
                    "capacity was checked before maintenance completed"
                );
                checked.store(true, Ordering::SeqCst);
                None::<HashMap<String, usize>>
            },
        )
        .await
        .unwrap();

        assert_eq!(reclaimed, 2);
        assert!(capacity.is_none());
        assert!(capacity_checked.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn reclaim_error_prevents_capacity_and_claim_work() {
        let capacity_checked = Arc::new(AtomicBool::new(false));
        let checked = capacity_checked.clone();

        let error = prepare_claim_capacity(
            true,
            async { Err(FusilladeError::Other(anyhow::anyhow!("maintenance failed"))) },
            move || {
                checked.store(true, Ordering::SeqCst);
                Some(HashMap::<String, usize>::new())
            },
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("maintenance failed"));
        assert!(!capacity_checked.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn non_nominated_loop_does_not_poll_reclaim() {
        let (reclaimed, capacity) = prepare_claim_capacity(
            false,
            async {
                panic!("non-nominated loop polled reclaim future");
                #[allow(unreachable_code)]
                Ok(0)
            },
            || Some(HashMap::from([("test".to_string(), 1)])),
        )
        .await
        .unwrap();

        assert_eq!(reclaimed, 0);
        assert_eq!(capacity.unwrap().get("test"), Some(&1));
    }

    #[test]
    fn default_claim_query_timeout_is_three_minutes() {
        assert_eq!(DaemonConfig::default().claim_query_timeout_ms, 180_000);
    }

    #[tokio::test]
    async fn attempt_write_retries_only_typed_infrastructure_failures() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let request_id = crate::request::RequestId::from(uuid::Uuid::new_v4());
        let attempt_id = crate::request::AttemptId::from(uuid::Uuid::new_v4());

        let resolution =
            attempt_write_until_resolved(&shutdown, request_id, attempt_id, "test_write", || {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if call < 2 {
                        Err(FusilladeError::AttemptPersistenceInfrastructure {
                            operation: "test_write",
                            source: anyhow::anyhow!("synthetic database outage"),
                        })
                    } else {
                        Ok(true)
                    }
                }
            })
            .await
            .expect("transient write should recover");

        assert_eq!(resolution, AttemptWriteResolution::Applied);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn deterministic_attempt_write_error_is_not_retried() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let request_id = crate::request::RequestId::from(uuid::Uuid::new_v4());
        let attempt_id = crate::request::AttemptId::from(uuid::Uuid::new_v4());

        let error =
            attempt_write_until_resolved(&shutdown, request_id, attempt_id, "test_write", || {
                calls.fetch_add(1, Ordering::SeqCst);
                async {
                    Err(FusilladeError::Other(anyhow::anyhow!(
                        "response body too large"
                    )))
                }
            })
            .await
            .expect_err("deterministic error must fail loudly");

        assert!(error.to_string().contains("response body too large"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn legacy_pool_timeout_text_does_not_make_other_retryable() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let calls = Arc::new(AtomicUsize::new(0));

        let error = attempt_write_until_resolved(
            &shutdown,
            crate::request::RequestId::from(uuid::Uuid::new_v4()),
            crate::request::AttemptId::from(uuid::Uuid::new_v4()),
            "test_write",
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                async {
                    Err(FusilladeError::Other(anyhow::anyhow!(
                        "pool timed out while waiting for an open connection"
                    )))
                }
            },
        )
        .await
        .expect_err("generic error text must not enter durability retry");

        assert!(matches!(error, FusilladeError::Other(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn definitive_attempt_write_outcomes_are_classified_without_retry() {
        let request_id = crate::request::RequestId::from(uuid::Uuid::new_v4());
        let attempt_id = crate::request::AttemptId::from(uuid::Uuid::new_v4());

        assert_eq!(
            classify_attempt_write_result(Ok(false)).unwrap(),
            AttemptWriteResolution::LostOwnership
        );
        assert_eq!(
            classify_attempt_write_result(Err(FusilladeError::RequestNotFound(request_id)))
                .unwrap(),
            AttemptWriteResolution::RequestMissing
        );
        assert_eq!(
            classify_attempt_write_result(Err(FusilladeError::RequestAttemptLost {
                id: request_id,
                attempt_id,
            }))
            .unwrap(),
            AttemptWriteResolution::LostOwnership
        );
        assert_eq!(
            classify_attempt_write_result(Err(FusilladeError::RequestStateConflict {
                id: request_id,
                current_state: "completed".to_string(),
                expected: "claimed or processing",
            }))
            .unwrap(),
            AttemptWriteResolution::LostOwnership
        );
        assert!(matches!(
            classify_attempt_write_result(Err(FusilladeError::ValidationError(
                "bad attempt".to_string()
            ))),
            Err(FusilladeError::ValidationError(_))
        ));
        assert!(matches!(
            classify_attempt_write_result(Err(FusilladeError::InvalidState(
                request_id,
                "claimed".to_string(),
                "processing".to_string(),
            ))),
            Err(FusilladeError::InvalidState(..))
        ));
    }

    #[tokio::test]
    async fn shutdown_interrupts_an_attempt_write_in_progress() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        shutdown.cancel();

        let result = tokio::time::timeout(
            Duration::from_millis(100),
            attempt_write_until_resolved(
                &shutdown,
                crate::request::RequestId::from(uuid::Uuid::new_v4()),
                crate::request::AttemptId::from(uuid::Uuid::new_v4()),
                "test_write",
                || std::future::pending::<Result<bool>>(),
            ),
        )
        .await
        .expect("shutdown must interrupt the storage future");

        assert!(matches!(result, Err(FusilladeError::Shutdown)));
    }

    #[tokio::test]
    async fn shutdown_interrupts_attempt_write_backoff() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let first_call = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let task = {
            let shutdown = shutdown.clone();
            let first_call = first_call.clone();
            let calls = calls.clone();
            tokio::spawn(async move {
                attempt_write_until_resolved(
                    &shutdown,
                    crate::request::RequestId::from(uuid::Uuid::new_v4()),
                    crate::request::AttemptId::from(uuid::Uuid::new_v4()),
                    "test_write",
                    || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        first_call.notify_one();
                        async {
                            Err(FusilladeError::AttemptPersistenceInfrastructure {
                                operation: "test_write",
                                source: anyhow::anyhow!("synthetic outage"),
                            })
                        }
                    },
                )
                .await
            })
        };

        first_call.notified().await;
        shutdown.cancel();
        let result = tokio::time::timeout(Duration::from_millis(100), task)
            .await
            .expect("shutdown must interrupt durability backoff")
            .expect("durability task panicked");
        assert!(matches!(result, Err(FusilladeError::Shutdown)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn query_timeout_converts_hang_into_error() {
        let hung = std::future::pending::<Result<()>>();
        let result = with_query_timeout("test query", Duration::from_millis(50), hung).await;
        let err = result.expect_err("hung future must time out").to_string();
        assert!(
            err.contains("timed out after 50ms"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn query_timeout_passes_through_completed_results() {
        let ok = with_query_timeout("test query", Duration::from_millis(50), async {
            Ok(7usize)
        })
        .await
        .expect("completed future must pass through");
        assert_eq!(ok, 7);

        let err = with_query_timeout("test query", Duration::from_millis(50), async {
            Err::<(), _>(FusilladeError::Other(anyhow::anyhow!("real db error")))
        })
        .await
        .expect_err("inner error must pass through");
        assert!(err.to_string().contains("real db error"));
    }
}
