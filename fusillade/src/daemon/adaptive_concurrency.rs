use std::time::{Duration, Instant};

use dashmap::DashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ConcurrencyAdjustment {
    pub previous_limit: usize,
    pub new_limit: usize,
}

#[derive(Debug)]
struct ModelState {
    effective_limit: usize,
    next_decrease_at: Instant,
    next_increase_at: Instant,
}

#[derive(Debug)]
pub(super) struct AdaptiveConcurrencyController {
    states: DashMap<String, ModelState>,
    recovery_interval: Duration,
}

impl AdaptiveConcurrencyController {
    pub(super) fn new(recovery_interval: Duration) -> Self {
        Self {
            states: DashMap::new(),
            recovery_interval,
        }
    }

    pub(super) fn effective_limit(&self, model: &str, configured_limit: usize) -> usize {
        self.states
            .get(model)
            .map(|state| state.effective_limit.min(configured_limit))
            .unwrap_or(configured_limit)
    }

    pub(super) fn record_overload(
        &self,
        model: &str,
        configured_limit: usize,
        now: Instant,
    ) -> Option<ConcurrencyAdjustment> {
        if configured_limit == 0 {
            return None;
        }

        let mut state = self
            .states
            .entry(model.to_owned())
            .or_insert_with(|| ModelState {
                effective_limit: configured_limit,
                next_decrease_at: now,
                next_increase_at: now,
            });
        state.effective_limit = state.effective_limit.min(configured_limit);

        if now < state.next_decrease_at {
            return None;
        }

        let previous_limit = state.effective_limit;
        state.effective_limit = (previous_limit / 2).max(1);
        state.next_decrease_at = now + self.recovery_interval;
        state.next_increase_at = now + self.recovery_interval;

        (state.effective_limit != previous_limit).then_some(ConcurrencyAdjustment {
            previous_limit,
            new_limit: state.effective_limit,
        })
    }

    pub(super) fn record_success(
        &self,
        model: &str,
        configured_limit: usize,
        now: Instant,
    ) -> Option<ConcurrencyAdjustment> {
        if configured_limit == 0 {
            return None;
        }

        let mut state = self.states.get_mut(model)?;
        state.effective_limit = state.effective_limit.min(configured_limit);

        if state.effective_limit >= configured_limit || now < state.next_increase_at {
            return None;
        }

        let previous_limit = state.effective_limit;
        state.effective_limit += 1;
        state.next_increase_at = now + self.recovery_interval;

        Some(ConcurrencyAdjustment {
            previous_limit,
            new_limit: state.effective_limit,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{AdaptiveConcurrencyController, ConcurrencyAdjustment};

    const RECOVERY_INTERVAL: Duration = Duration::from_secs(1);

    #[test]
    fn overload_halves_the_configured_limit_with_a_floor_of_one() {
        let controller = AdaptiveConcurrencyController::new(RECOVERY_INTERVAL);
        let now = Instant::now();

        assert_eq!(
            controller.record_overload("large-model", 100, now),
            Some(ConcurrencyAdjustment {
                previous_limit: 100,
                new_limit: 50,
            })
        );
        assert_eq!(controller.effective_limit("large-model", 100), 50);

        assert_eq!(controller.record_overload("small-model", 1, now), None);
        assert_eq!(controller.effective_limit("small-model", 1), 1);
        assert_eq!(controller.effective_limit("disabled-model", 0), 0);
    }

    #[test]
    fn overload_burst_causes_only_one_decrease_per_interval() {
        let controller = AdaptiveConcurrencyController::new(RECOVERY_INTERVAL);
        let now = Instant::now();

        assert!(controller.record_overload("model", 100, now).is_some());
        assert_eq!(
            controller.record_overload("model", 100, now + Duration::from_millis(999)),
            None
        );
        assert_eq!(controller.effective_limit("model", 100), 50);

        assert_eq!(
            controller.record_overload("model", 100, now + RECOVERY_INTERVAL),
            Some(ConcurrencyAdjustment {
                previous_limit: 50,
                new_limit: 25,
            })
        );
    }

    #[test]
    fn successes_recover_additively_at_most_once_per_interval() {
        let controller = AdaptiveConcurrencyController::new(RECOVERY_INTERVAL);
        let now = Instant::now();
        controller.record_overload("model", 8, now);

        assert_eq!(
            controller.record_success("model", 8, now + Duration::from_millis(999)),
            None
        );
        assert_eq!(
            controller.record_success("model", 8, now + RECOVERY_INTERVAL),
            Some(ConcurrencyAdjustment {
                previous_limit: 4,
                new_limit: 5,
            })
        );
        assert_eq!(
            controller.record_success("model", 8, now + RECOVERY_INTERVAL),
            None
        );
        assert_eq!(
            controller.record_success("model", 8, now + RECOVERY_INTERVAL * 2),
            Some(ConcurrencyAdjustment {
                previous_limit: 5,
                new_limit: 6,
            })
        );
    }

    #[test]
    fn effective_limit_tracks_runtime_changes_to_the_configured_ceiling() {
        let controller = AdaptiveConcurrencyController::new(RECOVERY_INTERVAL);
        let now = Instant::now();
        controller.record_overload("model", 100, now);

        assert_eq!(controller.effective_limit("model", 20), 20);
        assert_eq!(
            controller.record_success("model", 20, now + RECOVERY_INTERVAL),
            None
        );
        assert_eq!(controller.effective_limit("model", 20), 20);

        assert_eq!(
            controller.record_success("model", 60, now + RECOVERY_INTERVAL * 2),
            Some(ConcurrencyAdjustment {
                previous_limit: 20,
                new_limit: 21,
            })
        );
    }

    #[test]
    fn disabling_and_reenabling_a_model_does_not_wedge_its_limit_at_zero() {
        let controller = AdaptiveConcurrencyController::new(RECOVERY_INTERVAL);
        let now = Instant::now();
        controller.record_overload("model", 100, now);

        assert_eq!(controller.effective_limit("model", 0), 0);
        assert_eq!(
            controller.record_overload("model", 0, now + RECOVERY_INTERVAL),
            None
        );
        assert_eq!(
            controller.record_success("model", 0, now + RECOVERY_INTERVAL),
            None
        );

        assert_eq!(controller.effective_limit("model", 100), 50);
        assert_eq!(
            controller.record_success("model", 100, now + RECOVERY_INTERVAL),
            Some(ConcurrencyAdjustment {
                previous_limit: 50,
                new_limit: 51,
            })
        );
    }

    #[test]
    fn independent_models_keep_independent_limits() {
        let controller = AdaptiveConcurrencyController::new(RECOVERY_INTERVAL);
        let now = Instant::now();

        controller.record_overload("model-a", 100, now);

        assert_eq!(controller.effective_limit("model-a", 100), 50);
        assert_eq!(controller.effective_limit("model-b", 100), 100);
    }
}
