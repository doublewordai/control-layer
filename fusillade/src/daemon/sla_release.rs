//! When to stop sending a request's spillover tolerations.
//!
//! The daemon marks dispatched requests with tolerations (`[]` keeps them off
//! a paid spillover tier). Batch normally waits and retries rather than
//! spilling to the paid tier, but near SLA failure, meeting the deadline
//! matters more than the cost, so a request is sent without tolerations (it
//! then falls back to the policy default and may spill) when:
//!
//! - it is past its deadline, or inside the batch-claim deadline ramp (the
//!   floor, always), or
//! - with [`SlaReleaseConfig`] enabled, its deadline is before its model's
//!   release cutoff: the deadline before which our own workers are projected
//!   to miss.
//!
//! The claim query decides (and records the decision as
//! `requests.dispatched_tolerated`), so every replica decides from the same
//! rows. The cutoffs come from one daemon per refresh interval (see
//! [`run_release_leader`]), computed from tolerated completions in the
//! `requests` table (`fusillade_core::release`).

use std::time::Duration;

use chrono::{DateTime, Utc};
use fusillade_core::manager::DaemonStorage;
use fusillade_core::release::{ReleaseCutoffParams, TolerationsReleaseSettings};
use fusillade_core::request::TolerationsRelease;
use metrics::gauge;
use tokio_util::sync::CancellationToken;

/// Configuration for releasing tolerations on an SLA projection.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SlaReleaseConfig {
    /// How often the release cutoffs are recomputed, in seconds. One daemon
    /// computes per interval; slow on purpose, so a model's release decision
    /// does not flap.
    pub refresh_interval_secs: u64,
    /// Trailing window of tolerated completions the throughput is read from,
    /// in seconds.
    pub window_secs: u64,
    /// Fraction of each deadline's remaining time kept as margin: a deadline
    /// is on track if the work ahead of it finishes within
    /// `(1 - safety_margin)` of the time left.
    pub safety_margin: f64,
    /// Tolerated successful completions a model needs in the window before it
    /// gets a cutoff; below this only the deadline floor releases.
    pub min_samples: u64,
}

impl Default for SlaReleaseConfig {
    fn default() -> Self {
        Self {
            refresh_interval_secs: 300,
            window_secs: 900,
            safety_margin: 0.1,
            min_samples: 20,
        }
    }
}

impl SlaReleaseConfig {
    fn refresh_interval(&self) -> Duration {
        Duration::from_secs(self.refresh_interval_secs.max(1))
    }

    /// Cutoffs older than three refresh intervals are ignored by the claim
    /// (the computing daemons are failing): deadline floor only.
    pub(crate) fn cutoff_max_age_secs(&self) -> f64 {
        3.0 * self.refresh_interval().as_secs_f64()
    }

    pub(crate) fn params(&self) -> ReleaseCutoffParams {
        ReleaseCutoffParams {
            refresh_interval_secs: self.refresh_interval().as_secs_f64(),
            window_secs: self.window_secs.max(1) as f64,
            min_samples: i64::try_from(self.min_samples).unwrap_or(i64::MAX),
            safety_margin: self.safety_margin,
        }
    }
}

/// The settings the claim needs, from the daemon's configuration.
pub(crate) fn claim_settings(
    tolerations_enabled: bool,
    sla_release: Option<&SlaReleaseConfig>,
) -> TolerationsReleaseSettings {
    TolerationsReleaseSettings {
        tolerations_enabled,
        sla_release_enabled: sla_release.is_some(),
        cutoff_max_age_secs: sla_release.map_or(0.0, SlaReleaseConfig::cutoff_max_age_secs),
    }
}

/// The deadline floor, for claims that made no decision themselves (storage
/// that does not decide): released past the deadline, or within
/// `window_minutes ^ ramp_exponent` minutes of it, where the window runs from
/// `created_at` to the deadline (about 59 minutes for 24h and 10 for 1h at
/// 0.56), matching the batch-claim gate. No deadline (background): never. An
/// unreadable `created_at` keeps the tolerations until the deadline passes.
pub(crate) fn release_reason(
    deadline: Option<DateTime<Utc>>,
    created_at: Option<&str>,
    ramp_exponent: f64,
    now: DateTime<Utc>,
) -> Option<TolerationsRelease> {
    let deadline = deadline?;
    let remaining_secs = (deadline - now).num_milliseconds() as f64 / 1000.0;
    if remaining_secs <= 0.0 {
        return Some(TolerationsRelease::PastDeadline);
    }
    let created_at = created_at
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|created_at| created_at.with_timezone(&Utc))?;
    let window_secs = ((deadline - created_at).num_milliseconds() as f64 / 1000.0).max(0.0);
    (remaining_secs <= (window_secs / 60.0).powf(ramp_exponent) * 60.0)
        .then_some(TolerationsRelease::Ramp)
}

/// How often each daemon checks whether the cutoffs need computing: often
/// enough that a dead leader is replaced within a fraction of the interval.
fn leader_tick(config: &SlaReleaseConfig) -> Duration {
    (config.refresh_interval() / 10).clamp(Duration::from_secs(1), Duration::from_secs(30))
}

/// Recompute the release cutoffs when this daemon is the one to do it, and
/// publish the leader and cutoff-age metrics. Every daemon runs this; the
/// storage lets exactly one compute per refresh interval.
pub(crate) async fn run_release_leader<S>(
    storage: std::sync::Arc<S>,
    config: SlaReleaseConfig,
    shutdown: CancellationToken,
) where
    S: DaemonStorage + ?Sized,
{
    let mut interval = tokio::time::interval(leader_tick(&config));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = shutdown.cancelled() => break,
        }
        match storage.refresh_release_cutoffs(config.params()).await {
            Ok(Some(cutoffs)) => {
                gauge!("fusillade_release_leader").set(1.0);
                for cutoff in &cutoffs {
                    gauge!("fusillade_perceived_throughput", "model" => cutoff.model.clone())
                        .set(cutoff.throughput);
                    if cutoff.throughput > 0.0 {
                        gauge!("fusillade_projected_backlog_seconds", "model" => cutoff.model.clone())
                            .set(cutoff.backlog_requests as f64 / cutoff.throughput);
                    }
                }
                tracing::debug!(models = cutoffs.len(), "Computed release cutoffs");
            }
            Ok(None) => gauge!("fusillade_release_leader").set(0.0),
            Err(error) => {
                gauge!("fusillade_release_leader").set(0.0);
                metrics::counter!("fusillade_sla_release_refresh_errors_total").increment(1);
                tracing::warn!(%error, "Failed to refresh release cutoffs");
            }
        }
        match storage.release_cutoff_ages().await {
            Ok(ages) => {
                for (model, age) in ages {
                    gauge!("fusillade_release_cutoff_age_seconds", "model" => model).set(age);
                }
            }
            Err(error) => tracing::debug!(%error, "Failed to read release cutoff ages"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(raw: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(raw)
            .unwrap()
            .with_timezone(&Utc)
    }

    const CREATED: &str = "2026-10-08T00:00:00.000000Z";

    #[test]
    fn ramp_and_deadline_floor() {
        let created = ts(CREATED);
        let day = created + chrono::Duration::hours(24);
        let hour = created + chrono::Duration::hours(1);
        let at = |deadline: DateTime<Utc>, minutes_left: i64| {
            deadline - chrono::Duration::minutes(minutes_left)
        };
        let reason = |deadline, created_at, now| release_reason(deadline, created_at, 0.56, now);

        // 24h window at 0.56: 1440 ^ 0.56 ~= 58.7 minutes.
        assert_eq!(reason(Some(day), Some(CREATED), at(day, 600)), None);
        assert_eq!(reason(Some(day), Some(CREATED), at(day, 60)), None);
        assert_eq!(
            reason(Some(day), Some(CREATED), at(day, 58)),
            Some(TolerationsRelease::Ramp)
        );
        // 1h window at 0.56: 60 ^ 0.56 ~= 9.9 minutes.
        assert_eq!(reason(Some(hour), Some(CREATED), at(hour, 11)), None);
        assert_eq!(
            reason(Some(hour), Some(CREATED), at(hour, 9)),
            Some(TolerationsRelease::Ramp)
        );
        // Past the deadline (retry grace): released whatever the window.
        assert_eq!(
            reason(Some(day), Some(CREATED), at(day, -1)),
            Some(TolerationsRelease::PastDeadline)
        );
        assert_eq!(
            reason(Some(day), None, at(day, -1)),
            Some(TolerationsRelease::PastDeadline)
        );
        // No deadline (background): never released.
        assert_eq!(reason(None, Some(CREATED), at(day, -1)), None);
        // Unreadable created_at: kept until the deadline passes.
        assert_eq!(reason(Some(day), Some("yesterday"), at(day, 1)), None);
    }

    #[test]
    fn config_maps_to_claim_settings_and_params() {
        let config = SlaReleaseConfig::default();
        assert_eq!(
            config.cutoff_max_age_secs(),
            900.0,
            "three 5-minute intervals"
        );
        let params = config.params();
        assert_eq!(params.refresh_interval_secs, 300.0);
        assert_eq!(params.window_secs, 900.0);
        assert_eq!(params.min_samples, 20);
        assert_eq!(leader_tick(&config), Duration::from_secs(30));

        let off = claim_settings(true, None);
        assert!(off.tolerations_enabled && !off.sla_release_enabled);
        let on = claim_settings(true, Some(&config));
        assert!(on.sla_release_enabled);
        assert_eq!(on.cutoff_max_age_secs, 900.0);
    }
}
