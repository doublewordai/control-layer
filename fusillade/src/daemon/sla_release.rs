//! When to stop sending a request's scheduling tolerations.
//!
//! The daemon marks dispatched requests with scheduling tolerations that the
//! upstream inference backend understands; a backend may, for example, use
//! them to keep batch work off capacity reserved for other traffic. Near SLA
//! failure, meeting the deadline matters more than where the request runs, so
//! it is sent without tolerations (the backend may then schedule it anywhere)
//! when it is past its deadline or inside the batch-claim deadline ramp. The
//! claim query decides; [`release_reason`] is the same floor for claim paths
//! that make no decision.

use chrono::{DateTime, Utc};
use fusillade_core::request::TolerationsRelease;

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
}
