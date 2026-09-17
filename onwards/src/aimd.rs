//! Per-process, priority-pool preferred-first share control.
//!
//! A pool's controller holds a share `f`: the probability that an eligible
//! request tries the preferred provider (the first in definition order) first.
//! It watches completed first-token outcomes of preferred attempts and moves
//! `f` with additive increase / multiplicative decrease:
//!
//! - **Decrease** when the window's breach rate exceeds `breach_rate_target`.
//! - **Increase** once the breach rate has stayed at or below
//!   `recovery_breach_rate` for a dwell. Rates in between hold the share, so
//!   noise around a single threshold cannot make it hunt.
//! - **Idle recovery** steps the share up when a pool goes `idle_recovery_ms`
//!   without enough samples to judge, so a low-traffic pool is not left
//!   degraded indefinitely after an incident.
//!
//! Decisions use completed samples only. Unknown outcomes (cancelled attempts,
//! non-overload upstream errors, censored short deadlines) are excluded from
//! the rate rather than blocking decisions, and in-flight attempts never hold a
//! decision back: under sustained concurrency something is always in flight.
//! Upstream statuses listed in `overload_statuses` count as breaches, because
//! a provider shedding load is the strongest overload signal there is.
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
    /// First-frame latency, in milliseconds, beyond which an attempt breaches.
    pub latency_budget_ms: u64,
    /// Decrease the share when the window's breach rate exceeds this.
    pub breach_rate_target: f64,
    /// Increase the share only while the breach rate is at or below this.
    /// Rates between this and `breach_rate_target` hold the share steady.
    pub recovery_breach_rate: f64,
    /// Completed samples kept for the rate.
    pub window_samples: usize,
    /// Completed samples required before any latency-driven decision.
    pub min_samples: usize,
    /// Additive increase per recovery step.
    pub share_step: f64,
    /// Multiplicative decrease on overload.
    pub share_decay: f64,
    /// The share never drops below this, so real traffic keeps measuring the
    /// preferred provider and recovery needs no synthetic probes.
    pub share_floor: f64,
    /// Minimum interval between adjustments, and how long a healthy rate must
    /// hold before an increase.
    pub dwell_ms: u64,
    /// Step the share up after this long without enough samples to judge.
    /// `0` disables idle recovery.
    pub idle_recovery_ms: u64,
    /// Upstream error statuses from the preferred provider that count as
    /// breaches (e.g. over-capacity or rate-limited). Other error statuses are
    /// unknown outcomes.
    pub overload_statuses: Vec<u16>,
}

impl Default for AimdConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            latency_budget_ms: 10_000,
            breach_rate_target: 0.10,
            recovery_breach_rate: 0.03,
            window_samples: 100,
            min_samples: 20,
            share_step: 0.05,
            share_decay: 0.8,
            share_floor: 0.05,
            dwell_ms: 30_000,
            idle_recovery_ms: 300_000,
            overload_statuses: vec![429, 503, 529],
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
        if self.idle_recovery_ms > 86_400_000 {
            return Err("AIMD idle_recovery_ms must be at most 86400000 (0 disables it)");
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
        if !self.recovery_breach_rate.is_finite()
            || self.recovery_breach_rate < 0.0
            || self.recovery_breach_rate > self.breach_rate_target
        {
            return Err("AIMD requires 0 <= recovery_breach_rate <= breach_rate_target");
        }
        if self.overload_statuses.len() > 32
            || self
                .overload_statuses
                .iter()
                .any(|status| !(400..=599).contains(status))
        {
            return Err("AIMD overload_statuses must be at most 32 HTTP error statuses (400-599)");
        }
        Ok(())
    }
}

/// How one preferred attempt ended, as far as the controller is concerned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Outcome {
    Healthy,
    Breach,
    /// Excluded from the rate: says nothing about the provider's capacity.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Active,
    /// Kept across reloads while the pool is temporarily ineligible (for
    /// example its preferred provider was disabled), so it can resume if the
    /// same preferred provider returns.
    Parked,
    Retired,
}

#[derive(Debug)]
pub(crate) struct Controller {
    config: AimdConfig,
    state: State,
    share: f64,
    /// Bumped on every decrease and on resume: attempts that started under an
    /// older generation report on a share that no longer applies.
    generation: u64,
    /// Completed outcomes of the current generation; `true` is a breach.
    window: VecDeque<bool>,
    breaches: usize,
    /// Attempts begun and not yet finished, across generations. Observability
    /// only: in-flight work never holds a decision back.
    in_flight: usize,
    last_adjustment: Instant,
    healthy_since: Option<Instant>,
    /// `(model, pool)` metric labels, learned from the request path so state
    /// transitions made during reloads can still be published.
    labels: Option<(String, String)>,
}

impl Controller {
    pub(crate) fn new(config: AimdConfig, now: Instant) -> Self {
        Self {
            config,
            state: State::Active,
            share: 1.0,
            generation: 0,
            window: VecDeque::new(),
            breaches: 0,
            in_flight: 0,
            last_adjustment: now,
            healthy_since: None,
            labels: None,
        }
    }

    pub(crate) fn config(&self) -> &AimdConfig {
        &self.config
    }

    pub(crate) fn share(&self) -> f64 {
        self.share
    }

    pub(crate) fn active(&self) -> bool {
        self.state == State::Active
    }

    /// Stop permanently: the pool's AIMD configuration or preferred provider
    /// changed, so what this controller learned no longer applies.
    pub(crate) fn retire(&mut self) {
        self.state = State::Retired;
        self.publish();
    }

    /// Stop while keeping the learned share, for a pool that is temporarily
    /// ineligible. A retired controller stays retired.
    pub(crate) fn park(&mut self) {
        if self.state == State::Active {
            self.state = State::Parked;
            self.publish();
        }
    }

    /// Resume a parked controller. The share is kept (and idle recovery
    /// credits the time spent parked), but samples from before parking are
    /// discarded and attempts begun before it no longer count as outcomes; they
    /// still leave `in_flight` as they finish or drop. Returns whether the
    /// controller resumed.
    pub(crate) fn resume(&mut self) -> bool {
        if self.state != State::Parked {
            return false;
        }
        self.state = State::Active;
        self.generation = self.generation.wrapping_add(1);
        self.window.clear();
        self.breaches = 0;
        self.healthy_since = None;
        self.publish();
        true
    }

    pub(crate) fn remember_labels(&mut self, model: &str, pool: &str) {
        if self.labels.is_none() {
            self.labels = Some((model.to_string(), pool.to_string()));
        }
    }

    fn begin(&mut self) -> Option<u64> {
        if !self.active() {
            return None;
        }
        self.in_flight += 1;
        Some(self.generation)
    }

    /// Idle recovery: step the share up for each `idle_recovery_ms` elapsed
    /// since the last adjustment while the window lacks enough samples to
    /// judge. Returns `"increase"` when the share moved.
    pub(crate) fn tick(&mut self, now: Instant) -> Option<&'static str> {
        if !self.active()
            || self.config.idle_recovery_ms == 0
            || self.share >= 1.0
            || self.window.len() >= self.config.min_samples
        {
            return None;
        }
        let interval = Duration::from_millis(self.config.idle_recovery_ms);
        let elapsed = now.saturating_duration_since(self.last_adjustment);
        let steps = u32::try_from(elapsed.as_millis() / interval.as_millis()).unwrap_or(u32::MAX);
        if steps == 0 {
            return None;
        }
        self.share = (self.share + self.config.share_step * f64::from(steps)).min(1.0);
        self.last_adjustment += interval * steps;
        self.healthy_since = None;
        Some("increase")
    }

    fn finish(&mut self, generation: u64, outcome: Outcome, now: Instant) -> Option<&'static str> {
        self.in_flight = self.in_flight.saturating_sub(1);
        if !self.active() || generation != self.generation {
            return None;
        }
        let breach = match outcome {
            Outcome::Unknown => return None,
            Outcome::Healthy => false,
            Outcome::Breach => true,
        };
        self.window.push_back(breach);
        self.breaches += usize::from(breach);
        if self.window.len() > self.config.window_samples && self.window.pop_front() == Some(true) {
            self.breaches -= 1;
        }
        self.decide(now)
    }

    fn decide(&mut self, now: Instant) -> Option<&'static str> {
        let samples = self.window.len();
        if samples < self.config.min_samples {
            return None;
        }
        let rate = self.breaches as f64 / samples as f64;
        let settled = now.saturating_duration_since(self.last_adjustment)
            >= Duration::from_millis(self.config.dwell_ms);
        if rate > self.config.breach_rate_target {
            self.healthy_since = None;
            let next = (self.share * self.config.share_decay).max(self.config.share_floor);
            if !settled || next >= self.share {
                return None;
            }
            self.share = next;
            // Samples taken at the old share say nothing about the new one.
            self.generation = self.generation.wrapping_add(1);
            self.window.clear();
            self.breaches = 0;
            self.last_adjustment = now;
            return Some("decrease");
        }
        if rate > self.config.recovery_breach_rate {
            // Inside the hysteresis band: hold.
            self.healthy_since = None;
            return None;
        }
        let healthy_since = *self.healthy_since.get_or_insert(now);
        if self.share >= 1.0
            || !settled
            || now.saturating_duration_since(healthy_since)
                < Duration::from_millis(self.config.dwell_ms)
        {
            return None;
        }
        // Keep the window: a still-healthy window is evidence for the next
        // step too, which is what makes recovery take minutes, not hours.
        self.share = (self.share + self.config.share_step).min(1.0);
        self.last_adjustment = now;
        self.healthy_since = Some(now);
        Some("increase")
    }

    /// Export the controller's state under its learned labels.
    pub(crate) fn publish(&self) {
        let Some((model, pool)) = &self.labels else {
            return;
        };
        let samples = self.window.len();
        let rate = if samples == 0 {
            0.0
        } else {
            self.breaches as f64 / samples as f64
        };
        metrics::gauge!("onwards_provider_share", "model" => model.clone(), "pool" => pool.clone())
            .set(self.share);
        metrics::gauge!("onwards_aimd_active", "model" => model.clone(), "pool" => pool.clone())
            .set(if self.active() { 1.0 } else { 0.0 });
        metrics::gauge!("onwards_aimd_window_samples", "model" => model.clone(), "pool" => pool.clone())
            .set(samples as f64);
        metrics::gauge!("onwards_aimd_window_breach_rate", "model" => model.clone(), "pool" => pool.clone())
            .set(rate);
        metrics::gauge!("onwards_aimd_in_flight", "model" => model.clone(), "pool" => pool.clone())
            .set(self.in_flight as f64);
    }
}

/// A single eligible preferred attempt. Clones share completion state so a
/// timeout, a frame, a status and dropping the response cannot count the
/// attempt twice.
#[derive(Clone)]
pub(crate) struct Observation(Arc<Mutex<Attempt>>);
struct Attempt {
    controller: Arc<Mutex<Controller>>,
    generation: u64,
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
        let generation = {
            let mut guard = controller.lock().unwrap();
            guard.remember_labels(model, pool);
            guard.begin()?
        };
        Some(Self(Arc::new(Mutex::new(Attempt {
            controller,
            generation,
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
    /// The preferred provider answered with an error status, either as the
    /// HTTP status or embedded in a 2xx stream. Configured overload statuses
    /// are breaches; any other error is unknown.
    pub(crate) fn status(&self, status: u16) {
        let mut attempt = self.0.lock().unwrap();
        let overload = attempt
            .controller
            .lock()
            .unwrap()
            .config
            .overload_statuses
            .contains(&status);
        if overload && !attempt.completed {
            metrics::counter!(
                "onwards_aimd_overload_breaches_total",
                "model" => attempt.model.clone(),
                "pool" => attempt.pool.clone(),
                "status" => status.to_string(),
            )
            .increment(1);
        }
        attempt.finish(if overload {
            Outcome::Breach
        } else {
            Outcome::Unknown
        });
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
        let was_active = controller.active();
        let direction = controller.finish(self.generation, outcome, Instant::now());
        if !was_active {
            return;
        }
        if outcome == Outcome::Unknown {
            metrics::counter!("onwards_aimd_unknown_total", "model" => self.model.clone(), "pool" => self.pool.clone())
                .increment(1);
        }
        if let Some(direction) = direction {
            metrics::counter!("onwards_share_adjustments_total", "model" => self.model.clone(), "pool" => self.pool.clone(), "direction" => direction).increment(1);
        }
        controller.publish();
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
            recovery_breach_rate: 0.1,
            window_samples: 10,
            min_samples: 5,
            share_step: 0.1,
            share_decay: 0.5,
            share_floor: 0.1,
            dwell_ms: 1000,
            idle_recovery_ms: 10_000,
            overload_statuses: vec![429, 503, 529],
        }
    }

    fn sample(c: &mut Controller, outcome: Outcome, now: Instant) -> Option<&'static str> {
        let generation = c.begin().unwrap();
        c.finish(generation, outcome, now)
    }

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn retired_controller_rejects_pending_and_new_observations() {
        let start = Instant::now();
        let mut c = Controller::new(config(), start);
        let generation = c.begin().unwrap();
        c.retire();
        assert_eq!(c.finish(generation, Outcome::Breach, at(start, 5000)), None);
        assert_eq!(c.breaches, 0);
        assert_eq!(c.in_flight, 0);
        assert_eq!(c.begin(), None);
        assert!(!c.resume(), "a retired controller never resumes");
    }

    #[test]
    fn unknown_outcomes_are_excluded_rather_than_vetoing_decisions() {
        let start = Instant::now();
        let mut c = Controller::new(config(), start);
        sample(&mut c, Outcome::Unknown, at(start, 2000));
        for _ in 0..4 {
            sample(&mut c, Outcome::Breach, at(start, 2000));
        }
        // Four completed samples: the unknown did not count towards min_samples.
        assert_eq!(c.window.len(), 4);
        assert!(close(c.share(), 1.0));
        assert_eq!(
            sample(&mut c, Outcome::Breach, at(start, 2000)),
            Some("decrease")
        );
        assert!(close(c.share(), 0.5));
    }

    #[test]
    fn in_flight_attempts_never_hold_a_decision_back() {
        let start = Instant::now();
        let mut c = Controller::new(config(), start);
        let _still_running: Vec<_> = (0..20).map(|_| c.begin().unwrap()).collect();
        for _ in 0..5 {
            sample(&mut c, Outcome::Breach, at(start, 2000));
        }
        assert!(close(c.share(), 0.5));
        assert_eq!(c.in_flight, 20);
    }

    #[test]
    fn a_full_window_keeps_admitting_samples_while_attempts_are_pending() {
        let start = Instant::now();
        let mut c = Controller::new(config(), start);
        let _pending = c.begin().unwrap();
        for _ in 0..25 {
            sample(&mut c, Outcome::Healthy, at(start, 500));
        }
        assert_eq!(c.window.len(), 10, "bounded by window_samples");
        assert!(c.begin().is_some(), "never refuses a new sample");
    }

    #[test]
    fn a_breach_rate_inside_the_hysteresis_band_holds_the_share() {
        let start = Instant::now();
        let mut c = Controller::new(config(), start);
        for _ in 0..5 {
            sample(&mut c, Outcome::Breach, at(start, 2000));
        }
        assert!(close(c.share(), 0.5));
        // One breach in every 5 samples keeps every window of 5..=10 samples
        // above recovery (10%) and at or below the decrease target (20%).
        // Hold, however long it lasts.
        for index in 0..60u64 {
            let outcome = if index % 5 == 4 {
                Outcome::Breach
            } else {
                Outcome::Healthy
            };
            sample(&mut c, outcome, at(start, 4000 + index * 1000));
        }
        assert!(close(c.share(), 0.5), "share {}", c.share());
    }

    #[test]
    fn a_decrease_discards_samples_that_started_at_the_old_share() {
        let start = Instant::now();
        let mut c = Controller::new(config(), start);
        let old = c.begin().unwrap();
        for _ in 0..5 {
            sample(&mut c, Outcome::Breach, at(start, 2000));
        }
        assert!(close(c.share(), 0.5));
        assert_eq!(c.finish(old, Outcome::Breach, at(start, 2100)), None);
        assert_eq!(c.window.len(), 0);
        assert_eq!(c.in_flight, 0);
    }

    #[test]
    fn repeated_decreases_respect_dwell_and_the_floor() {
        let start = Instant::now();
        let mut c = Controller::new(config(), start);
        for _ in 0..5 {
            sample(&mut c, Outcome::Breach, at(start, 2000));
        }
        assert!(close(c.share(), 0.5));
        for _ in 0..5 {
            sample(&mut c, Outcome::Breach, at(start, 2500));
        }
        assert!(close(c.share(), 0.5), "within dwell");
        sample(&mut c, Outcome::Breach, at(start, 3100));
        assert!(close(c.share(), 0.25));
        let mut now = 3100;
        for _ in 0..20 {
            now += 1100;
            for _ in 0..5 {
                sample(&mut c, Outcome::Breach, at(start, now));
            }
        }
        assert!(close(c.share(), 0.1));
        assert_eq!(sample(&mut c, Outcome::Breach, at(start, now + 5000)), None);
    }

    #[test]
    fn recovery_keeps_the_window_so_each_step_needs_only_a_dwell() {
        let start = Instant::now();
        let mut c = Controller::new(config(), start);
        for _ in 0..5 {
            sample(&mut c, Outcome::Breach, at(start, 2000));
        }
        assert!(close(c.share(), 0.5));
        // Healthy samples: the first evaluation starts the healthy dwell; the
        // increase lands once both the dwell and the adjustment interval pass.
        for _ in 0..5 {
            sample(&mut c, Outcome::Healthy, at(start, 3000));
        }
        assert!(close(c.share(), 0.5));
        assert_eq!(
            sample(&mut c, Outcome::Healthy, at(start, 4000)),
            Some("increase")
        );
        assert!(close(c.share(), 0.6));
        assert_eq!(c.window.len(), 6, "an increase keeps the window");
        // One more healthy sample per dwell is enough for the next step.
        assert_eq!(
            sample(&mut c, Outcome::Healthy, at(start, 5000)),
            Some("increase")
        );
        assert!(close(c.share(), 0.7));
    }

    #[test]
    fn idle_recovery_credits_elapsed_intervals_without_enough_samples() {
        let start = Instant::now();
        let mut c = Controller::new(config(), start);
        for _ in 0..5 {
            sample(&mut c, Outcome::Breach, at(start, 2000));
        }
        assert!(close(c.share(), 0.5));
        assert_eq!(c.tick(at(start, 11_999)), None);
        assert_eq!(c.tick(at(start, 12_000)), Some("increase"));
        assert!(close(c.share(), 0.6));
        // Two more whole intervals elapse: two steps, and the cadence is kept.
        assert_eq!(c.tick(at(start, 32_500)), Some("increase"));
        assert!(close(c.share(), 0.8));
        assert_eq!(c.last_adjustment, at(start, 32_000));
        // With enough samples to judge, latency evidence decides instead.
        for _ in 0..5 {
            sample(&mut c, Outcome::Breach, at(start, 33_000));
        }
        assert!(close(c.share(), 0.4));
        for _ in 0..5 {
            sample(&mut c, Outcome::Breach, at(start, 33_500));
        }
        assert_eq!(c.tick(at(start, 90_000)), None);

        let mut disabled = config();
        disabled.idle_recovery_ms = 0;
        let mut c = Controller::new(disabled, start);
        for _ in 0..5 {
            sample(&mut c, Outcome::Breach, at(start, 2000));
        }
        assert_eq!(c.tick(at(start, 10_000_000)), None);
    }

    #[test]
    fn parked_controllers_keep_their_share_and_resume_fresh() {
        let start = Instant::now();
        let mut c = Controller::new(config(), start);
        for _ in 0..5 {
            sample(&mut c, Outcome::Breach, at(start, 2000));
        }
        let in_flight = c.begin().unwrap();
        c.park();
        assert!(!c.active());
        assert_eq!(c.begin(), None);
        assert!(c.resume());
        assert!(c.active());
        assert!(close(c.share(), 0.5));
        assert_eq!(c.finish(in_flight, Outcome::Breach, at(start, 3000)), None);
        assert!(!c.resume(), "resume is a no-op for an active controller");
    }

    #[test]
    fn default_thresholds_ignore_noise_at_the_old_target_and_act_on_real_overload() {
        for (every, expect_decrease) in [(20u64, false), (5, true)] {
            let start = Instant::now();
            let mut c = Controller::new(AimdConfig::default(), start);
            for index in 0..300u64 {
                let outcome = if index % every == 0 {
                    Outcome::Breach
                } else {
                    Outcome::Healthy
                };
                sample(&mut c, outcome, at(start, (index + 1) * 1000));
            }
            assert_eq!(
                c.share() < 1.0,
                expect_decrease,
                "1 in {every}: {}",
                c.share()
            );
        }
    }

    #[test]
    fn load_dependent_latency_converges_and_recovers_after_capacity_step() {
        let start = Instant::now();
        let mut c = Controller::new(config(), start);
        let mut now = 0;
        for _ in 0..400 {
            now += 200;
            // Deterministic service with capacity for 60% of offered load.
            let outcome = if c.share() > 0.6 {
                Outcome::Breach
            } else {
                Outcome::Healthy
            };
            sample(&mut c, outcome, at(start, now));
        }
        assert!((0.25..=0.7).contains(&c.share()), "share {}", c.share());
        for _ in 0..200 {
            now += 200;
            sample(&mut c, Outcome::Healthy, at(start, now));
        }
        assert!(close(c.share(), 1.0));
        for _ in 0..200 {
            now += 200;
            sample(&mut c, Outcome::Breach, at(start, now));
        }
        assert!(close(c.share(), c.config.share_floor));
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
        assert_eq!(controller.lock().unwrap().window.len(), 0);
        let late =
            Observation::new(controller.clone(), "model", "default", Instant::now()).unwrap();
        tokio::time::advance(Duration::from_secs(10)).await;
        late.deadline();
        late.frame();
        late.status(529);
        late.unknown();
        drop(late);
        drop(early);
        let c = controller.lock().unwrap();
        assert_eq!(c.window.len(), 1);
        assert_eq!(c.breaches, 1);
        assert_eq!(c.in_flight, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn overload_statuses_are_breaches_and_other_errors_are_unknown() {
        let controller = Arc::new(Mutex::new(Controller::new(config(), Instant::now())));
        for status in [529, 429, 503] {
            Observation::new(controller.clone(), "model", "default", Instant::now())
                .unwrap()
                .status(status);
        }
        for status in [500, 404] {
            Observation::new(controller.clone(), "model", "default", Instant::now())
                .unwrap()
                .status(status);
        }
        let c = controller.lock().unwrap();
        assert_eq!(c.window.len(), 3);
        assert_eq!(c.breaches, 3);
    }

    #[test]
    fn validates_bounds() {
        assert!(config().validate().is_ok());
        assert!(AimdConfig::default().validate().is_ok());
        let mut c = config();
        c.min_samples = 1;
        assert!(c.validate().is_err());
        let mut c = config();
        c.share_decay = f64::NAN;
        assert!(c.validate().is_err());
        let mut c = config();
        c.share_floor = 0.0;
        assert!(c.validate().is_err());
        let mut c = config();
        c.recovery_breach_rate = c.breach_rate_target + 0.01;
        assert!(c.validate().is_err());
        let mut c = config();
        c.overload_statuses = vec![200];
        assert!(c.validate().is_err());
        let mut c = config();
        c.idle_recovery_ms = 86_400_001;
        assert!(c.validate().is_err());
    }

    #[test]
    fn older_overrides_without_new_fields_still_parse() {
        let parsed: AimdConfig = serde_json::from_value(serde_json::json!({
            "enabled": true, "latency_budget_ms": 100, "breach_rate_target": 0.1,
            "window_samples": 20, "min_samples": 5, "share_step": 0.1,
            "share_decay": 0.5, "share_floor": 0.1, "dwell_ms": 1000
        }))
        .unwrap();
        assert_eq!(parsed.overload_statuses, vec![429, 503, 529]);
        assert!(parsed.validate().is_ok());
    }
}
