//! Reference cache: how many prompt tokens an ideal per-customer prefix cache would have
//! served each request, recorded as `http_analytics.reference_cached_tokens`, and the
//! input to assured cache billing ([`assurance`]) for organisations with terms.
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
//! - Streaming: at the first streamed chunk with output, with no token count yet; the
//!   completion fills in the counts. A reader that matches such a provisional entry uses
//!   its own tokens per byte.
//! - Non-streaming: at completion.
//! - A request that fails before its first chunk writes nothing; one that fails after it
//!   keeps the provisional entries until they expire.
//!
//! A request only matches entries that became visible no later than it arrived and were
//! still live then, so two concurrent requests sharing a prefix do not credit each other,
//! a streaming request never matches its own first-token write, and a long generation
//! does not lose a prefix that expired while it ran. `visible_at` is the database clock and the
//! arrival time is the gateway's; skew between the two shifts that cut-off by the same
//! amount.
//!
//! ## Where it runs
//!
//! Inside the prompt-cache layer ([`crate::prompt_cache::layer`]), which sits inside
//! protocol translation, so Chat Completions, Messages and Responses requests are all
//! hashed as the same translated Chat Completions body. [`ReferenceCache::begin`] starts
//! the lookup when the request arrives, in parallel with generation; the layer joins it
//! at the terminal usage frame (the whole response, when not streaming) for at most
//! `join_deadline_ms`, computes the estimate from the engine's `prompt_tokens`, and bills
//! and records that one number. The first token never waits for it. A lookup still
//! running at the deadline records NULL (and, for an assured organisation, bills the
//! engine's hit, counted in `dwctl_cache_assurance_fallbacks_total`); it carries on in the
//! background so the request's own entries are still written. The store holds at most
//! `max_connections` pool connections, so a slow or stuck reference table cannot take the
//! connections the rest of the gateway needs.

pub mod assurance;
pub mod layer;
pub mod store;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use metrics::counter;
use serde::{Deserialize, Deserializer, Serialize};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use uuid::Uuid;

use crate::crypto::ApiKeyHash;
use crate::metrics::errors::component::REFERENCE_CACHE;
use crate::prefix_chain::{ModelFilter, deserialize_model_list};
use crate::prompt_cache::{
    BilledCache, CacheReadSource, ParseError, PrincipalResolver, TelemetryPolicy, TierPolicy, parse_chat_completions,
};
use crate::types::UserId;

pub use assurance::{AssuranceBook, CacheAssurance};
pub use layer::FirstTokenDetector;
pub use store::{EntryWrite, LiveEntry, ReferenceStore};

/// Cap on spawned writes in flight at once; beyond it a write is skipped (counted
/// `backpressure`) rather than queued holding its prompt.
const MAX_IN_FLIGHT_WRITES: usize = 256;
/// Rows deleted per sweep batch.
const SWEEP_BATCH: i64 = 10_000;
/// How long the sweep keeps a row past its expiry, so a request that arrived inside the
/// window still finds it when it completes. Covers long generations; a request running
/// longer loses only prefixes that expired while it ran. Rows linger this long, so the
/// table holds about (ttl + grace) / ttl times the live set.
const SWEEP_GRACE: Duration = Duration::from_secs(10 * 60);

/// The `reference_cache` config section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReferenceCacheConfig {
    /// `false` is the only off state.
    pub enabled: bool,
    /// Sliding window an entry stays live after its last use.
    pub ttl_secs: u64,
    /// Billing principals to estimate for without billing from the estimate:
    /// `api_keys.user_id`, which is the org id for an org key and the user id for a
    /// personal key. Accepts a YAML list or a comma-separated string (env). Organisations
    /// with `cache_assurance` terms in the org overlay catalog are covered on those models
    /// whether listed here or not.
    #[serde(deserialize_with = "deserialize_uuid_list")]
    pub orgs: Vec<Uuid>,
    /// Which models `orgs` are estimated on. `null`: every model. A non-empty list:
    /// exactly these aliases, matched exactly. Assurance terms name their own models.
    #[serde(deserialize_with = "deserialize_model_list")]
    pub models: Option<Vec<String>>,
    /// Tokens subtracted from an exact match, per alias (default 0): the part of the
    /// writer's `prompt_tokens` its chat template adds after the last message, which a
    /// continuation does not reuse.
    pub template_allowance_tokens: HashMap<String, u32>,
    /// Bound on the background work for one request: the lookup started at arrival
    /// (principal, parse, entry lookup) and each entry write.
    pub estimate_timeout_ms: u64,
    /// How long the terminal usage frame (the whole response, when not streaming) may
    /// wait for a lookup that has not finished yet. Past it the request records NULL and
    /// an assured organisation is billed the engine's hit. The lookup started when the
    /// request arrived, so it has normally finished long before.
    pub join_deadline_ms: u64,
    /// How often each replica deletes expired entries.
    pub sweep_interval_secs: u64,
    /// Connections of the application pool the reference cache may hold at once
    /// (lookups and writes together; the sweeper takes one more). Work beyond it waits
    /// within the timeouts above or is skipped, so it cannot starve the pool.
    pub max_connections: u32,
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
            join_deadline_ms: 50,
            sweep_interval_secs: 60,
            max_connections: 4,
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
    /// `enabled: false` ignores everything else. An empty `orgs` is valid: assurance
    /// terms in the org overlay catalog cover their organisations by themselves.
    pub fn validate(&self, analytics_enabled: bool, cache_enabled: bool) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if !analytics_enabled {
            return Err("reference_cache.enabled is true but enable_analytics is false; the estimate is an analytics column".to_string());
        }
        if !cache_enabled {
            return Err("reference_cache.enabled is true but cache.enabled is false; the estimate runs in the cache layer".to_string());
        }
        if self.join_deadline_ms == 0 {
            return Err("reference_cache.join_deadline_ms must be greater than 0; 0 would record NULL for every request".to_string());
        }
        if self.ttl_secs == 0 {
            return Err("reference_cache.ttl_secs must be greater than 0".to_string());
        }
        if self.estimate_timeout_ms == 0 {
            return Err("reference_cache.estimate_timeout_ms must be greater than 0; 0 would record NULL for every request".to_string());
        }
        if self.sweep_interval_secs == 0 {
            return Err("reference_cache.sweep_interval_secs must be greater than 0".to_string());
        }
        if self.max_connections == 0 {
            return Err("reference_cache.max_connections must be greater than 0".to_string());
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

    pub fn join_deadline(&self) -> Duration {
        Duration::from_millis(self.join_deadline_ms)
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

/// Who a covered request is estimated for, known once its key resolves.
#[derive(Debug, Clone, PartialEq)]
pub struct Coverage {
    pub principal: UserId,
    /// The principal's assurance terms on the request's model, if it has any.
    pub terms: Option<CacheAssurance>,
}

/// Why a covered request has no estimate.
#[derive(Debug)]
enum Unavailable {
    Parse(&'static str),
    Backend(sqlx::Error),
    Principal,
    Timeout,
}

impl Unavailable {
    fn outcome(&self) -> &'static str {
        match self {
            Self::Parse(_) => "parse_error",
            Self::Backend(_) => "backend_error",
            Self::Principal => "principal_error",
            Self::Timeout => "timeout",
        }
    }
}

/// A finished lookup: every entry of the request's boundaries that was live when it
/// arrived. Turned into tokens once the engine's `prompt_tokens` is known.
#[derive(Debug, Clone)]
pub struct Lookup {
    pub coverage: Coverage,
    /// The entries' scope: the model name as requested.
    requested: String,
    b: Boundaries,
    live: Vec<LiveEntry>,
    arrival: DateTime<Utc>,
    allowance: u32,
}

impl Lookup {
    pub fn estimate(&self, prompt_tokens: u64) -> u64 {
        estimate(&self.b, &self.live, self.arrival, prompt_tokens, self.allowance)
    }
}

/// What joining a request's lookup found.
#[derive(Debug, Clone)]
pub enum Joined {
    /// Not a principal and model the reference cache covers.
    Uncovered,
    Ready(Lookup),
    /// No estimate. `coverage` is `None` when the key had not resolved yet either.
    Unavailable {
        coverage: Option<Coverage>,
        outcome: &'static str,
    },
}

impl Joined {
    pub fn coverage(&self) -> Option<&Coverage> {
        match self {
            Self::Uncovered => None,
            Self::Ready(l) => Some(&l.coverage),
            Self::Unavailable { coverage, .. } => coverage.as_ref(),
        }
    }

    /// The assurance terms that apply to the request, when known. A lookup that missed
    /// the deadline before its key resolved has none, and bills from the engine.
    pub fn terms(&self) -> Option<CacheAssurance> {
        self.coverage().and_then(|c| c.terms)
    }

    /// The estimate for a request the engine counted `prompt_tokens` for. `None` without
    /// a finished lookup.
    pub fn estimate(&self, prompt_tokens: u64) -> Option<u64> {
        match self {
            Self::Ready(l) => Some(l.estimate(prompt_tokens)),
            _ => None,
        }
    }

    fn outcome(&self) -> Option<&'static str> {
        match self {
            Self::Unavailable { outcome, .. } => Some(outcome),
            _ => None,
        }
    }
}

#[cfg(test)]
impl Joined {
    /// A finished lookup for `principal` whose estimate is `tokens`, capped at the prompt.
    pub(crate) fn ready_with(principal: UserId, terms: Option<CacheAssurance>, tokens: i32) -> Self {
        let past = Utc::now() - chrono::Duration::seconds(1);
        Self::Ready(Lookup {
            coverage: Coverage { principal, terms },
            requested: "model".to_string(),
            b: Boundaries {
                hashes: vec![b"h".to_vec()],
                cumulative_bytes: vec![100],
            },
            live: vec![LiveEntry {
                prefix_hash: b"h".to_vec(),
                cumulative_bytes: 100,
                tokens_per_byte: Some(1.0),
                exact_tokens: Some(tokens),
                visible_at: past,
            }],
            arrival: Utc::now(),
            allowance: 0,
        })
    }
}

type LookupTask = JoinHandle<Result<Option<Lookup>, Unavailable>>;

/// One request's reference-cache work, from arrival to completion: the lookup running in
/// the background, the first-token write, the join and the completion write. Dropped
/// before [`Self::finish`] (a client disconnect), it aborts the lookup and writes nothing
/// more.
pub struct PendingReference {
    cache: Arc<ReferenceCache>,
    api_key: ApiKeyHash,
    /// The name as requested: the entries' scope and the metrics label.
    requested: String,
    /// The model alias, which assurance terms name.
    model: String,
    body: Bytes,
    /// Filled by the lookup as soon as the key resolves.
    coverage: Arc<OnceLock<Option<Coverage>>>,
    task: Option<LookupTask>,
    joined: Option<Joined>,
    first_token_seen: bool,
}

impl Drop for PendingReference {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl PendingReference {
    /// The streamed response produced its first output: make the request's prefixes
    /// visible now, without token counts. Spawned; only the first call writes.
    pub fn first_token(&mut self) {
        if std::mem::replace(&mut self.first_token_seen, true) {
            return;
        }
        self.cache
            .observe_first_token(self.api_key, self.requested.clone(), self.model.clone(), self.body.clone());
    }

    /// Wait at most `deadline` for the lookup. The first call decides; later calls return
    /// the same result. A lookup still running at the deadline keeps running, so
    /// [`Self::finish`] can still write the request's entries.
    pub async fn join(&mut self, deadline: Duration) -> &Joined {
        if self.joined.is_none() {
            let joined = self.join_once(deadline).await;
            if let Some(outcome) = joined.outcome() {
                estimate_outcome(outcome);
            }
            self.joined = Some(joined);
        }
        self.joined.as_ref().expect("set above")
    }

    async fn join_once(&mut self, deadline: Duration) -> Joined {
        let Some(task) = self.task.as_mut() else {
            return Joined::Uncovered;
        };
        let result = match tokio::time::timeout(deadline, &mut *task).await {
            Ok(result) => result,
            Err(_) => {
                return match self.coverage.get() {
                    Some(None) => Joined::Uncovered,
                    known => Joined::Unavailable {
                        coverage: known.cloned().flatten(),
                        outcome: "deadline_exceeded",
                    },
                };
            }
        };
        self.task = None;
        match result {
            Ok(Ok(Some(lookup))) => Joined::Ready(lookup),
            Ok(Ok(None)) => Joined::Uncovered,
            Ok(Err(e)) => {
                match &e {
                    Unavailable::Backend(err) => {
                        crate::background_error!(REFERENCE_CACHE, "lookup", Warning, error = %err, "reference cache: lookup failed; estimate recorded as NULL");
                    }
                    Unavailable::Parse(kind) => debug!(kind, "reference cache: prompt not parseable; estimate recorded as NULL"),
                    Unavailable::Principal | Unavailable::Timeout => {}
                }
                Joined::Unavailable {
                    coverage: self.coverage.get().cloned().flatten(),
                    outcome: e.outcome(),
                }
            }
            Err(e) => Joined::Unavailable {
                coverage: self.coverage.get().cloned().flatten(),
                outcome: if e.is_panic() { "panicked" } else { "cancelled" },
            },
        }
    }

    /// The joined result, once [`Self::join`] ran.
    pub fn joined(&self) -> Option<&Joined> {
        self.joined.as_ref()
    }

    /// Record what a covered request was billed: the engine's, the reference's and the
    /// billed cached tokens per principal and model, and, for an assured principal whose
    /// unmarked request was billed from the engine instead, why. `injected` is false when
    /// the cache layer had nothing to bill (no cache tariff, classify failed).
    pub fn record(&self, billed: &BilledCache, engine: Option<u64>, prompt_tokens: Option<u64>, unmarked: bool, injected: bool) {
        let Some(joined) = &self.joined else {
            return;
        };
        let Some(coverage) = joined.coverage() else {
            return;
        };
        let Some(prompt_tokens) = prompt_tokens else {
            if matches!(joined, Joined::Ready(_)) {
                estimate_outcome("no_usage");
            }
            return;
        };
        if let Some(reference) = billed.reference {
            estimate_outcome(if reference > 0 { "hit" } else { "miss" });
        }
        let org = coverage.principal.to_string();
        let tokens = |kind: &'static str, value: u64| {
            counter!("dwctl_cache_assurance_tokens_total", "org" => org.clone(), "model" => self.requested.clone(), "kind" => kind)
                .increment(value);
        };
        tokens("prompt", prompt_tokens);
        tokens("engine", engine.unwrap_or(0).min(prompt_tokens));
        if let Some(reference) = billed.reference {
            tokens("reference", reference);
        }
        tokens("billed", billed.stats.read);
        if coverage.terms.is_some() && unmarked {
            let fallback = if !injected {
                Some("inactive")
            } else if billed.reference.is_none() {
                Some(joined.outcome().unwrap_or("unavailable"))
            } else {
                None
            };
            if let Some(reason) = fallback {
                counter!("dwctl_cache_assurance_fallbacks_total", "org" => org.clone(), "model" => self.requested.clone(), "reason" => reason)
                    .increment(1);
            }
            if billed.read_source == Some(CacheReadSource::Reference) {
                counter!("dwctl_cache_assurance_requests_total", "org" => org, "model" => self.requested.clone(), "outcome" => "raised")
                    .increment(1);
            } else {
                counter!("dwctl_cache_assurance_requests_total", "org" => org, "model" => self.requested.clone(), "outcome" => if fallback.is_some() { "fallback" } else { "engine" }).increment(1);
            }
        }
    }

    /// The request ended. A successful one that the engine counted `prompt_tokens` for
    /// writes its own entries in the background; anything else writes nothing more.
    pub fn finish(mut self, prompt_tokens: Option<u64>, success: bool) {
        let task = self.task.take();
        let joined = self.joined.take();
        let Some(prompt_tokens) = prompt_tokens.filter(|&t| t > 0 && success) else {
            if let Some(task) = task {
                task.abort();
            }
            if !success && joined.as_ref().is_some_and(|j| j.coverage().is_some()) {
                estimate_outcome("not_served");
            }
            return;
        };
        match (joined, task) {
            (Some(Joined::Ready(lookup)), _) => self.cache.spawn_completion_write(lookup, prompt_tokens),
            // The join gave up before the lookup finished: write once it does.
            (_, Some(task)) => {
                let cache = Arc::clone(&self.cache);
                tokio::spawn(async move {
                    if let Ok(Ok(Some(lookup))) = task.await {
                        cache.spawn_completion_write(lookup, prompt_tokens);
                    }
                });
            }
            _ => {}
        }
    }
}

pub struct ReferenceCache {
    store: ReferenceStore,
    principal: PrincipalResolver,
    orgs: HashSet<Uuid>,
    models: ModelFilter,
    assurance: Arc<AssuranceBook>,
    template_allowance: HashMap<String, u32>,
    estimate_timeout: Duration,
    join_deadline: Duration,
    tier_policy: TierPolicy,
    telemetry: TelemetryPolicy,
    in_flight: Arc<Semaphore>,
}

impl std::fmt::Debug for ReferenceCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReferenceCache")
            .field("orgs", &self.orgs.len())
            .field("models", &self.models)
            .field("assurance", &self.assurance)
            .finish_non_exhaustive()
    }
}

impl ReferenceCache {
    pub fn new(
        config: &ReferenceCacheConfig,
        pool: impl sqlx_pool_router::PoolProvider,
        telemetry: TelemetryPolicy,
        assurance: Arc<AssuranceBook>,
    ) -> Self {
        Self {
            store: ReferenceStore::new(pool.clone(), config.ttl(), config.max_connections as usize),
            principal: PrincipalResolver::new(pool),
            orgs: config.orgs.iter().copied().collect(),
            models: ModelFilter::from_config(&config.models),
            assurance,
            template_allowance: config.template_allowance_tokens.clone(),
            estimate_timeout: Duration::from_millis(config.estimate_timeout_ms),
            join_deadline: config.join_deadline(),
            // Every tier enabled: a marker naming a tier the cache layer has switched off
            // must not make the prompt unparseable here.
            tier_policy: TierPolicy::from_config(&["5m".into(), "1h".into(), "24h".into()], "5m"),
            telemetry,
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT_WRITES)),
        }
    }

    /// How long the usage frame may wait for the lookup (`join_deadline_ms`).
    pub fn join_deadline(&self) -> Duration {
        self.join_deadline
    }

    pub fn assurance(&self) -> &Arc<AssuranceBook> {
        &self.assurance
    }

    /// Start a chat request's lookup at `arrival`, in the background. `body` is the
    /// Chat Completions body the cache layer forwards (before marker stripping),
    /// `requested` the model name as sent and `model` its alias.
    pub fn begin(
        self: &Arc<Self>,
        api_key: ApiKeyHash,
        requested: &str,
        model: &str,
        body: Bytes,
        arrival: DateTime<Utc>,
    ) -> PendingReference {
        self.assurance.refresh_if_stale();
        let coverage = Arc::new(OnceLock::new());
        let task = {
            let this = Arc::clone(self);
            let coverage = Arc::clone(&coverage);
            let (requested, model, body) = (requested.to_string(), model.to_string(), body.clone());
            tokio::spawn(async move {
                tokio::time::timeout(
                    this.estimate_timeout,
                    this.lookup(&api_key, &requested, &model, &body, arrival, &coverage),
                )
                .await
                .unwrap_or(Err(Unavailable::Timeout))
            })
        };
        PendingReference {
            cache: Arc::clone(self),
            api_key,
            requested: requested.to_string(),
            model: model.to_string(),
            body,
            coverage,
            task: Some(task),
            joined: None,
            first_token_seen: false,
        }
    }

    /// Whether the key's principal is covered on this model, and on what terms. `Err` on
    /// a lookup failure.
    async fn coverage(&self, api_key: &ApiKeyHash, requested: &str, model: &str) -> Result<Option<Coverage>, Unavailable> {
        let principal = match self.principal.resolve(api_key).await {
            Ok(Some(p)) => p,
            Ok(None) => return Ok(None),
            Err(e) => {
                debug!(error = %e, "reference cache: principal lookup failed");
                return Err(Unavailable::Principal);
            }
        };
        let terms = self.assurance.terms(principal, model);
        let shadow = self.orgs.contains(&principal) && self.models.allows(requested);
        Ok((terms.is_some() || shadow).then_some(Coverage { principal, terms }))
    }

    async fn lookup(
        &self,
        api_key: &ApiKeyHash,
        requested: &str,
        model: &str,
        body: &[u8],
        arrival: DateTime<Utc>,
        coverage_cell: &OnceLock<Option<Coverage>>,
    ) -> Result<Option<Lookup>, Unavailable> {
        let coverage = self.coverage(api_key, requested, model).await?;
        let _ = coverage_cell.set(coverage.clone());
        let Some(coverage) = coverage else {
            return Ok(None);
        };
        let b = self.boundaries(body)?;
        let live = self
            .store
            .lookup(coverage.principal, requested, &b.hashes, arrival)
            .await
            .map_err(Unavailable::Backend)?;
        let allowance = self.template_allowance.get(requested).copied().unwrap_or(0);
        Ok(Some(Lookup {
            coverage,
            requested: requested.to_string(),
            b,
            live,
            arrival,
            allowance,
        }))
    }

    fn boundaries(&self, body: &[u8]) -> Result<Boundaries, Unavailable> {
        // Only the rule that fired is kept: some ParseError variants echo client values.
        boundaries(body, &self.tier_policy, &self.telemetry).map_err(|e| Unavailable::Parse(crate::prefix_chain::parse_error_kind(&e)))
    }

    /// A streaming request produced its first chunk: make its prefixes visible, without
    /// token counts yet. Spawned; returns immediately.
    pub fn observe_first_token(self: &Arc<Self>, api_key: ApiKeyHash, requested: String, model: String, body: Bytes) {
        let Ok(permit) = Arc::clone(&self.in_flight).try_acquire_owned() else {
            write_outcome("first_token", "backpressure");
            return;
        };
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let _permit = permit;
            this.first_token_write(&api_key, &requested, &model, &body).await;
        });
    }

    async fn first_token_write(&self, api_key: &ApiKeyHash, requested: &str, model: &str, body: &[u8]) {
        let principal_id = match self.coverage(api_key, requested, model).await {
            Ok(Some(c)) => c.principal,
            Ok(None) => return,
            Err(_) => {
                write_outcome("first_token", "principal_error");
                return;
            }
        };
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
            virtual_model: requested,
            prefix_hashes: &b.hashes,
            cumulative_bytes: &b.cumulative_bytes,
            exact_tokens: &exact,
            tokens_per_byte: None,
        };
        match tokio::time::timeout(self.estimate_timeout, self.store.upsert(&write)).await {
            Ok(Ok(_)) => write_outcome("first_token", "ok"),
            Ok(Err(e)) => {
                crate::background_error!(REFERENCE_CACHE, "first_token_write", Warning, error = %e, "reference cache: first-token write failed");
                write_outcome("first_token", "error");
            }
            Err(_) => write_outcome("first_token", "timeout"),
        }
    }

    fn spawn_completion_write(self: &Arc<Self>, lookup: Lookup, prompt_tokens: u64) {
        let Lookup {
            coverage,
            requested,
            b,
            live,
            ..
        } = lookup;
        let idx = needs_write(&b, &live);
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
        let timeout = self.estimate_timeout;
        let principal_id = coverage.principal;
        tokio::spawn(async move {
            let _permit = permit;
            let write = EntryWrite {
                principal_id,
                virtual_model: &requested,
                prefix_hashes: &hashes,
                cumulative_bytes: &bytes,
                exact_tokens: &exact,
                tokens_per_byte,
            };
            match tokio::time::timeout(timeout, store.upsert(&write)).await {
                Ok(Ok(_)) => write_outcome("completion", "ok"),
                Ok(Err(e)) => {
                    crate::background_error!(REFERENCE_CACHE, "completion_write", Warning, error = %e, "reference cache: completion write failed");
                    write_outcome("completion", "error");
                }
                Err(_) => write_outcome("completion", "timeout"),
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
            match store.sweep_expired(SWEEP_BATCH, SWEEP_GRACE).await {
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
mod billing_tests;
#[cfg(test)]
mod tests;
