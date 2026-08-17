//! Daemon for processing batched requests with per-model concurrency control.
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

mod adaptive_concurrency;
pub mod config;
mod memory_gate;

use adaptive_concurrency::{AdaptiveConcurrencyController, ConcurrencyAdjustment};
use memory_gate::{CgroupMemorySource, MemoryGate};
use metrics::{counter, gauge, histogram};
use tokio::task::JoinSet;

use opentelemetry::trace::TraceContextExt;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::FusilladeError;
use crate::batch::BatchId;
use crate::error::Result;
use crate::http::HttpClient;
use crate::manager::{
    ArchiveOutcome, DaemonStorage, RetainedResponseArchiveCutoffs,
    RetainedResponseRetirementOutcome, RetentionPolicy, Storage,
};
use crate::processor::{DefaultRequestProcessor, RequestProcessor};
use crate::request::{Claimed, DaemonId, FailureReason, Request, RequestCompletionResult};

pub use config::{
    DaemonConfig, DaemonMode, ModelEscalationConfig, RetentionMaintenanceConfig, ShouldRetryFn,
    default_should_retry,
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

/// A claimed request after route-at-claim-time rewriting.
///
/// `request.data.model` is the downstream model that receives the request.
/// `capacity_model` is the configured model whose claim slot this request
/// consumes. They differ when escalation rewrites the route after the storage
/// claim has already been made.
struct PreparedRequest {
    request: Request<Claimed>,
    capacity_model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimLoopKind {
    Request,
    Batch,
    BackgroundRequest,
    BackgroundBatch,
}

impl ClaimLoopKind {
    fn is_background(self) -> bool {
        matches!(self, Self::BackgroundRequest | Self::BackgroundBatch)
    }

    fn uses_foreground_accounting(self) -> bool {
        !self.is_background()
    }

    fn emits_legacy_claim_metrics(self) -> bool {
        self.uses_foreground_accounting()
    }

    fn claim_interval_ms(self, config: &DaemonConfig) -> u64 {
        if matches!(self, Self::Batch | Self::BackgroundBatch) && config.batch_claim_interval_ms > 0
        {
            config.batch_claim_interval_ms
        } else {
            config.claim_interval_ms
        }
    }

    fn claim_size(self, config: &DaemonConfig) -> usize {
        if matches!(self, Self::Batch | Self::BackgroundBatch) && config.batch_claim_size > 0 {
            config.batch_claim_size
        } else {
            config.claim_batch_size
        }
    }
}

/// Reserved NVIDIA Dynamo priority for background work. Higher integer values
/// are more important, so this is strictly below every SLA priority.
pub const BACKGROUND_DYNAMO_PRIORITY: i32 = i32::MIN;
/// Lowest priority an SLA request may receive, reserving `i32::MIN` for
/// background work.
pub const MIN_SLA_DYNAMO_PRIORITY: i32 = i32::MIN + 1;

/// Whether the adaptive controller may run.
///
/// It treats a model's configured limit as a starting point and grows past it,
/// so with it on nothing else bounds in-flight work. Pairing it with the memory
/// gate is not advice, it is the only thing standing between a successful ramp
/// and an OOM, so the daemon refuses the combination rather than trusting it to
/// be configured correctly.
fn adaptive_concurrency_permitted(requested: bool, has_memory_gate: bool) -> bool {
    !requested || has_memory_gate
}

fn background_capacity(ordinary_limit: usize, background_limit: usize, in_flight: usize) -> usize {
    ordinary_limit
        .min(background_limit)
        .saturating_sub(in_flight)
}

/// The error code onwards returns from its own concurrency limiter, alongside a
/// 429. Distinguishes "too many at once", which lowering concurrency fixes, from
/// a provider's token-per-minute quota, which it does not.
const ONWARDS_CONCURRENCY_LIMIT_CODE: &str = "concurrency_limit_exceeded";

/// Whether a failure means "the model had nowhere to put this request".
///
/// Two shapes count. An exact 529 is the upstream provider saying it is
/// overloaded. A 429 carrying `concurrency_limit_exceeded` is onwards' own
/// limiter, which is the wall this daemon actually reaches first: onwards sits
/// between fusillade and every provider and never emits 529 itself, so matching
/// 529 alone leaves the controller with a brake that cannot fire.
///
/// That matters because the increase side grows on *demand* - a model that fills
/// every slot it is offered is raised - so without a working decrease signal the
/// limit only ever ratchets up.
///
/// A bare 429 without that code is deliberately excluded. It is a provider rate
/// limit, usually tokens per minute, and fewer concurrent requests does not
/// necessarily mean fewer tokens per minute; cutting on it would shrink the
/// limit for a wall that concurrency does not control.
///
/// Timeouts and connection resets are also excluded: they happened to a request
/// the model had already accepted, so they say nothing about how many more it
/// could take, and counting them would shrink the limit on every network hiccup.
fn is_downstream_overload(reason: &FailureReason) -> bool {
    match reason {
        FailureReason::RetriableHttpStatus { status: 529, .. }
        | FailureReason::NonRetriableHttpStatus { status: 529, .. } => true,
        // 503 means the gateway could not place the request with any provider:
        // the pool was empty, or every attempt failed. Either way there is no
        // capacity to dispatch into right now, so continuing at the current
        // limit just burns attempts against nothing.
        //
        // Included despite not being a saturation signal, because the increase
        // side grows on demand and a failed request still fills a slot: a model
        // whose capacity has gone away keeps filling every slot it is offered
        // and ratchets its limit up throughout the outage. Cutting is what stops
        // that, and the limit re-grows multiplicatively once capacity returns.
        FailureReason::RetriableHttpStatus { status: 503, .. }
        | FailureReason::NonRetriableHttpStatus { status: 503, .. } => true,
        FailureReason::RetriableHttpStatus { status: 429, body }
        | FailureReason::NonRetriableHttpStatus { status: 429, body } => {
            body.contains(ONWARDS_CONCURRENCY_LIMIT_CODE)
        }
        _ => false,
    }
}

fn emit_concurrency_decrease(model: &str, adjustment: ConcurrencyAdjustment, status: &str) {
    counter!("fusillade_adaptive_concurrency_decreases_total", "model" => model.to_owned())
        .increment(1);
    gauge!("fusillade_adaptive_concurrency_limit", "model" => model.to_owned())
        .set(adjustment.new_limit as f64);
    tracing::warn!(
        model,
        previous_limit = adjustment.previous_limit,
        new_limit = adjustment.new_limit,
        // Carried from the failure rather than assumed: more than one status
        // now cuts the limit, so hard-coding one would misreport the others.
        status,
        "Reduced model concurrency after downstream overload"
    );
}

fn emit_concurrency_increase(model: &str, adjustment: ConcurrencyAdjustment) {
    counter!("fusillade_adaptive_concurrency_increases_total", "model" => model.to_owned())
        .increment(1);
    gauge!("fusillade_adaptive_concurrency_limit", "model" => model.to_owned())
        .set(adjustment.new_limit as f64);
    tracing::debug!(
        model,
        previous_limit = adjustment.previous_limit,
        new_limit = adjustment.new_limit,
        "Raised model concurrency: the model used every slot without a 529"
    );
}

fn sla_dynamo_priority(deadline: chrono::DateTime<chrono::Utc>) -> i32 {
    deadline
        .timestamp()
        .saturating_neg()
        .clamp(MIN_SLA_DYNAMO_PRIORITY as i64, i32::MAX as i64) as i32
}

fn inject_dynamo_priority(body: &mut String, priority: i32) {
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(body) else {
        return;
    };
    let Some(object) = json.as_object_mut() else {
        return;
    };

    let nvext = object
        .entry("nvext")
        .or_insert_with(|| serde_json::json!({}));
    if !nvext.is_object() {
        *nvext = serde_json::json!({});
    }
    let Some(nvext_object) = nvext.as_object_mut() else {
        return;
    };

    let agent_hints = nvext_object
        .entry("agent_hints")
        .or_insert_with(|| serde_json::json!({}));
    if !agent_hints.is_object() {
        *agent_hints = serde_json::json!({});
    }
    let Some(hints_object) = agent_hints.as_object_mut() else {
        return;
    };
    hints_object.insert(
        "priority".to_string(),
        serde_json::Value::Number(priority.into()),
    );

    if let Ok(new_body) = serde_json::to_string(&json) {
        *body = new_body;
    }
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

fn claim_loop_kinds_for_mode(
    mode: DaemonMode,
    supports_batch_claims: bool,
    supports_background_claims: bool,
    background_enabled: bool,
    inject_deadline_priority: bool,
) -> Result<Vec<ClaimLoopKind>> {
    if background_enabled {
        if !supports_background_claims {
            return Err(FusilladeError::Other(anyhow::anyhow!(
                "background processing requires storage support for background claims"
            )));
        }
        if !inject_deadline_priority {
            return Err(FusilladeError::Other(anyhow::anyhow!(
                "background processing requires inject_deadline_priority=true"
            )));
        }
    }

    let mut kinds = match mode {
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
    }?;

    if background_enabled {
        match mode {
            DaemonMode::RequestOnly => kinds.push(ClaimLoopKind::BackgroundRequest),
            DaemonMode::BatchOnly => kinds.push(ClaimLoopKind::BackgroundBatch),
            DaemonMode::Both => {
                kinds.push(ClaimLoopKind::BackgroundRequest);
                if supports_batch_claims {
                    kinds.push(ClaimLoopKind::BackgroundBatch);
                }
            }
        }
    }
    Ok(kinds)
}

fn owns_archive_maintenance(claim_loop_kinds: &[ClaimLoopKind]) -> bool {
    claim_loop_kinds.contains(&ClaimLoopKind::Batch)
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

/// Run maintenance work only while the daemon remains live. Dropping the
/// future cancels an in-flight SQLx operation and its transaction.
async fn until_shutdown<T>(
    shutdown: &tokio_util::sync::CancellationToken,
    fut: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => None,
        result = fut => Some(result),
    }
}

async fn maintenance_query<T>(
    shutdown: &tokio_util::sync::CancellationToken,
    what: &'static str,
    timeout: Duration,
    fut: impl Future<Output = Result<T>>,
) -> Result<Option<T>> {
    until_shutdown(shutdown, with_query_timeout(what, timeout, fut))
        .await
        .transpose()
}

fn retained_archive_cutoffs_at(
    observed_at: chrono::DateTime<chrono::Utc>,
    dwell_secs: f64,
    cancel_grace_secs: f64,
) -> Result<RetainedResponseArchiveCutoffs> {
    let dwell = Duration::try_from_secs_f64(dwell_secs).map_err(|_| {
        FusilladeError::ValidationError(
            "batchless archive dwell must be finite and non-negative".to_string(),
        )
    })?;
    let cancel_grace = Duration::try_from_secs_f64(cancel_grace_secs).map_err(|_| {
        FusilladeError::ValidationError(
            "batchless archive cancellation grace must be finite and non-negative".to_string(),
        )
    })?;
    let dwell = chrono::Duration::from_std(dwell).map_err(|_| {
        FusilladeError::ValidationError("batchless archive dwell is out of range".to_string())
    })?;
    let cancel_grace = chrono::Duration::from_std(cancel_grace).map_err(|_| {
        FusilladeError::ValidationError(
            "batchless archive cancellation grace is out of range".to_string(),
        )
    })?;
    let terminal_before = observed_at.checked_sub_signed(dwell).ok_or_else(|| {
        FusilladeError::ValidationError(
            "batchless archive terminal cutoff is out of range".to_string(),
        )
    })?;
    let cancel_grace_before = observed_at
        .checked_sub_signed(cancel_grace)
        .ok_or_else(|| {
            FusilladeError::ValidationError(
                "batchless archive cancellation cutoff is out of range".to_string(),
            )
        })?;
    RetainedResponseArchiveCutoffs::new(observed_at, terminal_before, cancel_grace_before)
        .map_err(FusilladeError::ValidationError)
}

async fn run_weekly_archive_partition_maintenance_loop<S>(
    storage: Arc<S>,
    shutdown: tokio_util::sync::CancellationToken,
    query_timeout: Duration,
    weeks_ahead: i32,
    period: Duration,
) where
    S: DaemonStorage + 'static,
{
    loop {
        match maintenance_query(
            &shutdown,
            "batch archive partition ensure",
            query_timeout,
            storage.ensure_archive_partitions(weeks_ahead),
        )
        .await
        {
            Ok(Some((created, ahead))) => {
                gauge!("fusillade_archive_partitions_ahead").set(ahead as f64);
                if created > 0 {
                    tracing::info!(created, ahead, "Created batch archive partitions");
                }
            }
            Ok(None) => break,
            Err(error) => {
                crate::background_error!(
                    "archive_partition_ensure_failed",
                    Error,
                    error = %error,
                    "Failed to ensure batch archive partitions"
                );
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(period) => {},
            _ = shutdown.cancelled() => break,
        }
    }
}

async fn run_retained_response_readiness_loop<S>(
    storage: Arc<S>,
    shutdown: tokio_util::sync::CancellationToken,
    query_timeout: Duration,
    retention_policy: RetentionPolicy,
    retained_days_ahead: i32,
    ready: Arc<AtomicBool>,
    period: Duration,
) where
    S: DaemonStorage + 'static,
{
    loop {
        let runway_ready = match maintenance_query(
            &shutdown,
            "retained response partition ensure",
            query_timeout,
            storage.ensure_retained_response_partitions(&retention_policy, retained_days_ahead),
        )
        .await
        {
            Ok(Some(runway)) => {
                gauge!("fusillade_retained_response_partitions_ahead")
                    .set(runway.contiguous_ahead as f64);
                if runway.created > 0 {
                    tracing::info!(
                        created = runway.created,
                        ahead = runway.contiguous_ahead,
                        required = runway.required,
                        "Created retained-response partitions"
                    );
                }
                runway.is_complete()
            }
            Ok(None) => {
                gauge!("fusillade_retained_response_partitions_ahead").set(0.0);
                false
            }
            Err(error) => {
                gauge!("fusillade_retained_response_partitions_ahead").set(0.0);
                crate::background_error!(
                    "retained_response_partition_ensure_failed",
                    Error,
                    error = %error,
                    "Failed to ensure retained-response partitions"
                );
                false
            }
        };
        gauge!("fusillade_retained_response_partition_runway_ready").set(if runway_ready {
            1.0
        } else {
            0.0
        });
        ready.store(runway_ready, Ordering::Release);
        let next_tick = if runway_ready {
            period
        } else {
            period.min(Duration::from_secs(30))
        };
        tokio::select! {
            _ = tokio::time::sleep(next_tick) => {},
            _ = shutdown.cancelled() => break,
        }
    }
}

async fn run_retained_response_retirement_loop<S>(
    storage: Arc<S>,
    shutdown: tokio_util::sync::CancellationToken,
    query_timeout: Duration,
    select_new: bool,
    period: Duration,
) where
    S: DaemonStorage + 'static,
{
    loop {
        let outcome = maintenance_query(
            &shutdown,
            "retained response partition retirement",
            query_timeout,
            storage.retire_expired_response_partition(select_new),
        )
        .await;
        let next_tick = match outcome {
            Ok(Some(RetainedResponseRetirementOutcome::Retired)) => {
                counter!("fusillade_retained_response_partitions_retired_total").increment(1);
                // Each storage transaction still retires at most one exact
                // relation. A short yield drains an existing multi-day
                // backlog without turning completion into a hot loop.
                period.min(Duration::from_secs(1))
            }
            Ok(Some(RetainedResponseRetirementOutcome::Retryable)) => {
                counter!("fusillade_retained_response_partition_retirement_retries_total")
                    .increment(1);
                period.min(Duration::from_secs(30))
            }
            // PostgreSQL's UTC date remains authoritative. Polling at a
            // bounded cadence notices a new database day without relying on
            // the pod clock or sleeping a nearly full extra day.
            Ok(Some(RetainedResponseRetirementOutcome::NoCandidate)) => {
                period.min(Duration::from_secs(300))
            }
            Ok(None) => break,
            Err(_) => {
                // The storage contract intentionally returns only content-free
                // failures here. Keep logs aggregate-only even for alternate
                // backend implementations.
                crate::background_error!(
                    "retained_response_partition_retirement_failed",
                    Error,
                    "Failed to retire retained-response partition"
                );
                period.min(Duration::from_secs(30))
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(next_tick) => {},
            _ = shutdown.cancelled() => break,
        }
    }
}

async fn run_retained_response_route_cleanup_loop<S>(
    storage: Arc<S>,
    shutdown: tokio_util::sync::CancellationToken,
    query_timeout: Duration,
    limit: i64,
    period: Duration,
) where
    S: DaemonStorage + 'static,
{
    loop {
        match maintenance_query(
            &shutdown,
            "retained response route cleanup",
            query_timeout,
            storage.cleanup_retained_response_routes(limit),
        )
        .await
        {
            Ok(Some(deleted)) => {
                counter!("fusillade_retained_response_routes_cleaned_total").increment(deleted);
            }
            Ok(None) => break,
            Err(_) => {
                crate::background_error!(
                    "retained_response_route_cleanup_failed",
                    Error,
                    "Failed to clean retained-response routes"
                );
            }
        }
        // Expired-fence removal is an independent phase: a route-cleanup
        // failure above must not suppress it, and vice versa.
        match maintenance_query(
            &shutdown,
            "retained response fence cleanup",
            query_timeout,
            storage.cleanup_expired_response_fences(limit),
        )
        .await
        {
            Ok(Some(deleted)) => {
                counter!("fusillade_retained_response_fences_cleaned_total").increment(deleted);
            }
            Ok(None) => break,
            Err(_) => {
                crate::background_error!(
                    "retained_response_fence_cleanup_failed",
                    Error,
                    "Failed to clean expired retained-response fences"
                );
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(period) => {},
            _ = shutdown.cancelled() => break,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ArchiveMoverTick {
    worker: &'static str,
    batch_enabled: bool,
    batchless_enabled: bool,
    batch_limit: i64,
    batch_concurrency: usize,
    batch_dwell_secs: f64,
    batchless_dwell_secs: f64,
    cancel_grace_secs: f64,
    batchless_group_limit: i64,
    batchless_byte_limit: i64,
}

async fn run_batch_archive_phase<S>(
    storage: Arc<S>,
    shutdown: &tokio_util::sync::CancellationToken,
    query_timeout: Duration,
    tick: ArchiveMoverTick,
) where
    S: DaemonStorage + 'static,
{
    let ids = match maintenance_query(
        shutdown,
        "batch archive candidate list",
        query_timeout,
        storage.list_archivable_batches(
            tick.batch_limit,
            true,
            tick.cancel_grace_secs,
            tick.batch_dwell_secs,
        ),
    )
    .await
    {
        Ok(Some(ids)) => ids,
        Ok(None) => return,
        Err(error) => {
            crate::background_error!(
                "archive_list_failed",
                Error,
                worker = tick.worker,
                error = %error,
                "Failed to list archivable batches"
            );
            return;
        }
    };

    let mut abort_phase = false;
    for wave in ids.chunks(tick.batch_concurrency.max(1)) {
        if shutdown.is_cancelled() || abort_phase {
            break;
        }
        let results = futures::future::join_all(wave.iter().map(|batch_id| {
            let storage = storage.clone();
            async move {
                let started = std::time::Instant::now();
                let result = maintenance_query(
                    shutdown,
                    "batch archive move",
                    query_timeout,
                    storage.archive_batch(*batch_id),
                )
                .await;
                (started.elapsed(), result)
            }
        }))
        .await;
        for (elapsed, result) in results {
            match result {
                Ok(Some(ArchiveOutcome::Archived { rows })) => {
                    counter!("fusillade_archive_moves_total", "worker" => tick.worker, "outcome" => "archived").increment(1);
                    counter!("fusillade_archive_moved_rows_total", "worker" => tick.worker)
                        .increment(rows);
                    histogram!("fusillade_archive_move_duration_seconds", "worker" => tick.worker)
                        .record(elapsed.as_secs_f64());
                }
                Ok(Some(outcome)) => {
                    let label = match outcome {
                        ArchiveOutcome::Archived { .. } => unreachable!(),
                        ArchiveOutcome::SkippedNotFound => "skipped_not_found",
                        ArchiveOutcome::SkippedNotLive => "skipped_not_live",
                        ArchiveOutcome::SkippedNotFrozen => "skipped_not_frozen",
                        ArchiveOutcome::SkippedNoPartition => "skipped_no_partition",
                        ArchiveOutcome::SkippedResponseSteps => "skipped_response_steps",
                        ArchiveOutcome::SkippedRetryRaced => "skipped_retry_raced",
                    };
                    counter!("fusillade_archive_moves_total", "worker" => tick.worker, "outcome" => label).increment(1);
                    if outcome == ArchiveOutcome::SkippedNoPartition {
                        crate::background_error!(
                            "archive_partition_missing",
                            Error,
                            worker = tick.worker,
                            "Archive partition missing for a batch move"
                        );
                    }
                }
                Ok(None) => return,
                Err(error) => {
                    if !abort_phase {
                        crate::background_error!(
                            "archive_move_failed",
                            Error,
                            worker = tick.worker,
                            error = %error,
                            "Failed to archive a batch"
                        );
                    }
                    abort_phase = true;
                }
            }
        }
    }

    if let Ok(Some(backlog)) = maintenance_query(
        shutdown,
        "batch archive backlog count",
        query_timeout,
        storage.count_archivable_batches(tick.cancel_grace_secs),
    )
    .await
    {
        gauge!("fusillade_archive_backlog").set(backlog as f64);
    }
}

async fn run_batchless_archive_phase<S>(
    storage: &S,
    shutdown: &tokio_util::sync::CancellationToken,
    query_timeout: Duration,
    retention_policy: &RetentionPolicy,
    cutoffs: &RetainedResponseArchiveCutoffs,
    tick: ArchiveMoverTick,
) where
    S: DaemonStorage,
{
    let started = std::time::Instant::now();
    match maintenance_query(
        shutdown,
        "retained response archive move",
        query_timeout,
        storage.archive_terminal_batchless_responses(
            retention_policy,
            cutoffs,
            tick.batchless_group_limit,
            tick.batchless_byte_limit,
        ),
    )
    .await
    {
        Ok(Some(outcome)) => {
            counter!("fusillade_retained_response_groups_archived_total", "worker" => tick.worker)
                .increment(outcome.groups_archived);
            counter!("fusillade_retained_response_requests_archived_total", "worker" => tick.worker)
                .increment(outcome.requests_archived);
            counter!("fusillade_retained_response_steps_archived_total", "worker" => tick.worker)
                .increment(outcome.steps_archived);
            counter!("fusillade_retained_response_templates_archived_total", "worker" => tick.worker)
                .increment(outcome.templates_archived);
            counter!("fusillade_retained_response_bytes_archived_total", "worker" => tick.worker)
                .increment(outcome.bytes_archived);
            gauge!("fusillade_retained_response_archive_may_have_more", "worker" => tick.worker)
                .set(u8::from(outcome.may_have_more) as f64);
            histogram!("fusillade_retained_response_archive_duration_seconds", "worker" => tick.worker)
                .record(started.elapsed().as_secs_f64());
            if outcome.groups_archived > 0 || outcome.skipped_locked {
                tracing::info!(
                    worker = tick.worker,
                    groups_archived = outcome.groups_archived,
                    requests_archived = outcome.requests_archived,
                    steps_archived = outcome.steps_archived,
                    templates_archived = outcome.templates_archived,
                    bytes_archived = outcome.bytes_archived,
                    skipped_locked = outcome.skipped_locked,
                    may_have_more = outcome.may_have_more,
                    "Retained-response archive phase completed"
                );
            }
        }
        Ok(None) => {}
        Err(error) => {
            crate::background_error!(
                "retained_response_archive_failed",
                Error,
                worker = tick.worker,
                error = %error,
                "Failed to archive retained-response graphs"
            );
        }
    }
}

async fn run_archive_mover_tick<S>(
    storage: Arc<S>,
    shutdown: &tokio_util::sync::CancellationToken,
    query_timeout: Duration,
    retention_policy: &RetentionPolicy,
    tick: ArchiveMoverTick,
    retained_runway_ready: &AtomicBool,
) where
    S: DaemonStorage + 'static,
{
    let observed_at = chrono::Utc::now();
    if tick.batch_enabled {
        run_batch_archive_phase(storage.clone(), shutdown, query_timeout, tick).await;
    }
    if tick.batchless_enabled {
        let index_ready = match maintenance_query(
            shutdown,
            "retained response archive index readiness",
            query_timeout,
            storage.retained_response_archive_index_ready(),
        )
        .await
        {
            Ok(Some(ready)) => ready,
            Ok(None) => false,
            Err(error) => {
                crate::background_error!(
                    "retained_response_index_readiness_failed",
                    Error,
                    worker = tick.worker,
                    error = %error,
                    "Failed to inspect retained-response archive index readiness"
                );
                false
            }
        };
        if !index_ready || !retained_runway_ready.load(Ordering::Acquire) {
            gauge!("fusillade_retained_response_archive_ready", "worker" => tick.worker).set(0.0);
            return;
        }
        gauge!("fusillade_retained_response_archive_ready", "worker" => tick.worker).set(1.0);
        let cutoffs = match retained_archive_cutoffs_at(
            observed_at,
            tick.batchless_dwell_secs,
            tick.cancel_grace_secs,
        ) {
            Ok(cutoffs) => cutoffs,
            Err(error) => {
                crate::background_error!(
                    "archive_mover_invalid_cutoffs",
                    Error,
                    worker = tick.worker,
                    error = %error,
                    "Archive mover could not resolve immutable cutoffs"
                );
                return;
            }
        };
        run_batchless_archive_phase(
            storage.as_ref(),
            shutdown,
            query_timeout,
            retention_policy,
            &cutoffs,
            tick,
        )
        .await;
    }
}

fn supervise_daemon_handles(
    handles: Vec<(&'static str, tokio::task::JoinHandle<()>)>,
) -> JoinSet<(
    &'static str,
    std::result::Result<(), tokio::task::JoinError>,
)> {
    let mut children = JoinSet::new();
    for (worker, handle) in handles {
        children.spawn(async move { (worker, handle.await) });
    }
    children
}

async fn supervise_next_daemon_child(
    children: &mut JoinSet<(
        &'static str,
        std::result::Result<(), tokio::task::JoinError>,
    )>,
) -> Result<()> {
    match children.join_next().await {
        Some(Ok((worker, Ok(())))) => Err(FusilladeError::Other(anyhow::anyhow!(
            "daemon child task `{worker}` exited unexpectedly"
        ))),
        Some(Ok((worker, Err(error)))) => Err(FusilladeError::Other(anyhow::anyhow!(
            "daemon child task `{worker}` panicked: {error}"
        ))),
        Some(Err(error)) => Err(FusilladeError::Other(anyhow::anyhow!(
            "daemon child supervisor panicked: {error}"
        ))),
        None => Err(FusilladeError::Other(anyhow::anyhow!(
            "all daemon child tasks exited unexpectedly"
        ))),
    }
}

async fn drain_supervised_daemon_children(
    children: &mut JoinSet<(
        &'static str,
        std::result::Result<(), tokio::task::JoinError>,
    )>,
) -> usize {
    let mut panics = 0;
    while let Some(result) = children.join_next().await {
        match result {
            Ok((worker, Err(error))) => {
                panics += 1;
                crate::background_error!(
                    "daemon_child_task_panicked",
                    Critical,
                    worker,
                    error = %error,
                    "Daemon child task panicked during shutdown"
                );
            }
            Err(error) => {
                panics += 1;
                crate::background_error!(
                    "daemon_child_supervisor_panicked",
                    Critical,
                    error = %error,
                    "Daemon child supervisor panicked during shutdown"
                );
            }
            Ok((_, Ok(()))) => {}
        }
    }
    panics
}

#[derive(Clone, Copy)]
struct ArchiveMovementWindows {
    sweep_dwell_secs: f64,
    cancellation_grace_secs: f64,
}

impl ArchiveMovementWindows {
    const fn new(sweep_dwell_secs: f64, cancellation_grace_secs: f64) -> Self {
        Self {
            sweep_dwell_secs,
            cancellation_grace_secs,
        }
    }
}

fn validate_retention_startup(
    config: &RetentionMaintenanceConfig,
    requested_mode: DaemonMode,
    owns_archive_maintenance: bool,
    storage_supports_retained_lifecycle: bool,
    purge_interval_ms: u64,
    purge_batch_size: i64,
    movement_windows: ArchiveMovementWindows,
) -> Result<()> {
    config.policy().validate().map_err(|error| {
        FusilladeError::ValidationError(format!("invalid retention configuration: {error}"))
    })?;
    if requested_mode == DaemonMode::RequestOnly {
        return Ok(());
    }

    if requested_mode != DaemonMode::RequestOnly
        && (config.policy().expire_files || config.policy().terminal_batch_seconds.is_some())
    {
        return Err(FusilladeError::ValidationError(
            "scheduled file and batch retention is not supported by retained-response maintenance"
                .to_string(),
        ));
    }

    let batchless_policy_configured = !config.policy().batchless_seconds_by_service_tier.is_empty();
    let movement_enabled =
        config.batchless_archive_sweep_enabled() || config.batchless_archive_backfill_enabled();

    if (movement_enabled || config.retained_response_retirement_enabled())
        && !batchless_policy_configured
    {
        return Err(FusilladeError::ValidationError(
            "batchless retention policy is required for retained-response maintenance".to_string(),
        ));
    }
    if movement_enabled
        && (config.batchless_archive_groups_per_tick() <= 0
            || config.batchless_archive_bytes_per_tick() <= 0)
    {
        return Err(FusilladeError::ValidationError(
            "batchless archive group and byte budgets must be positive".to_string(),
        ));
    }
    if batchless_policy_configured && config.retained_response_partitions_days_ahead() <= 0 {
        return Err(FusilladeError::ValidationError(
            "retained-response partition runway must be positive".to_string(),
        ));
    }
    let lifecycle_active = batchless_policy_configured
        || movement_enabled
        || config.retained_response_retirement_enabled();
    if requested_mode != DaemonMode::RequestOnly && lifecycle_active && !owns_archive_maintenance {
        return Err(FusilladeError::ValidationError(
            "enabled batchless movement requires an effective batch-capable archive owner"
                .to_string(),
        ));
    }
    if requested_mode != DaemonMode::RequestOnly
        && lifecycle_active
        && owns_archive_maintenance
        && !storage_supports_retained_lifecycle
    {
        return Err(FusilladeError::ValidationError(
            "configured storage backend does not support the retained-response lifecycle"
                .to_string(),
        ));
    }

    let retention_durations = config
        .policy()
        .batchless_seconds_by_service_tier
        .values()
        .copied()
        .map(Duration::from_secs)
        .collect::<Vec<_>>();
    if movement_enabled {
        let dwell =
            Duration::try_from_secs_f64(movement_windows.sweep_dwell_secs).map_err(|_| {
                FusilladeError::ValidationError(
                    "batchless archive sweep dwell must be finite and non-negative".to_string(),
                )
            })?;
        if retention_durations
            .iter()
            .any(|retention| dwell >= *retention)
        {
            return Err(FusilladeError::ValidationError(
                "batchless archive sweep dwell must be shorter than every configured retention period"
                    .to_string(),
            ));
        }
    }
    if movement_enabled {
        let cancellation_grace = Duration::try_from_secs_f64(
            movement_windows.cancellation_grace_secs,
        )
        .map_err(|_| {
            FusilladeError::ValidationError(
                "batchless archive cancellation grace must be finite and non-negative".to_string(),
            )
        })?;
        if retention_durations
            .iter()
            .any(|retention| cancellation_grace >= *retention)
        {
            return Err(FusilladeError::ValidationError(
                "batchless archive cancellation grace must be shorter than every configured retention period"
                    .to_string(),
            ));
        }
    }
    if requested_mode != DaemonMode::RequestOnly
        && config.policy().is_enabled()
        && (purge_interval_ms == 0 || purge_batch_size < 1)
    {
        return Err(FusilladeError::ValidationError(
            "automated retention requires an enabled orphan purge and a positive purge batch size"
                .to_string(),
        ));
    }
    Ok(())
}

fn validate_retirement_capability(
    config: &RetentionMaintenanceConfig,
    storage_supports_partition_retirement: bool,
) -> Result<()> {
    if config.retained_response_retirement_enabled() && !storage_supports_partition_retirement {
        return Err(FusilladeError::ValidationError(
            "retained-response partition retirement requires an explicit session-capable maintenance pool"
                .to_string(),
        ));
    }
    Ok(())
}

fn validate_maintenance_worker_config(
    config: &DaemonConfig,
    retention: &RetentionMaintenanceConfig,
    requested_mode: DaemonMode,
    owns_archive_maintenance: bool,
) -> Result<()> {
    if requested_mode == DaemonMode::RequestOnly || !owns_archive_maintenance {
        return Ok(());
    }

    let sweep_enabled =
        config.batch_archive_sweep_enabled || retention.batchless_archive_sweep_enabled();
    if sweep_enabled && config.batch_archive_sweep_interval_ms == 0 {
        return Err(FusilladeError::ValidationError(
            "archive sweep interval must be positive when enabled".to_string(),
        ));
    }
    if config.batch_archive_sweep_enabled && config.batch_archive_sweep_moves_per_tick <= 0 {
        return Err(FusilladeError::ValidationError(
            "batch archive sweep moves per tick must be positive when enabled".to_string(),
        ));
    }

    let backfill_enabled =
        config.batch_archive_backfill_enabled || retention.batchless_archive_backfill_enabled();
    if backfill_enabled && config.batch_archive_backfill_interval_ms == 0 {
        return Err(FusilladeError::ValidationError(
            "archive backfill interval must be positive when enabled".to_string(),
        ));
    }
    if config.batch_archive_backfill_enabled {
        if config.batch_archive_backfill_moves_per_tick <= 0 {
            return Err(FusilladeError::ValidationError(
                "batch archive backfill moves per tick must be positive when enabled".to_string(),
            ));
        }
        if config.batch_archive_backfill_concurrency == 0 {
            return Err(FusilladeError::ValidationError(
                "batch archive backfill concurrency must be positive when enabled".to_string(),
            ));
        }
    }

    if config.batch_archive_sweep_enabled
        && (!config.batch_archive_sweep_dwell_secs.is_finite()
            || config.batch_archive_sweep_dwell_secs < 0.0)
    {
        return Err(FusilladeError::ValidationError(
            "batch archive sweep dwell must be finite and non-negative".to_string(),
        ));
    }
    if (sweep_enabled || backfill_enabled)
        && (!config.batch_archive_cancel_grace_secs.is_finite()
            || config.batch_archive_cancel_grace_secs < 0.0)
    {
        return Err(FusilladeError::ValidationError(
            "archive cancellation grace must be finite and non-negative".to_string(),
        ));
    }
    if config.batch_archive_partitions_weeks_ahead < 0 {
        return Err(FusilladeError::ValidationError(
            "batch archive partition runway must not be negative".to_string(),
        ));
    }
    if config.batch_finalizer_enabled {
        if config.batch_finalizer_interval_ms == 0 || config.batch_finalizer_cancelled_per_tick <= 0
        {
            return Err(FusilladeError::ValidationError(
                "batch finalizer interval and per-tick bound must be positive when enabled"
                    .to_string(),
            ));
        }
        if !config.batch_finalizer_cancelled_grace_secs.is_finite()
            || config.batch_finalizer_cancelled_grace_secs < 0.0
        {
            return Err(FusilladeError::ValidationError(
                "batch finalizer cancellation grace must be finite and non-negative".to_string(),
            ));
        }
    }
    Ok(())
}

fn daemon_config_snapshot(
    config: &DaemonConfig,
    retention: &RetentionMaintenanceConfig,
) -> serde_json::Value {
    let mut snapshot = serde_json::to_value(config).expect("Failed to serialize daemon config");
    snapshot["retention_maintenance"] = serde_json::json!({
        "policy": retention.policy(),
        "controls": {
            "batchless_archive_sweep_enabled": retention.batchless_archive_sweep_enabled(),
            "batchless_archive_backfill_enabled": retention.batchless_archive_backfill_enabled(),
            "batchless_archive_groups_per_tick": retention.batchless_archive_groups_per_tick(),
            "batchless_archive_bytes_per_tick": retention.batchless_archive_bytes_per_tick(),
            "retained_response_partitions_days_ahead": retention.retained_response_partitions_days_ahead(),
            "retained_response_retirement_enabled": retention.retained_response_retirement_enabled(),
        },
        "required_gates": ["candidate_index", "continuous_partition_runway"],
    });
    snapshot
}

fn validate_daemon_intervals(config: &DaemonConfig) -> Result<()> {
    for (name, interval_ms) in [
        ("heartbeat_interval_ms", Some(config.heartbeat_interval_ms)),
        (
            "cancellation_poll_interval_ms",
            Some(config.cancellation_poll_interval_ms),
        ),
        ("status_log_interval_ms", config.status_log_interval_ms),
        (
            "throughput_log_interval_ms",
            config.throughput_log_interval_ms,
        ),
    ] {
        if interval_ms == Some(0) {
            return Err(FusilladeError::ValidationError(format!(
                "{name} must be positive when enabled"
            )));
        }
    }
    Ok(())
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

    async fn run(self) -> Result<()> {
        self.core.run_claim_loop(ClaimLoopKind::Request).await
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

    async fn run(self) -> Result<()> {
        self.core.run_claim_loop(ClaimLoopKind::Batch).await
    }
}

/// Daemon responsible for spare-capacity batchless background requests.
struct BackgroundRequestDaemon<S, H>
where
    S: Storage + DaemonStorage,
    H: HttpClient,
{
    core: Arc<Daemon<S, H>>,
}

impl<S, H> BackgroundRequestDaemon<S, H>
where
    S: Storage + DaemonStorage + 'static,
    H: HttpClient + 'static,
{
    fn new(core: Arc<Daemon<S, H>>) -> Self {
        Self { core }
    }

    async fn run(self) -> Result<()> {
        self.core
            .run_background_claim_loop(ClaimLoopKind::BackgroundRequest)
            .await
    }
}

/// Daemon responsible for spare-capacity file-backed background batches.
struct BackgroundBatchDaemon<S, H>
where
    S: Storage + DaemonStorage,
    H: HttpClient,
{
    core: Arc<Daemon<S, H>>,
}

impl<S, H> BackgroundBatchDaemon<S, H>
where
    S: Storage + DaemonStorage + 'static,
    H: HttpClient + 'static,
{
    fn new(core: Arc<Daemon<S, H>>) -> Self {
        Self { core }
    }

    async fn run(self) -> Result<()> {
        self.core
            .run_background_claim_loop(ClaimLoopKind::BackgroundBatch)
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
    retention_maintenance: RetentionMaintenanceConfig,
    /// Per-claim processing hook. Defaults to [`DefaultRequestProcessor`],
    /// which preserves the existing fire-and-store pipeline byte-for-byte.
    /// Override via [`Daemon::with_processor`] to inject custom orchestration
    /// (e.g. multi-step Open Responses loops) without changing any other
    /// daemon behavior.
    processor: Arc<dyn RequestProcessor<S, H>>,
    requests_in_flight: Arc<dashmap::DashMap<String, AtomicUsize>>,
    /// Per-model AIMD state. The configured concurrency remains the hard
    /// ceiling; HTTP 529 responses reduce this daemon's effective ceiling and
    /// successful responses recover it gradually.
    adaptive_concurrency: Arc<AdaptiveConcurrencyController>,
    /// Suppresses claiming while this process is near its own memory limit.
    /// `None` when disabled by config or when there is no readable cgroup limit.
    /// The adaptive controller grows on success and nothing upstream reports
    /// local memory pressure, so this is the only bound that corresponds to
    /// running out of memory.
    memory_gate: Option<Arc<MemoryGate>>,
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
    /// Serializes foreground request and batch claim loops while they compute
    /// available capacity, claim rows, and increment foreground in-flight
    /// counters. Background workers never acquire this mutex.
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
        let adaptive_concurrency = Arc::new(AdaptiveConcurrencyController::new(
            config.adaptive_growth_factor,
            config.adaptive_cut_factor,
        ));
        let memory_gate = MemoryGate::new(
            config.memory_gate_high_fraction,
            config.memory_gate_low_fraction,
            Box::new(CgroupMemorySource),
        )
        .map(Arc::new);

        // The controller treats a model's configured limit as a starting point
        // and grows past it, so with it on nothing else bounds in-flight work.
        // Running that combination is how a pod OOMs itself, so refuse it: fall
        // back to the configured limits, which is the behaviour without the
        // controller and is known to be survivable.
        let adaptive_concurrency_enabled = if adaptive_concurrency_permitted(
            config.adaptive_concurrency,
            memory_gate.is_some(),
        ) {
            config.adaptive_concurrency
        } else {
            crate::background_error!(
                "adaptive_concurrency_without_memory_gate",
                Critical,
                "adaptive_concurrency is on but no memory gate is configured; refusing to enable \
                 it. Set memory_gate_high_fraction (and keep memory_gate_low_fraction below it). \
                 Running at configured per-model limits instead."
            );
            false
        };
        let config = DaemonConfig {
            should_retry,
            adaptive_concurrency: adaptive_concurrency_enabled,
            ..config
        };

        Self {
            daemon_id: DaemonId::from(uuid::Uuid::new_v4()),
            storage,
            http_client,
            config,
            retention_maintenance: RetentionMaintenanceConfig::default(),
            processor: Arc::new(DefaultRequestProcessor),
            requests_in_flight: Arc::new(dashmap::DashMap::new()),
            adaptive_concurrency,
            memory_gate,
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

    /// Install retained-response maintenance controls without changing the
    /// source-compatible serialized daemon configuration.
    pub fn with_retention_maintenance(mut self, config: RetentionMaintenanceConfig) -> Self {
        self.retention_maintenance = config;
        self
    }

    /// Validate the complete startup structure without performing I/O or
    /// spawning children. Concrete runtimes call this before returning a
    /// leader handle; [`Daemon::run_with_mode`] repeats it defensively for
    /// direct library users.
    pub(crate) fn validate_startup(&self, mode: DaemonMode) -> Result<()> {
        validate_daemon_intervals(&self.config)?;
        let claim_loop_kinds = claim_loop_kinds_for_mode(
            mode,
            self.storage.supports_batch_claims(),
            self.storage.supports_background_claims(),
            self.config.background_concurrency_limit > 0,
            self.config.inject_deadline_priority,
        )?;
        let owns_archive_maintenance = owns_archive_maintenance(&claim_loop_kinds);
        validate_maintenance_worker_config(
            &self.config,
            &self.retention_maintenance,
            mode,
            owns_archive_maintenance,
        )?;
        if owns_archive_maintenance {
            validate_retirement_capability(
                &self.retention_maintenance,
                self.storage
                    .supports_retained_response_partition_retirement(),
            )?;
        }
        validate_retention_startup(
            &self.retention_maintenance,
            mode,
            owns_archive_maintenance,
            self.storage.supports_retained_response_lifecycle(),
            self.config.purge_interval_ms,
            self.config.purge_batch_size,
            ArchiveMovementWindows::new(
                self.config.batch_archive_sweep_dwell_secs,
                self.config.batch_archive_cancel_grace_secs,
            ),
        )
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

    /// The concurrency ceiling in force for a model: the controller's discovered
    /// limit, or the configured value when the controller is off.
    fn effective_model_limit(&self, model: &str, configured_limit: usize) -> usize {
        if self.config.adaptive_concurrency {
            self.adaptive_concurrency.limit(model, configured_limit)
        } else {
            configured_limit
        }
    }

    /// Total in-flight across every model, for the process-wide ceiling.
    ///
    /// Summed from the live counters rather than from the configured model list,
    /// so escalation traffic and models removed from config still count against
    /// the budget they are actually consuming.
    fn total_in_flight(&self) -> usize {
        self.requests_in_flight
            .iter()
            .map(|entry| entry.value().load(Ordering::Relaxed))
            .sum()
    }

    /// Whether local memory pressure should suppress claiming this cycle.
    ///
    /// Checked before per-model capacity is computed, because when it bites the
    /// answer is "nothing, from any model" - unlike the total in-flight cap,
    /// which scales models down proportionally.
    fn memory_pressure_blocks_claiming(&self) -> bool {
        self.memory_gate
            .as_ref()
            .is_some_and(|gate| gate.should_block(self.total_in_flight()))
    }

    fn available_capacity(&self) -> HashMap<String, usize> {
        if self.memory_pressure_blocks_claiming() {
            return HashMap::new();
        }
        let capacities: HashMap<String, usize> = self
            .config
            .model_concurrency_limits
            .iter()
            .filter_map(|entry| {
                let model = entry.key().clone();
                let configured_limit = *entry.value();
                let in_flight = self
                    .requests_in_flight
                    .get(&model)
                    .map(|e| e.value().load(Ordering::Relaxed))
                    .unwrap_or(0);
                let limit = self.effective_model_limit(&model, configured_limit);
                let available =
                    adaptive_concurrency::available_capacity_for_model(limit, in_flight);
                (available > 0).then_some((model, available))
            })
            .collect();

        capacities
    }

    fn background_available_capacity(&self) -> HashMap<String, usize> {
        if self.memory_pressure_blocks_claiming() {
            return HashMap::new();
        }
        let background_limit = self.config.background_concurrency_limit;
        let capacities: HashMap<String, usize> = self
            .config
            .model_concurrency_limits
            .iter()
            .filter_map(|entry| {
                let model = entry.key().clone();
                let configured_limit = *entry.value();
                let ordinary_limit = self.effective_model_limit(&model, configured_limit);
                let in_flight = self
                    .requests_in_flight
                    .get(&model)
                    .map(|count| count.value().load(Ordering::Relaxed))
                    .unwrap_or(0);
                let available = background_capacity(ordinary_limit, background_limit, in_flight);
                (available > 0).then_some((model, available))
            })
            .collect();

        // Background work occupies the same memory as foreground work, so it
        // has to answer to the same process-wide ceiling.
        capacities
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

    async fn run_background_claim_loop(self: Arc<Self>, kind: ClaimLoopKind) -> Result<()> {
        debug_assert!(kind.is_background());
        let mut join_set: JoinSet<Result<()>> = JoinSet::new();
        let loop_name = match kind {
            ClaimLoopKind::BackgroundRequest => "background_request_daemon",
            ClaimLoopKind::BackgroundBatch => "background_batch_daemon",
            _ => unreachable!("foreground kind passed to background claim loop"),
        };
        let interval_ms = kind.claim_interval_ms(&self.config);

        tracing::info!(
            daemon_id = %self.daemon_id,
            loop_name,
            interval_ms,
            "Claim loop started"
        );

        let mut consecutive_claim_failures: u32 = 0;

        let run_result = loop {
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

            // Background reads foreground capacity but never reserves it and
            // never waits for the foreground claim mutex.
            let available_capacity = self.background_available_capacity();
            if available_capacity.is_empty() {
                tracing::trace!(
                    loop_name,
                    "No foreground headroom available for any model, skipping background claim"
                );
                continue;
            }

            let total_capacity: usize = available_capacity.values().sum();
            if kind.emits_legacy_claim_metrics() {
                gauge!("fusillade_claim_capacity").set(total_capacity as f64);
            }
            gauge!("fusillade_claim_capacity", "daemon" => loop_name).set(total_capacity as f64);

            let user_active_counts = self.user_active_counts();
            let claim_start = std::time::Instant::now();
            let claim_timeout = Duration::from_millis(self.config.claim_query_timeout_ms);
            let claim_size = kind.claim_size(&self.config);
            let claim_result = match kind {
                ClaimLoopKind::BackgroundRequest => {
                    with_query_timeout(
                        "background batchless claim query",
                        claim_timeout,
                        self.storage.claim_background_batchless_requests(
                            claim_size,
                            self.daemon_id,
                            &available_capacity,
                            &user_active_counts,
                        ),
                    )
                    .await
                }
                ClaimLoopKind::BackgroundBatch => {
                    with_query_timeout(
                        "background batch claim query",
                        claim_timeout,
                        self.storage.claim_background_batch_requests(
                            claim_size,
                            self.config.batch_claim_batch_size,
                            self.daemon_id,
                            &available_capacity,
                            &user_active_counts,
                        ),
                    )
                    .await
                }
                _ => unreachable!("foreground kind passed to background claim loop"),
            };

            let claimed = match claim_result {
                Ok(claimed) => {
                    consecutive_claim_failures = 0;
                    claimed
                }
                Err(e) => {
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

            if kind.emits_legacy_claim_metrics() {
                histogram!("fusillade_claim_duration_seconds")
                    .record(claim_start.elapsed().as_secs_f64());
                histogram!("fusillade_claim_size").record(claimed.len() as f64);
            }
            histogram!("fusillade_claim_duration_seconds", "daemon" => loop_name)
                .record(claim_start.elapsed().as_secs_f64());
            histogram!("fusillade_claim_size", "daemon" => loop_name).record(claimed.len() as f64);

            tracing::debug!(
                loop_name,
                claimed_count = claimed.len(),
                "Claimed requests from storage"
            );

            let prepared = self.prepare_claimed_requests(claimed, kind);
            self.dispatch_claimed_requests(&mut join_set, prepared, kind);
        };
        join_set.abort_all();
        while join_set.join_next().await.is_some() {}
        run_result
    }

    async fn run_claim_loop(self: Arc<Self>, kind: ClaimLoopKind) -> Result<()> {
        debug_assert!(kind.uses_foreground_accounting());
        let mut join_set: JoinSet<Result<()>> = JoinSet::new();
        let loop_name = match kind {
            ClaimLoopKind::Request => "request_daemon",
            ClaimLoopKind::Batch => "batch_daemon",
            _ => unreachable!("background kind passed to foreground claim loop"),
        };
        let interval_ms = kind.claim_interval_ms(&self.config);

        tracing::info!(
            daemon_id = %self.daemon_id,
            loop_name,
            interval_ms,
            "Claim loop started"
        );

        let mut consecutive_claim_failures: u32 = 0;

        let run_result = loop {
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
            let available_capacity = self.available_capacity();
            if available_capacity.is_empty() {
                tracing::trace!(
                    loop_name,
                    "No capacity available for any model, skipping claim"
                );
                continue;
            }

            let total_capacity: usize = available_capacity.values().sum();
            // Dual-emit: keep the legacy unlabeled series alive alongside the
            // new per-daemon one so existing dashboards/alerts survive the
            // split (deprecation window).
            gauge!("fusillade_claim_capacity").set(total_capacity as f64);
            gauge!("fusillade_claim_capacity", "daemon" => loop_name).set(total_capacity as f64);

            let user_active_counts = self.user_active_counts();
            let leak_cooldown = if kind == ClaimLoopKind::Request {
                self.leak_cooldown()
            } else {
                std::collections::HashSet::new()
            };

            let claim_start = std::time::Instant::now();
            let claim_timeout = Duration::from_millis(self.config.claim_query_timeout_ms);
            let claim_result = match kind {
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
                    .await
                }
                ClaimLoopKind::Batch => {
                    with_query_timeout(
                        "batch claim query",
                        claim_timeout,
                        self.storage.claim_batch_requests(
                            kind.claim_size(&self.config),
                            self.config.batch_claim_batch_size,
                            self.daemon_id,
                            &available_capacity,
                            &user_active_counts,
                        ),
                    )
                    .await
                }
                _ => unreachable!("background kind passed to foreground claim loop"),
            };

            let claimed = match claim_result {
                Ok(claimed) => {
                    consecutive_claim_failures = 0;
                    claimed
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
            histogram!("fusillade_claim_duration_seconds")
                .record(claim_start.elapsed().as_secs_f64());
            histogram!("fusillade_claim_duration_seconds", "daemon" => loop_name)
                .record(claim_start.elapsed().as_secs_f64());
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

            self.grow_saturated_models(&claimed, &available_capacity);

            let prepared = self.prepare_claimed_requests(claimed, kind);
            self.dispatch_claimed_requests(&mut join_set, prepared, kind);
        };
        join_set.abort_all();
        while join_set.join_next().await.is_some() {}
        run_result
    }

    /// Raise the limit for every model that just used all of it.
    ///
    /// We offer each model a number of slots and see how many rows come back. If
    /// it took all of them, it had more work queued than we allowed through, so
    /// giving it more slots next time will actually be used. If it took fewer,
    /// it ran out of work (or the claim loop could not keep up) and a bigger
    /// limit would sit unused until a burst arrived and dispatched the lot at
    /// once.
    ///
    /// One known blind spot: when `claim_batch_size` trims a claim that several
    /// models were competing for, the trimmed model looks like it ran out of
    /// work and misses a raise. It errs toward not raising, and goes away once
    /// claim size is comfortably above the limits in play.
    fn grow_saturated_models(
        &self,
        claimed: &[Request<Claimed>],
        available_capacity: &HashMap<String, usize>,
    ) {
        if !self.config.adaptive_concurrency {
            return;
        }

        let mut claimed_per_model: HashMap<&str, usize> = HashMap::new();
        for request in claimed {
            *claimed_per_model
                .entry(request.data.model.as_str())
                .or_default() += 1;
        }

        for (model, offered) in available_capacity {
            if *offered == 0 {
                continue;
            }
            if claimed_per_model.get(model.as_str()).copied().unwrap_or(0) < *offered {
                continue;
            }
            if let Some(adjustment) = self.adaptive_concurrency.try_grow(model) {
                emit_concurrency_increase(model, adjustment);
            }
        }
    }

    fn prepare_claimed_requests(
        &self,
        claimed: Vec<Request<Claimed>>,
        kind: ClaimLoopKind,
    ) -> Vec<PreparedRequest> {
        let mut prepared: Vec<_> = claimed
            .into_iter()
            .map(|request| PreparedRequest {
                capacity_model: request.data.model.clone(),
                request,
            })
            .collect();

        for prepared_request in &mut prepared {
            let request = &mut prepared_request.request;
            if kind.is_background() {
                continue;
            }
            let Some(batch_expires_at) = request.state.batch_expires_at else {
                continue;
            };
            if let Some(config) = self.config.model_escalations.get(&request.data.model) {
                let time_remaining = batch_expires_at - chrono::Utc::now();
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

        for prepared_request in &mut prepared {
            let request = &mut prepared_request.request;
            let priority = if kind.is_background() {
                BACKGROUND_DYNAMO_PRIORITY
            } else if self.config.inject_deadline_priority {
                let Some(deadline) = request.state.batch_expires_at else {
                    continue;
                };
                sla_dynamo_priority(deadline)
            } else {
                continue;
            };
            inject_dynamo_priority(&mut request.data.body, priority);
        }

        prepared
    }

    fn dispatch_claimed_requests(
        self: &Arc<Self>,
        join_set: &mut JoinSet<Result<()>>,
        claimed: Vec<PreparedRequest>,
        kind: ClaimLoopKind,
    ) {
        let mut by_model: HashMap<String, Vec<_>> = HashMap::new();
        for prepared_request in claimed {
            let model = prepared_request.request.data.model.clone();
            by_model.entry(model).or_default().push(prepared_request);
        }

        tracing::debug!(
            models = by_model.len(),
            total_requests = by_model.values().map(|v| v.len()).sum::<usize>(),
            "Grouped requests by model"
        );

        for (model, requests) in by_model {
            tracing::debug!(model = %model, count = requests.len(), "Processing requests for model");

            for prepared_request in requests {
                let PreparedRequest {
                    request,
                    capacity_model,
                } = prepared_request;
                let request_id = request.data.id;
                let batch_id = request.data.batch_id;

                tracing::trace!(
                    request_id = %request_id,
                    batch_id = ?batch_id,
                    model = %model,
                    "Spawning processing task"
                );

                let model_clone = model.clone();
                let capacity_model_clone = capacity_model.clone();
                let user_id = request.data.created_by.clone();
                let is_background = kind.is_background();
                let uses_foreground_accounting = kind.uses_foreground_accounting();
                let completion_window = if is_background {
                    "background".to_string()
                } else {
                    request
                        .data
                        .batch_metadata
                        .get("completion_window")
                        .cloned()
                        .unwrap_or_default()
                };

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
                let adaptive_concurrency = self.adaptive_concurrency.clone();
                let model_concurrency_limits = self.config.model_concurrency_limits.clone();
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

                // Record which version of the limit this request is being sent
                // under, so that if it comes back 529 we can tell whether that
                // is news or an echo of an overload we already reacted to.
                //
                // Background work is left out. It runs on top of the foreground
                // limit rather than inside it, and only when foreground is
                // quiet, so a background rejection means background overflowed -
                // reacting to it would shrink the SLA-bearing traffic because
                // the spare-capacity traffic bounced.
                let control_generation =
                    (!is_background && self.config.adaptive_concurrency).then(|| {
                        let seed = model_concurrency_limits
                            .get(&capacity_model_clone)
                            .map(|limit| *limit)
                            .unwrap_or(0);
                        adaptive_concurrency.stamp(&capacity_model_clone, seed)
                    });

                if is_background {
                    gauge!("fusillade_background_requests_in_flight", "model" => model_clone.clone())
                        .increment(1.0);
                } else {
                    requests_in_flight
                        .entry(capacity_model_clone.clone())
                        .or_default()
                        .fetch_add(1, Ordering::Relaxed);
                    gauge!("fusillade_requests_in_flight", "model" => capacity_model_clone.clone())
                        .increment(1.0);

                    user_requests_in_flight
                        .entry(user_id.clone())
                        .or_default()
                        .fetch_add(1, Ordering::Relaxed);
                    gauge!("fusillade_user_requests_in_flight", "user" => user_id.clone(), "completion_window" => completion_window.clone())
                        .increment(1.0);
                }

                let process_span = tracing::info_span!(
                    parent: tracing::Span::none(),
                    "fusillade.process_request",
                    trace_id = tracing::field::Empty,
                    otel.name = "fusillade.process_request",
                    request_id = %request_id,
                    batch_id = ?batch_id,
                    model = %model,
                    capacity_model = %capacity_model,
                    outcome = tracing::field::Empty,
                );

                join_set.spawn(async move {
                    let span = tracing::Span::current();
                    let sc = span.context().span().span_context().clone();
                    if sc.is_valid() {
                        span.record("trace_id", tracing::field::display(sc.trace_id()));
                    }

                    let processing_start = std::time::Instant::now();
                    let model_for_guard = capacity_model_clone.clone();
                    let user_for_guard = user_id.clone();
                    let cw_for_guard = completion_window.clone();
                    let in_flight_for_guard = requests_in_flight.clone();
                    let user_in_flight_for_guard = user_requests_in_flight.clone();
                    let background_for_guard = is_background;
                    let foreground_accounting_for_guard = uses_foreground_accounting;
                    let background_model_for_guard = model_clone.clone();
                    let _guard = scopeguard::guard((), move |_| {
                        if background_for_guard {
                            gauge!("fusillade_background_requests_in_flight", "model" => background_model_for_guard).decrement(1.0);
                        } else if foreground_accounting_for_guard {
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
                        }
                    });

                    let batch_expires_at = request.state.batch_expires_at;
                    let retry_attempt_at_completion = request.state.retry_attempt;
                    let owning_daemon_id = request.state.daemon_id;

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

                    let completion_result = processor
                        .process(
                            request,
                            http_client,
                            storage.as_ref(),
                            should_retry.clone(),
                            cancellation,
                        )
                        .await;

                    match completion_result {
                        Ok(RequestCompletionResult::Completed(completed)) => {
                            tracing::Span::current().record("outcome", "completed");
                            // Deliberately nothing for the concurrency
                            // controller here. Raising the limit every time a
                            // request succeeds would push a model with five
                            // requests of work up to a limit of thousands, since
                            // all five keep succeeding. Raises happen in the
                            // claim loop instead, where we can see whether the
                            // model actually wanted the slots.
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

                            if let Some(batch_expires_at) = batch_expires_at {
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
                            }
                            Ok(())
                        }
                        Ok(RequestCompletionResult::Failed(failed)) => {
                            tracing::Span::current().record("outcome", "failed");
                            if is_downstream_overload(&failed.state.reason)
                                && let Some(generation) = control_generation
                                && let Some(adjustment) = adaptive_concurrency
                                    .record_overload(&capacity_model_clone, generation)
                            {
                                emit_concurrency_decrease(
                                    &capacity_model_clone,
                                    adjustment,
                                    &failed.state.reason.status_code_label(),
                                );
                            }
                            let retry_attempt = failed.state.retry_attempt;
                            let reason_label = failed.state.reason.metric_label();
                            let status_code_label = failed.state.reason.status_code_label();
                            if failed.state.reason.is_retriable() {
                                match failed.can_retry(retry_attempt, retry_config.clone()) {
                                    Ok(pending) => {
                                        let rescheduled = storage
                                            .reschedule_for_retry(
                                                request_id,
                                                owning_daemon_id,
                                                pending.state.retry_attempt,
                                                pending.state.not_before,
                                            )
                                            .await?;
                                        if rescheduled {
                                            // `reason`/`status_code` matter more
                                            // than they look: a retried failure
                                            // never lands in
                                            // `fusillade_requests_completed_total`
                                            // (that only records terminal
                                            // outcomes), so without them a
                                            // sustained stream of rejections is
                                            // invisible in metrics, and there is
                                            // no way to tell an upstream 529
                                            // from a 429 at the proxy's own
                                            // concurrency limit.
                                            counter!(
                                                "fusillade_requests_retried_total",
                                                "model" => model_clone.clone(),
                                                "attempt" => (retry_attempt + 1).to_string(),
                                                "reason" => reason_label,
                                                "status_code" => status_code_label.clone()
                                            )
                                            .increment(1);
                                            tracing::info!(
                                                request_id = %request_id,
                                                batch_id = ?batch_id,
                                                retry_attempt = retry_attempt + 1,
                                                "request.retry_persisted"
                                            );
                                        } else {
                                            counter!(
                                                "fusillade_requests_retry_lost_ownership_total",
                                                "model" => model_clone.clone()
                                            )
                                            .increment(1);
                                            tracing::warn!(
                                                request_id = %request_id,
                                                batch_id = ?batch_id,
                                                retry_attempt = retry_attempt + 1,
                                                "request.retry_skipped_lost_ownership"
                                            );
                                        }
                                        return Ok(());
                                    }
                                    Err(failed) => {
                                        storage.persist(&*failed).await?;
                                        requests_failed.fetch_add(1, Ordering::Relaxed);
                                        user_throughput.entry(user_id.clone()).or_insert_with(|| UserThroughputStats {
                                            completed: AtomicU64::new(0),
                                            failed: AtomicU64::new(0),
                                        }).failed.fetch_add(1, Ordering::Relaxed);
                                        counter!("fusillade_requests_completed_total", "model" => model_clone.clone(), "status" => "failed", "reason" => failed.state.reason.metric_label(), "status_code" => failed.state.reason.status_code_label(), "completion_window" => completion_window.clone()).increment(1);
                                        counter!("fusillade_user_requests_completed_total", "user" => user_id.clone(), "status" => "failed", "completion_window" => completion_window.clone()).increment(1);
                                        histogram!("fusillade_request_duration_seconds", "model" => model_clone.clone(), "status" => "failed")
                                            .record(processing_start.elapsed().as_secs_f64());
                                        if let Some(batch_expires_at) = batch_expires_at
                                            && failed.state.failed_at > batch_expires_at
                                        {
                                            counter!("fusillade_requests_completed_after_sla_total", "model" => model_clone.clone(), "status" => "failed", "completion_window" => completion_window.clone()).increment(1);
                                            tracing::warn!(
                                                request_id = %request_id,
                                                batch_id = ?batch_id,
                                                "Request failed permanently after SLA"
                                            );
                                        }
                                        tracing::warn!(
                                            request_id = %request_id,
                                            batch_id = ?batch_id,
                                            retry_attempt,
                                            failure_reason = %failed.state.reason.metric_label(),
                                            error = %failed.state.reason.to_error_message(),
                                            "request.terminal_failure"
                                        );
                                    }
                                }
                            } else {
                                requests_failed.fetch_add(1, Ordering::Relaxed);
                                user_throughput.entry(user_id.clone()).or_insert_with(|| UserThroughputStats {
                                    completed: AtomicU64::new(0),
                                    failed: AtomicU64::new(0),
                                }).failed.fetch_add(1, Ordering::Relaxed);
                                counter!("fusillade_requests_completed_total", "model" => model_clone.clone(), "status" => "failed", "reason" => failed.state.reason.metric_label(), "status_code" => failed.state.reason.status_code_label(), "completion_window" => completion_window.clone()).increment(1);
                                counter!("fusillade_user_requests_completed_total", "user" => user_id.clone(), "status" => "failed", "completion_window" => completion_window.clone()).increment(1);
                                histogram!("fusillade_request_duration_seconds", "model" => model_clone.clone(), "status" => "failed")
                                    .record(processing_start.elapsed().as_secs_f64());
                                if let Some(batch_expires_at) = batch_expires_at
                                    && failed.state.failed_at > batch_expires_at
                                {
                                    counter!("fusillade_requests_completed_after_sla_total", "model" => model_clone.clone(), "status" => "failed", "completion_window" => completion_window.clone()).increment(1);
                                    tracing::warn!(
                                        request_id = %request_id,
                                        batch_id = ?batch_id,
                                        "Request failed with non-retriable error after SLA"
                                    );
                                }
                                tracing::warn!(
                                    request_id = %request_id,
                                    batch_id = ?batch_id,
                                    failure_reason = %reason_label,
                                    error = %failed.state.reason.to_error_message(),
                                    "request.terminal_failure"
                                );
                            }
                            Ok(())
                        }
                        Ok(RequestCompletionResult::Canceled(_canceled)) => {
                            tracing::Span::current().record("outcome", "canceled");
                            counter!("fusillade_requests_cancelled_total", "model" => model_clone.clone()).increment(1);
                            // Keep the pre-split counter shape alive so existing
                            // dashboards/alerts on completed_total{status="cancelled"}
                            // don't silently break (deprecation window).
                            counter!("fusillade_requests_completed_total", "model" => model_clone.clone(), "status" => "cancelled", "completion_window" => completion_window.clone()).increment(1);
                            counter!("fusillade_user_requests_completed_total", "user" => user_id.clone(), "status" => "cancelled", "completion_window" => completion_window.clone()).increment(1);
                            Ok(())
                        }
                        Err(FusilladeError::Shutdown) => {
                            // Expected during daemon shutdown — treat as a clean
                            // exit so poll_processing_tasks doesn't log it as a
                            // background task failure.
                            tracing::Span::current().record("outcome", "shutdown");
                            Ok(())
                        }
                        Err(e) => {
                            tracing::Span::current().record("outcome", "error");
                            Err(e)
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
        self.validate_startup(mode)?;

        // Validate the configured claim topology before registering the daemon
        // or spawning any maintenance tasks. Background workers read this
        // process's foreground counters but run independently of its claim
        // mutex.
        let supports_batch_claims = self.storage.supports_batch_claims();
        let supports_background_claims = self.storage.supports_background_claims();
        let background_enabled = self.config.background_concurrency_limit > 0;
        let claim_loop_kinds = claim_loop_kinds_for_mode(
            mode,
            supports_batch_claims,
            supports_background_claims,
            background_enabled,
            self.config.inject_deadline_priority,
        )?;
        let owns_archive_maintenance = owns_archive_maintenance(&claim_loop_kinds);

        // Register daemon in database
        let daemon_record = DaemonRecord {
            data: DaemonData {
                id: self.daemon_id,
                hostname: get_hostname(),
                pid: get_pid(),
                version: get_version(),
                config_snapshot: daemon_config_snapshot(&self.config, &self.retention_maintenance),
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
        let mut daemon_handles: Vec<(&'static str, tokio::task::JoinHandle<()>)> = Vec::new();

        // Spawn periodic heartbeat task
        let storage = self.storage.clone();
        let requests_in_flight = self.requests_in_flight.clone();
        let requests_processed = self.requests_processed.clone();
        let requests_failed = self.requests_failed.clone();
        let daemon_id = self.daemon_id;
        let heartbeat_interval_ms = self.config.heartbeat_interval_ms;
        let heartbeat_query_timeout =
            Duration::from_millis(heartbeat_interval_ms.saturating_mul(4));
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
                        match with_query_timeout(
                            "heartbeat query",
                            heartbeat_query_timeout,
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
                        if let Err(e) = with_query_timeout(
                            "daemon shutdown query",
                            heartbeat_query_timeout,
                            daemon_record.shutdown(storage.as_ref()),
                        ).await {
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
        daemon_handles.push(("heartbeat", heartbeat_handle));

        // Spawn periodic status logging task if configured
        if let Some(interval_ms) = self.config.status_log_interval_ms {
            let requests_in_flight = self.requests_in_flight.clone();
            let daemon_id = self.daemon_id;
            let shutdown_token = self.shutdown_token.clone();
            let handle = tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
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
                        _ = shutdown_token.cancelled() => break,
                    }
                }
            });
            daemon_handles.push(("status_logger", handle));
        }

        // Spawn periodic per-user throughput emission task if configured
        if let Some(interval_ms) = self.config.throughput_log_interval_ms {
            let user_throughput = self.user_throughput.clone();
            let user_requests_in_flight = self.user_requests_in_flight.clone();
            let daemon_id = self.daemon_id;
            let shutdown_token = self.shutdown_token.clone();
            let handle = tokio::spawn(async move {
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
            daemon_handles.push(("throughput_logger", handle));
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

        let cancellation_poll_handle = tokio::spawn(async move {
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
        daemon_handles.push(("cancellation_poll", cancellation_poll_handle));

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

            let handle = tokio::spawn(async move {
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
                        match maintenance_query(
                            &shutdown_token,
                            "orphan purge query",
                            purge_query_timeout,
                            storage.purge_orphaned_rows(purge_batch_size),
                        )
                        .await
                        {
                            Ok(Some(0)) => break,
                            Ok(Some(deleted)) => {
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
                            Ok(None) => return,
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
                        match maintenance_query(
                            &shutdown_token,
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
                            Ok(Some(0)) => break,
                            Ok(Some(deleted)) => {
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
                            Ok(None) => return,
                            Err(e) => {
                                counter!("fusillade_purge_errors_total").increment(1);
                                tracing::error!(error = %e, "Failed to purge model_filters events");
                                break;
                            }
                        }
                    }
                }
            });
            daemon_handles.push(("purge", handle));
        }

        let mut claim_daemons: JoinSet<Result<()>> = JoinSet::new();

        if mode == DaemonMode::Both && !supports_batch_claims {
            tracing::info!(
                daemon_id = %self.daemon_id,
                "Storage backend does not support batch claims; running request-only"
            );
        }

        // ---- Archive maintenance + movers ----
        // Exactly the effective batch owner maintains both partition families
        // and runs both mover phases. An explicit request-only process never
        // performs DDL or movement even when it shares the same opaque config.
        let query_timeout = Duration::from_millis(self.config.claim_query_timeout_ms);
        let retention_policy = self.retention_maintenance.policy().clone();
        let retained_runway_ready = Arc::new(AtomicBool::new(false));
        if owns_archive_maintenance {
            let weekly_handle = tokio::spawn(run_weekly_archive_partition_maintenance_loop(
                self.storage.clone(),
                self.shutdown_token.clone(),
                query_timeout,
                self.config.batch_archive_partitions_weeks_ahead,
                Duration::from_secs(86_400),
            ));
            daemon_handles.push(("archive_partition_maintenance", weekly_handle));

            if !retention_policy
                .batchless_seconds_by_service_tier
                .is_empty()
            {
                let retained_handle = tokio::spawn(run_retained_response_readiness_loop(
                    self.storage.clone(),
                    self.shutdown_token.clone(),
                    query_timeout,
                    retention_policy.clone(),
                    self.retention_maintenance
                        .retained_response_partitions_days_ahead(),
                    retained_runway_ready.clone(),
                    Duration::from_secs(86_400),
                ));
                daemon_handles.push(("retained_response_readiness", retained_handle));
            }

            if self
                .storage
                .supports_retained_response_partition_retirement()
            {
                let retirement_handle = tokio::spawn(run_retained_response_retirement_loop(
                    self.storage.clone(),
                    self.shutdown_token.clone(),
                    query_timeout,
                    self.retention_maintenance
                        .retained_response_retirement_enabled(),
                    Duration::from_secs(86_400),
                ));
                daemon_handles.push(("retained_response_retirement", retirement_handle));
            }

            if self.storage.supports_retained_response_route_cleanup()
                && self.config.purge_interval_ms > 0
                && self.config.purge_batch_size > 0
            {
                let route_cleanup_handle = tokio::spawn(run_retained_response_route_cleanup_loop(
                    self.storage.clone(),
                    self.shutdown_token.clone(),
                    query_timeout,
                    self.config.purge_batch_size,
                    Duration::from_millis(self.config.purge_interval_ms),
                ));
                daemon_handles.push(("retained_response_route_cleanup", route_cleanup_handle));
            }

            for (
                worker,
                batch_enabled,
                batchless_configured,
                interval_ms,
                batch_limit,
                batch_dwell_secs,
                batch_concurrency,
            ) in [
                (
                    "sweep",
                    self.config.batch_archive_sweep_enabled,
                    self.retention_maintenance.batchless_archive_sweep_enabled(),
                    self.config.batch_archive_sweep_interval_ms,
                    self.config.batch_archive_sweep_moves_per_tick,
                    self.config.batch_archive_sweep_dwell_secs,
                    1usize,
                ),
                (
                    "backfill",
                    self.config.batch_archive_backfill_enabled,
                    self.retention_maintenance
                        .batchless_archive_backfill_enabled(),
                    self.config.batch_archive_backfill_interval_ms,
                    self.config.batch_archive_backfill_moves_per_tick,
                    0.0,
                    self.config.batch_archive_backfill_concurrency,
                ),
            ] {
                let batchless_enabled = batchless_configured;
                if !batch_enabled && !batchless_enabled {
                    continue;
                }

                let storage = self.storage.clone();
                let shutdown = self.shutdown_token.clone();
                let policy = retention_policy.clone();
                let retained_runway_ready = retained_runway_ready.clone();
                let tick = ArchiveMoverTick {
                    worker,
                    batch_enabled,
                    batchless_enabled,
                    batch_limit,
                    batch_concurrency,
                    batch_dwell_secs,
                    batchless_dwell_secs: self.config.batch_archive_sweep_dwell_secs,
                    cancel_grace_secs: self.config.batch_archive_cancel_grace_secs,
                    batchless_group_limit: self
                        .retention_maintenance
                        .batchless_archive_groups_per_tick(),
                    batchless_byte_limit: self
                        .retention_maintenance
                        .batchless_archive_bytes_per_tick(),
                };
                let handle = tokio::spawn(async move {
                    tracing::info!(
                        worker,
                        interval_ms,
                        batch_enabled,
                        batchless_enabled,
                        "Archive mover started"
                    );
                    loop {
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_millis(interval_ms)) => {},
                            _ = shutdown.cancelled() => break,
                        }
                        run_archive_mover_tick(
                            storage.clone(),
                            &shutdown,
                            query_timeout,
                            &policy,
                            tick,
                            retained_runway_ready.as_ref(),
                        )
                        .await;
                    }
                });
                daemon_handles.push((worker, handle));
            }
        }

        // Batch finalizer: owns terminal transitions (stamp + freeze, and for
        // cancelled batches settle-then-freeze) so that finalization never
        // depends on anyone reading the batch or on notification delivery.
        // Notification is a downstream consumer of counts_frozen_at.
        if owns_archive_maintenance && self.config.batch_finalizer_enabled {
            let storage = self.storage.clone();
            let shutdown_token = self.shutdown_token.clone();
            let interval_ms = self.config.batch_finalizer_interval_ms;
            let cancelled_grace = self.config.batch_finalizer_cancelled_grace_secs;
            let cancelled_per_tick = self.config.batch_finalizer_cancelled_per_tick;
            let query_timeout = Duration::from_millis(self.config.claim_query_timeout_ms);
            let handle = tokio::spawn(async move {
                tracing::info!(interval_ms, "Batch finalizer started");
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(interval_ms)) => {},
                        _ = shutdown_token.cancelled() => {
                            tracing::info!("Shutting down batch finalizer");
                            break;
                        }
                    }

                    match maintenance_query(
                        &shutdown_token,
                        "terminal batch finalization",
                        query_timeout,
                        storage.finalize_terminal_batches(),
                    )
                    .await
                    {
                        Ok(Some(n)) if n > 0 => {
                            counter!("fusillade_finalized_batches_total", "kind" => "terminal")
                                .increment(n as u64);
                        }
                        Ok(Some(_)) => {}
                        Ok(None) => return,
                        Err(e) => {
                            crate::background_error!("finalize_terminal_failed", Error, error = %e, "Failed to finalize terminal batches");
                        }
                    }

                    match maintenance_query(
                        &shutdown_token,
                        "cancelled batch finalization",
                        query_timeout,
                        storage.finalize_cancelled_batches(cancelled_grace, cancelled_per_tick),
                    )
                    .await
                    {
                        Ok(Some(n)) if n > 0 => {
                            counter!("fusillade_finalized_batches_total", "kind" => "cancelled")
                                .increment(n as u64);
                        }
                        Ok(Some(_)) => {}
                        Ok(None) => return,
                        Err(e) => {
                            crate::background_error!("finalize_cancelled_failed", Error, error = %e, "Failed to finalize cancelled batches");
                        }
                    }

                    // The finalizer's scoreboard lives with the finalizer (the
                    // archive loops are config-gated OFF by default and may
                    // not be running): sustained nonzero = batches stuck on
                    // the recount path, unable to archive.
                    if let Ok(Some(unfrozen)) = maintenance_query(
                        &shutdown_token,
                        "unfrozen terminal batch count",
                        query_timeout,
                        storage.count_unfrozen_terminal_batches(),
                    )
                    .await
                    {
                        gauge!("fusillade_unfrozen_terminal_batches").set(unfrozen as f64);
                    }
                }
            });
            daemon_handles.push(("batch_finalizer", handle));
        }

        for claim_loop_kind in claim_loop_kinds {
            match claim_loop_kind {
                ClaimLoopKind::Request => {
                    let request_daemon = RequestDaemon::new(self.clone());
                    claim_daemons.spawn(async move { request_daemon.run().await });
                }
                ClaimLoopKind::Batch => {
                    let batch_daemon = BatchDaemon::new(self.clone());
                    claim_daemons.spawn(async move { batch_daemon.run().await });
                }
                ClaimLoopKind::BackgroundRequest => {
                    let background_daemon = BackgroundRequestDaemon::new(self.clone());
                    claim_daemons.spawn(async move { background_daemon.run().await });
                }
                ClaimLoopKind::BackgroundBatch => {
                    let background_daemon = BackgroundBatchDaemon::new(self.clone());
                    claim_daemons.spawn(async move { background_daemon.run().await });
                }
            }
        }

        let mut daemon_children = supervise_daemon_handles(daemon_handles);
        let run_result = loop {
            tokio::select! {
                biased;
                _ = self.shutdown_token.cancelled() => {
                    tracing::info!("Shutdown signal received, stopping daemon");
                    break Ok(());
                }
                child_result = supervise_next_daemon_child(&mut daemon_children), if !daemon_children.is_empty() => {
                    self.shutdown_token.cancel();
                    break child_result;
                }
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
            }
        };
        claim_daemons.abort_all();
        while let Some(result) = claim_daemons.join_next().await {
            if let Err(error) = result
                && !error.is_cancelled()
            {
                crate::background_error!(
                    "claim_daemon_shutdown_failed",
                    Critical,
                    error = %error,
                    "Claim daemon failed while draining after abort"
                );
            }
        }
        self.shutdown_token.cancel();

        // Every child cooperates with the shared token. Join them concurrently
        // so one panic is reported without preventing healthy siblings from
        // completing their shutdown path. The heartbeat child marks the
        // daemon record dead before it returns.
        let _child_panics = drain_supervised_daemon_children(&mut daemon_children).await;

        run_result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::FailureReason;

    /// A 529 is a provider saying it is overloaded; a 429 carrying onwards'
    /// concurrency code is onwards' own limiter, which is the wall this daemon
    /// reaches first once the controller grows past a model's configured limit.
    /// Matching 529 alone leaves the cut path unreachable in this deployment.
    #[test]
    fn provider_529_and_onwards_concurrency_429_are_overload_signals() {
        let onwards_429 =
            r#"{"error":{"type":"rate_limit_error","code":"concurrency_limit_exceeded"}}"#;
        for reason in [
            FailureReason::RetriableHttpStatus {
                status: 529,
                body: String::new(),
            },
            FailureReason::NonRetriableHttpStatus {
                status: 529,
                body: String::new(),
            },
            FailureReason::RetriableHttpStatus {
                status: 429,
                body: onwards_429.to_string(),
            },
            FailureReason::NonRetriableHttpStatus {
                status: 429,
                body: onwards_429.to_string(),
            },
        ] {
            assert!(is_downstream_overload(&reason), "{reason:?}");
        }
    }

    /// A 503 is the gateway reporting it could not place the request with any
    /// provider. It is not saturation, but it must still cut: a failed request
    /// fills a slot, so a model whose capacity has gone away keeps filling every
    /// slot it is offered and ratchets its limit up for the whole outage.
    #[test]
    fn gateway_503_is_an_overload_signal() {
        for reason in [
            FailureReason::RetriableHttpStatus {
                status: 503,
                body: String::new(),
            },
            FailureReason::NonRetriableHttpStatus {
                status: 503,
                body: String::new(),
            },
        ] {
            assert!(is_downstream_overload(&reason), "{reason:?}");
        }
    }

    /// Everything else must leave the limit alone. A bare 429 is a provider rate
    /// limit, usually tokens per minute, which fewer concurrent requests does not
    /// necessarily reduce; timeouts and resets happened to a request the model had
    /// already accepted, so they say nothing about how many more it could take.
    #[test]
    fn other_failures_do_not_cut_the_limit() {
        for reason in [
            FailureReason::RetriableHttpStatus {
                status: 429,
                body: r#"{"error":{"code":"rate_limit_exceeded"}}"#.to_string(),
            },
            FailureReason::RetriableHttpStatus {
                status: 429,
                body: String::new(),
            },
            FailureReason::NetworkError {
                error: "connection reset".to_string(),
            },
            FailureReason::Timeout {
                error: "tokens timeout".to_string(),
            },
        ] {
            assert!(!is_downstream_overload(&reason), "{reason:?}");
        }
    }

    #[derive(Default)]
    struct GaugeHistory {
        values: std::sync::Mutex<Vec<f64>>,
    }

    impl metrics::GaugeFn for GaugeHistory {
        fn increment(&self, value: f64) {
            let mut values = self.values.lock().unwrap();
            let next = values.last().copied().unwrap_or_default() + value;
            values.push(next);
        }

        fn decrement(&self, value: f64) {
            self.increment(-value);
        }

        fn set(&self, value: f64) {
            self.values.lock().unwrap().push(value);
        }
    }

    #[derive(Default)]
    struct GaugeHistoryRecorder {
        gauges: std::sync::Mutex<HashMap<String, Arc<GaugeHistory>>>,
    }

    impl GaugeHistoryRecorder {
        fn values(&self, name: &str) -> Vec<f64> {
            self.gauges
                .lock()
                .unwrap()
                .get(name)
                .map(|gauge| gauge.values.lock().unwrap().clone())
                .unwrap_or_default()
        }
    }

    impl metrics::Recorder for GaugeHistoryRecorder {
        fn describe_counter(
            &self,
            _key: metrics::KeyName,
            _unit: Option<metrics::Unit>,
            _description: metrics::SharedString,
        ) {
        }

        fn describe_gauge(
            &self,
            _key: metrics::KeyName,
            _unit: Option<metrics::Unit>,
            _description: metrics::SharedString,
        ) {
        }

        fn describe_histogram(
            &self,
            _key: metrics::KeyName,
            _unit: Option<metrics::Unit>,
            _description: metrics::SharedString,
        ) {
        }

        fn register_counter(
            &self,
            _key: &metrics::Key,
            _metadata: &metrics::Metadata<'_>,
        ) -> metrics::Counter {
            metrics::Counter::noop()
        }

        fn register_gauge(
            &self,
            key: &metrics::Key,
            _metadata: &metrics::Metadata<'_>,
        ) -> metrics::Gauge {
            let mut gauges = self.gauges.lock().unwrap();
            let gauge = gauges
                .entry(key.name().to_string())
                .or_insert_with(|| Arc::new(GaugeHistory::default()))
                .clone();
            metrics::Gauge::from_arc(gauge)
        }

        fn register_histogram(
            &self,
            _key: &metrics::Key,
            _metadata: &metrics::Metadata<'_>,
        ) -> metrics::Histogram {
            metrics::Histogram::noop()
        }
    }

    struct FakeMaintenanceStorage {
        weekly_calls: AtomicUsize,
        retained_calls: AtomicUsize,
        batch_list_calls: AtomicUsize,
        batch_move_calls: AtomicUsize,
        batchless_calls: AtomicUsize,
        retirement_calls: AtomicUsize,
        retirement_buckets: AtomicUsize,
        retirement_select_new: std::sync::Mutex<Vec<bool>>,
        route_cleanup_calls: AtomicUsize,
        fence_cleanup_calls: AtomicUsize,
        fail_route_cleanup: std::sync::atomic::AtomicBool,
        batchless_cutoffs: std::sync::Mutex<Vec<RetainedResponseArchiveCutoffs>>,
        fail_weekly: std::sync::atomic::AtomicBool,
        fail_retained: std::sync::atomic::AtomicBool,
        block_retained: std::sync::atomic::AtomicBool,
        fail_batch_list: std::sync::atomic::AtomicBool,
        fail_batch_move: std::sync::atomic::AtomicBool,
        index_ready: std::sync::atomic::AtomicBool,
        index_readiness_calls: AtomicUsize,
        retained_contiguous_ahead: AtomicUsize,
        retained_required: AtomicUsize,
        batch_candidates: AtomicUsize,
        events: std::sync::Mutex<Vec<&'static str>>,
    }

    impl Default for FakeMaintenanceStorage {
        fn default() -> Self {
            Self {
                weekly_calls: AtomicUsize::new(0),
                retained_calls: AtomicUsize::new(0),
                batch_list_calls: AtomicUsize::new(0),
                batch_move_calls: AtomicUsize::new(0),
                batchless_calls: AtomicUsize::new(0),
                retirement_calls: AtomicUsize::new(0),
                retirement_buckets: AtomicUsize::new(0),
                retirement_select_new: std::sync::Mutex::new(Vec::new()),
                route_cleanup_calls: AtomicUsize::new(0),
                fence_cleanup_calls: AtomicUsize::new(0),
                fail_route_cleanup: std::sync::atomic::AtomicBool::new(false),
                batchless_cutoffs: std::sync::Mutex::new(Vec::new()),
                fail_weekly: std::sync::atomic::AtomicBool::new(false),
                fail_retained: std::sync::atomic::AtomicBool::new(false),
                block_retained: std::sync::atomic::AtomicBool::new(false),
                fail_batch_list: std::sync::atomic::AtomicBool::new(false),
                fail_batch_move: std::sync::atomic::AtomicBool::new(false),
                index_ready: std::sync::atomic::AtomicBool::new(true),
                index_readiness_calls: AtomicUsize::new(0),
                retained_contiguous_ahead: AtomicUsize::new(7),
                retained_required: AtomicUsize::new(7),
                batch_candidates: AtomicUsize::new(0),
                events: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl FakeMaintenanceStorage {
        fn fail() -> FusilladeError {
            FusilladeError::Other(anyhow::anyhow!("injected maintenance failure"))
        }

        fn record(&self, event: &'static str) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[async_trait::async_trait]
    impl DaemonStorage for FakeMaintenanceStorage {
        async fn persist_daemon<T: DaemonState + Clone>(
            &self,
            _record: &DaemonRecord<T>,
        ) -> Result<()>
        where
            AnyDaemonRecord: From<DaemonRecord<T>>,
        {
            Ok(())
        }

        async fn get_daemon(&self, _daemon_id: DaemonId) -> Result<AnyDaemonRecord> {
            Err(Self::fail())
        }

        async fn list_daemons(
            &self,
            _status_filter: Option<DaemonStatus>,
        ) -> Result<Vec<AnyDaemonRecord>> {
            Ok(Vec::new())
        }

        async fn purge_orphaned_rows(&self, _batch_size: i64) -> Result<u64> {
            Ok(0)
        }

        fn supports_retained_response_lifecycle(&self) -> bool {
            true
        }

        fn supports_retained_response_partition_retirement(&self) -> bool {
            true
        }

        async fn retained_response_archive_index_ready(&self) -> Result<bool> {
            self.index_readiness_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.index_ready.load(Ordering::SeqCst))
        }

        async fn archive_terminal_batchless_responses(
            &self,
            _policy: &RetentionPolicy,
            cutoffs: &RetainedResponseArchiveCutoffs,
            _max_groups: i64,
            _max_bytes: i64,
        ) -> Result<crate::RetainedResponseArchiveOutcome> {
            self.batchless_calls.fetch_add(1, Ordering::SeqCst);
            self.batchless_cutoffs.lock().unwrap().push(*cutoffs);
            self.record("batchless_move");
            Ok(crate::RetainedResponseArchiveOutcome::default())
        }

        async fn ensure_retained_response_partitions(
            &self,
            _policy: &RetentionPolicy,
            _days_ahead: i32,
        ) -> Result<crate::RetainedResponsePartitionRunway> {
            self.retained_calls.fetch_add(1, Ordering::SeqCst);
            self.record("retained_runway");
            if self.block_retained.load(Ordering::SeqCst) {
                std::future::pending().await
            }
            if self.fail_retained.load(Ordering::SeqCst) {
                Err(Self::fail())
            } else {
                Ok(crate::RetainedResponsePartitionRunway {
                    created: 0,
                    contiguous_ahead: self.retained_contiguous_ahead.load(Ordering::SeqCst) as i64,
                    required: self.retained_required.load(Ordering::SeqCst) as i64,
                })
            }
        }

        async fn retire_expired_response_partition(
            &self,
            select_new: bool,
        ) -> Result<crate::RetainedResponseRetirementOutcome> {
            self.retirement_calls.fetch_add(1, Ordering::SeqCst);
            self.retirement_select_new.lock().unwrap().push(select_new);
            self.record("response_retirement");
            let retired = self
                .retirement_buckets
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    (remaining > 0).then(|| remaining - 1)
                })
                .is_ok();
            Ok(if retired {
                crate::RetainedResponseRetirementOutcome::Retired
            } else {
                crate::RetainedResponseRetirementOutcome::NoCandidate
            })
        }

        async fn cleanup_retained_response_routes(&self, _limit: i64) -> Result<u64> {
            self.route_cleanup_calls.fetch_add(1, Ordering::SeqCst);
            self.record("response_route_cleanup");
            if self.fail_route_cleanup.load(Ordering::SeqCst) {
                Err(Self::fail())
            } else {
                Ok(0)
            }
        }

        async fn cleanup_expired_response_fences(&self, _limit: i64) -> Result<u64> {
            self.fence_cleanup_calls.fetch_add(1, Ordering::SeqCst);
            self.record("response_fence_cleanup");
            Ok(0)
        }

        async fn archive_batch(&self, _batch_id: BatchId) -> Result<ArchiveOutcome> {
            self.batch_move_calls.fetch_add(1, Ordering::SeqCst);
            self.record("batch_move");
            if self.fail_batch_move.load(Ordering::SeqCst) {
                Err(Self::fail())
            } else {
                Ok(ArchiveOutcome::Archived { rows: 1 })
            }
        }

        async fn list_archivable_batches(
            &self,
            _limit: i64,
            _oldest_first: bool,
            _cancel_grace_secs: f64,
            _min_frozen_age_secs: f64,
        ) -> Result<Vec<BatchId>> {
            self.batch_list_calls.fetch_add(1, Ordering::SeqCst);
            self.record("batch_list");
            if self.fail_batch_list.load(Ordering::SeqCst) {
                return Err(Self::fail());
            }
            Ok((0..self.batch_candidates.load(Ordering::SeqCst))
                .map(|_| BatchId::from(uuid::Uuid::new_v4()))
                .collect())
        }

        async fn count_archivable_batches(&self, _cancel_grace_secs: f64) -> Result<i64> {
            Ok(0)
        }

        async fn count_unfrozen_terminal_batches(&self) -> Result<i64> {
            Ok(0)
        }

        async fn finalize_terminal_batches(&self) -> Result<i64> {
            Ok(0)
        }

        async fn finalize_cancelled_batches(&self, _grace_secs: f64, _limit: i64) -> Result<i64> {
            Ok(0)
        }

        async fn ensure_archive_partitions(&self, _weeks_ahead: i32) -> Result<(i64, i64)> {
            self.weekly_calls.fetch_add(1, Ordering::SeqCst);
            self.record("weekly_runway");
            if self.fail_weekly.load(Ordering::SeqCst) {
                Err(Self::fail())
            } else {
                Ok((0, 4))
            }
        }

        async fn purge_model_filter_events(
            &self,
            _batch_size: i64,
            _keep_per_model: i64,
            _retention_secs: f64,
        ) -> Result<u64> {
            Ok(0)
        }
    }

    fn mover_tick(batch_enabled: bool, batchless_enabled: bool) -> ArchiveMoverTick {
        ArchiveMoverTick {
            worker: "test",
            batch_enabled,
            batchless_enabled,
            batch_limit: 1,
            batch_concurrency: 1,
            batch_dwell_secs: 1.0,
            batchless_dwell_secs: 1.0,
            cancel_grace_secs: 1.0,
            batchless_group_limit: 1,
            batchless_byte_limit: 1,
        }
    }

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

    #[tokio::test]
    async fn destructive_maintenance_future_is_cancelled_on_shutdown() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        shutdown.cancel();

        let result = tokio::time::timeout(
            Duration::from_millis(100),
            until_shutdown(&shutdown, std::future::pending::<()>()),
        )
        .await
        .expect("shutdown-aware maintenance should return promptly");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn maintenance_query_is_bounded_by_timeout() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let error = maintenance_query(
            &shutdown,
            "test maintenance query",
            Duration::from_millis(1),
            std::future::pending::<Result<()>>(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[tokio::test(start_paused = true)]
    async fn archive_partition_maintenance_runs_initially_then_daily() {
        let storage = Arc::new(FakeMaintenanceStorage::default());
        let shutdown = tokio_util::sync::CancellationToken::new();
        let policy = configured_batchless_maintenance().policy().clone();

        let retained_runway_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let weekly_handle = tokio::spawn(run_weekly_archive_partition_maintenance_loop(
            storage.clone(),
            shutdown.clone(),
            Duration::from_secs(1),
            4,
            Duration::from_secs(86_400),
        ));
        let retained_handle = tokio::spawn(run_retained_response_readiness_loop(
            storage.clone(),
            shutdown.clone(),
            Duration::from_secs(1),
            policy,
            7,
            retained_runway_ready.clone(),
            Duration::from_secs(86_400),
        ));

        tokio::task::yield_now().await;
        assert_eq!(storage.weekly_calls.load(Ordering::SeqCst), 1);
        assert_eq!(storage.retained_calls.load(Ordering::SeqCst), 1);
        assert!(retained_runway_ready.load(Ordering::SeqCst));
        tokio::time::advance(Duration::from_secs(86_399)).await;
        tokio::task::yield_now().await;
        assert_eq!(storage.retained_calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(storage.weekly_calls.load(Ordering::SeqCst), 2);
        assert_eq!(storage.retained_calls.load(Ordering::SeqCst), 2);

        shutdown.cancel();
        weekly_handle.await.unwrap();
        retained_handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_and_route_cleanup_run_even_when_runway_creation_fails() {
        let storage = Arc::new(FakeMaintenanceStorage::default());
        storage.fail_retained.store(true, Ordering::SeqCst);
        storage.fail_route_cleanup.store(true, Ordering::SeqCst);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let ready = Arc::new(AtomicBool::new(false));
        let runway = tokio::spawn(run_retained_response_readiness_loop(
            storage.clone(),
            shutdown.clone(),
            Duration::from_secs(1),
            configured_batchless_maintenance().policy().clone(),
            7,
            ready,
            Duration::from_secs(86_400),
        ));
        let retirement = tokio::spawn(run_retained_response_retirement_loop(
            storage.clone(),
            shutdown.clone(),
            Duration::from_secs(1),
            false,
            Duration::from_secs(86_400),
        ));
        let cleanup = tokio::spawn(run_retained_response_route_cleanup_loop(
            storage.clone(),
            shutdown.clone(),
            Duration::from_secs(1),
            10,
            Duration::from_secs(60),
        ));

        tokio::task::yield_now().await;
        assert_eq!(storage.retained_calls.load(Ordering::SeqCst), 1);
        assert_eq!(storage.retirement_calls.load(Ordering::SeqCst), 1);
        assert_eq!(storage.route_cleanup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            storage.fence_cleanup_calls.load(Ordering::SeqCst),
            1,
            "a route-cleanup failure must not suppress expired-fence cleanup"
        );
        assert_eq!(
            *storage.retirement_select_new.lock().unwrap(),
            vec![false],
            "a disabled flag must still permit unfinished-journal recovery"
        );

        shutdown.cancel();
        runway.await.unwrap();
        retirement.await.unwrap();
        cleanup.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn retirement_drains_backlog_promptly_then_polls_the_date_boundary() {
        let storage = Arc::new(FakeMaintenanceStorage::default());
        storage.retirement_buckets.store(3, Ordering::SeqCst);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let handle = tokio::spawn(run_retained_response_retirement_loop(
            storage.clone(),
            shutdown.clone(),
            Duration::from_secs(30),
            true,
            Duration::from_secs(86_400),
        ));

        tokio::task::yield_now().await;
        assert_eq!(storage.retirement_calls.load(Ordering::SeqCst), 1);
        for expected_calls in 2..=4 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
            assert_eq!(
                storage.retirement_calls.load(Ordering::SeqCst),
                expected_calls,
                "each completed relation should lead to one bounded follow-up tick"
            );
        }
        assert_eq!(storage.retirement_buckets.load(Ordering::SeqCst), 0);

        tokio::time::advance(Duration::from_secs(299)).await;
        tokio::task::yield_now().await;
        assert_eq!(storage.retirement_calls.load(Ordering::SeqCst), 4);
        storage.retirement_buckets.store(1, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            storage.retirement_calls.load(Ordering::SeqCst),
            5,
            "a newly eligible UTC day must be noticed on bounded cadence"
        );

        shutdown.cancel();
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn non_leading_retained_runway_gap_remains_fail_closed() {
        let storage = Arc::new(FakeMaintenanceStorage::default());
        storage.retained_contiguous_ahead.store(3, Ordering::SeqCst);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let ready = Arc::new(AtomicBool::new(false));
        let handle = tokio::spawn(run_retained_response_readiness_loop(
            storage.clone(),
            shutdown.clone(),
            Duration::from_secs(1),
            configured_batchless_maintenance().policy().clone(),
            7,
            ready.clone(),
            Duration::from_secs(86_400),
        ));

        tokio::task::yield_now().await;
        assert_eq!(storage.retained_calls.load(Ordering::SeqCst), 1);
        assert!(
            !ready.load(Ordering::SeqCst),
            "a later gap inside the required runway must keep movement disabled"
        );

        shutdown.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn daily_partition_phases_are_independent() {
        let storage = Arc::new(FakeMaintenanceStorage::default());
        storage.fail_weekly.store(true, Ordering::SeqCst);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let policy = configured_batchless_maintenance().policy().clone();
        let ready = Arc::new(AtomicBool::new(false));

        let weekly_handle = tokio::spawn(run_weekly_archive_partition_maintenance_loop(
            storage.clone(),
            shutdown.clone(),
            Duration::from_secs(1),
            4,
            Duration::from_secs(86_400),
        ));
        let retained_handle = tokio::spawn(run_retained_response_readiness_loop(
            storage.clone(),
            shutdown.clone(),
            Duration::from_secs(1),
            policy,
            7,
            ready.clone(),
            Duration::from_secs(86_400),
        ));
        tokio::task::yield_now().await;

        assert_eq!(storage.weekly_calls.load(Ordering::SeqCst), 1);
        assert_eq!(storage.retained_calls.load(Ordering::SeqCst), 1);
        assert!(
            ready.load(Ordering::SeqCst),
            "weekly partition failure must not suppress the retained runway"
        );
        shutdown.cancel();
        weekly_handle.await.unwrap();
        retained_handle.await.unwrap();
    }

    #[tokio::test]
    async fn request_only_topology_performs_no_archive_ddl_or_movement() {
        let storage = FakeMaintenanceStorage::default();
        let kinds =
            claim_loop_kinds_for_mode(DaemonMode::RequestOnly, true, true, true, true).unwrap();
        assert!(!owns_archive_maintenance(&kinds));
        tokio::task::yield_now().await;
        assert_eq!(storage.weekly_calls.load(Ordering::SeqCst), 0);
        assert_eq!(storage.retained_calls.load(Ordering::SeqCst), 0);
        assert_eq!(storage.batch_list_calls.load(Ordering::SeqCst), 0);
        assert_eq!(storage.batchless_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn mover_phases_are_independent_and_runway_happens_first() {
        let storage = Arc::new(FakeMaintenanceStorage::default());
        let shutdown = tokio_util::sync::CancellationToken::new();
        let policy = configured_batchless_maintenance().policy().clone();

        storage
            .ensure_retained_response_partitions(&policy, 7)
            .await
            .unwrap();
        let retained_runway_ready = std::sync::atomic::AtomicBool::new(true);
        storage.fail_batch_list.store(true, Ordering::SeqCst);
        run_archive_mover_tick(
            storage.clone(),
            &shutdown,
            Duration::from_secs(1),
            &policy,
            mover_tick(true, true),
            &retained_runway_ready,
        )
        .await;
        assert_eq!(storage.batchless_calls.load(Ordering::SeqCst), 1);

        storage.fail_batch_list.store(false, Ordering::SeqCst);
        storage.fail_batch_move.store(true, Ordering::SeqCst);
        storage.batch_candidates.store(1, Ordering::SeqCst);
        run_archive_mover_tick(
            storage.clone(),
            &shutdown,
            Duration::from_secs(1),
            &policy,
            mover_tick(true, true),
            &retained_runway_ready,
        )
        .await;
        assert_eq!(storage.batch_move_calls.load(Ordering::SeqCst), 1);
        assert_eq!(storage.batchless_calls.load(Ordering::SeqCst), 2);

        let events = storage.events.lock().unwrap();
        let runway = events
            .iter()
            .position(|event| *event == "retained_runway")
            .unwrap();
        let first_move = events
            .iter()
            .position(|event| *event == "batchless_move")
            .unwrap();
        assert!(runway < first_move);
    }

    #[tokio::test]
    async fn backfill_tick_uses_the_shared_batchless_dwell_cutoff() {
        let storage = Arc::new(FakeMaintenanceStorage::default());
        let shutdown = tokio_util::sync::CancellationToken::new();
        let policy = configured_batchless_maintenance().policy().clone();
        let mut tick = mover_tick(true, true);
        tick.worker = "backfill";
        tick.batch_dwell_secs = 0.0;
        tick.batchless_dwell_secs = 37.0;

        run_archive_mover_tick(
            storage.clone(),
            &shutdown,
            Duration::from_secs(1),
            &policy,
            tick,
            &std::sync::atomic::AtomicBool::new(true),
        )
        .await;

        let cutoffs = storage.batchless_cutoffs.lock().unwrap();
        assert_eq!(cutoffs.len(), 1);
        assert_eq!(
            cutoffs[0]
                .observed_at()
                .signed_duration_since(cutoffs[0].terminal_before()),
            chrono::Duration::seconds(37)
        );
    }

    #[tokio::test]
    async fn batchless_movement_gates_recover_without_restart() {
        let storage = Arc::new(FakeMaintenanceStorage::default());
        let shutdown = tokio_util::sync::CancellationToken::new();
        let policy = configured_batchless_maintenance().policy().clone();
        let retained_runway_ready = std::sync::atomic::AtomicBool::new(false);

        run_archive_mover_tick(
            storage.clone(),
            &shutdown,
            Duration::from_secs(1),
            &policy,
            mover_tick(false, true),
            &retained_runway_ready,
        )
        .await;
        assert_eq!(storage.batchless_calls.load(Ordering::SeqCst), 0);

        retained_runway_ready.store(true, Ordering::SeqCst);
        storage.index_ready.store(false, Ordering::SeqCst);
        run_archive_mover_tick(
            storage.clone(),
            &shutdown,
            Duration::from_secs(1),
            &policy,
            mover_tick(false, true),
            &retained_runway_ready,
        )
        .await;
        assert_eq!(storage.batchless_calls.load(Ordering::SeqCst), 0);

        storage.index_ready.store(true, Ordering::SeqCst);
        run_archive_mover_tick(
            storage.clone(),
            &shutdown,
            Duration::from_secs(1),
            &policy,
            mover_tick(false, true),
            &retained_runway_ready,
        )
        .await;
        assert_eq!(storage.batchless_calls.load(Ordering::SeqCst), 1);
        assert_eq!(storage.index_readiness_calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn retained_runway_failure_retries_on_bounded_cadence_and_recovers() {
        let storage = Arc::new(FakeMaintenanceStorage::default());
        storage.fail_retained.store(true, Ordering::SeqCst);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let ready = Arc::new(AtomicBool::new(false));
        let handle = tokio::spawn(run_retained_response_readiness_loop(
            storage.clone(),
            shutdown.clone(),
            Duration::from_secs(1),
            configured_batchless_maintenance().policy().clone(),
            7,
            ready.clone(),
            Duration::from_secs(86_400),
        ));

        tokio::task::yield_now().await;
        assert_eq!(storage.retained_calls.load(Ordering::SeqCst), 1);
        assert!(!ready.load(Ordering::SeqCst));
        storage.fail_retained.store(false, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(29)).await;
        tokio::task::yield_now().await;
        assert_eq!(storage.retained_calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(storage.retained_calls.load(Ordering::SeqCst), 2);
        assert!(ready.load(Ordering::SeqCst));

        shutdown.cancel();
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true, flavor = "current_thread")]
    async fn retained_runway_metrics_transition_from_ready_to_failed_closed() {
        let recorder = GaugeHistoryRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let storage = Arc::new(FakeMaintenanceStorage::default());
        let shutdown = tokio_util::sync::CancellationToken::new();
        let ready = Arc::new(AtomicBool::new(false));
        let handle = tokio::spawn(run_retained_response_readiness_loop(
            storage.clone(),
            shutdown.clone(),
            Duration::from_secs(1),
            configured_batchless_maintenance().policy().clone(),
            7,
            ready.clone(),
            Duration::from_secs(86_400),
        ));

        tokio::task::yield_now().await;
        assert!(ready.load(Ordering::SeqCst));

        storage.fail_retained.store(true, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(86_400)).await;
        tokio::task::yield_now().await;
        assert!(!ready.load(Ordering::SeqCst));

        shutdown.cancel();
        handle.await.unwrap();
        assert_eq!(
            recorder.values("fusillade_retained_response_partitions_ahead"),
            vec![7.0, 0.0]
        );
        assert_eq!(
            recorder.values("fusillade_retained_response_partition_runway_ready"),
            vec![1.0, 0.0]
        );
    }

    #[tokio::test(start_paused = true, flavor = "current_thread")]
    async fn retained_runway_metrics_clear_when_shutdown_interrupts_ensure() {
        let recorder = GaugeHistoryRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let storage = Arc::new(FakeMaintenanceStorage::default());
        let shutdown = tokio_util::sync::CancellationToken::new();
        let ready = Arc::new(AtomicBool::new(false));
        let handle = tokio::spawn(run_retained_response_readiness_loop(
            storage.clone(),
            shutdown.clone(),
            Duration::from_secs(1),
            configured_batchless_maintenance().policy().clone(),
            7,
            ready.clone(),
            Duration::from_secs(86_400),
        ));

        tokio::task::yield_now().await;
        assert!(ready.load(Ordering::SeqCst));

        storage.block_retained.store(true, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(86_400)).await;
        tokio::task::yield_now().await;
        assert_eq!(storage.retained_calls.load(Ordering::SeqCst), 2);

        shutdown.cancel();
        handle.await.unwrap();
        assert!(!ready.load(Ordering::SeqCst));
        assert_eq!(
            recorder.values("fusillade_retained_response_partitions_ahead"),
            vec![7.0, 0.0]
        );
        assert_eq!(
            recorder.values("fusillade_retained_response_partition_runway_ready"),
            vec![1.0, 0.0]
        );
    }

    #[test]
    fn enabled_archive_workers_reject_invalid_intervals_and_bounds() {
        let retention = configured_batchless_maintenance()
            .with_batchless_archive_sweep_enabled(true)
            .with_batchless_archive_backfill_enabled(true);
        let mutations: [fn(&mut DaemonConfig); 5] = [
            |config| config.batch_archive_sweep_interval_ms = 0,
            |config| config.batch_archive_backfill_interval_ms = 0,
            |config| {
                config.batch_archive_sweep_enabled = true;
                config.batch_archive_sweep_moves_per_tick = 0;
            },
            |config| {
                config.batch_archive_backfill_enabled = true;
                config.batch_archive_backfill_moves_per_tick = 0;
            },
            |config| {
                config.batch_archive_backfill_enabled = true;
                config.batch_archive_backfill_concurrency = 0;
            },
        ];
        for mutate in mutations {
            let mut config = DaemonConfig::default();
            mutate(&mut config);
            assert!(
                validate_maintenance_worker_config(&config, &retention, DaemonMode::Both, true,)
                    .is_err()
            );
        }
    }

    #[test]
    fn invalid_batch_phase_fails_instead_of_disabling_valid_batchless_phase() {
        let config = DaemonConfig {
            batch_archive_sweep_enabled: true,
            batch_archive_sweep_moves_per_tick: 0,
            ..Default::default()
        };
        let retention = configured_batchless_maintenance()
            .with_batchless_archive_sweep_enabled(true)
            .with_batchless_archive_limits(1, 1);
        assert!(
            validate_maintenance_worker_config(&config, &retention, DaemonMode::Both, true)
                .unwrap_err()
                .to_string()
                .contains("sweep")
        );
    }

    #[test]
    fn retention_snapshot_is_content_free_and_complete() {
        let retention = configured_batchless_maintenance()
            .with_batchless_archive_sweep_enabled(true)
            .with_batchless_archive_limits(3, 4096)
            .with_retained_response_partitions_days_ahead(9);
        let snapshot = daemon_config_snapshot(&DaemonConfig::default(), &retention);
        let summary = &snapshot["retention_maintenance"];
        assert_eq!(
            summary["policy"],
            serde_json::to_value(retention.policy()).unwrap()
        );
        assert_eq!(summary["controls"]["batchless_archive_sweep_enabled"], true);
        assert_eq!(summary["controls"]["batchless_archive_groups_per_tick"], 3);
        assert_eq!(
            summary["controls"]["batchless_archive_bytes_per_tick"],
            4096
        );
        assert_eq!(
            summary["controls"]["retained_response_partitions_days_ahead"],
            9
        );
        assert_eq!(
            summary["required_gates"],
            serde_json::json!(["candidate_index", "continuous_partition_runway"])
        );
        let encoded = serde_json::to_string(summary).unwrap();
        for forbidden in ["request_id", "group_id", "payload", "created_by", "api_key"] {
            assert!(!encoded.contains(forbidden));
        }
    }

    #[tokio::test]
    async fn unexpected_maintenance_completion_and_panic_are_failures() {
        for handle in [
            tokio::spawn(async {}),
            tokio::spawn(async { panic!("injected child panic") }),
        ] {
            let mut children = supervise_daemon_handles(vec![("test_child", handle)]);
            let error = supervise_next_daemon_child(&mut children)
                .await
                .expect_err("every pre-shutdown child exit must fail the daemon");
            assert!(
                error.to_string().contains("test_child") || error.to_string().contains("panicked")
            );
        }
    }

    #[tokio::test]
    async fn shutdown_awaits_every_maintenance_sibling_and_reports_panics() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let completed = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for worker in ["purge", "daily"] {
            let shutdown = shutdown.clone();
            let completed = completed.clone();
            handles.push((
                worker,
                tokio::spawn(async move {
                    shutdown.cancelled().await;
                    tokio::task::yield_now().await;
                    completed.fetch_add(1, Ordering::SeqCst);
                }),
            ));
        }
        handles.push((
            "mover",
            tokio::spawn(async move { panic!("injected maintenance panic") }),
        ));

        shutdown.cancel();
        let mut children = supervise_daemon_handles(handles);
        let panics = drain_supervised_daemon_children(&mut children).await;
        assert_eq!(panics, 1);
        assert_eq!(completed.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn daemon_mode_defaults_to_both_and_roundtrips_through_config() {
        assert_eq!(DaemonConfig::default().mode, DaemonMode::Both);

        let config = DaemonConfig {
            mode: DaemonMode::BatchOnly,
            ..Default::default()
        };

        let json = serde_json::to_value(&config).expect("config should serialize");
        assert_eq!(json["mode"], serde_json::json!("batch_only"));

        let decoded: DaemonConfig =
            serde_json::from_value(json).expect("config should deserialize");
        assert_eq!(decoded.mode, DaemonMode::BatchOnly);
    }

    #[test]
    fn daemon_mode_selects_the_expected_claim_loops() {
        assert_eq!(
            claim_loop_kinds_for_mode(DaemonMode::Both, true, false, false, false)
                .expect("both should be supported"),
            vec![ClaimLoopKind::Request, ClaimLoopKind::Batch]
        );
        assert_eq!(
            claim_loop_kinds_for_mode(DaemonMode::RequestOnly, true, false, false, false)
                .expect("request-only should be supported"),
            vec![ClaimLoopKind::Request]
        );
        assert_eq!(
            claim_loop_kinds_for_mode(DaemonMode::BatchOnly, true, false, false, false)
                .expect("batch-only should be supported"),
            vec![ClaimLoopKind::Batch]
        );
        assert!(
            claim_loop_kinds_for_mode(DaemonMode::BatchOnly, false, false, false, false).is_err(),
            "batch-only mode should fail loudly when storage cannot claim batches"
        );
    }

    /// The controller grows past a model's configured limit, so with it on the
    /// memory gate is the only bound left. Enabling one without the other is the
    /// configuration that OOMs a pod, so it is refused rather than trusted.
    #[test]
    fn adaptive_concurrency_requires_a_memory_gate() {
        assert!(!adaptive_concurrency_permitted(true, false), "unbounded");
        assert!(adaptive_concurrency_permitted(true, true));
        // Without the controller a model cannot exceed its configured limit, so
        // the gate is optional there.
        assert!(adaptive_concurrency_permitted(false, false));
        assert!(adaptive_concurrency_permitted(false, true));
    }

    #[test]
    fn effective_claim_topology_selects_exactly_one_archive_owner() {
        for (kinds, owns_archive) in [
            (vec![ClaimLoopKind::Request], false),
            (
                vec![ClaimLoopKind::Request, ClaimLoopKind::BackgroundRequest],
                false,
            ),
            (vec![ClaimLoopKind::Batch], true),
            (vec![ClaimLoopKind::Request, ClaimLoopKind::Batch], true),
            (
                vec![
                    ClaimLoopKind::Request,
                    ClaimLoopKind::Batch,
                    ClaimLoopKind::BackgroundRequest,
                    ClaimLoopKind::BackgroundBatch,
                ],
                true,
            ),
        ] {
            assert_eq!(owns_archive_maintenance(&kinds), owns_archive);
        }
    }

    fn configured_batchless_maintenance() -> RetentionMaintenanceConfig {
        RetentionMaintenanceConfig::new(crate::RetentionPolicy {
            batchless_seconds_by_service_tier: HashMap::from([("flex".to_owned(), 60)]),
            max_late_writer_seconds: Some(600),
            ..Default::default()
        })
    }

    #[test]
    fn retention_startup_validation_is_fail_closed_and_owner_aware() {
        let mut config =
            RetentionMaintenanceConfig::default().with_batchless_archive_sweep_enabled(true);
        assert!(
            validate_retention_startup(
                &config,
                DaemonMode::Both,
                true,
                true,
                1,
                1,
                ArchiveMovementWindows::new(0.0, 600.0),
            )
            .unwrap_err()
            .to_string()
            .contains("batchless retention policy")
        );

        config = configured_batchless_maintenance();
        config = config
            .with_batchless_archive_sweep_enabled(true)
            .with_batchless_archive_limits(0, 1);
        assert!(
            validate_retention_startup(
                &config,
                DaemonMode::Both,
                true,
                true,
                1,
                1,
                ArchiveMovementWindows::new(0.0, 600.0),
            )
            .unwrap_err()
            .to_string()
            .contains("group and byte budgets")
        );

        config = config.with_batchless_archive_limits(1, 0);
        assert!(
            validate_retention_startup(
                &config,
                DaemonMode::Both,
                true,
                true,
                1,
                1,
                ArchiveMovementWindows::new(0.0, 600.0),
            )
            .unwrap_err()
            .to_string()
            .contains("group and byte budgets")
        );

        config = config
            .with_batchless_archive_limits(1, 1)
            .with_retained_response_partitions_days_ahead(0)
            .with_batchless_archive_sweep_enabled(false);
        assert!(
            validate_retention_startup(
                &config,
                DaemonMode::RequestOnly,
                false,
                false,
                1,
                1,
                ArchiveMovementWindows::new(0.0, 600.0),
            )
            .is_ok(),
            "request-only mode must ignore owner-only runway controls"
        );

        config = config.with_retained_response_partitions_days_ahead(1);
        assert!(
            validate_retention_startup(
                &config,
                DaemonMode::RequestOnly,
                false,
                false,
                1,
                1,
                ArchiveMovementWindows::new(0.0, 600.0),
            )
            .is_ok()
        );

        config = config.with_batchless_archive_backfill_enabled(true);
        assert!(
            validate_retention_startup(
                &config,
                DaemonMode::RequestOnly,
                false,
                false,
                1,
                1,
                ArchiveMovementWindows::new(0.0, 30.0),
            )
            .is_ok(),
            "an intentional request-only process must ignore shared maintenance controls"
        );
        assert!(
            validate_retention_startup(
                &config,
                DaemonMode::Both,
                false,
                false,
                1,
                1,
                ArchiveMovementWindows::new(0.0, 30.0),
            )
            .unwrap_err()
            .to_string()
            .contains("batch-capable archive owner")
        );
        assert!(
            validate_retention_startup(
                &config,
                DaemonMode::Both,
                true,
                true,
                1,
                1,
                ArchiveMovementWindows::new(0.0, 30.0),
            )
            .is_ok()
        );

        let retirement_without_policy =
            RetentionMaintenanceConfig::default().with_retained_response_retirement_enabled(true);
        assert!(
            validate_retention_startup(
                &retirement_without_policy,
                DaemonMode::Both,
                true,
                true,
                1,
                1,
                ArchiveMovementWindows::new(0.0, 600.0),
            )
            .unwrap_err()
            .to_string()
            .contains("batchless retention policy")
        );
        assert!(
            validate_retention_startup(
                &retirement_without_policy,
                DaemonMode::RequestOnly,
                false,
                true,
                1,
                1,
                ArchiveMovementWindows::new(0.0, 600.0),
            )
            .is_ok()
        );
    }

    #[test]
    fn enabled_retirement_requires_the_session_capability_before_startup() {
        let config =
            configured_batchless_maintenance().with_retained_response_retirement_enabled(true);
        assert!(
            validate_retirement_capability(&config, false)
                .unwrap_err()
                .to_string()
                .contains("session-capable maintenance pool")
        );
        assert!(validate_retirement_capability(&config, true).is_ok());
        assert!(
            validate_retirement_capability(&RetentionMaintenanceConfig::default(), false).is_ok()
        );
    }

    #[sqlx::test]
    async fn request_only_daemon_ignores_a_shared_retirement_flag_without_a_ddl_pool(
        pool: sqlx::PgPool,
    ) {
        let storage = Arc::new(fusillade_arsenal::PostgresRequestManager::new(
            fusillade_arsenal::TestDbPools::new(pool)
                .await
                .expect("test pools must initialize"),
            fusillade_arsenal::PostgresStorageConfig::default(),
        ));
        let daemon = Daemon::new(
            storage,
            Arc::new(crate::MockHttpClient::new()),
            DaemonConfig::default(),
            tokio_util::sync::CancellationToken::new(),
        )
        .with_retention_maintenance(
            configured_batchless_maintenance().with_retained_response_retirement_enabled(true),
        );

        daemon
            .validate_startup(DaemonMode::RequestOnly)
            .expect("a request-only daemon must not require the archive owner's DDL pool");
        let error = daemon
            .validate_startup(DaemonMode::Both)
            .expect_err("an archive owner must fail closed without the DDL pool");
        assert!(
            error
                .to_string()
                .contains("session-capable maintenance pool")
        );
    }

    #[test]
    fn scheduled_file_and_batch_retention_are_rejected_until_supported() {
        for policy in [
            crate::RetentionPolicy {
                expire_files: true,
                ..Default::default()
            },
            crate::RetentionPolicy {
                terminal_batch_seconds: Some(60),
                ..Default::default()
            },
        ] {
            let error = validate_retention_startup(
                &RetentionMaintenanceConfig::new(policy),
                DaemonMode::BatchOnly,
                true,
                true,
                1,
                1,
                ArchiveMovementWindows::new(0.0, 0.0),
            )
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("file and batch retention is not supported")
            );
        }

        let shared_request_only_config = RetentionMaintenanceConfig::new(crate::RetentionPolicy {
            expire_files: true,
            terminal_batch_seconds: Some(60),
            ..Default::default()
        });
        assert!(
            validate_retention_startup(
                &shared_request_only_config,
                DaemonMode::RequestOnly,
                false,
                false,
                0,
                0,
                ArchiveMovementWindows::new(0.0, 0.0),
            )
            .is_ok(),
            "an intentional request-only process must ignore shared lifecycle capabilities"
        );
    }

    #[test]
    fn movement_windows_must_leave_time_before_retention_expiry() {
        let sweep = configured_batchless_maintenance()
            .with_batchless_archive_sweep_enabled(true)
            .with_batchless_archive_limits(1, 1);

        for (dwell, grace, expected) in [
            (60.0, 1.0, "sweep dwell"),
            (f64::NAN, 1.0, "sweep dwell"),
            (0.0, 60.0, "cancellation grace"),
            (0.0, f64::INFINITY, "cancellation grace"),
        ] {
            let error = validate_retention_startup(
                &sweep,
                DaemonMode::Both,
                true,
                true,
                1,
                1,
                ArchiveMovementWindows::new(dwell, grace),
            )
            .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }

        assert!(
            validate_retention_startup(
                &sweep,
                DaemonMode::Both,
                true,
                true,
                1,
                1,
                ArchiveMovementWindows::new(59.0, 59.0),
            )
            .is_ok()
        );

        let backfill = configured_batchless_maintenance()
            .with_batchless_archive_backfill_enabled(true)
            .with_batchless_archive_limits(1, 1);
        assert!(
            validate_retention_startup(
                &backfill,
                DaemonMode::Both,
                true,
                true,
                1,
                1,
                ArchiveMovementWindows::new(60.0, 1.0),
            )
            .unwrap_err()
            .to_string()
            .contains("sweep dwell")
        );
    }

    #[test]
    fn background_capacity_reserves_sla_headroom_per_model() {
        assert_eq!(background_capacity(100, 50, 70), 0);
        assert_eq!(background_capacity(100, 50, 40), 10);
        assert_eq!(background_capacity(100, 50, 0), 50);
        assert_eq!(background_capacity(20, 50, 0), 20);
        assert_eq!(background_capacity(100, 0, 0), 0);
    }

    #[test]
    fn background_workers_never_participate_in_foreground_accounting() {
        assert!(ClaimLoopKind::Request.uses_foreground_accounting());
        assert!(ClaimLoopKind::Batch.uses_foreground_accounting());
        assert!(!ClaimLoopKind::BackgroundRequest.uses_foreground_accounting());
        assert!(!ClaimLoopKind::BackgroundBatch.uses_foreground_accounting());

        assert!(ClaimLoopKind::Request.emits_legacy_claim_metrics());
        assert!(ClaimLoopKind::Batch.emits_legacy_claim_metrics());
        assert!(!ClaimLoopKind::BackgroundRequest.emits_legacy_claim_metrics());
        assert!(!ClaimLoopKind::BackgroundBatch.emits_legacy_claim_metrics());

        assert!(!ClaimLoopKind::Request.is_background());
        assert!(!ClaimLoopKind::Batch.is_background());
        assert!(ClaimLoopKind::BackgroundRequest.is_background());
        assert!(ClaimLoopKind::BackgroundBatch.is_background());
    }

    #[test]
    fn background_workers_mirror_modality_claim_configuration() {
        let config = DaemonConfig {
            claim_interval_ms: 13,
            batch_claim_interval_ms: 17,
            claim_batch_size: 11,
            batch_claim_size: 7,
            ..Default::default()
        };

        assert_eq!(
            ClaimLoopKind::BackgroundRequest.claim_interval_ms(&config),
            13
        );
        assert_eq!(
            ClaimLoopKind::BackgroundBatch.claim_interval_ms(&config),
            17
        );
        assert_eq!(ClaimLoopKind::BackgroundRequest.claim_size(&config), 11);
        assert_eq!(ClaimLoopKind::BackgroundBatch.claim_size(&config), 7);

        let inherited = DaemonConfig {
            claim_interval_ms: 19,
            batch_claim_interval_ms: 0,
            claim_batch_size: 23,
            batch_claim_size: 0,
            ..Default::default()
        };
        assert_eq!(
            ClaimLoopKind::BackgroundBatch.claim_interval_ms(&inherited),
            19
        );
        assert_eq!(ClaimLoopKind::BackgroundBatch.claim_size(&inherited), 23);
    }

    #[test]
    fn background_loop_requires_safe_shared_configuration() {
        assert_eq!(DaemonConfig::default().background_concurrency_limit, 0);
        assert_eq!(
            claim_loop_kinds_for_mode(DaemonMode::Both, true, true, true, true).unwrap(),
            vec![
                ClaimLoopKind::Request,
                ClaimLoopKind::Batch,
                ClaimLoopKind::BackgroundRequest,
                ClaimLoopKind::BackgroundBatch,
            ]
        );
        assert_eq!(
            claim_loop_kinds_for_mode(DaemonMode::RequestOnly, false, true, true, true).unwrap(),
            vec![ClaimLoopKind::Request, ClaimLoopKind::BackgroundRequest]
        );
        assert_eq!(
            claim_loop_kinds_for_mode(DaemonMode::BatchOnly, true, true, true, true).unwrap(),
            vec![ClaimLoopKind::Batch, ClaimLoopKind::BackgroundBatch]
        );
        assert_eq!(
            claim_loop_kinds_for_mode(DaemonMode::Both, false, true, true, true).unwrap(),
            vec![ClaimLoopKind::Request, ClaimLoopKind::BackgroundRequest]
        );
        assert!(claim_loop_kinds_for_mode(DaemonMode::Both, true, false, true, true).is_err());
        assert!(claim_loop_kinds_for_mode(DaemonMode::Both, true, true, true, false).is_err());
    }

    #[test]
    fn background_priority_is_reserved_and_preserves_nvext_siblings() {
        let mut body = serde_json::json!({
            "input": "hello",
            "nvext": {
                "cache_control": {"enabled": true},
                "agent_hints": {"priority": 123, "max_batch_size": 8}
            }
        })
        .to_string();

        inject_dynamo_priority(&mut body, BACKGROUND_DYNAMO_PRIORITY);

        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            json["nvext"]["agent_hints"]["priority"],
            serde_json::json!(i32::MIN)
        );
        assert_eq!(json["nvext"]["agent_hints"]["max_batch_size"], 8);
        assert_eq!(json["nvext"]["cache_control"]["enabled"], true);

        assert_eq!(
            sla_dynamo_priority(chrono::DateTime::<chrono::Utc>::MAX_UTC),
            MIN_SLA_DYNAMO_PRIORITY
        );
        assert!(
            sla_dynamo_priority(chrono::DateTime::<chrono::Utc>::MAX_UTC)
                > BACKGROUND_DYNAMO_PRIORITY
        );
    }

    #[test]
    fn default_claim_query_timeout_is_three_minutes() {
        assert_eq!(DaemonConfig::default().claim_query_timeout_ms, 180_000);
    }

    #[test]
    fn enabled_daemon_intervals_must_be_positive_before_tasks_spawn() {
        let mutations: [fn(&mut DaemonConfig); 4] = [
            |config: &mut DaemonConfig| config.heartbeat_interval_ms = 0,
            |config: &mut DaemonConfig| config.cancellation_poll_interval_ms = 0,
            |config: &mut DaemonConfig| config.status_log_interval_ms = Some(0),
            |config: &mut DaemonConfig| config.throughput_log_interval_ms = Some(0),
        ];
        for mutate in mutations {
            let mut config = DaemonConfig::default();
            mutate(&mut config);
            assert!(validate_daemon_intervals(&config).is_err());
        }

        let config = DaemonConfig {
            status_log_interval_ms: None,
            throughput_log_interval_ms: None,
            ..Default::default()
        };
        assert!(validate_daemon_intervals(&config).is_ok());
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
