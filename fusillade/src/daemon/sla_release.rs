//! When to stop sending a request's spillover tolerations.
//!
//! The daemon marks dispatched requests with tolerations (`[]` keeps them off
//! a paid spillover tier). Batch normally waits and retries rather than
//! spilling to the paid tier, but near SLA failure, meeting the deadline
//! matters more than the cost, so the daemon sends a request without
//! tolerations (it then falls back to the policy default and may spill) when:
//!
//! - it is past its deadline, or inside the batch-claim deadline ramp
//!   (always, the floor), or
//! - with [`SlaReleaseConfig`] enabled, the model's own workers are projected
//!   not to reach it in time: `work_ahead / throughput` exceeds the time left
//!   minus a safety margin.
//!
//! `work_ahead` comes from the database (outstanding requests whose deadline
//! is no later than this one, refreshed every few seconds for all models in
//! one query) and `throughput` from [`ThroughputEstimator`], which only counts
//! requests dispatched with tolerations, i.e. served by our own workers.

use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

/// Configuration for releasing tolerations on a throughput projection.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SlaReleaseConfig {
    /// Half-life of the throughput estimate, in seconds.
    pub half_life_secs: f64,
    /// Margin before the deadline, as a fraction of the request's completion
    /// window: release once the projected finish is later than
    /// `deadline - safety_margin * window`.
    pub safety_margin: f64,
    /// Successful tolerated completions a model needs before its estimate is
    /// trusted. Below this only the deadline ramp releases.
    pub min_samples: u64,
    /// How often the outstanding-work snapshot is refreshed from the
    /// database, in milliseconds.
    pub refresh_interval_ms: u64,
}

impl Default for SlaReleaseConfig {
    fn default() -> Self {
        Self {
            half_life_secs: 600.0,
            safety_margin: 0.1,
            min_samples: 20,
            refresh_interval_ms: 15_000,
        }
    }
}

/// Why a request was sent without its tolerations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReleaseReason {
    /// Inside the batch-claim deadline ramp.
    Ramp,
    /// Already past its deadline (retry grace).
    PastDeadline,
    /// Projected to miss its deadline waiting for our own workers.
    SlaProjection,
}

impl ReleaseReason {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Ramp => "ramp",
            Self::PastDeadline => "past_deadline",
            Self::SlaProjection => "sla_projection",
        }
    }
}

/// The projection inputs for one request's model, when available.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Projection {
    /// Outstanding requests of the model due no later than this request.
    pub work_ahead: f64,
    /// Successful completions per second our own workers are delivering.
    pub throughput: f64,
    /// [`SlaReleaseConfig::safety_margin`].
    pub safety_margin: f64,
}

/// Decide whether to send a request without its tolerations, and why.
///
/// The ramp: released once the time left is within `window_minutes ^
/// ramp_exponent` minutes, where the window runs from `created_at` to the
/// deadline (about 59 minutes for 24h and 10 for 1h at 0.56), matching the
/// batch-claim gate. Past the deadline: always released. No deadline
/// (background): never. With a projection, also released when
/// `work_ahead / throughput > remaining - safety_margin * window`. An
/// unreadable `created_at` disables the ramp and the projection (both need
/// the window) until the deadline passes.
pub(crate) fn release_reason(
    deadline: Option<DateTime<Utc>>,
    created_at: Option<&str>,
    ramp_exponent: f64,
    now: DateTime<Utc>,
    projection: Option<Projection>,
) -> Option<ReleaseReason> {
    let deadline = deadline?;
    let remaining_secs = (deadline - now).num_milliseconds() as f64 / 1000.0;
    if remaining_secs <= 0.0 {
        return Some(ReleaseReason::PastDeadline);
    }
    let created_at = created_at
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|created_at| created_at.with_timezone(&Utc))?;
    let window_secs = ((deadline - created_at).num_milliseconds() as f64 / 1000.0).max(0.0);
    if remaining_secs <= (window_secs / 60.0).powf(ramp_exponent) * 60.0 {
        return Some(ReleaseReason::Ramp);
    }
    let projection = projection?;
    let projected_secs = if projection.throughput > 0.0 {
        projection.work_ahead / projection.throughput
    } else {
        f64::INFINITY
    };
    (projected_secs > remaining_secs - projection.safety_margin * window_secs)
        .then_some(ReleaseReason::SlaProjection)
}

/// Per-model throughput of our own workers, from requests dispatched with
/// tolerations (which Dynamo never sends to a spillover tier).
///
/// Measured as successful completions per in-flight-second: exponentially
/// decayed completions divided by the exponentially decayed integral of
/// tolerated requests in flight. By Little's law that is `1 / latency`, so
/// multiplying it by the model's concurrency gives its throughput; the
/// daemon uses the database-wide in-flight count, which turns a per-pod
/// measurement into a whole-deployment estimate. Idle time decays both sums
/// equally, so an idle model keeps its last rate instead of drifting to zero.
#[derive(Debug)]
pub(crate) struct ThroughputEstimator {
    tau_secs: f64,
    min_samples: u64,
    models: dashmap::DashMap<String, Mutex<ModelRate>>,
}

#[derive(Debug)]
struct ModelRate {
    completions: f64,
    slot_secs: f64,
    in_flight: u64,
    total_completions: u64,
    last: Instant,
}

impl ModelRate {
    fn advance(&mut self, now: Instant, tau_secs: f64) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        if dt <= 0.0 {
            return;
        }
        let decay = (-dt / tau_secs).exp();
        self.completions *= decay;
        // Integral over dt of in_flight * exp(-(dt - t) / tau).
        self.slot_secs = self.slot_secs * decay + self.in_flight as f64 * tau_secs * (1.0 - decay);
        self.last = now;
    }
}

impl ThroughputEstimator {
    pub(crate) fn new(half_life_secs: f64, min_samples: u64) -> Self {
        Self {
            tau_secs: half_life_secs.max(1.0) / std::f64::consts::LN_2,
            min_samples,
            models: dashmap::DashMap::new(),
        }
    }

    fn with_model<T>(&self, model: &str, now: Instant, f: impl FnOnce(&mut ModelRate) -> T) -> T {
        let entry = self.models.entry(model.to_owned()).or_insert_with(|| {
            Mutex::new(ModelRate {
                completions: 0.0,
                slot_secs: 0.0,
                in_flight: 0,
                total_completions: 0,
                last: now,
            })
        });
        let mut rate = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        rate.advance(now, self.tau_secs);
        f(&mut rate)
    }

    /// A tolerated request was dispatched.
    pub(crate) fn started(&self, model: &str, now: Instant) {
        self.with_model(model, now, |rate| rate.in_flight += 1);
    }

    /// A tolerated request finished; only successes count as throughput.
    pub(crate) fn finished(&self, model: &str, succeeded: bool, now: Instant) {
        self.with_model(model, now, |rate| {
            rate.in_flight = rate.in_flight.saturating_sub(1);
            if succeeded {
                rate.completions += 1.0;
                rate.total_completions += 1;
            }
        });
    }

    /// Tolerated requests of this model in flight on this daemon.
    pub(crate) fn local_in_flight(&self, model: &str) -> u64 {
        self.models
            .get(model)
            .map(|rate| rate.lock().unwrap_or_else(|p| p.into_inner()).in_flight)
            .unwrap_or(0)
    }

    /// Successful completions per in-flight-second, or `None` until the model
    /// has `min_samples` successful tolerated completions.
    pub(crate) fn per_slot_rate(&self, model: &str, now: Instant) -> Option<f64> {
        self.models.get(model)?;
        self.with_model(model, now, |rate| {
            (rate.total_completions >= self.min_samples && rate.slot_secs > 0.0)
                .then(|| rate.completions / rate.slot_secs)
        })
    }

    /// Track one tolerated dispatch; dropping the guard records the finish.
    pub(crate) fn track(self: &Arc<Self>, model: &str) -> TrackedDispatch {
        self.started(model, Instant::now());
        TrackedDispatch {
            estimator: self.clone(),
            model: model.to_owned(),
            succeeded: false,
        }
    }
}

/// Guard for a tolerated dispatch. Set `succeeded` on a successful completion;
/// the drop records the finish whatever the outcome (failure, retry, abort).
pub(crate) struct TrackedDispatch {
    estimator: Arc<ThroughputEstimator>,
    model: String,
    pub(crate) succeeded: bool,
}

impl Drop for TrackedDispatch {
    fn drop(&mut self) {
        self.estimator
            .finished(&self.model, self.succeeded, Instant::now());
    }
}

/// Deadline offsets (seconds from the snapshot) at which outstanding work is
/// counted, cumulatively: entry `i` counts every outstanding request due
/// before `refreshed_at + LADDER_SECS[i]`, overdue ones included.
pub(crate) const LADDER_SECS: [i64; 20] = [
    0, 60, 300, 600, 900, 1_200, 1_800, 2_700, 3_600, 5_400, 7_200, 10_800, 14_400, 21_600, 28_800,
    43_200, 57_600, 72_000, 86_400, 172_800,
];

/// One model's outstanding work, from the last refresh.
#[derive(Debug, Clone, Default)]
pub(crate) struct ModelBacklog {
    /// Cumulative counts at [`LADDER_SECS`].
    pub cumulative: Vec<f64>,
    /// Requests claimed or processing across every daemon.
    pub global_in_flight: f64,
}

impl ModelBacklog {
    /// Outstanding requests due no later than `offset_secs` after the
    /// snapshot, interpolated linearly between ladder points.
    pub(crate) fn work_ahead(&self, offset_secs: f64) -> f64 {
        let Some(&last) = self.cumulative.last() else {
            return 0.0;
        };
        if offset_secs <= 0.0 {
            return self.cumulative[0];
        }
        for i in 1..self.cumulative.len().min(LADDER_SECS.len()) {
            let hi = LADDER_SECS[i] as f64;
            if offset_secs <= hi {
                let lo = LADDER_SECS[i - 1] as f64;
                let (c_lo, c_hi) = (self.cumulative[i - 1], self.cumulative[i]);
                return c_lo + (c_hi - c_lo) * (offset_secs - lo) / (hi - lo);
            }
        }
        last
    }

    /// Every outstanding request in the snapshot.
    pub(crate) fn total(&self) -> f64 {
        self.cumulative.last().copied().unwrap_or(0.0)
    }
}

/// The last outstanding-work snapshot for every model.
#[derive(Debug, Default)]
pub(crate) struct BacklogSnapshot {
    pub refreshed_at: Option<DateTime<Utc>>,
    pub models: std::collections::HashMap<String, ModelBacklog>,
}

/// Shared state for the projection: the estimator, the snapshot and config.
#[derive(Debug)]
pub(crate) struct SlaRelease {
    pub config: SlaReleaseConfig,
    pub estimator: Arc<ThroughputEstimator>,
    pub backlog: RwLock<BacklogSnapshot>,
}

impl SlaRelease {
    pub(crate) fn new(config: SlaReleaseConfig) -> Self {
        Self {
            estimator: Arc::new(ThroughputEstimator::new(
                config.half_life_secs,
                config.min_samples,
            )),
            backlog: RwLock::new(BacklogSnapshot::default()),
            config,
        }
    }

    /// Our workers' throughput for `model`: per-slot rate times the
    /// database-wide in-flight count (at least this daemon's own, and one).
    pub(crate) fn throughput(
        &self,
        model: &str,
        global_in_flight: f64,
        now: Instant,
    ) -> Option<f64> {
        let per_slot = self.estimator.per_slot_rate(model, now)?;
        let concurrency = global_in_flight
            .max(self.estimator.local_in_flight(model) as f64)
            .max(1.0);
        Some(per_slot * concurrency)
    }

    /// Projection inputs for a request of `model` due at `deadline`, or
    /// `None` without a snapshot or a trusted estimate (ramp-only then).
    pub(crate) fn projection(
        &self,
        model: &str,
        deadline: DateTime<Utc>,
        now: Instant,
    ) -> Option<Projection> {
        let snapshot = self.backlog.read().unwrap_or_else(|p| p.into_inner());
        let refreshed_at = snapshot.refreshed_at?;
        // A snapshot the refresher has not replaced for ten intervals (the
        // database is failing) is too old to project from: ramp-only.
        let max_age = chrono::Duration::from_std(self.refresh_interval() * 10).ok()?;
        if Utc::now() - refreshed_at > max_age {
            return None;
        }
        let backlog = snapshot.models.get(model)?;
        let throughput = self.throughput(model, backlog.global_in_flight, now)?;
        let offset = (deadline - refreshed_at).num_milliseconds() as f64 / 1000.0;
        Some(Projection {
            work_ahead: backlog.work_ahead(offset),
            throughput,
            safety_margin: self.config.safety_margin,
        })
    }

    /// Install a fresh snapshot and publish the per-model gauges.
    pub(crate) fn install(
        &self,
        refreshed_at: DateTime<Utc>,
        models: std::collections::HashMap<String, ModelBacklog>,
    ) {
        let now = Instant::now();
        for (model, backlog) in &models {
            if let Some(throughput) = self.throughput(model, backlog.global_in_flight, now) {
                metrics::gauge!("fusillade_perceived_throughput", "model" => model.clone())
                    .set(throughput);
                if throughput > 0.0 {
                    metrics::gauge!("fusillade_projected_backlog_seconds", "model" => model.clone())
                        .set(backlog.total() / throughput);
                }
            }
        }
        let mut snapshot = self.backlog.write().unwrap_or_else(|p| p.into_inner());
        snapshot.refreshed_at = Some(refreshed_at);
        snapshot.models = models;
    }

    pub(crate) fn refresh_interval(&self) -> Duration {
        Duration::from_millis(self.config.refresh_interval_ms.max(1_000))
    }
}

/// Read every model's outstanding work from storage in two count queries:
/// the cumulative deadline ladder over pending, claimed and processing rows,
/// and the claimed-or-processing total (the deployment-wide concurrency).
/// Background and realtime rows are excluded.
pub(crate) async fn read_backlog<S: fusillade_core::manager::Storage + ?Sized>(
    storage: &S,
) -> crate::Result<std::collections::HashMap<String, ModelBacklog>> {
    use fusillade_core::request::ServiceTierFilter;

    // Daemon work only: realtime rows (`priority` tier, processed by the
    // proxy, never claimed) are excluded; background is excluded by storage.
    let tiers = ServiceTierFilter::Exclude(vec![Some("priority".to_string())]);
    let ladder: Vec<(String, Option<i64>, i64)> = LADDER_SECS
        .iter()
        .map(|secs| (secs.to_string(), None, *secs))
        .collect();
    let outstanding = storage
        .get_pending_request_counts_by_model_and_window(
            &ladder,
            &["pending".into(), "claimed".into(), "processing".into()],
            &[],
            &tiers,
            false,
        )
        .await?;
    // Ten years: every in-flight row, whatever its deadline.
    let in_flight = storage
        .get_pending_request_counts_by_model_and_window(
            &[("all".into(), None, 315_360_000)],
            &["claimed".into(), "processing".into()],
            &[],
            &tiers,
            false,
        )
        .await?;

    let mut models = std::collections::HashMap::new();
    for (model, counts) in outstanding {
        let cumulative = LADDER_SECS
            .iter()
            .map(|secs| counts.get(&secs.to_string()).copied().unwrap_or(0) as f64)
            .collect();
        let global_in_flight = in_flight
            .get(&model)
            .and_then(|counts| counts.get("all"))
            .copied()
            .unwrap_or(0) as f64;
        models.insert(
            model,
            ModelBacklog {
                cumulative,
                global_in_flight,
            },
        );
    }
    Ok(models)
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
        let reason =
            |deadline, created_at, now| release_reason(deadline, created_at, 0.56, now, None);

        // 24h window at 0.56: 1440 ^ 0.56 ~= 58.7 minutes.
        assert_eq!(reason(Some(day), Some(CREATED), at(day, 600)), None);
        assert_eq!(reason(Some(day), Some(CREATED), at(day, 60)), None);
        assert_eq!(
            reason(Some(day), Some(CREATED), at(day, 58)),
            Some(ReleaseReason::Ramp)
        );
        // 1h window at 0.56: 60 ^ 0.56 ~= 9.9 minutes.
        assert_eq!(reason(Some(hour), Some(CREATED), at(hour, 11)), None);
        assert_eq!(
            reason(Some(hour), Some(CREATED), at(hour, 9)),
            Some(ReleaseReason::Ramp)
        );
        // Past the deadline (retry grace): released whatever the window.
        assert_eq!(
            reason(Some(day), Some(CREATED), at(day, -1)),
            Some(ReleaseReason::PastDeadline)
        );
        assert_eq!(
            reason(Some(day), None, at(day, -1)),
            Some(ReleaseReason::PastDeadline)
        );
        // No deadline (background): never released.
        assert_eq!(reason(None, Some(CREATED), at(day, -1)), None);
        // Unreadable created_at: kept until the deadline passes.
        assert_eq!(reason(Some(day), Some("yesterday"), at(day, 1)), None);
    }

    #[test]
    fn projection_decides_between_the_floor_and_far_away() {
        let created = ts(CREATED);
        let day = created + chrono::Duration::hours(24);
        // 10 hours left, 10% margin of a 24h window = 2.4h, so the budget is 7.6h.
        let now = day - chrono::Duration::hours(10);
        let decide = |work_ahead: f64, throughput: f64| {
            release_reason(
                Some(day),
                Some(CREATED),
                0.56,
                now,
                Some(Projection {
                    work_ahead,
                    throughput,
                    safety_margin: 0.1,
                }),
            )
        };
        // On track: 1000 requests at 1/s is under 17 minutes.
        assert_eq!(decide(1_000.0, 1.0), None);
        // Behind: 100k requests at 1/s is about 28h.
        assert_eq!(decide(100_000.0, 1.0), Some(ReleaseReason::SlaProjection));
        // Just inside and just outside the margin: 7.6h = 27360s.
        assert_eq!(decide(27_000.0, 1.0), None);
        assert_eq!(decide(28_000.0, 1.0), Some(ReleaseReason::SlaProjection));
        // Our workers deliver nothing while work waits: released.
        assert_eq!(decide(10.0, 0.0), Some(ReleaseReason::SlaProjection));

        // The floor wins whatever the projection says.
        let in_ramp = day - chrono::Duration::minutes(30);
        assert_eq!(
            release_reason(
                Some(day),
                Some(CREATED),
                0.56,
                in_ramp,
                Some(Projection {
                    work_ahead: 0.0,
                    throughput: 1_000.0,
                    safety_margin: 0.1
                })
            ),
            Some(ReleaseReason::Ramp)
        );
        // No deadline keeps the tolerations even far behind.
        assert_eq!(
            release_reason(
                None,
                Some(CREATED),
                0.56,
                now,
                Some(Projection {
                    work_ahead: 1e9,
                    throughput: 0.0,
                    safety_margin: 0.1
                })
            ),
            None
        );
    }

    #[test]
    fn estimator_is_cold_until_min_samples_then_measures_per_slot_rate() {
        let estimator = ThroughputEstimator::new(600.0, 3);
        let t0 = Instant::now();
        assert_eq!(estimator.per_slot_rate("m", t0), None, "never seen");

        // Two slots busy, each finishing a request every 2s: 1 completion/s
        // overall, 0.5 per in-flight-second.
        estimator.started("m", t0);
        estimator.started("m", t0);
        let mut t = t0;
        for _ in 0..2 {
            t += Duration::from_secs(1);
            estimator.finished("m", true, t);
            estimator.started("m", t);
        }
        assert_eq!(
            estimator.per_slot_rate("m", t),
            None,
            "two samples, three required"
        );
        for _ in 0..200 {
            t += Duration::from_secs(1);
            estimator.finished("m", true, t);
            estimator.started("m", t);
        }
        let rate = estimator.per_slot_rate("m", t).unwrap();
        assert!((rate - 0.5).abs() < 0.01, "{rate}");
        assert_eq!(estimator.local_in_flight("m"), 2);
    }

    #[test]
    fn estimator_counts_only_successes_and_decays_toward_the_new_rate() {
        let estimator = ThroughputEstimator::new(60.0, 1);
        let t0 = Instant::now();
        let mut t = t0;
        // One slot, a success every second: rate 1 per slot-second.
        estimator.started("m", t);
        for _ in 0..600 {
            t += Duration::from_secs(1);
            estimator.finished("m", true, t);
            estimator.started("m", t);
        }
        assert!((estimator.per_slot_rate("m", t).unwrap() - 1.0).abs() < 0.01);

        // The model slows: one success every 4s, failures in between.
        for i in 0..600 {
            t += Duration::from_secs(1);
            estimator.finished("m", i % 4 == 3, t);
            estimator.started("m", t);
        }
        let rate = estimator.per_slot_rate("m", t).unwrap();
        assert!((rate - 0.25).abs() < 0.03, "ten half-lives later: {rate}");

        // Idle time decays both sums alike: the rate holds.
        estimator.finished("m", false, t);
        let later = t + Duration::from_secs(3_600);
        let idle_rate = estimator.per_slot_rate("m", later).unwrap();
        assert!((idle_rate - rate).abs() < 0.03, "{idle_rate} vs {rate}");
    }

    #[test]
    fn tracked_dispatch_records_the_outcome_on_drop() {
        let estimator = Arc::new(ThroughputEstimator::new(600.0, 1));
        {
            let mut tracked = estimator.track("m");
            assert_eq!(estimator.local_in_flight("m"), 1);
            tracked.succeeded = true;
        }
        assert_eq!(estimator.local_in_flight("m"), 0);
        drop(estimator.track("m"));
        let rate = estimator.models.get("m").unwrap();
        assert_eq!(
            rate.lock().unwrap().total_completions,
            1,
            "the failure did not count"
        );
    }

    #[test]
    fn work_ahead_interpolates_the_cumulative_ladder() {
        let mut cumulative = vec![0.0; LADDER_SECS.len()];
        // 10 overdue, 100 more due by 60s, 1000 by 300s, flat after.
        cumulative[0] = 10.0;
        cumulative[1] = 110.0;
        for count in cumulative.iter_mut().skip(2) {
            *count = 1_110.0;
        }
        let backlog = ModelBacklog {
            cumulative,
            global_in_flight: 4.0,
        };
        assert_eq!(backlog.work_ahead(-5.0), 10.0);
        assert_eq!(backlog.work_ahead(30.0), 60.0);
        assert_eq!(backlog.work_ahead(60.0), 110.0);
        assert_eq!(backlog.work_ahead(180.0), 610.0);
        assert_eq!(backlog.work_ahead(1e9), 1_110.0);
        assert_eq!(backlog.total(), 1_110.0);
    }

    #[test]
    fn projection_needs_a_snapshot_and_a_warm_estimate() {
        let release = SlaRelease::new(SlaReleaseConfig {
            min_samples: 1,
            ..Default::default()
        });
        let deadline = Utc::now() + chrono::Duration::hours(1);
        let now = Instant::now();
        assert!(
            release.projection("m", deadline, now).is_none(),
            "no snapshot"
        );

        release.install(
            Utc::now(),
            std::collections::HashMap::from([(
                "m".to_string(),
                ModelBacklog {
                    cumulative: vec![50.0; LADDER_SECS.len()],
                    global_in_flight: 10.0,
                },
            )]),
        );
        assert!(
            release.projection("m", deadline, now).is_none(),
            "cold estimator"
        );

        release.estimator.started("m", now);
        release
            .estimator
            .finished("m", true, now + Duration::from_secs(2));
        let projection = release
            .projection("m", deadline, now + Duration::from_secs(2))
            .unwrap();
        assert_eq!(projection.work_ahead, 50.0);
        // 1 completion over 2 slot-seconds, times 10 in flight across daemons.
        assert!(
            (projection.throughput - 5.0).abs() < 0.01,
            "{}",
            projection.throughput
        );
    }
}
