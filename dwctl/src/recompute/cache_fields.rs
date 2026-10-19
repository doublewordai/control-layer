//! Reading the cache split out of a **raw upstream** response body.
//!
//! The live path ([`crate::request_logging::serializers::extract_cache_tokens`]) reads
//! cache *creation* only from the nested `cache_creation` object, and that is correct for
//! what it sees: dwctl's own cache layer writes both the flat total and the nested
//! per-tier breakdown together (`prompt_cache::inject`), and for a model dwctl does not
//! cache, `scrub_provider_cache_fields` strips the provider's fields entirely. So on the
//! serving path a body either has the nested object or has no cache fields at all.
//!
//! **A stored fusillade body is not that body.** It is what the upstream returned, and an
//! Anthropic-shaped upstream reports only the flat `cache_creation_input_tokens` — no
//! nested object, no `ephemeral_*` keys. Measured against the incident corpus: zero of the
//! 1,044 stored bodies carry a nested object, while the flat field holds real values.
//!
//! Reading those bodies with the nested-only parser therefore yields `creation = 0` and
//! reproduces half the very bug the recompute exists to repair. Hence this module: nested
//! wins when present (it carries the real tier split); the flat total is the fallback.
//!
//! The flat total says *how many* creation tokens there were but not *which tier* they
//! were written at, and the tiers price differently (5m ×1.25, 1h ×2.0, 24h ×2.5). That is
//! recorded as an inference, not a fact — see [`CacheReading::tier_inferred`].

use serde_json::Value;

use crate::request_logging::serializers::CacheTokens;

/// The TTL tier a flat creation total is attributed to when the body gives no breakdown.
///
/// Not a guess dressed as a default: 24h is disabled at server config and never emitted,
/// and the incident corpus's model ran 100% 5m-tier post-fix (43.4M 5m creation tokens,
/// zero 1h). 5m is also the cheapest write multiplier (×1.25 against ×2.0 and ×2.5), so
/// mis-attributing here under-states a correction rather than over-charging on a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CreationTier {
    #[default]
    FiveMinute,
    OneHour,
    TwentyFourHour,
}

/// A cache split read from a stored body, with how much of it was actually stated.
#[derive(Debug, Clone, Copy, Default)]
pub struct CacheReading {
    pub tokens: CacheTokens,
    /// True when creation came from the flat total and the tier was assigned by rule
    /// rather than read from the body. Surfaced per row in the report: the token count is
    /// solid, the *price* of those tokens rests on an assumption a human should see.
    pub tier_inferred: bool,
}

/// Read a `usage` object, preferring the nested per-tier breakdown and falling back to the
/// provider's flat total.
pub fn cache_tokens_both_shapes(usage: &Value, flat_tier: CreationTier) -> CacheReading {
    // Floor at 0 throughout: counts cannot be negative, but a malformed body could carry
    // one and it must never reach the cost maths.
    let read = usage.get("cache_read_input_tokens").and_then(Value::as_i64).unwrap_or(0).max(0);

    let nested = |k: &str| {
        usage
            .get("cache_creation")
            .and_then(|c| c.get(k))
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .max(0)
    };
    let (n5, n1, n24) = (
        nested("ephemeral_5m_input_tokens"),
        nested("ephemeral_1h_input_tokens"),
        nested("ephemeral_24h_input_tokens"),
    );

    // The nested object is authoritative whenever it carries anything: dwctl wrote it, and
    // it is the only source that states the tier split.
    if n5 + n1 + n24 > 0 {
        return CacheReading {
            tokens: CacheTokens {
                read,
                creation_5m: n5,
                creation_1h: n1,
                creation_24h: n24,
            },
            tier_inferred: false,
        };
    }

    let flat = usage.get("cache_creation_input_tokens").and_then(Value::as_i64).unwrap_or(0).max(0);

    if flat == 0 {
        // No creation from either shape — nothing inferred, this row simply had no writes.
        return CacheReading {
            tokens: CacheTokens {
                read,
                ..CacheTokens::default()
            },
            tier_inferred: false,
        };
    }

    let mut tokens = CacheTokens {
        read,
        ..CacheTokens::default()
    };
    match flat_tier {
        CreationTier::FiveMinute => tokens.creation_5m = flat,
        CreationTier::OneHour => tokens.creation_1h = flat,
        CreationTier::TwentyFourHour => tokens.creation_24h = flat,
    }
    CacheReading {
        tokens,
        tier_inferred: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The incident shape, from a real stored body (§2 of the handover): flat creation
    /// only, no nested object. The nested-only parser reads creation = 0 here, which is
    /// precisely the half of the bug a replay must not reproduce.
    #[test]
    fn flat_creation_is_recovered_and_marked_inferred() {
        let usage = json!({
            "input_tokens": 565,
            "output_tokens": 658,
            "cache_read_input_tokens": 17631,
            "cache_creation_input_tokens": 538
        });
        let got = cache_tokens_both_shapes(&usage, CreationTier::FiveMinute);
        assert_eq!(got.tokens.read, 17_631);
        assert_eq!(got.tokens.creation_5m, 538, "flat total recovered");
        assert_eq!(got.tokens.creation_1h, 0);
        assert!(got.tier_inferred, "tier was assigned by rule, not read");
    }

    /// When dwctl annotated the body itself, the nested object carries the real split and
    /// must win — including over a flat total that disagrees with it.
    #[test]
    fn nested_wins_over_flat_and_is_not_inferred() {
        let usage = json!({
            "cache_read_input_tokens": 12000,
            "cache_creation_input_tokens": 9999,
            "cache_creation": { "ephemeral_1h_input_tokens": 3000 }
        });
        let got = cache_tokens_both_shapes(&usage, CreationTier::FiveMinute);
        assert_eq!(got.tokens.creation_1h, 3_000);
        assert_eq!(got.tokens.creation_5m, 0);
        assert!(!got.tier_inferred, "the body stated the tier");
    }

    #[test]
    fn no_creation_at_all_is_not_flagged_as_inferred() {
        let usage = json!({ "cache_read_input_tokens": 12000 });
        let got = cache_tokens_both_shapes(&usage, CreationTier::FiveMinute);
        assert_eq!(got.tokens.read, 12_000);
        assert_eq!(got.tokens.creation_5m, 0);
        assert!(!got.tier_inferred, "nothing was inferred - there were no writes");
    }

    #[test]
    fn flat_total_honours_the_requested_tier() {
        let usage = json!({ "cache_creation_input_tokens": 500 });
        let bucket_5m: fn(CacheTokens) -> i64 = |c| c.creation_5m;
        for (tier, get) in [
            (CreationTier::FiveMinute, bucket_5m),
            (CreationTier::OneHour, |c: CacheTokens| c.creation_1h),
            (CreationTier::TwentyFourHour, |c: CacheTokens| c.creation_24h),
        ] {
            let got = cache_tokens_both_shapes(&usage, tier);
            assert_eq!(get(got.tokens), 500);
            assert!(got.tier_inferred);
        }
    }

    #[test]
    fn negative_counts_are_floored() {
        let usage = json!({
            "cache_read_input_tokens": -5,
            "cache_creation_input_tokens": -10
        });
        let got = cache_tokens_both_shapes(&usage, CreationTier::FiveMinute);
        assert_eq!(got.tokens.read, 0);
        assert_eq!(got.tokens.creation_5m, 0);
        assert!(!got.tier_inferred, "a floored-to-zero total infers nothing");
    }
}
