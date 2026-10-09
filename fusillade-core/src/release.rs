//! Release cutoffs for the daemon's spillover tolerations.
//!
//! The daemon sends batch requests with spillover tolerations so Dynamo keeps
//! them on our own workers. Batch normally waits and retries rather than
//! spilling to the paid tier, but when our workers will not reach a request
//! before its deadline, meeting the SLA matters more than the cost. A leader
//! computes, per model, the deadline before which requests are projected to
//! miss on our own workers (`release_before_deadline`); the claim releases
//! the tolerations of requests due before it.

use chrono::{DateTime, Utc};

/// Deadline offsets (seconds from now) at which outstanding work is counted,
/// cumulatively: entry `i` counts every outstanding request due before
/// `now + RELEASE_LADDER_SECS[i]`, overdue ones included.
pub const RELEASE_LADDER_SECS: [i64; 20] = [
    0, 60, 300, 600, 900, 1_200, 1_800, 2_700, 3_600, 5_400, 7_200, 10_800, 14_400, 21_600, 28_800,
    43_200, 57_600, 72_000, 86_400, 172_800,
];

/// Settings the daemon hands its storage for the claim-time decision.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TolerationsReleaseSettings {
    /// The daemon sends spillover tolerations; without them the claim makes
    /// no decision and leaves `dispatched_tolerated` unset.
    pub tolerations_enabled: bool,
    /// Release on the leader's cutoffs, not only on the deadline floor.
    pub sla_release_enabled: bool,
    /// Cutoffs older than this are ignored (deadline floor only).
    pub cutoff_max_age_secs: f64,
}

/// Inputs to one cutoff computation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReleaseCutoffParams {
    /// Skip the computation while the newest cutoff is younger than this:
    /// one replica computes per interval, whichever ticks first.
    pub refresh_interval_secs: f64,
    /// Trailing window of tolerated completions the throughput is read from.
    pub window_secs: f64,
    /// Tolerated completions a model needs in the window before it gets a
    /// cutoff; below this only the deadline floor releases.
    pub min_samples: i64,
    /// Fraction of each deadline's remaining time kept as margin.
    pub safety_margin: f64,
}

/// One model's computed cutoff.
#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseCutoff {
    pub model: String,
    /// Requests due before this are released. `None`: nothing beyond the
    /// deadline floor (on track, or too few samples to project).
    pub release_before_deadline: Option<DateTime<Utc>>,
    /// Our workers' completions per second (0 when below `min_samples`).
    pub throughput: f64,
    /// Outstanding requests (pending, claimed, processing) of the model.
    pub backlog_requests: i64,
    /// Tolerated successful completions in the window.
    pub samples: i64,
    pub computed_at: DateTime<Utc>,
}

/// A stored cutoff as every daemon reads it back, for metrics: each replica
/// publishes the same values, so no pod reports figures from a computation it
/// no longer leads.
#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseCutoffStatus {
    pub model: String,
    /// Seconds since the cutoff was computed.
    pub age_secs: f64,
    pub throughput: f64,
    pub backlog_requests: i64,
}

/// Throughput of our own workers from tolerated completions in the window:
/// `count / sum(completed_at - started_at)` is completions per in-flight
/// second (`1 / latency`, by Little's law; idle time is in neither sum), and
/// times the deployment-wide in-flight count (at least one) it is
/// completions per second.
pub fn tolerated_throughput(
    samples: i64,
    busy_secs: f64,
    in_flight: i64,
    min_samples: i64,
) -> Option<f64> {
    (samples >= min_samples.max(1) && busy_secs > 0.0)
        .then(|| samples as f64 / busy_secs * in_flight.max(1) as f64)
}

/// The shortest prefix of the deadline-ordered queue to release so that our
/// workers finish everything after it in time.
///
/// Requests are served roughly in deadline order. If the requests due before
/// `now + L[i]` are released (they may spill and stop occupying our workers),
/// the one due at `now + L[j]` waits for `W[j] - W[i]` requests ahead of it,
/// which takes `(W[j] - W[i]) / throughput`. The cutoff is the smallest ladder
/// point `L[i]` such that every later point finishes within
/// `(1 - safety_margin) * L[j]`. Returns the cutoff's offset in seconds, or
/// `None` when nothing beyond the overdue requests needs releasing.
///
/// `cumulative` holds the outstanding counts at [`RELEASE_LADDER_SECS`].
pub fn release_cutoff_offset(
    cumulative: &[f64],
    throughput: f64,
    safety_margin: f64,
) -> Option<i64> {
    let n = cumulative.len().min(RELEASE_LADDER_SECS.len());
    if n == 0 || throughput <= 0.0 {
        return None;
    }
    let keep = (1.0 - safety_margin).clamp(0.0, 1.0);
    let on_track_after = |i: usize| {
        (i + 1..n).all(|j| {
            (cumulative[j] - cumulative[i]) / throughput <= keep * RELEASE_LADDER_SECS[j] as f64
        })
    };
    let i = (0..n).find(|&i| on_track_after(i)).unwrap_or(n - 1);
    (i > 0).then_some(RELEASE_LADDER_SECS[i])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ladder(counts: &[(i64, f64)]) -> Vec<f64> {
        // Cumulative counts: each (offset, added) adds requests due by offset.
        RELEASE_LADDER_SECS
            .iter()
            .map(|secs| {
                counts
                    .iter()
                    .filter(|(due, _)| due <= secs)
                    .map(|(_, n)| n)
                    .sum()
            })
            .collect()
    }

    #[test]
    fn throughput_is_a_ratio_of_sums_scaled_by_concurrency() {
        // 300 completions over 600 busy seconds: 0.5 per slot-second.
        assert_eq!(tolerated_throughput(300, 600.0, 8, 20), Some(4.0));
        // Nothing in flight right now still counts as one slot.
        assert_eq!(tolerated_throughput(300, 600.0, 0, 20), Some(0.5));
        assert_eq!(
            tolerated_throughput(19, 600.0, 8, 20),
            None,
            "below min_samples"
        );
        assert_eq!(
            tolerated_throughput(0, 0.0, 8, 0),
            None,
            "no samples at all"
        );
    }

    #[test]
    fn on_track_needs_no_cutoff() {
        // 1000 requests due in 24h at 1/s: about 17 minutes of work.
        let cumulative = ladder(&[(86_400, 1_000.0)]);
        assert_eq!(release_cutoff_offset(&cumulative, 1.0, 0.1), None);
    }

    #[test]
    fn behind_releases_the_most_urgent_prefix() {
        // 3000 due within 1h, then 1000 due by 24h, at 0.5/s. Keeping
        // everything, the 1h work alone takes 6000s > 3240s. Releasing what is
        // due by 1h leaves the 24h work (2000s) comfortably on track.
        let cumulative = ladder(&[(3_600, 3_000.0), (86_400, 1_000.0)]);
        assert_eq!(release_cutoff_offset(&cumulative, 0.5, 0.1), Some(3_600));
        // Twice as fast, the 1h work fits (3000s <= 3240s): nothing released.
        assert_eq!(release_cutoff_offset(&cumulative, 1.0, 0.1), None);
    }

    #[test]
    fn hopeless_backlog_releases_through_the_last_point() {
        let cumulative = ladder(&[(600, 1e6), (172_800, 1e6)]);
        assert_eq!(release_cutoff_offset(&cumulative, 1.0, 0.1), Some(172_800));
    }

    #[test]
    fn overdue_work_alone_is_left_to_the_floor() {
        // Only overdue requests (offset 0): the past-deadline floor handles them.
        let cumulative = ladder(&[(0, 500.0)]);
        assert_eq!(release_cutoff_offset(&cumulative, 1.0, 0.1), None);
    }

    #[test]
    fn no_throughput_means_no_cutoff() {
        let cumulative = ladder(&[(600, 10.0)]);
        assert_eq!(release_cutoff_offset(&cumulative, 0.0, 0.1), None);
    }
}
