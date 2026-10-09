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
/// This is a pure check for one window: for each model in the batch, verify that
/// `(pending_requests + new_requests) <= throughput * window_seconds * relaxation`.
/// Concurrency control (advisory locks, reservations) and the choice of windows
/// live in [`reserve_capacity`].
///
/// # Arguments
/// * `file_model_counts` - Map of model alias to request count in the new batch
/// * `pending_counts` - Map of model alias -> window -> pending request count
/// * `model_throughputs` - Map of model alias to throughput (req/s)
/// * `default_throughput` - Default throughput for models not in `model_throughputs`
/// * `completion_window` - The completion window (e.g., "24h", "1h")
/// * `relaxation_factor` - Multiplier applied to the model's computed capacity before comparing
///   against pending + new requests. Expected range:
///   - `0.0`: block all new batches for this window (effective capacity = 0)
///   - `1.0`: strict — only accept what current throughput supports
///   - `> 1.0`: over-accept by this factor (e.g. `1.5` allows 50% more than strict capacity),
///     relying on capacity being provisioned before the window expires
///
/// # Returns
/// `SlaCapacityCheckResult` indicating whether there's capacity and which models are overloaded
pub fn check_sla_capacity(
    file_model_counts: &HashMap<String, i64>,
    pending_counts: &HashMap<String, HashMap<String, i64>>,
    model_throughputs: &HashMap<String, f32>,
    default_throughput: f32,
    completion_window: &str,
    relaxation_factor: f32,
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

        // Apply relaxation factor to the raw f64 capacity before clamping to i64,
        // preserving fractional precision especially at low throughputs/short windows.
        let capacity_f64 = (throughput as f64) * (window_seconds as f64) * (relaxation_factor as f64);
        let effective_capacity = if capacity_f64 >= i64::MAX as f64 {
            i64::MAX
        } else if capacity_f64 <= 0.0 {
            0
        } else {
            capacity_f64 as i64
        };

        // Check if we exceed capacity
        let total_requests = pending + new_requests;
        if total_requests > effective_capacity {
            let deficit = total_requests - effective_capacity;
            info!(
                model = %model_alias,
                pending = pending,
                new_requests = new_requests,
                capacity = capacity_f64,
                effective_capacity = effective_capacity,
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
pub(super) fn parse_window_to_seconds(window: &str) -> i64 {
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

// ---------------------------------------------------------------------------
// Shared capacity reservation — used by both API batch creation and sync activate
// ---------------------------------------------------------------------------

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use uuid::Uuid;

use crate::config::BatchConfig;

/// One completion window a batch admission is checked against.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AdmissionWindow {
    pub label: String,
    pub seconds: i64,
    pub relaxation_factor: f32,
}

/// The windows a batch submitted for `requested` must fit in: the requested
/// window and every longer allowed window, shortest first.
///
/// Work due sooner also consumes the capacity of every longer window (the
/// daemon serves earliest deadlines first, so a 1h batch delays 24h work that
/// is already queued). A 1h batch is therefore checked against both the 1h and
/// the 24h window; a 24h batch only against 24h, because it cannot delay work
/// that is due sooner than itself.
pub(crate) fn admission_windows(config: &BatchConfig, requested: &str, requested_relaxation: f32) -> Vec<AdmissionWindow> {
    let requested_seconds = parse_window_to_seconds(requested);
    let mut windows = vec![AdmissionWindow {
        label: requested.to_string(),
        seconds: requested_seconds,
        relaxation_factor: requested_relaxation,
    }];
    for label in &config.allowed_completion_windows {
        let seconds = parse_window_to_seconds(label);
        if seconds > requested_seconds && !windows.iter().any(|w| w.label == *label) {
            windows.push(AdmissionWindow {
                label: label.clone(),
                seconds,
                relaxation_factor: config.relaxation_factor(label),
            });
        }
    }
    windows.sort_by_key(|w| w.seconds);
    windows
}

/// Inputs for a capacity reservation check, independent of API/sync context.
pub(crate) struct CapacityReservationInput<'a> {
    /// The window the batch was submitted for; reservations are recorded under it.
    pub completion_window: &'a str,
    /// Every window the batch must fit in (see [`admission_windows`]).
    pub windows: &'a [AdmissionWindow],
    pub file_model_counts: &'a HashMap<String, i64>,
    pub model_throughputs: &'a HashMap<String, f32>,
    pub model_ids_by_alias: &'a HashMap<String, Uuid>,
    pub default_throughput: f32,
    pub reservation_ttl_secs: i64,
    pub include_pending_counts: bool,
    pub pending_counts_max_age_secs: u64,
    pub pending_counts_timeout_ms: u64,
}

impl<'a> CapacityReservationInput<'a> {
    /// Build the input from the batch config, for a batch submitted for `completion_window`.
    pub(crate) fn from_config(
        config: &BatchConfig,
        completion_window: &'a str,
        windows: &'a [AdmissionWindow],
        file_model_counts: &'a HashMap<String, i64>,
        model_throughputs: &'a HashMap<String, f32>,
        model_ids_by_alias: &'a HashMap<String, Uuid>,
    ) -> Self {
        Self {
            completion_window,
            windows,
            file_model_counts,
            model_throughputs,
            model_ids_by_alias,
            default_throughput: config.default_throughput,
            reservation_ttl_secs: config.reservation_ttl_secs,
            include_pending_counts: config.pending_capacity_counts_enabled,
            pending_counts_max_age_secs: config.pending_capacity_counts_max_age_secs,
            pending_counts_timeout_ms: config.pending_capacity_counts_timeout_ms,
        }
    }
}

/// A checked window that was full, with the models at capacity in it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FullWindow {
    pub window: String,
    /// Sorted model aliases. Each model is listed under the longest window it
    /// failed, and only there.
    pub models: Vec<String>,
}

/// `"model-a, model-b (24h); model-c (1h)"`, longest window first.
pub(crate) fn describe_full_windows(full_windows: &[FullWindow]) -> String {
    full_windows
        .iter()
        .rev()
        .map(|fw| format!("{} ({})", fw.models.join(", "), fw.window))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Error type for capacity reservation operations.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CapacityError {
    /// `completion_window` is the window the batch was submitted for;
    /// `full_windows` (shortest first) are the checked windows that were full,
    /// each with the models that are at capacity there. A window longer than
    /// `completion_window` appears when shorter-deadline work is rejected
    /// because a longer window it also consumes is full.
    #[error("insufficient capacity for {completion_window} window: {}", describe_full_windows(full_windows))]
    InsufficientCapacity {
        completion_window: String,
        full_windows: Vec<FullWindow>,
    },
    #[error("{0}")]
    Internal(String),
}

/// Outstanding admitted work per model alias and window label, plus the
/// dwctl-clock instant read *before* the count: every batch committed before
/// that instant is included in the counts.
#[derive(Debug, Clone)]
struct OutstandingDemand {
    counts: HashMap<String, HashMap<String, i64>>,
    since: DateTime<Utc>,
}

/// Per-replica cache of outstanding-work counts for batch admission.
///
/// Counting is proportional to a model's outstanding backlog, and batches are
/// submitted far more often than that backlog changes meaningfully, so each
/// replica refreshes a model's count at most once per
/// `pending_capacity_counts_max_age_secs` instead of on every submission.
///
/// Reusing a snapshot is sound at any age because of how it is combined with
/// reservations: a batch admitted after the snapshot was taken held a
/// reservation until its batch row was committed, and `reserve_capacity`
/// counts every reservation released at or after the snapshot's `since`
/// instant. So a stale snapshot never misses admitted work; it only still
/// counts work that has finished since, which errs towards rejecting. A batch
/// whose handler died before releasing is counted by its unreleased
/// reservation only until the TTL lapses, so the reuse age is capped at half
/// the TTL (see [`snapshot_max_age_secs`]). The
/// one time-dependent part — work whose deadline moves *into* a window as time
/// passes — is covered by counting each window over a horizon padded by the
/// longest age a snapshot may be used at (see [`outstanding_demand`]).
#[derive(Clone, Default)]
pub struct AdmissionDemandCache {
    inner: Arc<AdmissionDemandCacheInner>,
}

impl std::fmt::Debug for AdmissionDemandCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmissionDemandCache").finish_non_exhaustive()
    }
}

#[derive(Default)]
struct AdmissionDemandCacheInner {
    /// (model alias, window label) -> snapshot entry
    entries: Mutex<HashMap<(String, String), DemandEntry>>,
    /// Single-flight guard so concurrent submissions on one replica share one refresh.
    refresh: tokio::sync::Mutex<()>,
}

#[derive(Debug, Clone, Copy)]
struct DemandEntry {
    since: DateTime<Utc>,
    taken: Instant,
    horizon_pad_secs: i64,
    count: i64,
}

impl AdmissionDemandCache {
    fn lookup(
        &self,
        models: &[String],
        windows: &[AdmissionWindow],
        max_age: std::time::Duration,
        horizon_pad_secs: i64,
    ) -> Option<OutstandingDemand> {
        let entries = self.inner.entries.lock().unwrap_or_else(|e| e.into_inner());
        let mut counts: HashMap<String, HashMap<String, i64>> = HashMap::new();
        let mut since: Option<DateTime<Utc>> = None;
        for model in models {
            for window in windows {
                let entry = entries.get(&(model.clone(), window.label.clone()))?;
                if entry.taken.elapsed() > max_age || entry.horizon_pad_secs < horizon_pad_secs {
                    return None;
                }
                counts.entry(model.clone()).or_default().insert(window.label.clone(), entry.count);
                // Several entries combine under the oldest instant: counting
                // reservations released since then covers all of them.
                since = Some(since.map_or(entry.since, |s| s.min(entry.since)));
            }
        }
        Some(OutstandingDemand { counts, since: since? })
    }

    fn store(&self, models: &[String], windows: &[AdmissionWindow], demand: &OutstandingDemand, taken: Instant, horizon_pad_secs: i64) {
        let mut entries = self.inner.entries.lock().unwrap_or_else(|e| e.into_inner());
        for model in models {
            for window in windows {
                let count = demand.counts.get(model).and_then(|w| w.get(&window.label)).copied().unwrap_or(0);
                entries.insert(
                    (model.clone(), window.label.clone()),
                    DemandEntry {
                        since: demand.since,
                        taken,
                        horizon_pad_secs,
                        count,
                    },
                );
            }
        }
    }
}

/// Count outstanding admitted work for the batch's models, from the cache when
/// a fresh enough snapshot exists. Returns `None` when no usable count could be
/// obtained; admission then fails open to reservations only.
async fn outstanding_demand<P: sqlx_pool_router::PoolProvider>(
    dwctl_pool: &PgPool,
    request_manager: &fusillade_arsenal::PostgresRequestManager<P>,
    cache: Option<&AdmissionDemandCache>,
    input: &CapacityReservationInput<'_>,
) -> Option<OutstandingDemand> {
    let mut models: Vec<String> = input.file_model_counts.keys().cloned().collect();
    models.sort();
    let max_age_secs = snapshot_max_age_secs(input.pending_counts_max_age_secs, input.reservation_ttl_secs);
    let max_age = std::time::Duration::from_secs(max_age_secs);
    // A snapshot may be used up to 2 × max_age old (while another submission
    // refreshes it, or after a failed refresh). Count each window over a
    // horizon padded by that much, so work whose deadline enters the window
    // while the snapshot ages is already included.
    let stale_max_age = max_age.saturating_mul(2);
    let pad_secs = i64::try_from(max_age_secs.saturating_mul(2)).unwrap_or(i64::MAX);

    let Some(cache) = cache.filter(|_| max_age_secs > 0) else {
        return fetch_outstanding_demand(
            dwctl_pool,
            request_manager,
            &models,
            input.windows,
            0,
            input.pending_counts_timeout_ms,
        )
        .await
        .inspect_err(|e| log_count_failure(e))
        .ok();
    };

    if let Some(hit) = cache.lookup(&models, input.windows, max_age, pad_secs) {
        return Some(hit);
    }

    let _refresh = match cache.inner.refresh.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            // Another submission on this replica is refreshing. A snapshot
            // within the padded horizon is still sound; otherwise wait for it.
            if let Some(hit) = cache.lookup(&models, input.windows, stale_max_age, pad_secs) {
                return Some(hit);
            }
            cache.inner.refresh.lock().await
        }
    };
    if let Some(hit) = cache.lookup(&models, input.windows, max_age, pad_secs) {
        return Some(hit);
    }

    let taken = Instant::now();
    match fetch_outstanding_demand(
        dwctl_pool,
        request_manager,
        &models,
        input.windows,
        pad_secs,
        input.pending_counts_timeout_ms,
    )
    .await
    {
        Ok(demand) => {
            cache.store(&models, input.windows, &demand, taken, pad_secs);
            Some(demand)
        }
        Err(e) => {
            log_count_failure(&e);
            cache.lookup(&models, input.windows, stale_max_age, pad_secs)
        }
    }
}

/// The longest a cached outstanding-work snapshot may be reused without
/// refresh, in seconds: the configured max age, capped at half the
/// reservation TTL.
///
/// A batch admitted after a snapshot was taken is counted through its
/// reservation. A released reservation is counted via `released_since` at any
/// age, but an unreleased one (its handler died between creating the batch and
/// releasing) only until its TTL lapses. A snapshot is used up to twice this
/// age, so capping it at TTL / 2 keeps every reservation created after the
/// snapshot active for as long as the snapshot is in use.
fn snapshot_max_age_secs(configured_secs: u64, reservation_ttl_secs: i64) -> u64 {
    configured_secs.min(u64::try_from(reservation_ttl_secs / 2).unwrap_or(0))
}

fn log_count_failure(error: &str) {
    tracing::error!(
        error = %error,
        "Outstanding-work count failed during batch admission; admitting on the last usable snapshot or active reservations only"
    );
}

async fn fetch_outstanding_demand<P: sqlx_pool_router::PoolProvider>(
    dwctl_pool: &PgPool,
    request_manager: &fusillade_arsenal::PostgresRequestManager<P>,
    models: &[String],
    windows: &[AdmissionWindow],
    horizon_pad_secs: i64,
    timeout_ms: u64,
) -> Result<OutstandingDemand, String> {
    use crate::db::handlers::BatchAdmissionDemand;

    // Read from the dwctl database (the clock `released_at` is stamped with),
    // and before the count, so any reservation released earlier than this
    // instant belongs to a batch whose row the count below already sees.
    let since: DateTime<Utc> = sqlx::query_scalar!(r#"SELECT now() AS "now!""#)
        .fetch_one(dwctl_pool)
        .await
        .map_err(|e| format!("read reservation clock: {e}"))?;

    let horizons: Vec<(String, i64)> = windows
        .iter()
        .map(|w| (w.label.clone(), w.seconds.saturating_add(horizon_pad_secs)))
        .collect();

    // Primary, not a replica: the snapshot must include every batch committed
    // before `since`.
    let mut tx = request_manager
        .begin_write()
        .await
        .map_err(|e| format!("begin outstanding-work count: {e}"))?;
    let counts = BatchAdmissionDemand::new(&mut tx)
        .outstanding_by_model_and_window(models, &horizons, timeout_ms)
        .await
        .map_err(|e| format!("count outstanding batch work: {e}"))?;
    tx.rollback().await.ok();

    Ok(OutstandingDemand { counts, since })
}

/// Reserve capacity for a batch. Returns reservation IDs on success,
/// or `CapacityError::InsufficientCapacity` if any checked window is full.
///
/// For each window in `input.windows` and each model in the batch:
///
/// ```text
/// outstanding work due within the window      (pending_capacity_counts_enabled)
///   + reservations for this or shorter windows (active, plus released since the count)
///   + this batch
///   <= floor(throughput × window × relaxation)
/// ```
///
/// Used by both `POST /batches` (API) and `run_activate_batch` (sync pipeline).
pub(crate) async fn reserve_capacity<P: sqlx_pool_router::PoolProvider>(
    dwctl_pool: &PgPool,
    request_manager: &fusillade_arsenal::PostgresRequestManager<P>,
    cache: Option<&AdmissionDemandCache>,
    input: &CapacityReservationInput<'_>,
) -> Result<Vec<Uuid>, CapacityError> {
    use crate::db::handlers::BatchCapacityReservations;

    // The outstanding-work count is taken BEFORE the per-model advisory locks.
    // It is the expensive part of admission — it scales with the requested
    // models' backlog — and holding the locks across it would head-of-line
    // block every concurrent submission for the same model.
    //
    // Reading outstanding work before reservations opens a gap: a peer batch
    // could commit after the count and release its reservation before the
    // locked reservation read below, appearing in neither. The reservation sum
    // therefore also includes reservations released at or after the count's
    // `since` instant, so such a batch is still counted (at worst twice, which
    // only errs towards under-acceptance). The same argument makes a cached
    // count of any age sound; see `AdmissionDemandCache`.
    //
    // Fail open: a failed count must not fail the submission — that would turn
    // a database timeout into a customer-facing 500 for work the system could
    // accept. Admission then degrades to the reservations-only check it
    // performs with the flag off.
    let demand = if input.include_pending_counts {
        outstanding_demand(dwctl_pool, request_manager, cache, input).await
    } else {
        None
    };
    let released_since = demand.as_ref().map(|d| d.since);
    let mut pending: HashMap<String, HashMap<String, i64>> = demand.map(|d| d.counts).unwrap_or_default();

    let mut tx = dwctl_pool
        .begin()
        .await
        .map_err(|e| CapacityError::Internal(format!("begin reservation transaction: {e}")))?;

    // Lock every (model, window) pair this admission reads, in a global order
    // (model id, then window length) so concurrent admissions spanning several
    // models or windows cannot deadlock. A 1h and a 24h admission for the same
    // model both take (model, 24h) and so serialise against each other.
    let mut model_pairs: Vec<(String, Uuid)> = input.model_ids_by_alias.iter().map(|(a, id)| (a.clone(), *id)).collect();
    model_pairs.sort_by_key(|(_, id)| *id);

    for (alias, model_id) in &model_pairs {
        for window in input.windows {
            sqlx::query!(
                "SELECT pg_advisory_xact_lock(hashtext($1::text), hashtext($2::text))",
                model_id.to_string(),
                window.label
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| CapacityError::Internal(format!("lock reservation for {alias}: {e}")))?;
        }
    }

    // Reservations under the lock. A reservation's deadline is its own
    // window, so it counts towards every checked window at least that long.
    let model_ids: Vec<Uuid> = model_pairs.iter().map(|(_, id)| *id).collect();
    let id_to_alias: HashMap<Uuid, String> = model_pairs.iter().map(|(a, id)| (*id, a.clone())).collect();
    let mut reservations = BatchCapacityReservations::new(&mut tx);

    let reserved_rows = reservations
        .sum_by_model_and_window(&model_ids, released_since)
        .await
        .map_err(|e| CapacityError::Internal(format!("sum reservations: {e}")))?;

    for (model_id, reservation_window, reserved) in reserved_rows {
        let Some(alias) = id_to_alias.get(&model_id) else { continue };
        let reservation_seconds = parse_window_to_seconds(&reservation_window);
        let per_window = pending.entry(alias.clone()).or_default();
        for window in input.windows {
            if reservation_seconds <= window.seconds {
                *per_window.entry(window.label.clone()).or_insert(0) += reserved;
            }
        }
    }

    // Per model: the longest checked window it does not fit in, with the
    // deficit there. Windows are shortest first, so a later failure replaces
    // an earlier one and each model is reported against one window only.
    let mut overloaded_models: HashMap<String, (&str, i64)> = HashMap::new();
    for window in input.windows {
        let result = check_sla_capacity(
            input.file_model_counts,
            &pending,
            input.model_throughputs,
            input.default_throughput,
            &window.label,
            window.relaxation_factor,
        );
        for (model, deficit) in result.overloaded_models {
            overloaded_models.insert(model, (window.label.as_str(), deficit));
        }
    }

    if !overloaded_models.is_empty() {
        tx.rollback().await.ok();

        let full_windows: Vec<FullWindow> = input
            .windows
            .iter()
            .filter_map(|window| {
                let mut models: Vec<String> = overloaded_models
                    .iter()
                    .filter(|(_, (label, _))| *label == window.label)
                    .map(|(model, _)| model.clone())
                    .collect();
                if models.is_empty() {
                    return None;
                }
                models.sort_unstable();
                Some(FullWindow {
                    window: window.label.clone(),
                    models,
                })
            })
            .collect();
        let mut overloaded_details: Vec<String> = overloaded_models
            .iter()
            .map(|(model, (label, deficit))| format!("{model} (needs {deficit} more capacity in {label})"))
            .collect();
        overloaded_details.sort_unstable();
        tracing::warn!(
            completion_window = %input.completion_window,
            full_windows = %describe_full_windows(&full_windows),
            overloaded_models = %overloaded_details.join(", "),
            "Batch rejected due to insufficient capacity"
        );

        return Err(CapacityError::InsufficientCapacity {
            completion_window: input.completion_window.to_string(),
            full_windows,
        });
    }

    // Insert reservations
    let expires_at = Utc::now() + Duration::seconds(input.reservation_ttl_secs);
    let mut rows = Vec::new();
    for (alias, model_id) in &model_pairs {
        if let Some(&count) = input.file_model_counts.get(alias)
            && count > 0
        {
            rows.push((*model_id, input.completion_window, count, expires_at));
        }
    }

    let reservation_ids = reservations
        .insert_reservations(&rows)
        .await
        .map_err(|e| CapacityError::Internal(format!("insert reservations: {e}")))?;

    tx.commit()
        .await
        .map_err(|e| CapacityError::Internal(format!("commit reservation transaction: {e}")))?;

    Ok(reservation_ids)
}

/// Release capacity reservations (best-effort — TTL is the safety net).
pub(crate) async fn release_reservations(dwctl_pool: &PgPool, reservation_ids: &[Uuid]) -> Result<(), String> {
    use crate::db::handlers::BatchCapacityReservations;

    if reservation_ids.is_empty() {
        return Ok(());
    }

    let mut conn = dwctl_pool.acquire().await.map_err(|e| format!("acquire connection: {e}"))?;

    BatchCapacityReservations::new(&mut conn)
        .release_reservations(reservation_ids)
        .await
        .map_err(|e| format!("release reservations: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_max_age_capped_at_half_reservation_ttl() {
        // Default config: 10s max age, 600s TTL.
        assert_eq!(snapshot_max_age_secs(10, 600), 10);
        // A snapshot reused at 2 × max age must not outlive an unreleased
        // reservation created after it.
        assert_eq!(snapshot_max_age_secs(900, 600), 300);
        assert_eq!(snapshot_max_age_secs(u64::MAX, 600), 300);
        // A TTL too short to cover any reuse disables the cache.
        assert_eq!(snapshot_max_age_secs(10, 1), 0);
        assert_eq!(snapshot_max_age_secs(10, -5), 0);
    }

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

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

        assert!(result.has_capacity);
        assert!(result.overloaded_models.is_empty());
    }

    #[test]
    fn test_capacity_check_single_model_within_limits() {
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 1000)]);
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("24h".to_string(), 5000)]))]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]); // 1 req/s = 86400/day

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

        assert!(result.has_capacity);
        assert!(result.overloaded_models.is_empty());
    }

    #[test]
    fn test_capacity_check_single_model_exceeds_limits() {
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 50000)]);
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("24h".to_string(), 50000)]))]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]); // 1 req/s = 86400/day

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

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

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

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

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

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

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

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

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

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

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

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
            1.0,
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
            1.0,
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
            1.0,
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

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "1h", 1.0);

        // 2000 + 1000 = 3000 < 3600
        assert!(result.has_capacity);
    }

    #[test]
    fn test_capacity_check_1h_window_exceeds() {
        // 1 req/s for 1h = 3600 capacity
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 3000)]);
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("1h".to_string(), 1000)]))]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "1h", 1.0);

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
        let result_1h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "1h", 1.0);
        assert!(!result_1h.has_capacity);

        // Check 24h window: 1000 + 10000 = 11000 < 86400, should pass
        let result_24h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);
        assert!(result_24h.has_capacity);
    }

    // ==================== Edge cases with pending counts ====================

    #[test]
    fn test_capacity_check_no_pending_for_model() {
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 1000)]);
        let pending_counts = HashMap::new(); // No pending at all
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

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
        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

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

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

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

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

        // 5,000,000 < 8,640,000
        assert!(result.has_capacity);
    }

    #[test]
    fn test_capacity_check_fractional_throughput() {
        // 0.5 req/s for 24h = 43200 capacity
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 40000)]);
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 0.5)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

        // 40000 < 43200
        assert!(result.has_capacity);
    }

    #[test]
    fn test_capacity_check_fractional_throughput_exceeds() {
        // 0.5 req/s for 24h = 43200 capacity
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 50000)]);
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 0.5)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

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

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

        // 0 + 50000 < 86400
        assert!(result.has_capacity);
    }

    #[test]
    fn test_capacity_check_zero_pending() {
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 50000)]);
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("24h".to_string(), 0)]))]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

        // 50000 + 0 < 86400
        assert!(result.has_capacity);
    }

    #[test]
    fn test_capacity_check_zero_throughput() {
        // Zero throughput means zero capacity - all requests should be rejected
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 1)]);
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 0.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

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

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

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
            1.0,
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

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

        // 50 < 86
        assert!(result.has_capacity);
    }

    #[test]
    fn test_capacity_check_very_large_throughput_no_overflow() {
        // Very large throughput should not cause overflow
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 1_000_000_000)]);
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1_000_000.0)]); // 1M req/s

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

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

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

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

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

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

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

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

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

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
        let result_1h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "1h", 1.0);

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
        let result_24h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

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
        let result_1h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "1h", 1.0);
        assert!(!result_1h.has_capacity);
        assert_eq!(result_1h.overloaded_models.get("gpt-4"), Some(&400)); // 4000 - 3600

        // 24h check: 1000 + 0 = 1000 < 86400, should PASS
        let result_24h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);
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
        let result_1h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 0.1, "1h", 1.0);
        assert!(!result_1h.has_capacity);
        assert_eq!(result_1h.overloaded_models.get("model"), Some(&640)); // 1000 - 360

        // 24h check: 1000 + 8000 = 9000 > 8640 (24h capacity), should FAIL
        let result_24h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 0.1, "24h", 1.0);
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
        let result_1h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 0.1, "1h", 1.0);
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
        let result_1h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "1h", 1.0);
        assert!(result_1h.has_capacity);
    }

    // ==================== Relaxation factor tests ====================

    #[test]
    fn test_relaxation_factor_one_is_strict() {
        // factor=1.0 should behave identically to no relaxation
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 40001)]);
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("24h".to_string(), 46400)]))]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 1.0);

        // 40001 + 46400 = 86401 > 86400, rejected
        assert!(!result.has_capacity);
        assert_eq!(result.overloaded_models.get("gpt-4"), Some(&1));
    }

    #[test]
    fn test_relaxation_factor_above_one_expands_capacity() {
        // factor=1.5 expands 1h capacity from 3600 to 5400
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 4000)]);
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("1h".to_string(), 1000)]))]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        // Without relaxation: 4000 + 1000 = 5000 > 3600, would be rejected
        let strict = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "1h", 1.0);
        assert!(!strict.has_capacity);

        // With factor=1.5: effective capacity = 3600 * 1.5 = 5400, 5000 < 5400, accepted
        let relaxed = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "1h", 1.5);
        assert!(relaxed.has_capacity);
    }

    #[test]
    fn test_relaxation_factor_zero_blocks_all_requests() {
        // factor=0.0 means effective capacity=0, any request is rejected
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 1)]);
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "24h", 0.0);

        assert!(!result.has_capacity);
        assert_eq!(result.overloaded_models.get("gpt-4"), Some(&1));
    }

    #[test]
    fn test_relaxation_factor_deficit_reflects_effective_capacity() {
        // factor=2.0 doubles 1h capacity from 3600 to 7200
        // total=8000, effective_capacity=7200, deficit=800
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 8000)]);
        let pending_counts = HashMap::new();
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        let result = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "1h", 2.0);

        assert!(!result.has_capacity);
        assert_eq!(result.overloaded_models.get("gpt-4"), Some(&800)); // 8000 - 7200
    }

    #[test]
    fn test_relaxation_factor_does_not_affect_other_windows() {
        // Relaxation on 24h should not bleed into a 1h check — they are called separately
        // This test just confirms the factor is applied to the window being checked
        let file_model_counts = HashMap::from([("gpt-4".to_string(), 5000)]);
        let pending_counts = HashMap::from([("gpt-4".to_string(), HashMap::from([("1h".to_string(), 0)]))]);
        let model_throughputs = HashMap::from([("gpt-4".to_string(), 1.0)]);

        // 1h strict (factor=1.0): 5000 > 3600, rejected
        let strict_1h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "1h", 1.0);
        assert!(!strict_1h.has_capacity);

        // 1h relaxed (factor=2.0): effective=7200, 5000 < 7200, accepted
        let relaxed_1h = check_sla_capacity(&file_model_counts, &pending_counts, &model_throughputs, 1.0, "1h", 2.0);
        assert!(relaxed_1h.has_capacity);
    }

    // ==================== admission_windows tests ====================

    fn batch_config(windows: &[&str], relaxation: &[(&str, f32)]) -> BatchConfig {
        BatchConfig {
            allowed_completion_windows: windows.iter().map(|w| w.to_string()).collect(),
            window_relaxation_factors: relaxation.iter().map(|(w, f)| (w.to_string(), *f)).collect(),
            ..BatchConfig::default()
        }
    }

    #[test]
    fn test_admission_windows_include_requested_and_longer_windows() {
        let config = batch_config(&["24h", "1h"], &[("24h", 1.5)]);

        let one_hour = admission_windows(&config, "1h", 2.0);
        assert_eq!(
            one_hour,
            vec![
                AdmissionWindow {
                    label: "1h".to_string(),
                    seconds: 3600,
                    relaxation_factor: 2.0
                },
                AdmissionWindow {
                    label: "24h".to_string(),
                    seconds: 86400,
                    relaxation_factor: 1.5
                },
            ]
        );

        // A 24h batch cannot delay work due sooner than itself.
        let day = admission_windows(&config, "24h", 1.5);
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].label, "24h");
    }

    #[test]
    fn test_admission_windows_always_include_requested_window() {
        // e.g. a sync connection whose default window is not in the API allow-list
        let config = batch_config(&["24h"], &[]);
        let windows = admission_windows(&config, "48h", 1.0);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].label, "48h");
        assert_eq!(windows[0].seconds, 172800);
    }
}
