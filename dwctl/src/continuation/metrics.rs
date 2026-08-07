//! Prometheus instrumentation for mid-stream continuation.
//!
//! Same conventions as [`crate::prompt_cache::metrics`]: thin `record_*` helpers
//! over the `metrics` facade, every name `dwctl_continuation_*`, low-cardinality
//! labels as `&'static str` literals.
//!
//! **Cardinality bound on the `model` label.** It is attached only to metrics
//! emitted *after* the route gate ([`super::ContinuationRoutes::is_enabled`]),
//! i.e. only for aliases that carry a `continuation`-purpose `model_traffic_rules`
//! row — an admin-controlled, DB-bounded set, never raw request input. The
//! all-traffic metric (`outcome_total`) therefore carries NO `model` label: its
//! `ineligible` arm covers unknown/typo model names.
//!
//! The two questions these are designed to answer directly in PromQL:
//! - "what fraction of eligible streams died, and of those, how many did we
//!   save?" → `outcome_total{outcome="resumed"} / eligible_streams_total`;
//! - "what did resuming cost us?" → `eaten_prompt_tokens_total` × tariff, done in
//!   Grafana (never dollarized in code).

use metrics::{counter, histogram};

/// Terminal outcome for one armed (or rejected) stream.
///
/// `outcome` ∈ `resumed` (a resume leg carried the stream to completion) |
/// `failed` (we tried and could not) | `ineligible` (never armed) | `disarmed`
/// (armed, then something made the stream non-reconstructable).
///
/// `reason` is the sub-cause: `structured_output` | `unsupported_delta` |
/// `cap_exceeded` | `multi_choice` | `no_route` | `origin_disabled` | `throttled`
/// | `deadline` | `attempts_exhausted` | `client_disconnect` | `client_error` |
/// `not_streaming` | `unparseable` | `no_model` | `render_failed` | `no_envelope`
/// | death families from [`super::detect`] (`transport_error`, `truncated`,
/// `error_envelope`, `cancelled_499`, `stall`) | `ok` for a clean completion.
pub fn record_outcome(outcome: &'static str, reason: &'static str) {
    counter!("dwctl_continuation_outcome_total", "outcome" => outcome, "reason" => reason).increment(1);
}

/// An armed tee: a streaming request on a continuation-enabled model that we are
/// prepared to resume. The denominator for every resume ratio. `model` is bounded
/// (see the module docs: emitted only past the route gate).
pub fn record_eligible_stream(model: &str) {
    counter!("dwctl_continuation_eligible_streams_total", "model" => model.to_owned()).increment(1);
}

/// One dispatched resume leg. `attempt` is the 1-based chain position, clamped to
/// a small set of literals so a misconfigured `max_attempts` cannot grow the
/// label space.
pub fn record_resume_leg(model: &str, attempt: u32) {
    counter!(
        "dwctl_continuation_resume_legs_total",
        "model" => model.to_owned(),
        "attempt" => attempt_label(attempt)
    )
    .increment(1);
}

/// Bounded `attempt` label: 1, 2, 3, then a catch-all.
fn attempt_label(attempt: u32) -> &'static str {
    match attempt {
        1 => "1",
        2 => "2",
        3 => "3",
        _ => "4+",
    }
}

/// The seam: death detected → first resumed content token reaching the client.
/// This is what a user perceives as the gap, and what the resume deadline is
/// spent on (render + a cold provider prefill).
pub fn record_seam(model: &str, seconds: f64) {
    histogram!("dwctl_continuation_seam_seconds", "model" => model.to_owned()).record(seconds);
}

/// Prompt tokens re-paid on a resume leg — each leg re-prefills the WHOLE prompt
/// at the continuation target's input price. This is the eaten-cost line;
/// dollarization happens in Grafana against the tariff, never here.
pub fn record_eaten_prompt_tokens(model: &str, tokens: u64) {
    if tokens > 0 {
        counter!("dwctl_continuation_eaten_prompt_tokens_total", "model" => model.to_owned()).increment(tokens);
    }
}

/// A merged-usage sanity guard fired. `kind` ∈ `prompt_underflow` (see
/// [`super::rewrap::UsageAnomaly`]) | `no_usage_frame` (the final leg finished
/// without one, so the synthesized frame is render-derived).
pub fn record_usage_anomaly(kind: &'static str) {
    counter!("dwctl_continuation_usage_anomaly_total", "kind" => kind).increment(1);
}

/// tokenizer-svc `/v1/render` call for a resume prefix. `outcome` ∈ `ok` |
/// `unmapped` | `unsupported` | `missing_segment_count` | `http_error` |
/// `transport_error`. Deliberately unlabelled by model: a render failure is a
/// service-level event, and the alias is not validated on the error paths.
pub fn record_render(outcome: &'static str) {
    counter!("dwctl_continuation_render_total", "outcome" => outcome).increment(1);
}

/// `/v1/render` round-trip latency — the controllable half of the seam budget.
pub fn record_render_duration(seconds: f64) {
    histogram!("dwctl_continuation_render_duration_seconds").record(seconds);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempt_labels_are_bounded() {
        assert_eq!(attempt_label(1), "1");
        assert_eq!(attempt_label(3), "3");
        assert_eq!(attempt_label(4), "4+");
        assert_eq!(attempt_label(9_999), "4+");
    }
}
