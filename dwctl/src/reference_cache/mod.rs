//! Reference cache: a shadow estimate of how many prompt tokens an ideal per-customer
//! prefix cache would have served each request, recorded as
//! `http_analytics.reference_cached_tokens`. Nothing is billed from it and the usage a
//! customer sees is unchanged.
//!
//! The ideal cache holds, per `(principal, model alias)` scope, every prompt prefix sent
//! in the last TTL window (sliding, refreshed on reuse). A request's estimate is the
//! longest of its prefixes that was already visible when it arrived. The difference to
//! `engine_cached_tokens` is what routing (spill to other providers, cold replicas) costs
//! the customer in cache hits.
//!
//! ## No tokenizer
//!
//! Block boundaries and cumulative hashes come from the prompt-cache parser
//! ([`crate::prompt_cache::parse_chat_completions`]); token counts come from the
//! engine's reported `prompt_tokens`. Each entry stores the cumulative text bytes up to
//! its boundary and the writer's tokens per byte; the writer's final boundary also stores
//! its exact `prompt_tokens`. A request whose deepest visible match is such a final
//! boundary (a linear multi-turn continuation) gets that exact count, less a per-model
//! template allowance; any other match (a fork at an earlier boundary) is estimated as
//! bytes × tokens per byte.
//!
//! ## When a prefix becomes visible
//!
//! - Streaming: at the first streamed chunk ([`layer`]), with no token count yet; the
//!   completion fills in the counts. A reader that matches such a provisional entry uses
//!   its own tokens per byte.
//! - Non-streaming: at completion.
//! - A request that fails before its first chunk writes nothing; one that fails after it
//!   keeps the provisional entries until they expire.
//!
//! A request only matches entries that became visible no later than it arrived, so two
//! concurrent requests sharing a prefix do not credit each other, and a streaming request
//! never matches its own first-token write. `visible_at` is the database clock and the
//! arrival time is the gateway's; skew between the two shifts that cut-off by the same
//! amount.
//!
//! ## Where it runs
//!
//! The first-token write is spawned off the response stream. The estimate runs in the
//! analytics handler after the response, bounded by `estimate_timeout_ms`, before the
//! record goes to the outbox; the completion write is spawned from there. Any failure
//! records NULL and increments `dwctl_reference_cache_estimates_total{outcome}`.

pub mod layer;
pub mod store;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use metrics::counter;
use serde::{Deserialize, Deserializer, Serialize};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use uuid::Uuid;

use crate::metrics::errors::component::REFERENCE_CACHE;
use crate::prefix_chain::{ModelFilter, deserialize_model_list};
use crate::prompt_cache::{ParseError, PrincipalResolver, TelemetryPolicy, TierPolicy, parse_chat_completions};
use crate::types::UserId;

pub use layer::reference_cache_middleware;
pub use store::{EntryWrite, LiveEntry, ReferenceStore};

/// Cap on spawned writes in flight at once; beyond it a write is skipped (counted
/// `backpressure`) rather than queued holding its prompt.
const MAX_IN_FLIGHT_WRITES: usize = 256;
/// Rows deleted per sweep batch.
const SWEEP_BATCH: i64 = 10_000;

/// The `reference_cache` config section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReferenceCacheConfig {
    /// `false` is the only off state.
    pub enabled: bool,
    /// Sliding window an entry stays live after its last use.
    pub ttl_secs: u64,
    /// Billing principals to estimate for: `api_keys.user_id`, which is the org id for an
    /// org key and the user id for a personal key. Accepts a YAML list or a
    /// comma-separated string (env). Must be non-empty when enabled.
    #[serde(deserialize_with = "deserialize_uuid_list")]
    pub orgs: Vec<Uuid>,
    /// `null`: every model. A non-empty list: exactly these aliases, matched exactly.
    #[serde(deserialize_with = "deserialize_model_list")]
    pub models: Option<Vec<String>>,
    /// Tokens subtracted from an exact match, per alias (default 0): the part of the
    /// writer's `prompt_tokens` its chat template adds after the last message, which a
    /// continuation does not reuse.
    pub template_allowance_tokens: HashMap<String, u32>,
    /// Bound on the estimate (principal lookup, parse and the entry lookup) in the
    /// analytics handler. Past it the request records NULL.
    pub estimate_timeout_ms: u64,
    /// How often each replica deletes expired entries.
    pub sweep_interval_secs: u64,
}

impl Default for ReferenceCacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ttl_secs: 300,
            orgs: Vec::new(),
            models: None,
            template_allowance_tokens: HashMap::new(),
            estimate_timeout_ms: 2_000,
            sweep_interval_secs: 60,
        }
    }
}

/// `orgs` accepts a sequence (YAML) or a comma-separated string (env override).
fn deserialize_uuid_list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Uuid>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Seq(Vec<Uuid>),
        Str(String),
    }
    match Option::<Raw>::deserialize(d)? {
        None => Ok(Vec::new()),
        Some(Raw::Seq(v)) => Ok(v),
        Some(Raw::Str(s)) => s
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| Uuid::parse_str(s).map_err(serde::de::Error::custom))
            .collect(),
    }
}

impl ReferenceCacheConfig {
    /// `enabled: false` ignores everything else.
    pub fn validate(&self, analytics_enabled: bool) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if !analytics_enabled {
            return Err("reference_cache.enabled is true but enable_analytics is false; the estimate is an analytics column".to_string());
        }
        if self.orgs.is_empty() {
            return Err(
                "reference_cache.orgs is empty; list the principals to estimate for, or set reference_cache.enabled: false".to_string(),
            );
        }
        if self.ttl_secs == 0 {
            return Err("reference_cache.ttl_secs must be greater than 0".to_string());
        }
        if let Some(models) = &self.models
            && models.is_empty()
        {
            return Err("reference_cache.models is an empty list; omit it to estimate every model".to_string());
        }
        Ok(())
    }

    pub fn ttl(&self) -> Duration {
        Duration::from_secs(self.ttl_secs)
    }
}

/// A request's block boundaries: the cumulative content hash and cumulative text bytes
/// after each block.
#[derive(Debug, Clone, PartialEq)]
pub struct Boundaries {
    pub hashes: Vec<Vec<u8>>,
    pub cumulative_bytes: Vec<i64>,
}

impl Boundaries {
    pub fn total_bytes(&self) -> i64 {
        self.cumulative_bytes.last().copied().unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// Tokens per byte for this prompt, given the engine's count for it.
    pub fn tokens_per_byte(&self, prompt_tokens: u64) -> Option<f32> {
        let bytes = self.total_bytes();
        (bytes > 0).then(|| prompt_tokens as f32 / bytes as f32)
    }
}

/// Split a chat-completions body into boundaries. Bytes are the block texts as the parser
/// extracts them (message text, tool and tool-call JSON); image blocks count zero.
pub fn boundaries(body: &[u8], tier_policy: &TierPolicy, telemetry: &TelemetryPolicy) -> Result<Boundaries, ParseError> {
    let parsed = parse_chat_completions(body, tier_policy, telemetry)?;
    let mut total = 0i64;
    let cumulative_bytes = parsed
        .blocks
        .iter()
        .map(|b| {
            total = total.saturating_add(b.text.len() as i64);
            total
        })
        .collect();
    Ok(Boundaries {
        hashes: parsed.cumulative_hashes,
        cumulative_bytes,
    })
}

/// The reference estimate for a request: its deepest boundary with an entry that was
/// visible when it arrived, converted to tokens and capped at `prompt_tokens`. 0 when
/// nothing matches.
pub fn estimate(b: &Boundaries, live: &[LiveEntry], arrival: DateTime<Utc>, prompt_tokens: u64, template_allowance: u32) -> u64 {
    let by_hash: HashMap<&[u8], &LiveEntry> = live.iter().map(|e| (e.prefix_hash.as_slice(), e)).collect();
    let matched = b
        .hashes
        .iter()
        .rev()
        .filter_map(|h| by_hash.get(h.as_slice()))
        .find(|e| e.visible_at <= arrival);
    let Some(entry) = matched else {
        return 0;
    };
    let tokens = match entry.exact_tokens {
        Some(exact) => u64::try_from(exact).unwrap_or(0).saturating_sub(u64::from(template_allowance)),
        None => {
            // A writer still streaming has no ratio yet; this request's own is the best
            // available stand-in for the same content.
            let tpb = entry.tokens_per_byte.or_else(|| b.tokens_per_byte(prompt_tokens)).unwrap_or(0.0);
            (entry.cumulative_bytes.max(0) as f64 * f64::from(tpb)).round() as u64
        }
    };
    tokens.min(prompt_tokens)
}

/// Which boundaries the completion write must send, given what the lookup found live:
/// those not live, those live without a ratio, and the final one if it lacks an exact
/// count. Usually just the few blocks this turn added.
fn needs_write(b: &Boundaries, live: &[LiveEntry]) -> Vec<usize> {
    let by_hash: HashMap<&[u8], &LiveEntry> = live.iter().map(|e| (e.prefix_hash.as_slice(), e)).collect();
    let last = b.hashes.len().saturating_sub(1);
    (0..b.hashes.len())
        .filter(|&i| match by_hash.get(b.hashes[i].as_slice()) {
            None => true,
            Some(e) => e.tokens_per_byte.is_none() || (i == last && e.exact_tokens.is_none()),
        })
        .collect()
}

fn estimate_outcome(outcome: &'static str) {
    counter!("dwctl_reference_cache_estimates_total", "outcome" => outcome).increment(1);
}

fn write_outcome(phase: &'static str, outcome: &'static str) {
    counter!("dwctl_reference_cache_writes_total", "phase" => phase, "outcome" => outcome).increment(1);
}

/// What the analytics handler knows about a finished request.
pub struct Completion<'a> {
    pub path: &'a str,
    /// The effective status was 2xx.
    pub success: bool,
    pub bearer_token: Option<&'a str>,
    pub model: Option<&'a str>,
    pub body: Option<&'a Bytes>,
    pub prompt_tokens: i64,
    /// When the request arrived at the gateway.
    pub arrival: DateTime<Utc>,
}

/// Why a covered request records NULL.
#[derive(Debug)]
enum Unavailable {
    NotServed,
    NoUsage,
    NoBody,
    Parse(&'static str),
    Backend(sqlx::Error),
    Principal,
}

impl Unavailable {
    fn outcome(&self) -> &'static str {
        match self {
            Self::NotServed => "not_served",
            Self::NoUsage => "no_usage",
            Self::NoBody => "no_body",
            Self::Parse(_) => "parse_error",
            Self::Backend(_) => "backend_error",
            Self::Principal => "principal_error",
        }
    }
}

pub struct ReferenceCache {
    store: ReferenceStore,
    principal: PrincipalResolver,
    orgs: HashSet<Uuid>,
    models: ModelFilter,
    template_allowance: HashMap<String, u32>,
    estimate_timeout: Duration,
    tier_policy: TierPolicy,
    telemetry: TelemetryPolicy,
    in_flight: Arc<Semaphore>,
}

impl std::fmt::Debug for ReferenceCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReferenceCache")
            .field("orgs", &self.orgs.len())
            .field("models", &self.models)
            .finish_non_exhaustive()
    }
}

impl ReferenceCache {
    pub fn new(config: &ReferenceCacheConfig, pool: impl sqlx_pool_router::PoolProvider, telemetry: TelemetryPolicy) -> Self {
        Self {
            store: ReferenceStore::new(pool.clone(), config.ttl()),
            principal: PrincipalResolver::new(pool),
            orgs: config.orgs.iter().copied().collect(),
            models: ModelFilter::from_config(&config.models),
            template_allowance: config.template_allowance_tokens.clone(),
            estimate_timeout: Duration::from_millis(config.estimate_timeout_ms),
            // Every tier enabled: a marker naming a tier the cache layer has switched off
            // must not make the prompt unparseable here.
            tier_policy: TierPolicy::from_config(&["5m".into(), "1h".into(), "24h".into()], "5m"),
            telemetry,
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT_WRITES)),
        }
    }

    /// The principal behind `bearer_token`, if it is allowlisted. `Err` on a lookup failure.
    async fn covered_principal(&self, bearer_token: &str) -> Result<Option<UserId>, Unavailable> {
        match self.principal.resolve(bearer_token).await {
            Ok(Some(p)) if self.orgs.contains(&p) => Ok(Some(p)),
            Ok(_) => Ok(None),
            Err(e) => {
                debug!(error = %e, "reference cache: principal lookup failed");
                Err(Unavailable::Principal)
            }
        }
    }

    fn boundaries(&self, body: &[u8]) -> Result<Boundaries, Unavailable> {
        // Only the rule that fired is kept: some ParseError variants echo client values.
        boundaries(body, &self.tier_policy, &self.telemetry).map_err(|e| Unavailable::Parse(crate::prefix_chain::parse_error_kind(&e)))
    }

    /// A streaming request produced its first chunk: make its prefixes visible, without
    /// token counts yet. Spawned; returns immediately.
    pub fn observe_first_token(self: &Arc<Self>, bearer_token: String, body: Bytes) {
        let Ok(permit) = Arc::clone(&self.in_flight).try_acquire_owned() else {
            write_outcome("first_token", "backpressure");
            return;
        };
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let _permit = permit;
            this.first_token_write(&bearer_token, &body).await;
        });
    }

    async fn first_token_write(&self, bearer_token: &str, body: &[u8]) {
        let principal_id = match self.covered_principal(bearer_token).await {
            Ok(Some(p)) => p,
            Ok(None) => return,
            Err(_) => {
                write_outcome("first_token", "principal_error");
                return;
            }
        };
        #[derive(Deserialize)]
        struct ModelOnly {
            model: Option<String>,
        }
        let Some(model) = serde_json::from_slice::<ModelOnly>(body).ok().and_then(|m| m.model) else {
            write_outcome("first_token", "parse_error");
            return;
        };
        if !self.models.allows(&model) {
            return;
        }
        let b = match self.boundaries(body) {
            Ok(b) if !b.is_empty() => b,
            Ok(_) => return,
            Err(_) => {
                write_outcome("first_token", "parse_error");
                return;
            }
        };
        let exact = vec![None; b.hashes.len()];
        let write = EntryWrite {
            principal_id,
            virtual_model: &model,
            prefix_hashes: &b.hashes,
            cumulative_bytes: &b.cumulative_bytes,
            exact_tokens: &exact,
            tokens_per_byte: None,
        };
        match self.store.upsert(&write).await {
            Ok(_) => write_outcome("first_token", "ok"),
            Err(e) => {
                crate::background_error!(REFERENCE_CACHE, "first_token_write", Warning, error = %e, "reference cache: first-token write failed");
                write_outcome("first_token", "error");
            }
        }
    }

    /// The estimate for a finished request, and its own entries written (spawned).
    /// `None` when the request is not covered (not chat, not allowlisted, model filtered)
    /// or the estimate failed; a failure also increments the outcome counter.
    pub async fn complete(self: &Arc<Self>, c: Completion<'_>) -> Option<i64> {
        if !c.path.ends_with("/chat/completions") {
            return None;
        }
        let bearer_token = c.bearer_token?;
        let model = c.model.filter(|m| self.models.allows(m))?.to_string();
        let body = c.body.cloned();
        let arrival = c.arrival;
        let (success, prompt_tokens) = (c.success, c.prompt_tokens);

        let result = tokio::time::timeout(self.estimate_timeout, async {
            let Some(principal_id) = self.covered_principal(bearer_token).await? else {
                return Ok(None);
            };
            if !success {
                return Err(Unavailable::NotServed);
            }
            let prompt_tokens = u64::try_from(prompt_tokens).ok().filter(|&t| t > 0).ok_or(Unavailable::NoUsage)?;
            let body = body.ok_or(Unavailable::NoBody)?;
            let b = self.boundaries(&body)?;
            let live = self
                .store
                .lookup(principal_id, &model, &b.hashes)
                .await
                .map_err(Unavailable::Backend)?;
            let allowance = self.template_allowance.get(&model).copied().unwrap_or(0);
            let tokens = estimate(&b, &live, arrival, prompt_tokens, allowance);
            self.spawn_completion_write(principal_id, model.clone(), b, &live, prompt_tokens);
            Ok(Some(tokens))
        })
        .await;

        match result {
            Ok(Ok(Some(tokens))) => {
                estimate_outcome(if tokens > 0 { "hit" } else { "miss" });
                Some(i64::try_from(tokens).unwrap_or(i64::MAX))
            }
            Ok(Ok(None)) => None,
            Ok(Err(e)) => {
                if let Unavailable::Backend(err) = &e {
                    crate::background_error!(REFERENCE_CACHE, "lookup", Warning, error = %err, "reference cache: lookup failed; estimate recorded as NULL");
                }
                if let Unavailable::Parse(kind) = &e {
                    debug!(kind, "reference cache: prompt not parseable; estimate recorded as NULL");
                }
                estimate_outcome(e.outcome());
                None
            }
            Err(_) => {
                estimate_outcome("timeout");
                None
            }
        }
    }

    fn spawn_completion_write(
        self: &Arc<Self>,
        principal_id: UserId,
        model: String,
        b: Boundaries,
        live: &[LiveEntry],
        prompt_tokens: u64,
    ) {
        let idx = needs_write(&b, live);
        if idx.is_empty() {
            return;
        }
        let Ok(permit) = Arc::clone(&self.in_flight).try_acquire_owned() else {
            write_outcome("completion", "backpressure");
            return;
        };
        let last = b.hashes.len() - 1;
        let tokens_per_byte = b.tokens_per_byte(prompt_tokens);
        let exact_final = i32::try_from(prompt_tokens).ok();
        let hashes: Vec<Vec<u8>> = idx.iter().map(|&i| b.hashes[i].clone()).collect();
        let bytes: Vec<i64> = idx.iter().map(|&i| b.cumulative_bytes[i]).collect();
        let exact: Vec<Option<i32>> = idx.iter().map(|&i| if i == last { exact_final } else { None }).collect();
        let store = self.store.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let write = EntryWrite {
                principal_id,
                virtual_model: &model,
                prefix_hashes: &hashes,
                cumulative_bytes: &bytes,
                exact_tokens: &exact,
                tokens_per_byte,
            };
            match store.upsert(&write).await {
                Ok(_) => write_outcome("completion", "ok"),
                Err(e) => {
                    crate::background_error!(REFERENCE_CACHE, "completion_write", Warning, error = %e, "reference cache: completion write failed");
                    write_outcome("completion", "error");
                }
            }
        });
    }
}

/// Delete expired entries every `interval` until `shutdown`. Every replica runs it;
/// `SKIP LOCKED` lets concurrent sweeps share the work.
pub async fn run_sweeper(store: ReferenceStore, interval: Duration, shutdown: CancellationToken) {
    info!(interval_s = interval.as_secs(), "Reference cache sweeper started");
    let mut ticker = tokio::time::interval(interval.max(Duration::from_secs(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => {}
        }
        loop {
            match store.sweep_expired(SWEEP_BATCH).await {
                Ok(n) if n < SWEEP_BATCH as u64 => break,
                Ok(_) if shutdown.is_cancelled() => break,
                Ok(_) => continue,
                Err(e) => {
                    crate::background_error!(REFERENCE_CACHE, "sweep", Warning, error = %e, "reference cache: sweep failed");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
