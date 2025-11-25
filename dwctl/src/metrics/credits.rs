//! Credit balance metrics for Prometheus.

use once_cell::sync::Lazy;
use prometheus::{IntCounter, IntCounterVec, register_int_counter, register_int_counter_vec};

/// Counter for successful credit deductions
static CREDITS_DEDUCTED: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "genai_credits_deducted_total",
        "Total credits deducted for API usage (in cents)",
        &["user_id", "model"]
    )
    .expect("Failed to register genai_credits_deducted_total metric")
});

/// Counter for failed credit deductions
static CREDITS_DEDUCTION_ERRORS: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("genai_credits_deduction_errors_total", "Total credit deduction errors")
        .expect("Failed to register genai_credits_deduction_errors_total metric")
});

/// Record a successful credit deduction
///
/// # Arguments
/// * `user_id` - User ID as string
/// * `model` - Model name used
/// * `amount` - Amount deducted in dollars (will be converted to cents for counter)
pub fn record_credit_deduction(user_id: &str, model: &str, amount: f64) {
    // Convert dollars to cents for the counter (easier to work with integers)
    let cents = (amount * 100.0).round() as i64;
    // Ensure non-negative
    CREDITS_DEDUCTED.with_label_values(&[user_id, model]).inc_by(cents.max(0) as u64);
}

/// Record a failed credit deduction
pub fn record_credit_deduction_error() {
    CREDITS_DEDUCTION_ERRORS.inc();
}
