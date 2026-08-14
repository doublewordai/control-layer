//! Admin endpoint for the usage-recompute dry run.
//!
//! **Read-only, structurally.** The handler is handed `state.db.read()` and nothing else, so
//! it cannot write even by mistake — the pool it holds has no path to a mutation. Applying a
//! correction is a separate, human-run sequence documented in the internal
//! `usage-recompute-repair` runbook; see [`crate::recompute`] for why that split exists.
//!
//! The response is a document meant to be saved: it is the only record of what was
//! calculated, and the repair scripts consume it.

use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use utoipa::IntoParams;
use uuid::Uuid;

use crate::AppState;
use crate::auth::permissions::{RequiresPermission, operation, resource};
use crate::errors::{Error, Result};
use crate::recompute::cache_fields::CreationTier;
use crate::recompute::report::RecomputeReport;
use crate::recompute::source::CorpusFilter;
use sqlx_pool_router::PoolProvider;

/// The largest corpus a single call will return.
///
/// A recompute reads whole stored response bodies, so the cost is in megabytes rather than
/// rows. Incident corpora measured so far are ~1k rows; this leaves headroom without letting
/// a mistyped predicate pull the whole table into memory.
const MAX_LIMIT: i64 = 25_000;
const DEFAULT_LIMIT: i64 = 5_000;

/// The widest time window accepted, to stop an unbounded scan of a 185M-row table.
const MAX_WINDOW_DAYS: i64 = 31;

#[derive(Debug, Deserialize, IntoParams)]
pub struct RecomputeQuery {
    /// Start of the window (inclusive). Anchor this on the deploy that introduced the bug.
    pub start: DateTime<Utc>,
    /// End of the window (inclusive).
    pub end: DateTime<Utc>,
    /// Restrict to one user. Usually set — incidents are rarely spread evenly.
    pub user_id: Option<Uuid>,
    /// SQL `LIKE` pattern against the request path, e.g. `/messages%`.
    pub uri_pattern: Option<String>,
    /// Restrict to one model alias.
    pub model: Option<String>,
    /// Cap on rows returned. Defaults to 5,000, hard maximum 25,000.
    pub limit: Option<i64>,
    /// Tier to attribute a flat cache-creation total to when the stored body gives no
    /// breakdown: `5m` (default), `1h`, or `24h`.
    ///
    /// Do not guess. `prompt_cache_entries.ttl_tier` records the real tier per entry — query
    /// it for the corpus and pass the answer. Rows relying on this are flagged
    /// `cache_tier_inferred` so the assumption is visible rather than buried.
    pub flat_creation_tier: Option<String>,
}

/// Recompute the usage of already-billed requests and report what changed.
#[utoipa::path(
    get,
    path = "/usage-recompute",
    tag = "analytics",
    summary = "Dry-run recompute of recorded usage",
    description = "Replays stored request/response payloads through the live serializer and re-prices them, \
reporting stored vs recomputed usage per request. Read-only: it writes nothing. Applying a correction is a \
separate documented procedure.",
    responses(
        (status = 200, description = "The recompute report.", body = RecomputeReport),
        (status = 400, description = "Invalid or unbounded corpus predicate."),
        (status = 403, description = "Requires analytics read access.")
    ),
    params(RecomputeQuery)
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id))]
pub async fn recompute_usage<P: PoolProvider>(
    Query(query): Query<RecomputeQuery>,
    State(state): State<AppState<P>>,
    current_user: RequiresPermission<resource::Analytics, operation::ReadAll>,
) -> Result<Json<RecomputeReport>> {
    if query.end <= query.start {
        return Err(Error::BadRequest {
            message: "end must be after start".to_string(),
        });
    }
    if query.end - query.start > Duration::days(MAX_WINDOW_DAYS) {
        return Err(Error::BadRequest {
            message: format!("window must be at most {MAX_WINDOW_DAYS} days; narrow it and run several corpora"),
        });
    }

    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if limit <= 0 || limit > MAX_LIMIT {
        return Err(Error::BadRequest {
            message: format!("limit must be between 1 and {MAX_LIMIT}"),
        });
    }

    let flat_tier = match query.flat_creation_tier.as_deref() {
        None | Some("5m") => CreationTier::FiveMinute,
        Some("1h") => CreationTier::OneHour,
        Some("24h") => CreationTier::TwentyFourHour,
        Some(other) => {
            return Err(Error::BadRequest {
                message: format!("unknown flat_creation_tier {other:?}; expected 5m, 1h or 24h"),
            });
        }
    };

    let filter = CorpusFilter {
        start: query.start,
        end: query.end,
        user_id: query.user_id,
        uri_pattern: query.uri_pattern,
        model: query.model,
        limit,
    };

    // A classifier for the cache-split reconstruction, built on the READ pool — the index it
    // is given per row is historical and refuses writes, so this cannot mutate the cache.
    // Absent when caching is disabled, in which case rows simply carry no reconstruction.
    let cfg = state.current_config();
    let classifier = cfg.cache.enabled.then(|| {
        let pool = state.db.read().clone();
        crate::prompt_cache::Classifier::new(
            crate::prompt_cache::PrincipalResolver::new(pool.clone()),
            crate::prompt_cache::ModelConfigResolver::new(pool.clone()),
            crate::prompt_cache::TokenizerClient::new(cfg.cache.tokenizer_url.clone()),
            // Placeholder: `recompute_corpus` swaps in a per-row historical index.
            std::sync::Arc::new(crate::recompute::cache_replay::HistoricalIndex::new(pool, Utc::now())),
            crate::prompt_cache::TierPolicy::from_config(&cfg.cache.enabled_ttls, &cfg.cache.default_ttl),
            crate::prompt_cache::TelemetryPolicy::from_config(
                cfg.cache.telemetry_blocks.strip_from_prompt,
                &cfg.cache.telemetry_blocks.prefixes,
            ),
            cfg.cache.render_counting,
        )
    });

    // The read pool, deliberately: this path has no write handle at all.
    let report = crate::recompute::recompute_corpus(state.db.read(), &filter, flat_tier, classifier.as_ref())
        .await
        .map_err(|e| Error::Internal {
            operation: format!("recompute usage: {e}"),
        })?;

    tracing::info!(
        rows_total = report.summary.rows_total,
        rows_changed = report.summary.rows_changed,
        net_correction = %report.summary.net_correction,
        "usage recompute dry run"
    );

    Ok(Json(report))
}
