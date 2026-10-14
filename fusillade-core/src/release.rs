//! Release of held dispatches.
//!
//! The daemon can hold the batch and flex dispatches of opted-in models on the
//! capacity a scheduling-toleration list allows (fusillade's dispatch hold): a
//! held request carries the list, and the upstream inference backend may then
//! refuse it while that capacity is full. A held request is released, sent as
//! stored so the backend may schedule it anywhere, when it would otherwise
//! miss its completion window.
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
//! The decision fails open: a model whose held dispatches are refused while
//! too few complete has no own capacity to wait for, and missing or stale
//! counters release everything, because a missed completion window is the
//! worse failure.

use chrono::{DateTime, Duration, Utc};

use crate::request::{DispatchTolerations, TolerationsRelease};

/// The cutoff passed for a model that has none: no deadline is at or before
/// it.
pub const NO_CUTOFF: DateTime<Utc> = DateTime::<Utc>::UNIX_EPOCH;

/// Tuning of the per-model decision.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HoldParams {
    /// Trailing window of held completions and refusals, in seconds.
    pub window_secs: f64,
    /// Fraction of a deadline's projected work added as margin: a deadline
    /// `d` is behind when `work_due_by(d) / rate * (1 + safety_margin)`
    /// exceeds the time left until `d`.
    pub safety_margin: f64,
    /// Held completions a model needs in the window before its own rate is
    /// trusted.
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
    /// Outstanding requests of the model (pending, claimed or processing),
    /// cumulative by deadline: `(d, n)` means `n` requests are due at or
    /// before `d`. Ascending in `d` and `n`.
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
    /// Held completions per second over the window.
    pub own_rate: f64,
    pub backlog_requests: i64,
    /// Seconds to clear the backlog at `own_rate` (NaN without a rate).
    pub projected_backlog_secs: f64,
}

/// One model's decision.
///
/// - Held dispatches were refused while fewer than `min_samples` completed:
///   the capacity the hold allows is absent or full and cannot be measured, so
///   everything is released ([`TolerationsRelease::NoOwnCapacity`]).
/// - Fewer than `min_samples` completions and no refusals: nothing to project
///   from yet (a new hold, or a quiet model that is never refused); only the
///   deadline floor releases.
/// - Otherwise the own rate is `completed / window_secs` and the cutoff is
///   [`release_cutoff`] of the outstanding work.
pub fn decide_model_hold(
    inputs: &ModelHoldInputs,
    params: HoldParams,
    now: DateTime<Utc>,
) -> ModelHoldDecision {
    let window_secs = params.window_secs.max(1.0);
    let own_rate = inputs.completed.max(0) as f64 / window_secs;
    let backlog_requests = inputs.backlog_requests();
    let min_samples = params.min_samples.max(1);
    let hold = if inputs.refused > 0 && inputs.completed < min_samples {
        ModelHold::ReleaseAll(TolerationsRelease::NoOwnCapacity)
    } else if inputs.completed < min_samples {
        ModelHold::Hold { cutoff: None }
    } else {
        ModelHold::Hold {
            cutoff: release_cutoff(&inputs.backlog, own_rate, params.safety_margin, now),
        }
    };
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

/// Held completions and refusals one daemon counted since its last flush.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldStatsDelta {
    pub model: String,
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

/// The claim's decision for one held request beyond the deadline floor, the
/// same rule the claim queries apply: released whole, released when due at or
/// before the cutoff, otherwise held.
pub fn apply_model_hold(hold: ModelHold, deadline: DateTime<Utc>) -> DispatchTolerations {
    match hold {
        ModelHold::ReleaseAll(reason) => DispatchTolerations::Release(reason),
        ModelHold::Hold {
            cutoff: Some(cutoff),
        } if deadline <= cutoff => DispatchTolerations::Release(TolerationsRelease::SlaProjection),
        ModelHold::Hold { .. } => DispatchTolerations::Keep,
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

    fn inputs(completed: i64, refused: i64, backlog: &[(i64, i64)]) -> ModelHoldInputs {
        ModelHoldInputs {
            model: "m".to_string(),
            completed,
            refused,
            backlog: backlog.iter().map(|(secs, n)| (at(*secs), *n)).collect(),
        }
    }

    #[test]
    fn an_empty_backlog_has_no_cutoff() {
        let decision = decide_model_hold(&inputs(900, 0, &[]), PARAMS, now());
        assert_eq!(decision.hold, ModelHold::Hold { cutoff: None });
        assert_eq!(decision.own_rate, 1.0);
        assert_eq!(decision.backlog_requests, 0);
        assert_eq!(decision.projected_backlog_secs, 0.0);
    }

    #[test]
    fn refusals_without_completions_release_everything() {
        let decision = decide_model_hold(&inputs(0, 5, &[(3_600, 10)]), PARAMS, now());
        assert_eq!(
            decision.hold,
            ModelHold::ReleaseAll(TolerationsRelease::NoOwnCapacity),
            "refused and nothing completes: there is no own capacity to wait for"
        );
        assert_eq!(decision.own_rate, 0.0);
        assert!(decision.projected_backlog_secs.is_nan());
        // Too few completions to measure while being refused: the same.
        let decision = decide_model_hold(&inputs(19, 1, &[(3_600, 10)]), PARAMS, now());
        assert_eq!(
            decision.hold,
            ModelHold::ReleaseAll(TolerationsRelease::NoOwnCapacity)
        );
    }

    #[test]
    fn too_few_samples_without_refusals_leaves_the_floor() {
        let decision = decide_model_hold(&inputs(3, 0, &[(600, 1_000_000)]), PARAMS, now());
        assert_eq!(decision.hold, ModelHold::Hold { cutoff: None });
    }

    #[test]
    fn the_cutoff_is_the_latest_deadline_that_cannot_finish() {
        // 900 completions in 900s: 1/s. 3000 due within 1h, then 1000 more by
        // 24h. Everything due by 1h takes 3000 * 1.2 = 3600s >= 3600s left:
        // behind. Everything due by 24h takes 4800s, comfortably on track.
        let decision = decide_model_hold(
            &inputs(900, 7, &[(1_800, 1_000), (3_599, 3_000), (86_400, 4_000)]),
            PARAMS,
            now(),
        );
        assert_eq!(
            decision.hold,
            ModelHold::Hold {
                cutoff: Some(at(3_599))
            }
        );
        assert_eq!(decision.backlog_requests, 4_000);
        assert_eq!(decision.projected_backlog_secs, 4_000.0);
        // At twice the rate the hour's work fits: no cutoff.
        let decision = decide_model_hold(
            &inputs(1_800, 0, &[(1_800, 1_000), (3_599, 3_000), (86_400, 4_000)]),
            PARAMS,
            now(),
        );
        assert_eq!(decision.hold, ModelHold::Hold { cutoff: None });
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
        // Overdue work alone is behind (the past-deadline floor releases it
        // anyway).
        let backlog = [(at(-60), 1), (at(86_000), 2)];
        assert_eq!(release_cutoff(&backlog, 2.0, 0.2, now()), Some(at(-60)));
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
    fn applying_a_model_hold_matches_the_claim() {
        let hold = ModelHold::Hold {
            cutoff: Some(at(60)),
        };
        assert_eq!(
            apply_model_hold(hold, at(60)),
            DispatchTolerations::Release(TolerationsRelease::SlaProjection)
        );
        assert_eq!(apply_model_hold(hold, at(61)), DispatchTolerations::Keep);
        assert_eq!(
            apply_model_hold(ModelHold::Hold { cutoff: None }, at(1)),
            DispatchTolerations::Keep
        );
        assert_eq!(
            apply_model_hold(ModelHold::ReleaseAll(TolerationsRelease::Stale), at(1)),
            DispatchTolerations::Release(TolerationsRelease::Stale)
        );
        assert_eq!(parse_claim_decision("hold"), DispatchTolerations::Keep);
        assert_eq!(
            parse_claim_decision("ramp"),
            DispatchTolerations::Release(TolerationsRelease::Ramp)
        );
        assert_eq!(
            parse_claim_decision("unknown"),
            DispatchTolerations::Release(TolerationsRelease::Stale),
            "fail open"
        );
    }
}
