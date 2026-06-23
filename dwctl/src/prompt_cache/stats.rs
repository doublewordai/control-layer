//! The paradigm-neutral read/write token split ([`CacheStats`]) and the success-gated
//! index mutation ([`PendingWrite`]) that the classifier produces.
//!
//! `CacheStats` is what gets shaped into the response `usage` (OpenAI extension fields
//! today, Anthropic-native later — see `inject.rs`). `PendingWrite` is the index
//! mutation the cache layer commits **locally** on a 2xx response (plan §0/§6.3) — no
//! correlation id, because classify and commit share one dwctl scope.

use chrono::{DateTime, Utc};

use super::index::{CacheEntry, IndexScope, PrefixHash, TtlTier};

/// The neutral read/write token split for one request. All-zero means "no caching"
/// (the dormant/below-floor/disabled cases all return this).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Cached input tokens read back from a prior write (the discounted span).
    pub read: u64,
    /// New tokens written under each TTL tier (write premiums differ per tier).
    pub creation_5m: u64,
    pub creation_1h: u64,
    pub creation_24h: u64,
}

impl CacheStats {
    /// Total tokens written across all tiers.
    pub fn creation_total(&self) -> u64 {
        self.creation_5m.saturating_add(self.creation_1h).saturating_add(self.creation_24h)
    }

    /// True when every count is zero.
    pub fn is_zero(&self) -> bool {
        *self == Self::default()
    }

    /// Attribute `tokens` of cache creation to `tier`.
    pub fn add_creation(&mut self, tier: TtlTier, tokens: u64) {
        let slot = match tier {
            TtlTier::FiveMinutes => &mut self.creation_5m,
            TtlTier::OneHour => &mut self.creation_1h,
            TtlTier::TwentyFourHours => &mut self.creation_24h,
        };
        *slot = slot.saturating_add(tokens);
    }
}

/// The index mutation a classified request implies, committed by the cache layer once
/// the upstream response is known successful (success-gated, post-response — §6.3).
#[derive(Debug, Clone, Default)]
pub struct PendingWrite {
    /// New entries to upsert — one per breakpoint beyond the matched read (each marked
    /// prefix is independently cacheable, §1). Empty for a pure read.
    pub writes: Vec<CacheEntry>,
    /// A matched read entry whose expiry should slide forward (the sliding-TTL refresh
    /// on read). `None` when there was no read hit.
    pub refresh: Option<(IndexScope, PrefixHash, DateTime<Utc>)>,
}

impl PendingWrite {
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty() && self.refresh.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_creation_routes_to_tier() {
        let mut s = CacheStats::default();
        s.add_creation(TtlTier::OneHour, 100);
        s.add_creation(TtlTier::OneHour, 50);
        s.add_creation(TtlTier::FiveMinutes, 7);
        assert_eq!(s.creation_1h, 150);
        assert_eq!(s.creation_5m, 7);
        assert_eq!(s.creation_24h, 0);
        assert_eq!(s.creation_total(), 157);
        assert!(!s.is_zero());
    }

    #[test]
    fn default_is_zero_and_empty() {
        assert!(CacheStats::default().is_zero());
        assert!(PendingWrite::default().is_empty());
    }
}
