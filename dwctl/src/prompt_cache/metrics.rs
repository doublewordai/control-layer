//! Prometheus instrumentation for the cached-input-pricing layer.
//!
//! Thin `record_*` helpers over the `metrics` facade, mirroring [`crate::metrics::errors`].
//! Conventions:
//! - Every metric name is `dwctl_cache_*`.
//! - Low-cardinality labels are `&'static str` literals (`outcome`/`reason`/`tier`/
//!   `result`/`marked`). `model` is the deployed alias (bounded, ~dozens) as an owned
//!   `String`. NEVER label by principal / api-key / correlation-id / prefix-hash.
//! - Counters created lazily on first emission (no pre-registration needed, as in
//!   `errors.rs`); histograms use the recorder's default buckets unless tuned in the
//!   recorder builder.

use metrics::{counter, histogram};

// ── Adoption ──────────────────────────────────────────────────────────────────

/// Every chat-completions request the layer sees, labelled by whether the client
/// included any `cache_control` markers. Adoption % = `marked="true"` / all. Measured
/// at the marker-strip step, so it covers ALL traffic regardless of model enablement,
/// floor, or read/write outcome — i.e. how many users are putting caching in their prompts.
pub fn record_marker_request(model: &str, marked: bool) {
    counter!(
        "dwctl_cache_marker_requests_total",
        "model" => model.to_owned(),
        "marked" => if marked { "true" } else { "false" }
    )
    .increment(1);
}

// ── Request outcome + token volumes ───────────────────────────────────────────

/// Request-level cache behaviour. `outcome` ∈ `read` | `create_only` | `read_and_create`
/// | `zero_active` (enabled but nothing cached) | `inactive` (model not enabled / no key).
pub fn record_request_outcome(model: &str, outcome: &'static str) {
    counter!("dwctl_cache_requests_total", "model" => model.to_owned(), "outcome" => outcome).increment(1);
}

/// Token volumes for a cache-active request, from the classifier's split. Feeds the
/// cache-reuse rate = `read / (read + creation)`. The full-prompt hit rate and the
/// uncached residual are [DB]-derived from `http_analytics` (`prompt_tokens − read −
/// creation`), which is also the authoritative billing source — this counter is the
/// real-time *classified* volume (emitted before the commit success-gate).
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

/// Classify-join result. `outcome` ∈ `ok` | `deadline_exceeded` | `error` | `panicked`.
/// `deadline_exceeded` is the primary "tokenizer/index outage is adding latency" signal.
pub fn record_classify(outcome: &'static str) {
    counter!("dwctl_cache_classify_total", "outcome" => outcome).increment(1);
}

/// Wall-time of the fork-joined classify (parallel with the upstream call).
pub fn record_classify_duration(seconds: f64) {
    histogram!("dwctl_cache_classify_duration_seconds").record(seconds);
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

/// principal resolver memo. `result` ∈ `hit` | `miss` | `unknown_key`.
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

/// Write skipped by the success gate. `reason` ∈ `non_2xx` (non-billable response) |
/// `stream_aborted` (mid-stream error frame or client disconnect). A high veto rate flags
/// upstream instability / wasted classify.
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

/// Marker validation rejection. `reason` ∈ `too_many_breakpoints` | `invalid_ttl` | `unsupported_type`.
pub fn record_markers_rejected(reason: &'static str) {
    counter!("dwctl_cache_markers_rejected_total", "reason" => reason).increment(1);
}
