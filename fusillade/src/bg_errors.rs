//! Unified background-error metric and log for the fusillade daemon.
//!
//! `background_error!` emits the `fusillade_background_errors_total` counter AND a `tracing`
//! event from one call site, so the metric and the log can never drift. Mirrors dwctl's
//! `metrics::errors`. Named `bg_errors` (not `metrics`) to avoid shadowing the `metrics` crate
//! at the fusillade crate root.
//!
//! fusillade has a single background `component` ("daemon"), so it is baked in here rather than
//! passed at every call site. The `component` label is still emitted (so dashboards/queries
//! stay parallel to dwctl); if a second component ever appears, add the arg back then.

use metrics::counter;

/// Increment `fusillade_background_errors_total{component="daemon", reason, severity}`.
/// `reason`/`severity` are `&'static str` (string literals) - the type forbids dynamic labels.
pub fn record(reason: &'static str, severity: &'static str) {
    counter!(
        "fusillade_background_errors_total",
        "component" => "daemon",
        "reason" => reason,
        "severity" => severity
    )
    .increment(1);
}

/// Record a daemon background failure: increment the metric AND emit a `tracing` event, from
/// one site. Severity is a literal tier token: `Critical` (error! + pages), `Error` (error!,
/// no page), `Warning` (warn!, no page). `reason` must be a `&'static str` literal; trailing
/// tokens are forwarded to `tracing` as fields and message.
#[macro_export]
macro_rules! background_error {
    ($reason:expr, Critical, $($arg:tt)+) => {{
        $crate::bg_errors::record($reason, "critical");
        ::tracing::error!(component = "daemon", reason = $reason, $($arg)+);
    }};
    ($reason:expr, Error, $($arg:tt)+) => {{
        $crate::bg_errors::record($reason, "error");
        ::tracing::error!(component = "daemon", reason = $reason, $($arg)+);
    }};
    ($reason:expr, Warning, $($arg:tt)+) => {{
        $crate::bg_errors::record($reason, "warning");
        ::tracing::warn!(component = "daemon", reason = $reason, $($arg)+);
    }};
}
