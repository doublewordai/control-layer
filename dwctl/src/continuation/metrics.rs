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

use metrics::{counter, gauge, histogram};

/// Whether the resume layer is actually wired into the stack. Set once at
/// router build: 1 when continuation is enabled and the state built, 0 when it
/// was enabled but the build failed (the gateway keeps serving without resume).
/// `enabled=false` deployments never set it. Alert on `== 0`: a deployment that
/// asked for continuation and silently lost it looks healthy everywhere else.
pub fn record_layer_wired(wired: bool) {
    gauge!("dwctl_continuation_layer_wired").set(if wired { 1.0 } else { 0.0 });
}

/// Terminal outcome for one armed (or rejected) stream.
///
/// `outcome` ∈ `resumed` (a resume leg carried the stream to completion — its
/// `reason` is the death family the stream was rescued FROM) |
/// `failed` (we tried and could not) | `ineligible` (never armed) | `disarmed`
/// (armed, then something made the stream non-reconstructable).
///
/// `reason` is the sub-cause: `structured_output` | `unsupported_delta` |
/// `cap_exceeded` | `multi_choice` | `no_route` | `origin_disabled` | `throttled`
/// | `deadline` | `attempts_exhausted` | `client_disconnect` |
/// `not_streaming` | `unparseable` | `no_model` | `render_failed` | `no_envelope`
/// | `needs_forward_parser` | `logprobs`
/// | death families from [`super::detect`] (`transport_error`, `truncated`,
/// `error_envelope`, `error_envelope_4xx`, `cancelled_499`, `stall`) | `ok` for
/// a clean completion.
/// `origin` ∈ `realtime` | `batch` | `playground` | `unknown` (gates that run
/// before the key purpose is resolved).
pub fn record_outcome(outcome: &'static str, reason: &'static str, origin: &'static str) {
    counter!("dwctl_continuation_outcome_total", "outcome" => outcome, "reason" => reason, "origin" => origin).increment(1);
}

/// An armed tee: a streaming request on a continuation-enabled model that we are
/// prepared to resume. The denominator for every resume ratio. `model` is bounded
/// (see the module docs: emitted only past the route gate).
pub fn record_eligible_stream(model: &str, origin: &'static str) {
    counter!("dwctl_continuation_eligible_streams_total", "model" => model.to_owned(), "origin" => origin).increment(1);
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
/// `provider` ∈ `dynamo` (free self-hosted hop) | `external` (paid third
/// party) — detected from the serving leg's first frame.
pub fn record_seam(model: &str, provider: &'static str, seconds: f64) {
    histogram!("dwctl_continuation_seam_seconds", "model" => model.to_owned(), "provider" => provider).record(seconds);
}

/// One resume leg that produced its first frame, attributed to the upstream
/// that served it, with the leg's rendered prompt size. `leg_served_tokens ×
/// the external provider's input price` is the paid-rescue cost line (the
/// dynamo share is the free-hop counterfactual); `eaten_prompt_tokens_total`
/// stays the dispatch-side total (dead legs included).
pub fn record_leg_served(model: &str, provider: &'static str, prompt_tokens: u64) {
    counter!("dwctl_continuation_legs_served_total", "model" => model.to_owned(), "provider" => provider).increment(1);
    if prompt_tokens > 0 {
        counter!("dwctl_continuation_leg_served_tokens_total", "model" => model.to_owned(), "provider" => provider)
            .increment(prompt_tokens);
    }
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

/// Which delta key caused an `unsupported_delta` disarm. The outcome metric
/// keeps its single stable `unsupported_delta` reason; this side counter is the
/// per-kind split that sizes reconstructor work per family: `reasoning_content`
/// / `tool_calls` (a family reconstructor lifts these), `reasoning` (foreign
/// dialect — reasoning text with no measured position in the raw sequence),
/// `function_call` (legacy encoding, never measured).
pub fn record_unsupported_delta(kind: &'static str) {
    counter!("dwctl_continuation_unsupported_delta_total", "kind" => kind).increment(1);
}

/// The largest inter-frame gap observed on an armed stream, recorded once at
/// stream end. This is the empirical answer to "is N seconds of silence a
/// death sentence?": before trusting any stall timeout, read this
/// distribution's tail — a healthy population above a proposed timeout means
/// the timeout would sever recovering streams. Keep-alive comments reset the
/// gap (they are liveness).
pub fn record_max_frame_gap(model: &str, seconds: f64) {
    histogram!("dwctl_continuation_max_frame_gap_seconds", "model" => model.to_owned()).record(seconds);
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
