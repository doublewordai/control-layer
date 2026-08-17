//! Shared request-pricing arithmetic.
//!
//! This is the single source of the cost maths for a single request: list price, the
//! cache-adjusted charged cost, the cache-split safety rules, and tariff resolution
//! as-of a point in time.
//!
//! It lives outside [`crate::request_logging::batcher`] because two callers need it and
//! they must not drift:
//!
//! - the **live path** ([`crate::request_logging::batcher`]), pricing a request as it
//!   completes, and
//! - the **recompute path** ([`crate::recompute`]), re-pricing a historical request from
//!   its stored payload when a bug wrote the wrong token counts.
//!
//! A recompute that priced differently from the live path would produce a "correction"
//! that is itself wrong, so the arithmetic is deliberately shared rather than
//! re-implemented. The input is [`TokenCounts`] — a narrow struct of just the counts that
//! affect price — so neither caller has to own the other's record type.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::config::CachePricingConfig;
use crate::db::models::api_keys::ApiKeyPurpose;

/// The token counts that determine a request's price.
///
/// `prompt` is the TOTAL input, including any cached portion — that is what
/// `prompt_tokens` means everywhere in dwctl. The `cache_*` fields break out the cached
/// share of that same total; they are not additional tokens on top of it. (An ingress
/// whose provider reports input EXCLUDING cache must add the cache buckets back before
/// constructing this — see the Anthropic branch of
/// `request_logging::serializers::TokenMetrics`, where getting this wrong billed the
/// cached tokens at nothing.)
///
/// The split usually reconciles to `prompt` but is not guaranteed to; see
/// [`compute_total_cost`] for the two-tier safety rule that handles drift and corruption.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TokenCounts {
    pub prompt: i64,
    pub completion: i64,
    pub cache_read: i64,
    pub cache_creation_5m: i64,
    pub cache_creation_1h: i64,
    pub cache_creation_24h: i64,
}

/// A `model_cache_tariffs` row (per model, per tier), with its validity window so batch
/// requests can be priced as of their creation time (mirrors `model_tariffs` handling).
/// One `model_cache_tariffs` version: all three tiers in a single row, plus the validity
/// window so a batch request prices as of its creation time. Completeness is guaranteed by
/// the schema (every multiplier is NOT NULL), so there is no missing-tier case to default.
#[derive(Clone)]
pub(crate) struct CacheTariffRow {
    pub write_multiplier_5m: Decimal,
    pub write_multiplier_1h: Decimal,
    pub write_multiplier_24h: Decimal,
    pub read_multiplier: Decimal,
    pub valid_from: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,
}

/// The cache multipliers resolved for one request at a point in time.
#[derive(Clone, Copy)]
pub(crate) struct CacheMultipliers {
    pub read: Decimal,
    pub write_5m: Decimal,
    pub write_1h: Decimal,
    pub write_24h: Decimal,
}

impl CacheMultipliers {
    /// The operator-configured defaults ([`CachePricingConfig`]) — the same values a freshly
    /// enabled tariff would get. Used as the fallback when a request carries cache tokens with
    /// no tariff valid at its time (unreachable in practice — classify gates on an active row,
    /// and the call site emits a `cache_tariff_missing` background error if it ever happens).
    pub fn from_config(c: &CachePricingConfig) -> Self {
        Self {
            read: c.default_read_multiplier,
            write_5m: c.default_write_multiplier_5m,
            write_1h: c.default_write_multiplier_1h,
            write_24h: c.default_write_multiplier_24h,
        }
    }
}

impl Default for CacheMultipliers {
    /// Mirrors the shipped [`CachePricingConfig`] defaults (read 0.1, writes 1.25/2.0/2.5) so
    /// the hardcoded default and the config defaults can't drift. Production reads the live
    /// config via [`CacheMultipliers::from_config`]; this is for tests/standalone callers.
    fn default() -> Self {
        Self::from_config(&CachePricingConfig::default())
    }
}

/// Model info with tariffs.
#[derive(Debug)]
pub(crate) struct ModelInfo {
    pub provider_name: String,
    pub tariffs: Vec<TariffInfo>,
}

/// Tariff info for pricing lookup.
#[derive(Debug)]
pub(crate) struct TariffInfo {
    pub purpose: ApiKeyPurpose,
    pub effective_from: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,
    pub input_price_per_token: Decimal,
    pub output_price_per_token: Decimal,
    pub completion_window: Option<String>,
}

/// Resolve the multipliers from the model's cache-tariff row valid at `timestamp` — the
/// most-recently-effective version still in its window. One row carries all tiers, so
/// there is no per-tier resolution or gap. `None` when no version was valid at `timestamp`
/// (so the caller can distinguish "no tariff" from a real row and fall back deliberately).
pub(crate) fn resolve_cache_multipliers(rows: &[CacheTariffRow], timestamp: DateTime<Utc>) -> Option<CacheMultipliers> {
    rows.iter()
        .filter(|r| r.valid_from <= timestamp && r.valid_until.is_none_or(|u| u > timestamp))
        .max_by_key(|r| r.valid_from)
        .map(|r| CacheMultipliers {
            read: r.read_multiplier,
            write_5m: r.write_multiplier_5m,
            write_1h: r.write_multiplier_1h,
            write_24h: r.write_multiplier_24h,
        })
}

/// Find the best matching tariff for a request.
///
/// Implements fallback logic:
/// 1. Try exact match (purpose + completion_window + timestamp)
/// 2. Fall back to generic tariff for that purpose (completion_window = None)
/// 3. Fall back to realtime purpose (generic)
pub(crate) fn find_best_tariff(
    tariffs: &[TariffInfo],
    api_key_purpose: Option<&ApiKeyPurpose>,
    completion_window: Option<&str>,
    timestamp: DateTime<Utc>,
) -> (Option<Decimal>, Option<Decimal>) {
    let purpose = api_key_purpose.unwrap_or(&ApiKeyPurpose::Realtime);

    // Filter tariffs valid at timestamp:
    // effective_from <= timestamp AND (valid_until IS NULL OR valid_until > timestamp)
    let valid_tariffs: Vec<_> = tariffs
        .iter()
        .filter(|t| t.effective_from <= timestamp && t.valid_until.is_none_or(|valid_until| valid_until > timestamp))
        .collect();

    // Try exact match with completion_window (for batch tariffs with specific priority)
    if let Some(cw) = completion_window
        && let Some(tariff) = valid_tariffs
            .iter()
            .find(|t| &t.purpose == purpose && t.completion_window.as_deref() == Some(cw))
    {
        return (Some(tariff.input_price_per_token), Some(tariff.output_price_per_token));
    }

    // Try generic tariff for this purpose (completion_window = None)
    // This ensures we don't accidentally match a different priority tier
    if let Some(tariff) = valid_tariffs
        .iter()
        .find(|t| &t.purpose == purpose && t.completion_window.is_none())
    {
        return (Some(tariff.input_price_per_token), Some(tariff.output_price_per_token));
    }

    // Fall back to generic realtime tariff
    if purpose != &ApiKeyPurpose::Realtime
        && let Some(tariff) = valid_tariffs
            .iter()
            .find(|t| t.purpose == ApiKeyPurpose::Realtime && t.completion_window.is_none())
    {
        return (Some(tariff.input_price_per_token), Some(tariff.output_price_per_token));
    }

    (None, None)
}

/// The charged cost for a request, gating the cache discount on dwctl enablement: when a
/// tariff was valid at inference (`cache_mults` is `Some`) apply the cache-adjusted pricing;
/// otherwise bill the full input at list price. The `None` case deliberately ignores any
/// cache_* tokens in the response — without an active tariff those are the upstream
/// provider's own caching, not dwctl's, and must not earn dwctl's discount.
///
/// `component` labels any anomaly emitted by [`compute_total_cost`], so the live path and a
/// recompute are distinguishable on dashboards rather than both reporting as the batcher.
pub(crate) fn charged_cost(
    counts: &TokenCounts,
    model: Option<&str>,
    input_price: Option<Decimal>,
    output_price: Option<Decimal>,
    cache_mults: Option<CacheMultipliers>,
    component: &'static str,
) -> Option<Decimal> {
    match cache_mults {
        Some(m) => compute_total_cost(counts, model, input_price, output_price, &m, component),
        None => list_price(counts.prompt, counts.completion, input_price, output_price),
    }
}

/// The cache-adjusted request cost. Reduces to the plain
/// `prompt × input + completion × output` when there are no cache tokens, so non-cache
/// requests are unaffected. `None` when the model has no pricing at all (→ no ledger row),
/// matching the old generated `total_cost`'s NULL.
pub(crate) fn compute_total_cost(
    counts: &TokenCounts,
    model: Option<&str>,
    input_price: Option<Decimal>,
    output_price: Option<Decimal>,
    m: &CacheMultipliers,
    component: &'static str,
) -> Option<Decimal> {
    if input_price.is_none() && output_price.is_none() {
        return None;
    }
    let inp = input_price.unwrap_or(Decimal::ZERO);
    let outp = output_price.unwrap_or(Decimal::ZERO);

    let mut read = Decimal::from(counts.cache_read.max(0));
    let c5 = Decimal::from(counts.cache_creation_5m.max(0));
    let c1 = Decimal::from(counts.cache_creation_1h.max(0));
    let c24 = Decimal::from(counts.cache_creation_24h.max(0));
    let prompt = Decimal::from(counts.prompt.max(0));
    let creations = c5 + c1 + c24;

    // Billing safety, two tiers. The classifier's tokenizer counts the request CONTENT
    // while the engine's prompt_tokens counts its chat-template rendering, so the two
    // legitimately disagree by a small margin — on fully-marked prompts (agent traffic)
    // the classifier's sum routinely lands a percent or so ABOVE prompt_tokens. That is
    // drift, not corruption: CAP the split to the prompt by removing the excess from the
    // read count (the cheapest-rate bucket — deterministic and audit-simple; keeping the
    // premium-billed write buckets intact means the reduction lands where it lowers the
    // bill the least, so the cap is conservative: it slightly favors the house, never the
    // reverse).
    //
    // Only when the WRITE counts alone exceed the whole prompt is the split genuinely
    // corrupt (writes bill at a premium, so trusting them could overcharge): distrust it
    // entirely and bill the input at base rate, loudly.
    if creations > prompt {
        crate::background_error!(
            component,
            "cache_split_exceeds_prompt",
            Warning,
            model = model.unwrap_or("?"),
            prompt_tokens = counts.prompt,
            "cache write counts alone exceed prompt_tokens; ignoring the split and billing at base rate"
        );
        return list_price(counts.prompt, counts.completion, input_price, output_price);
    }
    if read + creations > prompt {
        metrics::counter!("dwctl_cache_split_capped_total").increment(1);
        tracing::debug!(
            model = model.unwrap_or("?"),
            prompt_tokens = counts.prompt,
            overrun = %(read + creations - prompt),
            "cached token split exceeds prompt_tokens; capping the read count to fit"
        );
        read = prompt - creations;
    }
    let cached_total = read + creations;

    // Uncached = full input minus the cached portion, floored at zero (our tokenizer and
    // the provider's can differ; never let the cached count drive uncached negative).
    let uncached = (prompt - cached_total).max(Decimal::ZERO);

    let input_cost = uncached * inp + read * inp * m.read + c5 * inp * m.write_5m + c1 * inp * m.write_1h + c24 * inp * m.write_24h;
    let output_cost = Decimal::from(counts.completion.max(0)) * outp;
    Some(input_cost + output_cost)
}

/// List price for raw token counts: `prompt·input + completion·output`, or `None` when the
/// model has no pricing at all. The single source of the base-cost arithmetic — used by the
/// no-cache `total_cost` path ([`charged_cost`]) and by `uncached_cost`, and exposed so test
/// fixtures derive their cost the same way production does instead of re-implementing it.
pub(crate) fn list_price(
    prompt_tokens: i64,
    completion_tokens: i64,
    input_price: Option<Decimal>,
    output_price: Option<Decimal>,
) -> Option<Decimal> {
    if input_price.is_none() && output_price.is_none() {
        return None;
    }
    let inp = input_price.unwrap_or(Decimal::ZERO);
    let outp = output_price.unwrap_or(Decimal::ZERO);
    Some(Decimal::from(prompt_tokens.max(0)) * inp + Decimal::from(completion_tokens.max(0)) * outp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::errors::component::ANALYTICS_BATCHER;
    use rust_decimal::prelude::FromStr;

    /// The token fields the cost arithmetic reads, and nothing else.
    fn counts(prompt: i64, completion: i64, read: i64, c5: i64, c1: i64, c24: i64) -> TokenCounts {
        TokenCounts {
            prompt,
            completion,
            cache_read: read,
            cache_creation_5m: c5,
            cache_creation_1h: c1,
            cache_creation_24h: c24,
        }
    }

    /// Price a record the way the live path does, with the batcher's error component.
    fn cost(c: &TokenCounts, m: &CacheMultipliers) -> Option<Decimal> {
        compute_total_cost(c, Some("m"), Some(inp()), Some(outp()), m, ANALYTICS_BATCHER)
    }

    /// The un-discounted list price for a record, for savings comparisons.
    fn list(c: &TokenCounts) -> Option<Decimal> {
        list_price(c.prompt, c.completion, Some(inp()), Some(outp()))
    }

    // input price 0.001, output price 0.002.
    fn inp() -> Decimal {
        Decimal::new(1, 3)
    }
    fn outp() -> Decimal {
        Decimal::new(2, 3)
    }

    #[test]
    fn cost_without_cache_is_plain_arithmetic() {
        // No cache tokens → identical to the old prompt×input + completion×output.
        let c = counts(1000, 100, 0, 0, 0, 0);
        assert_eq!(cost(&c, &CacheMultipliers::default()).unwrap(), Decimal::new(12, 1)); // 1.2
    }

    #[test]
    fn cost_with_cache_applies_per_tier_multipliers() {
        // 2000 input: 1000 read + 500 1h-creation + 500 uncached; completion 100.
        let c = counts(2000, 100, 1000, 0, 500, 0);
        let m = CacheMultipliers {
            read: Decimal::new(1, 1), // 0.1
            write_5m: Decimal::ONE,
            write_1h: Decimal::from(2), // 2.0
            write_24h: Decimal::ONE,
        };
        // 500*0.001 (uncached) + 1000*0.001*0.1 (read) + 500*0.001*2.0 (1h write) + 100*0.002 (out)
        // = 0.5 + 0.1 + 1.0 + 0.2 = 1.8
        assert_eq!(cost(&c, &m).unwrap(), Decimal::new(18, 1));
    }

    #[test]
    fn cost_none_when_no_pricing() {
        let c = counts(1000, 100, 0, 0, 0, 0);
        assert!(compute_total_cost(&c, Some("m"), None, None, &CacheMultipliers::default(), ANALYTICS_BATCHER).is_none());
    }

    #[test]
    fn corrupt_writes_exceeding_prompt_bill_at_base_rate() {
        // WRITE counts alone (150) exceed the prompt (100) — impossible, corrupt. The
        // split is distrusted and the whole input is billed at base rate (the list
        // price), never at the cache write premium that would overcharge on a mistake.
        let c = counts(100, 5, 0, 150, 0, 0);
        let got = cost(&c, &CacheMultipliers::default()).unwrap();
        // list price = 100*0.001 + 5*0.002 = 0.11
        assert_eq!(got, Decimal::new(11, 2));
        // No savings shown for a distrusted split: total == un-discounted list price.
        assert_eq!(got, list(&c).unwrap());
    }

    #[test]
    fn drifted_split_is_capped_by_reducing_read_not_discarded() {
        // The tokenizer-drift shape seen in production agent traffic (2026-07): markers
        // cover the whole prompt and the classifier's count lands ~1% above the engine's
        // prompt_tokens (10_000 read + 200 write vs 10_100 prompt = 100-token overrun).
        // The overrun comes off the READ count; the discount survives.
        let m = CacheMultipliers::default(); // read 0.1, write_5m 1.25
        let c = counts(10_100, 5, 10_000, 200, 0, 0);
        let got = cost(&c, &m).unwrap();
        // capped read = 10_100 - 200 = 9_900; uncached = 0
        // input = 9_900*0.001*0.1 + 200*0.001*1.25 = 0.99 + 0.25 = 1.24; output = 5*0.002 = 0.01
        assert_eq!(got, Decimal::new(125, 2));
        // The whole point: massively cheaper than the discarded-split list price.
        assert!(got < list(&c).unwrap());
    }

    #[test]
    fn split_exactly_at_prompt_is_untouched() {
        // cached_total == prompt: no cap, no distrust — billed exactly as reported.
        let c = counts(10_000, 0, 9_800, 200, 0, 0);
        // 9_800*0.001*0.1 + 200*0.001*1.25 = 0.98 + 0.25
        assert_eq!(cost(&c, &CacheMultipliers::default()).unwrap(), Decimal::new(123, 2));
    }

    #[test]
    fn cap_can_consume_the_entire_read() {
        // Writes fill the whole prompt and read overruns entirely: read caps to zero and
        // the writes bill at their premium — still a trusted, capped split (writes alone
        // do NOT exceed the prompt, so this is drift, not corruption).
        let c = counts(1_000, 0, 50, 1_000, 0, 0);
        // read capped to 0; 1_000*0.001*1.25 = 1.25
        assert_eq!(cost(&c, &CacheMultipliers::default()).unwrap(), Decimal::new(125, 2));
    }

    #[test]
    fn charged_cost_gates_discount_on_enablement() {
        // 600 cache-read tokens reported on the response.
        let c = counts(1000, 100, 600, 0, 0, 0);

        // Not dwctl-cache-enabled (no tariff → None): the provider's cache tokens are ignored
        // and the full input is billed at list price — no read discount.
        let not_enabled = charged_cost(&c, Some("m"), Some(inp()), Some(outp()), None, ANALYTICS_BATCHER).unwrap();
        assert_eq!(not_enabled, list(&c).unwrap());
        assert_eq!(not_enabled, Decimal::new(12, 1)); // 1000*0.001 + 100*0.002

        // Cache-enabled (Some): the read discount applies, so it costs strictly less.
        let m = CacheMultipliers {
            read: Decimal::new(1, 1), // 0.1
            write_5m: Decimal::ONE,
            write_1h: Decimal::ONE,
            write_24h: Decimal::ONE,
        };
        let enabled = charged_cost(&c, Some("m"), Some(inp()), Some(outp()), Some(m), ANALYTICS_BATCHER).unwrap();
        // 400 uncached*0.001 + 600 read*0.001*0.1 + 100*0.002 = 0.66
        assert_eq!(enabled, Decimal::new(66, 2));
        assert!(enabled < not_enabled, "the discount must make the enabled case cheaper");
    }

    #[test]
    fn list_price_ignores_cache_split_and_is_none_without_pricing() {
        // Cache tokens present, but the list price is the full input+output at base rates.
        let c = counts(1000, 100, 500, 0, 200, 0);
        assert_eq!(list(&c).unwrap(), Decimal::new(12, 1)); // 1000*0.001 + 100*0.002 = 1.2
        assert!(
            list_price(c.prompt, c.completion, None, None).is_none(),
            "no pricing → NULL list price"
        );
    }

    fn tariff_row(write_1h: Decimal, from_hrs: i64, valid_until: Option<DateTime<Utc>>) -> CacheTariffRow {
        CacheTariffRow {
            write_multiplier_5m: Decimal::new(125, 2), // 1.25
            write_multiplier_1h: write_1h,
            write_multiplier_24h: Decimal::new(25, 1), // 2.5
            read_multiplier: Decimal::new(1, 1),       // 0.1
            valid_from: chrono::Utc::now() - chrono::Duration::hours(from_hrs),
            valid_until,
        }
    }

    #[test]
    fn resolve_multipliers_picks_latest_valid_version() {
        let now = chrono::Utc::now();
        // Two versions; the newer (valid_from 1h ago) wins over the older (5h ago).
        let rows = vec![tariff_row(Decimal::from(2), 1, None), tariff_row(Decimal::from(3), 5, None)];
        let m = resolve_cache_multipliers(&rows, now).expect("a valid version exists");
        assert_eq!(m.write_1h, Decimal::from(2), "latest valid version wins");
        assert_eq!(m.write_5m, Decimal::new(125, 2), "all tiers come from that one row");
        assert_eq!(m.write_24h, Decimal::new(25, 1));
        assert_eq!(m.read, Decimal::new(1, 1));
    }

    #[test]
    fn resolve_multipliers_none_when_empty_or_expired() {
        let now = chrono::Utc::now();
        // empty → no version valid → None (the caller falls back to defaults deliberately).
        assert!(resolve_cache_multipliers(&[], now).is_none(), "no rows → None");
        // expired version ignored → None.
        let expired = vec![tariff_row(Decimal::from(5), 2, Some(now - chrono::Duration::hours(1)))];
        assert!(resolve_cache_multipliers(&expired, now).is_none(), "expired version ignored → None");
    }

    #[test]
    fn default_multipliers_mirror_config_pricing_defaults() {
        // Default delegates to CachePricingConfig::default() so the two can't drift.
        let m = CacheMultipliers::default();
        let c = CachePricingConfig::default();
        assert_eq!(m.read, c.default_read_multiplier, "read = config default (0.1)");
        assert_eq!(m.write_5m, c.default_write_multiplier_5m, "5m = config default (1.25)");
        assert_eq!(m.write_1h, c.default_write_multiplier_1h, "1h = config default (2.0)");
        assert_eq!(m.write_24h, c.default_write_multiplier_24h, "24h = config default (2.5)");
    }

    /// Helper to create test tariffs
    fn make_tariff(
        purpose: ApiKeyPurpose,
        effective_from: DateTime<Utc>,
        valid_until: Option<DateTime<Utc>>,
        input_price: &str,
        output_price: &str,
        completion_window: Option<&str>,
    ) -> TariffInfo {
        TariffInfo {
            purpose,
            effective_from,
            valid_until,
            input_price_per_token: Decimal::from_str(input_price).unwrap(),
            output_price_per_token: Decimal::from_str(output_price).unwrap(),
            completion_window: completion_window.map(|s| s.to_string()),
        }
    }

    #[test]
    fn test_find_best_tariff_exact_match() {
        let now = chrono::Utc::now();
        let tariffs = vec![make_tariff(
            ApiKeyPurpose::Realtime,
            now - chrono::Duration::days(1),
            None,
            "0.00010",
            "0.00020",
            None,
        )];

        let (input, output) = find_best_tariff(&tariffs, Some(&ApiKeyPurpose::Realtime), None, now);
        assert_eq!(input, Some(Decimal::from_str("0.00010").unwrap()));
        assert_eq!(output, Some(Decimal::from_str("0.00020").unwrap()));
    }

    #[test]
    fn test_find_best_tariff_batch_vs_realtime() {
        let now = chrono::Utc::now();
        let tariffs = vec![
            make_tariff(
                ApiKeyPurpose::Realtime,
                now - chrono::Duration::days(1),
                None,
                "0.00010",
                "0.00020",
                None,
            ),
            make_tariff(
                ApiKeyPurpose::Batch,
                now - chrono::Duration::days(1),
                None,
                "0.00005",
                "0.00010",
                None,
            ),
        ];

        // Batch purpose should get batch pricing
        let (input, output) = find_best_tariff(&tariffs, Some(&ApiKeyPurpose::Batch), None, now);
        assert_eq!(input, Some(Decimal::from_str("0.00005").unwrap()));
        assert_eq!(output, Some(Decimal::from_str("0.00010").unwrap()));

        // Realtime purpose should get realtime pricing
        let (input, output) = find_best_tariff(&tariffs, Some(&ApiKeyPurpose::Realtime), None, now);
        assert_eq!(input, Some(Decimal::from_str("0.00010").unwrap()));
        assert_eq!(output, Some(Decimal::from_str("0.00020").unwrap()));
    }

    #[test]
    fn test_find_best_tariff_fallback_to_realtime() {
        // When batch tariff is missing, should fall back to realtime
        let now = chrono::Utc::now();
        let tariffs = vec![make_tariff(
            ApiKeyPurpose::Realtime,
            now - chrono::Duration::days(1),
            None,
            "0.00015",
            "0.00030",
            None,
        )];

        // Batch purpose with no batch tariff should fall back to realtime
        let (input, output) = find_best_tariff(&tariffs, Some(&ApiKeyPurpose::Batch), None, now);
        assert_eq!(input, Some(Decimal::from_str("0.00015").unwrap()));
        assert_eq!(output, Some(Decimal::from_str("0.00030").unwrap()));
    }

    #[test]
    fn test_find_best_tariff_historical_pricing() {
        // Test that expired tariffs are not selected for current requests
        // but ARE selected for historical timestamps
        let now = chrono::Utc::now();
        let old_tariff_start = now - chrono::Duration::days(30);
        let old_tariff_end = now - chrono::Duration::days(10);
        let new_tariff_start = now - chrono::Duration::days(10);

        let tariffs = vec![
            // Old tariff: valid from 30 days ago until 10 days ago
            make_tariff(
                ApiKeyPurpose::Realtime,
                old_tariff_start,
                Some(old_tariff_end),
                "0.00020", // Old higher price
                "0.00040",
                None,
            ),
            // New tariff: valid from 10 days ago, still active
            make_tariff(
                ApiKeyPurpose::Realtime,
                new_tariff_start,
                None,
                "0.00010", // New lower price
                "0.00020",
                None,
            ),
        ];

        // Current request should use new pricing
        let (input, output) = find_best_tariff(&tariffs, Some(&ApiKeyPurpose::Realtime), None, now);
        assert_eq!(
            input,
            Some(Decimal::from_str("0.00010").unwrap()),
            "Current request should use new pricing"
        );
        assert_eq!(output, Some(Decimal::from_str("0.00020").unwrap()));

        // Historical request (20 days ago) should use old pricing — this is the property the
        // recompute path depends on: a correction re-prices as of the original request.
        let historical_time = now - chrono::Duration::days(20);
        let (input, output) = find_best_tariff(&tariffs, Some(&ApiKeyPurpose::Realtime), None, historical_time);
        assert_eq!(
            input,
            Some(Decimal::from_str("0.00020").unwrap()),
            "Historical request should use old pricing"
        );
        assert_eq!(output, Some(Decimal::from_str("0.00040").unwrap()));
    }

    #[test]
    fn test_find_best_tariff_completion_window_exact_match() {
        // Test that completion_window-specific tariffs are matched correctly
        let now = chrono::Utc::now();
        let tariffs = vec![
            // Generic batch tariff (no completion_window)
            make_tariff(
                ApiKeyPurpose::Batch,
                now - chrono::Duration::days(1),
                None,
                "0.00010",
                "0.00020",
                None,
            ),
            // Priority-specific batch tariff for 24h window
            make_tariff(
                ApiKeyPurpose::Batch,
                now - chrono::Duration::days(1),
                None,
                "0.00005", // Cheaper for 24h priority
                "0.00010",
                Some("24h"),
            ),
        ];

        // Request with 24h completion window should get the priority-specific pricing
        let (input, output) = find_best_tariff(&tariffs, Some(&ApiKeyPurpose::Batch), Some("24h"), now);
        assert_eq!(
            input,
            Some(Decimal::from_str("0.00005").unwrap()),
            "24h priority should get specific pricing"
        );
        assert_eq!(output, Some(Decimal::from_str("0.00010").unwrap()));

        // Request without completion window should get generic batch pricing
        let (input, output) = find_best_tariff(&tariffs, Some(&ApiKeyPurpose::Batch), None, now);
        assert_eq!(
            input,
            Some(Decimal::from_str("0.00010").unwrap()),
            "No priority should get generic pricing"
        );
        assert_eq!(output, Some(Decimal::from_str("0.00020").unwrap()));
    }

    #[test]
    fn test_find_best_tariff_completion_window_fallback_to_generic() {
        // Test that unknown completion_window falls back to generic tariff, not another priority
        let now = chrono::Utc::now();
        let tariffs = vec![
            // Generic batch tariff
            make_tariff(
                ApiKeyPurpose::Batch,
                now - chrono::Duration::days(1),
                None,
                "0.00010",
                "0.00020",
                None,
            ),
            // 24h priority tariff
            make_tariff(
                ApiKeyPurpose::Batch,
                now - chrono::Duration::days(1),
                None,
                "0.00005",
                "0.00010",
                Some("24h"),
            ),
            // 7d priority tariff
            make_tariff(
                ApiKeyPurpose::Batch,
                now - chrono::Duration::days(1),
                None,
                "0.00003",
                "0.00006",
                Some("7d"),
            ),
        ];

        // Request with unknown "1h" priority should fall back to generic, NOT to 24h or 7d
        let (input, output) = find_best_tariff(&tariffs, Some(&ApiKeyPurpose::Batch), Some("1h"), now);
        assert_eq!(
            input,
            Some(Decimal::from_str("0.00010").unwrap()),
            "Unknown priority should fall back to generic, not another priority"
        );
        assert_eq!(output, Some(Decimal::from_str("0.00020").unwrap()));
    }

    #[test]
    fn test_find_best_tariff_no_matching_tariff() {
        let now = chrono::Utc::now();
        let tariffs = vec![];

        let (input, output) = find_best_tariff(&tariffs, Some(&ApiKeyPurpose::Realtime), None, now);
        assert_eq!(input, None);
        assert_eq!(output, None);
    }

    #[test]
    fn test_find_best_tariff_future_tariff_not_used() {
        // Tariff that starts in the future should not be selected
        let now = chrono::Utc::now();
        let tariffs = vec![make_tariff(
            ApiKeyPurpose::Realtime,
            now + chrono::Duration::days(1), // Starts tomorrow
            None,
            "0.00010",
            "0.00020",
            None,
        )];

        let (input, output) = find_best_tariff(&tariffs, Some(&ApiKeyPurpose::Realtime), None, now);
        assert_eq!(input, None, "Future tariff should not be selected");
        assert_eq!(output, None);
    }
}
