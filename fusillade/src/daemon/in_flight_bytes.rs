//! Byte-based budget for claimed-but-unfinished work.
//!
//! The memory gate measures the process's actual working set and suppresses
//! claiming above its high mark. Measuring has a structural limit: claims
//! already dispatched cannot be retracted, so everything in flight when the
//! gate engages keeps accruing memory into the headroom between the high mark
//! and the limit. A burst that claims tens of thousands of rows in the seconds
//! around engagement can outgrow that headroom and get the pod OOM-killed
//! anyway - on curie's request daemon (2026-09-10) the working-set ratio was
//! 0.92 of the limit while the gate was engaged, and the pod died at 1.0.
//!
//! This budget bounds what the daemon admits instead of what it currently
//! holds. Each claim is charged:
//!
//! - its request bytes (known at claim time), plus
//! - an estimate of its response bytes: a per-model running average of the
//!   response sizes this workload has actually produced, falling back to
//!   `memory_budget_default_response_bytes` until a model has completions.
//!
//! Once the committed total reaches the budget, claiming stops, exactly like
//! the memory gate; completions release their charges and the budget reopens.
//! Because the estimate is learned from the workload's own completions, the
//! in-flight count the budget permits self-corrects across workloads instead
//! of needing a per-model `batch_capacity` guess.
//!
//! The estimate is charged at claim time and never corrected mid-flight, so
//! committed bytes track charges rather than true usage. That is deliberate:
//! the reassembled response is built below this layer (dwctl reassembles
//! streams; fusillade sees the finished body), so per-chunk accrual is not
//! visible here. The budget's job is to keep the burst small enough that the
//! gate's headroom survives whatever is still in flight, and the memory gate
//! remains the measured backstop for everything the estimate gets wrong.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use dashmap::DashMap;
use metrics::{counter, gauge};

/// Weight of each new observation in a model's response estimate. A quarter
/// adapts within a handful of completions while keeping one enormous response
/// from pinning the estimate for long.
const RESPONSE_ESTIMATE_NUM: i64 = 1;
const RESPONSE_ESTIMATE_DEN: i64 = 4;

pub(super) struct InFlightBytesAccounting {
    /// Budget in bytes, or `None` when disabled by config or when the process
    /// has no readable cgroup limit.
    budget: Option<u64>,
    /// Sum of outstanding charges: request bytes plus the response estimate
    /// each claim was charged with. Released as requests reach terminal states.
    committed: AtomicU64,
    default_response_bytes: u64,
    response_estimates: DashMap<String, AtomicU64>,
    /// Whether the budget is currently blocking claims, so the exhaustion
    /// counter counts engagement episodes the way the memory gate counts
    /// engagements, rather than every claim cycle while exhausted.
    engaged: AtomicBool,
}

impl InFlightBytesAccounting {
    pub(super) fn new(budget: Option<u64>, default_response_bytes: u64) -> Self {
        let accounting = Self {
            budget,
            committed: AtomicU64::new(0),
            default_response_bytes,
            response_estimates: DashMap::new(),
            engaged: AtomicBool::new(false),
        };
        if let Some(budget) = budget {
            gauge!("fusillade_in_flight_bytes_budget").set(budget as f64);
        }
        accounting
    }

    /// Bytes to charge for claiming this request: its request body plus the
    /// model's current response estimate. The charge is added to the committed
    /// total immediately and returned; pass the return value to
    /// [`release`](Self::release) when the request reaches a terminal state.
    ///
    /// Zero when disabled, so callers can charge unconditionally.
    pub(super) fn charge(&self, model: &str, request_bytes: usize) -> u64 {
        let Some(_) = self.budget else {
            return 0;
        };
        let charge = request_bytes as u64 + self.response_estimate(model);
        self.committed.fetch_add(charge, Ordering::Relaxed);
        charge
    }

    /// Release a charge once its request has reached a terminal state.
    ///
    /// Must run even when the task is aborted or panics; callers hold the
    /// charge in a drop guard for exactly that reason.
    pub(super) fn release(&self, charge: u64) {
        if charge == 0 {
            return;
        }
        self.committed.fetch_sub(charge, Ordering::Relaxed);
    }

    /// Fold an observed response into its model's estimate. A model's first
    /// observation becomes the estimate directly; later ones pull it toward
    /// the observation by the configured weight.
    ///
    /// Counts every completed attempt, retries included: each attempt is what
    /// the daemon actually paid memory for. Zero-byte outcomes are skipped:
    /// they are transport failures and cancellations, not evidence that this
    /// model's responses are empty, and folding them in would loosen the
    /// budget exactly when failures pile up.
    pub(super) fn record_response(&self, model: &str, response_bytes: usize) {
        if self.budget.is_none() || response_bytes == 0 {
            return;
        }
        let observed = response_bytes as i64;
        let estimate = self
            .response_estimates
            .entry(model.to_owned())
            .or_insert_with(|| AtomicU64::new(observed.max(0) as u64));
        let previous = estimate.load(Ordering::Relaxed) as i64;
        estimate.store(blend(previous, observed).max(0) as u64, Ordering::Relaxed);
    }

    /// The model's current response estimate.
    fn response_estimate(&self, model: &str) -> u64 {
        self.response_estimates
            .get(model)
            .map(|e| e.load(Ordering::Relaxed))
            .unwrap_or(self.default_response_bytes)
    }

    /// Whether claiming should be suppressed this cycle.
    ///
    /// No hysteresis: committed bytes fall only as charged work completes, so
    /// unlike the gate's measured usage this quantity cannot hover on the
    /// boundary. The residual flap is one claim batch wide - the same
    /// quantization the gate lives with.
    pub(super) fn blocks_claiming(&self) -> bool {
        let Some(budget) = self.budget else {
            return false;
        };
        let committed = self.committed.load(Ordering::Relaxed);
        gauge!("fusillade_in_flight_bytes").set(committed as f64);
        let was_engaged = self.engaged.load(Ordering::Relaxed);
        let blocked = committed >= budget;
        gauge!("fusillade_memory_budget_blocks_claiming").set(u8::from(blocked));
        if blocked && !was_engaged {
            counter!("fusillade_memory_budget_exhaustions_total").increment(1);
            tracing::warn!(
                committed_bytes = committed,
                budget_bytes = budget,
                "In-flight byte budget exhausted; suspending claims until work completes"
            );
        }
        self.engaged.store(blocked, Ordering::Relaxed);
        blocked
    }
}

/// Blend one new observation into an existing estimate.
fn blend(current: i64, observed: i64) -> i64 {
    let delta = observed - current;
    current + delta * RESPONSE_ESTIMATE_NUM / RESPONSE_ESTIMATE_DEN
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accounting(budget: Option<u64>) -> InFlightBytesAccounting {
        InFlightBytesAccounting::new(budget, 1000)
    }

    #[test]
    fn charges_request_bytes_plus_the_models_response_estimate() {
        let a = accounting(Some(10_000));
        assert_eq!(
            a.charge("m", 500),
            1500,
            "no observations yet: default estimate"
        );
        assert_eq!(a.committed.load(Ordering::Relaxed), 1500);
    }

    #[test]
    fn releasing_returns_the_exact_charge() {
        let a = accounting(Some(10_000));
        let first = a.charge("m", 500);
        let second = a.charge("m", 500);
        a.release(first);
        assert_eq!(a.committed.load(Ordering::Relaxed), second);
        a.release(second);
        assert_eq!(
            a.committed.load(Ordering::Relaxed),
            0,
            "charges and releases balance"
        );
    }

    #[test]
    fn a_disabled_budget_never_charges_or_blocks() {
        let a = accounting(None);
        assert_eq!(a.charge("m", 500), 0);
        assert_eq!(a.committed.load(Ordering::Relaxed), 0);
        assert!(!a.blocks_claiming());
        // Recording is a no-op too, so the estimate map stays empty.
        a.record_response("m", 999);
        assert_eq!(a.response_estimate("m"), 1000);
    }

    #[test]
    fn blocks_once_committed_reaches_the_budget_and_reopens_below_it() {
        // Each charge is the request bytes plus the model's response estimate
        // (1000 here, from the helper) - 2000 per request.
        let a = accounting(Some(4000));
        assert!(!a.blocks_claiming(), "empty ledger does not block");
        let first = a.charge("m", 1000);
        assert!(!a.blocks_claiming());
        a.charge("m", 1000);
        assert!(a.blocks_claiming(), "committed 4000 of 4000 blocks");
        a.release(first);
        assert!(!a.blocks_claiming(), "committed 2000 of 4000 claims again");
    }

    #[test]
    fn responses_are_blended_into_the_estimate() {
        let a = accounting(Some(1_000_000));
        a.record_response("m", 1000);
        assert_eq!(
            a.response_estimate("m"),
            1000,
            "first observation sets the estimate"
        );
        a.record_response("m", 5000);
        assert_eq!(
            a.response_estimate("m"),
            1000 + (5000 - 1000) / 4,
            "later observations pull a quarter of the way"
        );
    }

    #[test]
    fn estimates_are_per_model() {
        let a = accounting(Some(1_000_000));
        a.record_response("big", 1_000_000);
        assert_eq!(
            a.response_estimate("small"),
            1000,
            "unrelated model keeps its default"
        );
        assert_eq!(a.response_estimate("big"), 1_000_000);
    }

    #[test]
    fn a_response_larger_than_the_estimate_pulls_the_estimate_up() {
        let a = accounting(Some(10_000_000));
        a.record_response("m", 10_000);
        a.record_response("m", 1_000_000);
        let estimate = a.response_estimate("m") as i64;
        assert!(
            estimate > 10_000 && estimate < 1_000_000,
            "estimate moved from 10000 toward 1000000, got {estimate}"
        );
    }

    #[test]
    fn blend_moves_toward_the_observation_and_never_goes_negative() {
        assert_eq!(blend(100, 0), 75);
        assert_eq!(blend(0, 1000), 250);
        // Integer division truncates toward zero, so a small estimate sticks
        // rather than undershooting below zero.
        assert_eq!(blend(1, 0), 1);
        assert_eq!(blend(3, 0), 3);
    }

    #[test]
    fn zero_byte_outcomes_do_not_loosen_the_estimate() {
        // Transport failures and cancellations report 0 bytes; folding those
        // in would drag the estimate down and admit more work during exactly
        // the failures the budget exists to survive.
        let a = accounting(Some(1_000_000));
        a.record_response("m", 10_000);
        a.record_response("m", 0);
        a.record_response("m", 0);
        assert_eq!(a.response_estimate("m"), 10_000);
    }
}
