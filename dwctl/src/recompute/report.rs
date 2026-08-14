//! The dry-run report: what each request currently says, what it should say, and why.
//!
//! This is a **downloadable document, not a database table**. The operator saves it, attaches
//! it to the incident, and feeds it to the repair scripts. It is deliberately the only piece
//! of state the feature produces — every later repair step derives from the database
//! (analytics from this file, billing from analytics, balance from the ledger), so there is
//! no run history to persist, migrate or garbage-collect.
//!
//! Every usage field is reported, including ones that did not change. A field showing a zero
//! delta has been *measured* as correct; it is not a field the engine skipped. Which fields a
//! bug breaks is not knowable in advance — the August incident left completion counts intact,
//! the July one destroyed every count in the response — so the report never pre-judges.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use super::RecomputedUsage;
use super::source::CorpusRow;
use crate::pricing::TokenCounts;

/// How much of a row's answer rests on evidence versus inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum Evidence {
    /// Body present and re-read. The counts are what the provider actually reported.
    BodyFrame,
    /// No body available — ZDR, no fusillade link, or purged. Nothing can be verified about
    /// the tokens; only the stored columns are known.
    ColumnsOnly,
    /// A body exists but could not be replayed (unparseable, corrupt status). Needs a human;
    /// deliberately not folded into any total.
    NotReplayable,
}

/// The usage figures for one request, on either side of the comparison.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UsageFigures {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub reasoning_tokens: i64,
    pub total_tokens: i64,
    pub cache_read: i64,
    pub cache_creation_5m: i64,
    pub cache_creation_1h: i64,
    pub cache_creation_24h: i64,
    #[schema(value_type = Option<String>)]
    pub cost: Option<Decimal>,
}

/// One request's before and after.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ReportRow {
    pub analytics_id: i64,
    pub fusillade_request_id: Option<Uuid>,
    /// Non-null means a batch corpus, which needs the `batch_aggregates` analytics columns
    /// delta-folded as well. Carried because the recomputed numbers alone cannot say so.
    pub fusillade_batch_id: Option<Uuid>,
    pub timestamp: DateTime<Utc>,
    pub model: Option<String>,
    pub stored: UsageFigures,
    /// Absent when the row could not be replayed.
    pub recomputed: Option<UsageFigures>,
    pub evidence: Evidence,
    /// `reported` / `rendered` / `estimated`, when a recompute happened.
    pub token_source: Option<String>,
    /// The stored body gave only a flat creation total, so the tier — and therefore the write
    /// multiplier — was assigned by rule. Resolve it against `prompt_cache_entries` rather
    /// than accepting it; see the repair runbook.
    pub cache_tier_inferred: bool,
    /// True when anything at all differs from what is stored.
    pub changed: bool,
    /// Why a row could not be replayed, when it could not.
    pub note: Option<String>,
}

/// Corpus-level totals. Read these before the rows.
#[derive(Debug, Clone, Serialize, ToSchema, Default)]
pub struct ReportSummary {
    pub rows_total: i64,
    pub rows_changed: i64,
    pub rows_unchanged: i64,
    pub rows_not_replayable: i64,
    pub rows_columns_only: i64,
    pub rows_tier_inferred: i64,
    pub stored_prompt_tokens: i64,
    pub recomputed_prompt_tokens: i64,
    pub stored_completion_tokens: i64,
    pub recomputed_completion_tokens: i64,
    #[schema(value_type = String)]
    pub stored_cost: Decimal,
    #[schema(value_type = String)]
    pub recomputed_cost: Decimal,
    /// `recomputed_cost − stored_cost`. Positive means undercharged.
    #[schema(value_type = String)]
    pub net_correction: Decimal,
}

/// The document.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RecomputeReport {
    pub generated_at: DateTime<Utc>,
    /// The corpus predicate, echoed back so the file is self-describing months later.
    pub corpus: serde_json::Value,
    pub summary: ReportSummary,
    pub rows: Vec<ReportRow>,
}

impl UsageFigures {
    fn from_stored(row: &CorpusRow) -> Self {
        Self {
            prompt_tokens: row.stored_prompt_tokens,
            completion_tokens: row.stored_completion_tokens,
            reasoning_tokens: row.stored_reasoning_tokens,
            total_tokens: row.stored_total_tokens,
            cache_read: row.stored_cache_read,
            cache_creation_5m: row.stored_cache_creation_5m,
            cache_creation_1h: row.stored_cache_creation_1h,
            cache_creation_24h: row.stored_cache_creation_24h,
            cost: row.stored_total_cost,
        }
    }

    fn from_recomputed(usage: &RecomputedUsage, cost: Option<Decimal>) -> Self {
        let TokenCounts {
            prompt,
            completion,
            cache_read,
            cache_creation_5m,
            cache_creation_1h,
            cache_creation_24h,
        } = usage.counts;
        Self {
            prompt_tokens: prompt,
            completion_tokens: completion,
            reasoning_tokens: usage.reasoning,
            total_tokens: usage.total,
            cache_read,
            cache_creation_5m,
            cache_creation_1h,
            cache_creation_24h,
            cost,
        }
    }

    /// Field-by-field equality. Cost compares numerically so that trailing-zero differences
    /// in `numeric` scale are not reported as a change — an artefact, not a correction.
    fn differs_from(&self, other: &Self) -> bool {
        self.prompt_tokens != other.prompt_tokens
            || self.completion_tokens != other.completion_tokens
            || self.reasoning_tokens != other.reasoning_tokens
            || self.total_tokens != other.total_tokens
            || self.cache_read != other.cache_read
            || self.cache_creation_5m != other.cache_creation_5m
            || self.cache_creation_1h != other.cache_creation_1h
            || self.cache_creation_24h != other.cache_creation_24h
            || match (self.cost, other.cost) {
                (Some(a), Some(b)) => a != b,
                (None, None) => false,
                _ => true,
            }
    }
}

impl ReportRow {
    /// A row that was replayed successfully.
    pub fn replayed(row: &CorpusRow, usage: &RecomputedUsage, cost: Option<Decimal>) -> Self {
        let stored = UsageFigures::from_stored(row);
        let recomputed = UsageFigures::from_recomputed(usage, cost);
        let changed = stored.differs_from(&recomputed);
        Self {
            analytics_id: row.analytics_id,
            fusillade_request_id: row.fusillade_request_id,
            fusillade_batch_id: row.fusillade_batch_id,
            timestamp: row.timestamp,
            model: row.model.clone(),
            stored,
            recomputed: Some(recomputed),
            evidence: Evidence::BodyFrame,
            token_source: Some(usage.prompt_source.as_str().to_string()),
            cache_tier_inferred: usage.cache_tier_inferred,
            changed,
            note: None,
        }
    }

    /// A row that could not be replayed. Reported rather than dropped: a corpus with silent
    /// omissions is worse than one that says what it could not do.
    pub fn unreplayable(row: &CorpusRow, evidence: Evidence, note: String) -> Self {
        Self {
            analytics_id: row.analytics_id,
            fusillade_request_id: row.fusillade_request_id,
            fusillade_batch_id: row.fusillade_batch_id,
            timestamp: row.timestamp,
            model: row.model.clone(),
            stored: UsageFigures::from_stored(row),
            recomputed: None,
            evidence,
            token_source: None,
            cache_tier_inferred: false,
            changed: false,
            note: Some(note),
        }
    }
}

impl ReportSummary {
    pub fn of(rows: &[ReportRow]) -> Self {
        let mut s = Self {
            rows_total: rows.len() as i64,
            ..Default::default()
        };
        for r in rows {
            match r.evidence {
                Evidence::NotReplayable => s.rows_not_replayable += 1,
                Evidence::ColumnsOnly => s.rows_columns_only += 1,
                Evidence::BodyFrame => {}
            }
            if r.cache_tier_inferred {
                s.rows_tier_inferred += 1;
            }
            s.stored_prompt_tokens += r.stored.prompt_tokens;
            s.stored_completion_tokens += r.stored.completion_tokens;
            s.stored_cost += r.stored.cost.unwrap_or_default();

            match &r.recomputed {
                Some(rec) => {
                    if r.changed {
                        s.rows_changed += 1;
                    } else {
                        s.rows_unchanged += 1;
                    }
                    s.recomputed_prompt_tokens += rec.prompt_tokens;
                    s.recomputed_completion_tokens += rec.completion_tokens;
                    s.recomputed_cost += rec.cost.unwrap_or_default();
                }
                // An un-replayed row contributes its STORED figures to the recomputed totals,
                // so the two sides stay comparable and `net_correction` is the real delta
                // rather than one inflated by rows nobody could check.
                None => {
                    s.recomputed_prompt_tokens += r.stored.prompt_tokens;
                    s.recomputed_completion_tokens += r.stored.completion_tokens;
                    s.recomputed_cost += r.stored.cost.unwrap_or_default();
                }
            }
        }
        s.net_correction = s.recomputed_cost - s.stored_cost;
        s
    }
}
