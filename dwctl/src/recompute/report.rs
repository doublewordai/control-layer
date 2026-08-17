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

    /// The cache split re-derived from the prefix index as it stood at this request's
    /// timestamp — an answer independent of anything the response reported.
    ///
    /// Absent when cache replay was not attempted (caching disabled, no request body) or the
    /// model was not cache-enabled for this principal, which is different from a split of
    /// zero.
    pub reconstructed_cache: Option<ReconstructedCache>,

    /// The exact chat-templated token count of the stored request from tokenizer-svc's
    /// `/v1/render`, when a tokenizer was available and the model is mapped. Recorded NEXT
    /// TO the provider's count, never adopted over it — replacing an agreeing count with
    /// ours ± template drift would flip every healthy row to "changed".
    pub prompt_render_total: Option<i64>,
    /// Whether the render agrees with the recomputed prompt within tolerance. `None` when
    /// no render ran; `Some(false)` is a per-row finding for the operator, not a change.
    pub prompt_render_agrees: Option<bool>,
    /// How the completion figure was arrived at (`reported` / `estimated`). An `estimated`
    /// completion — a tokenizer count of extracted response text, used only when the body
    /// carries no usage — must not be applied without an explicit opt-in.
    pub completion_token_source: Option<String>,
}

/// A cache split re-derived from the prefix index, and whether it agrees with the row.
///
/// This is the strong form of verification the recompute exists to support: the counts come
/// from replaying the request through the serving classifier against a time-travelling
/// index, so agreement with the stored row means the billing pipeline was right — not merely
/// self-consistent.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ReconstructedCache {
    pub cache_read: i64,
    pub cache_creation_5m: i64,
    pub cache_creation_1h: i64,
    pub cache_creation_24h: i64,
    /// Exact chat-templated prompt total from `/v1/render`, when the classifier counted that
    /// way. The tokenizer's independent answer for the whole prompt.
    pub render_total: Option<i64>,
    /// `exact`, or `approximate` where an entry predates the episode-per-row cutover and its
    /// window over-approximates liveness (which under-counts creations).
    pub fidelity: String,
    /// Whether the re-derived split matches the split stored on the row.
    pub agrees_with_stored: bool,
    /// Whether `render_total` matches the stored prompt tokens. `None` when the classifier
    /// did not render.
    pub render_matches_prompt: Option<bool>,
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
    /// Rows whose cache split was independently re-derived from the prefix index.
    pub rows_cache_reconstructed: i64,
    /// Of those, how many disagreed with the split stored on the row. Non-zero is a finding:
    /// either the classifier was wrong at serving time, or the index no longer reflects it.
    pub rows_cache_disagreed: i64,
    /// Rows whose prompt was independently re-counted by `/v1/render`.
    pub rows_render_checked: i64,
    /// Of those, how many diverged beyond tolerance from the provider's count.
    pub rows_render_disagreed: i64,
    /// Usage-less rows rescued by the tokenizer (rendered prompt + estimated completion).
    /// These carry `completion_token_source: "estimated"` and are gated from auto-apply.
    pub rows_tokenizer_rescued: i64,
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
    /// The share of `net_correction` that rests on ESTIMATED completions (tokenizer
    /// rescues). Not exact by construction — the apply step must exclude these rows unless
    /// explicitly opted in, so the two figures are separated here rather than leaving the
    /// operator to discover the mix per row.
    #[schema(value_type = String)]
    pub net_correction_estimated: Decimal,
}

/// The document.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RecomputeReport {
    pub generated_at: DateTime<Utc>,
    /// The corpus predicate, echoed back so the file is self-describing months later.
    pub corpus: serde_json::Value,
    pub summary: ReportSummary,
    /// Conditions that limit what this report can be trusted for. Empty is the normal case.
    pub warnings: Vec<String>,
    pub rows: Vec<ReportRow>,
}

/// Days of `prompt_cache_entries` history the cache sweeper will retain.
///
/// Beyond this the entries backing a cache-split reconstruction are pruned, so a corpus older
/// than the grace cannot have its split re-derived — only carried through from the response,
/// or not recovered at all if the response never reported one.
pub const CACHE_GRACE_DAYS: i64 = 7;

/// Warnings that depend on the corpus rather than any individual row.
pub fn corpus_warnings(oldest: Option<DateTime<Utc>>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(oldest) = oldest {
        let age = (Utc::now() - oldest).num_days();
        if age > CACHE_GRACE_DAYS {
            out.push(format!(
                "corpus reaches {age} days back, beyond the {CACHE_GRACE_DAYS}-day cache retention grace: \
                 prompt_cache_entries for the older rows may already be pruned, so the cache split \
                 cannot be re-derived for them and the tier cannot be resolved from ttl_tier"
            ));
        }
    }
    out
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

impl ReconstructedCache {
    /// Compare a reconstructed split against what the row stores.
    pub fn compare(split: &super::cache_replay::ReconstructedSplit, row: &CorpusRow) -> Self {
        let (read, c5, c1, c24) = (
            split.read as i64,
            split.creation_5m as i64,
            split.creation_1h as i64,
            split.creation_24h as i64,
        );
        Self {
            cache_read: read,
            cache_creation_5m: c5,
            cache_creation_1h: c1,
            cache_creation_24h: c24,
            render_total: split.render_total.map(|t| t as i64),
            fidelity: match split.fidelity {
                super::cache_replay::Fidelity::Exact => "exact",
                super::cache_replay::Fidelity::Approximate => "approximate",
            }
            .to_string(),
            agrees_with_stored: read == row.stored_cache_read
                && c5 == row.stored_cache_creation_5m
                && c1 == row.stored_cache_creation_1h
                && c24 == row.stored_cache_creation_24h,
            render_matches_prompt: split.render_total.map(|t| t as i64 == row.stored_prompt_tokens),
        }
    }
}

impl ReportRow {
    /// A row that was replayed successfully.
    pub(crate) fn replayed(row: &CorpusRow, usage: &RecomputedUsage, cost: Option<Decimal>) -> Self {
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
            reconstructed_cache: None,
            prompt_render_total: None,
            prompt_render_agrees: None,
            completion_token_source: Some(usage.completion_source.as_str().to_string()),
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
            reconstructed_cache: None,
            prompt_render_total: None,
            prompt_render_agrees: None,
            completion_token_source: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_within_the_grace_warns_about_nothing() {
        let recent = Utc::now() - chrono::Duration::days(CACHE_GRACE_DAYS - 1);
        assert!(corpus_warnings(Some(recent)).is_empty());
        assert!(corpus_warnings(None).is_empty(), "an empty corpus has nothing to warn about");
    }

    /// Beyond the grace the cache entries backing a split reconstruction may already be
    /// pruned, so the report has to say so rather than silently reporting a split it could
    /// not have re-derived.
    #[test]
    fn corpus_beyond_the_grace_warns() {
        let old = Utc::now() - chrono::Duration::days(CACHE_GRACE_DAYS + 3);
        let warnings = corpus_warnings(Some(old));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("cache retention grace"), "got: {}", warnings[0]);
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
            if let Some(rc) = &r.reconstructed_cache {
                s.rows_cache_reconstructed += 1;
                if !rc.agrees_with_stored {
                    s.rows_cache_disagreed += 1;
                }
            }
            if let Some(agrees) = r.prompt_render_agrees {
                s.rows_render_checked += 1;
                if !agrees {
                    s.rows_render_disagreed += 1;
                }
            }
            if r.completion_token_source.as_deref() == Some("estimated") {
                s.rows_tokenizer_rescued += 1;
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
                    if r.completion_token_source.as_deref() == Some("estimated") {
                        s.net_correction_estimated += rec.cost.unwrap_or_default() - r.stored.cost.unwrap_or_default();
                    }
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
