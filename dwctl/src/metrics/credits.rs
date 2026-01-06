//! Credit balance metrics for Prometheus.

use once_cell::sync::Lazy;
use prometheus::{Histogram, IntCounter, IntCounterVec, register_histogram, register_int_counter, register_int_counter_vec};

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

/// Histogram for analytics processing lag (time between request and analytics storage)
/// Buckets: 100ms, 500ms, 1s, 5s, 10s, 30s, 60s, 120s, 300s, 600s
static ANALYTICS_LAG_SECONDS: Lazy<Histogram> = Lazy::new(|| {
    register_histogram!(
        "genai_analytics_lag_seconds",
        "Time between request timestamp and analytics processing (seconds)",
        vec![0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0]
    )
    .expect("Failed to register genai_analytics_lag_seconds metric")
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

/// Record the lag between request timestamp and analytics processing
pub fn record_analytics_lag(lag_seconds: f64) {
    ANALYTICS_LAG_SECONDS.observe(lag_seconds);
}
