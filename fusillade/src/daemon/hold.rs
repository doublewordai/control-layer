//! The dispatch hold: when to stop holding a request, and the per-daemon
//! counters the decision is computed from.
//!
//! A held request carries the hold list (see [`super::DispatchHold`]); the
//! upstream inference backend may then refuse it while the capacity the list
//! allows is full. Near SLA failure, meeting the deadline matters more than
//! where the request runs, so it is released (sent as stored, without the hold
//! list, so the hold no longer limits where the backend schedules it) when:
//!
//! - it is past its deadline, or inside the batch-claim deadline ramp (the
//!   floor, always; applied by the claim queries);
//! - its model's held dispatches were refused throughout a whole window of
//!   holding (from its start to its recent part) while none completed, or the
//!   decisions are stale ([`ModelHold::ReleaseAll`]);
//! - it is due at or before its model's cutoff: the latest deadline whose work
//!   cannot finish at the model's own rate ([`ModelHold::Hold`]). The rate is
//!   measured only while the model's held work was refused throughout the
//!   window (its own capacity was full); otherwise the held completions are
//!   the demand, not the capacity, and there is no cutoff;
//! - it is so close to the retry cut-off that a held refusal could not be
//!   retried before it (applied when the dispatch is prepared).
//!
//! Every daemon counts its held completions and held refusals in memory, by
//! the minute they happened in, flushes them every few seconds into the
//! shared per-minute `hold_stats` table (counts a failed flush kept back still
//! land in their own minute), and every refresh interval reads the counters
//! and the outstanding work back and computes every held model's decision
//! (`fusillade_core::release`). The claim queries apply the decisions, passed
//! as parameters, so no leader, lock or extra table of decisions is needed and
//! nothing is written to the request rows. Only a storage whose claims make
//! the decision can hold: a claimed request without one is sent as stored.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use fusillade_core::manager::DaemonStorage;
use fusillade_core::release::{
    HoldClaim, HoldParams, HoldStatsDelta, ModelHold, ModelHoldMemory, decide_model_hold,
};
use metrics::gauge;
use tokio_util::sync::CancellationToken;

use super::DispatchHold;

/// How often each daemon flushes its counters into `hold_stats`.
const FLUSH_INTERVAL: Duration = Duration::from_secs(10);
/// How long `hold_stats` rows are kept at least.
const MIN_STATS_RETENTION: chrono::Duration = chrono::Duration::hours(2);

/// How long `hold_stats` rows are kept: two windows, and at least two hours,
/// so the read always sees the whole window.
pub(crate) fn stats_retention(window: Duration) -> chrono::Duration {
    chrono::Duration::from_std(window.saturating_mul(2))
        .unwrap_or(chrono::Duration::MAX)
        .max(MIN_STATS_RETENTION)
}

/// Counter minutes before this are past retention at `now`: the purge removes
/// them and the read never counts them. `None` when the retention reaches
/// past the earliest representable time (nothing is that old).
fn retention_cutoff(now: DateTime<Utc>, window: Duration) -> Option<DateTime<Utc>> {
    now.checked_sub_signed(stats_retention(window))
}

/// Per (model, minute counted in): (held completions, held refusals) since
/// the last flush.
type Counters = HashMap<(String, DateTime<Utc>), (i64, i64)>;

/// One daemon's hold state: its unflushed counters and the latest decisions.
pub(crate) struct HoldState {
    hold: DispatchHold,
    /// This daemon's counters since the last flush.
    counters: Mutex<Counters>,
    /// The decisions the claim queries were last given.
    claim: RwLock<HoldClaim>,
    /// Per model: what the decision remembers between refreshes.
    memory: Mutex<HashMap<String, ModelHoldMemory>>,
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
            memory: Mutex::new(HashMap::new()),
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

    fn add(&self, model: &str, minute: DateTime<Utc>, completed: i64, refused: i64) {
        let mut counters = self
            .counters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = counters.entry((model.to_string(), minute)).or_default();
        entry.0 += completed;
        entry.1 += refused;
    }

    /// A held dispatch completed successfully.
    pub(crate) fn record_completed(&self, model: &str) {
        self.add(model, minute_of(Utc::now()), 1, 0);
    }

    /// A held dispatch was refused (503/529).
    pub(crate) fn record_refused(&self, model: &str) {
        self.add(model, minute_of(Utc::now()), 0, 1);
    }

    /// Take the counters for a flush, one delta per model and minute.
    fn take(&self) -> Vec<HoldStatsDelta> {
        let mut counters = self
            .counters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        counters
            .drain()
            .filter(|(_, (completed, refused))| *completed > 0 || *refused > 0)
            .map(|((model, minute), (completed, refused))| HoldStatsDelta {
                model,
                minute,
                completed,
                refused,
            })
            .collect()
    }

    /// Put back counters a flush failed to write, under the minutes they were
    /// counted in. Those already past retention are dropped: the purge would
    /// remove them and the read would never count them.
    fn restore(&self, deltas: Vec<HoldStatsDelta>, now: DateTime<Utc>) {
        let cutoff = retention_cutoff(now, self.hold.window);
        for delta in deltas {
            if cutoff.is_none_or(|cutoff| delta.minute >= cutoff) {
                self.add(&delta.model, delta.minute, delta.completed, delta.refused);
            }
        }
    }

    /// Every held model's decision from the inputs read on one refresh,
    /// updating what each model's decision remembers.
    fn decide(
        &self,
        models: &[String],
        inputs: &[fusillade_core::release::ModelHoldInputs],
        params: HoldParams,
        now: DateTime<Utc>,
        stale_after: chrono::Duration,
    ) -> (HoldClaim, Vec<fusillade_core::release::ModelHoldDecision>) {
        let mut memory = self
            .memory
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        decide_all(models, inputs, params, now, stale_after, &mut memory)
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
                    let flush = storage.flush_hold_stats(&deltas);
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
            match super::maintenance_query(
                &shutdown,
                "hold_stats flush",
                query_timeout,
                storage.flush_hold_stats(&deltas),
            )
            .await
            {
                Ok(Some(())) => {}
                Ok(None) => {
                    state.restore(deltas, Utc::now());
                    continue;
                }
                Err(error) => {
                    state.restore(deltas, Utc::now());
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
                &state,
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
    state: &HoldState,
    models: &[String],
    params: HoldParams,
    stale_after: chrono::Duration,
    query_timeout: Duration,
    shutdown: &CancellationToken,
) -> fusillade_core::Result<Option<(HoldClaim, Vec<fusillade_core::release::ModelHoldDecision>)>>
where
    S: DaemonStorage + ?Sized,
{
    // Checked: a window too long to subtract from now (the configuration
    // rejects one) leaves nothing old enough to purge, rather than panicking.
    if let Some(before) = retention_cutoff(Utc::now(), state.hold().window) {
        let Some(_) = super::maintenance_query(
            shutdown,
            "hold_stats purge",
            query_timeout,
            storage.purge_hold_stats(before),
        )
        .await?
        else {
            return Ok(None);
        };
    }
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
    Ok(Some(state.decide(
        models,
        &inputs,
        params,
        now,
        stale_after,
    )))
}

/// Every held model's decision from the inputs read on one refresh (a model
/// with no counters and no work has empty inputs), updating each model's
/// memory.
pub(crate) fn decide_all(
    models: &[String],
    inputs: &[fusillade_core::release::ModelHoldInputs],
    params: HoldParams,
    now: DateTime<Utc>,
    stale_after: chrono::Duration,
    memory: &mut HashMap<String, ModelHoldMemory>,
) -> (HoldClaim, Vec<fusillade_core::release::ModelHoldDecision>) {
    memory.retain(|model, _| models.contains(model));
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
            decide_model_hold(
                inputs,
                params,
                now,
                memory.entry(model.clone()).or_default(),
            )
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
    use fusillade_core::request::TolerationsRelease;

    fn state() -> HoldState {
        HoldState::new(
            DispatchHold::new(serde_json::json!([]), ["m".to_string()]),
            Utc::now(),
        )
    }

    fn sorted(mut deltas: Vec<HoldStatsDelta>) -> Vec<HoldStatsDelta> {
        deltas.sort_by(|a, b| (&a.model, a.minute).cmp(&(&b.model, b.minute)));
        deltas
    }

    #[test]
    fn counters_are_taken_once_and_restored_after_a_failed_flush() {
        let state = state();
        let before = minute_of(Utc::now());
        state.record_completed("m");
        state.record_completed("m");
        state.record_refused("m");
        state.record_refused("n");
        let after = minute_of(Utc::now());
        let deltas = sorted(state.take());
        if before == after {
            assert_eq!(
                deltas,
                vec![
                    HoldStatsDelta {
                        model: "m".to_string(),
                        minute: before,
                        completed: 2,
                        refused: 1
                    },
                    HoldStatsDelta {
                        model: "n".to_string(),
                        minute: before,
                        completed: 0,
                        refused: 1
                    },
                ]
            );
        }
        assert!(state.take().is_empty(), "taken once");

        // A failed flush puts them back, added to what arrived since in the
        // same minute.
        let minute = minute_of(Utc::now());
        let failed = vec![HoldStatsDelta {
            model: "m".to_string(),
            minute,
            completed: 2,
            refused: 1,
        }];
        state.restore(failed, Utc::now());
        state.add("m", minute, 1, 0);
        assert_eq!(
            state.take(),
            vec![HoldStatsDelta {
                model: "m".to_string(),
                minute,
                completed: 3,
                refused: 1,
            }]
        );
    }

    #[test]
    fn counts_kept_back_by_a_failed_flush_stay_in_the_minute_they_happened() {
        // Five minutes of completions kept back by failed flushes are written
        // under their own minutes, not packed into the minute the flush
        // finally succeeds in (which would make them look like one minute's
        // work and inflate the rate).
        let state = state();
        let now = minute_of(Utc::now());
        let kept: Vec<_> = (1..=5)
            .map(|ago| HoldStatsDelta {
                model: "m".to_string(),
                minute: now - chrono::Duration::minutes(ago),
                completed: 10,
                refused: 0,
            })
            .collect();
        state.restore(kept.clone(), now);
        state.add("m", now, 10, 0);
        let mut expected = kept;
        expected.push(HoldStatsDelta {
            model: "m".to_string(),
            minute: now,
            completed: 10,
            refused: 0,
        });
        assert_eq!(sorted(state.take()), sorted(expected));

        // Counts past retention are dropped: no read would count them.
        let ancient = HoldStatsDelta {
            model: "m".to_string(),
            minute: now - chrono::Duration::hours(3),
            completed: 1,
            refused: 1,
        };
        state.restore(vec![ancient], now);
        assert!(state.take().is_empty());
    }

    #[test]
    fn an_unrepresentable_window_does_not_overflow_the_purge_cutoff() {
        let now = Utc::now();
        assert_eq!(
            retention_cutoff(now, Duration::MAX),
            None,
            "nothing is old enough to purge"
        );
        assert_eq!(
            retention_cutoff(now, Duration::from_secs(900)),
            Some(now - chrono::Duration::hours(2))
        );
        // Restoring under such a window keeps everything.
        let mut hold = DispatchHold::new(serde_json::json!([]), ["m".to_string()]);
        hold.window = Duration::MAX;
        let state = HoldState::new(hold, now);
        let minute = minute_of(now) - chrono::Duration::hours(3);
        let delta = HoldStatsDelta {
            model: "m".to_string(),
            minute,
            completed: 1,
            refused: 0,
        };
        state.restore(vec![delta.clone()], now);
        assert_eq!(state.take(), vec![delta]);
    }

    #[test]
    fn every_held_model_gets_a_decision() {
        let params = HoldParams {
            window_secs: 900.0,
            safety_margin: 0.2,
        };
        let now = Utc::now();
        let inputs = [fusillade_core::release::ModelHoldInputs {
            model: "busy".to_string(),
            completed: 0,
            refused: 4,
            held_since: Some(now - chrono::Duration::minutes(15)),
            first_refused: Some(now - chrono::Duration::minutes(15)),
            last_refused: Some(now - chrono::Duration::minutes(1)),
            refused_minutes: 4,
            backlog: vec![(now + chrono::Duration::hours(1), 10)],
        }];
        let models = ["busy".to_string(), "quiet".to_string()];
        // Every daemon reading the same counters and work decides alike.
        let decide = |memory: &mut HashMap<String, ModelHoldMemory>| {
            decide_all(
                &models,
                &inputs,
                params,
                now,
                chrono::Duration::minutes(5),
                memory,
            )
        };
        let mut a = HashMap::from([("gone".to_string(), ModelHoldMemory::default())]);
        let mut b = HashMap::new();
        let (claim, decisions) = decide(&mut a);
        assert_eq!(decide(&mut b).0.models, claim.models);
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
        assert_eq!(
            a.get("busy").and_then(|memory| memory.last_release_at),
            Some(now),
            "the release is remembered"
        );
        assert!(
            !a.contains_key("gone"),
            "models no longer held are forgotten"
        );
    }

    #[test]
    fn counters_outlive_the_window() {
        // At least two hours, and two windows of a longer window, so the read
        // never divides a whole window's time into part of its counters.
        assert_eq!(
            stats_retention(Duration::from_secs(900)),
            chrono::Duration::hours(2)
        );
        assert_eq!(
            stats_retention(Duration::from_secs(4 * 3600)),
            chrono::Duration::hours(8)
        );
    }
}
