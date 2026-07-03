//! Prometheus instrumentation for the cached-input-pricing layer.
//!
//! Thin `record_*` helpers over the `metrics` facade, mirroring [`crate::metrics::errors`].
//! Conventions:
//! - Every metric name is `dwctl_cache_*`.
//! - Low-cardinality labels are `&'static str` literals (`outcome`/`reason`/`tier`/`result`/`marked`).
//!   The one dynamic label is `model` (owned `String`), attached **only** by `record_token_volumes`,
//!   which is emitted solely for cache-ENABLED models — a small, validated, bounded set (the alias
//!   has a tariff). The all-traffic metrics (`marker_requests`, `requests`) carry **no `model` label**,
//!   because the raw request `model` is unvalidated there (unknown/typo strings) and would be
//!   unbounded cardinality. Never label `model` from unvalidated input, nor by principal / api-key /
//!   correlation-id / prefix-hash.
//! - Counters created lazily on first emission (no pre-registration needed, as in
//!   `errors.rs`); histograms use the recorder's default buckets unless tuned in the
//!   recorder builder.

use metrics::{counter, histogram};

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
/// enabled / no key) | `aborted` (streaming request whose client disconnected before classify was
/// joined — the outcome is unknown, including whether it would even have been cache-active).
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
/// `abandoned` (streaming request whose client disconnected before the join, so the task was
/// aborted un-joined). `deadline_exceeded` is the primary "tokenizer/index outage is adding
/// latency" signal; a high `abandoned` rate flags wasted classify work from client disconnects.
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
/// | `tokenizer_unmapped` | `tokenize_failed` | `count_mismatch` | `below_floor`.
/// (Non-enabled models / missing keys are counted by `record_request_outcome{outcome="inactive"}`.)
pub fn record_skip(reason: &'static str) {
    counter!("dwctl_cache_skip_total", "reason" => reason).increment(1);
}

// ── Tokenizer-svc ─────────────────────────────────────────────────────────────

/// `outcome` ∈ `ok` | `http_error` | `unmapped_422` | `transport_error` (timeout/connection).
pub fn record_tokenizer_request(outcome: &'static str) {
    counter!("dwctl_cache_tokenizer_requests_total", "outcome" => outcome).increment(1);
}

/// tokenizer-svc round-trip latency.
pub fn record_tokenizer_duration(seconds: f64) {
    histogram!("dwctl_cache_tokenizer_duration_seconds").record(seconds);
}

/// alias→version moka cache. `result` ∈ `hit` | `miss`.
pub fn record_tokenizer_version_cache(result: &'static str) {
    counter!("dwctl_cache_tokenizer_version_cache_total", "result" => result).increment(1);
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
