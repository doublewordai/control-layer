//! Release of held dispatches.
//!
//! The daemon can hold the batch and flex dispatches of opted-in models on the
//! capacity a scheduling-toleration list allows (fusillade's dispatch hold): a
//! held request carries the list, and the upstream inference backend may then
//! refuse it while that capacity is full. A held request is released, sent as
//! stored without the hold list, so the hold no longer limits where the
//! backend schedules it, when it would otherwise miss its completion window.
//!
//! Every daemon decides alike, without a leader: each one counts its own held
//! completions and held refusals in memory, flushes them into shared
//! per-minute counters (`hold_stats`), and on every refresh reads the
//! counters and the outstanding work back and computes each held model's
//! [`ModelHold`] with [`decide_model_hold`]. The claim queries receive the
//! result as array parameters ([`HoldClaim::claim_arrays`]) and apply it,
//! together with the deadline floor (past the deadline, or inside the
//! batch-claim ramp), to each row they claim. Nothing is written to the
//! request rows.
//!
//! The decision fails open: a model whose held dispatches were refused
//! throughout a whole window of holding, while none completed, has no own
//! capacity to wait for, and missing or stale decisions release everything,
//! because a missed completion window is the worse failure.
//!
//! A release must not feed itself. Released requests can still run on the
//! capacity the hold allows, where their completions are not counted, so
//! while a release is in the window the measured held rate understates the
//! capacity. Each daemon therefore remembers the rate it last measured with
//! nothing released ([`ModelHoldMemory`]) and projects with the larger of the
//! two until the window is clean again.

use chrono::{DateTime, Duration, Utc};

use crate::request::{DispatchTolerations, TolerationsRelease};

/// The cutoff passed for a model that has none: no deadline is at or before
/// it.
pub const NO_CUTOFF: DateTime<Utc> = DateTime::<Utc>::UNIX_EPOCH;

/// Width of one `hold_stats` counter row, in seconds: counters are kept per
/// minute, so how long a model has been held is known to within this.
pub const STATS_BUCKET_SECS: f64 = 60.0;

/// The shortest window the decision supports, in seconds. Counters are kept
/// per minute, so the window must span several of them: whether a model was
/// held for the whole window is only known to within a minute, and "refused
/// lately" needs a recent part of the window that a minute's rounding and a
/// flush delay fit in.
pub const MIN_WINDOW_SECS: u64 = 300;

/// The longest window the decision supports, in seconds: one day. Counters
/// are kept for two windows, and a longer window would no longer describe the
/// capacity a model has now.
pub const MAX_WINDOW_SECS: u64 = 86_400;

/// The recent part of the window a refusal must fall in for the model to
/// count as refused lately: this fraction of the window, and at least two
/// counter minutes, so a refusal a moment ago is in it whatever minute it was
/// counted in.
pub const RECENT_REFUSAL_FRACTION: f64 = 0.25;

/// How many windows a remembered clean rate is used for while releases keep
/// the window from being clean. Past that the measured rate is used even
/// though it is diluted: it errs toward releasing, which is the safe side.
pub const CLEAN_RATE_MAX_WINDOWS: f64 = 4.0;

/// Tuning of the per-model decision.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HoldParams {
    /// Trailing window of held completions and refusals, in seconds.
    pub window_secs: f64,
    /// Fraction of a deadline's projected work added as margin: a deadline
    /// `d` is behind when `work_due_by(d) / rate * (1 + safety_margin)`
    /// exceeds the time left until `d`.
    pub safety_margin: f64,
    /// Held completions a model needs before its own rate is trusted, unless
    /// it was refused throughout a whole window of holding (then whatever
    /// completed is its rate).
    pub min_samples: i64,
}

/// One model's counters and outstanding work, as read on a refresh.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModelHoldInputs {
    pub model: String,
    /// Held dispatches that completed successfully in the window.
    pub completed: i64,
    /// Held dispatches refused (503/529) in the window.
    pub refused: i64,
    /// The first counter minute in the window: the model has been held, and
    /// counted, since about then. `None` without counters. The rate is taken
    /// over this span, not the whole window, so a hold that began inside the
    /// window is not understated. Counters are written under the minute they
    /// were counted in, not the minute they were flushed in, so a delayed
    /// flush does not shorten this span.
    pub held_since: Option<DateTime<Utc>>,
    /// The last counter minute in the window with a refusal. `None` without
    /// refusals. A model counts as refused throughout the window only if it
    /// was refused lately too: one old refusal is not evidence that the
    /// capacity the hold allows is full now.
    pub last_refused: Option<DateTime<Utc>>,
    /// Pending requests of the model that are not yet overdue, cumulative by
    /// deadline: `(d, n)` means `n` requests are due at or before `d`.
    /// Ascending in `d` and `n`. Requests in flight are left out: by Little's
    /// law they amount to about one request latency of work, and some of
    /// them are released and running elsewhere. Overdue requests are left
    /// out because the claim releases them anyway.
    pub backlog: Vec<(DateTime<Utc>, i64)>,
}

impl ModelHoldInputs {
    /// Outstanding requests with a deadline.
    pub fn backlog_requests(&self) -> i64 {
        self.backlog.last().map_or(0, |(_, n)| *n)
    }
}

/// What the claim does with a held model's requests, beyond the deadline
/// floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelHold {
    /// Hold, releasing requests due at or before `cutoff` (`None`: only the
    /// deadline floor releases).
    Hold { cutoff: Option<DateTime<Utc>> },
    /// Release every request of the model.
    ReleaseAll(TolerationsRelease),
}

/// A model's decision and the figures behind it, for metrics.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelHoldDecision {
    pub model: String,
    pub hold: ModelHold,
    /// The own rate the decision used, in held completions per second (the
    /// raw held rate when there was too little to project from).
    pub own_rate: f64,
    pub backlog_requests: i64,
    /// Seconds to clear the backlog at `own_rate` (NaN without a rate).
    pub projected_backlog_secs: f64,
}

/// What one daemon remembers about a model between refreshes, so that a
/// release cannot feed itself (see the module docs). Each daemon keeps its
/// own; one that has just started remembers nothing and uses the measured
/// rate, which errs toward releasing.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ModelHoldMemory {
    /// The own rate last measured with no release in the window, and when.
    pub clean_rate: Option<(f64, DateTime<Utc>)>,
    /// When the model last had a cutoff or was released whole.
    pub last_release_at: Option<DateTime<Utc>>,
}

/// One model's decision, updating what the daemon remembers about it.
///
/// The held rate is `completed / held_secs`, where `held_secs` is how long
/// the model has been counted within the window. The model was refused
/// throughout the window when it has been counted for the whole window (to
/// within one counter minute) and its last refusal is recent (in the last
/// [`RECENT_REFUSAL_FRACTION`] of the window, and at least the last two
/// minutes). The rate is measured once `min_samples` held dispatches
/// completed, or once the model was refused throughout the window (then
/// whatever completed is the capacity the hold allows). While a release is in
/// the window, the larger of it and the remembered clean rate is used. Then:
///
/// - with a rate, the cutoff is [`release_cutoff`] of the outstanding work;
/// - refused throughout the window with nothing completed, and no clean rate
///   to go on: the capacity the hold allows is absent, so everything is
///   released ([`TolerationsRelease::NoOwnCapacity`]). With nothing completed
///   in the window, every counter minute in it has refusals, so the refusals
///   span the window from its first minute to its recent part. Refusals early
///   in a hold are not enough (a held refusal is immediate while a completion
///   takes a request's latency), nor is one old refusal on a quiet model;
/// - otherwise there is nothing to project from yet (a new hold, a slow first
///   window, or a quiet model that is never refused); only the deadline floor
///   releases.
pub fn decide_model_hold(
    inputs: &ModelHoldInputs,
    params: HoldParams,
    now: DateTime<Utc>,
    memory: &mut ModelHoldMemory,
) -> ModelHoldDecision {
    let window_secs = params.window_secs.max(1.0);
    let secs_since = |at: DateTime<Utc>| (now - at).num_milliseconds() as f64 / 1000.0;
    let held_secs = inputs
        .held_since
        .map_or(0.0, |since| secs_since(since).clamp(0.0, window_secs));
    let completed = inputs.completed.max(0);
    // Held for the whole window, to within the counters' minute: the first
    // counted minute starts up to a minute before the hold did. Never more
    // than half the window, so even a window shorter than the supported
    // minimum needs time held.
    let bucket_tolerance = STATS_BUCKET_SECS.min(window_secs / 2.0);
    let whole_window = inputs.held_since.is_some() && held_secs + bucket_tolerance >= window_secs;
    let recent_secs = (window_secs * RECENT_REFUSAL_FRACTION)
        .max(2.0 * STATS_BUCKET_SECS)
        .min(window_secs);
    let refused_lately = inputs.refused > 0
        && inputs
            .last_refused
            .is_some_and(|at| secs_since(at) <= recent_secs);
    let refused_throughout = whole_window && refused_lately;
    let raw_rate = completed as f64 / held_secs.max(1.0);
    let measured = (completed >= params.min_samples.max(1)
        || (refused_throughout && completed > 0))
        .then_some(raw_rate);

    let released_in_window = memory
        .last_release_at
        .is_some_and(|at| secs_since(at) < window_secs);
    let clean_rate = memory
        .clean_rate
        .filter(|(_, at)| secs_since(*at) <= window_secs * CLEAN_RATE_MAX_WINDOWS)
        .map(|(rate, _)| rate);
    let rate = if released_in_window {
        match (measured, clean_rate) {
            (Some(measured), Some(clean)) => Some(measured.max(clean)),
            (measured, clean) => measured.or(clean),
        }
    } else {
        measured
    };

    let hold = match rate {
        Some(rate) => ModelHold::Hold {
            cutoff: release_cutoff(&inputs.backlog, rate, params.safety_margin, now),
        },
        None if refused_throughout && completed == 0 => {
            ModelHold::ReleaseAll(TolerationsRelease::NoOwnCapacity)
        }
        None => ModelHold::Hold { cutoff: None },
    };

    if !released_in_window {
        memory.clean_rate = measured.map(|rate| (rate, now));
    }
    if matches!(
        hold,
        ModelHold::Hold { cutoff: Some(_) } | ModelHold::ReleaseAll(_)
    ) {
        memory.last_release_at = Some(now);
    }

    let own_rate = rate.unwrap_or(raw_rate);
    let backlog_requests = inputs.backlog_requests();
    ModelHoldDecision {
        model: inputs.model.clone(),
        hold,
        own_rate,
        backlog_requests,
        projected_backlog_secs: if own_rate > 0.0 {
            backlog_requests as f64 / own_rate
        } else {
            f64::NAN
        },
    }
}

/// The latest deadline `d` whose work cannot finish in time at `rate`: the
/// requests due at or before `d` take `n / rate * (1 + safety_margin)`
/// seconds, more than `d - now`. Requests due at or before it are released;
/// those due later are on track even with every earlier request still ahead
/// of them. `None` when every deadline is on track (or there is no rate).
///
/// Requests sharing a deadline are released together: a batch whose requests
/// all share one deadline is released whole, not only the part own capacity
/// cannot finish.
pub fn release_cutoff(
    backlog: &[(DateTime<Utc>, i64)],
    rate: f64,
    safety_margin: f64,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if rate.is_nan() || rate <= 0.0 {
        return None;
    }
    let factor = 1.0 + safety_margin.max(0.0);
    backlog
        .iter()
        .rev()
        .find(|(deadline, due)| {
            let remaining_secs = (*deadline - now).num_milliseconds() as f64 / 1000.0;
            *due as f64 / rate * factor > remaining_secs
        })
        .map(|(deadline, _)| *deadline)
}

/// Held completions and refusals one daemon counted in one minute since its
/// last flush.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldStatsDelta {
    pub model: String,
    /// The minute the dispatches finished in (truncated to the minute). A
    /// flush writes each delta under its own minute, so counts kept back by a
    /// failed flush land where they happened, not in the minute they are
    /// finally written: a delayed flush cannot pack minutes of completions
    /// into one and inflate the rate.
    pub minute: DateTime<Utc>,
    pub completed: i64,
    pub refused: i64,
}

/// The decisions the claim queries apply, from the latest refresh.
#[derive(Debug, Clone, PartialEq)]
pub struct HoldClaim {
    /// Every held model and its decision.
    pub models: Vec<(String, ModelHold)>,
    /// When the decisions were computed.
    pub computed_at: DateTime<Utc>,
    /// Decisions older than this release every held model
    /// ([`TolerationsRelease::Stale`]): the refresh is failing.
    pub stale_after: Duration,
}

/// [`HoldClaim`] as the claim queries' array parameters, aligned by index.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HoldClaimArrays {
    pub models: Vec<String>,
    /// [`NO_CUTOFF`] for a model with no cutoff.
    pub cutoffs: Vec<DateTime<Utc>>,
    /// The release reason label for a model released whole, `""` otherwise.
    pub release_all: Vec<String>,
}

impl HoldClaim {
    /// Before the first refresh: every model held with the deadline floor
    /// only, as of `now`. If no refresh succeeds within `stale_after`, the
    /// decisions go stale and everything is released.
    pub fn initial(
        models: impl IntoIterator<Item = String>,
        now: DateTime<Utc>,
        stale_after: Duration,
    ) -> Self {
        Self {
            models: models
                .into_iter()
                .map(|model| (model, ModelHold::Hold { cutoff: None }))
                .collect(),
            computed_at: now,
            stale_after,
        }
    }

    fn is_stale(&self, now: DateTime<Utc>) -> bool {
        now - self.computed_at > self.stale_after
    }

    /// One model's decision at `now`, or `None` when it is not held.
    pub fn model_hold(&self, model: &str, now: DateTime<Utc>) -> Option<ModelHold> {
        let (_, hold) = self.models.iter().find(|(held, _)| held == model)?;
        Some(if self.is_stale(now) {
            ModelHold::ReleaseAll(TolerationsRelease::Stale)
        } else {
            *hold
        })
    }

    /// The claim queries' parameters at `now`.
    pub fn claim_arrays(&self, now: DateTime<Utc>) -> HoldClaimArrays {
        let stale = self.is_stale(now);
        let mut arrays = HoldClaimArrays::default();
        for (model, hold) in &self.models {
            let hold = if stale {
                ModelHold::ReleaseAll(TolerationsRelease::Stale)
            } else {
                *hold
            };
            arrays.models.push(model.clone());
            match hold {
                ModelHold::Hold { cutoff } => {
                    arrays.cutoffs.push(cutoff.unwrap_or(NO_CUTOFF));
                    arrays.release_all.push(String::new());
                }
                ModelHold::ReleaseAll(reason) => {
                    arrays.cutoffs.push(NO_CUTOFF);
                    arrays.release_all.push(reason.label().to_string());
                }
            }
        }
        arrays
    }
}

/// Parse the claim queries' decision column: `hold`, or a release reason.
/// An unknown label releases (fail open).
pub fn parse_claim_decision(label: &str) -> DispatchTolerations {
    if label == "hold" {
        DispatchTolerations::Keep
    } else {
        DispatchTolerations::Release(
            TolerationsRelease::from_label(label).unwrap_or(TolerationsRelease::Stale),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARAMS: HoldParams = HoldParams {
        window_secs: 900.0,
        safety_margin: 0.2,
        min_samples: 20,
    };

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-14T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn at(secs: i64) -> DateTime<Utc> {
        now() + Duration::seconds(secs)
    }

    /// Counters over the last `held_secs` (`None`: no counters at all), the
    /// last refusal (if any) in the current minute.
    fn inputs(
        completed: i64,
        refused: i64,
        held_secs: Option<i64>,
        backlog: &[(i64, i64)],
    ) -> ModelHoldInputs {
        ModelHoldInputs {
            model: "m".to_string(),
            completed,
            refused,
            held_since: held_secs.map(|secs| at(-secs)),
            last_refused: (refused > 0).then(|| at(-30)),
            backlog: backlog.iter().map(|(secs, n)| (at(*secs), *n)).collect(),
        }
    }

    fn decide(inputs: &ModelHoldInputs) -> ModelHoldDecision {
        decide_model_hold(inputs, PARAMS, now(), &mut ModelHoldMemory::default())
    }

    #[test]
    fn an_empty_backlog_has_no_cutoff() {
        let decision = decide(&inputs(900, 0, Some(900), &[]));
        assert_eq!(decision.hold, ModelHold::Hold { cutoff: None });
        assert_eq!(decision.own_rate, 1.0);
        assert_eq!(decision.backlog_requests, 0);
        assert_eq!(decision.projected_backlog_secs, 0.0);
    }

    #[test]
    fn the_rate_is_taken_over_the_time_held_not_the_whole_window() {
        // Held for the last 5 minutes of a 15-minute window: 300 in 300s is
        // 1/s, not 1/3 per second.
        let decision = decide(&inputs(300, 0, Some(300), &[]));
        assert_eq!(decision.own_rate, 1.0);
    }

    #[test]
    fn refusals_without_completions_release_everything_only_after_a_whole_window() {
        // A held refusal is immediate while a completion takes a request's
        // latency: a minute of refusals with nothing completed yet proves
        // nothing.
        let early = decide(&inputs(0, 50, Some(60), &[(3_600, 10)]));
        assert_eq!(early.hold, ModelHold::Hold { cutoff: None });

        // Refused throughout a whole window (to within one counter minute)
        // and nothing completed: no own capacity to wait for.
        for held_secs in [900, 850] {
            let decision = decide(&inputs(0, 5, Some(held_secs), &[(3_600, 10)]));
            assert_eq!(
                decision.hold,
                ModelHold::ReleaseAll(TolerationsRelease::NoOwnCapacity),
                "held {held_secs}s"
            );
            assert_eq!(decision.own_rate, 0.0);
            assert!(decision.projected_backlog_secs.is_nan());
        }
    }

    #[test]
    fn one_old_refusal_is_not_refused_throughout_the_window() {
        // A quiet model refused once, 14 minutes ago, with its held work since
        // still running: the refusals do not span the window, so it is held on
        // the floor rather than released whole.
        let mut old = inputs(0, 1, Some(850), &[(3_600, 10)]);
        old.last_refused = Some(at(-850));
        assert_eq!(decide(&old).hold, ModelHold::Hold { cutoff: None });

        // Nor is a rate under min_samples trusted on that evidence.
        let mut slow = inputs(5, 1, Some(900), &[(3_600, 5), (86_400, 3_000)]);
        slow.last_refused = Some(at(-850));
        assert_eq!(decide(&slow).hold, ModelHold::Hold { cutoff: None });

        // Refused in the last quarter of the window (and the first): it is.
        for last_refused_secs in [30, 225] {
            let mut lately = inputs(0, 2, Some(850), &[(3_600, 10)]);
            lately.last_refused = Some(at(-last_refused_secs));
            assert_eq!(
                decide(&lately).hold,
                ModelHold::ReleaseAll(TolerationsRelease::NoOwnCapacity),
                "last refused {last_refused_secs}s ago"
            );
        }
        let mut stale = inputs(0, 2, Some(850), &[(3_600, 10)]);
        stale.last_refused = Some(at(-226));
        assert_eq!(decide(&stale).hold, ModelHold::Hold { cutoff: None });
    }

    #[test]
    fn a_first_refusal_never_releases_everything_whatever_the_window() {
        // At the shortest supported window, refusals in the current minute
        // alone are not a whole window of them; four minutes of them are.
        let shortest = HoldParams {
            window_secs: MIN_WINDOW_SECS as f64,
            ..PARAMS
        };
        let decide_in = |params: HoldParams, inputs: &ModelHoldInputs| {
            decide_model_hold(inputs, params, now(), &mut ModelHoldMemory::default()).hold
        };
        for held_secs in [0, 30, 59, 60, 179] {
            assert_eq!(
                decide_in(shortest, &inputs(0, 1, Some(held_secs), &[(3_600, 10)])),
                ModelHold::Hold { cutoff: None },
                "held {held_secs}s"
            );
        }
        assert_eq!(
            decide_in(shortest, &inputs(0, 9, Some(240), &[(3_600, 10)])),
            ModelHold::ReleaseAll(TolerationsRelease::NoOwnCapacity)
        );

        // A window of a minute or less (below the supported minimum, which
        // configuration rejects) still needs half of it held: a refusal
        // counted this minute does not release on the first refresh.
        for window_secs in [30.0, 60.0] {
            let tiny = HoldParams {
                window_secs,
                ..PARAMS
            };
            assert_eq!(
                decide_in(tiny, &inputs(0, 1, Some(5), &[(3_600, 10)])),
                ModelHold::Hold { cutoff: None },
                "window {window_secs}s"
            );
        }
    }

    #[test]
    fn a_slow_saturated_model_is_projected_not_released_whole() {
        // About 22 completions per 15 minutes while realtime saturates the
        // model. A minute into the hold: refusals, one or two completions.
        // Hold on the floor; never release everything for being slow.
        let backlog = [(3_600, 5), (86_400, 30)];
        let early = decide(&inputs(2, 40, Some(60), &backlog));
        assert_eq!(early.hold, ModelHold::Hold { cutoff: None });

        // A whole window held: 15 completed, under min_samples, but refused
        // throughout, so 15 per 900s is the capacity. The 24h work fits.
        let settled = decide(&inputs(15, 400, Some(900), &backlog));
        assert_eq!(settled.hold, ModelHold::Hold { cutoff: None });
        assert!((settled.own_rate - 15.0 / 900.0).abs() < 1e-12);

        // A backlog that rate cannot clear in time is released by deadline.
        let behind = decide(&inputs(15, 400, Some(900), &[(3_600, 5), (86_400, 3_000)]));
        assert_eq!(
            behind.hold,
            ModelHold::Hold {
                cutoff: Some(at(86_400))
            }
        );
    }

    #[test]
    fn too_few_samples_without_refusals_leaves_the_floor() {
        let decision = decide(&inputs(3, 0, Some(900), &[(600, 1_000_000)]));
        assert_eq!(decision.hold, ModelHold::Hold { cutoff: None });
        let decision = decide(&inputs(0, 0, None, &[(600, 1_000_000)]));
        assert_eq!(decision.hold, ModelHold::Hold { cutoff: None });
    }

    #[test]
    fn the_cutoff_is_the_latest_deadline_that_cannot_finish() {
        // 900 completions in 900s: 1/s. 3000 due within 1h, then 1000 more by
        // 24h. Everything due by 1h takes 3000 * 1.2 = 3600s >= 3600s left:
        // behind. Everything due by 24h takes 4800s, comfortably on track.
        let decision = decide(&inputs(
            900,
            7,
            Some(900),
            &[(1_800, 1_000), (3_599, 3_000), (86_400, 4_000)],
        ));
        assert_eq!(
            decision.hold,
            ModelHold::Hold {
                cutoff: Some(at(3_599))
            }
        );
        assert_eq!(decision.backlog_requests, 4_000);
        assert_eq!(decision.projected_backlog_secs, 4_000.0);
        // At twice the rate the hour's work fits: no cutoff.
        let decision = decide(&inputs(
            1_800,
            0,
            Some(900),
            &[(1_800, 1_000), (3_599, 3_000), (86_400, 4_000)],
        ));
        assert_eq!(decision.hold, ModelHold::Hold { cutoff: None });
    }

    #[test]
    fn a_release_does_not_feed_itself() {
        let mut memory = ModelHoldMemory::default();
        let backlog = [(3_000, 3_000), (86_400, 10_000)];
        let mut decide_at = |secs: i64, completed: i64, refused: i64| {
            let now = at(secs);
            let inputs = ModelHoldInputs {
                model: "m".to_string(),
                completed,
                refused,
                held_since: Some(now - Duration::seconds(900)),
                last_refused: (refused > 0).then_some(now),
                backlog: backlog.iter().map(|(due, n)| (at(*due), *n)).collect(),
            };
            decide_model_hold(&inputs, PARAMS, now, &mut memory)
        };

        // Clean: 1/s, so the hour's 3000 are behind and released.
        let first = decide_at(0, 900, 10);
        assert_eq!(
            first.hold,
            ModelHold::Hold {
                cutoff: Some(at(3_000))
            }
        );

        // The released requests now take most of the own capacity, so held
        // completions collapse. The remembered clean rate keeps the cutoff
        // where it was instead of releasing the 24h work too.
        let diluted = decide_at(60, 100, 500);
        assert_eq!(
            diluted.hold,
            ModelHold::Hold {
                cutoff: Some(at(3_000))
            }
        );
        assert_eq!(diluted.own_rate, 1.0);

        // Nothing held completes at all while refusals continue: still not
        // "no own capacity", the model had a clean rate.
        let starved = decide_at(120, 0, 900);
        assert_eq!(
            starved.hold,
            ModelHold::Hold {
                cutoff: Some(at(3_000))
            }
        );

        // A higher measured rate (more capacity) is still used.
        let faster = decide_at(180, 2_700, 0);
        assert_eq!(faster.own_rate, 3.0);
        assert_eq!(faster.hold, ModelHold::Hold { cutoff: None });
    }

    #[test]
    fn a_clean_rate_is_relearned_once_the_window_is_clean_and_expires() {
        let backlog = [(86_400, 100)];
        let held = |now: DateTime<Utc>, completed: i64, refused: i64| ModelHoldInputs {
            model: "m".to_string(),
            completed,
            refused,
            held_since: Some(now - Duration::seconds(900)),
            last_refused: (refused > 0).then_some(now),
            backlog: backlog.iter().map(|(due, n)| (at(*due), *n)).collect(),
        };

        // No release in the window: the measured rate becomes the clean rate.
        let mut memory = ModelHoldMemory::default();
        decide_model_hold(&held(now(), 450, 0), PARAMS, now(), &mut memory);
        assert_eq!(memory.clean_rate, Some((0.5, now())));
        assert_eq!(memory.last_release_at, None);

        // A clean rate older than CLEAN_RATE_MAX_WINDOWS windows is not used:
        // the measured, diluted, rate is (erring toward release).
        let mut memory = ModelHoldMemory {
            clean_rate: Some((1.0, at(-3_700))),
            last_release_at: Some(at(-60)),
        };
        let decision = decide_model_hold(&held(now(), 90, 10), PARAMS, now(), &mut memory);
        assert_eq!(decision.own_rate, 0.1);
        assert_eq!(
            memory.clean_rate,
            Some((1.0, at(-3_700))),
            "not relearned while a release is in the window"
        );

        // A window after the last release the rate is relearned.
        let later = at(900);
        decide_model_hold(&held(later, 90, 10), PARAMS, later, &mut memory);
        assert_eq!(memory.clean_rate, Some((0.1, later)));
    }

    #[test]
    fn the_margin_is_applied() {
        // 1000 due in 1100s at 1/s: on track without margin, behind with 20%.
        let backlog = [(at(1_100), 1_000)];
        assert_eq!(release_cutoff(&backlog, 1.0, 0.0, now()), None);
        assert_eq!(release_cutoff(&backlog, 1.0, 0.2, now()), Some(at(1_100)));
        assert_eq!(release_cutoff(&backlog, 0.0, 0.2, now()), None, "no rate");
    }

    #[test]
    fn a_shared_deadline_releases_the_whole_cohort() {
        // A 24h batch of 100000 requests sharing one deadline at 1/s: the
        // whole cohort is released, not only the excess own capacity cannot
        // finish (documented over-release).
        let backlog = [(at(86_400), 100_000)];
        assert_eq!(release_cutoff(&backlog, 1.0, 0.2, now()), Some(at(86_400)));
    }

    #[test]
    fn mixed_windows_release_only_the_urgent_prefix() {
        // 1h work behind, 24h work on track even with the 1h work ahead of it.
        let backlog = [(at(2_900), 5_000), (at(86_000), 6_000)];
        assert_eq!(release_cutoff(&backlog, 2.0, 0.2, now()), Some(at(2_900)));
        // A later deadline behind releases everything due before it too.
        let backlog = [(at(600), 10), (at(86_000), 200_000)];
        assert_eq!(release_cutoff(&backlog, 2.0, 0.2, now()), Some(at(86_000)));
    }

    #[test]
    fn stale_decisions_release_every_model() {
        let claim = HoldClaim {
            models: vec![
                (
                    "a".to_string(),
                    ModelHold::Hold {
                        cutoff: Some(at(60)),
                    },
                ),
                ("b".to_string(), ModelHold::Hold { cutoff: None }),
                (
                    "c".to_string(),
                    ModelHold::ReleaseAll(TolerationsRelease::NoOwnCapacity),
                ),
            ],
            computed_at: now(),
            stale_after: Duration::seconds(300),
        };
        let fresh = claim.claim_arrays(now() + Duration::seconds(300));
        assert_eq!(fresh.models, ["a", "b", "c"]);
        assert_eq!(fresh.cutoffs, [at(60), NO_CUTOFF, NO_CUTOFF]);
        assert_eq!(fresh.release_all, ["", "", "no_own_capacity"]);
        assert_eq!(
            claim.model_hold("a", now()),
            Some(ModelHold::Hold {
                cutoff: Some(at(60))
            })
        );
        assert_eq!(claim.model_hold("z", now()), None, "not held");

        let stale = claim.claim_arrays(now() + Duration::seconds(301));
        assert_eq!(stale.release_all, ["stale", "stale", "stale"]);
        assert_eq!(
            claim.model_hold("b", now() + Duration::seconds(301)),
            Some(ModelHold::ReleaseAll(TolerationsRelease::Stale))
        );

        let initial = HoldClaim::initial(["a".to_string()], now(), Duration::seconds(300));
        assert_eq!(initial.claim_arrays(now()).release_all, [""]);
        assert_eq!(
            initial
                .claim_arrays(now() + Duration::seconds(301))
                .release_all,
            ["stale"],
            "a refresh that never succeeds fails open"
        );
    }

    #[test]
    fn claim_decisions_parse_and_unknown_ones_release() {
        assert_eq!(parse_claim_decision("hold"), DispatchTolerations::Keep);
        assert_eq!(
            parse_claim_decision("ramp"),
            DispatchTolerations::Release(TolerationsRelease::Ramp)
        );
        for reason in [
            TolerationsRelease::Ramp,
            TolerationsRelease::PastDeadline,
            TolerationsRelease::SlaProjection,
            TolerationsRelease::NoOwnCapacity,
            TolerationsRelease::Stale,
            TolerationsRelease::RetryCutoff,
        ] {
            assert_eq!(TolerationsRelease::from_label(reason.label()), Some(reason));
        }
        assert_eq!(
            parse_claim_decision("unknown"),
            DispatchTolerations::Release(TolerationsRelease::Stale),
            "fail open"
        );
    }
}
