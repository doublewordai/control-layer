//! Load balancer for distributing requests across multiple providers
//!
//! This module implements weighted least-connections load balancing. Providers are
//! assigned weights, and the load balancer selects the provider with the lowest
//! `active_connections / weight` ratio. Ties are broken by weighted random selection
//! (proportional to provider weights), so cold-start behavior still respects weights.
//!
//! Pool members are either leaf providers (a concrete [`Target`]) or named groups
//! of leaf providers with their own load-balancing strategy, so e.g. a `priority`
//! pool can fail over from a direct provider into a `weighted_random` group.
//! A group occupies one slot in the pool's member order; under a priority parent
//! it is drained (every selectable leaf tried) before the parent advances past it,
//! and under a weighted parent it scores like one big provider
//! (`sum of leaf active / group weight`).
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

/// A single member within a pool: a leaf provider or a nested group
#[derive(Debug, Clone)]
pub struct Provider {
    /// What this member routes to (a concrete target, or a group of providers)
    pub node: ProviderNode,
    /// Weight for load balancing (higher = more traffic)
    pub weight: u32,
    /// Tracks active connections and enforces optional concurrency limit.
    /// For a group this counts requests in flight through the whole group.
    limiter: ConcurrencyLimiter,
}

/// A pool member's routing node. Groups only ever contain leaf members
/// (enforced at construction), though the selection logic itself is
/// depth-agnostic.
// Leaf is by far the common variant; boxing the Target to shrink the enum
// would put an indirection on every request's selection path.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum ProviderNode {
    /// A concrete upstream target
    Leaf(Target),
    /// A named set of leaf providers balanced by the group's own strategy
    Group {
        name: String,
        strategy: LoadBalanceStrategy,
        members: Vec<Provider>,
    },
}

impl Provider {
    /// Create a new provider with no concurrency limit
    pub fn new(target: Target, weight: u32) -> Self {
        Self {
            node: ProviderNode::Leaf(target),
            weight,
            limiter: ConcurrencyLimiter::new(),
        }
    }

    /// Create a new provider with a concurrency limit
    pub fn with_concurrency_limit(target: Target, weight: u32, limit: usize) -> Self {
        Self {
            node: ProviderNode::Leaf(target),
            weight,
            limiter: ConcurrencyLimiter::with_limit(limit),
        }
    }

    /// Create a group member from leaf providers. Panics if any member is
    /// itself a group — nesting is capped at one level of groups.
    pub fn group(
        name: String,
        strategy: LoadBalanceStrategy,
        weight: u32,
        members: Vec<Provider>,
    ) -> Self {
        assert!(
            members
                .iter()
                .all(|m| matches!(m.node, ProviderNode::Leaf(_))),
            "group members must be leaf providers"
        );
        Self {
            node: ProviderNode::Group {
                name,
                strategy,
                members,
            },
            weight,
            limiter: ConcurrencyLimiter::new(),
        }
    }

    /// Get the current number of active connections through this member
    /// (for a group, the group node's own counter)
    pub fn active_connections(&self) -> usize {
        self.limiter.active()
    }

    /// The target for a leaf member, None for a group
    pub fn target(&self) -> Option<&Target> {
        match &self.node {
            ProviderNode::Leaf(target) => Some(target),
            ProviderNode::Group { .. } => None,
        }
    }

    /// Number of leaf providers under this member
    pub fn leaf_count(&self) -> usize {
        match &self.node {
            ProviderNode::Leaf(_) => 1,
            ProviderNode::Group { members, .. } => members.iter().map(|m| m.leaf_count()).sum(),
        }
    }

    /// Total active connections across every leaf under this member.
    /// This is the "active" side of the weighted least-connections score:
    /// a group scores like one big provider.
    fn total_active(&self) -> usize {
        match &self.node {
            ProviderNode::Leaf(_) => self.limiter.active(),
            ProviderNode::Group { members, .. } => members.iter().map(|m| m.total_active()).sum(),
        }
    }

    /// Whether any leaf under this member is non-excluded and under capacity.
    /// `base` is the leaf ordinal (depth-first flattened index) of this
    /// member's first leaf.
    fn has_selectable_leaf(&self, base: usize, exclude: &HashSet<usize>) -> bool {
        if self.limiter.at_capacity() {
            return false;
        }
        match &self.node {
            ProviderNode::Leaf(_) => !exclude.contains(&base),
            ProviderNode::Group { members, .. } => {
                let mut member_base = base;
                members.iter().any(|m| {
                    let selectable = m.has_selectable_leaf(member_base, exclude);
                    member_base += m.leaf_count();
                    selectable
                })
            }
        }
    }
}

/// RAII stack of concurrency guards held for one selection. Selecting a leaf
/// inside a group holds both the group node's slot and the leaf's; dropping
/// the stack releases every level together.
#[derive(Debug)]
pub struct SelectionGuard {
    guards: Vec<ConcurrencyGuard>,
}

impl SelectionGuard {
    fn single(guard: ConcurrencyGuard) -> Self {
        Self {
            guards: vec![guard],
        }
    }

    fn push(&mut self, guard: ConcurrencyGuard) {
        self.guards.push(guard);
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
    /// Returns a SelectionGuard that tracks the active connection at every
    /// level on the selected path (group node + leaf). When dropped, the
    /// connection counts are decremented.
    ///
    /// The returned index is the selected leaf's ordinal: its position in the
    /// depth-first flattening of the member tree (identical to the provider
    /// index for pools without groups).
    pub fn select(&self) -> Option<(usize, &Target, SelectionGuard)> {
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
    /// (defaults to leaf count) and sits *above* the LB strategy: when a pass
    /// is exhausted, the cascade restarts (exclusions cleared) until the budget is
    /// spent. So every strategy — including a single-provider `Priority` pool —
    /// honors the configured retry count, rather than stopping after one cascade.
    pub fn select_iter(&self) -> SelectIter<'_> {
        let with_replacement = self.fallback.as_ref().is_some_and(|f| f.with_replacement);
        let max_attempts = self
            .fallback
            .as_ref()
            .and_then(|f| f.max_attempts)
            .unwrap_or(self.leaf_count());

        SelectIter {
            pool: self,
            excluded: HashSet::new(),
            max_attempts,
            attempts: 0,
            with_replacement,
        }
    }

    /// Internal: select excluding specific leaf ordinals
    fn select_excluding(
        &self,
        exclude: &HashSet<usize>,
    ) -> Option<(usize, &Target, SelectionGuard)> {
        select_members(&self.providers, self.strategy, 0, exclude)
    }

    /// Strategy of the level that owns the leaf at `ordinal`: the group's
    /// strategy for a leaf inside a group, else the pool's own strategy.
    fn owning_strategy(&self, ordinal: usize) -> LoadBalanceStrategy {
        let mut base = 0;
        for provider in &self.providers {
            let next = base + provider.leaf_count();
            if ordinal < next {
                return match &provider.node {
                    ProviderNode::Leaf(_) => self.strategy,
                    ProviderNode::Group { strategy, .. } => *strategy,
                };
            }
            base = next;
        }
        self.strategy
    }

    /// Get the pool's top-level members (direct providers and groups) in
    /// member order (for listing models, etc.)
    pub fn providers(&self) -> &[Provider] {
        &self.providers
    }

    /// Depth-first iterator over the pool's leaf providers
    pub fn leaves(&self) -> impl Iterator<Item = &Provider> {
        self.providers.iter().flat_map(|p| match &p.node {
            ProviderNode::Leaf(_) => std::slice::from_ref(p),
            ProviderNode::Group { members, .. } => members.as_slice(),
        })
    }

    /// Get the number of leaf providers in the pool (across all groups)
    pub fn leaf_count(&self) -> usize {
        self.providers.iter().map(|p| p.leaf_count()).sum()
    }

    /// Get the number of leaf providers in the pool
    pub fn len(&self) -> usize {
        self.leaf_count()
    }

    /// Check if the pool has no leaf providers
    pub fn is_empty(&self) -> bool {
        self.leaves().next().is_none()
    }

    /// Get the first leaf's target, depth-first (useful for getting shared config like keys)
    pub fn first_target(&self) -> Option<&Target> {
        self.leaves().next().and_then(|p| p.target())
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
    /// LB strategy: `fallback.max_attempts` if set, else the leaf count (one
    /// pass through the pool). Unlike provider selection within a pass, this is
    /// NOT clamped by provider count or strategy — `SelectIter` restarts the LB
    /// cascade when a pass is exhausted, so e.g. a single-provider `Priority`
    /// pool with `max_attempts = 3` retries that provider three times.
    pub fn fallback_max_attempts(&self) -> usize {
        self.fallback
            .as_ref()
            .and_then(|f| f.max_attempts)
            .unwrap_or(self.leaf_count())
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
    /// Matches leaves by (url, onwards_key, onwards_model) identity across the
    /// flattened member tree (so a provider moved into or out of a group keeps
    /// its counter), and group node counters by group name. Where a match
    /// exists, the new node takes ownership of the old node's
    /// `ConcurrencyLimiter` counter (`Arc<AtomicUsize>`). This keeps in-flight
    /// `ConcurrencyGuard`s connected to the live pool, so the weighted
    /// least-connections algorithm sees accurate active counts across config
    /// reloads.
    ///
    /// New providers (not in the old pool) keep their fresh zero counters.
    /// Removed providers (not in the new pool) are simply dropped.
    /// The pool-level concurrency limiter is also preserved if present in both.
    pub fn adopt_provider_state(&mut self, old: &ProviderPool) {
        let old_leaves: Vec<&Provider> = old.leaves().collect();
        for new_provider in &mut self.providers {
            adopt_member_state(new_provider, &old_leaves, &old.providers);
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

/// Adopt counters for one member of a (new) pool from the old pool's state:
/// leaves match by target identity anywhere in the old tree, group nodes match
/// by name among the old pool's top-level members.
fn adopt_member_state(provider: &mut Provider, old_leaves: &[&Provider], old_members: &[Provider]) {
    match &mut provider.node {
        ProviderNode::Leaf(target) => {
            if let Some(old_provider) = old_leaves.iter().find(|old_p| {
                old_p.target().is_some_and(|old_t| {
                    old_t.url == target.url
                        && old_t.onwards_key == target.onwards_key
                        && old_t.onwards_model == target.onwards_model
                })
            }) {
                provider.limiter.adopt_active_counter(&old_provider.limiter);
            }
        }
        ProviderNode::Group { name, members, .. } => {
            if let Some(old_group) = old_members.iter().find(|old_p| {
                matches!(&old_p.node, ProviderNode::Group { name: old_name, .. } if old_name == name)
            }) {
                provider.limiter.adopt_active_counter(&old_group.limiter);
            }
            for member in members {
                adopt_member_state(member, old_leaves, &[]);
            }
        }
    }
}

/// Select a leaf from `members` using `strategy`. `base` is the leaf ordinal
/// (depth-first flattened index) of the first leaf under `members`; `exclude`
/// holds the leaf ordinals already tried in the current pass.
fn select_members<'a>(
    members: &'a [Provider],
    strategy: LoadBalanceStrategy,
    base: usize,
    exclude: &HashSet<usize>,
) -> Option<(usize, &'a Target, SelectionGuard)> {
    if members.is_empty() {
        return None;
    }

    match strategy {
        LoadBalanceStrategy::Priority => select_priority(members, base, exclude),
        LoadBalanceStrategy::WeightedRandom => select_least_connections(members, base, exclude),
    }
}

/// Try to acquire a connection slot down one member's path. For a leaf this is
/// its own limiter; for a group, the group's strategy picks a leaf and the
/// group node's slot is held alongside the leaf's (released together when the
/// returned guard stack drops).
fn select_member<'a>(
    member: &'a Provider,
    base: usize,
    exclude: &HashSet<usize>,
) -> Option<(usize, &'a Target, SelectionGuard)> {
    match &member.node {
        ProviderNode::Leaf(target) => {
            if exclude.contains(&base) {
                return None;
            }
            member
                .limiter
                .try_acquire()
                .map(|guard| (base, target, SelectionGuard::single(guard)))
        }
        ProviderNode::Group {
            strategy, members, ..
        } => {
            if member.limiter.at_capacity() {
                return None;
            }
            let (ordinal, target, mut guards) = select_members(members, *strategy, base, exclude)?;
            // Hold the group node's slot alongside the leaf's; `?` drops the
            // leaf guard if the group hit its limit in the meantime.
            let group_guard = member.limiter.try_acquire()?;
            guards.push(group_guard);
            Some((ordinal, target, guards))
        }
    }
}

/// Select using priority order: first available member in definition order.
/// A group member is selectable while it has at least one non-excluded,
/// non-at-capacity leaf, so a priority parent drains a group before advancing
/// past it.
fn select_priority<'a>(
    members: &'a [Provider],
    base: usize,
    exclude: &HashSet<usize>,
) -> Option<(usize, &'a Target, SelectionGuard)> {
    let mut member_base = base;
    for member in members {
        if let Some(result) = select_member(member, member_base, exclude) {
            return Some(result);
        }
        member_base += member.leaf_count();
    }
    None
}

/// Select using weighted least connections: pick the member with the lowest
/// active/weight ratio, breaking ties with weighted random selection. A group
/// scores like one big provider: aggregate leaf active over the group weight,
/// eligible while any of its leaves is non-excluded and under capacity.
fn select_least_connections<'a>(
    members: &'a [Provider],
    base: usize,
    exclude: &HashSet<usize>,
) -> Option<(usize, &'a Target, SelectionGuard)> {
    // Find the minimum active/weight score among available members
    let mut best_score = f64::INFINITY;
    let mut candidates: Vec<usize> = Vec::new();
    let mut member_bases: Vec<usize> = Vec::with_capacity(members.len());

    let mut member_base = base;
    for (idx, member) in members.iter().enumerate() {
        member_bases.push(member_base);
        let current_base = member_base;
        member_base += member.leaf_count();

        // Skip members with nothing selectable (excluded or at capacity)
        if !member.has_selectable_leaf(current_base, exclude) {
            continue;
        }

        let score = member.total_active() as f64 / member.weight as f64;

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
        let total_weight: u32 = candidates.iter().map(|&idx| members[idx].weight).sum();
        let r: u32 = rng.random_range(0..total_weight);
        let mut cumulative = 0;
        let mut picked = candidates[0];
        for &idx in &candidates {
            cumulative += members[idx].weight;
            if r < cumulative {
                picked = idx;
                break;
            }
        }
        picked
    };

    // Atomically acquire a connection slot
    let member = &members[selected];
    match select_member(member, member_bases[selected], exclude) {
        Some(result) => Some(result),
        None => {
            // Race: member hit a limit between our check and acquire.
            // Retry with this member's leaves excluded.
            let mut new_exclude = exclude.clone();
            for ordinal in member_bases[selected]..member_bases[selected] + member.leaf_count() {
                new_exclude.insert(ordinal);
            }
            select_least_connections(members, base, &new_exclude)
        }
    }
}

/// Lazy iterator for fallback provider selection.
///
/// Each call to `next()` performs a fresh least-connections evaluation,
/// ensuring the most up-to-date load information is used for each attempt.
pub struct SelectIter<'a> {
    pool: &'a ProviderPool,
    excluded: HashSet<usize>,
    max_attempts: usize,
    attempts: usize,
    with_replacement: bool,
}

impl<'a> Iterator for SelectIter<'a> {
    type Item = (usize, &'a Target, SelectionGuard);

    fn next(&mut self) -> Option<Self::Item> {
        if self.attempts >= self.max_attempts {
            return None;
        }
        self.attempts += 1;

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
        // `None` there means an empty pool or every provider at capacity: end the
        // iterator rather than re-running the same scan.
        let result = match self.pool.select_excluding(&self.excluded) {
            Some(result) => result,
            None if self.excluded.is_empty() => return None,
            None => {
                self.excluded.clear();
                self.pool.select_excluding(&self.excluded)?
            }
        };

        // For priority strategy, exclude the leaf just tried so the next step
        // advances through the list within this pass. with_replacement only
        // applies to weighted random selection (sample with replacement within a
        // pass); cross-pass retries are driven by `max_attempts` above. The
        // deciding strategy is the one that owns the selected leaf's level, so
        // e.g. a weighted group under a priority parent follows the group's
        // with_replacement semantics for its own leaves.
        let should_exclude = match self.pool.owning_strategy(result.0) {
            LoadBalanceStrategy::Priority => true,
            LoadBalanceStrategy::WeightedRandom => !self.with_replacement,
        };
        if should_exclude {
            self.excluded.insert(result.0);
        }

        Some(result)
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

    fn priority_pool_with_group(providers: Vec<Provider>) -> ProviderPool {
        ProviderPool::with_config(
            providers,
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
    fn test_priority_pool_fails_over_into_weighted_group() {
        // [leaf A, group G(weighted: B, C)]: A is always tried first; when A
        // fails (is excluded) requests fall into the group and spread across
        // B and C. Once B and C are also excluded the pool is exhausted.
        let mut first_group_urls = HashMap::new();
        for _ in 0..100 {
            let group = Provider::group(
                "third-party".to_string(),
                LoadBalanceStrategy::WeightedRandom,
                1,
                vec![
                    Provider::new(create_test_target("https://b.example.com"), 1),
                    Provider::new(create_test_target("https://c.example.com"), 1),
                ],
            );
            let pool = priority_pool_with_group(vec![
                Provider::new(create_test_target("https://a.example.com"), 1),
                group,
            ]);

            let order: Vec<String> = pool
                .select_iter()
                .map(|(_, target, _)| target.url.to_string())
                .collect();
            assert_eq!(order.len(), 3, "one attempt per leaf, then exhausted");
            assert_eq!(order[0], "https://a.example.com/");
            let rest: HashSet<&str> = order[1..].iter().map(|u| u.as_str()).collect();
            assert!(rest.contains("https://b.example.com/"));
            assert!(rest.contains("https://c.example.com/"));
            *first_group_urls.entry(order[1].clone()).or_insert(0) += 1;
        }
        assert_eq!(
            first_group_urls.len(),
            2,
            "weighted group should spread the first fallback across B and C, got {:?}",
            first_group_urls
        );
    }

    #[test]
    fn test_weighted_pool_scores_group_as_aggregate() {
        // Weighted pool: direct leaf L (weight 1) vs group G (weight 3, two
        // leaves). Holding guards, the group scores like one big provider
        // (aggregate active / group weight), so it settles at ~3x the leaf.
        let group = Provider::group(
            "heavy".to_string(),
            LoadBalanceStrategy::WeightedRandom,
            3,
            vec![
                Provider::new(create_test_target("https://b.example.com"), 1),
                Provider::new(create_test_target("https://c.example.com"), 1),
            ],
        );
        let pool = ProviderPool::new(vec![
            Provider::new(create_test_target("https://l.example.com"), 1),
            group,
        ]);

        let mut guards = Vec::new();
        for _ in 0..40 {
            let (_, _, guard) = pool.select().unwrap();
            guards.push(guard);
        }

        let leaf_active = pool.providers()[0].active_connections();
        let group_active = pool.providers()[1].active_connections();
        assert_eq!(leaf_active + group_active, 40);
        let ratio = group_active as f64 / leaf_active as f64;
        assert!(
            ratio > 2.0 && ratio < 5.0,
            "Expected ratio around 3.0, got {} (group={}, leaf={})",
            ratio,
            group_active,
            leaf_active
        );

        // Both group leaves take traffic, and their counters sum to the
        // group node's counter (one slot at each level per selection)
        let ProviderNode::Group { members, .. } = &pool.providers()[1].node else {
            panic!("expected group member");
        };
        assert!(members[0].active_connections() > 0);
        assert!(members[1].active_connections() > 0);
        assert_eq!(
            members[0].active_connections() + members[1].active_connections(),
            group_active
        );
    }

    #[test]
    fn test_weighted_group_skipped_when_all_leaves_at_capacity() {
        // Group weight 100 dominates while it has room; once both its
        // (limit-1) leaves are full the group is at capacity and every
        // further selection must go to the unlimited direct leaf.
        let group = Provider::group(
            "limited".to_string(),
            LoadBalanceStrategy::WeightedRandom,
            100,
            vec![
                Provider::with_concurrency_limit(create_test_target("https://b.example.com"), 1, 1),
                Provider::with_concurrency_limit(create_test_target("https://c.example.com"), 1, 1),
            ],
        );
        let pool = ProviderPool::new(vec![
            Provider::new(create_test_target("https://l.example.com"), 1),
            group,
        ]);

        let mut guards = Vec::new();
        for _ in 0..6 {
            let (_, _, guard) = pool.select().unwrap();
            guards.push(guard);
        }

        let ProviderNode::Group { members, .. } = &pool.providers()[1].node else {
            panic!("expected group member");
        };
        assert_eq!(members[0].active_connections(), 1);
        assert_eq!(members[1].active_connections(), 1);
        assert_eq!(pool.providers()[0].active_connections(), 4);

        // Group is full — selections keep landing on the direct leaf
        let (_, target, _guard) = pool.select().unwrap();
        assert_eq!(target.url.as_str(), "https://l.example.com/");
    }

    #[test]
    fn test_priority_parent_drains_group_before_advancing() {
        // [G(weighted: B, C), D]: the priority parent must not advance past
        // the group while any of its leaves is selectable. Without
        // replacement, both leaves are visited exactly once before D.
        for _ in 0..50 {
            let group = Provider::group(
                "first".to_string(),
                LoadBalanceStrategy::WeightedRandom,
                1,
                vec![
                    Provider::new(create_test_target("https://b.example.com"), 1),
                    Provider::new(create_test_target("https://c.example.com"), 1),
                ],
            );
            let pool = priority_pool_with_group(vec![
                group,
                Provider::new(create_test_target("https://d.example.com"), 1),
            ]);

            let order: Vec<String> = pool
                .select_iter()
                .map(|(_, target, _)| target.url.to_string())
                .collect();
            assert_eq!(order.len(), 3);
            let group_urls: HashSet<&str> = order[..2].iter().map(|u| u.as_str()).collect();
            assert!(group_urls.contains("https://b.example.com/"));
            assert!(group_urls.contains("https://c.example.com/"));
            assert_eq!(order[2], "https://d.example.com/");
        }
    }

    #[test]
    fn test_group_selection_holds_group_and_leaf_guards() {
        let group = Provider::group(
            "g".to_string(),
            LoadBalanceStrategy::Priority,
            1,
            vec![Provider::new(
                create_test_target("https://b.example.com"),
                1,
            )],
        );
        let pool = ProviderPool::new(vec![group]);

        let (ordinal, target, guard) = pool.select().unwrap();
        assert_eq!(ordinal, 0);
        assert_eq!(target.url.as_str(), "https://b.example.com/");

        // Both the group node's and the leaf's counters are held...
        let group_member = &pool.providers()[0];
        assert_eq!(group_member.active_connections(), 1);
        let ProviderNode::Group { members, .. } = &group_member.node else {
            panic!("expected group member");
        };
        assert_eq!(members[0].active_connections(), 1);

        // ...and released together when the guard stack drops
        drop(guard);
        assert_eq!(group_member.active_connections(), 0);
        assert_eq!(members[0].active_connections(), 0);
    }

    #[test]
    fn test_default_max_attempts_counts_leaves() {
        // 1 direct provider + a group of 2 = 3 leaves
        let group = Provider::group(
            "g".to_string(),
            LoadBalanceStrategy::WeightedRandom,
            1,
            vec![
                Provider::new(create_test_target("https://b.example.com"), 1),
                Provider::new(create_test_target("https://c.example.com"), 1),
            ],
        );
        let pool = priority_pool_with_group(vec![
            Provider::new(create_test_target("https://a.example.com"), 1),
            group,
        ]);

        assert_eq!(pool.leaf_count(), 3);
        assert_eq!(pool.len(), 3);
        assert!(!pool.is_empty());
        assert_eq!(pool.fallback_max_attempts(), 3);
        assert_eq!(pool.select_iter().count(), 3);
    }

    #[test]
    fn test_adopt_provider_state_with_groups() {
        let make_pool = || {
            let group = Provider::group(
                "third-party".to_string(),
                LoadBalanceStrategy::WeightedRandom,
                1,
                vec![
                    Provider::new(create_test_target("https://b.example.com"), 1),
                    Provider::new(create_test_target("https://c.example.com"), 1),
                ],
            );
            priority_pool_with_group(vec![
                Provider::new(create_test_target("https://a.example.com"), 1),
                group,
            ])
        };

        let old_pool = make_pool();
        // Hold one guard per leaf: A, then both group leaves
        let mut guards: Vec<_> = old_pool.select_iter().map(|(_, _, g)| g).collect();
        assert_eq!(guards.len(), 3);
        assert_eq!(old_pool.providers()[0].active_connections(), 1);
        assert_eq!(old_pool.providers()[1].active_connections(), 2);

        // Simulate a config reload with the same shape
        let mut new_pool = make_pool();
        new_pool.adopt_provider_state(&old_pool);

        // Leaf counters matched by target identity, group node by name
        assert_eq!(new_pool.providers()[0].active_connections(), 1);
        assert_eq!(new_pool.providers()[1].active_connections(), 2);
        let ProviderNode::Group { members, .. } = &new_pool.providers()[1].node else {
            panic!("expected group member");
        };
        assert_eq!(members[0].active_connections(), 1);
        assert_eq!(members[1].active_connections(), 1);

        // Dropping an in-flight guard decrements the adopted counters
        guards.clear();
        assert_eq!(new_pool.providers()[0].active_connections(), 0);
        assert_eq!(new_pool.providers()[1].active_connections(), 0);
        assert_eq!(members[0].active_connections(), 0);
        assert_eq!(members[1].active_connections(), 0);
    }

    #[test]
    fn test_adopt_provider_state_leaf_moved_into_group() {
        // Old pool: B is a direct provider. New pool: B lives inside a group.
        // The leaf counter survives the move; the new group node (no old
        // counterpart) starts fresh.
        let old_pool = ProviderPool::new(vec![Provider::new(
            create_test_target("https://b.example.com"),
            1,
        )]);
        let _guards: Vec<_> = (0..3)
            .filter_map(|_| old_pool.select().map(|(_, _, g)| g))
            .collect();

        let group = Provider::group(
            "g".to_string(),
            LoadBalanceStrategy::WeightedRandom,
            1,
            vec![Provider::new(
                create_test_target("https://b.example.com"),
                1,
            )],
        );
        let mut new_pool = ProviderPool::new(vec![group]);
        new_pool.adopt_provider_state(&old_pool);

        let ProviderNode::Group { members, .. } = &new_pool.providers()[0].node else {
            panic!("expected group member");
        };
        assert_eq!(members[0].active_connections(), 3);
        assert_eq!(new_pool.providers()[0].active_connections(), 0);
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
