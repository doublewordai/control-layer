//! Unified background-error metric and log.
//!
//! `background_error!` emits the `dwctl_background_errors_total` counter AND a `tracing`
//! event from a single call site, so the metric and the log can never drift. It is for failures
//! we want attributed in the unified error metric - primarily background (off-request-path)
//! failures, since foreground route failures that surface as a 5xx are already counted by
//! `dwctl_http_requests_total{status}`. A handful of request-path sites also use it for failures
//! that are swallowed (logged but not returned as the response status), which would otherwise be
//! invisible.

use metrics::counter;

/// Bounded subsystem labels. Always a `&'static str` so the label set stays low cardinality.
pub mod component {
    pub const SUPERVISOR: &str = "supervisor";
    pub const NOTIFICATIONS: &str = "notifications";
    pub const AUTO_TOPUP: &str = "auto_topup";
    pub const WEBHOOK_DISPATCH: &str = "webhook_dispatch";
    pub const LEADER_ELECTION: &str = "leader_election";
    pub const CONFIG_WATCHER: &str = "config_watcher";
    pub const PROBE_SCHEDULER: &str = "probe_scheduler";
    pub const TASK_WORKER: &str = "task_worker";
    pub const ONWARDS_SYNC: &str = "onwards_sync";
    pub const API_KEY_CACHE_SYNC: &str = "api_key_cache_sync";
    pub const ZDR_DISPATCH: &str = "zdr_dispatch";
    pub const ONWARDS_HEARTBEAT: &str = "onwards_heartbeat";
    pub const ANALYTICS: &str = "analytics";
    pub const ANALYTICS_BATCHER: &str = "analytics_batcher";
    pub const RESPONSES_WRITER: &str = "responses_writer";
    pub const BATCH_POPULATE: &str = "batch_populate";
    pub const PAYMENTS: &str = "payments";
    pub const USAGE_REFRESH: &str = "usage_refresh";
}

/// Increment `dwctl_background_errors_total`. `component`/`reason`/`severity` are `&'static str`
/// (string literals) - the type forbids dynamic, high-cardinality labels (a `format!` would not
/// coerce to `&'static str`).
pub fn record(component: &'static str, reason: &'static str, severity: &'static str) {
    counter!(
        "dwctl_background_errors_total",
        "component" => component,
        "reason" => reason,
        "severity" => severity
    )
    .increment(1);
}

/// Record a background failure: increment the metric AND emit a `tracing` event, from one site.
///
/// Severity is a literal tier token:
/// - `Critical` - logs at `error!`, AND pages (the alert keys on `severity="critical"`).
/// - `Error` - logs at `error!`, does NOT page (dashboard / triage).
/// - `Warning` - logs at `warn!`, does NOT page.
///
/// `component` and `reason` must be `&'static str` literals (e.g. `component::WEBHOOK_DISPATCH`,
/// `"delivery_create"`). Trailing tokens are forwarded to `tracing` as fields and message:
///
/// ```ignore
/// background_error!(component::WEBHOOK_DISPATCH, "delivery_create", Critical,
///     error = %e, delivery_id = %id, "Failed to create webhook delivery records");
/// ```
#[macro_export]
macro_rules! background_error {
    ($component:expr, $reason:expr, Critical, $($arg:tt)+) => {{
        $crate::metrics::errors::record($component, $reason, "critical");
        ::tracing::error!(component = $component, reason = $reason, $($arg)+);
    }};
    ($component:expr, $reason:expr, Error, $($arg:tt)+) => {{
        $crate::metrics::errors::record($component, $reason, "error");
        ::tracing::error!(component = $component, reason = $reason, $($arg)+);
    }};
    ($component:expr, $reason:expr, Warning, $($arg:tt)+) => {{
        $crate::metrics::errors::record($component, $reason, "warning");
        ::tracing::warn!(component = $component, reason = $reason, $($arg)+);
    }};
}
