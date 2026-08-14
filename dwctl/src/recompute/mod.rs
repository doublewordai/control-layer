//! Recomputing the usage metrics of requests that were already billed.
//!
//! # Why this exists
//!
//! Twice now a bug has written wrong token counts into `http_analytics`, and therefore
//! wrong charges into `credits_transactions`:
//!
//! - **GLM-5.2, July 2026** — a self-hosted backend returned `"usage":null` on every
//!   non-`stop` finish reason. The affected requests were billed nothing at all, because
//!   `credits_transactions.amount` carries a `> 0` CHECK, so there was not even a row to
//!   correct.
//! - **Anthropic `/v1/messages`, August 2026** — `input_tokens` excludes cached tokens in
//!   Anthropic's shape, but was recorded verbatim as `prompt_tokens`, and cache creation
//!   was dropped entirely. Billing derives uncached input as `prompt − cached`, so the
//!   cached tokens were subtracted twice and billed at nothing. Measured: 1,044 rows,
//!   prompt sum 5,086,592 against 32,401,267 actually processed — a 6.4× undercount.
//!
//! Both were fixed going forward. Neither fix repairs the rows already written, and both
//! times that repair was a hand-rolled scripting exercise against production. This module
//! is the arithmetic half of that repair, productised.
//!
//! # This module never writes
//!
//! It computes what a request's usage *should* have been and reports it. It does not amend
//! `http_analytics`, does not touch `credits_transactions`, and has no database access at
//! all. Applying a correction is a sequence of reviewed SQL steps run by a human, covered
//! by the internal runbook `usage-recompute-repair`: amend the canonical analytics row,
//! append compensating ledger rows, heal the balance checkpoints, patch the daily usage
//! rollup (which has a forward-only cursor and so never re-reads an amended row), and —
//! for a batch corpus only — delta-fold the batch aggregate's analytics columns.
//!
//! That split is deliberate. The arithmetic is fiddly, testable and worth automating; the
//! money is neither. It also means the questions a write path would have to answer
//! prematurely — whether a debit correction should trip auto-topup, how the warehouse
//! handles an analytics UPDATE — get answered by a person at the time, with the specific
//! corpus in front of them.
//!
//! The report is a **downloadable JSON document, not a database table**. There is no run
//! history to persist, migrate or garbage-collect: the operator saves the file, attaches it
//! to the incident, and feeds it to the repair scripts. It is also the only thing that has
//! to carry state, because every later step derives from the database — step 1 amends
//! `http_analytics` from the JSON, and billing then derives from the amended analytics
//! (`credits_transactions.source_id` is the analytics row id), and the balance derives from
//! the ledger. One document in, a chain of pure derivations after it.
//!
//! # Everything is recomputed; nothing is assumed still correct
//!
//! The engine re-derives **every** usage figure the row stores — prompt, completion,
//! reasoning, total, and the cache split — not just the ones a given incident is known to
//! have broken. A field that comes back identical is then a *measurement* that it was fine,
//! reported as a zero delta, rather than an assumption nobody checked.
//!
//! That distinction matters more than it sounds. In the August incident the completion
//! counts happened to be correct on all 1,044 rows, and it would have been tempting to
//! hard-code "completion is fine, skip it". The next incident will break a different field —
//! the July one broke *everything*, because the response carried no usage at all — and a
//! recompute that only looks where the last bug was is a recompute that misses the next one.
//!
//! Two findings from that work belong here, because whatever eventually reports or applies
//! these corrections has to honour them:
//!
//! - **A report must carry `fusillade_batch_id` per row.** A batch corpus needs its
//!   `batch_aggregates` analytics columns delta-folded to stay consistent with the amended
//!   analytics rows, and an operator cannot know that from the recomputed numbers alone.
//! - **A correction for a batched request must NOT carry that batch id into the ledger.**
//!   The transactions list shows batched spend only via `batch_aggregates`, and its
//!   individual-transaction arm filters `fusillade_batch_id IS NULL` — so a batch-tagged
//!   correction is charged to the customer and visible nowhere. Pinned by
//!   `db::handlers::credits::tests::batched_transaction_is_invisible_without_an_aggregate_row`.
//!
//! # How a recompute stays honest
//!
//! The engine does not re-derive counts with logic of its own. It **replays the stored
//! request/response payload through the very same serializer the live path uses**
//! ([`crate::request_logging::serializers`]) and prices it through the very same
//! arithmetic ([`crate::pricing`]).
//!
//! That is a deliberate structural choice, and it buys the property the whole feature
//! rests on: **a recompute of healthy traffic is a guaranteed no-op**. Any delta it
//! reports is a genuine change in what dwctl believes about that request — a bug that has
//! since been fixed — rather than a second opinion from a parallel implementation that
//! could itself be wrong.
//!
//! # The cache split
//!
//! The split is carried through from the stored response. Where the response reported it
//! correctly — both incidents so far — re-deriving a number already held exactly can only
//! introduce error.
//!
//! It is reconstructable when it has to be: recompute the request's prefix hashes
//! (deterministic from the request body and the scope
//! `(principal_id, virtual_model, tokenizer_version)`), then for each breakpoint ask whether
//! an episode covering that hash was live at the request's timestamp. Live means a cache
//! read; not live means a creation.
//!
//! Two bounds on that, both of which belong in any report built on it:
//!
//! - **Retention.** `prompt_cache_entries` currently retains expired rows, so all history is
//!   available. Once the sweeper ships with its grace period (7 days), anything older cannot
//!   be reconstructed.
//! - **Accuracy before the episode-per-row cutover.** A post-expiry write revives the same
//!   row in place and keeps the original `created_at`, so `[created_at, expires_at]` can
//!   contain dead gaps and over-approximates liveness. Answers for that period classify some
//!   creations as reads, which under-bills, and must be labelled approximate.

use crate::pricing::TokenCounts;
use crate::request_logging::serializers::{TokenMetrics, extract_from_last_usage, parse_ai_response};
use outlet::{RequestData, ResponseData};

pub mod cache_fields;
pub mod cache_replay;
pub mod replay;
pub mod report;
pub mod source;

use cache_fields::{CacheReading, CreationTier, cache_tokens_both_shapes};
use replay::RecomputeError;

/// Recompute a whole corpus and build the report.
///
/// Read-only by construction: it takes a `&PgPool` it only ever `SELECT`s through, and the
/// report it returns is a document. Nothing here writes, and nothing it returns is applied
/// automatically — see the module docs.
#[tracing::instrument(skip(pool, filter, classifier), fields(limit = filter.limit))]
pub async fn recompute_corpus(
    pool: &sqlx::PgPool,
    filter: &source::CorpusFilter,
    flat_tier: CreationTier,
    classifier: Option<&crate::prompt_cache::Classifier>,
) -> Result<report::RecomputeReport, sqlx::Error> {
    let corpus = source::load_corpus(pool, filter).await?;

    let mut rows: Vec<report::ReportRow> = corpus
        .iter()
        .map(|row| {
            let Some(exchange) = &row.exchange else {
                // No body at all. For a ZDR account this is permanent and expected; the
                // tokens simply cannot be verified, only the dollars-from-stored-tokens
                // check remains available.
                return report::ReportRow::unreplayable(
                    row,
                    report::Evidence::ColumnsOnly,
                    "no stored request/response body for this request".to_string(),
                );
            };

            let replayed = exchange
                .to_outlet_pair()
                .and_then(|(req, resp)| recompute_from_stored_response(&req, &resp, flat_tier));

            match replayed {
                Ok(usage) => {
                    // Price with the rates resolved at inference time, carried on the row.
                    // Re-resolving the tariff here would silently re-base a correction onto
                    // pricing that changed after the request was served.
                    let cost = crate::pricing::charged_cost(
                        &usage.counts,
                        row.model.as_deref(),
                        row.input_price_per_token,
                        row.output_price_per_token,
                        cache_multipliers_for(row),
                        crate::metrics::errors::component::ANALYTICS,
                    );
                    report::ReportRow::replayed(row, &usage, cost)
                }
                Err(e) => report::ReportRow::unreplayable(row, report::Evidence::NotReplayable, e.to_string()),
            }
        })
        .collect();

    // Independently re-derive each row's cache split from the prefix index as it stood at
    // the request's timestamp. This is what turns the report from "the response and the row
    // agree" into "the billing pipeline was right": the counts come from replaying the
    // request through the serving classifier, not from anything the response said.
    if let Some(base) = classifier {
        for (report_row, row) in rows.iter_mut().zip(corpus.iter()) {
            let (Some(principal), Some(model), Some(exchange)) = (row.user_id, row.model.as_deref(), row.exchange.as_ref()) else {
                continue;
            };
            let Some(body) = exchange.request_body.as_deref() else { continue };

            let historical = base.with_index(std::sync::Arc::new(cache_replay::HistoricalIndex::new(pool.clone(), row.timestamp)));
            match cache_replay::reconstruct_split(&historical, model, body, principal, row.timestamp).await {
                Ok(Some(split)) => {
                    report_row.reconstructed_cache = Some(report::ReconstructedCache::compare(&split, row));
                }
                // Not cache-enabled for this principal: no split to reconstruct, which is not
                // the same as a split of zero, so the field stays absent.
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!(analytics_id = row.analytics_id, error = %e, "cache split reconstruction failed");
                }
            }
        }
    }

    let oldest = corpus.iter().map(|r| r.timestamp).min();

    Ok(report::RecomputeReport {
        generated_at: chrono::Utc::now(),
        warnings: report::corpus_warnings(oldest),
        corpus: serde_json::json!({
            "start": filter.start,
            "end": filter.end,
            "user_id": filter.user_id,
            "uri_pattern": filter.uri_pattern,
            "model": filter.model,
            "limit": filter.limit,
        }),
        summary: report::ReportSummary::of(&rows),
        rows,
    })
}

/// Whether dwctl's cache discount applied to this request, inferred from the row itself.
///
/// The live path gates the discount on a `model_cache_tariffs` row valid at inference time.
/// A recompute cannot re-resolve that without risking a tariff that changed since, so it
/// uses the evidence already on the row: a request that recorded cache tokens was, by
/// definition, priced by a cache-enabled model at the time. One that recorded none gets no
/// multipliers, which is exactly the "provider's own caching, not ours" case.
fn cache_multipliers_for(row: &source::CorpusRow) -> Option<crate::pricing::CacheMultipliers> {
    (row.stored_cache_total() > 0).then(crate::pricing::CacheMultipliers::default)
}

/// Where a recomputed figure came from, and therefore how much it can be trusted.
///
/// Carried per-figure (prompt and completion are sourced independently) and surfaced in
/// the dry-run report, because an apply must be able to refuse the approximate ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// Re-read from the response's own `usage`, through the current serializer. Exact:
    /// the provider told us, we are simply reading it with semantics that are now correct.
    /// This is what repairs an ingress-semantics bug like the Anthropic one.
    Reported,
    /// Counted by tokenizer-svc's `/v1/render` — the exact chat-templated token count, the
    /// same bytes the engine tokenizes. Exact, but only available for models the service
    /// has a template baked for.
    Rendered,
    /// Reconstructed with a per-finish-reason overhead constant. **Approximate.** This is
    /// the only option when the response carried no usage at all, and it is why an apply
    /// must not write these without an explicit opt-in.
    Estimated,
}

impl TokenSource {
    /// Whether a figure from this source is exact enough to apply without an explicit
    /// operator opt-in.
    pub fn is_exact(&self) -> bool {
        matches!(self, Self::Reported | Self::Rendered)
    }

    /// The stable string written to `usage_recompute_items.token_source`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Reported => "reported",
            Self::Rendered => "rendered",
            Self::Estimated => "estimated",
        }
    }
}

/// Two independent readings of a request's prompt size that did not agree.
///
/// Surfaced rather than silently resolved: when the tokenizer's render and the response's
/// own arithmetic disagree, one of them is wrong about this request and an operator should
/// see both before anything is billed on the strength of either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disagreement {
    /// What the response's `usage` implies the total input was.
    pub reported_prompt: i64,
    /// What tokenizer-svc's `/v1/render` counted.
    pub rendered_prompt: i64,
}

impl Disagreement {
    /// The gap as a fraction of the larger reading, in basis points — the shape the
    /// tolerance is expressed in, so a 1% drift on a 20k prompt and on a 200-token prompt
    /// are judged the same way.
    pub fn divergence_bps(&self) -> i64 {
        let hi = self.reported_prompt.max(self.rendered_prompt).max(1);
        let gap = (self.reported_prompt - self.rendered_prompt).abs();
        gap.saturating_mul(10_000) / hi
    }
}

/// The result of recomputing one request.
#[derive(Debug, Clone)]
pub struct RecomputedUsage {
    /// The counts to bill on, in the shape [`crate::pricing`] prices.
    pub counts: TokenCounts,
    /// Reasoning tokens, re-read from the response. A *subset* of `counts.completion` rather
    /// than an addition to it, so it does not affect price — but `http_analytics` stores it
    /// as its own column, and a repair that leaves it stale makes the row internally
    /// inconsistent. Zero for shapes with no such concept (Anthropic folds thinking into
    /// `output_tokens` and reports no separate count).
    pub reasoning: i64,
    /// Total tokens as the response reports them, or `prompt + completion` where the shape
    /// carries no total (Anthropic). Stored on its own column in `http_analytics`, so it is
    /// recomputed rather than assumed to still follow from the other two.
    pub total: i64,
    /// How `counts.prompt` was arrived at.
    pub prompt_source: TokenSource,
    /// How `counts.completion` was arrived at.
    pub completion_source: TokenSource,
    /// Set when the render and the reported total disagreed beyond tolerance.
    pub disagreement: Option<Disagreement>,
    /// True when the cache-creation tier was assigned by rule because the stored body gave
    /// only a flat total. The token count is solid; the price of those tokens rests on an
    /// assumption, so the report must show it rather than bury it.
    pub cache_tier_inferred: bool,
    /// `response_type` as the serializer classified it, for the report.
    pub response_type: String,
    /// The model the response claims, when it states one.
    pub response_model: Option<String>,
}

impl RecomputedUsage {
    /// Whether every figure here is exact enough to apply without an opt-in.
    pub fn is_exact(&self) -> bool {
        self.prompt_source.is_exact() && self.completion_source.is_exact()
    }
}

/// Recompute a request's usage by replaying its stored payload through the live path's
/// own serializer.
///
/// This is the [`TokenSource::Reported`] path: no tokenizer, no reconstruction, just the
/// current reading of what the provider actually said. It is exact, and it is what repairs
/// the Anthropic class of bug — where the response held the right numbers all along and
/// only our interpretation of them was wrong.
///
/// The cache split is read from the stored body and never invented. It goes through
/// [`cache_tokens_both_shapes`] rather than the live path's nested-only parser, because a
/// stored fusillade body is the *upstream's* response and an Anthropic-shaped upstream
/// reports creation as a flat total — see [`cache_fields`] for why that distinction is the
/// difference between repairing the incident and reproducing half of it.
pub fn recompute_from_stored_response(
    request_data: &RequestData,
    response_data: &ResponseData,
    flat_tier: CreationTier,
) -> Result<RecomputedUsage, RecomputeError> {
    let parsed = parse_ai_response(request_data, response_data).map_err(RecomputeError::Parse)?;
    let metrics = TokenMetrics::from(&parsed);
    let CacheReading {
        tokens: cache,
        tier_inferred,
    } = extract_from_last_usage(response_data, |usage| cache_tokens_both_shapes(usage, flat_tier));

    Ok(RecomputedUsage {
        counts: TokenCounts {
            prompt: metrics.prompt_tokens,
            completion: metrics.completion_tokens,
            cache_read: cache.read,
            cache_creation_5m: cache.creation_5m,
            cache_creation_1h: cache.creation_1h,
            cache_creation_24h: cache.creation_24h,
        },
        reasoning: metrics.reasoning_tokens,
        total: metrics.total_tokens,
        prompt_source: TokenSource::Reported,
        completion_source: TokenSource::Reported,
        disagreement: None,
        cache_tier_inferred: tier_inferred,
        response_type: metrics.response_type,
        response_model: metrics.response_model,
    })
}

/// Fold a tokenizer-svc render into a recomputed reading.
///
/// Precedence, and why:
///
/// - The response reported nothing usable (`prompt == 0`) — the GLM-5.2 shape. The render
///   is the only source there is, so take it. The completion side has no such rescue and
///   stays whatever the caller marked it.
/// - The two agree within `tolerance_bps` — take the render (it counts the exact templated
///   bytes the engine saw) and mark it [`TokenSource::Rendered`].
/// - They disagree beyond tolerance — keep the **reported** figure and record the
///   [`Disagreement`]. Deliberately conservative: the reported number is what the provider
///   billed us on, so an operator reviews the conflict rather than the tool silently
///   re-pricing a request on a contested count.
///
/// The cache split is untouched in every branch.
///
// Not yet reachable: the corpus recompute reads usage from the stored response, which is
// exact and covers both incidents seen so far. This is the entry point for the tokenizer
// path — needed when a response carries no usage at all (the July shape) — and is kept with
// its tests because the precedence rules it encodes are the hard part, not the plumbing.
#[allow(dead_code)]
pub fn fold_render(mut usage: RecomputedUsage, rendered_prompt: i64, tolerance_bps: i64) -> RecomputedUsage {
    let reported_prompt = usage.counts.prompt;

    if reported_prompt <= 0 {
        usage.counts.prompt = rendered_prompt;
        usage.prompt_source = TokenSource::Rendered;
        return usage;
    }

    let disagreement = Disagreement {
        reported_prompt,
        rendered_prompt,
    };
    if disagreement.divergence_bps() <= tolerance_bps {
        usage.counts.prompt = rendered_prompt;
        usage.prompt_source = TokenSource::Rendered;
    } else {
        usage.disagreement = Some(disagreement);
    }
    usage
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(prompt: i64, read: i64, c1h: i64) -> RecomputedUsage {
        RecomputedUsage {
            counts: TokenCounts {
                prompt,
                completion: 50,
                cache_read: read,
                cache_creation_5m: 0,
                cache_creation_1h: c1h,
                cache_creation_24h: 0,
            },
            reasoning: 0,
            total: prompt + 50,
            prompt_source: TokenSource::Reported,
            completion_source: TokenSource::Reported,
            disagreement: None,
            cache_tier_inferred: false,
            response_type: "chat_completion".to_string(),
            response_model: None,
        }
    }

    #[test]
    fn render_is_taken_when_it_agrees_within_tolerance() {
        // 20_000 vs 20_100 = 50 bps of drift, inside a 100 bps tolerance.
        let got = fold_render(usage(20_000, 12_000, 3_000), 20_100, 100);
        assert_eq!(got.counts.prompt, 20_100, "the exact templated count wins");
        assert_eq!(got.prompt_source, TokenSource::Rendered);
        assert!(got.disagreement.is_none());
    }

    #[test]
    fn disagreement_keeps_the_reported_figure_and_is_surfaced() {
        // A 50% gap is not drift. Keep what the provider billed us on and escalate.
        let got = fold_render(usage(20_000, 12_000, 3_000), 10_000, 100);
        assert_eq!(got.counts.prompt, 20_000, "a contested count is not silently re-priced");
        assert_eq!(got.prompt_source, TokenSource::Reported);
        let d = got.disagreement.expect("the conflict must be surfaced");
        assert_eq!(d.reported_prompt, 20_000);
        assert_eq!(d.rendered_prompt, 10_000);
        assert_eq!(d.divergence_bps(), 5_000);
    }

    #[test]
    fn render_rescues_a_response_that_reported_no_usage() {
        // The GLM-5.2 shape: usage was null, so the row was billed as zero tokens. The
        // render is the only source of truth available.
        let got = fold_render(usage(0, 0, 0), 4_096, 100);
        assert_eq!(got.counts.prompt, 4_096);
        assert_eq!(got.prompt_source, TokenSource::Rendered);
        assert!(got.disagreement.is_none(), "there was nothing to disagree with");
    }

    /// The invariant the whole design turns on: whatever the tokenizer says, the cache
    /// split is carried through untouched. Inventing one would put a fabricated discount
    /// rate on a customer's invoice.
    #[test]
    fn folding_a_render_never_touches_the_cache_split() {
        let before = usage(20_000, 12_000, 3_000);
        for (rendered, tolerance) in [(20_100, 100), (10_000, 100), (0, 100)] {
            let after = fold_render(before.clone(), rendered, tolerance);
            assert_eq!(after.counts.cache_read, before.counts.cache_read);
            assert_eq!(after.counts.cache_creation_5m, before.counts.cache_creation_5m);
            assert_eq!(after.counts.cache_creation_1h, before.counts.cache_creation_1h);
            assert_eq!(after.counts.cache_creation_24h, before.counts.cache_creation_24h);
        }
    }

    #[test]
    fn divergence_is_scale_free() {
        // 1% drift reads as 100 bps whether the prompt is 200 tokens or 20k.
        let small = Disagreement {
            reported_prompt: 200,
            rendered_prompt: 198,
        };
        let large = Disagreement {
            reported_prompt: 20_000,
            rendered_prompt: 19_800,
        };
        assert_eq!(small.divergence_bps(), 100);
        assert_eq!(large.divergence_bps(), 100);
    }

    #[test]
    fn divergence_handles_zero_without_dividing_by_it() {
        let d = Disagreement {
            reported_prompt: 0,
            rendered_prompt: 0,
        };
        assert_eq!(d.divergence_bps(), 0);
    }

    #[test]
    fn only_exact_sources_are_applicable_without_opt_in() {
        assert!(TokenSource::Reported.is_exact());
        assert!(TokenSource::Rendered.is_exact());
        assert!(!TokenSource::Estimated.is_exact(), "estimates must not auto-apply");

        let mut u = usage(100, 0, 0);
        assert!(u.is_exact());
        u.completion_source = TokenSource::Estimated;
        assert!(!u.is_exact(), "one estimated figure taints the whole row");
    }
}
