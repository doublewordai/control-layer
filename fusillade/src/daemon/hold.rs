//! The dispatch hold: when to stop holding a request, and the per-daemon
//! counters the decision is computed from.
//!
//! A held request carries the hold list (see [`super::DispatchHold`]); the
//! upstream inference backend may then refuse it while the capacity the list
//! allows is full. Near SLA failure, meeting the deadline matters more than
//! where the request runs, so it is released (sent as stored, and the backend
//! may then schedule it anywhere) when:
//!
//! - it is past its deadline, or inside the batch-claim deadline ramp (the
//!   floor, always; [`release_reason`]);
//! - its model's held dispatches are refused while too few complete, or the
//!   decisions are stale ([`ModelHold::ReleaseAll`]);
//! - it is due at or before its model's cutoff: the latest deadline whose work
//!   cannot finish at the model's own rate ([`ModelHold::Hold`]).
//!
//! Every daemon counts its held completions and held refusals in memory,
//! flushes them every few seconds into the shared per-minute `hold_stats`
//! table, and every refresh interval reads the counters and the outstanding
//! work back and computes every held model's decision
//! (`fusillade_core::release`). The claim queries apply the decisions, passed
//! as parameters, so no leader, lock or extra table of decisions is needed and
//! nothing is written to the request rows.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use fusillade_core::manager::DaemonStorage;
use fusillade_core::release::{
    HoldClaim, HoldParams, HoldStatsDelta, ModelHold, apply_model_hold, decide_model_hold,
};
use fusillade_core::request::{DispatchTolerations, TolerationsRelease};
use metrics::gauge;
use tokio_util::sync::CancellationToken;

use super::DispatchHold;

/// How often each daemon flushes its counters into `hold_stats`.
const FLUSH_INTERVAL: Duration = Duration::from_secs(10);
/// How long `hold_stats` rows are kept: well past any window.
const STATS_RETENTION: chrono::Duration = chrono::Duration::hours(2);

/// The deadline floor, for claims that made no decision themselves (storage
/// that does not decide): released past the deadline, or within
/// `window_minutes ^ ramp_exponent` minutes of it, where the window runs from
/// `created_at` to the deadline (about 59 minutes for 24h and 10 for 1h at
/// 0.56), matching the batch-claim gate. No deadline (background): never. An
/// unreadable `created_at` keeps the hold until the deadline passes.
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

/// One daemon's hold state: its unflushed counters and the latest decisions.
pub(crate) struct HoldState {
    hold: DispatchHold,
    /// Per model: (held completions, held refusals) since the last flush.
    counters: Mutex<HashMap<String, (i64, i64)>>,
    /// The decisions the claim queries were last given.
    claim: RwLock<HoldClaim>,
}

impl HoldState {
    pub(crate) fn new(hold: DispatchHold, now: DateTime<Utc>) -> Self {
        let claim = HoldClaim::initial(
            hold.models.iter().cloned(),
            now,
            hold.stale_after_duration(),
        );
        Self {
            hold,
            counters: Mutex::new(HashMap::new()),
            claim: RwLock::new(claim),
        }
    }

    pub(crate) fn hold(&self) -> &DispatchHold {
        &self.hold
    }

    /// The decisions the claim queries apply.
    pub(crate) fn claim(&self) -> HoldClaim {
        self.claim
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn set_claim(&self, claim: HoldClaim) {
        *self
            .claim
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = claim;
    }

    fn add(&self, model: &str, completed: i64, refused: i64) {
        let mut counters = self
            .counters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = counters.entry(model.to_string()).or_default();
        entry.0 += completed;
        entry.1 += refused;
    }

    /// A held dispatch completed successfully.
    pub(crate) fn record_completed(&self, model: &str) {
        self.add(model, 1, 0);
    }

    /// A held dispatch was refused (503/529).
    pub(crate) fn record_refused(&self, model: &str) {
        self.add(model, 0, 1);
    }

    /// Take the counters for a flush.
    fn take(&self) -> Vec<HoldStatsDelta> {
        let mut counters = self
            .counters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        counters
            .drain()
            .filter(|(_, (completed, refused))| *completed > 0 || *refused > 0)
            .map(|(model, (completed, refused))| HoldStatsDelta {
                model,
                completed,
                refused,
            })
            .collect()
    }

    /// Put back counters a flush failed to write.
    fn restore(&self, deltas: Vec<HoldStatsDelta>) {
        for delta in deltas {
            self.add(&delta.model, delta.completed, delta.refused);
        }
    }

    /// The decision for one claimed request of a held model when the claim
    /// path made none: the deadline floor, then the model's decision. `None`
    /// without a deadline (never held).
    pub(crate) fn decide(
        &self,
        model: &str,
        deadline: Option<DateTime<Utc>>,
        created_at: Option<&str>,
        ramp_exponent: f64,
        now: DateTime<Utc>,
    ) -> Option<DispatchTolerations> {
        let deadline_at = deadline?;
        if let Some(reason) = release_reason(deadline, created_at, ramp_exponent, now) {
            return Some(DispatchTolerations::Release(reason));
        }
        let claim = self
            .claim
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let hold = claim
            .model_hold(model, now)
            .unwrap_or(ModelHold::ReleaseAll(TolerationsRelease::Stale));
        Some(apply_model_hold(hold, deadline_at))
    }
}

/// Flush this daemon's counters, then (once per refresh interval) recompute
/// every held model's decision and hand it to the claim queries.
///
/// Each storage call is bounded by `query_timeout` and abandoned on shutdown.
/// A failed flush keeps the counters for the next one; a failed refresh keeps
/// the previous decisions, which the claim treats as stale (everything
/// released) once they are older than `stale_after`. On shutdown the counters
/// are flushed one last time.
pub(crate) async fn run_hold_refresh<S>(
    storage: Arc<S>,
    state: Arc<HoldState>,
    query_timeout: Duration,
    shutdown: CancellationToken,
) where
    S: DaemonStorage + ?Sized,
{
    let hold = state.hold().clone();
    let models: Vec<String> = {
        let mut models: Vec<String> = hold.models.iter().cloned().collect();
        models.sort();
        models
    };
    let params = HoldParams {
        window_secs: hold.window.as_secs_f64(),
        safety_margin: hold.safety_margin,
        min_samples: i64::try_from(hold.min_samples).unwrap_or(i64::MAX),
    };
    let stale_after = hold.stale_after_duration();
    let refresh_interval = hold.refresh_interval.max(Duration::from_secs(1));
    let mut interval = tokio::time::interval(FLUSH_INTERVAL.min(refresh_interval));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let started = tokio::time::Instant::now();
    let mut last_refresh: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = shutdown.cancelled() => {
                // Last flush, bounded, so a draining pod's counts are not lost.
                let deltas = state.take();
                if !deltas.is_empty() {
                    let flush = storage.flush_hold_stats(minute_of(Utc::now()), &deltas);
                    match tokio::time::timeout(query_timeout, flush).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            tracing::debug!(%error, "Failed the final hold_stats flush");
                        }
                        Err(_) => tracing::debug!("The final hold_stats flush timed out"),
                    }
                }
                break;
            }
        }

        let deltas = state.take();
        if !deltas.is_empty() {
            let minute = minute_of(Utc::now());
            match super::maintenance_query(
                &shutdown,
                "hold_stats flush",
                query_timeout,
                storage.flush_hold_stats(minute, &deltas),
            )
            .await
            {
                Ok(Some(())) => {}
                Ok(None) => {
                    state.restore(deltas);
                    continue;
                }
                Err(error) => {
                    state.restore(deltas);
                    crate::background_error!(
                        "hold_stats_flush",
                        Warning,
                        error = %error,
                        "Failed to flush hold_stats; the counts are kept for the next flush"
                    );
                    report_age(started, last_refresh);
                    continue;
                }
            }
        }

        let due = last_refresh.is_none_or(|at| at.elapsed() >= refresh_interval);
        if due {
            match refresh(
                &*storage,
                &models,
                params,
                stale_after,
                query_timeout,
                &shutdown,
            )
            .await
            {
                Ok(Some((claim, decisions))) => {
                    storage.set_dispatch_hold(Some(claim.clone()));
                    state.set_claim(claim);
                    last_refresh = Some(tokio::time::Instant::now());
                    for decision in decisions {
                        let model = decision.model;
                        gauge!("fusillade_hold_own_rate", "model" => model.clone())
                            .set(decision.own_rate);
                        gauge!("fusillade_projected_backlog_seconds", "model" => model.clone())
                            .set(decision.projected_backlog_secs);
                        if let ModelHold::ReleaseAll(reason) = decision.hold {
                            tracing::debug!(
                                model,
                                reason = reason.label(),
                                "Releasing every held request of the model"
                            );
                        }
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    crate::background_error!(
                        "hold_refresh",
                        Warning,
                        error = %error,
                        "Failed to refresh the dispatch hold decisions; stale decisions release everything"
                    );
                }
            }
        }
        report_age(started, last_refresh);
    }
}

/// `fusillade_hold_stats_age_seconds`: time since the decisions were last
/// computed (since startup before the first).
fn report_age(started: tokio::time::Instant, last_refresh: Option<tokio::time::Instant>) {
    gauge!("fusillade_hold_stats_age_seconds")
        .set(last_refresh.unwrap_or(started).elapsed().as_secs_f64());
}

fn minute_of(now: DateTime<Utc>) -> DateTime<Utc> {
    use chrono::DurationRound;
    now.duration_trunc(chrono::Duration::minutes(1))
        .unwrap_or(now)
}

/// Purge old counters, read the window's counters and the outstanding work,
/// and decide. `Ok(None)` on shutdown.
async fn refresh<S>(
    storage: &S,
    models: &[String],
    params: HoldParams,
    stale_after: chrono::Duration,
    query_timeout: Duration,
    shutdown: &CancellationToken,
) -> fusillade_core::Result<Option<(HoldClaim, Vec<fusillade_core::release::ModelHoldDecision>)>>
where
    S: DaemonStorage + ?Sized,
{
    let Some(_) = super::maintenance_query(
        shutdown,
        "hold_stats purge",
        query_timeout,
        storage.purge_hold_stats(Utc::now() - STATS_RETENTION),
    )
    .await?
    else {
        return Ok(None);
    };
    let Some((now, inputs)) = super::maintenance_query(
        shutdown,
        "hold inputs",
        query_timeout,
        storage.read_hold_inputs(models, params.window_secs),
    )
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(decide_all(models, &inputs, params, now, stale_after)))
}

/// Every held model's decision from the inputs read on one refresh (a model
/// with no counters and no work has empty inputs).
pub(crate) fn decide_all(
    models: &[String],
    inputs: &[fusillade_core::release::ModelHoldInputs],
    params: HoldParams,
    now: DateTime<Utc>,
    stale_after: chrono::Duration,
) -> (HoldClaim, Vec<fusillade_core::release::ModelHoldDecision>) {
    let decisions: Vec<_> = models
        .iter()
        .map(|model| {
            let empty = fusillade_core::release::ModelHoldInputs {
                model: model.clone(),
                ..Default::default()
            };
            let inputs = inputs
                .iter()
                .find(|inputs| &inputs.model == model)
                .unwrap_or(&empty);
            decide_model_hold(inputs, params, now)
        })
        .collect();
    let claim = HoldClaim {
        models: decisions
            .iter()
            .map(|decision| (decision.model.clone(), decision.hold))
            .collect(),
        computed_at: Utc::now(),
        stale_after,
    };
    (claim, decisions)
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

    fn state() -> HoldState {
        HoldState::new(
            DispatchHold::new(serde_json::json!([]), ["m".to_string()]),
            Utc::now(),
        )
    }

    #[test]
    fn counters_are_taken_once_and_restored_after_a_failed_flush() {
        let state = state();
        state.record_completed("m");
        state.record_completed("m");
        state.record_refused("m");
        state.record_refused("n");
        let mut deltas = state.take();
        deltas.sort_by(|a, b| a.model.cmp(&b.model));
        assert_eq!(
            deltas,
            vec![
                HoldStatsDelta {
                    model: "m".to_string(),
                    completed: 2,
                    refused: 1
                },
                HoldStatsDelta {
                    model: "n".to_string(),
                    completed: 0,
                    refused: 1
                },
            ]
        );
        assert!(state.take().is_empty(), "taken once");

        // A failed flush puts them back, added to what arrived since.
        state.restore(deltas);
        state.record_completed("m");
        let mut deltas = state.take();
        deltas.sort_by(|a, b| a.model.cmp(&b.model));
        assert_eq!((deltas[0].completed, deltas[0].refused), (3, 1));
    }

    #[test]
    fn every_held_model_gets_a_decision() {
        let params = HoldParams {
            window_secs: 900.0,
            safety_margin: 0.2,
            min_samples: 20,
        };
        let now = Utc::now();
        let inputs = [fusillade_core::release::ModelHoldInputs {
            model: "busy".to_string(),
            completed: 0,
            refused: 4,
            backlog: vec![(now + chrono::Duration::hours(1), 10)],
        }];
        let (claim, decisions) = decide_all(
            &["busy".to_string(), "quiet".to_string()],
            &inputs,
            params,
            now,
            chrono::Duration::minutes(5),
        );
        assert_eq!(decisions.len(), 2);
        assert_eq!(
            claim.model_hold("busy", now),
            Some(ModelHold::ReleaseAll(TolerationsRelease::NoOwnCapacity))
        );
        assert_eq!(
            claim.model_hold("quiet", now),
            Some(ModelHold::Hold { cutoff: None }),
            "a model with no counters and no work is held, floor only"
        );
        assert_eq!(claim.stale_after, chrono::Duration::minutes(5));
    }

    #[test]
    fn the_in_process_decision_matches_the_claim() {
        let state = state();
        let now = Utc::now();
        let created = (now - chrono::Duration::hours(1)).to_rfc3339();
        let due = now + chrono::Duration::hours(23);
        let decide = |deadline| state.decide("m", deadline, Some(&created), 0.56, now);
        assert_eq!(decide(None), None, "no deadline: not held");
        assert_eq!(decide(Some(due)), Some(DispatchTolerations::Keep));
        state.set_claim(HoldClaim {
            models: vec![("m".to_string(), ModelHold::Hold { cutoff: Some(due) })],
            computed_at: now,
            stale_after: chrono::Duration::minutes(5),
        });
        assert_eq!(
            decide(Some(due)),
            Some(DispatchTolerations::Release(
                TolerationsRelease::SlaProjection
            ))
        );
        assert_eq!(
            decide(Some(now - chrono::Duration::minutes(1))),
            Some(DispatchTolerations::Release(
                TolerationsRelease::PastDeadline
            )),
            "the floor first"
        );
    }
}
