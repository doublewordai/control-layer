//! Load balancer for distributing requests across multiple providers
//!
//! This module implements weighted least-connections load balancing. Providers are
//! assigned weights, and the load balancer selects the provider with the lowest
//! `active_connections / weight` ratio. Ties are broken by weighted random selection
//! (proportional to provider weights), so cold-start behavior still respects weights.
//!
//! Pool-level configuration (keys, rate limits) is shared across all providers.

use crate::auth::KeySet;
use crate::target::{
    ConcurrencyGuard, ConcurrencyLimiter, FallbackConfig, LoadBalanceStrategy, RateLimiter,
    RoutingAction, RoutingRule, Target,
};
use rand::Rng;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// A pool of providers that share an alias, with load balancing support
#[derive(Debug, Clone)]
pub struct ProviderPool {
    /// The list of providers in this pool
    providers: Vec<Provider>,
    /// Pool-level access control keys (who can call this alias)
    keys: Option<KeySet>,
    /// Pool-level rate limiter (applies to all requests to this alias)
    pool_limiter: Option<Arc<dyn RateLimiter>>,
    /// Pool-level concurrency limiter (applies to all requests to this alias)
    pool_concurrency_limiter: Option<ConcurrencyLimiter>,
    /// Fallback configuration for retrying failed requests
    fallback: Option<FallbackConfig>,
    /// Load balancing strategy
    strategy: LoadBalanceStrategy,
    /// Mark this pool as trusted to bypass strict mode error sanitization.
    /// When strict_mode is enabled globally AND trusted is true for a pool,
    /// error response sanitization is skipped, but success responses are still sanitized.
    /// WARNING: Trusted pools can leak metadata and non-standard responses.
    /// Only use for providers you fully control or trust.
    /// Defaults to false.
    trusted: bool,
    /// Routing rules evaluated against key labels before processing
    routing_rules: Vec<RoutingRule>,
}

/// A single provider within a pool
#[derive(Debug, Clone)]
pub struct Provider {
    /// The target configuration for this provider
    pub target: Target,
    /// Weight for load balancing (higher = more traffic)
    pub weight: u32,
    /// Tracks active connections and enforces optional concurrency limit
    limiter: ConcurrencyLimiter,
}

impl Provider {
    /// Create a new provider with no concurrency limit
    pub fn new(target: Target, weight: u32) -> Self {
        Self {
            target,
            weight,
            limiter: ConcurrencyLimiter::new(),
        }
    }

    /// Create a new provider with a concurrency limit
    pub fn with_concurrency_limit(target: Target, weight: u32, limit: usize) -> Self {
        Self {
            target,
            weight,
            limiter: ConcurrencyLimiter::with_limit(limit),
        }
    }

    /// Get the current number of active connections to this provider
    pub fn active_connections(&self) -> usize {
        self.limiter.active()
    }
}

impl ProviderPool {
    /// Create a new provider pool from a list of providers
    pub fn new(providers: Vec<Provider>) -> Self {
        Self {
            providers,
            keys: None,
            pool_limiter: None,
            pool_concurrency_limiter: None,
            fallback: None,
            strategy: LoadBalanceStrategy::default(),
            trusted: false,
            routing_rules: Vec::new(),
        }
    }

    /// Create a new provider pool with pool-level configuration
    #[allow(clippy::too_many_arguments)]
    pub fn with_config(
        providers: Vec<Provider>,
        keys: Option<KeySet>,
        pool_limiter: Option<Arc<dyn RateLimiter>>,
        pool_concurrency_limiter: Option<ConcurrencyLimiter>,
        fallback: Option<FallbackConfig>,
        strategy: LoadBalanceStrategy,
        trusted: bool,
        routing_rules: Vec<RoutingRule>,
    ) -> Self {
        Self {
            providers,
            keys,
            pool_limiter,
            pool_concurrency_limiter,
            fallback,
            strategy,
            trusted,
            routing_rules,
        }
    }

    /// Create a pool with a single provider
    pub fn single(target: Target, weight: u32) -> Self {
        Self::new(vec![Provider::new(target, weight)])
    }

    /// Select the best available provider using weighted least connections.
    ///
    /// For WeightedRandom strategy: picks the provider with the lowest
    /// `active_connections / weight` ratio, breaking ties with weighted random
    /// selection. Skips providers at their concurrency limit.
    ///
    /// For Priority strategy: returns the first available provider in definition
    /// order, skipping providers at their concurrency limit.
    ///
    /// Returns a ConcurrencyGuard that tracks the active connection. When dropped,
    /// the connection count is decremented.
    pub fn select(&self) -> Option<(usize, &Target, ConcurrencyGuard)> {
        self.select_excluding(&HashSet::new())
    }

    /// Select providers lazily for fallback scenarios.
    ///
    /// Returns an iterator that yields one provider at a time. Each call to
    /// `next()` asks the LB strategy for the next provider not yet tried in the
    /// current pass (excluding previously tried providers, unless
    /// `with_replacement` is set for weighted-random).
    ///
    /// The total number of attempts is controlled by `fallback.max_attempts`
    /// (defaults to provider count) and sits *above* the LB strategy: when a pass
    /// is exhausted, the cascade restarts (exclusions cleared) until the budget is
    /// spent. So every strategy — including a single-provider `Priority` pool —
    /// honors the configured retry count, rather than stopping after one cascade.
    pub fn select_iter(&self) -> SelectIter<'_> {
        let with_replacement = self.fallback.as_ref().is_some_and(|f| f.with_replacement);
        let max_attempts = self
            .fallback
            .as_ref()
            .and_then(|f| f.max_attempts)
            .unwrap_or(self.providers.len());

        SelectIter {
            pool: self,
            state: SelectionState::new(max_attempts, with_replacement),
        }
    }

    /// Select the next provider using caller-owned fallback selection state.
    pub(crate) fn select_next(
        &self,
        state: &mut SelectionState,
    ) -> Option<(usize, &Target, ConcurrencyGuard)> {
        if state.attempts >= state.max_attempts {
            return None;
        }
        state.attempts += 1;

        // Ask the LB strategy for the next eligible provider for the current
        // exclusions. `select_excluding` returns `None` when no provider is
        // eligible — either every provider has been tried in this pass, or the
        // untried ones are all at their concurrency limit.
        //
        // If that happens but the attempt budget still allows it, start a fresh
        // pass: clear the exclusions (re-including already-tried providers) and
        // cascade through the strategy's options again. This is what puts the
        // configured retry budget *above* the LB strategy — every strategy
        // (including a single-provider Priority pool) keeps retrying until
        // `max_attempts` is spent, rather than stopping after one cascade.
        //
        // When `excluded` is already empty there is nothing to re-include, so a
        // `None` there means an empty pool or every provider at capacity: end
        // the selection rather than re-running the same scan.
        let result = match self.select_excluding(&state.excluded) {
            Some(result) => result,
            None if state.excluded.is_empty() => return None,
            None => {
                state.excluded.clear();
                self.select_excluding(&state.excluded)?
            }
        };

        // For priority strategy, exclude the provider just tried so the next
        // step advances through the list within this pass. with_replacement
        // only applies to weighted random selection (sample with replacement
        // within a pass); cross-pass retries are driven by `max_attempts` above.
        let should_exclude = match self.strategy {
            LoadBalanceStrategy::Priority => true,
            LoadBalanceStrategy::WeightedRandom => !state.with_replacement,
        };
        if should_exclude {
            state.excluded.insert(result.0);
        }

        Some(result)
    }

    /// Internal: select excluding specific provider indices
    fn select_excluding(
        &self,
        exclude: &HashSet<usize>,
    ) -> Option<(usize, &Target, ConcurrencyGuard)> {
        if self.providers.is_empty() {
            return None;
        }

        match self.strategy {
            LoadBalanceStrategy::Priority => self.select_priority(exclude),
            LoadBalanceStrategy::WeightedRandom => self.select_least_connections(exclude),
        }
    }

    /// Select using priority order: first available provider in definition order
    fn select_priority(
        &self,
        exclude: &HashSet<usize>,
    ) -> Option<(usize, &Target, ConcurrencyGuard)> {
        for (idx, provider) in self.providers.iter().enumerate() {
            if exclude.contains(&idx) {
                continue;
            }
            if let Some(guard) = provider.limiter.try_acquire() {
                return Some((idx, &provider.target, guard));
            }
        }
        None
    }

    /// Select using weighted least connections: pick the provider with the lowest
    /// active/weight ratio, breaking ties with weighted random selection
    fn select_least_connections(
        &self,
        exclude: &HashSet<usize>,
    ) -> Option<(usize, &Target, ConcurrencyGuard)> {
        // Find the minimum active/weight score among available providers
        let mut best_score = f64::INFINITY;
        let mut candidates: Vec<usize> = Vec::new();

        for (idx, provider) in self.providers.iter().enumerate() {
            if exclude.contains(&idx) {
                continue;
            }
            // Skip providers at their concurrency limit
            if provider.limiter.at_capacity() {
                continue;
            }

            let score = provider.limiter.active() as f64 / provider.weight as f64;

            if score < best_score - f64::EPSILON {
                best_score = score;
                candidates.clear();
                candidates.push(idx);
            } else if (score - best_score).abs() < f64::EPSILON {
                candidates.push(idx);
            }
        }

        if candidates.is_empty() {
            return None;
        }

        // Weighted random tiebreak: pick among tied candidates proportional to weight
        let selected = if candidates.len() == 1 {
            candidates[0]
        } else {
            let mut rng = rand::rng();
            let total_weight: u32 = candidates
                .iter()
                .map(|&idx| self.providers[idx].weight)
                .sum();
            let r: u32 = rng.random_range(0..total_weight);
            let mut cumulative = 0;
            let mut picked = candidates[0];
            for &idx in &candidates {
                cumulative += self.providers[idx].weight;
                if r < cumulative {
                    picked = idx;
                    break;
                }
            }
            picked
        };

        // Atomically acquire a connection slot
        let provider = &self.providers[selected];
        match provider.limiter.try_acquire() {
            Some(guard) => Some((selected, &provider.target, guard)),
            None => {
                // Race: provider hit limit between our check and acquire.
                // Retry with this provider excluded.
                let mut new_exclude = exclude.clone();
                new_exclude.insert(selected);
                self.select_least_connections(&new_exclude)
            }
        }
    }

    /// Get all providers in the pool (for listing models, etc.)
    pub fn providers(&self) -> &[Provider] {
        &self.providers
    }

    /// Get the number of providers in the pool
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Check if the pool is empty
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Get the first provider's target (useful for getting shared config like keys)
    pub fn first_target(&self) -> Option<&Target> {
        self.providers.first().map(|p| &p.target)
    }

    /// Get pool-level access control keys
    pub fn keys(&self) -> Option<&KeySet> {
        self.keys.as_ref()
    }

    /// Get pool-level rate limiter
    pub fn pool_limiter(&self) -> Option<&Arc<dyn RateLimiter>> {
        self.pool_limiter.as_ref()
    }

    /// Get pool-level concurrency limiter
    pub fn pool_concurrency_limiter(&self) -> Option<&ConcurrencyLimiter> {
        self.pool_concurrency_limiter.as_ref()
    }

    /// Get the fallback configuration
    pub fn fallback(&self) -> Option<&FallbackConfig> {
        self.fallback.as_ref()
    }

    /// Check if fallback is enabled for this pool
    pub fn fallback_enabled(&self) -> bool {
        self.fallback.as_ref().is_some_and(|f| f.enabled)
    }

    /// Check if a status code should trigger fallback to the next provider
    pub fn should_fallback_on_status(&self, status_code: u16) -> bool {
        self.fallback
            .as_ref()
            .is_some_and(|f| f.should_fallback_on_status(status_code))
    }

    /// Check if local rate limits should trigger fallback
    pub fn should_fallback_on_rate_limit(&self) -> bool {
        self.fallback
            .as_ref()
            .is_some_and(|f| f.enabled && f.on_rate_limit)
    }

    /// Maximum number of attempts `select_iter()` may yield, given the current
    /// fallback configuration, load-balancing strategy, and provider count.
    ///
    /// The total attempt budget for the fallback/retry loop, sitting *above* the
    /// LB strategy: `fallback.max_attempts` if set, else the provider count (one
    /// pass through the pool). Unlike provider selection within a pass, this is
    /// NOT clamped by provider count or strategy — `SelectIter` restarts the LB
    /// cascade when a pass is exhausted, so e.g. a single-provider `Priority`
    /// pool with `max_attempts = 3` retries that provider three times.
    pub fn fallback_max_attempts(&self) -> usize {
        self.fallback
            .as_ref()
            .and_then(|f| f.max_attempts)
            .unwrap_or(self.providers.len())
    }

    /// Get the load balancing strategy
    pub fn strategy(&self) -> LoadBalanceStrategy {
        self.strategy
    }

    /// Check if this pool is marked as trusted
    pub fn is_trusted(&self) -> bool {
        self.trusted
    }

    /// Get the routing rules for this pool
    pub fn routing_rules(&self) -> &[RoutingRule] {
        &self.routing_rules
    }

    /// Evaluate routing rules against key labels.
    /// Returns the first matching action, or None if no rules match (allow by default).
    pub fn evaluate_routing_rules(
        &self,
        key_labels: &HashMap<String, String>,
    ) -> Option<&RoutingAction> {
        self.routing_rules.iter().find_map(|rule| {
            let matches = rule
                .match_labels
                .iter()
                .all(|(k, v)| key_labels.get(k).is_some_and(|kv| kv == v));
            matches.then_some(&rule.action)
        })
    }

    /// Adopt active connection counters from an old pool into this (new) pool.
    ///
    /// Matches providers by (url, onwards_key, onwards_model) identity. Where a
    /// provider exists in both old and new pools, the new provider takes ownership
    /// of the old provider's `ConcurrencyLimiter` counter (`Arc<AtomicUsize>`).
    /// This keeps in-flight `ConcurrencyGuard`s connected to the live pool, so
    /// the weighted least-connections algorithm sees accurate active counts
    /// across config reloads.
    ///
    /// New providers (not in the old pool) keep their fresh zero counters.
    /// Removed providers (not in the new pool) are simply dropped.
    /// The pool-level concurrency limiter is also preserved if present in both.
    pub fn adopt_provider_state(&mut self, old: &ProviderPool) {
        for new_provider in &mut self.providers {
            if let Some(old_provider) = old.providers.iter().find(|old_p| {
                old_p.target.url == new_provider.target.url
                    && old_p.target.onwards_key == new_provider.target.onwards_key
                    && old_p.target.onwards_model == new_provider.target.onwards_model
            }) {
                new_provider
                    .limiter
                    .adopt_active_counter(&old_provider.limiter);
            }
        }

        // Preserve pool-level concurrency counter if both old and new have one
        if let (Some(new_limiter), Some(old_limiter)) = (
            &mut self.pool_concurrency_limiter,
            &old.pool_concurrency_limiter,
        ) {
            new_limiter.adopt_active_counter(old_limiter);
        }
    }
}

/// Lazy iterator for fallback provider selection.
///
/// Each call to `next()` performs a fresh least-connections evaluation,
/// ensuring the most up-to-date load information is used for each attempt.
pub struct SelectIter<'a> {
    pool: &'a ProviderPool,
    state: SelectionState,
}

/// Owned state for repeated provider selection.
pub(crate) struct SelectionState {
    excluded: HashSet<usize>,
    max_attempts: usize,
    attempts: usize,
    with_replacement: bool,
}

impl SelectionState {
    pub(crate) fn new(max_attempts: usize, with_replacement: bool) -> Self {
        Self {
            excluded: HashSet::new(),
            max_attempts,
            attempts: 0,
            with_replacement,
        }
    }
}

impl<'a> Iterator for SelectIter<'a> {
    type Item = (usize, &'a Target, ConcurrencyGuard);

    fn next(&mut self) -> Option<Self::Item> {
        self.pool.select_next(&mut self.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::Target;
    use std::collections::HashMap;

    fn create_test_target(url: &str) -> Target {
        Target::builder().url(url.parse().unwrap()).build()
    }

    fn priority_pool_with_three_targets() -> ProviderPool {
        ProviderPool::with_config(
            vec![
                Provider::new(create_test_target("https://p0.example.com"), 1),
                Provider::new(create_test_target("https://p1.example.com"), 1),
                Provider::new(create_test_target("https://p2.example.com"), 1),
            ],
            None,
            None,
            None,
            None,
            LoadBalanceStrategy::Priority,
            false,
            Vec::new(),
        )
    }

    #[test]
    fn test_selection_state_uses_independent_attempt_budget() {
        let pool = priority_pool_with_three_targets();
        let mut state = SelectionState::new(2, false);
        let first = pool.select_next(&mut state).unwrap().0;
        let second = pool.select_next(&mut state).unwrap().0;
        assert_eq!((first, second), (0, 1));
        assert!(pool.select_next(&mut state).is_none());
    }

    #[test]
    fn test_single_provider_pool() {
        let target = create_test_target("https://api.example.com");
        let pool = ProviderPool::single(target.clone(), 1);

        assert_eq!(pool.len(), 1);
        assert!(!pool.is_empty());

        let selected = pool.select();
        assert!(selected.is_some());
        let (_, target, _guard) = selected.unwrap();
        assert_eq!(target.url.as_str(), "https://api.example.com/");
    }

    #[test]
    fn test_empty_pool_returns_none() {
        let pool = ProviderPool::new(vec![]);

        assert!(pool.is_empty());
        assert!(pool.select().is_none());
    }

    #[test]
    fn test_weighted_selection_distribution() {
        // With least connections, when guards are dropped between selections,
        // all providers have 0 active connections and ties are broken by
        // weighted random — so the distribution matches weights.
        let providers = vec![
            Provider::new(create_test_target("https://api1.example.com"), 3),
            Provider::new(create_test_target("https://api2.example.com"), 1),
        ];
        let pool = ProviderPool::new(providers);

        let mut counts: HashMap<String, usize> = HashMap::new();
        for _ in 0..1000 {
            if let Some((_, target, _guard)) = pool.select() {
                *counts.entry(target.url.to_string()).or_insert(0) += 1;
            }
            // guard dropped here — active count returns to 0
        }

        let count1 = *counts.get("https://api1.example.com/").unwrap_or(&0);
        let count2 = *counts.get("https://api2.example.com/").unwrap_or(&0);

        let ratio = count1 as f64 / count2 as f64;
        assert!(
            ratio > 1.5 && ratio < 6.0,
            "Expected ratio around 3.0, got {}",
            ratio
        );
    }

    #[test]
    fn test_least_connections_prefers_less_loaded() {
        // When guards are held, least connections should prefer the less loaded provider
        let providers = vec![
            Provider::new(create_test_target("https://api1.example.com"), 1),
            Provider::new(create_test_target("https://api2.example.com"), 1),
        ];
        let pool = ProviderPool::new(providers);

        // First selection: both at 0, random tiebreak (equal weights)
        let (idx1, _, guard1) = pool.select().unwrap();

        // Second selection: one at 1, other at 0 — should pick the other
        let (idx2, _, _guard2) = pool.select().unwrap();
        assert_ne!(idx1, idx2, "Should pick the less loaded provider");

        // Drop first guard, making that provider less loaded again
        drop(guard1);

        // Now idx1 has 0 active, idx2 has 1 active — should prefer idx1
        let (idx3, _, _guard3) = pool.select().unwrap();
        assert_eq!(
            idx3, idx1,
            "Should pick the provider whose guard was dropped"
        );
    }

    #[test]
    fn test_weighted_least_connections_respects_weights() {
        // Weight 3 provider should accumulate ~3x the connections before
        // the score matches weight 1 provider
        let providers = vec![
            Provider::new(create_test_target("https://heavy.example.com"), 3),
            Provider::new(create_test_target("https://light.example.com"), 1),
        ];
        let pool = ProviderPool::new(providers);

        // Hold all guards to accumulate connections
        let mut guards = Vec::new();
        for _ in 0..40 {
            if let Some((_, _, guard)) = pool.select() {
                guards.push(guard);
            }
        }

        let heavy_active = pool.providers()[0].active_connections();
        let light_active = pool.providers()[1].active_connections();

        // Ratio should be approximately 3:1
        let ratio = heavy_active as f64 / light_active as f64;
        assert!(
            ratio > 2.0 && ratio < 5.0,
            "Expected ratio around 3.0, got {} (heavy={}, light={})",
            ratio,
            heavy_active,
            light_active
        );
    }

    #[test]
    fn test_concurrency_limit_skips_full_provider() {
        let providers = vec![
            Provider::with_concurrency_limit(
                create_test_target("https://limited.example.com"),
                1,
                1,
            ),
            Provider::new(create_test_target("https://unlimited.example.com"), 1),
        ];
        let pool = ProviderPool::new(providers);

        // First request goes to limited provider (both at 0, random tiebreak)
        // Keep trying until we get the limited one
        let mut guard_on_limited = None;
        for _ in 0..100 {
            let (idx, _, guard) = pool.select().unwrap();
            if idx == 0 {
                guard_on_limited = Some(guard);
                break;
            }
        }
        assert!(
            guard_on_limited.is_some(),
            "Should eventually select the limited provider"
        );

        // Now limited provider is at capacity (1/1). Next selection must go to unlimited.
        let (idx, _, _guard) = pool.select().unwrap();
        assert_eq!(idx, 1, "Should skip the full provider");
    }

    #[test]
    fn test_all_at_capacity_returns_none() {
        let providers = vec![
            Provider::with_concurrency_limit(create_test_target("https://a.example.com"), 1, 1),
            Provider::with_concurrency_limit(create_test_target("https://b.example.com"), 1, 1),
        ];
        let pool = ProviderPool::new(providers);

        let (_, _, _g1) = pool.select().unwrap();
        let (_, _, _g2) = pool.select().unwrap();

        // Both at capacity
        assert!(pool.select().is_none());
    }

    #[test]
    fn test_first_target() {
        let providers = vec![
            Provider::new(create_test_target("https://api1.example.com"), 1),
            Provider::new(create_test_target("https://api2.example.com"), 2),
        ];
        let pool = ProviderPool::new(providers);

        let first = pool.first_target();
        assert!(first.is_some());
        assert_eq!(first.unwrap().url.as_str(), "https://api1.example.com/");
    }

    #[test]
    fn test_fallback_max_attempts() {
        use crate::target::FallbackConfig;

        let two_providers = || {
            vec![
                Provider::new(create_test_target("https://a.example.com"), 1),
                Provider::new(create_test_target("https://b.example.com"), 1),
            ]
        };

        // No fallback config — caps at provider count.
        let pool = ProviderPool::new(two_providers());
        assert_eq!(pool.fallback_max_attempts(), 2);

        // max_attempts unset, no replacement — caps at provider count.
        let pool = ProviderPool::with_config(
            two_providers(),
            None,
            None,
            None,
            Some(FallbackConfig {
                enabled: true,
                ..Default::default()
            }),
            LoadBalanceStrategy::default(),
            false,
            Vec::new(),
        );
        assert_eq!(pool.fallback_max_attempts(), 2);

        // max_attempts is the attempt budget and sits ABOVE the LB strategy: it
        // is no longer clamped to provider count. `SelectIter` restarts the
        // cascade when a pass is exhausted, so the budget is honored verbatim for
        // every strategy — even without `with_replacement`.
        for strategy in [
            LoadBalanceStrategy::WeightedRandom,
            LoadBalanceStrategy::Priority,
        ] {
            let pool = ProviderPool::with_config(
                two_providers(),
                None,
                None,
                None,
                Some(FallbackConfig {
                    enabled: true,
                    max_attempts: Some(10),
                    ..Default::default()
                }),
                strategy,
                false,
                Vec::new(),
            );
            assert_eq!(pool.fallback_max_attempts(), 10);
            // And the iterator actually yields that many, restarting the pass
            // (count() drops each guard immediately, so concurrency is fine).
            assert_eq!(pool.select_iter().count(), 10);
        }
    }

    #[test]
    fn test_providers_accessor() {
        let providers = vec![
            Provider::new(create_test_target("https://api1.example.com"), 1),
            Provider::new(create_test_target("https://api2.example.com"), 2),
        ];
        let pool = ProviderPool::new(providers);

        assert_eq!(pool.providers().len(), 2);
        assert_eq!(pool.providers()[0].weight, 1);
        assert_eq!(pool.providers()[1].weight, 2);
    }

    #[test]
    fn test_select_iter_priority_strategy() {
        use crate::target::LoadBalanceStrategy;

        let providers = vec![
            Provider::new(create_test_target("https://primary.example.com"), 1),
            Provider::new(create_test_target("https://secondary.example.com"), 10),
            Provider::new(create_test_target("https://tertiary.example.com"), 5),
        ];

        let pool = ProviderPool::with_config(
            providers,
            None,
            None,
            None,
            None,
            LoadBalanceStrategy::Priority,
            false,
            Vec::new(),
        );

        // Priority strategy should return providers in definition order
        let order: Vec<_> = pool.select_iter().collect();
        assert_eq!(order.len(), 3);
        assert_eq!(order[0].0, 0);
        assert_eq!(order[1].0, 1);
        assert_eq!(order[2].0, 2);
        assert_eq!(order[0].1.url.as_str(), "https://primary.example.com/");
        assert_eq!(order[1].1.url.as_str(), "https://secondary.example.com/");
        assert_eq!(order[2].1.url.as_str(), "https://tertiary.example.com/");
    }

    #[test]
    fn test_select_iter_priority_with_replacement_still_advances() {
        use crate::target::{FallbackConfig, LoadBalanceStrategy};

        let providers = vec![
            Provider::new(create_test_target("https://primary.example.com"), 1),
            Provider::new(create_test_target("https://secondary.example.com"), 1),
            Provider::new(create_test_target("https://tertiary.example.com"), 1),
        ];

        let fallback = Some(FallbackConfig {
            enabled: true,
            with_replacement: true,
            max_attempts: Some(3),
            ..Default::default()
        });

        let pool = ProviderPool::with_config(
            providers,
            None,
            None,
            None,
            fallback,
            LoadBalanceStrategy::Priority,
            false,
            Vec::new(),
        );

        // Even with with_replacement=true, priority strategy should advance
        // through providers in order (with_replacement is ignored for priority)
        let order: Vec<_> = pool.select_iter().collect();
        assert_eq!(order.len(), 3);
        assert_eq!(order[0].1.url.as_str(), "https://primary.example.com/");
        assert_eq!(order[1].1.url.as_str(), "https://secondary.example.com/");
        assert_eq!(order[2].1.url.as_str(), "https://tertiary.example.com/");
    }

    #[test]
    fn test_select_iter_weighted_random_includes_all() {
        use crate::target::LoadBalanceStrategy;

        let providers = vec![
            Provider::new(create_test_target("https://api1.example.com"), 3),
            Provider::new(create_test_target("https://api2.example.com"), 1),
        ];

        let pool = ProviderPool::with_config(
            providers,
            None,
            None,
            None,
            None,
            LoadBalanceStrategy::WeightedRandom,
            false,
            Vec::new(),
        );

        let order: Vec<_> = pool.select_iter().collect();
        assert_eq!(order.len(), 2);

        let urls: std::collections::HashSet<_> =
            order.iter().map(|(_, t, _)| t.url.as_str()).collect();
        assert!(urls.contains("https://api1.example.com/"));
        assert!(urls.contains("https://api2.example.com/"));
    }

    #[test]
    fn test_select_iter_weighted_random_distribution() {
        use crate::target::LoadBalanceStrategy;

        let providers = vec![
            Provider::new(create_test_target("https://heavy.example.com"), 9),
            Provider::new(create_test_target("https://light.example.com"), 1),
        ];

        let pool = ProviderPool::with_config(
            providers,
            None,
            None,
            None,
            None,
            LoadBalanceStrategy::WeightedRandom,
            false,
            Vec::new(),
        );

        // When guards are dropped between iterations, all providers are at 0
        // active connections, so tiebreaking is weighted random.
        let mut heavy_first = 0;
        let iterations = 1000;
        for _ in 0..iterations {
            let order: Vec<_> = pool.select_iter().collect();
            if order[0].1.url.as_str() == "https://heavy.example.com/" {
                heavy_first += 1;
            }
        }

        let percentage = (heavy_first * 100) / iterations;
        assert!(
            (80..=98).contains(&percentage),
            "Expected heavy to be first ~90% of the time, got {}% ({}/{})",
            percentage,
            heavy_first,
            iterations
        );
    }

    #[test]
    fn test_select_iter_empty_pool() {
        let pool = ProviderPool::new(vec![]);
        let order: Vec<_> = pool.select_iter().collect();
        assert!(order.is_empty());
    }

    #[test]
    fn test_select_iter_single_provider() {
        use crate::target::LoadBalanceStrategy;

        let providers = vec![Provider::new(
            create_test_target("https://only.example.com"),
            1,
        )];

        for strategy in [
            LoadBalanceStrategy::Priority,
            LoadBalanceStrategy::WeightedRandom,
        ] {
            let pool = ProviderPool::with_config(
                providers.clone(),
                None,
                None,
                None,
                None,
                strategy,
                false,
                Vec::new(),
            );

            let order: Vec<_> = pool.select_iter().collect();
            assert_eq!(order.len(), 1);
            assert_eq!(order[0].1.url.as_str(), "https://only.example.com/");
        }
    }

    #[test]
    fn test_select_iter_with_replacement_allows_duplicates() {
        use crate::target::{FallbackConfig, LoadBalanceStrategy};

        let providers = vec![
            Provider::new(create_test_target("https://api1.example.com"), 9),
            Provider::new(create_test_target("https://api2.example.com"), 1),
        ];

        let fallback = Some(FallbackConfig {
            enabled: true,
            with_replacement: true,
            max_attempts: Some(5),
            ..Default::default()
        });

        let pool = ProviderPool::with_config(
            providers,
            None,
            None,
            None,
            fallback,
            LoadBalanceStrategy::WeightedRandom,
            false,
            Vec::new(),
        );

        // With replacement + max_attempts=5, should get exactly 5 entries
        let order: Vec<_> = pool.select_iter().collect();
        assert_eq!(order.len(), 5);

        // With least connections + with_replacement, the same provider can be picked
        // multiple times (since guards from prior iterations are dropped and the
        // provider becomes least-loaded again)
        let mut found_duplicate = false;
        for _ in 0..100 {
            let order: Vec<_> = pool.select_iter().collect();
            let indices: Vec<usize> = order.iter().map(|(idx, _, _)| *idx).collect();
            let unique: std::collections::HashSet<_> = indices.iter().collect();
            if unique.len() < indices.len() {
                found_duplicate = true;
                break;
            }
        }
        assert!(
            found_duplicate,
            "With replacement should allow the same provider to appear multiple times"
        );
    }

    #[test]
    fn test_select_iter_max_attempts_controls_length() {
        use crate::target::{FallbackConfig, LoadBalanceStrategy};

        let providers = vec![
            Provider::new(create_test_target("https://api1.example.com"), 1),
            Provider::new(create_test_target("https://api2.example.com"), 1),
            Provider::new(create_test_target("https://api3.example.com"), 1),
        ];

        let fallback = Some(FallbackConfig {
            enabled: true,
            max_attempts: Some(2),
            ..Default::default()
        });

        let pool = ProviderPool::with_config(
            providers,
            None,
            None,
            None,
            fallback,
            LoadBalanceStrategy::WeightedRandom,
            false,
            Vec::new(),
        );

        let order: Vec<_> = pool.select_iter().collect();
        assert_eq!(
            order.len(),
            2,
            "max_attempts should cap the ordering length"
        );

        let indices: std::collections::HashSet<_> = order.iter().map(|(idx, _, _)| *idx).collect();
        assert_eq!(
            indices.len(),
            2,
            "Without replacement, all entries should be unique"
        );
    }

    #[test]
    fn test_select_iter_max_attempts_with_priority() {
        use crate::target::{FallbackConfig, LoadBalanceStrategy};

        let providers = vec![
            Provider::new(create_test_target("https://primary.example.com"), 1),
            Provider::new(create_test_target("https://secondary.example.com"), 1),
            Provider::new(create_test_target("https://tertiary.example.com"), 1),
        ];

        let fallback = Some(FallbackConfig {
            enabled: true,
            max_attempts: Some(2),
            ..Default::default()
        });

        let pool = ProviderPool::with_config(
            providers,
            None,
            None,
            None,
            fallback,
            LoadBalanceStrategy::Priority,
            false,
            Vec::new(),
        );

        let order: Vec<_> = pool.select_iter().collect();
        assert_eq!(order.len(), 2);
        assert_eq!(order[0].1.url.as_str(), "https://primary.example.com/");
        assert_eq!(order[1].1.url.as_str(), "https://secondary.example.com/");
    }

    #[test]
    fn test_select_iter_retries_above_strategy() {
        use crate::target::{FallbackConfig, LoadBalanceStrategy};

        // A single-provider Priority pool with max_attempts = 3: the retry budget
        // sits above the strategy, so the one provider is yielded 3 times — genuine
        // single-model retry, which Priority could not express before (no
        // with_replacement needed). count() drops each guard before the next pull.
        let pool = ProviderPool::with_config(
            vec![Provider::new(
                create_test_target("https://only.example.com"),
                1,
            )],
            None,
            None,
            None,
            Some(FallbackConfig {
                enabled: true,
                max_attempts: Some(3),
                ..Default::default()
            }),
            LoadBalanceStrategy::Priority,
            false,
            Vec::new(),
        );
        let indices: Vec<_> = pool.select_iter().map(|(idx, _, _)| idx).collect();
        assert_eq!(
            indices,
            vec![0, 0, 0],
            "single provider retried up to the budget"
        );

        // A multi-provider Priority pool with a budget beyond one cascade walks the
        // priority order, then restarts and cascades again until the budget is spent.
        let pool = ProviderPool::with_config(
            vec![
                Provider::new(create_test_target("https://p0.example.com"), 1),
                Provider::new(create_test_target("https://p1.example.com"), 1),
            ],
            None,
            None,
            None,
            Some(FallbackConfig {
                enabled: true,
                max_attempts: Some(5),
                ..Default::default()
            }),
            LoadBalanceStrategy::Priority,
            false,
            Vec::new(),
        );
        let indices: Vec<_> = pool.select_iter().map(|(idx, _, _)| idx).collect();
        assert_eq!(indices, vec![0, 1, 0, 1, 0], "cascade, restart, cascade...");
    }

    #[test]
    fn test_select_iter_defaults_preserve_behavior() {
        use crate::target::LoadBalanceStrategy;

        let providers = vec![
            Provider::new(create_test_target("https://api1.example.com"), 3),
            Provider::new(create_test_target("https://api2.example.com"), 1),
        ];

        let pool = ProviderPool::with_config(
            providers,
            None,
            None,
            None,
            None,
            LoadBalanceStrategy::WeightedRandom,
            false,
            Vec::new(),
        );

        let order: Vec<_> = pool.select_iter().collect();
        assert_eq!(order.len(), 2);

        let urls: std::collections::HashSet<_> =
            order.iter().map(|(_, t, _)| t.url.as_str()).collect();
        assert!(urls.contains("https://api1.example.com/"));
        assert!(urls.contains("https://api2.example.com/"));
    }

    #[test]
    fn test_select_iter_with_replacement_single_provider() {
        use crate::target::{FallbackConfig, LoadBalanceStrategy};

        let providers = vec![Provider::new(
            create_test_target("https://only.example.com"),
            1,
        )];

        let fallback = Some(FallbackConfig {
            enabled: true,
            with_replacement: true,
            max_attempts: Some(3),
            ..Default::default()
        });

        let pool = ProviderPool::with_config(
            providers,
            None,
            None,
            None,
            fallback,
            LoadBalanceStrategy::WeightedRandom,
            false,
            Vec::new(),
        );

        let order: Vec<_> = pool.select_iter().collect();
        assert_eq!(
            order.len(),
            3,
            "Single provider with replacement should repeat"
        );
        for (idx, target, _) in &order {
            assert_eq!(*idx, 0);
            assert_eq!(target.url.as_str(), "https://only.example.com/");
        }
    }

    #[test]
    fn test_select_iter_with_replacement_respects_weights() {
        use crate::target::{FallbackConfig, LoadBalanceStrategy};

        let providers = vec![
            Provider::new(create_test_target("https://heavy.example.com"), 99),
            Provider::new(create_test_target("https://light.example.com"), 1),
        ];

        let fallback = Some(FallbackConfig {
            enabled: true,
            with_replacement: true,
            max_attempts: Some(10),
            ..Default::default()
        });

        let pool = ProviderPool::with_config(
            providers,
            None,
            None,
            None,
            fallback,
            LoadBalanceStrategy::WeightedRandom,
            false,
            Vec::new(),
        );

        // Over many runs, the heavy provider should dominate first-pick
        let mut heavy_count = 0;
        let iterations = 100;
        for _ in 0..iterations {
            let order: Vec<_> = pool.select_iter().collect();
            heavy_count += order
                .iter()
                .filter(|(_, t, _)| t.url.as_str() == "https://heavy.example.com/")
                .count();
        }

        let total = iterations * 10;
        let percentage = (heavy_count * 100) / total;
        assert!(
            percentage > 85,
            "Heavy provider (99:1 weight) should appear >85% of the time, got {}%",
            percentage
        );
    }

    #[test]
    fn test_evaluate_routing_rules_no_rules() {
        let pool = ProviderPool::new(vec![Provider::new(
            create_test_target("https://api.example.com"),
            1,
        )]);

        let labels = HashMap::from([("purpose".to_string(), "batch".to_string())]);
        assert!(pool.evaluate_routing_rules(&labels).is_none());
    }

    #[test]
    fn test_evaluate_routing_rules_deny() {
        use crate::target::{RoutingAction, RoutingRule};

        let rules = vec![RoutingRule {
            match_labels: HashMap::from([("purpose".to_string(), "playground".to_string())]),
            action: RoutingAction::Deny,
        }];

        let pool = ProviderPool::with_config(
            vec![Provider::new(
                create_test_target("https://api.example.com"),
                1,
            )],
            None,
            None,
            None,
            None,
            LoadBalanceStrategy::default(),
            false,
            rules,
        );

        let labels = HashMap::from([("purpose".to_string(), "playground".to_string())]);
        assert!(matches!(
            pool.evaluate_routing_rules(&labels),
            Some(RoutingAction::Deny)
        ));

        let labels = HashMap::from([("purpose".to_string(), "batch".to_string())]);
        assert!(pool.evaluate_routing_rules(&labels).is_none());

        assert!(pool.evaluate_routing_rules(&HashMap::new()).is_none());
    }

    #[test]
    fn test_evaluate_routing_rules_redirect() {
        use crate::target::{RoutingAction, RoutingRule};

        let rules = vec![RoutingRule {
            match_labels: HashMap::from([("purpose".to_string(), "batch".to_string())]),
            action: RoutingAction::Redirect {
                target: "gpt-4o-mini".to_string(),
            },
        }];

        let pool = ProviderPool::with_config(
            vec![Provider::new(
                create_test_target("https://api.example.com"),
                1,
            )],
            None,
            None,
            None,
            None,
            LoadBalanceStrategy::default(),
            false,
            rules,
        );

        let labels = HashMap::from([("purpose".to_string(), "batch".to_string())]);
        match pool.evaluate_routing_rules(&labels) {
            Some(RoutingAction::Redirect { target }) => {
                assert_eq!(target, "gpt-4o-mini");
            }
            other => panic!("Expected Redirect, got {:?}", other),
        }
    }

    #[test]
    fn test_evaluate_routing_rules_first_match_wins() {
        use crate::target::{RoutingAction, RoutingRule};

        let rules = vec![
            RoutingRule {
                match_labels: HashMap::from([("purpose".to_string(), "batch".to_string())]),
                action: RoutingAction::Deny,
            },
            RoutingRule {
                match_labels: HashMap::from([("purpose".to_string(), "batch".to_string())]),
                action: RoutingAction::Redirect {
                    target: "other".to_string(),
                },
            },
        ];

        let pool = ProviderPool::with_config(
            vec![Provider::new(
                create_test_target("https://api.example.com"),
                1,
            )],
            None,
            None,
            None,
            None,
            LoadBalanceStrategy::default(),
            false,
            rules,
        );

        let labels = HashMap::from([("purpose".to_string(), "batch".to_string())]);
        assert!(matches!(
            pool.evaluate_routing_rules(&labels),
            Some(RoutingAction::Deny)
        ));
    }

    #[test]
    fn test_evaluate_routing_rules_multiple_label_conditions() {
        use crate::target::{RoutingAction, RoutingRule};

        let rules = vec![RoutingRule {
            match_labels: HashMap::from([
                ("purpose".to_string(), "batch".to_string()),
                ("tier".to_string(), "free".to_string()),
            ]),
            action: RoutingAction::Deny,
        }];

        let pool = ProviderPool::with_config(
            vec![Provider::new(
                create_test_target("https://api.example.com"),
                1,
            )],
            None,
            None,
            None,
            None,
            LoadBalanceStrategy::default(),
            false,
            rules,
        );

        let labels = HashMap::from([
            ("purpose".to_string(), "batch".to_string()),
            ("tier".to_string(), "free".to_string()),
        ]);
        assert!(matches!(
            pool.evaluate_routing_rules(&labels),
            Some(RoutingAction::Deny)
        ));

        let labels = HashMap::from([("purpose".to_string(), "batch".to_string())]);
        assert!(pool.evaluate_routing_rules(&labels).is_none());

        let labels = HashMap::from([
            ("purpose".to_string(), "batch".to_string()),
            ("tier".to_string(), "free".to_string()),
            ("org".to_string(), "acme".to_string()),
        ]);
        assert!(matches!(
            pool.evaluate_routing_rules(&labels),
            Some(RoutingAction::Deny)
        ));
    }

    #[test]
    fn test_adopt_provider_state_preserves_active_counts() {
        let providers = vec![
            Provider::new(create_test_target("https://api1.example.com"), 3),
            Provider::new(create_test_target("https://api2.example.com"), 1),
        ];
        let old_pool = ProviderPool::new(providers);

        // Accumulate some active connections on the old pool
        let mut guards = Vec::new();
        for _ in 0..8 {
            if let Some((_, _, guard)) = old_pool.select() {
                guards.push(guard);
            }
        }

        let old_active_0 = old_pool.providers()[0].active_connections();
        let old_active_1 = old_pool.providers()[1].active_connections();
        assert!(
            old_active_0 > 0,
            "Should have active connections on provider 0"
        );
        assert!(
            old_active_1 > 0,
            "Should have active connections on provider 1"
        );

        // Create a new pool (simulating a config reload) and adopt state
        let new_providers = vec![
            Provider::new(create_test_target("https://api1.example.com"), 3),
            Provider::new(create_test_target("https://api2.example.com"), 1),
        ];
        let mut new_pool = ProviderPool::new(new_providers);

        assert_eq!(new_pool.providers()[0].active_connections(), 0);
        assert_eq!(new_pool.providers()[1].active_connections(), 0);

        new_pool.adopt_provider_state(&old_pool);

        // New pool should see the same active counts
        assert_eq!(new_pool.providers()[0].active_connections(), old_active_0);
        assert_eq!(new_pool.providers()[1].active_connections(), old_active_1);

        // Dropping a guard should decrement both old and new views
        guards.pop();
        let total_after = new_pool.providers()[0].active_connections()
            + new_pool.providers()[1].active_connections();
        assert_eq!(total_after, old_active_0 + old_active_1 - 1);
    }

    #[test]
    fn test_adopt_provider_state_new_provider_starts_at_zero() {
        let old_providers = vec![Provider::new(
            create_test_target("https://api1.example.com"),
            1,
        )];
        let old_pool = ProviderPool::new(old_providers);

        // Accumulate connections on old pool
        let _guards: Vec<_> = (0..5)
            .filter_map(|_| old_pool.select().map(|(_, _, g)| g))
            .collect();

        // New pool has an additional provider
        let new_providers = vec![
            Provider::new(create_test_target("https://api1.example.com"), 1),
            Provider::new(create_test_target("https://api2.example.com"), 1),
        ];
        let mut new_pool = ProviderPool::new(new_providers);
        new_pool.adopt_provider_state(&old_pool);

        // Existing provider should have adopted counts
        assert_eq!(new_pool.providers()[0].active_connections(), 5);
        // New provider should start fresh at 0
        assert_eq!(new_pool.providers()[1].active_connections(), 0);
    }

    #[test]
    fn test_adopt_provider_state_removed_provider_ignored() {
        let old_providers = vec![
            Provider::new(create_test_target("https://api1.example.com"), 1),
            Provider::new(create_test_target("https://api2.example.com"), 1),
        ];
        let old_pool = ProviderPool::new(old_providers);

        let _guards: Vec<_> = (0..4)
            .filter_map(|_| old_pool.select().map(|(_, _, g)| g))
            .collect();

        // New pool removes api2
        let new_providers = vec![Provider::new(
            create_test_target("https://api1.example.com"),
            1,
        )];
        let mut new_pool = ProviderPool::new(new_providers);
        new_pool.adopt_provider_state(&old_pool);

        // Should only have the surviving provider's count
        assert!(new_pool.providers()[0].active_connections() > 0);
        assert_eq!(new_pool.providers().len(), 1);
    }

    #[test]
    fn test_adopt_provider_state_preserves_pool_concurrency_limiter() {
        use crate::target::ConcurrencyLimiter;

        let old_pool = ProviderPool::with_config(
            vec![Provider::new(
                create_test_target("https://api1.example.com"),
                1,
            )],
            None,
            None,
            Some(ConcurrencyLimiter::with_limit(100)),
            None,
            LoadBalanceStrategy::default(),
            false,
            Vec::new(),
        );

        // Acquire some pool-level concurrency slots
        let _guard1 = old_pool.pool_concurrency_limiter().unwrap().try_acquire();
        let _guard2 = old_pool.pool_concurrency_limiter().unwrap().try_acquire();
        assert_eq!(old_pool.pool_concurrency_limiter().unwrap().active(), 2);

        let mut new_pool = ProviderPool::with_config(
            vec![Provider::new(
                create_test_target("https://api1.example.com"),
                1,
            )],
            None,
            None,
            Some(ConcurrencyLimiter::with_limit(200)), // new limit
            None,
            LoadBalanceStrategy::default(),
            false,
            Vec::new(),
        );

        new_pool.adopt_provider_state(&old_pool);

        // Should have adopted the active count
        assert_eq!(new_pool.pool_concurrency_limiter().unwrap().active(), 2);
        // But the new limit should apply
        assert_eq!(
            new_pool.pool_concurrency_limiter().unwrap().limit(),
            Some(200)
        );
    }
}
