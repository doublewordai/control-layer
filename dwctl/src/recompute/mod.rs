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
//!   Anthropic's shape, but was recorded verbatim as `prompt_tokens`. Billing derives
//!   uncached input as `prompt − cached`, so the cached tokens were subtracted twice and
//!   billed at nothing (3.28M prompt tokens recorded against 19.34M processed).
//!
//! Both were fixed going forward. Neither fix repairs the rows already written, and both
//! times that repair was a hand-rolled scripting exercise against production. This module
//! is that repair, productised.
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
//! # The one thing a recompute cannot do
//!
//! It cannot recalculate the **cache split** (`cache_read_input_tokens` and the three
//! `cache_creation_*` tiers). A tokenizer returns a total; it cannot say how much of that
//! total was a cache *read* (billed at the ~0.1x read multiplier) versus a cache *write*
//! (1.25–2.5x). The state that decided this lived in `prompt_cache_entries`, which is a
//! cache and not a ledger: rows are upserted in place under a UNIQUE key, `expires_at`
//! slides forward on every read, and entries age out on a 5m/1h window. By the time
//! anyone is remediating, the evidence is gone.
//!
//! So the split is **carried through from the stored response, never invented**. If a
//! future incident corrupts the split itself rather than the total, this module cannot
//! repair it and the remediation is manual. That limitation is accepted and deliberate;
//! it does not block the incidents above, where the split was recorded correctly and only
//! the total was wrong.

// The engine lands before its callers: the dry-run job, the API handlers and the persisted
// run/item tables are the next commits on this branch. Until those exist the only consumers
// are this module's tests, so the whole surface reads as dead. Remove this attribute once
// `recompute::job` is wired into `crate::tasks`.
#![allow(dead_code)]

use crate::pricing::TokenCounts;
use crate::request_logging::serializers::{TokenMetrics, extract_cache_tokens, parse_ai_response};
use outlet::{RequestData, ResponseData};

pub mod replay;

use replay::RecomputeError;

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
    /// How `counts.prompt` was arrived at.
    pub prompt_source: TokenSource,
    /// How `counts.completion` was arrived at.
    pub completion_source: TokenSource,
    /// Set when the render and the reported total disagreed beyond tolerance.
    pub disagreement: Option<Disagreement>,
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
/// The cache split comes from [`extract_cache_tokens`], unchanged. See the module docs for
/// why that is not negotiable.
pub fn recompute_from_stored_response(request_data: &RequestData, response_data: &ResponseData) -> Result<RecomputedUsage, RecomputeError> {
    let parsed = parse_ai_response(request_data, response_data).map_err(RecomputeError::Parse)?;
    let metrics = TokenMetrics::from(&parsed);
    let cache = extract_cache_tokens(response_data);

    Ok(RecomputedUsage {
        counts: TokenCounts {
            prompt: metrics.prompt_tokens,
            completion: metrics.completion_tokens,
            cache_read: cache.read,
            cache_creation_5m: cache.creation_5m,
            cache_creation_1h: cache.creation_1h,
            cache_creation_24h: cache.creation_24h,
        },
        prompt_source: TokenSource::Reported,
        completion_source: TokenSource::Reported,
        disagreement: None,
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
            prompt_source: TokenSource::Reported,
            completion_source: TokenSource::Reported,
            disagreement: None,
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
