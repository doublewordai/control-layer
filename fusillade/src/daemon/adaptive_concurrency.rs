//! Per-model adaptive concurrency control.
//!
//! With `adaptive_concurrency` on, the controller discovers a model's
//! sustainable in-flight count from downstream backpressure rather than running
//! at its configured limit unconditionally. The configured limit is where each
//! model starts; from there the controller owns the number, and
//! `max_total_in_flight` is what bounds the process.
//!
//! There is deliberately no per-model ceiling. A static number is too high at
//! one model replica and far too low at a hundred, which is the whole reason for
//! this; capping the controller with one would leave the "too low" half unfixed.
//! Memory is bounded by total in-flight across all models, not per model, so
//! `max_total_in_flight` is the guard that actually corresponds to the risk -
//! and if one model's discovered limit crowds out the others under that cap,
//! that is a signal to scale fusillade out rather than something to arbitrate
//! here.
//!
//! Turning `adaptive_concurrency` off returns a model to running at its
//! configured limit, exactly as before, so the flag is a safe thing to flip.
//!
//! # How it moves
//!
//! **Down**: a request comes back HTTP 529, meaning the model had nowhere to put
//! it. Multiply the limit by `cut_factor`.
//!
//! **Up**: a model used every slot it was offered on the last claim. Multiply
//! the limit by `growth_factor`.
//!
//! Both directions are multiplicative, so nothing here is sized to the fleet: a
//! model running at 500 and one running at 50,000 take the same number of steps
//! to move by the same proportion. A fixed number of requests per step would be
//! too coarse for the first and hopeless for the second - recovering 40,000
//! concurrency in steps of 16 takes most of an hour, and one raise per claim
//! cycle is the ceiling on how fast steps can happen.
//!
//! Fast recovery is not a nicety. Dynamo rejects by priority, so batch work is
//! pushed down to almost no concurrency whenever realtime traffic is busy. Its
//! limit has to climb straight back when the pressure lifts, and the controller
//! cannot tell "the model is full" from "I am being outranked".
//!
//! # Why requests are stamped
//!
//! In-flight work is never cancelled, so after a cut the requests sent under the
//! old limit keep failing for up to a request lifetime. Reacting to those would
//! cut again and again for a single overload event - a scale-down evicting
//! thousands of requests at once would drive the limit to its floor of 1 in
//! seconds, when the model may still have had plenty of capacity.
//!
//! So each request carries the value of a counter, and the counter is bumped on
//! every adjustment. A 529 whose stamp does not match the current counter is
//! dropped: it is news about a limit we have already moved away from. One
//! overload event therefore costs one cut, while genuinely sustained overload
//! keeps producing fresh reports and keeps cutting.
//!
//! A timer cannot do this. The tail of stale failures lasts a request lifetime,
//! so any interval short enough to react promptly is also short enough to cut
//! dozens of times on the same event.
//!
//! # What this costs
//!
//! Growing past the model's real capacity is how we find out where it is, so in
//! steady state the limit oscillates around it and a fraction of requests are
//! rejected. Those are retried rather than lost, but they are wasted work and a
//! database write each, and `growth_factor` sets how much of it there is: bigger
//! recovers faster and overshoots further.
//!
//! An alternative would be to read the number of requests actually admitted when
//! rejections start - that *is* the capacity - and use it directly, which would
//! sit closer to the true limit with no ongoing rejections. It is worth
//! revisiting once there is production data on whether that number is stable
//! enough to read.

use dashmap::DashMap;

/// A change to a model's effective concurrency limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ConcurrencyAdjustment {
    pub previous_limit: usize,
    pub new_limit: usize,
}

/// Which version of a model's limit a request was sent under.
///
/// A 529 carrying an old stamp is dropped - it is news about a limit that has
/// already been changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Generation(u64);

#[derive(Debug)]
struct ModelState {
    limit: usize,
    /// Bumped on every change to `limit`. Requests carry the value they were
    /// sent under.
    generation: u64,
    /// Requests sent since the last change to `limit`.
    ///
    /// Load-bearing, despite looking redundant against "grow at most once per
    /// claim cycle". A cut opens a new generation, so without this the very next
    /// cycle would raise the limit again having sent nothing under it - and
    /// since one cut is all a generation can produce, the limit would climb
    /// `growth_factor` and fall `cut_factor` per cycle, netting a rise however
    /// hard the model is being rejected. It runs away to overflow in seconds.
    dispatched: usize,
}

impl ModelState {
    fn new(configured: usize) -> Self {
        Self {
            limit: configured.max(1),
            generation: 0,
            dispatched: 0,
        }
    }

    /// Called on every change to `limit`. Bumping the counter is what discards
    /// reports about the old limit, so nothing else has to track whether we
    /// have already reacted.
    fn open_new_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.dispatched = 0;
    }
}

#[derive(Debug)]
pub(super) struct AdaptiveConcurrencyController {
    states: DashMap<String, ModelState>,
    growth_factor: f64,
    cut_factor: f64,
}

impl AdaptiveConcurrencyController {
    pub(super) fn new(growth_factor: f64, cut_factor: f64) -> Self {
        Self {
            states: DashMap::new(),
            growth_factor: growth_factor.clamp(1.01, 10.0),
            cut_factor: cut_factor.clamp(0.05, 0.99),
        }
    }

    /// The model's current limit, starting from its configured value the first
    /// time we see it.
    pub(super) fn limit(&self, model: &str, configured: usize) -> usize {
        self.states
            .entry(model.to_owned())
            .or_insert_with(|| ModelState::new(configured))
            .limit
    }

    /// The stamp a request going out now should carry, counting it toward the
    /// evidence the next raise needs.
    pub(super) fn stamp(&self, model: &str, configured: usize) -> Generation {
        let mut state = self
            .states
            .entry(model.to_owned())
            .or_insert_with(|| ModelState::new(configured));
        state.dispatched = state.dispatched.saturating_add(1);
        Generation(state.generation)
    }

    /// A request came back 529. Cut the limit, unless this is an echo.
    ///
    /// `generation` is the stamp the request went out with. If it does not match
    /// the current one, the limit has already been changed since it was sent, so
    /// this tells us nothing new and is dropped. That is what stops one overload
    /// event - which can produce thousands of these - from cutting more than
    /// once.
    pub(super) fn record_overload(
        &self,
        model: &str,
        generation: Generation,
    ) -> Option<ConcurrencyAdjustment> {
        let mut state = self.states.get_mut(model)?;
        if state.generation != generation.0 {
            return None;
        }

        let previous_limit = state.limit;
        // On a small limit the multiply rounds back to where it started - 4 x
        // 0.8 floors to 3, but 2 x 0.9 floors to 1... and 1 x 0.9 floors to 0.
        // Force at least one off, and never below 1, so it always moves and
        // never stops sending altogether.
        let scaled = (previous_limit as f64 * self.cut_factor).floor() as usize;
        let new_limit = scaled.min(previous_limit.saturating_sub(1)).max(1);

        state.limit = new_limit;
        state.open_new_generation();

        (new_limit != previous_limit).then_some(ConcurrencyAdjustment {
            previous_limit,
            new_limit,
        })
    }

    /// Raise the limit.
    ///
    /// Only call this for a model that used every slot it was offered on the
    /// last claim. A model that used fewer had run out of work, and raising its
    /// limit would achieve nothing except leaving a large number sitting there
    /// for the next burst of traffic to dispatch all at once.
    ///
    /// Growth is multiplicative for the same reason cuts are: a fixed number of
    /// requests per step is either too big for a small model or hopeless for a
    /// large one. Doubling-ish recovers a model that was cut to near nothing in
    /// a handful of claim cycles, which matters because Dynamo rejects by
    /// priority - batch work gets pushed down to almost no concurrency whenever
    /// realtime is busy, and has to come straight back when it isn't.
    ///
    /// Returns `None` until as many requests have been sent under the current
    /// limit as the raise is about to add. Sending N without a rejection is what
    /// earns the right to add N more, and skipping it lets the limit climb on a
    /// generation that has proved nothing - which compounds into a runaway,
    /// because a cut can only fire once per generation while a raise fires every
    /// claim cycle.
    pub(super) fn try_grow(&self, model: &str) -> Option<ConcurrencyAdjustment> {
        let mut state = self.states.get_mut(model)?;

        let previous_limit = state.limit;
        let grown = (previous_limit as f64 * self.growth_factor).ceil() as usize;
        // `ceil` already guarantees movement above 1; the explicit +1 covers a
        // growth factor clamped so low that it rounds back to where it started.
        let new_limit = grown.max(previous_limit.saturating_add(1));

        if state.dispatched < new_limit - previous_limit {
            return None;
        }

        state.limit = new_limit;
        state.open_new_generation();

        Some(ConcurrencyAdjustment {
            previous_limit,
            new_limit,
        })
    }
}

/// Per-model claim capacity: the controller's limit, less what is already in
/// flight.
pub(super) fn available_capacity_for_model(limit: usize, in_flight: usize) -> usize {
    limit.saturating_sub(in_flight)
}

/// Scale per-model capacities down so their total fits the process-wide
/// in-flight ceiling.
///
/// This is the only bound once the controller is running, and it is the one that
/// matches the risk: memory is total in-flight times request size, across all
/// models. Without it the first successful ramp exhausts the instance.
///
/// Hitting it is a signal to scale fusillade out rather than a state to sit in.
/// Scaling is proportional so no model is starved
/// outright, with any rounding remainder handed out largest-first so the result
/// is deterministic.
pub(super) fn apply_total_in_flight_cap(
    capacities: &mut std::collections::HashMap<String, usize>,
    total_in_flight: usize,
    max_total_in_flight: usize,
) {
    if max_total_in_flight == 0 {
        return;
    }

    let headroom = max_total_in_flight.saturating_sub(total_in_flight);
    let requested: usize = capacities.values().sum();
    if requested <= headroom {
        return;
    }
    if headroom == 0 {
        capacities.clear();
        return;
    }

    let mut scaled: Vec<(String, usize)> = capacities
        .iter()
        .map(|(model, capacity)| {
            // Integer division floors, so the scaled total never exceeds
            // headroom before the remainder is distributed.
            let share = (*capacity as u128 * headroom as u128 / requested as u128) as usize;
            (model.clone(), share)
        })
        .collect();

    // Largest original capacity first, model name as a tiebreak, so two daemons
    // with identical inputs make identical decisions.
    scaled.sort_by(|(left_model, _), (right_model, _)| {
        let left = capacities.get(left_model).copied().unwrap_or(0);
        let right = capacities.get(right_model).copied().unwrap_or(0);
        right.cmp(&left).then_with(|| left_model.cmp(right_model))
    });

    let mut remainder = headroom.saturating_sub(scaled.iter().map(|(_, share)| *share).sum());
    for (_, share) in scaled.iter_mut() {
        if remainder == 0 {
            break;
        }
        *share += 1;
        remainder -= 1;
    }

    capacities.clear();
    for (model, share) in scaled {
        if share > 0 {
            capacities.insert(model, share);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const GROWTH: f64 = 1.5;
    const CUT: f64 = 0.8;

    fn controller() -> AdaptiveConcurrencyController {
        AdaptiveConcurrencyController::new(GROWTH, CUT)
    }

    #[test]
    fn starts_at_the_configured_limit_and_can_go_above_it() {
        // The configured value is where a model starts, not a cap. Being able to
        // exceed it is the point: a static number sized for one model replica is
        // far too low at a hundred, and a controller that could only ratchet down
        // would leave that unfixed.
        let controller = controller();
        assert_eq!(controller.limit("m", 100), 100);

        // Nothing has been sent yet, so nothing has been proved.
        assert_eq!(controller.try_grow("m"), None);

        for _ in 0..50 {
            controller.stamp("m", 100);
        }
        assert!(controller.try_grow("m").is_some());
        assert_eq!(controller.limit("m", 100), 150);
    }

    #[test]
    fn recovery_from_a_cut_takes_a_handful_of_steps_at_any_scale() {
        // The reason growth is multiplicative. Dynamo rejects by priority, so
        // batch work is cut to almost nothing whenever realtime is busy and has
        // to climb straight back. A fixed step of 16 would take ~2,500 claim
        // cycles to recover a 40,000 gap; this takes single digits.
        let controller = controller();
        let start = 60_000;

        let generation = controller.stamp("m", start);
        controller.record_overload("m", generation);
        assert!(controller.limit("m", start) < start);

        let mut steps = 0;
        while controller.limit("m", start) < start {
            // Each raise has to be earned by sending as many requests as it
            // will add, so the traffic that pays for it happens here.
            let limit = controller.limit("m", start);
            for _ in 0..limit {
                controller.stamp("m", start);
            }
            controller.try_grow("m");
            steps += 1;
            assert!(steps <= 5, "took {steps} steps to recover");
        }
    }

    #[test]
    fn a_model_being_rejected_does_not_climb() {
        // Caught by the simulation, not by construction. A raise fires every
        // claim cycle while a cut can only fire once per generation, so growth
        // that is not earned nets `growth_factor * cut_factor` per cycle - above
        // 1 for any sensible pair - and the limit runs away to overflow within
        // seconds while every request is being rejected.
        let controller = controller();
        let mut limit = controller.limit("m", 150);

        for _ in 0..200 {
            // A cycle against a saturated upstream: claim up to the limit, send,
            // and have the first of them rejected. The rest carry the superseded
            // stamp and are discarded.
            let generation = controller.stamp("m", 150);
            for _ in 0..limit {
                controller.stamp("m", 150);
            }
            controller.record_overload("m", generation);
            controller.try_grow("m");

            let next = controller.limit("m", 150);
            assert!(
                next <= limit,
                "limit climbed from {limit} to {next} while everything was being rejected"
            );
            limit = next;
        }

        assert_eq!(limit, 1, "sustained rejection should walk the limit down");
    }

    #[test]
    fn overload_cuts_multiplicatively() {
        let controller = controller();
        let generation = controller.stamp("m", 100);

        assert_eq!(
            controller.record_overload("m", generation),
            Some(ConcurrencyAdjustment {
                previous_limit: 100,
                new_limit: 80,
            })
        );
        assert_eq!(controller.limit("m", 100), 80);
    }

    #[test]
    fn a_burst_of_overloads_costs_exactly_one_cut() {
        // The failure this exists to prevent: a scale-down evicts many in-flight
        // requests at once, and reacting to each one collapses the limit far
        // below where it should land.
        let controller = controller();
        let generation = controller.stamp("m", 100);

        assert!(controller.record_overload("m", generation).is_some());
        for _ in 0..50 {
            assert_eq!(controller.record_overload("m", generation), None);
        }
        assert_eq!(controller.limit("m", 100), 80);
    }

    #[test]
    fn sustained_overload_still_walks_the_limit_down() {
        // A real capacity drop is not a burst: each new generation that gets
        // rejected must cut again, or the controller never reaches the new
        // capacity.
        let controller = controller();

        let first = controller.stamp("m", 100);
        assert!(controller.record_overload("m", first).is_some());

        let second = controller.stamp("m", 100);
        assert_eq!(
            controller.record_overload("m", second),
            Some(ConcurrencyAdjustment {
                previous_limit: 80,
                new_limit: 64,
            })
        );
    }

    #[test]
    fn overloads_from_a_stale_generation_are_ignored() {
        // Requests dispatched before the last adjustment carry evidence that
        // predates it. Acting on them cuts twice for one overload event.
        let controller = controller();
        let stale = controller.stamp("m", 100);
        assert!(controller.record_overload("m", stale).is_some());
        assert_eq!(controller.limit("m", 100), 80);

        // Any adjustment supersedes the stamp, growth included.
        for _ in 0..40 {
            controller.stamp("m", 100);
        }
        assert!(controller.try_grow("m").is_some());
        assert_eq!(controller.record_overload("m", stale), None);
        assert_eq!(controller.limit("m", 100), 120);
    }

    #[test]
    fn small_limits_still_make_progress_downward_and_never_reach_zero() {
        // A gentle cut factor floors to a no-op on small limits. A controller
        // that cannot move is worse than one that moves slowly.
        let controller = AdaptiveConcurrencyController::new(GROWTH, 0.99);

        let mut previous = controller.limit("m", 4);
        for _ in 0..10 {
            let generation = controller.stamp("m", 4);
            controller.record_overload("m", generation);
            let current = controller.limit("m", 4);
            assert!(current < previous || current == 1);
            previous = current;
        }
        assert_eq!(controller.limit("m", 4), 1);
    }

    #[test]
    fn models_are_independent() {
        let controller = controller();
        let generation = controller.stamp("a", 100);
        controller.record_overload("a", generation);

        assert_eq!(controller.limit("a", 100), 80);
        assert_eq!(controller.limit("b", 100), 100);
    }

    #[test]
    fn total_cap_leaves_capacities_alone_when_they_fit() {
        let mut capacities = HashMap::from([("a".to_string(), 10), ("b".to_string(), 20)]);
        apply_total_in_flight_cap(&mut capacities, 100, 1000);
        assert_eq!(
            capacities,
            HashMap::from([("a".into(), 10), ("b".into(), 20)])
        );
    }

    #[test]
    fn total_cap_is_disabled_by_zero() {
        let mut capacities = HashMap::from([("a".to_string(), 10)]);
        apply_total_in_flight_cap(&mut capacities, 10_000, 0);
        assert_eq!(capacities, HashMap::from([("a".into(), 10)]));
    }

    #[test]
    fn total_cap_scales_proportionally_and_never_exceeds_headroom() {
        // 60 in flight against a ceiling of 100 leaves 40 to hand out, against
        // 120 requested.
        let mut capacities = HashMap::from([
            ("a".to_string(), 60),
            ("b".to_string(), 40),
            ("c".to_string(), 20),
        ]);
        apply_total_in_flight_cap(&mut capacities, 60, 100);

        let total: usize = capacities.values().sum();
        assert_eq!(total, 40);
        assert!(capacities["a"] > capacities["b"]);
        assert!(capacities["b"] > capacities["c"]);
    }

    #[test]
    fn total_cap_yields_nothing_when_already_at_the_ceiling() {
        let mut capacities = HashMap::from([("a".to_string(), 10)]);
        apply_total_in_flight_cap(&mut capacities, 100, 100);
        assert!(capacities.is_empty());
    }

    #[test]
    fn total_cap_keeps_small_models_alive_via_the_remainder() {
        // Proportional flooring alone would give the small model zero and starve
        // it entirely; the remainder pass has to be there.
        let mut capacities = HashMap::from([("big".to_string(), 100), ("small".to_string(), 1)]);
        apply_total_in_flight_cap(&mut capacities, 0, 10);

        assert_eq!(capacities.values().sum::<usize>(), 10);
    }
}
