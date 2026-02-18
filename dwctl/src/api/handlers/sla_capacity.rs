//! Completion window capacity checking for batch creation.
//!
//! This module provides functions to check whether a new batch can be accepted
//! within the requested completion window based on model throughput and pending work.

use std::collections::HashMap;
use tracing::info;

/// Maximum window size in seconds to prevent overflow (roughly 100 years)
const MAX_WINDOW_SECONDS: i64 = 3_153_600_000;

/// Result of a completion window capacity check
#[derive(Debug)]
pub struct SlaCapacityCheckResult {
    /// Whether there is sufficient capacity for the batch
    pub has_capacity: bool,
    /// Models that exceed capacity (model_alias -> deficit in requests)
    pub overloaded_models: HashMap<String, i64>,
}

/// Check if a batch can be accepted within the requested completion window.
///
/// This is a simple check: for each model in the batch, verify that
/// `(pending_requests + new_requests) <= throughput * window_seconds`
///
/// # Note on Concurrency for V1
/// This capacity check does not use locking, so concurrent batch creations may
/// both pass the check and then exceed the actual capacity. This is a known
/// limitation that provides "best effort" protection - it will reject obvious
/// overflows but may allow slight over-acceptance during concurrent bursts.
/// This trade-off is intentional for Phase 1 to avoid complexity; proper
/// concurrency control (e.g., advisory locks) will be added in future iterations.
///
/// # Arguments
/// * `file_model_counts` - Map of model alias to request count in the new batch
/// * `pending_counts` - Map of model alias -> window -> pending request count
/// * `model_throughputs` - Map of model alias to throughput (req/s)
/// * `default_throughput` - Default throughput for models not in `model_throughputs`
/// * `completion_window` - The completion window (e.g., "24h", "1h")
///
/// # Returns
/// `SlaCapacityCheckResult` indicating whether there's capacity and which models are overloaded
pub fn check_sla_capacity(
    file_model_counts: &HashMap<String, i64>,
    pending_counts: &HashMap<String, HashMap<String, i64>>,
    model_throughputs: &HashMap<String, f32>,
    default_throughput: f32,
    completion_window: &str,
) -> SlaCapacityCheckResult {
    let window_seconds = parse_window_to_seconds(completion_window);
    let mut overloaded_models = HashMap::new();

    for (model_alias, &new_requests) in file_model_counts {
        // Get pending count for this model and window
        let pending = pending_counts
            .get(model_alias)
            .and_then(|windows| windows.get(completion_window))
            .copied()
            .unwrap_or(0);

        // Get throughput for this model (or default)
        // Treat non-positive throughput as effectively zero capacity
        let throughput = model_throughputs.get(model_alias).copied().unwrap_or(default_throughput).max(0.0); // Clamp to non-negative

        // Calculate capacity using f64 throughout to avoid overflow,
        // then clamp to i64 range for final comparison
        let capacity_f64 = (throughput as f64) * (window_seconds as f64);
        let capacity = if capacity_f64 >= i64::MAX as f64 {
            i64::MAX
        } else if capacity_f64 <= 0.0 {
            0
        } else {
            capacity_f64 as i64
        };

        // Check if we exceed capacity
        let total_requests = pending + new_requests;
        if total_requests > capacity {
            let deficit = total_requests - capacity;
            info!(
                model = %model_alias,
                pending = pending,
                new_requests = new_requests,
                capacity = capacity,
                throughput = throughput,
                window = completion_window,
                deficit = deficit,
                "Model exceeds completion window capacity"
            );
            overloaded_models.insert(model_alias.clone(), deficit);
        }
    }

    SlaCapacityCheckResult {
        has_capacity: overloaded_models.is_empty(),
        overloaded_models,
    }
}

/// Parse a completion window string (e.g., "24h", "1h") to seconds.
///
/// Returns the window duration in seconds. Invalid or negative values
/// default to 24 hours (86400 seconds). Very large values are clamped
/// to MAX_WINDOW_SECONDS to prevent overflow in capacity calculations.
pub fn parse_window_to_seconds(window: &str) -> i64 {
    let parsed = if window.ends_with('h') {
        window.trim_end_matches('h').parse::<i64>().ok().map(|h| h * 3600)
    } else if window.ends_with('m') {
        window.trim_end_matches('m').parse::<i64>().ok().map(|m| m * 60)
    } else if window.ends_with('s') {
        window.trim_end_matches('s').parse::<i64>().ok()
    } else {
        None
    };

    match parsed {
        // Reject negative or zero values, default to 24h
        Some(secs) if secs <= 0 => {
            tracing::warn!(
                window = %window,
                "Invalid non-positive window value, defaulting to 24h"
            );
            86400
        }
        // Clamp very large values to prevent overflow
        Some(secs) if secs > MAX_WINDOW_SECONDS => {
            tracing::warn!(
                window = %window,
                max = MAX_WINDOW_SECONDS,
                "Window value too large, clamping to maximum"
            );
            MAX_WINDOW_SECONDS
        }
        Some(secs) => secs,
        // Default to 24 hours if parsing fails
        None => {
            tracing::warn!(
                window = %window,
                "Failed to parse window, defaulting to 24h"
            );
            86400
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== parse_window_to_seconds tests ====================

    #[test]
    fn test_parse_window_hours() {
        assert_eq!(parse_window_to_seconds("1h"), 3600);
        assert_eq!(parse_window_to_seconds("24h"), 86400);
        assert_eq!(parse_window_to_seconds("48h"), 172800);
        assert_eq!(parse_window_to_seconds("168h"), 604800); // 1 week
    }

    #[test]
    fn test_parse_window_minutes() {
        assert_eq!(parse_window_to_seconds("1m"), 60);
        assert_eq!(parse_window_to_seconds("30m"), 1800);
        assert_eq!(parse_window_to_seconds("60m"), 3600);
        assert_eq!(parse_window_to_seconds("90m"), 5400);
    }

    #[test]
    fn test_parse_window_seconds() {
        assert_eq!(parse_window_to_seconds("1s"), 1);
        assert_eq!(parse_window_to_seconds("60s"), 60);
        assert_eq!(parse_window_to_seconds("3600s"), 3600);
    }

    #[test]
    fn test_parse_window_invalid_defaults_to_24h() {
        assert_eq!(parse_window_to_seconds("invalid"), 86400);
        assert_eq!(parse_window_to_seconds(""), 86400);
        assert_eq!(parse_window_to_seconds("abc"), 86400);
        assert_eq!(parse_window_to_seconds("24"), 86400); // missing unit
        assert_eq!(parse_window_to_seconds("h24"), 86400); // wrong order
    }

    #[test]
    fn test_parse_window_zero_defaults_to_24h() {
        // Zero values should default to 24h (not be treated as valid)
        assert_eq!(parse_window_to_seconds("0h"), 86400);
        assert_eq!(parse_window_to_seconds("0m"), 86400);
        assert_eq!(parse_window_to_seconds("0s"), 86400);
    }

    #[test]
    fn test_parse_window_negative_defaults_to_24h() {
        // Negative values should default to 24h
        assert_eq!(parse_window_to_seconds("-1h"), 86400);
        assert_eq!(parse_window_to_seconds("-24h"), 86400);
        assert_eq!(parse_window_to_seconds("-30m"), 86400);
        assert_eq!(parse_window_to_seconds("-60s"), 86400);
    }

    #[test]
    fn test_parse_window_very_large_clamped() {
        // Very large values should be clamped to MAX_WINDOW_SECONDS
        assert_eq!(parse_window_to_seconds("999999999999h"), MAX_WINDOW_SECONDS);
        assert_eq!(parse_window_to_seconds("9999999999999999s"), MAX_WINDOW_SECONDS);
    }

    // ==================== Basic capacity check tests ====================

    #[test]
    fn test_capacity_check_empty_batch() {
        let file_model_counts = HashMap::new();
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::new();

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        assert!(result.has_capacity);
        assert!(result.overloaded_models.is_empty());
    }

    #[test]
    fn test_capacity_check_single_model_within_limits() {
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 1000)]);
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("24h".to_string(), 5000)]))]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]); // 1 req/s = 86400/day

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        assert!(result.has_capacity);
        assert!(result.overloaded_models.is_empty());
    }

    #[test]
    fn test_capacity_check_single_model_exceeds_limits() {
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 50000)]);
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("24h".to_string(), 50000)]))]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]); // 1 req/s = 86400/day

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        assert!(!result.has_capacity);
        assert!(result.overloaded_models.contains_key("gpt-4"));
        // 50000 + 50000 = 100000, capacity = 86400, deficit = 13600
        assert_eq!(result.overloaded_models.get("gpt-4"), Some(&13600));
    }

    #[test]
    fn test_capacity_check_exactly_at_limit() {
        // Throughput of 1 req/s for 24h = 86400 capacity
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 40000)]);
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("24h".to_string(), 46400)]))]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 40000 + 46400 = 86400 = capacity, should pass
        assert!(result.has_capacity);
        assert!(result.overloaded_models.is_empty());
    }

    #[test]
    fn test_capacity_check_one_over_limit() {
        // Throughput of 1 req/s for 24h = 86400 capacity
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 40001)]);
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("24h".to_string(), 46400)]))]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 40001 + 46400 = 86401 > 86400, should fail
        assert!(!result.has_capacity);
        assert_eq!(result.overloaded_models.get("gpt-4"), Some(&1));
    }

    // ==================== Multiple models tests ====================

    #[test]
    fn test_capacity_check_multiple_models_all_within_limits() {
        let file_model_counts = HashMap::from([
            ("gpt-4".to_string(), 10000),
            ("gpt-3.5".to_string(), 50000),
            ("claude".to_string(), 15000),
        ]);
        let pending_counts = HashMap::from([
            ("gpt-4".to_string(), HashMap::from([("24h".to_string(), 5000)])),
            ("gpt-3.5".to_string(), HashMap::from([("24h".to_string(), 10000)])),
            ("claude".to_string(), HashMap::from([("24h".to_string(), 5000)])),
        ]);
        let model_throughputs = HashMap::from([
            ("gpt-4".to_string(), 1.0),   // 86400 capacity
            ("gpt-3.5".to_string(), 2.0), // 172800 capacity
            ("claude".to_string(), 1.0),  // 86400 capacity
        ]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        assert!(result.has_capacity);
        assert!(result.overloaded_models.is_empty());
    }

    #[test]
    fn test_capacity_check_multiple_models_one_exceeds() {
        let file_model_counts = HashMap::from([
            ("gpt-4".to_string(), 10000),
            ("gpt-3.5".to_string(), 100000), // This will exceed
            ("claude".to_string(), 15000),
        ]);
        let pending_counts = HashMap::from([
            ("gpt-4".to_string(), HashMap::from([("24h".to_string(), 5000)])),
            ("gpt-3.5".to_string(), HashMap::from([("24h".to_string(), 100000)])),
            ("claude".to_string(), HashMap::from([("24h".to_string(), 5000)])),
        ]);
        let model_throughputs = HashMap::from([
            ("gpt-4".to_string(), 1.0),   // 86400 capacity
            ("gpt-3.5".to_string(), 2.0), // 172800 capacity
            ("claude".to_string(), 1.0),  // 86400 capacity
        ]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        assert!(!result.has_capacity);
        assert_eq!(result.overloaded_models.len(), 1);
        assert!(result.overloaded_models.contains_key("gpt-3.5"));
        // 100000 + 100000 = 200000, capacity = 172800, deficit = 27200
        assert_eq!(result.overloaded_models.get("gpt-3.5"), Some(&27200));
    }

    #[test]
    fn test_capacity_check_multiple_models_all_exceed() {
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 50000), ("gpt-3.5".to_string(), 100000)]);
        let pending_counts = HashMap::from([
            ("gpt-4".to_string(), HashMap::from([("24h".to_string(), 50000)])),
            ("gpt-3.5".to_string(), HashMap::from([("24h".to_string(), 100000)])),
        ]);
        let model_throughputs = HashMap::from([
            ("gpt-4".to_string(), 1.0),   // 86400 capacity
            ("gpt-3.5".to_string(), 1.0), // 86400 capacity
        ]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        assert!(!result.has_capacity);
        assert_eq!(result.overloaded_models.len(), 2);
        assert!(result.overloaded_models.contains_key("gpt-4"));
        assert!(result.overloaded_models.contains_key("gpt-3.5"));
    }

    // ==================== Default throughput tests ====================

    #[test]
    fn test_capacity_check_uses_default_throughput_for_unknown_model() {
        let file_model_counts = HashMap::from([("unknown-model".to_string(), 1000)]);
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::new(); // No throughput configured

        let result = check_sla_capacity(
            &file_model_counts,
            &pending_counts,
            &model_throughputs,
            1.0, // Default: 1 req/s = 86400 capacity
            "24h",
        );

        assert!(result.has_capacity); // 1000 < 86400
    }

    #[test]
    fn test_capacity_check_uses_default_throughput_exceeds() {
        let file_model_counts = HashMap::from([("unknown-model".to_string(), 100000)]);
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::new();

        let result = check_sla_capacity(
            &file_model_counts,
            &pending_counts,
            &model_throughputs,
            1.0, // Default: 1 req/s = 86400 capacity
            "24h",
        );

        assert!(!result.has_capacity);
        assert_eq!(result.overloaded_models.get("unknown-model"), Some(&13600)); // 100000 - 86400
    }

    #[test]
    fn test_capacity_check_mixed_known_and_unknown_models() {
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 10000), ("unknown-model".to_string(), 50000)]);
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::from([
            ("gpt-4".to_string(), 2.0), // 172800 capacity - plenty of room
        ]);

        let result = check_sla_capacity(
            &file_model_counts,
            &pending_counts,
            &model_throughputs,
            0.5, // Default: 0.5 req/s = 43200 capacity
            "24h",
        );

        assert!(!result.has_capacity);
        // gpt-4: 10000 < 172800, OK
        // unknown-model: 50000 > 43200, NOT OK
        assert_eq!(result.overloaded_models.len(), 1);
        assert_eq!(result.overloaded_models.get("unknown-model"), Some(&6800)); // 50000 - 43200
    }

    // ==================== Different completion window tests ====================

    #[test]
    fn test_capacity_check_1h_window() {
        // 1 req/s for 1h = 3600 capacity
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 2000)]);
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("1h".to_string(), 1000)]))]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "1h");

        // 2000 + 1000 = 3000 < 3600
        assert!(result.has_capacity);
    }

    #[test]
    fn test_capacity_check_1h_window_exceeds() {
        // 1 req/s for 1h = 3600 capacity
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 3000)]);
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("1h".to_string(), 1000)]))]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "1h");

        // 3000 + 1000 = 4000 > 3600
        assert!(!result.has_capacity);
        assert_eq!(result.overloaded_models.get("gpt-4"), Some(&400));
    }

    #[test]
    fn test_capacity_check_different_windows_same_model() {
        // Same model can have different pending counts for different windows
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 1000)]);
        let pending_counts = HashMap::from([(
            "gpt-4".to_string(),
            HashMap::from([
                ("1h".to_string(), 3000),   // High pending for 1h
                ("24h".to_string(), 10000), // Lower relative pending for 24h
            ]),
        )]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        // Check 1h window: 1000 + 3000 = 4000 > 3600, should fail
        let result_1h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "1h");
        assert!(!result_1h.has_capacity);

        // Check 24h window: 1000 + 10000 = 11000 < 86400, should pass
        let result_24h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");
        assert!(result_24h.has_capacity);
    }

    // ==================== Edge cases with pending counts ====================

    #[test]
    fn test_capacity_check_no_pending_for_model() {
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 1000)]);
        let pending_counts = HashMap::new(); // No pending at all
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 1000 + 0 = 1000 < 86400
        assert!(result.has_capacity);
    }

    #[test]
    fn test_capacity_check_pending_for_different_window() {
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 1000)]);
        let pending_counts = HashMap::from([
            ("gpt-4".to_string(), HashMap::from([("1h".to_string(), 50000)])), // Only 1h pending
        ]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        // Checking 24h window - no 24h pending exists, so treated as 0
        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 1000 + 0 = 1000 < 86400
        assert!(result.has_capacity);
    }

    #[test]
    fn test_capacity_check_pending_for_different_model() {
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 1000)]);
        let pending_counts = HashMap::from([
            ("gpt-3.5".to_string(), HashMap::from([("24h".to_string(), 50000)])), // Only gpt-3.5 pending
        ]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // gpt-4: 1000 + 0 = 1000 < 86400
        assert!(result.has_capacity);
    }

    // ==================== High throughput tests ====================

    #[test]
    fn test_capacity_check_high_throughput_model() {
        // 100 req/s for 24h = 8,640,000 capacity
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 5_000_000)]);
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 100.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 5,000,000 < 8,640,000
        assert!(result.has_capacity);
    }

    #[test]
    fn test_capacity_check_fractional_throughput() {
        // 0.5 req/s for 24h = 43200 capacity
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 40000)]);
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 0.5)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 40000 < 43200
        assert!(result.has_capacity);
    }

    #[test]
    fn test_capacity_check_fractional_throughput_exceeds() {
        // 0.5 req/s for 24h = 43200 capacity
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 50000)]);
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 0.5)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 50000 > 43200
        assert!(!result.has_capacity);
        assert_eq!(result.overloaded_models.get("gpt-4"), Some(&6800));
    }

    // ==================== Zero/edge value tests ====================

    #[test]
    fn test_capacity_check_zero_new_requests() {
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 0)]);
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("24h".to_string(), 50000)]))]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 0 + 50000 < 86400
        assert!(result.has_capacity);
    }

    #[test]
    fn test_capacity_check_zero_pending() {
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 50000)]);
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("24h".to_string(), 0)]))]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 50000 + 0 < 86400
        assert!(result.has_capacity);
    }

    #[test]
    fn test_capacity_check_zero_throughput() {
        // Zero throughput means zero capacity - all requests should be rejected
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 1)]);
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 0.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // capacity = 0, any requests exceed
        assert!(!result.has_capacity);
        assert_eq!(result.overloaded_models.get("gpt-4"), Some(&1));
    }

    #[test]
    fn test_capacity_check_negative_throughput_treated_as_zero() {
        // Negative throughput should be clamped to 0 (zero capacity)
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 1)]);
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::from([("gpt-4".to_string(), -5.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // capacity = 0 (clamped from negative), any requests exceed
        assert!(!result.has_capacity);
        assert_eq!(result.overloaded_models.get("gpt-4"), Some(&1));
    }

    #[test]
    fn test_capacity_check_negative_default_throughput_treated_as_zero() {
        // Negative default throughput should be clamped to 0
        let file_model_counts = HashMap::from([("unknown-model".to_string(), 1)]);
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::new();

        let result = check_sla_capacity(
            &file_model_counts,
            &pending_counts,
            &model_throughputs,
            -10.0, // Negative default
            "24h",
        );

        // capacity = 0 (clamped), any requests exceed
        assert!(!result.has_capacity);
    }

    #[test]
    fn test_capacity_check_very_small_throughput() {
        // 0.001 req/s for 24h = 86.4 capacity (rounds to 86)
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 50)]);
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 0.001)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 50 < 86
        assert!(result.has_capacity);
    }

    #[test]
    fn test_capacity_check_very_large_throughput_no_overflow() {
        // Very large throughput should not cause overflow
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 1_000_000_000)]);
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1_000_000.0)]); // 1M req/s

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 1M req/s * 86400s = 86.4 billion capacity, should not overflow
        // 1 billion < 86.4 billion
        assert!(result.has_capacity);
    }

    // ==================== Composite model scenarios ====================

    #[test]
    fn test_capacity_check_composite_model_as_sum_of_components() {
        // Composite models are treated as their own model for capacity purposes
        let file_model_counts = HashMap::from([("gpt-4-composite".to_string(), 10000)]);
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::from([("gpt-4-composite".to_string(), 1.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        assert!(result.has_capacity);
    }

    // ==================== Real-world scenario tests ====================

    #[test]
    fn test_capacity_check_realistic_production_scenario() {
        // Production scenario: multiple models, varying throughputs, mixed pending states
        let file_model_counts = HashMap::from([
            ("gpt-4-turbo".to_string(), 50000),
            ("gpt-3.5-turbo".to_string(), 200000),
            ("claude-3-sonnet".to_string(), 30000),
        ]);
        let pending_counts = HashMap::from([
            ("gpt-4-turbo".to_string(), HashMap::from([("24h".to_string(), 100000)])),
            ("gpt-3.5-turbo".to_string(), HashMap::from([("24h".to_string(), 500000)])),
            ("claude-3-sonnet".to_string(), HashMap::from([("24h".to_string(), 20000)])),
        ]);
        let model_throughputs = HashMap::from([
            ("gpt-4-turbo".to_string(), 2.0),     // 172800 capacity
            ("gpt-3.5-turbo".to_string(), 10.0),  // 864000 capacity
            ("claude-3-sonnet".to_string(), 1.0), // 86400 capacity
        ]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // gpt-4-turbo: 50000 + 100000 = 150000 < 172800 ✓
        // gpt-3.5-turbo: 200000 + 500000 = 700000 < 864000 ✓
        // claude-3-sonnet: 30000 + 20000 = 50000 < 86400 ✓
        assert!(result.has_capacity);
    }

    #[test]
    fn test_capacity_check_burst_scenario() {
        // Burst scenario: large sudden batch submission
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 80000)]); // Big burst
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("24h".to_string(), 10000)]))]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 80000 + 10000 = 90000 > 86400
        assert!(!result.has_capacity);
        assert_eq!(result.overloaded_models.get("gpt-4"), Some(&3600));
    }

    #[test]
    fn test_capacity_check_gradual_queue_buildup() {
        // Simulating gradual queue buildup approaching capacity
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 1000)]);
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("24h".to_string(), 85000)]))]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 1000 + 85000 = 86000 < 86400 - just squeaks through
        assert!(result.has_capacity);
    }

    // ==================== Window isolation tests (1h vs 24h) ====================

    #[test]
    fn test_capacity_check_1h_window_independent_of_24h_pending() {
        // Key test: 24h pending should NOT affect 1h capacity check
        // This simulates the scenario where 24h queue is saturated but 1h queue is empty

        let file_model_counts = HashMap::from([("gpt-4".to_string(), 300)]); // 300 requests

        // 24h queue is saturated with 80000 requests, but 1h queue has 0
        let pending_counts = HashMap::from([(
            "gpt-4".to_string(),
            HashMap::from([
                ("24h".to_string(), 80000), // Saturated 24h queue
                ("1h".to_string(), 0),      // Empty 1h queue
            ]),
        )]);

        // Throughput of 1.0 req/s:
        // - 1h capacity = 3600
        // - 24h capacity = 86400
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        // Check 1h window: should only consider 1h pending (0), NOT 24h pending
        let result_1h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "1h");

        // 300 + 0 = 300 < 3600, should PASS
        assert!(
            result_1h.has_capacity,
            "1h batch should be accepted when 1h queue is empty, regardless of 24h queue"
        );
        assert!(result_1h.overloaded_models.is_empty());
    }

    #[test]
    fn test_capacity_check_24h_window_independent_of_1h_pending() {
        // Reverse test: 1h pending should NOT affect 24h capacity check

        let file_model_counts = HashMap::from([("gpt-4".to_string(), 50000)]);

        // 1h queue is saturated, but 24h queue has room
        let pending_counts = HashMap::from([(
            "gpt-4".to_string(),
            HashMap::from([
                ("1h".to_string(), 3500),   // Near-saturated 1h queue
                ("24h".to_string(), 10000), // Plenty of room in 24h queue
            ]),
        )]);

        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        // Check 24h window: should only consider 24h pending (10000)
        let result_24h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 50000 + 10000 = 60000 < 86400, should PASS
        assert!(result_24h.has_capacity, "24h batch should be accepted based on 24h queue only");
    }

    #[test]
    fn test_capacity_check_1h_saturated_24h_empty() {
        // 1h queue saturated, 24h queue empty
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 1000)]);

        let pending_counts = HashMap::from([(
            "gpt-4".to_string(),
            HashMap::from([
                ("1h".to_string(), 3000), // Near capacity for 1h
                ("24h".to_string(), 0),   // Empty 24h queue
            ]),
        )]);

        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        // 1h check: 1000 + 3000 = 4000 > 3600, should FAIL
        let result_1h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "1h");
        assert!(!result_1h.has_capacity);
        assert_eq!(result_1h.overloaded_models.get("gpt-4"), Some(&400)); // 4000 - 3600

        // 24h check: 1000 + 0 = 1000 < 86400, should PASS
        let result_24h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");
        assert!(result_24h.has_capacity);
    }

    #[test]
    fn test_capacity_check_low_throughput_different_windows() {
        // Test with low throughput (0.1 req/s) like in the acceptance tests
        // 0.1 req/s:
        // - 1h capacity = 0.1 * 3600 = 360
        // - 24h capacity = 0.1 * 86400 = 8640

        let file_model_counts = HashMap::from([("model".to_string(), 1000)]);

        // 24h queue is full (8000), but 1h queue is empty
        let pending_counts = HashMap::from([(
            "model".to_string(),
            HashMap::from([("24h".to_string(), 8000), ("1h".to_string(), 0)]),
        )]);

        let model_throughputs = HashMap::from([("model".to_string(), 0.1)]);

        // 1h check: 1000 + 0 = 1000 > 360 (1h capacity), should FAIL due to 1h capacity limit
        let result_1h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 0.1, "1h");
        assert!(!result_1h.has_capacity);
        assert_eq!(result_1h.overloaded_models.get("model"), Some(&640)); // 1000 - 360

        // 24h check: 1000 + 8000 = 9000 > 8640 (24h capacity), should FAIL
        let result_24h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 0.1, "24h");
        assert!(!result_24h.has_capacity);
        assert_eq!(result_24h.overloaded_models.get("model"), Some(&360)); // 9000 - 8640
    }

    #[test]
    fn test_capacity_check_low_throughput_small_batch_accepted() {
        // With 0.1 req/s, 1h capacity = 360
        // A small batch of 300 should be accepted when 1h queue is empty

        let file_model_counts = HashMap::from([("model".to_string(), 300)]);

        let pending_counts = HashMap::from([(
            "model".to_string(),
            HashMap::from([
                ("24h".to_string(), 8000), // Full 24h queue (doesn't matter for 1h check)
                ("1h".to_string(), 0),     // Empty 1h queue
            ]),
        )]);

        let model_throughputs = HashMap::from([("model".to_string(), 0.1)]);

        // 1h check: 300 + 0 = 300 < 360, should PASS
        let result_1h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 0.1, "1h");
        assert!(result_1h.has_capacity, "Small batch (300) should fit in 1h window capacity (360)");
    }

    #[test]
    fn test_capacity_check_missing_window_in_pending_counts() {
        // If a window doesn't exist in pending_counts, it should be treated as 0
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 1000)]);

        // Only 24h pending exists, no 1h key
        let pending_counts = HashMap::from([(
            "gpt-4".to_string(),
            HashMap::from([
                ("24h".to_string(), 80000), // Only 24h
            ]),
        )]);

        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        // 1h check: 1h pending is missing, should default to 0
        // 1000 + 0 = 1000 < 3600, should PASS
        let result_1h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "1h");
        assert!(result_1h.has_capacity);
    }
}
