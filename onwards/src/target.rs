//! Target management and configuration
//!
//! This module handles the configuration and management of proxy targets - the
//! upstream services that requests are forwarded to. Targets are dynamically
//! loaded from configuration files and support hot-reloading when files change.
//!
//! ## Load Balancing
//!
//! Targets support load balancing across multiple providers. A single alias can
//! route to multiple downstream providers with different API keys, weights, and
//! rate limits. The configuration is backwards compatible - a single object is
//! treated as a pool with one provider.
//!
//! Pool-level configuration (keys, rate_limit) applies to all providers in the pool.
//! Provider-level configuration (url, onwards_key, weight) is specific to each provider.
use crate::auth::KeySet;
use crate::load_balancer::{Provider, ProviderPool};
use crate::reasoning::ReasoningTranslationConfig;
use anyhow::anyhow;
use async_trait::async_trait;
use bon::Builder;
use dashmap::DashMap;
use futures_util::{Stream, StreamExt};
use governor::{DefaultDirectRateLimiter, Quota};
use notify::{Config as NotifyConfig, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{collections::HashMap, num::NonZeroU32, path::PathBuf, pin::Pin, sync::Arc};
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, WatchStream};
use tracing::{debug, error, info, trace};
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitParameters {
    pub requests_per_second: NonZeroU32,
    pub burst_size: Option<NonZeroU32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConcurrencyLimitParameters {
    pub max_concurrent_requests: usize,
}

/// The pool every request class falls back to. Exactly today's single pool.
pub const DEFAULT_POOL: &str = "default";

/// The pool serving completions-class requests, when a composite defines one.
pub const COMPLETIONS_POOL: &str = "completions";

/// Pool names a composite may define. A name outside this set is a config error
/// rather than a silently-never-selected pool — the resolver only ever asks for
/// the names below, so a typo would otherwise be invisible.
pub const KNOWN_POOLS: [&str; 2] = [DEFAULT_POOL, COMPLETIONS_POOL];

/// The class of an inbound request, derived from its path alone.
///
/// This is the input to pool resolution and nothing else: a class names a pool,
/// and [`TargetPools::resolve`] hands back that pool or the default. Purpose
/// labels play no part — a completions request is a completions request whoever
/// sent it, which is what keeps the mechanism usable by ordinary user
/// completions traffic as well as by continuation resume legs.
///
/// New request classes are new arms here plus a pool name; selection, failover
/// and the limiters never learn that classes exist.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RequestClass {
    /// Chat completions, responses, embeddings, everything else.
    #[default]
    Normal,
    /// The legacy completions endpoint, under either router.
    Completions,
}

impl RequestClass {
    /// Classify a request path. The strict router forwards `/completions` and
    /// the passthrough router `/v1/completions`, so the suffix — not the whole
    /// path — is the discriminator. `/chat/completions` is deliberately NOT
    /// completions-class.
    pub fn from_path(path: &str) -> Self {
        let path = path.split('?').next().unwrap_or(path);
        let path = path.trim_end_matches('/');
        if path.ends_with("/completions") && !path.ends_with("/chat/completions") {
            RequestClass::Completions
        } else {
            RequestClass::Normal
        }
    }

    /// The pool this class asks for. Absent that pool, the default serves it.
    pub fn pool_name(self) -> &'static str {
        match self {
            RequestClass::Normal => DEFAULT_POOL,
            RequestClass::Completions => COMPLETIONS_POOL,
        }
    }
}

/// Provider-specific configuration for a single upstream provider.
/// This is used within a pool to configure individual providers.
#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
pub struct ProviderSpec {
    pub url: Url,
    pub onwards_key: Option<String>,
    pub onwards_model: Option<String>,
    pub rate_limit: Option<RateLimitParameters>,
    pub concurrency_limit: Option<ConcurrencyLimitParameters>,
    #[serde(default)]
    pub upstream_auth_header_name: Option<String>,
    #[serde(default)]
    pub upstream_auth_header_prefix: Option<String>,

    /// Custom headers to include in responses (e.g., pricing, metadata)
    #[serde(default)]
    pub response_headers: Option<HashMap<String, String>>,

    /// Weight for load balancing (higher = more traffic). Defaults to 1.
    #[serde(default = "default_weight")]
    #[builder(default = default_weight())]
    pub weight: u32,

    /// Enable response sanitization to enforce strict OpenAI schema compliance.
    /// Removes provider-specific fields and rewrites the model field.
    /// Defaults to false.
    #[serde(default)]
    pub sanitize_response: bool,

    /// Open Responses API configuration
    #[serde(default)]
    pub open_responses: Option<OpenResponsesConfig>,

    /// Request timeout in seconds. If specified, requests exceeding this duration
    /// will be cancelled and return a 504 Gateway Timeout error.
    /// If fallback is enabled, the next provider will be tried.
    /// Note: This timeout applies only to receiving the response headers, not to
    /// reading the full response body (which may be streamed over a longer period).
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,

    /// Per-provider override for strict mode error sanitization trust.
    /// When Some(true), error responses from this provider bypass sanitization.
    /// When Some(false), error responses are sanitized even if the pool is trusted.
    /// When None (default), inherits the pool-level `trusted` setting.
    #[serde(default)]
    pub trusted: Option<bool>,

    /// Propagate W3C trace context (traceparent / tracestate) on outbound
    /// requests to this provider. When unset, defaults to the resolved
    /// `trusted` value — providers participating in our distributed tracing
    /// fabric (typically self-hosted upstreams marked `trusted: true`) receive
    /// trace context; external providers do not. Explicit `Some(true)` /
    /// `Some(false)` overrides the inherited value.
    #[serde(default)]
    pub propagate_trace_context: Option<bool>,

    /// Translate canonical OpenAI reasoning controls into this provider's request shape.
    #[serde(default)]
    pub reasoning_translation: Option<ReasoningTranslationConfig>,
}

/// Configuration for Open Responses API behavior
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpenResponsesConfig {
    /// Enable the adapter to provide full Open Responses semantics over Chat Completions.
    /// When true, /v1/responses requests are converted to /v1/chat/completions internally.
    /// When false (default), requests are passed through to the upstream.
    #[serde(default)]
    pub adapter: bool,
}

/// Load balancing strategy for selecting providers
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalanceStrategy {
    /// Weighted random selection - distribute traffic proportionally based on weights (default)
    #[default]
    WeightedRandom,
    /// Priority-based selection - always use the first available provider in order
    Priority,
}

/// Configuration for fallback behavior when requests fail
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FallbackConfig {
    /// Whether fallback is enabled
    #[serde(default)]
    pub enabled: bool,

    /// Status codes that trigger fallback to the next provider.
    /// Supports wildcards: 50 matches all 50x codes (500-509), 5 matches all 5xx codes (500-599).
    /// Common values: [429, 502, 503, 504] or [5] for all 5xx errors.
    #[serde(default)]
    pub on_status: Vec<u16>,

    /// Whether to fallback on local rate limits (pool-level and provider-level).
    /// If true, hitting a local rate limit will try the next provider instead of returning 429.
    #[serde(default)]
    pub on_rate_limit: bool,

    /// When true, weighted random failover samples with replacement,
    /// allowing the same provider to be retried. Only affects WeightedRandom strategy.
    #[serde(default)]
    pub with_replacement: bool,

    /// Maximum number of failover attempts. Defaults to provider count.
    /// This is an attempt budget above the LB strategy: values larger than the
    /// provider count retry across additional passes through the pool (the
    /// cascade restarts), so it is useful for every strategy — including a
    /// single-provider `Priority` pool — not only with `with_replacement: true`.
    #[serde(default)]
    pub max_attempts: Option<usize>,

    /// Inter-attempt backoff. When unset (default), attempts proceed with no delay.
    #[serde(default)]
    pub backoff: Option<BackoffConfig>,

    /// Cap on cumulative time spent sleeping between attempts, in milliseconds.
    /// When the next scheduled backoff would push the total past this cap, the
    /// retry loop terminates and the last error is returned to the client.
    /// Bounds only the inter-attempt sleeps, not upstream request time.
    #[serde(default)]
    pub max_total_backoff_ms: Option<u64>,
}

impl FallbackConfig {
    /// Check if a status code should trigger fallback
    pub fn should_fallback_on_status(&self, status: u16) -> bool {
        if !self.enabled {
            return false;
        }
        self.on_status.iter().any(|&pattern| {
            if pattern < 10 {
                // Single digit: matches all codes starting with that digit (e.g., 5 matches 500-599)
                status / 100 == pattern
            } else if pattern < 100 {
                // Two digits: matches all codes starting with those digits (e.g., 50 matches 500-509)
                status / 10 == pattern
            } else {
                // Full status code: exact match
                status == pattern
            }
        })
    }
}

/// Jitter strategy applied to retry backoff delays.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JitterStrategy {
    /// Use the computed exponential delay verbatim.
    None,
    /// Sample uniformly from `[0, computed_delay]`. Recommended for shared
    /// upstreams, since synchronized retries from many clients would otherwise
    /// arrive in lockstep (the "thundering herd" failure mode).
    #[default]
    Full,
}

/// Exponential backoff between attempts inside the fallback / retry loop.
///
/// `delay(N) = min(initial_ms * factor^(N-1), max_ms)` where `N` is the
/// 1-indexed retry attempt (1 = first retry, i.e. the wait *after* the
/// original attempt failed). Jitter is then applied per `jitter`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackoffConfig {
    /// Delay before the first retry, in milliseconds.
    pub initial_ms: u64,

    /// Hard ceiling on the per-retry delay, in milliseconds. Defaults to 5000.
    #[serde(default = "default_max_backoff_ms")]
    pub max_ms: u64,

    /// Exponential growth factor between successive retries. Defaults to 2.0.
    #[serde(default = "default_backoff_factor")]
    pub factor: f64,

    /// Jitter strategy applied to each delay. Defaults to `full`.
    #[serde(default)]
    pub jitter: JitterStrategy,
}

fn default_max_backoff_ms() -> u64 {
    5_000
}

fn default_backoff_factor() -> f64 {
    2.0
}

impl BackoffConfig {
    /// Compute the un-jittered exponential delay for the given 1-indexed retry
    /// attempt. Returns `max_ms` once the exponential overflows or exceeds the
    /// ceiling, so unreasonable `factor`/`retry` combinations stay bounded.
    fn base_delay_ms(&self, retry_index: u32) -> u64 {
        if retry_index == 0 {
            return 0;
        }
        let exponent = retry_index.saturating_sub(1);
        // Bail out early on absurd retry indices — a u32 → i32 cast would wrap
        // negative and yield a *smaller* delay, the opposite of what we want.
        // Any reasonable factor reaches max_ms well within 64 doublings.
        let scaled = match i32::try_from(exponent) {
            Ok(e) => (self.initial_ms as f64) * self.factor.powi(e),
            Err(_) => f64::INFINITY,
        };
        if !scaled.is_finite() || scaled >= self.max_ms as f64 {
            self.max_ms
        } else if scaled <= 0.0 {
            0
        } else {
            scaled as u64
        }
    }

    /// Compute the actual sleep duration for the given 1-indexed retry attempt,
    /// applying the configured jitter strategy.
    pub fn delay(&self, retry_index: u32) -> std::time::Duration {
        let base = self.base_delay_ms(retry_index);
        let jittered = match self.jitter {
            JitterStrategy::None => base,
            JitterStrategy::Full => {
                if base == 0 {
                    0
                } else {
                    use rand::Rng;
                    rand::rng().random_range(0..=base)
                }
            }
        };
        std::time::Duration::from_millis(jittered)
    }
}

/// Pool-level configuration with shared settings and multiple providers.
/// This is the recommended format for load balancing configurations.
#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
pub struct PoolSpec {
    /// Access control keys for this alias (who can call this endpoint)
    #[serde(default)]
    pub keys: Option<KeySet>,

    /// Pool-level rate limit (applies to all requests to this alias)
    #[serde(default)]
    pub rate_limit: Option<RateLimitParameters>,

    /// Pool-level concurrency limit (applies to all requests to this alias)
    #[serde(default)]
    pub concurrency_limit: Option<ConcurrencyLimitParameters>,

    /// Default response headers for all providers in this pool
    #[serde(default)]
    pub response_headers: Option<HashMap<String, String>>,

    /// Fallback configuration for retrying failed requests on other providers
    #[serde(default)]
    pub fallback: Option<FallbackConfig>,

    /// Load balancing strategy (defaults to weighted_random)
    #[serde(default)]
    pub strategy: LoadBalanceStrategy,

    /// Enable response sanitization for all providers in this pool.
    /// Individual providers can override this setting. Defaults to false.
    #[serde(default)]
    #[builder(default)]
    pub sanitize_response: bool,

    /// Open Responses API configuration for all providers in this pool
    #[serde(default)]
    pub open_responses: Option<OpenResponsesConfig>,

    /// Mark this pool as trusted to bypass strict mode sanitization.
    /// When strict_mode is enabled globally AND trusted is true for a pool,
    /// error response sanitization is skipped, but success responses are still sanitized.
    /// WARNING: Trusted pools can leak metadata and non-standard responses.
    /// Only use for providers you fully control or trust.
    /// Defaults to false.
    #[serde(default)]
    #[builder(default)]
    pub trusted: bool,
    /// Routing rules evaluated against key labels before processing.
    /// First matching rule wins; no match = allow.
    #[serde(default)]
    pub routing_rules: Vec<RoutingRule>,

    /// The list of providers to load balance across
    pub providers: Vec<ProviderSpec>,
}

/// Legacy single-provider configuration (backwards compatible).
/// New configurations should prefer PoolSpec with providers array.
#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
pub struct TargetSpec {
    pub url: Url,
    pub keys: Option<KeySet>,
    pub onwards_key: Option<String>,
    pub onwards_model: Option<String>,
    pub rate_limit: Option<RateLimitParameters>,
    pub concurrency_limit: Option<ConcurrencyLimitParameters>,
    #[serde(default)]
    pub upstream_auth_header_name: Option<String>,
    #[serde(default)]
    pub upstream_auth_header_prefix: Option<String>,

    /// Custom headers to include in responses (e.g., pricing, metadata)
    #[serde(default)]
    pub response_headers: Option<HashMap<String, String>>,

    /// Weight for load balancing (higher = more traffic). Defaults to 1.
    #[serde(default = "default_weight")]
    #[builder(default = default_weight())]
    pub weight: u32,

    /// Enable response sanitization to enforce strict OpenAI schema compliance.
    /// Defaults to false.
    #[serde(default)]
    #[builder(default)]
    pub sanitize_response: bool,

    /// Open Responses API configuration
    #[serde(default)]
    pub open_responses: Option<OpenResponsesConfig>,

    /// Mark this target as trusted to bypass strict mode sanitization.
    /// For single-provider configs, this becomes the pool-level trusted flag.
    /// Defaults to false.
    #[serde(default)]
    #[builder(default)]
    pub trusted: bool,

    /// Propagate W3C trace context (traceparent / tracestate) on outbound
    /// requests to this provider. Same semantics as `ProviderSpec`'s
    /// equivalent field — when unset, defaults to the resolved trusted
    /// value. Honoured for the legacy single-provider config format.
    #[serde(default)]
    pub propagate_trace_context: Option<bool>,

    /// Request timeout in seconds. If specified, requests exceeding this duration
    /// will be cancelled and return a 504 Gateway Timeout error.
    /// If fallback is enabled, the next provider will be tried.
    /// Note: This timeout applies only to receiving the response headers, not to
    /// reading the full response body (which may be streamed over a longer period).
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,

    /// Translate canonical OpenAI reasoning controls into this provider's request shape.
    #[serde(default)]
    pub reasoning_translation: Option<ReasoningTranslationConfig>,
}

fn default_weight() -> u32 {
    1
}

/// A composite carrying more than one named pool.
///
/// The map must contain `default` — that is the pool every request class falls
/// back to, and a composite without one could serve nothing. Names are bounded
/// by [`KNOWN_POOLS`]: the resolver asks for a fixed set of names, so an
/// unrecognised one is a typo that would never be selected, and failing the
/// config load is the only way that gets noticed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolsSpec {
    /// Pools by name. `default` is required; `completions` is the only other
    /// name understood today.
    pub pools: HashMap<String, PoolSpec>,
}

/// A target specification that supports multiple configuration formats.
/// This enables backwards-compatible configuration while supporting new pool-based format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TargetSpecOrList {
    /// Several named pools, one per request class (see [`PoolsSpec`]).
    ///
    /// First in the untagged order because it is the only variant with a
    /// `pools` key: `PoolSpec` requires `providers` and `TargetSpec` requires
    /// `url`, so no existing config shape can be captured by this arm.
    Pools(PoolsSpec),
    /// Pool with explicit providers array (recommended for load balancing)
    Pool(PoolSpec),
    /// Array of providers (legacy format, still supported)
    List(Vec<TargetSpec>),
    /// Single provider (backwards compatible)
    Single(TargetSpec),
}

/// Extracted pool configuration from any target format
#[derive(Debug)]
pub struct PoolConfig {
    pub keys: Option<KeySet>,
    pub rate_limit: Option<RateLimitParameters>,
    pub concurrency_limit: Option<ConcurrencyLimitParameters>,
    pub response_headers: Option<HashMap<String, String>>,
    pub fallback: Option<FallbackConfig>,
    pub strategy: LoadBalanceStrategy,
    pub sanitize_response: bool,
    pub open_responses: Option<OpenResponsesConfig>,
    pub trusted: bool,
    pub routing_rules: Vec<RoutingRule>,
    pub providers: Vec<ProviderSpec>,
}

impl From<PoolSpec> for PoolConfig {
    fn from(pool: PoolSpec) -> Self {
        PoolConfig {
            keys: pool.keys,
            rate_limit: pool.rate_limit,
            concurrency_limit: pool.concurrency_limit,
            response_headers: pool.response_headers,
            fallback: pool.fallback,
            strategy: pool.strategy,
            sanitize_response: pool.sanitize_response,
            open_responses: pool.open_responses,
            trusted: pool.trusted,
            routing_rules: pool.routing_rules,
            providers: pool.providers,
        }
    }
}

/// The subset of an alias's routing rules a NAMED (non-`default`) pool may
/// inherit: **deny rules only**.
///
/// The two actions are not the same kind of decision:
/// - `Deny` is access control on the ALIAS. A model denied to batch traffic must
///   stay denied whichever request class asks for it, or adding a `completions`
///   pool would quietly open a batch-only model back up.
/// - `Redirect` is a routing decision that names another model ALIAS, and it is
///   only meaningful for the class the rule was written against. Following one
///   from inside a named pool sends the request to a different model: for a
///   resume leg that means token-ids rendered against model X being decoded by
///   model Y — a prefix the target never produced, so it generates garbage from
///   the first token. A redirect on the alias therefore does not apply within a
///   non-default pool.
///
/// Inheritance only. A named pool that states rules of its own keeps them
/// verbatim, redirects included: that is somebody deliberately routing this
/// class, not a rule leaking in from another one.
pub fn inheritable_routing_rules(rules: &[RoutingRule]) -> Vec<RoutingRule> {
    rules
        .iter()
        .filter(|rule| matches!(rule.action, RoutingAction::Deny))
        .cloned()
        .collect()
}

impl TargetSpecOrList {
    /// Convert to the composite's named pools, `default` first.
    ///
    /// Every legacy config shape yields exactly one pool, named `default` —
    /// which is what makes named pools inert for existing configs: one pool
    /// called `default` is the same thing as today's single pool.
    ///
    /// Non-default pools inherit the default pool's **access control** (`keys`,
    /// and the DENY half of `routing_rules` — see
    /// [`inheritable_routing_rules`]) when they do not state their own. Access
    /// control is a property of the alias, not of which upstream answers, so a
    /// pool added for one request class must not become a way around the
    /// alias's keys or deny rules.
    pub fn into_pool_configs(self) -> Result<Vec<(String, PoolConfig)>, anyhow::Error> {
        let TargetSpecOrList::Pools(spec) = self else {
            return Ok(vec![(DEFAULT_POOL.to_string(), self.into_pool_config()?)]);
        };

        let mut pools = spec.pools;
        for name in pools.keys() {
            if !KNOWN_POOLS.contains(&name.as_str()) {
                return Err(anyhow!(
                    "Unknown pool name '{}'. Known pools: {}.",
                    name,
                    KNOWN_POOLS.join(", ")
                ));
            }
        }
        let default: PoolConfig = pools
            .remove(DEFAULT_POOL)
            .ok_or_else(|| anyhow!("A `pools` target must define a '{DEFAULT_POOL}' pool."))?
            .into();

        let mut configs = Vec::with_capacity(pools.len() + 1);
        // `default` first so the caller can log/report it as the composite's
        // primary, and so pool construction order is deterministic.
        for (name, pool) in pools {
            let mut config: PoolConfig = pool.into();
            if config.keys.is_none() {
                config.keys = default.keys.clone();
            }
            if config.routing_rules.is_empty() {
                config.routing_rules = inheritable_routing_rules(&default.routing_rules);
            }
            configs.push((name, config));
        }
        configs.sort_by(|(a, _), (b, _)| a.cmp(b));
        configs.insert(0, (DEFAULT_POOL.to_string(), default));
        Ok(configs)
    }

    /// Convert to pool-level config and list of provider specs.
    ///
    /// For a multi-pool target this is the `default` pool — the one that serves
    /// every request class without a pool of its own.
    pub fn into_pool_config(self) -> Result<PoolConfig, anyhow::Error> {
        match self {
            TargetSpecOrList::Pools(spec) => spec
                .pools
                .into_iter()
                .find(|(name, _)| name == DEFAULT_POOL)
                .map(|(_, pool)| pool.into())
                .ok_or_else(|| anyhow!("A `pools` target must define a '{DEFAULT_POOL}' pool.")),
            TargetSpecOrList::Pool(pool) => Ok(pool.into()),
            TargetSpecOrList::List(list) => {
                // Legacy list format: no pool-level config, convert TargetSpecs to ProviderSpecs
                // Take keys and trusted from first provider for backwards compatibility
                let keys = list.first().and_then(|t| t.keys.clone());
                let trusted = list.first().map(|t| t.trusted).unwrap_or(false);

                // Validate that all providers in the list have the same trusted value
                if list.iter().any(|t| t.trusted != trusted) {
                    return Err(anyhow::anyhow!(
                        "All providers in a legacy list format must have the same 'trusted' value. \
                         Use pool config format if you need different trust levels per provider."
                    ));
                }

                let providers = list
                    .into_iter()
                    .map(|t| ProviderSpec {
                        url: t.url,
                        onwards_key: t.onwards_key,
                        onwards_model: t.onwards_model,
                        rate_limit: t.rate_limit,
                        concurrency_limit: t.concurrency_limit,
                        upstream_auth_header_name: t.upstream_auth_header_name,
                        upstream_auth_header_prefix: t.upstream_auth_header_prefix,
                        response_headers: t.response_headers,
                        weight: t.weight,
                        sanitize_response: t.sanitize_response,
                        open_responses: t.open_responses,
                        request_timeout_secs: t.request_timeout_secs,
                        trusted: None, // pool-level trusted handles this for legacy format
                        // Carry each provider's explicit override. Unlike `trusted`
                        // (which is forced uniform to the pool level in legacy list
                        // format), per-provider trace-context overrides are honoured;
                        // an unset value still inherits the resolved trusted value.
                        propagate_trace_context: t.propagate_trace_context,
                        reasoning_translation: t.reasoning_translation,
                    })
                    .collect();
                Ok(PoolConfig {
                    keys,
                    rate_limit: None,
                    concurrency_limit: None,
                    response_headers: None,
                    fallback: None,
                    strategy: LoadBalanceStrategy::default(),
                    sanitize_response: false,
                    open_responses: None,
                    trusted,
                    routing_rules: Vec::new(),
                    providers,
                })
            }
            TargetSpecOrList::Single(spec) => {
                // Single provider: use its keys and trusted as pool-level, convert to ProviderSpec
                let keys = spec.keys.clone();
                let sanitize_response = spec.sanitize_response;
                let open_responses = spec.open_responses.clone();
                let trusted = spec.trusted;
                let provider = ProviderSpec {
                    url: spec.url,
                    onwards_key: spec.onwards_key,
                    onwards_model: spec.onwards_model,
                    rate_limit: spec.rate_limit,
                    concurrency_limit: spec.concurrency_limit,
                    upstream_auth_header_name: spec.upstream_auth_header_name,
                    upstream_auth_header_prefix: spec.upstream_auth_header_prefix,
                    response_headers: spec.response_headers,
                    weight: spec.weight,
                    sanitize_response: false, // Will be OR'd with pool-level setting
                    open_responses: open_responses.clone(),
                    request_timeout_secs: spec.request_timeout_secs,
                    trusted: None, // pool-level trusted handles this for single-provider format
                    // Carry the legacy spec's explicit override (if any). Without
                    // this, setting propagate_trace_context on the single-provider
                    // YAML form would be silently dropped.
                    propagate_trace_context: spec.propagate_trace_context,
                    reasoning_translation: spec.reasoning_translation,
                };
                Ok(PoolConfig {
                    keys,
                    rate_limit: None,
                    concurrency_limit: None,
                    response_headers: None,
                    fallback: None,
                    strategy: LoadBalanceStrategy::default(),
                    sanitize_response,
                    open_responses,
                    trusted,
                    routing_rules: Vec::new(),
                    providers: vec![provider],
                })
            }
        }
    }
}

/// Normalizes a URL to ensure it has a trailing slash
///
/// This is critical for correct path joining behavior. The url::Url::join() method
/// treats URLs without trailing slashes as having a "file" component that gets replaced:
/// - Without trailing slash: `https://api.example.com/v1` + `messages` = `https://api.example.com/messages` (wrong)
/// - With trailing slash: `https://api.example.com/v1/` + `messages` = `https://api.example.com/v1/messages` (correct)
fn normalize_url(mut url: Url) -> Url {
    let path = url.path();
    if !path.ends_with('/') {
        url.set_path(&format!("{}/", path));
    }
    url
}

impl From<TargetSpec> for Target {
    fn from(value: TargetSpec) -> Self {
        Target {
            url: normalize_url(value.url),
            keys: value.keys,
            onwards_key: value.onwards_key,
            onwards_model: value.onwards_model,
            limiter: value.rate_limit.map(|rl| {
                Arc::new(governor::RateLimiter::direct(
                    Quota::per_second(rl.requests_per_second)
                        .allow_burst(rl.burst_size.unwrap_or(rl.requests_per_second)),
                )) as Arc<dyn RateLimiter>
            }),
            upstream_auth_header_name: value.upstream_auth_header_name,
            upstream_auth_header_prefix: value.upstream_auth_header_prefix,
            response_headers: value.response_headers,
            sanitize_response: value.sanitize_response,
            open_responses: value.open_responses,
            request_timeout_secs: value.request_timeout_secs,
            trusted: None,
            propagate_trace_context: value.propagate_trace_context,
            reasoning_translation: value.reasoning_translation,
        }
    }
}

impl From<ProviderSpec> for Target {
    fn from(value: ProviderSpec) -> Self {
        Target {
            url: normalize_url(value.url),
            keys: None, // Provider-level targets don't have keys; keys are at pool level
            onwards_key: value.onwards_key,
            onwards_model: value.onwards_model,
            limiter: value.rate_limit.map(|rl| {
                Arc::new(governor::RateLimiter::direct(
                    Quota::per_second(rl.requests_per_second)
                        .allow_burst(rl.burst_size.unwrap_or(rl.requests_per_second)),
                )) as Arc<dyn RateLimiter>
            }),
            upstream_auth_header_name: value.upstream_auth_header_name,
            upstream_auth_header_prefix: value.upstream_auth_header_prefix,
            response_headers: value.response_headers,
            sanitize_response: value.sanitize_response,
            open_responses: value.open_responses,
            request_timeout_secs: value.request_timeout_secs,
            trusted: value.trusted,
            propagate_trace_context: value.propagate_trace_context,
            reasoning_translation: value.reasoning_translation,
        }
    }
}

/// Error returned when rate limit is exceeded
#[derive(Debug, Clone, Copy)]
pub struct RateLimitExceeded;

pub trait RateLimiter: std::fmt::Debug + Send + Sync {
    fn check(&self) -> Result<(), RateLimitExceeded>;
}

impl RateLimiter for DefaultDirectRateLimiter {
    fn check(&self) -> Result<(), RateLimitExceeded> {
        self.check().map_err(|_| RateLimitExceeded)
    }
}

/// RAII guard that tracks an active connection/request.
/// When dropped, decrements the associated counter.
#[derive(Debug)]
pub struct ConcurrencyGuard {
    active: Arc<AtomicUsize>,
}

impl Drop for ConcurrencyGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Release);
    }
}

/// Tracks active connections and enforces an optional concurrency limit.
///
/// Used uniformly for provider connection tracking, pool-level concurrency
/// limits, and per-key concurrency limits.
#[derive(Debug, Clone)]
pub struct ConcurrencyLimiter {
    active: Arc<AtomicUsize>,
    limit: Option<usize>,
}

impl Default for ConcurrencyLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl ConcurrencyLimiter {
    /// Create a limiter that tracks connections without a limit.
    pub fn new() -> Self {
        Self {
            active: Arc::new(AtomicUsize::new(0)),
            limit: None,
        }
    }

    /// Create a limiter with a maximum concurrent request limit.
    pub fn with_limit(limit: usize) -> Self {
        Self {
            active: Arc::new(AtomicUsize::new(0)),
            limit: Some(limit),
        }
    }

    /// Try to acquire a concurrency slot.
    /// Returns a guard on success, None if at capacity.
    pub fn try_acquire(&self) -> Option<ConcurrencyGuard> {
        loop {
            let current = self.active.load(Ordering::Acquire);
            if let Some(max) = self.limit
                && current >= max
            {
                return None;
            }
            if self
                .active
                .compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(ConcurrencyGuard {
                    active: Arc::clone(&self.active),
                });
            }
        }
    }

    /// Check if the limiter is at capacity without acquiring a slot.
    pub fn at_capacity(&self) -> bool {
        match self.limit {
            Some(max) => self.active.load(Ordering::Acquire) >= max,
            None => false,
        }
    }

    /// Get the current number of active connections.
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    /// Get the concurrency limit, if set.
    pub fn limit(&self) -> Option<usize> {
        self.limit
    }

    /// Adopt the active connection counter from another limiter.
    ///
    /// This preserves in-flight connection tracking across config reloads:
    /// existing `ConcurrencyGuard`s still reference the old `Arc<AtomicUsize>`,
    /// so by sharing it into the new limiter, active counts remain accurate.
    /// The limit is taken from `self` (the new config), not the old limiter.
    pub fn adopt_active_counter(&mut self, old: &ConcurrencyLimiter) {
        self.active = Arc::clone(&old.active);
    }
}

/// A target represents a destination for requests, specified by its URL.
///
/// ## Validating incoming requests
/// A target can have a set of keys associated with it, one of which will be required in the Authorization: Bearer header.
///
/// ## Forwarding requests
/// A target can have a onwards_key and a onwards_model. The key is put into the Authorization:
/// Bearer {} header of the request. The onwards_model is used to determine which model to put in
/// the json body when forwarding the request.
#[derive(Debug, Clone, Builder)]
#[builder(derive(Clone))]
pub struct Target {
    pub url: Url,
    pub keys: Option<KeySet>,
    pub onwards_key: Option<String>,
    pub onwards_model: Option<String>,
    pub limiter: Option<Arc<dyn RateLimiter>>,
    pub upstream_auth_header_name: Option<String>,
    pub upstream_auth_header_prefix: Option<String>,
    /// Custom headers to include in responses (e.g., pricing, metadata)
    pub response_headers: Option<HashMap<String, String>>,
    /// Enable response sanitization to enforce strict OpenAI schema compliance
    #[builder(default)]
    pub sanitize_response: bool,
    /// Open Responses API configuration
    pub open_responses: Option<OpenResponsesConfig>,
    pub request_timeout_secs: Option<u64>,
    /// Per-provider override for strict mode error sanitization trust.
    /// None means inherit from the pool-level trusted setting.
    pub trusted: Option<bool>,
    /// Per-provider override for W3C trace context propagation on outbound
    /// requests. None means inherit from the resolved trusted value.
    pub propagate_trace_context: Option<bool>,
    /// Provider-specific translation for canonical OpenAI reasoning controls.
    pub reasoning_translation: Option<ReasoningTranslationConfig>,
}

impl Target {
    /// Convert this target into a single-provider pool with weight 1
    /// This is a convenience method for backwards compatibility and testing
    /// Note: Keys from the target are transferred to the pool level
    pub fn into_pool(self) -> TargetPools {
        let keys = self.keys.clone();
        ProviderPool::with_config(
            vec![Provider::new(self, 1)],
            keys,
            None,
            None,
            None,
            LoadBalanceStrategy::default(),
            false,
            Vec::new(),
        )
        .into()
    }
}

/// The named pools of one composite (one entry in [`Targets::targets`]).
///
/// A composite is `{default: Pool, completions?: Pool}`. `default` is exactly
/// today's pool, always present; the others are optional and serve one request
/// class each. Which pool answers a request is decided ONCE, before selection,
/// by [`TargetPools::resolve`] — everything below it (the load-balance
/// strategy, `select_iter`, the failover loop, rate and concurrency limits)
/// runs on the chosen pool and never learns that other pools exist.
///
/// `default` is a field rather than a map entry so its presence is a type
/// guarantee: resolution can fall back without an unwrap.
#[derive(Debug, Clone)]
pub struct TargetPools {
    /// Serves every request class that has no pool of its own.
    default: ProviderPool,
    /// Additional pools by name. Empty for every composite that has not been
    /// given one, which is what makes this inert for existing configs.
    extra: HashMap<String, ProviderPool>,
}

impl From<ProviderPool> for TargetPools {
    /// A single pool is a composite whose only pool is `default`.
    fn from(default: ProviderPool) -> Self {
        Self {
            default,
            extra: HashMap::new(),
        }
    }
}

impl TargetPools {
    /// Build from a default pool plus any additional named pools.
    pub fn with_pools(default: ProviderPool, extra: HashMap<String, ProviderPool>) -> Self {
        Self { default, extra }
    }

    /// The pool resolver: request class → pool name → that pool, or the
    /// default when the composite defines no such pool.
    ///
    /// This is the whole of the request-class mechanism. A future class (say,
    /// pools chosen on `max_tokens`) adds an arm to [`RequestClass`] and a pool
    /// name; nothing here or below changes.
    pub fn resolve(&self, class: RequestClass) -> &ProviderPool {
        self.named_pool(class).unwrap_or(&self.default)
    }

    /// The name of the pool [`TargetPools::resolve`] would pick, but only when
    /// it is not the default — so "was a non-default pool used?" is exactly
    /// "is this `Some`?", which is what the span field reports.
    pub fn resolved_name(&self, class: RequestClass) -> Option<&'static str> {
        self.named_pool(class).map(|_| class.pool_name())
    }

    /// This class's own pool, if the composite has one WITH members.
    ///
    /// An empty pool is treated as absent, not as a pool that refuses
    /// everything: a pool whose members were all removed or disabled would
    /// otherwise start 503-ing traffic the default pool served perfectly well
    /// the day before. What keeps resume legs off unvalidated members is not
    /// this fallback — it is dwctl's route cache, which only calls a model
    /// resumable while its completions pool has at least one enabled member, so
    /// an empty pool means no resume legs are generated at all. Ordinary
    /// `/v1/completions` traffic keeps working throughout.
    fn named_pool(&self, class: RequestClass) -> Option<&ProviderPool> {
        self.extra
            .get(class.pool_name())
            .filter(|pool| !pool.is_empty())
    }

    /// The pool serving everything without a pool of its own. Callers that are
    /// not serving a request (model listing, config diagnostics) want this.
    pub fn default_pool(&self) -> &ProviderPool {
        &self.default
    }

    /// The default pool's first provider — the composite's representative for
    /// composite-level config checks (e.g. Open Responses adapter detection),
    /// which predate named pools and are not per-request-class.
    pub fn first_target(&self) -> Option<&Target> {
        self.default.first_target()
    }

    /// Evaluate the composite's routing rules. Rules live on the default pool
    /// (named pools inherit its deny rules when unset — see
    /// [`inheritable_routing_rules`]), so the default pool's rules ARE the
    /// composite's rules for label-based checks.
    pub fn evaluate_routing_rules(
        &self,
        labels: &HashMap<String, String>,
    ) -> Option<&RoutingAction> {
        self.default.evaluate_routing_rules(labels)
    }

    /// Look a pool up by name.
    pub fn get(&self, name: &str) -> Option<&ProviderPool> {
        if name == DEFAULT_POOL {
            Some(&self.default)
        } else {
            self.extra.get(name)
        }
    }

    /// Every pool, `default` first.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &ProviderPool)> {
        std::iter::once((DEFAULT_POOL, &self.default))
            .chain(self.extra.iter().map(|(name, pool)| (name.as_str(), pool)))
    }

    /// Number of pools, always at least 1.
    ///
    /// Deliberately NOT called `len`/`is_empty`: a composite used to BE a pool,
    /// and those names would silently answer a different question than they did
    /// (providers, not pools) at every call site that still compiled.
    pub fn pool_count(&self) -> usize {
        self.extra.len() + 1
    }

    /// Carry live connection counters across a config reload, pool by pool.
    /// A pool that did not exist before starts at zero, as a new pool should.
    pub fn adopt_provider_state(&mut self, old: &TargetPools) {
        self.default.adopt_provider_state(&old.default);
        for (name, pool) in self.extra.iter_mut() {
            if let Some(old_pool) = old.extra.get(name) {
                pool.adopt_provider_state(old_pool);
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyDefinition {
    pub key: String,
    pub rate_limit: Option<RateLimitParameters>,
    pub concurrency_limit: Option<ConcurrencyLimitParameters>,
    /// Labels for this key (e.g., {"purpose": "batch"}).
    /// Used by routing rules to match requests to actions.
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

/// A rule that matches on key labels and takes an action (deny or redirect).
/// Rules are evaluated in order; first match wins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingRule {
    /// All label conditions must match for this rule to apply
    pub match_labels: HashMap<String, String>,
    /// Action to take when matched
    pub action: RoutingAction,
}

/// Action taken when a routing rule matches
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum RoutingAction {
    /// Return 403 Forbidden
    Deny,
    /// Redirect to another model alias
    Redirect { target: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
pub struct Auth {
    /// global keys are merged with the per-target keys.
    global_keys: KeySet,
    /// key definitions with their actual keys and per-key rate limits
    pub key_definitions: Option<HashMap<String, KeyDefinition>>,
}

/// HTTP connection pool configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpPoolConfig {
    /// Maximum number of idle HTTP connections to keep alive per upstream host.
    /// Higher values improve performance under load by reusing connections.
    /// - Fan-out scenarios (many upstreams): 100-300
    /// - Single upstream scenarios: 1000-2000
    #[serde(default = "default_pool_max_idle_per_host")]
    pub max_idle_per_host: usize,
    /// How long (in seconds) to keep idle HTTP connections alive.
    /// 90s balances connection reuse with avoiding stale connections.
    #[serde(default = "default_pool_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
}

fn default_pool_max_idle_per_host() -> usize {
    100
}

fn default_pool_idle_timeout_secs() -> u64 {
    90
}

/// The config file contains a map of target names to targets.
/// Each target can be a single provider or a list of providers for load balancing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    pub targets: HashMap<String, TargetSpecOrList>,
    pub auth: Option<Auth>,
    /// Enable strict mode with schema validation and typed handlers.
    /// When false (default), all requests are passed through transparently.
    /// When true, only known OpenAI API paths are accepted and validated.
    #[serde(default)]
    pub strict_mode: bool,
    /// HTTP connection pooling configuration (global)
    #[serde(default)]
    pub http_pool: Option<HttpPoolConfig>,
}

/// The live-updating collection of targets.
#[derive(Debug, Clone)]
pub struct Targets {
    /// Map of alias names to their named pools (supports load balancing, and
    /// per-request-class pools — see [`TargetPools`])
    pub targets: Arc<DashMap<String, TargetPools>>,
    /// Direct rate limiters per actual API key (actual key -> rate limiter)
    pub key_rate_limiters: Arc<DashMap<String, Arc<DefaultDirectRateLimiter>>>,
    /// Concurrency limiters per actual API key (actual key -> concurrency limiter)
    pub key_concurrency_limiters: Arc<DashMap<String, ConcurrencyLimiter>>,
    /// Labels per actual API key (actual key -> labels map)
    pub key_labels: Arc<DashMap<String, HashMap<String, String>>>,
    /// Enable strict mode with schema validation
    pub strict_mode: bool,
    /// HTTP connection pool configuration (global)
    pub http_pool_config: Option<HttpPoolConfig>,
}

#[async_trait]
pub trait TargetsStream {
    // TODO: Replace Into<anyhow::Error> with a custom error type using thiserror
    type Error: Into<anyhow::Error> + Send + Sync + 'static;

    /// Returns a stream of Targets updates
    async fn stream(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Targets, Self::Error>> + Send>>, Self::Error>;
}

pub struct WatchedFile(pub PathBuf);

#[async_trait]
impl TargetsStream for WatchedFile {
    type Error = anyhow::Error;

    /// Watches a file for changes and returns a stream of Targets updates.
    async fn stream(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Targets, Self::Error>> + Send>>, Self::Error> {
        // TODO(fergus): think about buffers
        let (targets_tx, targets_rx) = mpsc::channel(100);
        let (file_tx, mut file_rx) = mpsc::channel(100);

        let mut watcher = RecommendedWatcher::new(
            move |res| {
                let _ = file_tx.blocking_send(res);
            },
            NotifyConfig::default(),
        )?;

        watcher.watch(&self.0, RecursiveMode::NonRecursive)?;

        let config_path = self.0.clone();
        // TODO(fergus): stash the handle to this thread somewhere
        tokio::spawn(async move {
            while let Some(res) = file_rx.recv().await {
                match res {
                    Ok(event) => {
                        if event.kind.is_modify() {
                            info!("Config file changed, reloading targets...");
                            match Targets::from_config_file(&config_path).await {
                                Ok(new_targets) => {
                                    if targets_tx.send(Ok(new_targets)).await.is_err() {
                                        break; // Receiver dropped
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to reload config: {}", e);
                                    if targets_tx.send(Err(e)).await.is_err() {
                                        break; // Receiver dropped
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Watch error: {}", e);
                        if targets_tx
                            .send(Err(anyhow!("Watch error: {}", e)))
                            .await
                            .is_err()
                        {
                            break; // Receiver dropped
                        }
                    }
                }
            }
        });

        // Keep the watcher alive
        std::mem::forget(watcher);

        Ok(Box::pin(ReceiverStream::new(targets_rx)))
    }
}

/// A watch channel-based implementation of TargetsStream for receiving configuration updates
pub struct WatchTargetsStream {
    receiver: tokio::sync::watch::Receiver<Targets>,
}

impl WatchTargetsStream {
    pub fn new(receiver: tokio::sync::watch::Receiver<Targets>) -> Self {
        Self { receiver }
    }
}

#[async_trait]
impl TargetsStream for WatchTargetsStream {
    type Error = std::convert::Infallible;

    async fn stream(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Targets, Self::Error>> + Send>>, Self::Error> {
        let stream = WatchStream::from_changes(self.receiver.clone()).map(Ok);
        Ok(Box::pin(stream))
    }
}

/// Build one pool of the composite `alias` from its config.
///
/// Identical for every pool name — a pool is a pool, and `default` gets no
/// special treatment beyond being the one that always exists.
fn build_pool(
    alias: &str,
    pool_name: &str,
    pool_config: PoolConfig,
    global_keys: &KeySet,
) -> Result<ProviderPool, anyhow::Error> {
    for (index, provider) in pool_config.providers.iter().enumerate() {
        if let Some(config) = provider.reasoning_translation.as_ref() {
            config.validate().map_err(|error| {
                anyhow!(
                    "Invalid reasoning translation for target '{}' pool '{}' provider {}: {}",
                    alias,
                    pool_name,
                    index,
                    error.message()
                )
            })?;
        }
    }

    // Merge global keys with pool-level keys
    let merged_keys = if let Some(mut keys) = pool_config.keys {
        debug!(
            "Pool '{}' ({}) has {} keys configured",
            alias,
            pool_name,
            keys.len()
        );
        keys.extend(global_keys.clone());
        Some(keys)
    } else if !global_keys.is_empty() {
        Some(global_keys.clone())
    } else {
        None
    };

    // Create pool-level rate limiter
    let pool_limiter: Option<Arc<dyn RateLimiter>> = pool_config.rate_limit.map(|rl| {
        Arc::new(governor::RateLimiter::direct(
            Quota::per_second(rl.requests_per_second)
                .allow_burst(rl.burst_size.unwrap_or(rl.requests_per_second)),
        )) as Arc<dyn RateLimiter>
    });

    // Create pool-level concurrency limiter
    let pool_concurrency_limiter = pool_config
        .concurrency_limit
        .map(|cl| ConcurrencyLimiter::with_limit(cl.max_concurrent_requests));

    // Convert provider specs to providers
    // Pool-level sanitize_response enables sanitization for all providers
    let pool_sanitize = pool_config.sanitize_response;
    let providers: Vec<Provider> = pool_config
        .providers
        .into_iter()
        .map(|mut spec| {
            let weight = spec.weight;
            let concurrency_limit = spec
                .concurrency_limit
                .as_ref()
                .map(|cl| cl.max_concurrent_requests);
            // Enable sanitization if either pool or provider level is true
            spec.sanitize_response = pool_sanitize || spec.sanitize_response;
            let target: Target = spec.into();
            match concurrency_limit {
                Some(limit) => Provider::with_concurrency_limit(target, weight, limit),
                None => Provider::new(target, weight),
            }
        })
        .collect();

    Ok(ProviderPool::with_config(
        providers,
        merged_keys,
        pool_limiter,
        pool_concurrency_limiter,
        pool_config.fallback,
        pool_config.strategy,
        pool_config.trusted,
        pool_config.routing_rules,
    ))
}

impl Targets {
    pub async fn from_config_file(config_path: &PathBuf) -> Result<Self, anyhow::Error> {
        let contents = tokio::fs::read_to_string(config_path).await.map_err(|e| {
            anyhow!(
                "Failed to read config file {}: {}",
                config_path.display(),
                e
            )
        })?;

        let config_file: ConfigFile = serde_json::from_str(&contents).map_err(|e| {
            anyhow!(
                "Failed to parse config file {}: {}",
                config_path.display(),
                e
            )
        })?;

        let targets = Self::from_config(config_file)?;

        info!(
            "Loaded {} targets from {}",
            targets.targets.len(),
            config_path.display()
        );
        Ok(targets)
    }

    pub fn from_config(mut config_file: ConfigFile) -> Result<Self, anyhow::Error> {
        let (global_keys, key_definitions) = config_file
            .auth
            .take()
            .map(|auth| (auth.global_keys, auth.key_definitions.unwrap_or_default()))
            .unwrap_or_default();
        debug!("{} global keys configured", global_keys.len());
        debug!("{} key definitions configured", key_definitions.len());

        // Set up rate limiters and labels keyed by actual API keys
        let key_rate_limiters = Arc::new(DashMap::new());
        let key_concurrency_limiters = Arc::new(DashMap::new());
        let key_labels = Arc::new(DashMap::new());

        for (_key_id, key_def) in key_definitions {
            if let Some(ref rate_limit) = key_def.rate_limit {
                let limiter = Arc::new(governor::RateLimiter::direct(
                    Quota::per_second(rate_limit.requests_per_second).allow_burst(
                        rate_limit
                            .burst_size
                            .unwrap_or(rate_limit.requests_per_second),
                    ),
                ));
                // Map the actual API key to its rate limiter
                key_rate_limiters.insert(key_def.key.clone(), limiter);
            }
            if let Some(ref concurrency_limit) = key_def.concurrency_limit {
                let limiter =
                    ConcurrencyLimiter::with_limit(concurrency_limit.max_concurrent_requests);
                key_concurrency_limiters.insert(key_def.key.clone(), limiter);
            }
            if !key_def.labels.is_empty() {
                key_labels.insert(key_def.key.clone(), key_def.labels);
            }
        }

        let targets = Arc::new(DashMap::new());
        for (name, target_spec_or_list) in config_file.targets {
            // Every target is one or more named pools; the legacy shapes are a
            // single pool called `default`.
            let mut pool_configs = target_spec_or_list.into_pool_configs()?.into_iter();
            let (_, default_config) = pool_configs
                .next()
                .expect("into_pool_configs always yields the default pool first");
            let default_pool = build_pool(&name, DEFAULT_POOL, default_config, &global_keys)?;
            let extra = pool_configs
                .map(|(pool_name, config)| {
                    let pool = build_pool(&name, &pool_name, config, &global_keys)?;
                    Ok::<_, anyhow::Error>((pool_name, pool))
                })
                .collect::<Result<HashMap<_, _>, _>>()?;

            let pools = TargetPools::with_pools(default_pool, extra);
            for (pool_name, pool) in pools.iter() {
                debug!(
                    "Created provider pool '{}' ({}) with {} provider(s), fallback enabled: {}, strategy: {:?}",
                    name,
                    pool_name,
                    pool.len(),
                    pool.fallback_enabled(),
                    pool.strategy()
                );
            }
            targets.insert(name, pools);
        }

        Ok(Targets {
            targets,
            key_rate_limiters,
            key_concurrency_limiters,
            key_labels,
            strict_mode: config_file.strict_mode,
            http_pool_config: config_file.http_pool,
        })
    }

    /// Receives updates from a stream of targets and updates the internal targets map.
    /// Note: This method updates targets, key_rate_limiters, and key_concurrency_limiters,
    /// but does NOT update http_pool_config or strict_mode. Changes to these settings
    /// in the config file require a restart. The HTTP client pool is created once in
    /// AppState::new and cannot be reconfigured at runtime.
    pub async fn receive_updates<W>(&self, targets_stream: W) -> Result<(), W::Error>
    where
        W: TargetsStream + Send + 'static,
        W::Error: Into<anyhow::Error>,
    {
        let targets = Arc::clone(&self.targets);
        let key_rate_limiters = Arc::clone(&self.key_rate_limiters);
        let key_concurrency_limiters = Arc::clone(&self.key_concurrency_limiters);
        let key_labels = Arc::clone(&self.key_labels);

        let mut stream = targets_stream.stream().await?;

        // TODO(fergus): stash the handle to this thread somewhere
        tokio::spawn(async move {
            while let Some(result) = stream.next().await {
                match result {
                    Ok(new_targets) => {
                        info!("Config file changed, updating targets...");
                        trace!("{:?}", new_targets);

                        // Update targets atomically
                        let current_target_keys: Vec<String> =
                            targets.iter().map(|entry| entry.key().clone()).collect();

                        // Do it like this for atomicity (if you delete and recreate, there's a
                        // moment with no targets during which requests can fail)

                        // Remove deleted targets
                        for key in current_target_keys {
                            if !new_targets.targets.contains_key(&key) {
                                targets.remove(&key);
                            }
                        }

                        // Insert/update targets, preserving active connection
                        // counters from existing pools so in-flight ConcurrencyGuards
                        // stay connected and weighted least-connections sees accurate
                        // active counts across config reloads.
                        for entry in new_targets.targets.iter() {
                            let alias = entry.key().clone();
                            let mut new_pool = entry.value().clone();
                            if let Some(old_pool) = targets.get(&alias) {
                                new_pool.adopt_provider_state(&old_pool);
                            }
                            targets.insert(alias, new_pool);
                        }

                        // Update key rate limiters atomically (same pattern)
                        let current_rate_limiter_keys: Vec<String> = key_rate_limiters
                            .iter()
                            .map(|entry| entry.key().clone())
                            .collect();

                        // Remove deleted rate limiters
                        for key in current_rate_limiter_keys {
                            if !new_targets.key_rate_limiters.contains_key(&key) {
                                key_rate_limiters.remove(&key);
                            }
                        }

                        // Insert/update key rate limiters
                        for entry in new_targets.key_rate_limiters.iter() {
                            key_rate_limiters.insert(entry.key().clone(), entry.value().clone());
                        }

                        // Update key concurrency limiters atomically (same pattern)
                        let current_concurrency_limiter_keys: Vec<String> =
                            key_concurrency_limiters
                                .iter()
                                .map(|entry| entry.key().clone())
                                .collect();

                        // Remove deleted concurrency limiters
                        for key in current_concurrency_limiter_keys {
                            if !new_targets.key_concurrency_limiters.contains_key(&key) {
                                key_concurrency_limiters.remove(&key);
                            }
                        }

                        // Insert/update key concurrency limiters, preserving
                        // active counters from existing limiters.
                        for entry in new_targets.key_concurrency_limiters.iter() {
                            let key = entry.key().clone();
                            let mut new_limiter = entry.value().clone();
                            if let Some(old_limiter) = key_concurrency_limiters.get(&key) {
                                new_limiter.adopt_active_counter(&old_limiter);
                            }
                            key_concurrency_limiters.insert(key, new_limiter);
                        }

                        // Update key labels atomically (same pattern)
                        let current_label_keys: Vec<String> =
                            key_labels.iter().map(|entry| entry.key().clone()).collect();

                        for key in current_label_keys {
                            if !new_targets.key_labels.contains_key(&key) {
                                key_labels.remove(&key);
                            }
                        }

                        for entry in new_targets.key_labels.iter() {
                            key_labels.insert(entry.key().clone(), entry.value().clone());
                        }
                    }
                    Err(e) => {
                        let err: anyhow::Error = e.into();
                        error!("Failed to reload config: {}", err);
                    }
                }
            }
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::ConstantTimeString;
    use dashmap::DashMap;
    use std::collections::HashSet;

    /// The default pool of `alias`, cloned so tests keep the `let pool = …`
    /// shape without holding a DashMap ref. Pool clones share their live
    /// concurrency counters, exactly as the request path's clone does.
    fn default_pool(targets: &Targets, alias: &str) -> ProviderPool {
        targets.targets.get(alias).unwrap().default_pool().clone()
    }
    use std::sync::Arc;
    pub struct MockConfigWatcher {
        configs: Vec<Result<Targets, String>>,
    }

    impl MockConfigWatcher {
        pub fn with_targets(targets_list: Vec<Targets>) -> Self {
            Self {
                configs: targets_list.into_iter().map(Ok).collect(),
            }
        }

        pub fn with_error(error: String) -> Self {
            Self {
                configs: vec![Err(error)],
            }
        }
    }

    #[async_trait]
    impl TargetsStream for MockConfigWatcher {
        type Error = anyhow::Error;

        async fn stream(
            &self,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<Targets, Self::Error>> + Send>>, Self::Error>
        {
            use tokio_stream::wrappers::ReceiverStream;

            let (tx, rx) = mpsc::channel(100);

            let configs = self.configs.clone();
            tokio::spawn(async move {
                for config in configs {
                    let result = config.map_err(|e| anyhow::anyhow!(e));
                    if tx.send(result).await.is_err() {
                        break; // Receiver dropped
                    }
                    // Small delay to simulate file watching
                    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                }
            });

            Ok(Box::pin(ReceiverStream::new(rx)))
        }
    }

    fn create_test_targets(models: Vec<(&str, &str)>) -> Targets {
        let targets_map = Arc::new(DashMap::new());
        for (model, url) in models {
            targets_map.insert(
                model.to_string(),
                Target::builder()
                    .url(url.parse().unwrap())
                    .onwards_key(format!("key-{model}"))
                    .build()
                    .into_pool(),
            );
        }
        Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        }
    }

    #[tokio::test]
    async fn test_config_watcher_updates_targets() {
        // Create initial targets
        let initial_targets = create_test_targets(vec![("gpt-4", "https://api.openai.com")]);

        // Create new targets that will be "watched"
        let updated_targets = create_test_targets(vec![
            ("gpt-4", "https://api.openai.com"),
            ("claude-3", "https://api.anthropic.com"),
        ]);

        let mock_watcher = MockConfigWatcher::with_targets(vec![updated_targets]);

        // Start watching
        initial_targets.receive_updates(mock_watcher).await.unwrap();

        // Give some time for the watcher to process
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Verify targets were updated
        assert_eq!(initial_targets.targets.len(), 2);
        assert!(initial_targets.targets.contains_key("gpt-4"));
        assert!(initial_targets.targets.contains_key("claude-3"));
    }

    #[tokio::test]
    async fn test_config_watcher_removes_deleted_targets() {
        // Create initial targets with multiple models
        let initial_targets = create_test_targets(vec![
            ("gpt-4", "https://api.openai.com"),
            ("claude-3", "https://api.anthropic.com"),
        ]);

        // Create updated targets with only one model (remove claude-3)
        let updated_targets = create_test_targets(vec![("gpt-4", "https://api.openai.com")]);

        let mock_watcher = MockConfigWatcher::with_targets(vec![updated_targets]);

        // Start watching
        initial_targets.receive_updates(mock_watcher).await.unwrap();

        // Give some time for the watcher to process
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Verify claude-3 was removed but gpt-4 remains
        assert_eq!(initial_targets.targets.len(), 1);
        assert!(initial_targets.targets.contains_key("gpt-4"));
        assert!(!initial_targets.targets.contains_key("claude-3"));
    }

    #[tokio::test]
    async fn test_config_watcher_multiple_updates() {
        // Create initial empty targets
        let targets = Targets {
            targets: Arc::new(DashMap::new()),
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        // Create sequence of target updates
        let update1 = create_test_targets(vec![("gpt-4", "https://api.openai.com")]);
        let update2 = create_test_targets(vec![
            ("gpt-4", "https://api.openai.com"),
            ("claude-3", "https://api.anthropic.com"),
        ]);
        let update3 = create_test_targets(vec![("claude-3", "https://api.anthropic.com")]);

        let mock_watcher = MockConfigWatcher::with_targets(vec![update1, update2, update3]);

        // Start watching
        targets.receive_updates(mock_watcher).await.unwrap();

        // Give time for all updates to process
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Verify final state (should only have claude-3)
        assert_eq!(targets.targets.len(), 1);
        assert!(!targets.targets.contains_key("gpt-4"));
        assert!(targets.targets.contains_key("claude-3"));
    }

    #[tokio::test]
    async fn test_config_watcher_handles_errors() {
        // Create initial targets
        let targets = create_test_targets(vec![("gpt-4", "https://api.openai.com")]);

        let mock_watcher = MockConfigWatcher::with_error("Invalid config file".to_string());

        // Start watching - should not panic
        targets.receive_updates(mock_watcher).await.unwrap();

        // Give some time for the error to be processed
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Original targets should remain unchanged
        assert_eq!(targets.targets.len(), 1);
        assert!(targets.targets.contains_key("gpt-4"));
    }

    #[tokio::test]
    async fn test_config_watcher_updates_target_properties() {
        // Create initial targets
        let initial_targets = create_test_targets(vec![("gpt-4", "https://api.openai.com")]);

        // Create updated targets with different URL and key
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "gpt-4".to_string(),
            Target::builder()
                .url("https://api.openai.com/v2".parse().unwrap()) // Different URL
                .onwards_key("new-key".to_string()) // Different key
                .onwards_model("gpt-4-turbo".to_string()) // Added model_key
                .build()
                .into_pool(),
        );
        let updated_targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        let mock_watcher = MockConfigWatcher::with_targets(vec![updated_targets]);

        // Start watching
        initial_targets.receive_updates(mock_watcher).await.unwrap();

        // Give some time for the watcher to process
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Verify target properties were updated
        let pool = default_pool(&initial_targets, "gpt-4");
        let target = pool.first_target().unwrap();
        // Note: URL normalization only happens during from_config, not direct Target creation
        assert_eq!(target.url.as_str(), "https://api.openai.com/v2");
        assert_eq!(target.onwards_key, Some("new-key".to_string()));
        assert_eq!(target.onwards_model, Some("gpt-4-turbo".to_string()));
    }

    #[test]
    fn test_from_config_merges_global_keys_with_target_keys() {
        let mut target_keys = HashSet::new();
        target_keys.insert(ConstantTimeString::from("target-key-1".to_string()));
        target_keys.insert(ConstantTimeString::from("target-key-2".to_string()));
        target_keys.insert(ConstantTimeString::from("shared-key".to_string()));

        let mut global_keys = HashSet::new();
        global_keys.insert(ConstantTimeString::from("global-key-1".to_string()));
        global_keys.insert(ConstantTimeString::from("global-key-2".to_string()));
        global_keys.insert(ConstantTimeString::from("shared-key".to_string())); // Duplicate with target

        let mut targets = HashMap::new();
        targets.insert(
            "test-model".to_string(),
            TargetSpecOrList::Single(
                TargetSpec::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .onwards_key("test-key".to_string())
                    .keys(target_keys)
                    .build(),
            ),
        );

        let config_file = ConfigFile {
            targets,
            auth: Some(Auth {
                global_keys,
                key_definitions: None,
            }),
            strict_mode: false,
            http_pool: None,
        };

        let targets = Targets::from_config(config_file).unwrap();
        let pool = default_pool(&targets, "test-model");

        // Pool should have both its own keys and global keys (5 unique keys)
        let pool_keys = pool.keys().unwrap();
        assert_eq!(pool_keys.len(), 5);
        assert!(pool_keys.contains(&ConstantTimeString::from("target-key-1".to_string())));
        assert!(pool_keys.contains(&ConstantTimeString::from("target-key-2".to_string())));
        assert!(pool_keys.contains(&ConstantTimeString::from("global-key-1".to_string())));
        assert!(pool_keys.contains(&ConstantTimeString::from("global-key-2".to_string())));
        assert!(pool_keys.contains(&ConstantTimeString::from("shared-key".to_string())));
    }

    #[test]
    fn test_from_config_target_without_keys_gets_global_keys() {
        let mut global_keys = HashSet::new();
        global_keys.insert(ConstantTimeString::from("global-key-1".to_string()));
        global_keys.insert(ConstantTimeString::from("global-key-2".to_string()));

        let mut targets = HashMap::new();
        targets.insert(
            "test-model".to_string(),
            TargetSpecOrList::Single(
                TargetSpec::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .onwards_key("test-key".to_string())
                    .build(),
            ),
        );

        let config_file = ConfigFile {
            targets,
            auth: Some(Auth {
                global_keys,
                key_definitions: None,
            }),
            strict_mode: false,
            http_pool: None,
        };

        let targets = Targets::from_config(config_file).unwrap();
        let pool = default_pool(&targets, "test-model");

        // Pool without explicit keys should get global keys
        let pool_keys = pool.keys().unwrap();
        assert_eq!(pool_keys.len(), 2);
        assert!(pool_keys.contains(&ConstantTimeString::from("global-key-1".to_string())));
        assert!(pool_keys.contains(&ConstantTimeString::from("global-key-2".to_string())));
    }

    #[test]
    fn test_target_with_rate_limit_config() {
        let mut targets = HashMap::new();
        targets.insert(
            "test-model".to_string(),
            TargetSpecOrList::Single(
                TargetSpec::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .rate_limit(RateLimitParameters {
                        requests_per_second: NonZeroU32::new(10).unwrap(),
                        burst_size: Some(NonZeroU32::new(20).unwrap()),
                    })
                    .build(),
            ),
        );

        let config_file = ConfigFile {
            targets,
            auth: None,
            strict_mode: false,
            http_pool: None,
        };

        let targets = Targets::from_config(config_file).unwrap();
        let pool = default_pool(&targets, "test-model");
        let target = pool.first_target().unwrap();

        // Target should have rate limiter configured
        assert!(target.limiter.is_some());
    }

    #[test]
    fn test_target_without_rate_limit_config() {
        let mut targets = HashMap::new();
        targets.insert(
            "test-model".to_string(),
            TargetSpecOrList::Single(
                TargetSpec::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .build(),
            ),
        );

        let config_file = ConfigFile {
            targets,
            auth: None,
            strict_mode: false,
            http_pool: None,
        };

        let targets = Targets::from_config(config_file).unwrap();
        let pool = default_pool(&targets, "test-model");
        let target = pool.first_target().unwrap();

        // Target should not have rate limiter
        assert!(target.limiter.is_none());
    }

    #[derive(Debug)]
    struct MockRateLimiter {
        should_allow: std::sync::Mutex<bool>,
    }

    impl MockRateLimiter {
        fn new(should_allow: bool) -> Self {
            Self {
                should_allow: std::sync::Mutex::new(should_allow),
            }
        }

        fn set_should_allow(&self, allow: bool) {
            *self.should_allow.lock().unwrap() = allow;
        }
    }

    impl RateLimiter for MockRateLimiter {
        fn check(&self) -> Result<(), RateLimitExceeded> {
            if *self.should_allow.lock().unwrap() {
                Ok(())
            } else {
                Err(RateLimitExceeded)
            }
        }
    }

    #[test]
    fn test_rate_limiter_trait_allows_requests() {
        let limiter = MockRateLimiter::new(true);
        assert!(limiter.check().is_ok());
    }

    #[test]
    fn test_rate_limiter_trait_blocks_requests() {
        let limiter = MockRateLimiter::new(false);
        assert!(limiter.check().is_err());
    }

    #[test]
    fn test_rate_limiter_trait_can_change_state() {
        let limiter = MockRateLimiter::new(true);
        assert!(limiter.check().is_ok());

        limiter.set_should_allow(false);
        assert!(limiter.check().is_err());

        limiter.set_should_allow(true);
        assert!(limiter.check().is_ok());
    }

    #[test]
    fn test_per_key_rate_limiting_with_actual_key() {
        use std::collections::HashMap;
        use std::num::NonZeroU32;

        // Create key definitions with rate limits
        let mut key_definitions = HashMap::new();
        key_definitions.insert(
            "basic_user".to_string(),
            KeyDefinition {
                key: "sk-user-123".to_string(),
                rate_limit: Some(RateLimitParameters {
                    requests_per_second: NonZeroU32::new(10).unwrap(),
                    burst_size: Some(NonZeroU32::new(20).unwrap()),
                }),
                concurrency_limit: None,
                labels: HashMap::new(),
            },
        );

        let config_file = ConfigFile {
            targets: HashMap::new(),
            auth: Some(Auth {
                global_keys: std::collections::HashSet::new(),
                key_definitions: Some(key_definitions),
            }),
            strict_mode: false,
            http_pool: None,
        };

        let targets = Targets::from_config(config_file).unwrap();

        // The actual API key should have a rate limiter
        assert!(targets.key_rate_limiters.contains_key("sk-user-123"));

        // First request should be allowed
        let limiter = targets.key_rate_limiters.get("sk-user-123").unwrap();
        assert!(limiter.check().is_ok());
    }

    #[test]
    fn test_per_key_rate_limiting_with_literal_key() {
        use std::collections::HashMap;

        let config_file = ConfigFile {
            targets: HashMap::new(),
            auth: None,
            strict_mode: false,
            http_pool: None,
        };

        let targets = Targets::from_config(config_file).unwrap();

        // Literal keys should not have rate limiters
        assert!(!targets.key_rate_limiters.contains_key("sk-literal-key"));
    }

    #[test]
    fn test_per_key_rate_limiting_no_rate_limit_configured() {
        use std::collections::HashMap;

        // Create key definition without rate limits
        let mut key_definitions = HashMap::new();
        key_definitions.insert(
            "unlimited_user".to_string(),
            KeyDefinition {
                key: "sk-unlimited-123".to_string(),
                rate_limit: None,
                concurrency_limit: None,
                labels: HashMap::new(),
            },
        );

        let config_file = ConfigFile {
            targets: HashMap::new(),
            auth: Some(Auth {
                global_keys: std::collections::HashSet::new(),
                key_definitions: Some(key_definitions),
            }),
            strict_mode: false,
            http_pool: None,
        };

        let targets = Targets::from_config(config_file).unwrap();

        // Key without rate limits should not have a rate limiter
        assert!(!targets.key_rate_limiters.contains_key("sk-unlimited-123"));
    }

    #[test]
    fn test_from_config_no_global_keys() {
        let mut target_keys = HashSet::new();
        target_keys.insert(ConstantTimeString::from("target-key-1".to_string()));
        target_keys.insert(ConstantTimeString::from("target-key-2".to_string()));

        let mut targets = HashMap::new();
        targets.insert(
            "model-with-keys".to_string(),
            TargetSpecOrList::Single(
                TargetSpec::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .onwards_key("test-key".to_string())
                    .keys(target_keys)
                    .build(),
            ),
        );
        targets.insert(
            "model-without-keys".to_string(),
            TargetSpecOrList::Single(
                TargetSpec::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .onwards_key("test-key".to_string())
                    .build(),
            ),
        );

        let config_file = ConfigFile {
            targets,
            auth: None,
            strict_mode: false,
            http_pool: None,
        };

        let targets = Targets::from_config(config_file).unwrap();

        // Pool with keys should keep them unchanged
        let pool_with_keys = default_pool(&targets, "model-with-keys");
        let pool_keys = pool_with_keys.keys().unwrap();
        assert_eq!(pool_keys.len(), 2);
        assert!(pool_keys.contains(&ConstantTimeString::from("target-key-1".to_string())));
        assert!(pool_keys.contains(&ConstantTimeString::from("target-key-2".to_string())));

        // Pool without keys should remain None
        let pool_without_keys = default_pool(&targets, "model-without-keys");
        assert!(pool_without_keys.keys().is_none());
    }

    #[test]
    fn test_normalize_url_adds_trailing_slash() {
        // URL without trailing slash should get one added
        let url_without_slash: Url = "https://api.example.com/v1".parse().unwrap();
        let normalized = super::normalize_url(url_without_slash);
        assert_eq!(normalized.as_str(), "https://api.example.com/v1/");

        // URL with trailing slash should remain unchanged
        let url_with_slash: Url = "https://api.example.com/v1/".parse().unwrap();
        let normalized = super::normalize_url(url_with_slash);
        assert_eq!(normalized.as_str(), "https://api.example.com/v1/");

        // Root URL without path should get trailing slash
        let root_url: Url = "https://api.example.com".parse().unwrap();
        let normalized = super::normalize_url(root_url);
        assert_eq!(normalized.as_str(), "https://api.example.com/");
    }

    #[test]
    fn test_url_joining_after_normalization() {
        // Test that normalized URLs join correctly with paths
        let base_url: Url = "https://api.example.com/v1".parse().unwrap();
        let normalized = super::normalize_url(base_url);

        // Should append path segments correctly
        let joined = normalized.join("messages").unwrap();
        assert_eq!(joined.as_str(), "https://api.example.com/v1/messages");

        // Should also work with leading slash
        let normalized_again: Url = "https://api.example.com/v1".parse().unwrap();
        let normalized_again = super::normalize_url(normalized_again);
        let joined_with_slash = normalized_again.join("messages/create").unwrap();
        assert_eq!(
            joined_with_slash.as_str(),
            "https://api.example.com/v1/messages/create"
        );
    }

    #[test]
    fn test_target_spec_conversion_normalizes_url() {
        let target_spec = TargetSpec::builder()
            .url("https://api.example.com/v1".parse().unwrap())
            .onwards_key("test-key".to_string())
            .build();

        let target: Target = target_spec.into();

        // URL should have trailing slash after conversion
        assert_eq!(target.url.as_str(), "https://api.example.com/v1/");
    }

    #[test]
    fn test_target_with_concurrency_limit_config() {
        let mut targets = HashMap::new();
        targets.insert(
            "test-model".to_string(),
            TargetSpecOrList::Single(
                TargetSpec::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .concurrency_limit(ConcurrencyLimitParameters {
                        max_concurrent_requests: 5,
                    })
                    .weight(1)
                    .build(),
            ),
        );

        let config_file = ConfigFile {
            targets,
            auth: None,
            strict_mode: false,
            http_pool: None,
        };

        let targets = Targets::from_config(config_file).unwrap();
        let pool = default_pool(&targets, "test-model");

        // Provider should have concurrency limiter configured (limit of 5)
        // Verify by selecting 5 times (all succeed) then a 6th (fails)
        let guards: Vec<_> = (0..5).filter_map(|_| pool.select()).collect();
        assert_eq!(guards.len(), 5);
        assert!(pool.select().is_none());
    }

    #[test]
    fn test_target_without_concurrency_limit_config() {
        let mut targets = HashMap::new();
        targets.insert(
            "test-model".to_string(),
            TargetSpecOrList::Single(
                TargetSpec::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .weight(1)
                    .build(),
            ),
        );

        let config_file = ConfigFile {
            targets,
            auth: None,
            strict_mode: false,
            http_pool: None,
        };

        let targets = Targets::from_config(config_file).unwrap();
        let pool = default_pool(&targets, "test-model");

        // Provider should not have concurrency limiter (unlimited selects)
        let guards: Vec<_> = (0..100).filter_map(|_| pool.select()).collect();
        assert_eq!(guards.len(), 100);
    }

    #[test]
    fn test_concurrency_limiter_allows_requests() {
        let limiter = ConcurrencyLimiter::with_limit(2);

        // First request should succeed
        let guard1 = limiter.try_acquire();
        assert!(guard1.is_some());

        // Second request should succeed
        let guard2 = limiter.try_acquire();
        assert!(guard2.is_some());
    }

    #[test]
    fn test_concurrency_limiter_blocks_at_capacity() {
        let limiter = ConcurrencyLimiter::with_limit(2);

        // Acquire all available permits
        let _guard1 = limiter.try_acquire().unwrap();
        let _guard2 = limiter.try_acquire().unwrap();

        // Third request should fail
        let guard3 = limiter.try_acquire();
        assert!(guard3.is_none());
    }

    #[test]
    fn test_concurrency_limiter_releases_on_drop() {
        let limiter = ConcurrencyLimiter::with_limit(1);

        {
            // Acquire the only permit
            let _guard = limiter.try_acquire().unwrap();

            // Second request should fail while guard is held
            assert!(limiter.try_acquire().is_none());
        } // guard drops here

        // After guard is dropped, should be able to acquire again
        let guard = limiter.try_acquire();
        assert!(guard.is_some());
    }

    #[test]
    fn test_per_key_concurrency_limiting_configured() {
        use std::collections::HashMap;

        // Create key definitions with concurrency limits
        let mut key_definitions = HashMap::new();
        key_definitions.insert(
            "limited_user".to_string(),
            KeyDefinition {
                key: "sk-limited-123".to_string(),
                rate_limit: None,
                concurrency_limit: Some(ConcurrencyLimitParameters {
                    max_concurrent_requests: 3,
                }),
                labels: HashMap::new(),
            },
        );

        let config_file = ConfigFile {
            targets: HashMap::new(),
            auth: Some(Auth {
                global_keys: std::collections::HashSet::new(),
                key_definitions: Some(key_definitions),
            }),
            strict_mode: false,
            http_pool: None,
        };

        let targets = Targets::from_config(config_file).unwrap();

        // The actual API key should have a concurrency limiter
        assert!(
            targets
                .key_concurrency_limiters
                .contains_key("sk-limited-123")
        );
    }

    #[test]
    fn test_per_key_concurrency_limiting_not_configured() {
        use std::collections::HashMap;

        // Create key definition without concurrency limits
        let mut key_definitions = HashMap::new();
        key_definitions.insert(
            "unlimited_user".to_string(),
            KeyDefinition {
                key: "sk-unlimited-456".to_string(),
                rate_limit: None,
                concurrency_limit: None,
                labels: HashMap::new(),
            },
        );

        let config_file = ConfigFile {
            targets: HashMap::new(),
            auth: Some(Auth {
                global_keys: std::collections::HashSet::new(),
                key_definitions: Some(key_definitions),
            }),
            strict_mode: false,
            http_pool: None,
        };

        let targets = Targets::from_config(config_file).unwrap();

        // Key without concurrency limits should not have a concurrency limiter
        assert!(
            !targets
                .key_concurrency_limiters
                .contains_key("sk-unlimited-456")
        );
    }

    #[test]
    fn test_fallback_config_wildcard_status_codes() {
        let config = FallbackConfig {
            enabled: true,
            on_status: vec![5, 429], // 5 = all 5xx codes, 429 = exact match
            on_rate_limit: false,
            ..Default::default()
        };

        // 5 should match all 5xx codes
        assert!(config.should_fallback_on_status(500));
        assert!(config.should_fallback_on_status(502));
        assert!(config.should_fallback_on_status(503));
        assert!(config.should_fallback_on_status(599));

        // 429 exact match
        assert!(config.should_fallback_on_status(429));

        // Should not match other codes
        assert!(!config.should_fallback_on_status(400));
        assert!(!config.should_fallback_on_status(404));
        assert!(!config.should_fallback_on_status(200));
    }

    #[test]
    fn test_fallback_config_two_digit_wildcard() {
        let config = FallbackConfig {
            enabled: true,
            on_status: vec![50, 52], // 50 = 500-509, 52 = 520-529
            on_rate_limit: false,
            ..Default::default()
        };

        // 50 matches 500-509
        assert!(config.should_fallback_on_status(500));
        assert!(config.should_fallback_on_status(504));
        assert!(config.should_fallback_on_status(509));
        assert!(!config.should_fallback_on_status(510));

        // 52 matches 520-529
        assert!(config.should_fallback_on_status(520));
        assert!(config.should_fallback_on_status(522));
        assert!(!config.should_fallback_on_status(530));
    }

    #[test]
    fn test_fallback_config_disabled_ignores_status() {
        let config = FallbackConfig {
            enabled: false,
            on_status: vec![5, 429],
            on_rate_limit: true,
            ..Default::default()
        };

        // Even matching codes should return false when disabled
        assert!(!config.should_fallback_on_status(500));
        assert!(!config.should_fallback_on_status(429));
    }

    #[test]
    fn test_backwards_compat_single_target_config() {
        // Old single-target format
        let json = r#"{
            "targets": {
                "gpt-4": {
                    "url": "https://api.openai.com",
                    "onwards_key": "sk-test"
                }
            }
        }"#;

        let config: ConfigFile = serde_json::from_str(json).unwrap();
        let targets = Targets::from_config(config).unwrap();

        let pool = default_pool(&targets, "gpt-4");
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.strategy(), LoadBalanceStrategy::WeightedRandom); // default
        assert!(!pool.fallback_enabled());
    }

    #[test]
    fn test_backwards_compat_list_format_config() {
        // Old list format
        let json = r#"{
            "targets": {
                "gpt-4": [
                    { "url": "https://api1.example.com", "weight": 3 },
                    { "url": "https://api2.example.com", "weight": 1 }
                ]
            }
        }"#;

        let config: ConfigFile = serde_json::from_str(json).unwrap();
        let targets = Targets::from_config(config).unwrap();

        let pool = default_pool(&targets, "gpt-4");
        assert_eq!(pool.len(), 2);
        assert_eq!(pool.strategy(), LoadBalanceStrategy::WeightedRandom); // default
    }

    #[test]
    fn test_pool_config_with_strategy() {
        // New pool format with explicit strategy
        let json = r#"{
            "targets": {
                "gpt-4": {
                    "strategy": "priority",
                    "fallback": {
                        "enabled": true,
                        "on_status": [429, 5],
                        "on_rate_limit": true
                    },
                    "providers": [
                        { "url": "https://primary.example.com" },
                        { "url": "https://backup.example.com" }
                    ]
                }
            }
        }"#;

        let config: ConfigFile = serde_json::from_str(json).unwrap();
        let targets = Targets::from_config(config).unwrap();

        let pool = default_pool(&targets, "gpt-4");
        assert_eq!(pool.len(), 2);
        assert_eq!(pool.strategy(), LoadBalanceStrategy::Priority);
        assert!(pool.fallback_enabled());
        assert!(pool.should_fallback_on_status(429));
        assert!(pool.should_fallback_on_status(500));
        assert!(pool.should_fallback_on_rate_limit());
    }

    #[test]
    fn test_pool_config_weighted_random_strategy() {
        let json = r#"{
            "targets": {
                "gpt-4": {
                    "strategy": "weighted_random",
                    "providers": [
                        { "url": "https://api1.example.com", "weight": 3 },
                        { "url": "https://api2.example.com", "weight": 1 }
                    ]
                }
            }
        }"#;

        let config: ConfigFile = serde_json::from_str(json).unwrap();
        let targets = Targets::from_config(config).unwrap();

        let pool = default_pool(&targets, "gpt-4");
        assert_eq!(pool.strategy(), LoadBalanceStrategy::WeightedRandom);
    }

    #[test]
    fn test_into_pool_preserves_sanitize_response() {
        // Create a target with sanitize_response enabled
        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .sanitize_response(true)
            .build();

        // Convert to pool
        let pool = target.into_pool();

        // Verify the target's sanitize_response is accessible through the pool
        let (_, first_target, _guard) = pool.default_pool().select_iter().next().unwrap();
        assert!(
            first_target.sanitize_response,
            "into_pool should preserve sanitize_response setting"
        );
    }

    #[test]
    fn test_into_pool_preserves_sanitize_response_disabled() {
        // Create a target with sanitize_response disabled (default)
        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        // Convert to pool
        let pool = target.into_pool();

        // Verify the target's sanitize_response is accessible through the pool
        let (_, first_target, _guard) = pool.default_pool().select_iter().next().unwrap();
        assert!(
            !first_target.sanitize_response,
            "into_pool should preserve default sanitize_response setting"
        );
    }

    #[test]
    fn test_single_target_config_with_timeout() {
        // Test that request_timeout_secs is properly deserialized in single-target format
        let json = r#"{
            "targets": {
                "gpt-4": {
                    "url": "https://api.openai.com/v1/",
                    "onwards_key": "sk-test-key",
                    "request_timeout_secs": 30
                }
            }
        }"#;

        let config: ConfigFile = serde_json::from_str(json).unwrap();
        let targets = Targets::from_config(config).unwrap();

        let pool = default_pool(&targets, "gpt-4");
        let target = pool.first_target().unwrap();
        assert_eq!(target.request_timeout_secs, Some(30));
    }

    #[test]
    fn test_trusted_field_defaults_to_false() {
        let json = r#"{
            "targets": {
                "test-model": {
                    "url": "https://api.example.com",
                    "onwards_key": "sk-test"
                }
            }
        }"#;

        let config: ConfigFile = serde_json::from_str(json).unwrap();
        let targets = Targets::from_config(config).unwrap();

        let pool = default_pool(&targets, "test-model");
        assert!(!pool.is_trusted(), "trusted should default to false");
    }

    #[test]
    fn test_trusted_field_set_to_true() {
        let json = r#"{
            "targets": {
                "test-model": {
                    "url": "https://api.example.com",
                    "onwards_key": "sk-test",
                    "trusted": true
                }
            }
        }"#;

        let config: ConfigFile = serde_json::from_str(json).unwrap();
        let targets = Targets::from_config(config).unwrap();

        let pool = default_pool(&targets, "test-model");
        assert!(
            pool.is_trusted(),
            "trusted should be true when explicitly set"
        );
    }

    #[test]
    fn test_trusted_field_preserved_in_pool_conversion() {
        // Test PoolSpec -> PoolConfig conversion
        let pool_spec = PoolSpec {
            keys: None,
            rate_limit: None,
            concurrency_limit: None,
            response_headers: None,
            fallback: None,
            strategy: LoadBalanceStrategy::default(),
            sanitize_response: false,
            open_responses: None,
            trusted: true,
            routing_rules: Vec::new(),
            providers: vec![ProviderSpec {
                url: "https://api.example.com".parse().unwrap(),
                onwards_key: None,
                onwards_model: None,
                rate_limit: None,
                concurrency_limit: None,
                upstream_auth_header_name: None,
                upstream_auth_header_prefix: None,
                response_headers: None,
                weight: 1,
                sanitize_response: false,
                open_responses: None,
                request_timeout_secs: None,
                trusted: None,
                propagate_trace_context: None,
                reasoning_translation: None,
            }],
        };

        let pool_config = TargetSpecOrList::Pool(pool_spec)
            .into_pool_config()
            .unwrap();
        assert!(
            pool_config.trusted,
            "PoolSpec conversion should preserve trusted field"
        );

        // Test TargetSpec (single provider) -> PoolConfig conversion via JSON
        let json = r#"{
            "targets": {
                "test-model": {
                    "url": "https://api.example.com",
                    "trusted": true
                }
            }
        }"#;

        let config: ConfigFile = serde_json::from_str(json).unwrap();
        let targets = Targets::from_config(config).unwrap();

        let pool = default_pool(&targets, "test-model");
        assert!(
            pool.is_trusted(),
            "Single TargetSpec with trusted: true should create trusted pool"
        );
    }

    #[test]
    fn test_legacy_list_mixed_trusted_values_rejected() {
        // Test that legacy list format with mixed trusted values is rejected
        let json = r#"{
            "targets": {
                "test-model": [
                    {
                        "url": "https://api.example1.com",
                        "trusted": true
                    },
                    {
                        "url": "https://api.example2.com",
                        "trusted": false
                    }
                ]
            }
        }"#;

        let config: ConfigFile = serde_json::from_str(json).unwrap();
        let result = Targets::from_config(config);

        assert!(
            result.is_err(),
            "Config with mixed trusted values in legacy list should be rejected"
        );

        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("same 'trusted' value"),
            "Error message should mention trusted value mismatch, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_legacy_list_carries_per_provider_propagate_trace_context() {
        // Regression: the legacy list format must carry each provider's
        // explicit `propagate_trace_context` through to the ProviderSpec
        // (the `trusted` field is forced uniform in this format, but
        // trace-context overrides are honoured per-provider). Previously
        // this was dropped to None, silently ignoring operator config.
        let list: TargetSpecOrList = serde_json::from_str(
            r#"[
                { "url": "https://provider1.example.com", "propagate_trace_context": true },
                { "url": "https://provider2.example.com", "propagate_trace_context": false },
                { "url": "https://provider3.example.com" }
            ]"#,
        )
        .unwrap();

        let pool_config = list.into_pool_config().unwrap();
        assert_eq!(pool_config.providers.len(), 3);
        assert_eq!(
            pool_config.providers[0].propagate_trace_context,
            Some(true),
            "explicit true must survive"
        );
        assert_eq!(
            pool_config.providers[1].propagate_trace_context,
            Some(false),
            "explicit false must survive"
        );
        assert_eq!(
            pool_config.providers[2].propagate_trace_context, None,
            "unset stays None (inherits resolved trusted)"
        );
    }

    #[test]
    fn test_single_target_config_without_timeout() {
        // Test that request_timeout_secs defaults to None when not specified
        let json = r#"{
            "targets": {
                "gpt-4": {
                    "url": "https://api.openai.com/v1/",
                    "onwards_key": "sk-test-key"
                }
            }
        }"#;

        let config: ConfigFile = serde_json::from_str(json).unwrap();
        let targets = Targets::from_config(config).unwrap();

        let pool = default_pool(&targets, "gpt-4");
        let target = pool.first_target().unwrap();
        assert_eq!(target.request_timeout_secs, None);
    }

    #[test]
    fn test_pool_config_with_timeout() {
        // Test that request_timeout_secs is properly deserialized in pool/providers format
        let json = r#"{
            "targets": {
                "gpt-4": {
                    "providers": [
                        {
                            "url": "https://api.openai.com/v1/",
                            "onwards_key": "sk-test-key-1",
                            "request_timeout_secs": 30,
                            "weight": 1
                        },
                        {
                            "url": "https://api.azure.com/v1/",
                            "onwards_key": "sk-test-key-2",
                            "request_timeout_secs": 60,
                            "weight": 1
                        }
                    ],
                    "fallback": {
                        "enabled": true,
                        "on_status": [502, 503]
                    }
                }
            }
        }"#;

        let config: ConfigFile = serde_json::from_str(json).unwrap();
        let targets = Targets::from_config(config).unwrap();

        let pool = default_pool(&targets, "gpt-4");
        assert_eq!(pool.len(), 2);

        // Check both providers have their respective timeouts
        let providers = pool.providers();

        // First provider should have 30 second timeout
        let provider1 = providers
            .iter()
            .find(|p| p.target.url.host_str() == Some("api.openai.com"))
            .unwrap();
        assert_eq!(provider1.target.request_timeout_secs, Some(30));

        // Second provider should have 60 second timeout
        let provider2 = providers
            .iter()
            .find(|p| p.target.url.host_str() == Some("api.azure.com"))
            .unwrap();
        assert_eq!(provider2.target.request_timeout_secs, Some(60));
    }

    #[test]
    fn test_pool_config_mixed_timeouts() {
        // Test that some providers can have timeouts while others don't
        let json = r#"{
            "targets": {
                "gpt-4": {
                    "providers": [
                        {
                            "url": "https://api.openai.com/v1/",
                            "onwards_key": "sk-test-key-1",
                            "request_timeout_secs": 30,
                            "weight": 1
                        },
                        {
                            "url": "https://api.azure.com/v1/",
                            "onwards_key": "sk-test-key-2",
                            "weight": 1
                        }
                    ]
                }
            }
        }"#;

        let config: ConfigFile = serde_json::from_str(json).unwrap();
        let targets = Targets::from_config(config).unwrap();

        let pool = default_pool(&targets, "gpt-4");
        let providers = pool.providers();

        // One provider with timeout
        assert!(
            providers
                .iter()
                .any(|p| p.target.request_timeout_secs == Some(30))
        );
        // One provider without timeout
        assert!(
            providers
                .iter()
                .any(|p| p.target.request_timeout_secs.is_none())
        );
    }

    #[test]
    fn test_trusted_field_in_pool_config() {
        // Test pool with trusted flag set
        let json_trusted = r#"{
            "targets": {
                "trusted-pool": {
                    "trusted": true,
                    "providers": [
                        {
                            "url": "https://api1.example.com"
                        },
                        {
                            "url": "https://api2.example.com"
                        }
                    ]
                }
            }
        }"#;

        let config: ConfigFile = serde_json::from_str(json_trusted).unwrap();
        let targets = Targets::from_config(config).unwrap();

        let pool = default_pool(&targets, "trusted-pool");
        assert!(
            pool.is_trusted(),
            "Pool with trusted: true should be trusted"
        );
        assert_eq!(pool.len(), 2, "Pool should have 2 providers");

        // Test pool without trusted flag (defaults to false)
        let json_untrusted = r#"{
            "targets": {
                "untrusted-pool": {
                    "providers": [
                        {
                            "url": "https://api1.example.com"
                        },
                        {
                            "url": "https://api2.example.com"
                        }
                    ]
                }
            }
        }"#;

        let config: ConfigFile = serde_json::from_str(json_untrusted).unwrap();
        let targets = Targets::from_config(config).unwrap();

        let pool = default_pool(&targets, "untrusted-pool");
        assert!(
            !pool.is_trusted(),
            "Pool without trusted flag should default to false"
        );
    }

    #[test]
    fn test_provider_spec_trusted_defaults_to_none() {
        let json = r#"{"url": "https://example.com"}"#;
        let spec: ProviderSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.trusted, None);
    }

    #[test]
    fn test_provider_spec_trusted_some_true() {
        let json = r#"{"url": "https://example.com", "trusted": true}"#;
        let spec: ProviderSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.trusted, Some(true));
    }

    #[test]
    fn test_provider_spec_trusted_some_false() {
        let json = r#"{"url": "https://example.com", "trusted": false}"#;
        let spec: ProviderSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.trusted, Some(false));
    }

    #[test]
    fn test_target_from_provider_spec_propagates_trusted() {
        let spec: ProviderSpec =
            serde_json::from_str(r#"{"url": "https://example.com", "trusted": true}"#).unwrap();
        let target: Target = spec.into();
        assert_eq!(target.trusted, Some(true));
    }

    #[test]
    fn test_target_from_provider_spec_trusted_none() {
        let spec: ProviderSpec = serde_json::from_str(r#"{"url": "https://example.com"}"#).unwrap();
        let target: Target = spec.into();
        assert_eq!(target.trusted, None);
    }

    #[test]
    fn test_per_provider_trusted_parsed_from_config() {
        let json = r#"{
            "targets": {
                "gpt-4": {
                    "trusted": false,
                    "providers": [
                        {"url": "https://internal.example.com", "trusted": true},
                        {"url": "https://external.example.com"}
                    ]
                }
            }
        }"#;

        let config: ConfigFile = serde_json::from_str(json).unwrap();
        let targets = Targets::from_config(config).unwrap();
        let pool = default_pool(&targets, "gpt-4");

        assert!(!pool.is_trusted(), "Pool-level trusted should be false");

        let providers = pool.providers();
        assert_eq!(
            providers[0].target.trusted,
            Some(true),
            "First provider should have trusted=Some(true)"
        );
        assert_eq!(
            providers[1].target.trusted, None,
            "Second provider should have trusted=None (inherits pool)"
        );
    }

    #[test]
    fn test_routing_rules_deserialized_from_config() {
        let json = r#"{
            "targets": {
                "gpt-4": {
                    "routing_rules": [
                        {
                            "match_labels": {"purpose": "playground"},
                            "action": {"type": "deny"}
                        },
                        {
                            "match_labels": {"purpose": "batch"},
                            "action": {"type": "redirect", "target": "gpt-4o-mini"}
                        }
                    ],
                    "providers": [
                        { "url": "https://api.openai.com/v1/" }
                    ]
                }
            }
        }"#;

        let config: ConfigFile = serde_json::from_str(json).unwrap();
        let targets = Targets::from_config(config).unwrap();

        let pool = default_pool(&targets, "gpt-4");
        let rules = pool.routing_rules();
        assert_eq!(rules.len(), 2);

        // First rule: deny playground
        assert_eq!(rules[0].match_labels.get("purpose").unwrap(), "playground");
        assert!(matches!(rules[0].action, RoutingAction::Deny));

        // Second rule: redirect batch
        assert_eq!(rules[1].match_labels.get("purpose").unwrap(), "batch");
        match &rules[1].action {
            RoutingAction::Redirect { target } => assert_eq!(target, "gpt-4o-mini"),
            other => panic!("Expected Redirect, got {:?}", other),
        }
    }

    #[test]
    fn test_key_labels_wired_through_config() {
        let mut key_definitions = HashMap::new();
        key_definitions.insert(
            "batch_user".to_string(),
            KeyDefinition {
                key: "sk-batch-123".to_string(),
                rate_limit: None,
                concurrency_limit: None,
                labels: HashMap::from([("purpose".to_string(), "batch".to_string())]),
            },
        );
        key_definitions.insert(
            "playground_user".to_string(),
            KeyDefinition {
                key: "sk-play-456".to_string(),
                rate_limit: None,
                concurrency_limit: None,
                labels: HashMap::from([("purpose".to_string(), "playground".to_string())]),
            },
        );
        key_definitions.insert(
            "no_labels_user".to_string(),
            KeyDefinition {
                key: "sk-nolabel-789".to_string(),
                rate_limit: None,
                concurrency_limit: None,
                labels: HashMap::new(),
            },
        );

        let config_file = ConfigFile {
            targets: HashMap::new(),
            auth: Some(Auth {
                global_keys: std::collections::HashSet::new(),
                key_definitions: Some(key_definitions),
            }),
            strict_mode: false,
            http_pool: None,
        };

        let targets = Targets::from_config(config_file).unwrap();

        // Keys with labels should be in key_labels
        let batch_labels = targets.key_labels.get("sk-batch-123").unwrap();
        assert_eq!(batch_labels.get("purpose").unwrap(), "batch");

        let play_labels = targets.key_labels.get("sk-play-456").unwrap();
        assert_eq!(play_labels.get("purpose").unwrap(), "playground");

        // Key without labels should NOT be in key_labels
        assert!(!targets.key_labels.contains_key("sk-nolabel-789"));
    }

    #[test]
    fn test_routing_rules_empty_by_default() {
        // Single target format has no routing_rules field
        let json = r#"{
            "targets": {
                "gpt-4": {
                    "url": "https://api.openai.com",
                    "onwards_key": "sk-test"
                }
            }
        }"#;

        let config: ConfigFile = serde_json::from_str(json).unwrap();
        let targets = Targets::from_config(config).unwrap();

        let pool = default_pool(&targets, "gpt-4");
        assert!(pool.routing_rules().is_empty());
    }

    fn backoff(initial_ms: u64, max_ms: u64, factor: f64, jitter: JitterStrategy) -> BackoffConfig {
        BackoffConfig {
            initial_ms,
            max_ms,
            factor,
            jitter,
        }
    }

    #[test]
    fn test_backoff_first_retry_is_initial() {
        let b = backoff(100, 5_000, 2.0, JitterStrategy::None);
        assert_eq!(b.base_delay_ms(1), 100);
    }

    #[test]
    fn test_backoff_exponential_growth() {
        let b = backoff(100, 5_000, 2.0, JitterStrategy::None);
        assert_eq!(b.base_delay_ms(1), 100);
        assert_eq!(b.base_delay_ms(2), 200);
        assert_eq!(b.base_delay_ms(3), 400);
        assert_eq!(b.base_delay_ms(4), 800);
        assert_eq!(b.base_delay_ms(5), 1_600);
        assert_eq!(b.base_delay_ms(6), 3_200);
    }

    #[test]
    fn test_backoff_clamped_to_max_ms() {
        let b = backoff(100, 1_000, 2.0, JitterStrategy::None);
        // Without clamping these would be 1600, 3200, ...
        assert_eq!(b.base_delay_ms(5), 1_000);
        assert_eq!(b.base_delay_ms(20), 1_000);
    }

    #[test]
    fn test_backoff_zero_retry_index_is_zero() {
        // retry_index 0 means "no retry yet" — the original attempt does not sleep.
        let b = backoff(100, 5_000, 2.0, JitterStrategy::None);
        assert_eq!(b.base_delay_ms(0), 0);
    }

    #[test]
    fn test_backoff_extreme_exponent_stays_bounded() {
        // Even silly inputs cannot escape the max_ms ceiling or cause overflow.
        let b = backoff(1_000, 10_000, 10.0, JitterStrategy::None);
        assert_eq!(b.base_delay_ms(100), 10_000);
        assert_eq!(b.base_delay_ms(u32::MAX), 10_000);
    }

    #[test]
    fn test_backoff_factor_one_is_constant() {
        let b = backoff(250, 5_000, 1.0, JitterStrategy::None);
        for retry in 1..=10 {
            assert_eq!(b.base_delay_ms(retry), 250);
        }
    }

    #[test]
    fn test_backoff_full_jitter_within_bounds() {
        let b = backoff(200, 5_000, 2.0, JitterStrategy::Full);
        // delay(3) base = 800; full jitter should keep samples within [0, 800].
        for _ in 0..256 {
            let d = b.delay(3).as_millis() as u64;
            assert!(d <= 800, "jittered delay {d} exceeded base 800");
        }
    }

    #[test]
    fn test_backoff_none_jitter_is_deterministic() {
        let b = backoff(150, 5_000, 2.0, JitterStrategy::None);
        let first = b.delay(2);
        for _ in 0..16 {
            assert_eq!(b.delay(2), first);
        }
        assert_eq!(first, std::time::Duration::from_millis(300));
    }

    #[test]
    fn test_backoff_serde_defaults() {
        // Operators should be able to specify just `initial_ms` and pick up
        // sensible defaults for the rest.
        let json = r#"{ "initial_ms": 100 }"#;
        let b: BackoffConfig = serde_json::from_str(json).unwrap();
        assert_eq!(b.initial_ms, 100);
        assert_eq!(b.max_ms, 5_000);
        assert!((b.factor - 2.0).abs() < f64::EPSILON);
        assert_eq!(b.jitter, JitterStrategy::Full);
    }

    #[test]
    fn test_fallback_config_backoff_unset_by_default() {
        // Existing configs without `backoff` keep the legacy zero-delay behavior.
        let f: FallbackConfig = serde_json::from_str("{}").unwrap();
        assert!(f.backoff.is_none());
        assert!(f.max_total_backoff_ms.is_none());
    }

    #[test]
    fn test_fallback_config_with_backoff_and_budget() {
        let json = r#"{
            "enabled": true,
            "on_status": [502, 503],
            "max_attempts": 4,
            "max_total_backoff_ms": 3000,
            "backoff": {
                "initial_ms": 100,
                "max_ms": 1500,
                "factor": 2.0,
                "jitter": "none"
            }
        }"#;
        let f: FallbackConfig = serde_json::from_str(json).unwrap();
        assert!(f.enabled);
        assert_eq!(f.max_total_backoff_ms, Some(3000));
        let b = f.backoff.expect("backoff parsed");
        assert_eq!(b.initial_ms, 100);
        assert_eq!(b.max_ms, 1500);
        assert_eq!(b.jitter, JitterStrategy::None);
    }

    #[test]
    fn test_from_config_rejects_invalid_reasoning_translation() {
        let config: ConfigFile = serde_json::from_value(serde_json::json!({
            "targets": {
                "reasoning-model": {
                    "url": "https://provider.example.com/v1",
                    "reasoning_translation": {
                        "chat_completions": {
                            "unsupported_efforts": ["minimal", "low", "medium", "high", "xhigh", "max"],
                            "writes": [{
                                "target_path": "/thinking_token_budget",
                                "values": {"none": 0}
                            }]
                        }
                    }
                }
            }
        }))
        .unwrap();

        let error = Targets::from_config(config).unwrap_err();

        assert!(error.to_string().contains("thinking_token_budget"));
        assert!(error.to_string().contains("reasoning_effort"));
    }

    // ── named pools / request class ──────────────────────────────────────────

    /// Rule 3, at the config layer: a config nobody has touched — no `pools`
    /// key anywhere — becomes a composite whose only pool is `default`. That is
    /// what makes named pools inert for every existing composite.
    #[test]
    fn an_existing_config_becomes_a_single_default_pool() {
        let json = r#"{
            "targets": {
                "gpt-4": {
                    "providers": [
                        {"url": "https://dynamo.example.com/"},
                        {"url": "https://openrouter.example.com/"}
                    ]
                }
            }
        }"#;
        let config: ConfigFile = serde_json::from_str(json).unwrap();
        let pools = config.targets["gpt-4"].clone().into_pool_configs().unwrap();
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].0, DEFAULT_POOL);
        assert_eq!(pools[0].1.providers.len(), 2);

        // And the built composite resolves EVERY class to that one pool.
        let targets = Targets::from_config(serde_json::from_str(json).unwrap()).unwrap();
        let entry = targets.targets.get("gpt-4").unwrap();
        assert_eq!(entry.pool_count(), 1);
        for class in [RequestClass::Normal, RequestClass::Completions] {
            assert_eq!(entry.resolve(class).len(), 2);
            assert!(entry.resolved_name(class).is_none());
        }
    }

    /// Snapshot: an existing single-pool config still serializes as a bare
    /// pool, NOT as a named-pool map. The named-pool variant is additive — it
    /// must not rewrite anybody's config into a new shape, and a config written
    /// out by this build must still be readable by one without pools.
    #[test]
    fn an_existing_config_re_serializes_without_a_pools_wrapper() {
        let json = r#"{
            "targets": {
                "pool-form": {
                    "keys": ["sk-a"],
                    "strategy": "priority",
                    "providers": [
                        {"url": "https://dynamo.example.com/"},
                        {"url": "https://openrouter.example.com/"}
                    ]
                },
                "list-form": [{"url": "https://a.example.com/"}],
                "single-form": {"url": "https://b.example.com/"}
            },
            "strict_mode": false
        }"#;
        let config: ConfigFile = serde_json::from_str(json).unwrap();
        let serialized = serde_json::to_value(&config).unwrap();

        // The shape discriminators are unchanged: a pool is still `providers`,
        // a list is still an array, a single is still a bare `url` — and none
        // of them grew a `pools` key.
        let targets = &serialized["targets"];
        assert!(targets["pool-form"]["providers"].is_array());
        assert!(targets["pool-form"].get("pools").is_none());
        assert!(targets["list-form"].is_array());
        assert!(targets["single-form"]["url"].is_string());
        assert!(
            !serialized.to_string().contains("\"pools\""),
            "no existing target may serialize as a named-pool map: {serialized}"
        );

        // And the written-out config parses back to the same thing: one pool,
        // named `default`, with the same members in the same order.
        let reparsed: ConfigFile = serde_json::from_value(serialized).unwrap();
        for name in ["pool-form", "list-form", "single-form"] {
            let pools = reparsed.targets[name].clone().into_pool_configs().unwrap();
            assert_eq!(pools.len(), 1, "{name}");
            assert_eq!(pools[0].0, DEFAULT_POOL, "{name}");
        }
        let pool_form = reparsed.targets["pool-form"]
            .clone()
            .into_pool_configs()
            .unwrap();
        let urls: Vec<_> = pool_form[0]
            .1
            .providers
            .iter()
            .map(|p| p.url.to_string())
            .collect();
        assert_eq!(
            urls,
            vec![
                "https://dynamo.example.com/",
                "https://openrouter.example.com/"
            ]
        );
        assert_eq!(pool_form[0].1.strategy, LoadBalanceStrategy::Priority);
        assert_eq!(pool_form[0].1.keys.as_ref().unwrap().len(), 1);
    }

    /// The multi-pool shape: the same hosted model may sit in both pools with
    /// independent per-pool ordering, and the validated target only in
    /// `completions`.
    #[test]
    fn a_pools_target_parses_into_named_pools() {
        let json = r#"{
            "targets": {
                "dsv4-flash": {
                    "pools": {
                        "default": {
                            "strategy": "priority",
                            "providers": [
                                {"url": "https://dynamo.example.com/"},
                                {"url": "https://openrouter.example.com/"}
                            ]
                        },
                        "completions": {
                            "strategy": "priority",
                            "providers": [
                                {"url": "https://dynamo.example.com/"},
                                {"url": "https://fireworks.example.com/"}
                            ]
                        }
                    }
                }
            }
        }"#;
        let targets = Targets::from_config(serde_json::from_str(json).unwrap()).unwrap();
        let entry = targets.targets.get("dsv4-flash").unwrap();
        assert_eq!(entry.pool_count(), 2);

        let urls = |class| {
            entry
                .resolve(class)
                .providers()
                .iter()
                .map(|p| p.target.url.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            urls(RequestClass::Normal),
            vec![
                "https://dynamo.example.com/",
                "https://openrouter.example.com/"
            ]
        );
        assert_eq!(
            urls(RequestClass::Completions),
            vec![
                "https://dynamo.example.com/",
                "https://fireworks.example.com/"
            ]
        );
        assert_eq!(
            entry.resolved_name(RequestClass::Completions),
            Some(COMPLETIONS_POOL)
        );
        assert!(entry.resolved_name(RequestClass::Normal).is_none());
    }

    /// Access control belongs to the alias: a pool added for one request class
    /// is not a way around the alias's keys or deny rules.
    #[test]
    fn a_named_pool_inherits_the_default_pools_access_control() {
        let json = r#"{
            "targets": {
                "dsv4-flash": {
                    "pools": {
                        "default": {
                            "keys": ["sk-alias"],
                            "routing_rules": [
                                {"match_labels": {"purpose": "batch"}, "action": {"type": "deny"}}
                            ],
                            "providers": [{"url": "https://dynamo.example.com/"}]
                        },
                        "completions": {
                            "providers": [{"url": "https://fireworks.example.com/"}]
                        }
                    }
                }
            }
        }"#;
        let targets = Targets::from_config(serde_json::from_str(json).unwrap()).unwrap();
        let entry = targets.targets.get("dsv4-flash").unwrap();
        let completions = entry.resolve(RequestClass::Completions);
        assert_eq!(
            completions.keys().unwrap().len(),
            1,
            "completions pool must be behind the same keys"
        );
        assert_eq!(completions.routing_rules().len(), 1);
    }

    /// Deny is access control on the alias: a model closed to a label stays
    /// closed to it through every pool, so a `completions` pool cannot be used
    /// to reach a batch-denied model.
    #[test]
    fn a_named_pool_inherits_deny_rules() {
        let json = r#"{
            "targets": {
                "dsv4-flash": {
                    "pools": {
                        "default": {
                            "routing_rules": [
                                {"match_labels": {"purpose": "batch"}, "action": {"type": "deny"}}
                            ],
                            "providers": [{"url": "https://dynamo.example.com/"}]
                        },
                        "completions": {
                            "providers": [{"url": "https://fireworks.example.com/"}]
                        }
                    }
                }
            }
        }"#;
        let targets = Targets::from_config(serde_json::from_str(json).unwrap()).unwrap();
        let entry = targets.targets.get("dsv4-flash").unwrap();
        let batch = HashMap::from([("purpose".to_string(), "batch".to_string())]);

        for class in [RequestClass::Normal, RequestClass::Completions] {
            assert!(
                matches!(
                    entry.resolve(class).evaluate_routing_rules(&batch),
                    Some(RoutingAction::Deny)
                ),
                "{class:?} traffic must hit the alias's deny rule"
            );
        }
        // An unmatched label is still allowed everywhere.
        let realtime = HashMap::from([("purpose".to_string(), "realtime".to_string())]);
        assert!(
            entry
                .resolve(RequestClass::Completions)
                .evaluate_routing_rules(&realtime)
                .is_none()
        );
    }

    /// A redirect names another model ALIAS. Following one out of a named pool
    /// would serve this class from a different model — for a resume leg, a
    /// token-id prefix rendered against one model decoded by another — so
    /// redirects do not travel with inheritance.
    #[test]
    fn a_named_pool_does_not_inherit_redirect_rules() {
        let json = r#"{
            "targets": {
                "dsv4-flash": {
                    "pools": {
                        "default": {
                            "routing_rules": [
                                {"match_labels": {"purpose": "batch"}, "action": {"type": "redirect", "target": "dsv4-flash-batch"}}
                            ],
                            "providers": [{"url": "https://dynamo.example.com/"}]
                        },
                        "completions": {
                            "providers": [{"url": "https://fireworks.example.com/"}]
                        }
                    }
                },
                "dsv4-flash-batch": {"url": "https://batch.example.com/"}
            }
        }"#;
        let targets = Targets::from_config(serde_json::from_str(json).unwrap()).unwrap();
        let entry = targets.targets.get("dsv4-flash").unwrap();
        let batch = HashMap::from([("purpose".to_string(), "batch".to_string())]);

        // The class the rule was written against still redirects.
        assert!(matches!(
            entry
                .resolve(RequestClass::Normal)
                .evaluate_routing_rules(&batch),
            Some(RoutingAction::Redirect { target }) if target == "dsv4-flash-batch"
        ));
        // The completions pool routes normally within itself: no rule fires, and
        // the request is served by the pool's own members.
        let completions = entry.resolve(RequestClass::Completions);
        assert!(completions.routing_rules().is_empty());
        assert!(completions.evaluate_routing_rules(&batch).is_none());
        assert_eq!(
            completions
                .providers()
                .iter()
                .map(|p| p.target.url.to_string())
                .collect::<Vec<_>>(),
            vec!["https://fireworks.example.com/"]
        );
    }

    /// Only inheritance filters. Rules written ON a named pool are somebody
    /// deliberately routing that class, and are kept verbatim — redirects
    /// included.
    #[test]
    fn a_named_pool_keeps_its_own_routing_rules() {
        let json = r#"{
            "targets": {
                "dsv4-flash": {
                    "pools": {
                        "default": {
                            "routing_rules": [
                                {"match_labels": {"purpose": "batch"}, "action": {"type": "deny"}}
                            ],
                            "providers": [{"url": "https://dynamo.example.com/"}]
                        },
                        "completions": {
                            "routing_rules": [
                                {"match_labels": {"purpose": "batch"}, "action": {"type": "redirect", "target": "dsv4-flash-batch"}}
                            ],
                            "providers": [{"url": "https://fireworks.example.com/"}]
                        }
                    }
                },
                "dsv4-flash-batch": {"url": "https://batch.example.com/"}
            }
        }"#;
        let targets = Targets::from_config(serde_json::from_str(json).unwrap()).unwrap();
        let entry = targets.targets.get("dsv4-flash").unwrap();
        let batch = HashMap::from([("purpose".to_string(), "batch".to_string())]);

        let completions = entry.resolve(RequestClass::Completions);
        assert_eq!(completions.routing_rules().len(), 1, "no inherited deny is added");
        assert!(matches!(
            completions.evaluate_routing_rules(&batch),
            Some(RoutingAction::Redirect { target }) if target == "dsv4-flash-batch"
        ));
        // The default pool is untouched by any of this.
        assert!(matches!(
            entry
                .resolve(RequestClass::Normal)
                .evaluate_routing_rules(&batch),
            Some(RoutingAction::Deny)
        ));
    }

    /// A pool that states its own keys keeps them — inheritance fills a gap, it
    /// does not overwrite a decision.
    #[test]
    fn a_named_pool_keeps_its_own_access_control() {
        let json = r#"{
            "targets": {
                "dsv4-flash": {
                    "pools": {
                        "default": {
                            "keys": ["sk-alias"],
                            "providers": [{"url": "https://dynamo.example.com/"}]
                        },
                        "completions": {
                            "keys": ["sk-continuation", "sk-other"],
                            "providers": [{"url": "https://fireworks.example.com/"}]
                        }
                    }
                }
            }
        }"#;
        let targets = Targets::from_config(serde_json::from_str(json).unwrap()).unwrap();
        let entry = targets.targets.get("dsv4-flash").unwrap();
        assert_eq!(
            entry
                .resolve(RequestClass::Completions)
                .keys()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(entry.resolve(RequestClass::Normal).keys().unwrap().len(), 1);
    }

    /// A composite must be able to serve the classes it has no pool for.
    #[test]
    fn a_pools_target_without_a_default_is_rejected() {
        let json = r#"{
            "targets": {
                "dsv4-flash": {
                    "pools": {
                        "completions": {"providers": [{"url": "https://fireworks.example.com/"}]}
                    }
                }
            }
        }"#;
        let config: ConfigFile = serde_json::from_str(json).unwrap();
        let error = config.targets["dsv4-flash"]
            .clone()
            .into_pool_configs()
            .unwrap_err();
        assert!(error.to_string().contains("default"), "{error}");
    }

    /// A pool name the resolver never asks for would be silently dead config.
    #[test]
    fn an_unknown_pool_name_is_rejected() {
        let json = r#"{
            "targets": {
                "dsv4-flash": {
                    "pools": {
                        "default": {"providers": [{"url": "https://dynamo.example.com/"}]},
                        "completion": {"providers": [{"url": "https://fireworks.example.com/"}]}
                    }
                }
            }
        }"#;
        let config: ConfigFile = serde_json::from_str(json).unwrap();
        let error = config.targets["dsv4-flash"]
            .clone()
            .into_pool_configs()
            .unwrap_err();
        assert!(error.to_string().contains("completion"), "{error}");
    }

    /// Classification is by path suffix only — no purpose, no header, no body.
    #[test]
    fn request_class_is_derived_from_the_path_alone() {
        // The strict router forwards `/completions`; the passthrough router
        // sees the full `/v1/completions`; dwctl nests both under `/ai`.
        for path in [
            "/completions",
            "/v1/completions",
            "/ai/v1/completions",
            "/v1/completions?stream=true",
            "/v1/completions/",
        ] {
            assert_eq!(
                RequestClass::from_path(path),
                RequestClass::Completions,
                "{path}"
            );
        }
        for path in [
            "/chat/completions",
            "/v1/chat/completions",
            "/ai/v1/chat/completions",
            "/v1/chat/completions?stream=true",
            "/v1/responses",
            "/v1/embeddings",
            "/v1/models",
            "/",
            "",
        ] {
            assert_eq!(
                RequestClass::from_path(path),
                RequestClass::Normal,
                "{path}"
            );
        }
        assert_eq!(RequestClass::Completions.pool_name(), COMPLETIONS_POOL);
        assert_eq!(RequestClass::Normal.pool_name(), DEFAULT_POOL);
    }
}
