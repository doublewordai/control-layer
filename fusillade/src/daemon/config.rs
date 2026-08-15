//! Shared daemon configuration.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::http::HttpResponse;

/// Predicate function to determine if a response should be retried.
pub type ShouldRetryFn = Arc<dyn Fn(&HttpResponse) -> bool + Send + Sync>;

/// Default retry predicate: retry on server errors, rate limits, timeouts, and not
/// found, plus successful chat completions that carry reasoning but no final answer.
pub fn default_should_retry(response: &HttpResponse) -> bool {
    if response.status >= 500
        || response.status == 429
        || response.status == 408
        || response.status == 404
    {
        return true;
    }
    (200..300).contains(&response.status) && is_reasoning_without_answer(&response.body)
}

/// A completed chat response whose message holds chain-of-thought but no answer:
/// the engine emitted an end-of-sequence token mid-reasoning, so the whole output
/// was filed as reasoning and `content` never arrived. The response looks
/// successful (2xx, `finish_reason: "stop"`) but is useless to the caller, so it
/// is classified as a failure and retried.
///
/// Detection must read the body, not usage counts: some engine builds omit the
/// reasoning split from the usage frame (`reasoning_tokens: 0` despite real
/// reasoning), so token-based predicates miss them.
///
/// `finish_reason: "length"` is deliberately excluded — hitting a token cap is a
/// valid outcome under caller-controlled parameters, and retrying it can re-arm
/// arbitrarily large generations.
fn is_reasoning_without_answer(body: &str) -> bool {
    // Non-reasoning responses can never match; skip parsing their bodies.
    if !body.contains("\"reasoning") {
        return false;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    let Some(choices) = v.get("choices").and_then(|c| c.as_array()) else {
        return false;
    };
    choices.iter().any(|choice| {
        if choice.get("finish_reason").and_then(|f| f.as_str()) != Some("stop") {
            return false;
        }
        let Some(message) = choice.get("message") else {
            return false;
        };
        // A tool invocation is a complete answer with legitimately empty content.
        let has_tool_calls = message
            .get("tool_calls")
            .and_then(|t| t.as_array())
            .is_some_and(|t| !t.is_empty())
            || message.get("function_call").is_some_and(|f| !f.is_null());
        if has_tool_calls {
            return false;
        }
        let has_reasoning = ["reasoning_content", "reasoning"].iter().any(|key| {
            message
                .get(key)
                .and_then(|r| r.as_str())
                .is_some_and(|r| !r.is_empty())
        });
        let no_answer = match message.get("content") {
            None | Some(serde_json::Value::Null) => true,
            Some(serde_json::Value::String(s)) => s.is_empty(),
            // Structured content parts are an answer even when unusual.
            Some(_) => false,
        };
        has_reasoning && no_answer
    })
}

fn default_should_retry_fn() -> ShouldRetryFn {
    Arc::new(default_should_retry)
}

fn default_additional_retryable_statuses() -> Vec<u16> {
    vec![499]
}

fn default_model_escalations() -> Arc<dashmap::DashMap<String, ModelEscalationConfig>> {
    Arc::new(dashmap::DashMap::new())
}

fn default_model_concurrency_limits() -> Arc<dashmap::DashMap<String, usize>> {
    Arc::new(dashmap::DashMap::new())
}

fn serialize_model_concurrency_limits<S>(
    limits: &Arc<dashmap::DashMap<String, usize>>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::Serialize;

    let limits: HashMap<String, usize> = limits
        .iter()
        .map(|entry| (entry.key().clone(), *entry.value()))
        .collect();
    limits.serialize(serializer)
}

fn deserialize_model_concurrency_limits<'de, D>(
    deserializer: D,
) -> std::result::Result<Arc<dashmap::DashMap<String, usize>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;

    let limits = HashMap::<String, usize>::deserialize(deserializer)?;
    let map = dashmap::DashMap::new();
    for (model, limit) in limits {
        map.insert(model, limit);
    }
    Ok(Arc::new(map))
}

fn default_escalation_threshold_seconds() -> i64 {
    900
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelEscalationConfig {
    pub escalation_model: String,
    #[serde(default = "default_escalation_threshold_seconds")]
    pub escalation_threshold_seconds: i64,
}

/// Which claim loops a daemon process should run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonMode {
    /// Run both batchless request claims and batch claims.
    #[default]
    Both,
    /// Run only the batchless request claim loop.
    RequestOnly,
    /// Run only the batch claim loop.
    BatchOnly,
}

impl DaemonMode {
    /// Stable label value for the `mode` dimension on daemon metrics
    /// (e.g. `fusillade_daemon_up`). Matches the serde snake_case encoding so
    /// config values and metric labels read identically on dashboards.
    pub fn metric_label(&self) -> &'static str {
        match self {
            DaemonMode::Both => "both",
            DaemonMode::RequestOnly => "request_only",
            DaemonMode::BatchOnly => "batch_only",
        }
    }
}

/// Additive controls for retained-response maintenance.
///
/// This configuration is installed with [`super::Daemon::with_retention_maintenance`]
/// so existing exhaustive [`DaemonConfig`] literals remain source compatible.
/// The policy carries no implicit retention duration and every destructive
/// action is disabled by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionMaintenanceConfig {
    policy: crate::RetentionSweepPolicy,
    batchless_archive_sweep_enabled: bool,
    batchless_archive_backfill_enabled: bool,
    batchless_archive_groups_per_tick: i64,
    batchless_archive_bytes_per_tick: i64,
    retained_response_partitions_days_ahead: i32,
    retained_response_retirement_enabled: bool,
}

impl Default for RetentionMaintenanceConfig {
    fn default() -> Self {
        Self {
            policy: crate::RetentionSweepPolicy::default(),
            batchless_archive_sweep_enabled: false,
            batchless_archive_backfill_enabled: false,
            batchless_archive_groups_per_tick: 4,
            batchless_archive_bytes_per_tick: 64 * 1_024 * 1_024,
            retained_response_partitions_days_ahead: 7,
            retained_response_retirement_enabled: false,
        }
    }
}

impl RetentionMaintenanceConfig {
    /// Create disabled maintenance controls for an explicit retention policy.
    pub fn new(policy: crate::RetentionSweepPolicy) -> Self {
        Self {
            policy,
            ..Self::default()
        }
    }

    /// Enable or disable steady movement of newly terminal batchless graphs.
    pub fn with_batchless_archive_sweep_enabled(mut self, enabled: bool) -> Self {
        self.batchless_archive_sweep_enabled = enabled;
        self
    }

    /// Enable or disable historical batchless archive movement.
    pub fn with_batchless_archive_backfill_enabled(mut self, enabled: bool) -> Self {
        self.batchless_archive_backfill_enabled = enabled;
        self
    }

    /// Set the complete-graph and retained-payload byte bounds per mover tick.
    pub fn with_batchless_archive_limits(mut self, max_groups: i64, max_bytes: i64) -> Self {
        self.batchless_archive_groups_per_tick = max_groups;
        self.batchless_archive_bytes_per_tick = max_bytes;
        self
    }

    /// Set the daily retained-response partition runway.
    pub fn with_retained_response_partitions_days_ahead(mut self, days_ahead: i32) -> Self {
        self.retained_response_partitions_days_ahead = days_ahead;
        self
    }

    /// Set the independently gated retirement control.
    ///
    /// This release validates and carries the control but does not schedule
    /// retained-response retirement.
    pub fn with_retained_response_retirement_enabled(mut self, enabled: bool) -> Self {
        self.retained_response_retirement_enabled = enabled;
        self
    }

    /// Return the operator-supplied retention policy.
    pub fn policy(&self) -> &crate::RetentionSweepPolicy {
        &self.policy
    }

    /// Whether steady batchless archive movement is enabled.
    pub fn batchless_archive_sweep_enabled(&self) -> bool {
        self.batchless_archive_sweep_enabled
    }

    /// Whether historical batchless archive movement is enabled.
    pub fn batchless_archive_backfill_enabled(&self) -> bool {
        self.batchless_archive_backfill_enabled
    }

    /// Maximum complete response graphs moved per tick.
    pub fn batchless_archive_groups_per_tick(&self) -> i64 {
        self.batchless_archive_groups_per_tick
    }

    /// Maximum retained payload bytes moved per tick.
    pub fn batchless_archive_bytes_per_tick(&self) -> i64 {
        self.batchless_archive_bytes_per_tick
    }

    /// Number of future daily retained-response partitions to ensure.
    pub fn retained_response_partitions_days_ahead(&self) -> i32 {
        self.retained_response_partitions_days_ahead
    }

    /// Whether the reserved retained-response retirement control is enabled.
    pub fn retained_response_retirement_enabled(&self) -> bool {
        self.retained_response_retirement_enabled
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct DaemonConfig {
    /// Claim-loop mode for this daemon process.
    #[serde(default)]
    pub mode: DaemonMode,
    pub claim_batch_size: usize,
    /// Per-model concurrency.
    ///
    /// With `adaptive_concurrency` off these are the limits, unchanged. With it
    /// on they are where each model starts, and the controller owns the number
    /// from there - bounded by the memory gate, not per model.
    #[serde(
        default = "default_model_concurrency_limits",
        serialize_with = "serialize_model_concurrency_limits",
        deserialize_with = "deserialize_model_concurrency_limits"
    )]
    pub model_concurrency_limits: Arc<dashmap::DashMap<String, usize>>,
    /// Discover each model's concurrency from downstream backpressure instead of
    /// running flat out at its configured value.
    ///
    /// Off by default, and turning it off returns every model to its configured
    /// value exactly as before, so the flag is safe to flip either way. While it
    /// is on a model's limit can go above its configured value, so the daemon
    /// refuses to enable it unless `memory_gate_high_fraction` is set - there
    /// would otherwise be nothing bounding growth.
    #[serde(default)]
    pub adaptive_concurrency: bool,
    /// What to multiply a model's limit by each time it goes up.
    ///
    /// Clamped to `1.01..=10.0`. Higher recovers faster from a cut but overshoots
    /// the model's real capacity further before a 529 says so, and every request
    /// past that point is a retry and a database write.
    #[serde(default = "default_adaptive_growth_factor")]
    pub adaptive_growth_factor: f64,
    /// What to multiply a model's limit by when it returns a 529.
    ///
    /// Clamped to `0.05..=0.99`. Closer to 1 gives up less throughput per
    /// rejection but takes more steps to get down when capacity really has
    /// dropped.
    #[serde(default = "default_adaptive_cut_factor")]
    pub adaptive_cut_factor: f64,
    /// Fraction of this process's own memory limit at or above which claiming
    /// stops. Zero disables the gate.
    ///
    /// A count of in-flight requests cannot express this: per-request bytes
    /// vary by more than an order of magnitude between workloads, so no count is
    /// safe across all of them. This bounds the thing that actually kills the
    /// process, by measuring it rather than predicting it: above the mark the
    /// daemon claims nothing and in-flight drains as requests finish. Nothing
    /// upstream signals local memory pressure, so with `adaptive_concurrency` on
    /// this is the only control that corresponds to running out of memory.
    ///
    /// Claiming resumes on either of two conditions: usage falling below
    /// `memory_gate_low_fraction`, or in-flight falling to
    /// `memory_gate_release_in_flight_fraction` of what it was when the gate
    /// engaged. The second exists because the first is not always reachable -
    /// see that field for why.
    #[serde(default)]
    pub memory_gate_high_fraction: f64,
    /// Fraction of the memory limit below which claiming resumes. Must be above
    /// zero and below `memory_gate_high_fraction`, or the gate stays off.
    ///
    /// Separate from the high mark so the gate does not flip on and off every
    /// claim cycle while usage sits on the boundary.
    #[serde(default = "default_memory_gate_low_fraction")]
    pub memory_gate_low_fraction: f64,
    #[serde(skip, default = "default_model_escalations")]
    pub model_escalations: Arc<dashmap::DashMap<String, ModelEscalationConfig>>,
    #[serde(default)]
    pub inject_deadline_priority: bool,
    /// Database-wide per-model foreground in-flight threshold below which
    /// background work may be dispatched. Already-dispatched background work
    /// does not consume this threshold. Clamped to each process's ordinary
    /// model concurrency limit. Zero disables background claim workers while
    /// leaving submission APIs available.
    #[serde(default)]
    pub background_concurrency_limit: usize,
    pub claim_interval_ms: u64,
    #[serde(default = "default_batch_claim_size")]
    pub batch_claim_size: usize,
    #[serde(default = "default_batch_claim_batch_size")]
    pub batch_claim_batch_size: usize,
    #[serde(default)]
    pub batch_claim_require_live: bool,
    #[serde(default = "default_batch_claim_interval_ms")]
    pub batch_claim_interval_ms: u64,
    #[serde(default = "default_claim_loop_max_consecutive_failures")]
    pub claim_loop_max_consecutive_failures: u32,
    /// Upper bound on the daemon's periodic database queries, in milliseconds.
    ///
    /// This bounds silently severed database connections so claim, heartbeat,
    /// cancellation poll, and purge loops can surface a transient failure and
    /// continue on a fresh connection instead of waiting for TCP keepalive.
    #[serde(default = "default_claim_query_timeout_ms")]
    pub claim_query_timeout_ms: u64,
    /// Maximum number of request state transitions that may write to storage
    /// concurrently. This is independent of inference concurrency so a large
    /// claim can saturate downstream workers without opening the same number of
    /// database connections. Set to `0` to disable the storage-side limit.
    #[serde(default = "default_max_concurrent_state_writes")]
    pub max_concurrent_state_writes: usize,
    pub max_retries: Option<u32>,
    pub stop_before_deadline_ms: Option<i64>,
    pub backoff_ms: u64,
    pub backoff_factor: u64,
    pub max_backoff_ms: u64,
    /// Maximum request-body upload idle time, in milliseconds.
    ///
    /// This watchdog covers outbound body progress for both streaming and
    /// non-streaming requests. Keep it lower than `first_chunk_timeout_ms`:
    /// both clocks can run during `send()`, and whichever expires first
    /// determines the reported timeout. Progress is observed in
    /// `upload_chunk_bytes` units and checked every `upload_stall_poll_ms`.
    #[serde(default = "default_upload_stall_timeout_ms")]
    pub upload_stall_timeout_ms: u64,
    /// Request-body bytes per upload progress unit.
    ///
    /// Smaller values detect incremental progress more finely but create more
    /// body frames. Larger values reduce framing overhead but require a full
    /// unit to be accepted before the watchdog observes progress. Must be
    /// greater than zero.
    #[serde(default = "default_upload_chunk_bytes")]
    pub upload_chunk_bytes: usize,
    /// How often the upload stall watchdog checks progress, in milliseconds.
    ///
    /// A stalled upload may be aborted up to roughly this long after
    /// `upload_stall_timeout_ms` expires. Keep this well below the stall
    /// timeout so it does not materially delay detection. Must be greater
    /// than zero.
    #[serde(default = "default_upload_stall_poll_ms")]
    pub upload_stall_poll_ms: u64,
    /// Maximum time to the first streaming response event, in milliseconds.
    ///
    /// This includes connection setup, request upload, response headers, and
    /// the first event. The daemon itself no longer reads streams, so it applies
    /// `first_chunk_timeout_ms + body_timeout_ms` as one overall request timeout
    /// and the layer that does read them enforces this budget on its own.
    pub first_chunk_timeout_ms: u64,
    /// Maximum idle time between subsequent SSE events, in milliseconds.
    ///
    /// Starts after the first event has arrived. This is the budget that catches
    /// a stream which opens and then stalls, and it has no equivalent in an
    /// overall request timeout, so it is enforced only where the stream is read.
    /// A stream that trips it comes back to the daemon as a timeout error.
    pub chunk_timeout_ms: u64,
    /// Maximum total response-body collection time, in milliseconds.
    ///
    /// Enforced across the whole read where the stream is read, and contributing
    /// to the daemon's combined overall request timeout with
    /// `first_chunk_timeout_ms`.
    pub body_timeout_ms: u64,
    pub status_log_interval_ms: Option<u64>,
    pub heartbeat_interval_ms: u64,
    #[serde(skip, default = "default_should_retry_fn")]
    pub should_retry: ShouldRetryFn,
    /// HTTP statuses retried in addition to those selected by `should_retry`.
    ///
    /// Defaults to `[499]`. Set this to an empty list to disable additional
    /// status-based retries. Values below 400 are ignored.
    #[serde(default = "default_additional_retryable_statuses")]
    pub additional_retryable_statuses: Vec<u16>,
    pub claim_timeout_ms: u64,
    pub processing_timeout_ms: u64,
    #[serde(default = "default_pending_request_counts_timeout_ms")]
    pub pending_request_counts_timeout_ms: u64,
    pub stale_daemon_threshold_ms: u64,
    pub unclaim_batch_size: usize,
    pub cancellation_poll_interval_ms: u64,
    #[serde(default = "default_batch_metadata_fields")]
    pub batch_metadata_fields: Vec<String>,
    pub purge_interval_ms: u64,
    pub purge_batch_size: i64,
    pub purge_throttle_ms: u64,
    /// Batch-archive sweeper (phase 3): moves frozen terminal batches' rows
    /// from `requests` into `batch_requests_archive`. OFF by default — the
    /// blue/green invariant is that deploys never move data; only flipping
    /// this flag does, and only on a single-generation fleet (old pods read
    /// `requests` directly and must all be gone first).
    #[serde(default)]
    pub batch_archive_sweep_enabled: bool,
    #[serde(default = "default_archive_sweep_interval_ms")]
    pub batch_archive_sweep_interval_ms: u64,
    /// Bounded work per tick (orphan-purge pattern, never drain-until-empty):
    /// at most this many batch moves per sweep tick.
    #[serde(default = "default_archive_moves_per_tick")]
    pub batch_archive_sweep_moves_per_tick: i64,
    /// Post-freeze dwell before a batch becomes a sweep candidate. Default 0
    /// (move immediately): reads are mid-move safe by construction, and the
    /// sweep tick + queue already provide organic dwell. Raise only with
    /// evidence from the download-source metrics.
    #[serde(default)]
    pub batch_archive_sweep_dwell_secs: f64,
    /// Cancellation grace window: a batch with canceled rows that were IN
    /// FLIGHT at cancel (claimed_at set) younger than this is not archived
    /// yet, so late billed results can still supersede the cancel on the
    /// live row. Default mirrors processing_timeout (~10 min); only
    /// cancelled batches archive later, fully served from live meanwhile.
    #[serde(default = "default_archive_cancel_grace_secs")]
    pub batch_archive_cancel_grace_secs: f64,
    /// Historical backfill worker: same move machinery as the sweeper on its
    /// own pacing, oldest-first. OFF by default; enable after the sweeper is
    /// live and steady, ramp via the per-tick knob, flip off to pause
    /// instantly (resumable by construction — the queue is the data).
    #[serde(default)]
    pub batch_archive_backfill_enabled: bool,
    #[serde(default = "default_archive_backfill_interval_ms")]
    pub batch_archive_backfill_interval_ms: u64,
    #[serde(default = "default_archive_moves_per_tick")]
    pub batch_archive_backfill_moves_per_tick: i64,
    /// Concurrent moves per backfill tick (waves of this size). Per-move
    /// cost is dominated by fixed transaction overhead on small batches, so
    /// concurrency — not tick pacing — is what raises drain throughput.
    /// Safe under concurrent movers (SKIP LOCKED); values < 1 behave as 1.
    /// The sweeper stays sequential — steady-state volume never needs more.
    #[serde(default = "default_archive_backfill_concurrency")]
    pub batch_archive_backfill_concurrency: usize,
    /// Weekly-partition runway maintained by the daily maintenance tick.
    #[serde(default = "default_archive_partitions_weeks_ahead")]
    pub batch_archive_partitions_weeks_ahead: i32,
    /// Batch finalizer: stamps terminal timestamps and freezes final counts
    /// for batches whose rows have all settled — including cancelled batches,
    /// whose leftover rows it settles first. Runs wherever the batch daemon
    /// runs, independent of notification delivery (which merely consumes
    /// frozen batches). ON by default: frozen counts gate archiving.
    #[serde(default = "default_batch_finalizer_enabled")]
    pub batch_finalizer_enabled: bool,
    #[serde(default = "default_batch_finalizer_interval_ms")]
    pub batch_finalizer_interval_ms: u64,
    /// Grace after cancelled_at before the finalizer settles + freezes a
    /// cancelled batch, letting in-flight daemon aborts drain naturally.
    #[serde(default = "default_batch_finalizer_cancelled_grace_secs")]
    pub batch_finalizer_cancelled_grace_secs: f64,
    /// Bounded cancelled-batch finalizations per tick.
    #[serde(default = "default_batch_finalizer_cancelled_per_tick")]
    pub batch_finalizer_cancelled_per_tick: i64,
    pub throughput_log_interval_ms: Option<u64>,
    #[serde(default)]
    pub urgency_weight: f64,
    #[serde(default = "default_service_tier_completion_windows_ms")]
    pub service_tier_completion_windows_ms: HashMap<String, u64>,
    #[serde(default = "default_completion_window_ms")]
    pub default_completion_window_ms: u64,
    #[serde(default = "default_claim_ramp_exponent")]
    pub claim_ramp_exponent: f64,
    #[serde(default = "default_leaks_per_window")]
    pub leaks_per_window: f64,
    #[serde(default = "default_model_filters_keep_per_model")]
    pub model_filters_keep_per_model: i64,
    #[serde(default = "default_model_filters_retention_ms")]
    pub model_filters_retention_ms: u64,
}

fn default_batch_metadata_fields() -> Vec<String> {
    vec![
        "id".to_string(),
        "endpoint".to_string(),
        "created_at".to_string(),
        "completion_window".to_string(),
    ]
}

fn default_service_tier_completion_windows_ms() -> HashMap<String, u64> {
    HashMap::from([("flex".to_string(), 3_600_000)])
}

fn default_completion_window_ms() -> u64 {
    86_400_000
}

fn default_pending_request_counts_timeout_ms() -> u64 {
    60_000
}

fn default_batch_claim_size() -> usize {
    0
}

fn default_batch_claim_batch_size() -> usize {
    4
}

fn default_batch_claim_interval_ms() -> u64 {
    0
}

fn default_claim_loop_max_consecutive_failures() -> u32 {
    10
}

fn default_claim_query_timeout_ms() -> u64 {
    180_000
}

fn default_max_concurrent_state_writes() -> usize {
    64
}

fn default_upload_stall_timeout_ms() -> u64 {
    crate::http::DEFAULT_UPLOAD_STALL_TIMEOUT.as_millis() as u64
}

fn default_upload_chunk_bytes() -> usize {
    crate::http::DEFAULT_UPLOAD_CHUNK_BYTES
}

fn default_upload_stall_poll_ms() -> u64 {
    crate::http::DEFAULT_UPLOAD_STALL_POLL.as_millis() as u64
}

/// Ten points under a 0.75 high mark: wide enough that the gate does not flap,
/// narrow enough that a pod does not sit idle far below its ceiling.
fn default_memory_gate_low_fraction() -> f64 {
    0.65
}

/// Half the work in flight at engagement. Low enough that a genuinely loaded pod
/// holds for a meaningful stretch rather than flapping, high enough that it
/// always recovers well before a full drain. If memory is still over the high
/// mark when it resumes, the next cycle re-engages, so the cost of releasing too
/// early is one claim batch.
fn default_adaptive_growth_factor() -> f64 {
    1.5
}

fn default_adaptive_cut_factor() -> f64 {
    0.8
}

fn default_archive_sweep_interval_ms() -> u64 {
    5_000
}

fn default_archive_moves_per_tick() -> i64 {
    4
}

fn default_batch_finalizer_enabled() -> bool {
    true
}

fn default_batch_finalizer_interval_ms() -> u64 {
    10_000
}

fn default_batch_finalizer_cancelled_grace_secs() -> f64 {
    3_600.0
}

fn default_batch_finalizer_cancelled_per_tick() -> i64 {
    50
}

fn default_archive_cancel_grace_secs() -> f64 {
    600.0
}

fn default_archive_backfill_concurrency() -> usize {
    1
}

fn default_archive_backfill_interval_ms() -> u64 {
    1_000
}

fn default_archive_partitions_weeks_ahead() -> i32 {
    4
}

fn default_claim_ramp_exponent() -> f64 {
    0.56
}

fn default_leaks_per_window() -> f64 {
    60.0
}

fn default_model_filters_keep_per_model() -> i64 {
    50
}

fn default_model_filters_retention_ms() -> u64 {
    604_800_000
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            mode: DaemonMode::default(),
            claim_batch_size: 100,
            model_concurrency_limits: Arc::new(dashmap::DashMap::new()),
            adaptive_concurrency: false,
            adaptive_growth_factor: default_adaptive_growth_factor(),
            adaptive_cut_factor: default_adaptive_cut_factor(),
            memory_gate_high_fraction: 0.0,
            memory_gate_low_fraction: default_memory_gate_low_fraction(),
            model_escalations: default_model_escalations(),
            inject_deadline_priority: false,
            background_concurrency_limit: 0,
            claim_interval_ms: 1000,
            batch_claim_size: default_batch_claim_size(),
            batch_claim_batch_size: default_batch_claim_batch_size(),
            batch_claim_require_live: false,
            batch_claim_interval_ms: default_batch_claim_interval_ms(),
            claim_loop_max_consecutive_failures: default_claim_loop_max_consecutive_failures(),
            claim_query_timeout_ms: default_claim_query_timeout_ms(),
            max_concurrent_state_writes: default_max_concurrent_state_writes(),
            max_retries: Some(1000),
            stop_before_deadline_ms: Some(0),
            backoff_ms: 1000,
            backoff_factor: 2,
            max_backoff_ms: 10000,
            upload_stall_timeout_ms: default_upload_stall_timeout_ms(),
            upload_chunk_bytes: default_upload_chunk_bytes(),
            upload_stall_poll_ms: default_upload_stall_poll_ms(),
            first_chunk_timeout_ms: 540_000,
            chunk_timeout_ms: 540_000,
            body_timeout_ms: 60_000,
            status_log_interval_ms: Some(2000),
            heartbeat_interval_ms: 5000,
            should_retry: Arc::new(default_should_retry),
            additional_retryable_statuses: default_additional_retryable_statuses(),
            claim_timeout_ms: 60000,
            processing_timeout_ms: 600000,
            pending_request_counts_timeout_ms: default_pending_request_counts_timeout_ms(),
            stale_daemon_threshold_ms: 30_000,
            unclaim_batch_size: 100,
            cancellation_poll_interval_ms: 5000,
            batch_metadata_fields: default_batch_metadata_fields(),
            purge_interval_ms: 600_000,
            batch_archive_sweep_enabled: false,
            batch_archive_sweep_interval_ms: default_archive_sweep_interval_ms(),
            batch_archive_sweep_moves_per_tick: default_archive_moves_per_tick(),
            batch_archive_sweep_dwell_secs: 0.0,
            batch_archive_cancel_grace_secs: default_archive_cancel_grace_secs(),
            batch_archive_backfill_enabled: false,
            batch_archive_backfill_interval_ms: default_archive_backfill_interval_ms(),
            batch_archive_backfill_moves_per_tick: default_archive_moves_per_tick(),
            batch_archive_backfill_concurrency: default_archive_backfill_concurrency(),
            batch_archive_partitions_weeks_ahead: default_archive_partitions_weeks_ahead(),
            batch_finalizer_enabled: default_batch_finalizer_enabled(),
            batch_finalizer_interval_ms: default_batch_finalizer_interval_ms(),
            batch_finalizer_cancelled_grace_secs: default_batch_finalizer_cancelled_grace_secs(),
            batch_finalizer_cancelled_per_tick: default_batch_finalizer_cancelled_per_tick(),
            purge_batch_size: 1000,
            purge_throttle_ms: 100,
            throughput_log_interval_ms: Some(60_000),
            urgency_weight: 0.0,
            service_tier_completion_windows_ms: default_service_tier_completion_windows_ms(),
            default_completion_window_ms: default_completion_window_ms(),
            claim_ramp_exponent: default_claim_ramp_exponent(),
            leaks_per_window: default_leaks_per_window(),
            model_filters_keep_per_model: default_model_filters_keep_per_model(),
            model_filters_retention_ms: default_model_filters_retention_ms(),
        }
    }
}

impl DaemonConfig {
    pub(crate) fn retry_predicate(&self) -> ShouldRetryFn {
        let should_retry = self.should_retry.clone();
        let additional_retryable_statuses: HashSet<u16> = self
            .additional_retryable_statuses
            .iter()
            .copied()
            .filter(|status| *status >= 400)
            .collect();

        Arc::new(move |response| {
            should_retry(response) || additional_retryable_statuses.contains(&response.status)
        })
    }
}

impl From<&DaemonConfig> for crate::request::transitions::RetryConfig {
    fn from(config: &DaemonConfig) -> Self {
        Self {
            max_retries: config.max_retries,
            stop_before_deadline_ms: config.stop_before_deadline_ms,
            backoff_ms: config.backoff_ms,
            backoff_factor: config.backoff_factor,
            max_backoff_ms: config.max_backoff_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            body: body.to_string(),
        }
    }

    #[test]
    fn retention_maintenance_defaults_are_safe_and_bounded() {
        let config = RetentionMaintenanceConfig::default();

        assert!(!config.policy.is_enabled());
        assert!(!config.batchless_archive_sweep_enabled);
        assert!(!config.batchless_archive_backfill_enabled);
        assert!(!config.retained_response_retirement_enabled);
        assert!(config.batchless_archive_groups_per_tick > 0);
        assert!(config.batchless_archive_bytes_per_tick > 0);
        assert!(config.retained_response_partitions_days_ahead > 0);
    }

    #[test]
    fn daemon_mode_metric_labels_match_serde_encoding() {
        // The `mode` label on daemon metrics (fusillade_daemon_up) must read
        // identically to the config value that produced it, or dashboards
        // filtering by mode silently diverge from deployed config.
        for mode in [
            DaemonMode::Both,
            DaemonMode::RequestOnly,
            DaemonMode::BatchOnly,
        ] {
            let serde_encoding = serde_json::to_value(mode).unwrap();
            assert_eq!(serde_encoding.as_str().unwrap(), mode.metric_label());
        }
    }

    #[test]
    fn adaptive_concurrency_ships_dark() {
        // Turning it on lets a model's limit grow past its configured value, so
        // the failure mode for defaulting it on is an OOM rather than a slow
        // queue. It has to be a deliberate flip.
        let config = DaemonConfig::default();
        assert!(!config.adaptive_concurrency);
        assert_eq!(config.memory_gate_high_fraction, 0.0);
    }

    #[test]
    fn adaptive_concurrency_knobs_default_and_round_trip() {
        let default_config = DaemonConfig::default();
        assert_eq!(default_config.adaptive_growth_factor, 1.5);
        assert_eq!(default_config.adaptive_cut_factor, 0.8);

        // Configs serialized before these keys existed must keep deserializing.
        let mut serialized = serde_json::to_value(&default_config).unwrap();
        {
            let serialized = serialized.as_object_mut().unwrap();
            serialized.remove("adaptive_concurrency");
            serialized.remove("adaptive_growth_factor");
            serialized.remove("adaptive_cut_factor");
            serialized.remove("memory_gate_high_fraction");
        }
        let decoded: DaemonConfig = serde_json::from_value(serialized).unwrap();
        assert!(!decoded.adaptive_concurrency);
        assert_eq!(decoded.adaptive_growth_factor, 1.5);
        assert_eq!(decoded.adaptive_cut_factor, 0.8);
        assert_eq!(decoded.memory_gate_high_fraction, 0.0);

        let configured = DaemonConfig {
            adaptive_concurrency: true,
            adaptive_growth_factor: 2.0,
            adaptive_cut_factor: 0.5,
            memory_gate_high_fraction: 0.75,
            ..DaemonConfig::default()
        };
        let decoded: DaemonConfig =
            serde_json::from_value(serde_json::to_value(configured).unwrap()).unwrap();
        assert!(decoded.adaptive_concurrency);
        assert_eq!(decoded.adaptive_growth_factor, 2.0);
        assert_eq!(decoded.adaptive_cut_factor, 0.5);
        assert_eq!(decoded.memory_gate_high_fraction, 0.75);
    }

    #[test]
    fn archive_flags_default_off_and_knobs_sane() {
        // Deploys must never move data: both movers ship dark. The knobs
        // exist so flag flips + pacing are config changes, not releases.
        let c = DaemonConfig::default();
        assert!(!c.batch_archive_sweep_enabled);
        assert!(!c.batch_archive_backfill_enabled);
        assert_eq!(c.batch_archive_sweep_dwell_secs, 0.0);
        assert_eq!(c.batch_archive_cancel_grace_secs, 600.0);
        assert_eq!(c.batch_archive_partitions_weeks_ahead, 4);
        assert!(c.batch_archive_sweep_moves_per_tick > 0);
        assert!(c.batch_archive_backfill_moves_per_tick > 0);
        assert_eq!(c.batch_archive_backfill_concurrency, 1);
        // Old serialized configs (no archive keys) must keep deserializing:
        // strip the new keys before decoding so the missing-field path is
        // what the test actually exercises.
        let mut serialized = serde_json::to_value(&c).unwrap();
        let obj = serialized.as_object_mut().unwrap();
        for key in [
            "batch_archive_sweep_enabled",
            "batch_archive_sweep_interval_ms",
            "batch_archive_sweep_moves_per_tick",
            "batch_archive_sweep_dwell_secs",
            "batch_archive_backfill_concurrency",
            "batch_archive_cancel_grace_secs",
            "batch_archive_backfill_enabled",
            "batch_archive_backfill_interval_ms",
            "batch_archive_backfill_moves_per_tick",
            "batch_archive_partitions_weeks_ahead",
        ] {
            obj.remove(key);
        }
        let decoded: DaemonConfig = serde_json::from_value(serialized).unwrap();
        assert!(!decoded.batch_archive_sweep_enabled);
        assert_eq!(decoded.batch_archive_partitions_weeks_ahead, 4);
    }

    #[test]
    fn preserves_existing_default_retry_statuses() {
        for status in [404, 408, 429, 500, 503] {
            assert!(default_should_retry(&response(status, "")));
        }

        for status in [200, 400, 401, 403, 422, 498, 499] {
            assert!(!default_should_retry(&response(status, "")));
        }
    }

    /// Builds a 2xx chat-completion body with the given message fields spliced in.
    fn chat_completion(finish_reason: &str, message_fields: &str) -> String {
        format!(
            r#"{{"id":"cmpl-1","object":"chat.completion","choices":[{{"index":0,"finish_reason":"{finish_reason}","message":{{"role":"assistant"{message_fields}}}}}],"usage":{{"prompt_tokens":10,"completion_tokens":5}}}}"#
        )
    }

    #[test]
    fn reasoning_without_answer_is_retriable() {
        // The premature end-of-sequence shape: reasoning present, content never
        // arrived (absent, null, or empty) — regardless of which reasoning field
        // the serving stack uses.
        for message in [
            r#","reasoning_content":"We need to assess the headnotes...""#,
            r#","reasoning_content":"We need to assess...","content":null"#,
            r#","reasoning_content":"We need to assess...","content":"""#,
            r#","reasoning":"We need to assess...""#,
        ] {
            let body = chat_completion("stop", message);
            assert!(
                default_should_retry(&response(200, &body)),
                "expected retry for {message}"
            );
        }
    }

    #[test]
    fn reasoning_with_answer_is_not_retriable() {
        let body = chat_completion(
            "stop",
            r#","reasoning_content":"We need to assess...","content":"Headnote 1: ...""#,
        );
        assert!(!default_should_retry(&response(200, &body)));
    }

    #[test]
    fn tool_calls_with_empty_content_are_not_retriable() {
        // Tool invocations legitimately return no content alongside reasoning.
        for message in [
            r#","reasoning_content":"I should call the tool...","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"f","arguments":"{}"}}]"#,
            r#","reasoning_content":"I should call the tool...","function_call":{"name":"f","arguments":"{}"}"#,
        ] {
            let body = chat_completion("stop", message);
            assert!(
                !default_should_retry(&response(200, &body)),
                "expected no retry for {message}"
            );
        }
    }

    #[test]
    fn length_capped_reasoning_is_not_retriable() {
        // Exhausting the caller's token budget mid-reasoning is the caller's
        // signal to handle (finish_reason "length"), not a retriable fault.
        let body = chat_completion("length", r#","reasoning_content":"We need to assess...""#);
        assert!(!default_should_retry(&response(200, &body)));
    }

    #[test]
    fn empty_content_without_reasoning_is_not_retriable() {
        let body = chat_completion("stop", r#","content":"""#);
        assert!(!default_should_retry(&response(200, &body)));
    }

    #[test]
    fn structured_content_counts_as_an_answer() {
        let body = chat_completion(
            "stop",
            r#","reasoning_content":"We need to assess...","content":[{"type":"text","text":"answer"}]"#,
        );
        assert!(!default_should_retry(&response(200, &body)));
    }

    #[test]
    fn unparseable_or_non_chat_bodies_are_not_retriable() {
        for body in [
            "not json at all",
            r#"{"reasoning_content":"orphaned"}"#,
            r#"{"object":"list","data":[],"note":"reasoning"}"#,
            r#"data: {"choices":[{"delta":{"reasoning_content":"sse frame"}}]}"#,
        ] {
            assert!(!default_should_retry(&response(200, body)));
        }
    }

    #[test]
    fn default_config_serializes_additional_retryable_statuses() {
        let config = serde_json::to_value(DaemonConfig::default()).unwrap();

        assert_eq!(
            config["additional_retryable_statuses"],
            serde_json::json!([499])
        );
    }

    #[test]
    fn missing_additional_retryable_statuses_deserializes_to_default() {
        let mut serialized = serde_json::to_value(DaemonConfig::default()).unwrap();
        serialized
            .as_object_mut()
            .unwrap()
            .remove("additional_retryable_statuses");

        let config: DaemonConfig = serde_json::from_value(serialized).unwrap();

        assert_eq!(config.additional_retryable_statuses, vec![499]);
    }

    #[test]
    fn additional_retryable_statuses_are_additive_and_overridable() {
        let default_config = DaemonConfig::default();
        let default_predicate = default_config.retry_predicate();
        assert!(default_predicate(&response(499, "arbitrary upstream body")));

        let disabled_config = DaemonConfig {
            additional_retryable_statuses: vec![],
            ..DaemonConfig::default()
        };
        let disabled_predicate = disabled_config.retry_predicate();
        assert!(!disabled_predicate(&response(499, "")));
        assert!(disabled_predicate(&response(500, "")));

        let overridden_config = DaemonConfig {
            additional_retryable_statuses: vec![200, 204, 418],
            ..DaemonConfig::default()
        };
        let overridden_predicate = overridden_config.retry_predicate();
        assert!(overridden_predicate(&response(418, "")));
        assert!(!overridden_predicate(&response(499, "")));
        assert!(!overridden_predicate(&response(200, "")));
        assert!(!overridden_predicate(&response(204, "")));

        let custom_config = DaemonConfig {
            should_retry: Arc::new(|response| response.status == 409),
            additional_retryable_statuses: vec![418],
            ..DaemonConfig::default()
        };
        let custom_predicate = custom_config.retry_predicate();
        assert!(custom_predicate(&response(409, "")));
        assert!(custom_predicate(&response(418, "")));
        assert!(!custom_predicate(&response(500, "")));
    }

    #[test]
    fn explicit_additional_retryable_statuses_round_trip() {
        for statuses in [vec![], vec![418, 499]] {
            let config = DaemonConfig {
                additional_retryable_statuses: statuses.clone(),
                ..DaemonConfig::default()
            };

            let serialized = serde_json::to_value(config).unwrap();
            let deserialized: DaemonConfig = serde_json::from_value(serialized).unwrap();

            assert_eq!(deserialized.additional_retryable_statuses, statuses);
        }
    }

    #[test]
    fn upload_watchdog_defaults_when_missing() {
        let mut serialized = serde_json::to_value(DaemonConfig::default()).unwrap();
        {
            let serialized = serialized.as_object_mut().unwrap();
            serialized.remove("upload_stall_timeout_ms");
            serialized.remove("upload_chunk_bytes");
            serialized.remove("upload_stall_poll_ms");
        }

        let config: DaemonConfig = serde_json::from_value(serialized).unwrap();

        assert_eq!(config.upload_stall_timeout_ms, 60_000);
        assert_eq!(config.upload_chunk_bytes, 64 * 1024);
        assert_eq!(config.upload_stall_poll_ms, 100);
    }

    #[test]
    fn upload_watchdog_explicit_values_round_trip() {
        let config = DaemonConfig {
            upload_stall_timeout_ms: 12_345,
            upload_chunk_bytes: 8 * 1024,
            upload_stall_poll_ms: 25,
            ..DaemonConfig::default()
        };

        let serialized = serde_json::to_value(config).unwrap();
        let deserialized: DaemonConfig = serde_json::from_value(serialized).unwrap();

        assert_eq!(deserialized.upload_stall_timeout_ms, 12_345);
        assert_eq!(deserialized.upload_chunk_bytes, 8 * 1024);
        assert_eq!(deserialized.upload_stall_poll_ms, 25);
    }

    #[test]
    fn state_write_concurrency_defaults_when_missing() {
        let mut serialized = serde_json::to_value(DaemonConfig::default()).unwrap();
        serialized
            .as_object_mut()
            .unwrap()
            .remove("max_concurrent_state_writes");

        let decoded: DaemonConfig = serde_json::from_value(serialized).unwrap();
        let reencoded = serde_json::to_value(decoded).unwrap();

        assert_eq!(reencoded["max_concurrent_state_writes"], 64);
    }

    #[test]
    fn state_write_concurrency_explicit_value_round_trips() {
        let mut serialized = serde_json::to_value(DaemonConfig::default()).unwrap();
        serialized["max_concurrent_state_writes"] = serde_json::json!(17);

        let decoded: DaemonConfig = serde_json::from_value(serialized).unwrap();
        let reencoded = serde_json::to_value(decoded).unwrap();

        assert_eq!(reencoded["max_concurrent_state_writes"], 17);
    }
}
