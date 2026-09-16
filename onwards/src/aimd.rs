//! Per-process, priority-pool preferred-first share control.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

/// Priority-pool defaults; set enabled=false to retain ordinary selection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AimdConfig {
    pub enabled: bool,
    pub latency_budget_ms: u64,
    pub breach_rate_target: f64,
    pub window_samples: usize,
    pub min_samples: usize,
    pub share_step: f64,
    pub share_decay: f64,
    pub share_floor: f64,
    pub dwell_ms: u64,
}

impl Default for AimdConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            latency_budget_ms: 10_000,
            breach_rate_target: 0.05,
            window_samples: 200,
            min_samples: 50,
            share_step: 0.02,
            share_decay: 0.8,
            share_floor: 0.05,
            dwell_ms: 30_000,
        }
    }
}

impl AimdConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.latency_budget_ms == 0
            || self.latency_budget_ms > 3_600_000
            || self.dwell_ms == 0
            || self.dwell_ms > 86_400_000
        {
            return Err("AIMD budget must be 1..=3600000 ms and dwell 1..=86400000 ms");
        }
        if self.min_samples < 2
            || self.min_samples > self.window_samples
            || self.window_samples > 100_000
        {
            return Err("AIMD requires 2 <= min_samples <= window_samples <= 100000");
        }
        if !self.breach_rate_target.is_finite()
            || !(0.0..1.0).contains(&self.breach_rate_target)
            || !self.share_decay.is_finite()
            || self.share_decay <= 0.0
            || self.share_decay >= 1.0
            || !self.share_step.is_finite()
            || self.share_step <= 0.0
            || self.share_step > 1.0
            || !self.share_floor.is_finite()
            || self.share_floor <= 0.0
            || self.share_floor > 1.0
        {
            return Err("AIMD requires target in [0,1), decay in (0,1), and step/floor in (0,1]");
        }
        Ok(())
    }
}

/// Unknown and unfinished attempts stay in the start-ordered window. They
/// inhibit adjustment until displaced, rather than biasing the rate downward.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Outcome {
    Healthy,
    Breach,
    Unknown,
}

#[derive(Debug)]
pub(crate) struct Controller {
    config: AimdConfig,
    active: bool,
    share: f64,
    generation: u64,
    next_id: u64,
    window: VecDeque<(u64, Option<Outcome>)>,
    pending: usize,
    unknown: usize,
    breaches: usize,
    last_adjustment: Instant,
    healthy_since: Option<Instant>,
}

impl Controller {
    pub(crate) fn new(config: AimdConfig, now: Instant) -> Self {
        Self {
            config,
            active: true,
            share: 1.0,
            generation: 0,
            next_id: 0,
            window: VecDeque::new(),
            pending: 0,
            unknown: 0,
            breaches: 0,
            last_adjustment: now,
            healthy_since: None,
        }
    }
    pub(crate) fn share(&self) -> f64 {
        self.share
    }
    pub(crate) fn retire(&mut self) {
        self.active = false;
    }
    pub(crate) fn active(&self) -> bool {
        self.active
    }
    fn begin(&mut self) -> Option<(u64, u64)> {
        if !self.active {
            return None;
        }
        // Never evict an unfinished sample in favor of a newer, faster one.
        // At capacity stop admitting samples until the cohort has resolved.
        if self.window.len() == self.config.window_samples && self.pending > 0 {
            return None;
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.window.push_back((id, None));
        self.pending += 1;
        if self.window.len() > self.config.window_samples
            && let Some((_, Some(outcome))) = self.window.pop_front()
        {
            match outcome {
                Outcome::Breach => self.breaches -= 1,
                Outcome::Unknown => self.unknown -= 1,
                Outcome::Healthy => {}
            }
        }
        Some((self.generation, id))
    }
    fn finish(
        &mut self,
        generation: u64,
        id: u64,
        outcome: Outcome,
        now: Instant,
    ) -> Option<&'static str> {
        if !self.active || generation != self.generation {
            return None;
        }
        let first_id = self.window.front()?.0;
        let offset = usize::try_from(id.wrapping_sub(first_id)).ok()?;
        let entry = self.window.get_mut(offset)?;
        if entry.0 != id || entry.1.is_some() {
            return None;
        }
        entry.1 = Some(outcome);
        self.pending -= 1;
        match outcome {
            Outcome::Breach => self.breaches += 1,
            Outcome::Unknown => self.unknown += 1,
            Outcome::Healthy => {}
        }
        if self.unknown > 0 {
            self.healthy_since = None;
            return None;
        }
        // In-flight work is not evidence that the last healthy window became
        // unhealthy. Block decisions while pending without restarting dwell
        // on every completion of a concurrent cohort.
        if self.window.len() < self.config.min_samples || self.pending > 0 {
            return None;
        }
        let overloaded =
            self.breaches as f64 / self.window.len() as f64 > self.config.breach_rate_target;
        let dwell = Duration::from_millis(self.config.dwell_ms);
        if overloaded {
            self.healthy_since = None;
        } else {
            self.healthy_since.get_or_insert(now);
        }
        if now.duration_since(self.last_adjustment) < dwell {
            return None;
        }
        let next = if overloaded {
            (self.share * self.config.share_decay).max(self.config.share_floor)
        } else if now.duration_since(self.healthy_since.unwrap()) >= dwell {
            (self.share + self.config.share_step).min(1.0)
        } else {
            return None;
        };
        if next == self.share {
            return None;
        }
        let direction = if next < self.share {
            "decrease"
        } else {
            "increase"
        };
        self.share = next;
        self.generation = self.generation.wrapping_add(1);
        self.window.clear();
        self.pending = 0;
        self.unknown = 0;
        self.breaches = 0;
        self.last_adjustment = now;
        self.healthy_since = None;
        Some(direction)
    }
}

/// A single eligible preferred attempt. Clones share completion state so a
/// timeout, a frame, and dropping the response cannot count the attempt twice.
#[derive(Clone)]
pub(crate) struct Observation(Arc<Mutex<Attempt>>);
struct Attempt {
    controller: Arc<Mutex<Controller>>,
    generation: u64,
    id: u64,
    start: Instant,
    completed: bool,
    model: String,
    pool: String,
}
impl Observation {
    pub(crate) fn new(
        controller: Arc<Mutex<Controller>>,
        model: &str,
        pool: &str,
        start: Instant,
    ) -> Option<Self> {
        let (generation, id) = controller.lock().unwrap().begin()?;
        Some(Self(Arc::new(Mutex::new(Attempt {
            controller,
            generation,
            id,
            start,
            completed: false,
            model: model.into(),
            pool: pool.into(),
        }))))
    }
    pub(crate) fn frame(&self) {
        let mut attempt = self.0.lock().unwrap();
        let budget = attempt.controller.lock().unwrap().config.latency_budget_ms;
        let outcome = if attempt.start.elapsed() > Duration::from_millis(budget) {
            Outcome::Breach
        } else {
            Outcome::Healthy
        };
        attempt.finish(outcome);
    }
    pub(crate) fn deadline(&self) {
        let mut attempt = self.0.lock().unwrap();
        let budget = attempt.controller.lock().unwrap().config.latency_budget_ms;
        // A shorter inherited failover deadline is censored below the budget,
        // so it cannot establish a budget breach.
        let outcome = if attempt.start.elapsed() >= Duration::from_millis(budget) {
            Outcome::Breach
        } else {
            Outcome::Unknown
        };
        attempt.finish(outcome);
    }
    pub(crate) fn unknown(&self) {
        self.0.lock().unwrap().finish(Outcome::Unknown);
    }
}
impl Attempt {
    fn finish(&mut self, outcome: Outcome) {
        if self.completed {
            return;
        }
        self.completed = true;
        let mut controller = self.controller.lock().unwrap();
        if !controller.active() {
            return;
        }
        if let Some(direction) =
            controller.finish(self.generation, self.id, outcome, Instant::now())
        {
            metrics::counter!("onwards_share_adjustments_total", "model" => self.model.clone(), "pool" => self.pool.clone(), "direction" => direction).increment(1);
        }
        metrics::gauge!("onwards_provider_share", "model" => self.model.clone(), "pool" => self.pool.clone()).set(controller.share());
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        self.finish(Outcome::Unknown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn config() -> AimdConfig {
        AimdConfig {
            enabled: true,
            latency_budget_ms: 100,
            breach_rate_target: 0.2,
            window_samples: 10,
            min_samples: 5,
            share_step: 0.1,
            share_decay: 0.5,
            share_floor: 0.1,
            dwell_ms: 1000,
        }
    }
    fn sample(c: &mut Controller, outcome: Outcome, now: Instant) -> Option<&'static str> {
        let (generation, id) = c.begin().unwrap();
        c.finish(generation, id, outcome, now)
    }
    #[test]
    fn retired_controller_rejects_pending_and_new_observations() {
        let start = Instant::now();
        let mut controller = Controller::new(AimdConfig::default(), start);
        let (generation, id) = controller.begin().unwrap();
        controller.retire();
        assert_eq!(
            controller.finish(generation, id, Outcome::Breach, start),
            None
        );
        assert_eq!(controller.breaches, 0);
        assert_eq!(controller.begin(), None);
    }

    // Synthetic per-replica scenarios exercise low/high breach rates, sparse
    // traffic, and bounded healthy windows. Successful frames are within budget.
    #[test]
    fn synthetic_count_scenarios_are_evaluated_per_replica() {
        for (healthy, breaches, expected) in [
            (800, 12, 1.0),
            (850, 17, 1.0), // below 5% in each window
            (90, 22, 0.64),
            (90, 19, 0.64), // two fresh 50-sample windows
            (8, 3, 1.0),
            (11, 4, 1.0), // insufficient local samples
            (14000, 0, 1.0),
            (15000, 0, 1.0),
            (0, 0, 1.0),
        ] {
            let start = Instant::now();
            let mut controller = Controller::new(AimdConfig::default(), start);
            let total = healthy + breaches;
            for index in 0..total {
                let breach = (index + 1) * breaches / total > index * breaches / total;
                sample(
                    &mut controller,
                    if breach {
                        Outcome::Breach
                    } else {
                        Outcome::Healthy
                    },
                    start + Duration::from_secs(index as u64 + 1),
                );
                assert!(controller.window.len() <= 200);
            }
            assert!(
                (controller.share() - expected).abs() < 1e-10,
                "{healthy} healthy, {breaches} breaches: {}",
                controller.share()
            );
        }
    }

    #[test]
    fn deployment_counts_do_not_hide_unknown_coverage() {
        let start = Instant::now();
        let mut controller = Controller::new(AimdConfig::default(), start);
        sample(&mut controller, Outcome::Unknown, start);
        for index in 0..114 {
            let outcome = if (index + 1) * 22 / 114 > index * 22 / 114 {
                Outcome::Breach
            } else {
                Outcome::Healthy
            };
            sample(
                &mut controller,
                outcome,
                start + Duration::from_secs(index + 1),
            );
        }
        assert_eq!(controller.share(), 1.0);
    }

    #[tokio::test(start_paused = true)]
    async fn inherited_short_deadline_is_unknown_and_completion_is_idempotent() {
        let controller = Arc::new(Mutex::new(Controller::new(
            AimdConfig::default(),
            Instant::now(),
        )));
        let early =
            Observation::new(controller.clone(), "model", "default", Instant::now()).unwrap();
        tokio::time::advance(Duration::from_secs(5)).await;
        early.deadline();
        assert_eq!(controller.lock().unwrap().unknown, 1);
        assert_eq!(controller.lock().unwrap().breaches, 0);
        let late =
            Observation::new(controller.clone(), "model", "default", Instant::now()).unwrap();
        tokio::time::advance(Duration::from_secs(10)).await;
        late.deadline();
        late.frame();
        late.unknown();
        drop(late);
        drop(early);
        let c = controller.lock().unwrap();
        assert_eq!(c.unknown, 1);
        assert_eq!(c.breaches, 1);
        assert_eq!(c.pending, 0);
    }

    #[test]
    fn concurrent_cohorts_can_recover_without_ignoring_pending_samples() {
        let start = Instant::now();
        let mut c = Controller::new(config(), start);
        for _ in 0..5 {
            sample(&mut c, Outcome::Breach, start + Duration::from_secs(1));
        }
        let cohort: Vec<_> = (0..5).map(|_| c.begin().unwrap()).collect();
        for (g, id) in cohort {
            c.finish(g, id, Outcome::Healthy, start + Duration::from_secs(2));
        }
        assert_eq!(c.share(), 0.5);
        let cohort: Vec<_> = (0..5).map(|_| c.begin().unwrap()).collect();
        for &(g, id) in &cohort[..4] {
            c.finish(g, id, Outcome::Healthy, start + Duration::from_secs(4));
        }
        assert_eq!(c.share(), 0.5);
        let (g, id) = cohort[4];
        c.finish(g, id, Outcome::Healthy, start + Duration::from_secs(4));
        assert_eq!(c.share(), 0.6);
    }

    #[test]
    fn saturated_window_keeps_pending_attempts_and_bounds_memory() {
        let now = Instant::now();
        let mut c = Controller::new(config(), now);
        let pending: Vec<_> = (0..10).map(|_| c.begin().unwrap()).collect();
        assert!(c.begin().is_none());
        for &(generation, id) in &pending[1..] {
            c.finish(generation, id, Outcome::Healthy, now);
        }
        assert!(c.begin().is_none());
        c.finish(pending[0].0, pending[0].1, Outcome::Breach, now);
        assert!(c.begin().is_some());
        assert_eq!(c.window.len(), 10);
    }

    #[test]
    fn load_dependent_latency_converges_and_recovers_after_capacity_step() {
        let start = Instant::now();
        let mut c = Controller::new(config(), start);
        let mut now = start;
        for _ in 0..200 {
            now += Duration::from_millis(200);
            // Deterministic service with capacity for 60% of offered load.
            let outcome = if c.share() > 0.6 {
                Outcome::Breach
            } else {
                Outcome::Healthy
            };
            sample(&mut c, outcome, now);
        }
        assert!((0.25..=0.7).contains(&c.share()), "share {}", c.share());
        for _ in 0..200 {
            now += Duration::from_millis(200);
            sample(&mut c, Outcome::Healthy, now);
        }
        assert_eq!(c.share(), 1.0);
        for _ in 0..200 {
            now += Duration::from_millis(200);
            sample(&mut c, Outcome::Breach, now);
        }
        assert_eq!(c.share(), c.config.share_floor);
    }

    #[test]
    fn tail_rate_overload_and_fresh_generations() {
        let now = Instant::now();
        let mut c = Controller::new(config(), now);
        sample(&mut c, Outcome::Breach, now);
        for _ in 0..4 {
            sample(&mut c, Outcome::Healthy, now + Duration::from_secs(2));
        }
        assert_eq!(c.share(), 1.0); // One tail outlier is exactly the target.
        let stale = c.begin().unwrap();
        c.finish(
            stale.0,
            stale.1,
            Outcome::Breach,
            now + Duration::from_secs(2),
        );
        assert_eq!(c.share(), 0.5);
        c.finish(
            stale.0,
            stale.1,
            Outcome::Breach,
            now + Duration::from_secs(5),
        );
        for _ in 0..4 {
            sample(&mut c, Outcome::Breach, now + Duration::from_secs(5));
        }
        assert_eq!(c.share(), 0.5);
        sample(&mut c, Outcome::Breach, now + Duration::from_secs(5));
        assert_eq!(c.share(), 0.25);
    }
    #[test]
    fn recovery_dwell_and_unknowns() {
        let now = Instant::now();
        let mut c = Controller::new(config(), now);
        for _ in 0..5 {
            sample(&mut c, Outcome::Breach, now + Duration::from_secs(1));
        }
        assert_eq!(c.share(), 0.5);
        for _ in 0..5 {
            sample(&mut c, Outcome::Healthy, now + Duration::from_secs(2));
        }
        assert_eq!(c.share(), 0.5);
        sample(&mut c, Outcome::Healthy, now + Duration::from_secs(3));
        assert_eq!(c.share(), 0.6);
        sample(&mut c, Outcome::Unknown, now + Duration::from_secs(4));
        for _ in 0..9 {
            sample(&mut c, Outcome::Breach, now + Duration::from_secs(10));
        }
        assert_eq!(c.share(), 0.6);
        sample(&mut c, Outcome::Breach, now + Duration::from_secs(10));
        assert_eq!(c.share(), 0.3);
    }
    #[test]
    fn pending_attempts_and_dwell_block_repeated_decreases() {
        let now = Instant::now();
        let mut c = Controller::new(config(), now);
        let pending = c.begin().unwrap();
        for _ in 0..5 {
            sample(&mut c, Outcome::Breach, now + Duration::from_secs(2));
        }
        assert_eq!(c.share(), 1.0);
        c.finish(
            pending.0,
            pending.1,
            Outcome::Breach,
            now + Duration::from_secs(2),
        );
        assert_eq!(c.share(), 0.5);
        for _ in 0..5 {
            sample(&mut c, Outcome::Breach, now + Duration::from_millis(2500));
        }
        assert_eq!(c.share(), 0.5);
        sample(&mut c, Outcome::Breach, now + Duration::from_secs(3));
        assert_eq!(c.share(), 0.25);
    }
    #[test]
    fn validates_bounds() {
        assert!(config().validate().is_ok());
        let mut c = config();
        c.min_samples = 1;
        assert!(c.validate().is_err());
        let mut c = config();
        c.share_decay = f64::NAN;
        assert!(c.validate().is_err());
        let mut c = config();
        c.share_floor = 0.0;
        assert!(c.validate().is_err());
    }
}
