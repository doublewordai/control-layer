//! SLA capacity checking for batch creation.
//!
//! This module provides functions to check whether a new batch can be accepted
//! within the requested SLA window based on model throughput and pending work.

use std::collections::HashMap;
use tracing::info;

/// Result of an SLA capacity check
#[derive(Debug)]
pub struct SlaCapacityCheckResult {
    /// Whether there is sufficient capacity for the batch
    pub has_capacity: bool,
    /// Models that exceed capacity (model_alias -> deficit in requests)
    pub overloaded_models: HashMap<String, i64>,
}

/// Check if a batch can be accepted within the SLA window.
///
/// This is a simple check: for each model in the batch, verify that
/// `(pending_requests + new_requests) <= throughput * window_seconds`
///
/// # Arguments
/// * `file_model_counts` - Map of model alias to request count in the new batch
/// * `pending_counts` - Map of model alias -> window -> pending request count
/// * `model_throughputs` - Map of model alias to throughput (req/s)
/// * `default_throughput` - Default throughput for models not in `model_throughputs`
/// * `completion_window` - The SLA window (e.g., "24h", "1h")
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
        let throughput = model_throughputs.get(model_alias).copied().unwrap_or(default_throughput);

        // Calculate capacity: throughput * window_seconds
        let capacity = (throughput as f64 * window_seconds as f64) as i64;

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
                "Model exceeds SLA capacity"
            );
            overloaded_models.insert(model_alias.clone(), deficit);
        }
    }

    SlaCapacityCheckResult {
        has_capacity: overloaded_models.is_empty(),
        overloaded_models,
    }
}

/// Parse a completion window string (e.g., "24h", "1h") to seconds
fn parse_window_to_seconds(window: &str) -> i64 {
    // Simple parser for common formats
    if window.ends_with('h')
        && let Ok(hours) = window.trim_end_matches('h').parse::<i64>()
    {
        return hours * 3600;
    }

    if window.ends_with('m')
        && let Ok(minutes) = window.trim_end_matches('m').parse::<i64>()
    {
        return minutes * 60;
    }

    if window.ends_with('s')
        && let Ok(seconds) = window.trim_end_matches('s').parse::<i64>()
    {
        return seconds;
    }
    // Default to 24 hours if parsing fails
    24 * 3600
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
    fn test_parse_window_zero() {
        assert_eq!(parse_window_to_seconds("0h"), 0);
        assert_eq!(parse_window_to_seconds("0m"), 0);
        assert_eq!(parse_window_to_seconds("0s"), 0);
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
            ("gpt-3.5".to_string(), 20000),
            ("claude".to_string(), 15000),
        ]);
        let pending_counts = HashMap::from([
            ("gpt-4".to_string(), HashMap::from([("24h".to_string(), 10000)])),
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
            ("gpt-3.5".to_string(), 100000), // This one will exceed
            ("claude".to_string(), 15000),
        ]);
        let pending_counts = HashMap::from([
            ("gpt-4".to_string(), HashMap::from([("24h".to_string(), 10000)])),
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
            1.0, // default: 1 req/s = 86400/day
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
            1.0, // default: 1 req/s = 86400/day
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
            0.5, // default: 0.5 req/s = 43200/day
            "24h",
        );

        assert!(!result.has_capacity);
        // gpt-4: 10000 < 172800, OK
        // unknown-model: 50000 > 43200, NOT OK
        assert_eq!(result.overloaded_models.len(), 1);
        assert_eq!(result.overloaded_models.get("unknown-model"), Some(&6800)); // 50000 - 43200
    }

    // ==================== Different SLA window tests ====================

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
                ("1h".to_string(), 3000),   // High load in 1h window
                ("24h".to_string(), 10000), // Different load in 24h window
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
            ("claude".to_string(), HashMap::from([("24h".to_string(), 50000)])), // Different model
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
        let file_model_counts = HashMap::from([("fast-model".to_string(), 5_000_000)]);
        let pending_counts = HashMap::from([("fast-model".to_string(), HashMap::from([("24h".to_string(), 3_000_000)]))]);
        let model_throughputs = HashMap::from([("fast-model".to_string(), 100.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 5M + 3M = 8M < 8.64M
        assert!(result.has_capacity);
    }

    #[test]
    fn test_capacity_check_fractional_throughput() {
        // 0.1 req/s for 24h = 8640 capacity
        let file_model_counts = HashMap::from([("slow-model".to_string(), 5000)]);
        let pending_counts = HashMap::from([("slow-model".to_string(), HashMap::from([("24h".to_string(), 3000)]))]);
        let model_throughputs = HashMap::from([("slow-model".to_string(), 0.1)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 5000 + 3000 = 8000 < 8640
        assert!(result.has_capacity);
    }

    #[test]
    fn test_capacity_check_fractional_throughput_exceeds() {
        // 0.1 req/s for 24h = 8640 capacity
        let file_model_counts = HashMap::from([("slow-model".to_string(), 6000)]);
        let pending_counts = HashMap::from([("slow-model".to_string(), HashMap::from([("24h".to_string(), 3000)]))]);
        let model_throughputs = HashMap::from([("slow-model".to_string(), 0.1)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 6000 + 3000 = 9000 > 8640
        assert!(!result.has_capacity);
        assert_eq!(result.overloaded_models.get("slow-model"), Some(&360)); // 9000 - 8640
    }

    // ==================== Zero/edge value tests ====================

    #[test]
    fn test_capacity_check_zero_new_requests() {
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 0)]);
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("24h".to_string(), 50000)]))]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 0 + 50000 = 50000 < 86400
        assert!(result.has_capacity);
    }

    #[test]
    fn test_capacity_check_zero_pending() {
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 50000)]);
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("24h".to_string(), 0)]))]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 50000 + 0 = 50000 < 86400
        assert!(result.has_capacity);
    }

    #[test]
    fn test_capacity_check_zero_throughput() {
        // Edge case: throughput of 0 means 0 capacity
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 1)]);
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 0.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // Capacity = 0, any requests exceed it
        assert!(!result.has_capacity);
        assert_eq!(result.overloaded_models.get("gpt-4"), Some(&1));
    }

    #[test]
    fn test_capacity_check_very_small_throughput() {
        // 0.001 req/s for 24h = 86.4 ≈ 86 capacity (truncated to i64)
        let file_model_counts = HashMap::from([("slow-model".to_string(), 80)]);
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::from([("slow-model".to_string(), 0.001)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 80 < 86
        assert!(result.has_capacity);
    }

    // ==================== Composite model scenarios ====================

    #[test]
    fn test_capacity_check_composite_model_as_sum_of_components() {
        // Scenario: User has set composite model throughput as sum of its components
        // composite-gpt has components gpt-4 (1 req/s) + gpt-3.5 (2 req/s) = 3 req/s
        let file_model_counts = HashMap::from([("composite-gpt".to_string(), 200000)]);
        let pending_counts = HashMap::from([("composite-gpt".to_string(), HashMap::from([("24h".to_string(), 50000)]))]);
        let model_throughputs = HashMap::from([
            ("composite-gpt".to_string(), 3.0), // Admin manually set this
        ]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 200000 + 50000 = 250000 < 259200 (3 * 86400)
        assert!(result.has_capacity);
    }

    // ==================== Real-world scenario tests ====================

    #[test]
    fn test_capacity_check_realistic_production_scenario() {
        // Scenario: Production system with multiple models, varying throughputs
        let file_model_counts = HashMap::from([
            ("gpt-4".to_string(), 10000),               // Premium model, lower volume
            ("gpt-3.5-turbo".to_string(), 50000),       // High volume model
            ("text-embedding-ada".to_string(), 100000), // Embedding requests
        ]);
        let pending_counts = HashMap::from([
            ("gpt-4".to_string(), HashMap::from([("24h".to_string(), 5000)])),
            ("gpt-3.5-turbo".to_string(), HashMap::from([("24h".to_string(), 20000)])),
            ("text-embedding-ada".to_string(), HashMap::from([("24h".to_string(), 50000)])),
        ]);
        let model_throughputs = HashMap::from([
            ("gpt-4".to_string(), 0.5),               // 43200/day - limited capacity
            ("gpt-3.5-turbo".to_string(), 5.0),       // 432000/day - high capacity
            ("text-embedding-ada".to_string(), 10.0), // 864000/day - very high capacity
        ]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // gpt-4: 10000 + 5000 = 15000 < 43200 ✓
        // gpt-3.5-turbo: 50000 + 20000 = 70000 < 432000 ✓
        // text-embedding-ada: 100000 + 50000 = 150000 < 864000 ✓
        assert!(result.has_capacity);
    }

    #[test]
    fn test_capacity_check_burst_scenario() {
        // Scenario: Large batch that would overwhelm a model
        let file_model_counts = HashMap::from([
            ("gpt-4".to_string(), 100000), // Huge batch
        ]);
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("1h".to_string(), 2000)]))]);
        let model_throughputs = HashMap::from([
            ("gpt-4".to_string(), 1.0), // 3600/hour capacity
        ]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "1h");

        // 100000 + 2000 = 102000 >> 3600
        assert!(!result.has_capacity);
        assert!(result.overloaded_models.get("gpt-4").unwrap() > &90000);
    }

    #[test]
    fn test_capacity_check_gradual_queue_buildup() {
        // Scenario: Queue has been building up, new batch would tip it over
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 10000)]);
        let pending_counts = HashMap::from([
            ("gpt-4".to_string(), HashMap::from([("24h".to_string(), 80000)])), // Already near capacity
        ]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        // 10000 + 80000 = 90000 > 86400
        assert!(!result.has_capacity);
        assert_eq!(result.overloaded_models.get("gpt-4"), Some(&3600));
    }
}
