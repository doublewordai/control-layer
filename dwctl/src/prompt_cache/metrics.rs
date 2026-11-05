//! Prometheus instrumentation for the cached-input-pricing layer.
//!
//! Thin `record_*` helpers over the `metrics` facade, mirroring [`crate::metrics::errors`].
//! Conventions:
//! - Every metric name is `dwctl_cache_*`.
//! - Low-cardinality labels are `&'static str` literals (`outcome`/`reason`/`tier`/`result`/`marked`).
//!   The dynamic label is `model` (owned `String`), attached in exactly TWO places, each with its
//!   own bounding guarantee: `record_token_volumes` (emitted solely for cache-ENABLED models — the
//!   alias has a tariff) and `record_tokenizer_duration` (the caller labels by model only for a name
//!   tokenizer-svc ACCEPTED — its baked map is the bound; every error path is clamped to a fixed
//!   value, see `TokenizerClient::tokenize`). The all-traffic metrics (`marker_requests`,
//!   `requests`) carry **no `model` label**, because the raw request `model` is unvalidated there
//!   (unknown/typo strings) and would be unbounded cardinality. Never label `model` from
//!   unvalidated input, nor by principal / api-key / correlation-id / prefix-hash — a new
//!   model-labelled metric needs a bounding argument like the two above.
//! - Counters created lazily on first emission (no pre-registration needed, as in
//!   `errors.rs`); histograms use the recorder's default buckets unless tuned in the
//!   recorder builder.

use metrics::{counter, gauge, histogram};

// ── Adoption ──────────────────────────────────────────────────────────────────

/// Every chat-completions request the layer sees, labelled by whether the client included
/// any `cache_control` markers. Adoption % = `marked="true"` / all. Measured at the strip
/// step — before the model is validated — so it covers ALL traffic; deliberately **no
/// `model` label** (raw, unbounded user input here). Per-model lives on `record_token_volumes`.
pub fn record_marker_request(marked: bool) {
    counter!(
        "dwctl_cache_marker_requests_total",
        "marked" => if marked { "true" } else { "false" }
    )
    .increment(1);
}

// ── Request outcome + token volumes ───────────────────────────────────────────

/// Request-level cache behaviour across ALL traffic. `outcome` ∈ `read` | `create_only` |
/// `read_and_create` | `zero_active` (enabled but nothing cached) | `inactive` (model not
/// enabled / no key) | `aborted` (classify was cancelled before it resolved — a client disconnect,
/// or a downstream failure that made waiting for it pointless — so the outcome is unknown, including
/// whether it would even have been cache-active).
/// **No `model` label** — `inactive` covers unknown/typo models (raw input → unbounded); per-model
/// volumes are on `record_token_volumes` (enabled-only).
pub fn record_request_outcome(outcome: &'static str) {
    counter!("dwctl_cache_requests_total", "outcome" => outcome).increment(1);
}

/// Token volumes for a cache-active request, from the classifier's split. The `model`
/// label is safe here (unlike the all-traffic metrics): this is emitted only for
/// cache-ACTIVE requests, so `model` is always a tariffed/enabled alias — a bounded,
/// validated set, never raw request input. Feeds the cache-reuse rate =
/// `read / (read + creation)`. The full-prompt hit rate and the uncached residual are
/// [DB]-derived from `http_analytics` (`prompt_tokens − read − creation`), which is also
/// the authoritative billing source — this counter is the real-time *classified* volume.
pub fn record_token_volumes(model: &str, read: u64, creation_5m: u64, creation_1h: u64, creation_24h: u64) {
    if read > 0 {
        counter!("dwctl_cache_read_input_tokens_total", "model" => model.to_owned()).increment(read);
    }
    for (tier, n) in [("5m", creation_5m), ("1h", creation_1h), ("24h", creation_24h)] {
        if n > 0 {
            counter!("dwctl_cache_creation_input_tokens_total", "model" => model.to_owned(), "tier" => tier).increment(n);
        }
    }
}

// ── Classify path ─────────────────────────────────────────────────────────────

/// Classify-join result. `outcome` ∈ `ok` | `deadline_exceeded` | `error` | `panicked` |
/// `abandoned` (the client disconnected before the join, so the task was aborted un-joined) |
/// `downstream_failed` (the upstream response failed — non-2xx or a body transport error — while
/// classify was still running, so it was aborted rather than waited for). `deadline_exceeded` is
/// the primary "tokenizer/index outage is adding latency" signal; a high `abandoned` rate flags
/// wasted classify work from client disconnects.
pub fn record_classify(outcome: &'static str) {
    counter!("dwctl_cache_classify_total", "outcome" => outcome).increment(1);
}

/// Wall-time of the fork-joined classify (parallel with the upstream call).
pub fn record_classify_duration(seconds: f64) {
    histogram!("dwctl_cache_classify_duration_seconds").record(seconds);
}

/// Index-lookup (cache READ) latency — the `prompt_cache_entries` point-lookup on its own,
/// separate from tokenize and commit so a read-path p99 spike is attributable to the DB read
/// (or connection acquisition) rather than buried inside `classify_duration`.
pub fn record_lookup_duration(seconds: f64) {
    histogram!("dwctl_cache_lookup_duration_seconds").record(seconds);
}

/// Why a cache-enabled request cached nothing. `reason` ∈ `no_markers` | `unparseable`
/// | `tokenizer_unmapped` | `tokenize_failed` | `count_mismatch` | `below_floor` |
/// `blockless_route` (a route with no module-cacheable blocks, e.g. plain /completions —
/// classification short-circuits before the body is parsed).
/// (Non-enabled models / missing keys are counted by `record_request_outcome{outcome="inactive"}`.)
pub fn record_skip(reason: &'static str) {
    counter!("dwctl_cache_skip_total", "reason" => reason).increment(1);
}

// ── Tokenizer-svc ─────────────────────────────────────────────────────────────

/// A LOGICAL tokenizer operation (one `tokenize()`/`render()` call, across ALL of its serving-scope
/// attempts). `outcome` ∈ `ok` | `http_error` | `unmapped_422` | `transport_error` (timeout/connection).
/// Contrast with [`record_tokenizer_attempt`], which counts each HTTP attempt inside one such call.
pub fn record_tokenizer_request(outcome: &'static str) {
    counter!("dwctl_cache_tokenizer_requests_total", "outcome" => outcome).increment(1);
}

/// LOGICAL tokenizer-svc operation latency (one `tokenize()`/`render()` call, wall-time across all
/// of its attempts, including retry backoff/budget waits), attributable per model and payload size.
/// For the per-attempt view use [`record_tokenizer_attempt`]; for retry queueing use
/// [`record_tokenizer_retry_budget_wait`].
///
/// Labels (added after the 2026-07 deadline-miss investigation, where the unlabelled series
/// couldn't answer "which model / how big"): `model` is the virtual-model alias — bounded by
/// the tokenizer-svc map (the caller labels by model only on an svc-accepted name and clamps
/// every error path to `unmapped`/`error`, see `TokenizerClient::tokenize`); `size` is a coarse
/// payload bucket (see [`tokenize_size_bucket`]) so slow-call attribution (big cold prefixes
/// vs service-wide slowness) is a single query.
///
/// Existing `sum by (le)` dashboard/alert queries aggregate across these labels unchanged.
/// Deliberately NOT also recording an unlabelled twin under the same name: every observation
/// would then be counted twice in any label-agnostic aggregation, corrupting the quantiles.
pub fn record_tokenizer_duration(model: &str, size: &'static str, seconds: f64) {
    histogram!("dwctl_cache_tokenizer_duration_seconds", "model" => model.to_owned(), "size" => size).record(seconds);
}

/// Coarse request-payload bucket for [`record_tokenizer_duration`]: total bytes across the
/// tokenized segments. Buckets chosen around observed traffic: chat turns land ≤16k, agentic
/// cold prefixes (the deadline-miss shape) 64k+.
pub fn tokenize_size_bucket(total_bytes: usize) -> &'static str {
    match total_bytes {
        0..=16_383 => "lt16k",
        16_384..=65_535 => "16k_64k",
        65_536..=262_143 => "64k_256k",
        _ => "gte256k",
    }
}

// ── Tokenizer-svc retries (serving scope) ─────────────────────────────────────
// `op` ∈ `tokenize` | `render` | `models`. Every label here is `&'static str`, so retry series
// cannot multiply with model/request-id/URL/body cardinality. Callers live in
// `tokenizer.rs`/`tokenizer_retry.rs`.

/// Every completed tokenizer-svc HTTP attempt made by a logical operation (any scope).
/// `op` ∈ `tokenize` | `render` | `models`; `attempt` ∈ `first` | `retry`; `result` ∈ `ok` |
/// `overloaded_503` | `http_error` | `transport_error`. This is the per-ATTEMPT counter: a logical
/// call that succeeds after two 503s emits `first/overloaded_503`, `retry/overloaded_503`,
/// `retry/ok`. Non-serving callers (recompute/replay/prefix-chain) are single-attempt and so emit
/// only `attempt="first"`. The 5s per-attempt timeout is a `transport_error` here; malformed 2xx
/// and non-503 statuses are `http_error`.
pub fn record_tokenizer_attempt(op: &'static str, attempt: &'static str, result: &'static str) {
    counter!(
        "dwctl_cache_tokenizer_attempts_total",
        "op" => op,
        "attempt" => attempt,
        "result" => result
    )
    .increment(1);
}

/// A LOGICAL operation (one `tokenize()`/`render()` call) that succeeded after at least one 503
/// retry. Dividing this by the logical-operation counters gives the share of calls the retry loop
/// rescued; it is deliberately not derivable from the per-attempt counter alone.
pub fn record_tokenizer_retry_recovered(op: &'static str) {
    counter!("dwctl_cache_tokenizer_retry_recovered_total", "op" => op).increment(1);
}

/// Time a retry spent waiting for the shared budget (rate token + concurrency permit) before
/// its HTTP attempt started. Recorded once per retry attempt (0 when no wait). The companion
/// `..._waits_total` counts only the non-zero waits, so
/// `waits_total / retry-attempts` is the fraction of retries that queue, while the histogram
/// p50/p95 is the actual queue depth. A rising p95 is the first sign the shared budget (not
/// tokenizer-svc itself) is throttling recovery.
pub fn record_tokenizer_retry_budget_wait(op: &'static str, seconds: f64) {
    histogram!("dwctl_cache_tokenizer_retry_budget_wait_seconds", "op" => op).record(seconds);
    if seconds > 0.0 {
        counter!("dwctl_cache_tokenizer_retry_budget_waits_total", "op" => op).increment(1);
    }
}

/// Current number of in-flight retry HTTP attempts (process-wide). Gauge, not a counter: it
/// saturates at `cache.tokenizer_retry.max_concurrent_retries` by construction, so a flat top
/// means the concurrency permit is the bottleneck.
pub fn set_tokenizer_retry_inflight(n: usize) {
    gauge!("dwctl_cache_tokenizer_retry_inflight").set(n as f64);
}

/// alias→version moka cache. `result` ∈ `hit` | `miss`.
pub fn record_tokenizer_version_cache(result: &'static str) {
    counter!("dwctl_cache_tokenizer_version_cache_total", "result" => result).increment(1);
}

// ── Index resilience ──────────────────────────────────────────────────────────

/// A cache-index op hit a connection-class error and is being retried (bounded by
/// `cache.index_conn_retries`; see `postgres.rs`). Incremented once per retry ATTEMPT, so a
/// single op can contribute up to the configured budget under a sustained outage.
/// `op` ∈ `lookup` | `write` | `refresh`. The rate of THIS counter shows the
/// underlying connection churn (severed idle conns / handshake storms on the bursty batch
/// pod) even when the retry succeeds and no classify error surfaces — the before/after
/// instrument for the burst load test.
pub fn record_index_conn_retry(op: &'static str) {
    counter!("dwctl_cache_index_conn_retries_total", "op" => op).increment(1);
}

// ── Resolver L1 caches ────────────────────────────────────────────────────────

/// principal resolver memo. `result` ∈ `hit` (L1 hit on a known key) | `miss` (L1 miss,
/// resolved to a known key via DB) | `unknown_key` (key not found — cached `None` or a fresh
/// DB miss). L1 hit-rate ≈ `hit / (hit + miss)`; key-probe volume ≈ `unknown_key`.
pub fn record_principal_resolve(result: &'static str) {
    counter!("dwctl_cache_principal_resolve_total", "result" => result).increment(1);
}

/// model-config resolver (the 60s-TTL enablement cache). `result` ∈ `hit` | `miss`.
pub fn record_model_config_resolve(result: &'static str) {
    counter!("dwctl_cache_model_config_resolve_total", "result" => result).increment(1);
}

// ── Commit path (success-gated write) ─────────────────────────────────────────

/// `result` ∈ `ok` | `error` | `timeout`.
pub fn record_commit(result: &'static str) {
    counter!("dwctl_cache_commit_total", "result" => result).increment(1);
}

/// Write skipped by the success gate. `reason` ∈ `non_2xx` (non-billable status) |
/// `error_frame` (2xx stream that carried a mid-stream error frame) | `no_usage` (2xx response
/// that never emitted a usage frame/object, so there's nothing billable to gate on). A high veto
/// rate flags upstream instability / wasted classify. (A true client disconnect aborts the
/// deferred classify before the gate runs, so it isn't counted here.)
pub fn record_commit_vetoed(reason: &'static str) {
    counter!("dwctl_cache_commit_vetoed_total", "reason" => reason).increment(1);
}

/// Commit (index write/refresh) latency.
pub fn record_commit_duration(seconds: f64) {
    histogram!("dwctl_cache_commit_duration_seconds").record(seconds);
}

// ── Safety / anti-abuse ───────────────────────────────────────────────────────

/// A cacheable request whose body couldn't be buffered — the configured body limit was
/// exceeded OR the body stream errored — so it degraded to no-cache. (`to_bytes` doesn't
/// distinguish the two without depending on axum-internal error types; both mean "couldn't
/// read the body to classify".) No `model` label: the body isn't parsed once the read fails.
pub fn record_body_read_failed() {
    counter!("dwctl_cache_body_read_failed_total").increment(1);
}

/// Marker validation rejection — now a 400 to the client (the cache layer rejects synchronously
/// before forwarding), not a silent no-cache. `reason` ∈ `too_many_breakpoints` | `invalid_ttl` |
/// `unsupported_type` | `tier_disabled` (a valid tier the platform has turned off in config) |
/// `malformed_cache_control` (a non-object `cache_control`, a missing/non-string `type`, or a non-string `ttl`).
pub fn record_markers_rejected(reason: &'static str) {
    counter!("dwctl_cache_markers_rejected_total", "reason" => reason).increment(1);
}

/// A `cacheBreakpoint` query param was seen on a cacheable request (see `super::query` — the
/// out-of-band form of the top-level automatic-caching marker). `outcome` ∈ `applied` (marker
/// injected into the body) | `body_field_wins` (body already had a non-null top-level
/// `cache_control`; explicit wins) | `invalid` (unrecognized value → 400) | `not_json` /
/// `not_an_object` (nowhere to inject; onwards rejects the body downstream) |
/// `reserialize_failed` (defensive: forwarded un-injected) | `non_chat_ignored` (the param
/// arrived on a non-chat cacheable route — stripped from the URI, otherwise ignored).
/// Adoption/rollout signal for the
/// param, distinct from `marker_requests` which counts the injected marker as body adoption.
pub fn record_query_breakpoint(outcome: &'static str) {
    counter!("dwctl_cache_query_breakpoint_total", "outcome" => outcome).increment(1);
}
