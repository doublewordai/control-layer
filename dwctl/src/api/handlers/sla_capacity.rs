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

    #[test]
    fn test_parse_window_to_seconds() {
        assert_eq!(parse_window_to_seconds("24h"), 86400);
        assert_eq!(parse_window_to_seconds("1h"), 3600);
        assert_eq!(parse_window_to_seconds("30m"), 1800);
        assert_eq!(parse_window_to_seconds("60s"), 60);
        assert_eq!(parse_window_to_seconds("invalid"), 86400); // default
    }

    #[test]
    fn test_check_sla_capacity_within_limits() {
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 1000)]);
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("24h".to_string(), 5000)]))]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]); // 1 req/s = 86400/day

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h");

        assert!(result.has_capacity);
        assert!(result.overloaded_models.is_empty());
    }

    #[test]
    fn test_check_sla_capacity_exceeds_limits() {
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
    fn test_check_sla_capacity_uses_default_throughput() {
        let file_model_counts = HashMap::from([("unknown-model".to_string(), 1000)]);
        let pending_counts = HashMap::new(); // No pending
        let model_throughputs = HashMap::new(); // No throughput configured

        let result = check_sla_capacity(
            &file_model_counts,
            &pending_counts,
            &model_throughputs,
            1.0, // default: 1 req/s
            "24h",
        );

        assert!(result.has_capacity); // 1000 < 86400
    }
}
