//! Prefix-chain capture: content-free records of prompt STRUCTURE, per chat-completions
//! request, written to ClickHouse for workload profiling.
//!
//! Zero-data-retention customers give us no bodies, and the analytics row alone cannot
//! say which requests share a prefix (a system prompt, a tool set) or how a conversation
//! grows turn by turn. This module records, per request, a chain of keyed hashes: one per
//! content block of the prompt in parse order (tool definitions, system, each message,
//! each tool call), each an HMAC-SHA256 over the cumulative content up to that block,
//! truncated to 16 bytes. Equal prefixes give equal chain prefixes, so sessions and
//! shared templates fall out of the data with no content stored; without the key a
//! leaked table is structure only. Alongside the chain: each block's role and its
//! token count on its own (tokenizer-svc), so template overhead and per-block sizes are
//! recoverable, which is what a replay corpus needs.
//!
//! The block split and the cumulative hashes are the prompt-cache layer's own parser
//! ([`crate::prompt_cache::parse_chat_completions`]) under the same policies the billing
//! classifier uses, so a chain entry is exactly `HMAC(prefix_hash)` of what that layer
//! would index. Nothing here touches the classify path or billing: capture runs in the
//! outlet analytics handler, after the response, off the request path, and any failure
//! yields no record and a counter.
//!
//! Best effort end to end: the ClickHouse sink drops on a full queue or a twice-failed
//! insert (see [`crate::clickhouse`]). Design and decisions: notes campaign
//! `zdr-workload-profiling`, 2026-09-02.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use metrics::counter;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::Sha256;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::clickhouse::{ClickhouseConfig, SinkHandle, SinkOptions, is_identifier};
use crate::metrics::errors::component::PREFIX_CHAIN;
use crate::prompt_cache::{
    ParseError, PrincipalResolver, TelemetryPolicy, TierPolicy, TokenizerClient, TokenizerError, parse_chat_completions,
};

/// Schema version of [`PromptChainRow`]. Bump when the row shape changes.
pub const RECORD_VERSION: u8 = 1;
/// Bytes of the HMAC kept per chain entry (hex-encoded to 32 characters in the row).
const HASH_BYTES: usize = 16;

fn default_key_version() -> u8 {
    1
}
fn default_table() -> String {
    "prompt_chains".to_string()
}
fn default_flush_interval() -> Duration {
    Duration::from_secs(5)
}
fn default_max_batch_rows() -> usize {
    5_000
}
fn default_queue_capacity() -> usize {
    20_000
}
fn default_retry_delay() -> Duration {
    Duration::from_millis(500)
}
fn default_max_in_flight() -> usize {
    256
}

/// The `prefix_chain` config section.
///
/// The connection lives on the top-level `clickhouse` section; this holds what is
/// specific to this writer. The semantics are chosen so that no configuration is a
/// silent no-op — see [`PrefixChainConfig::validate`].
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PrefixChainConfig {
    /// Master switch. `false` is the only off state: no parse, no sink, no records.
    pub enabled: bool,
    /// Which models to capture. `null` / absent: every model. A non-empty list: exactly
    /// those dwctl aliases, matched exactly and case-sensitively against the alias the
    /// request resolved to (the string that lands in `http_analytics.model`). An empty
    /// list is a startup error. Accepts a YAML list or a comma-separated string (env).
    #[serde(deserialize_with = "deserialize_model_list")]
    pub models: Option<Vec<String>>,
    /// 32-byte key, hex-encoded (64 characters). `DWCTL_PREFIX_CHAIN__HMAC_KEY`.
    pub hmac_key: String,
    /// Recorded on every row so chains under different keys are never joined.
    pub key_version: u8,
    /// Table within the `clickhouse` section's database.
    pub table: String,
    #[serde(with = "humantime_serde")]
    pub flush_interval: Duration,
    pub max_batch_rows: usize,
    pub queue_capacity: usize,
    #[serde(with = "humantime_serde")]
    pub retry_delay: Duration,
    /// Cap on requests whose principal lookup and tokenization are in flight at once.
    /// Beyond it, new requests are skipped (counted `backpressure`) rather than queued,
    /// so a slow tokenizer-svc bounds memory instead of growing a backlog of prompts.
    pub max_in_flight: usize,
}

impl Default for PrefixChainConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            models: None,
            hmac_key: String::new(),
            key_version: default_key_version(),
            table: default_table(),
            flush_interval: default_flush_interval(),
            max_batch_rows: default_max_batch_rows(),
            queue_capacity: default_queue_capacity(),
            retry_delay: default_retry_delay(),
            max_in_flight: default_max_in_flight(),
        }
    }
}

impl fmt::Debug for PrefixChainConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrefixChainConfig")
            .field("enabled", &self.enabled)
            .field("models", &self.models)
            .field("hmac_key", &if self.hmac_key.is_empty() { "<unset>" } else { "<redacted>" })
            .field("key_version", &self.key_version)
            .field("table", &self.table)
            .field("flush_interval", &self.flush_interval)
            .field("max_batch_rows", &self.max_batch_rows)
            .field("queue_capacity", &self.queue_capacity)
            .field("retry_delay", &self.retry_delay)
            .field("max_in_flight", &self.max_in_flight)
            .finish()
    }
}

/// `models` accepts a sequence (YAML) or a comma-separated string (env override).
/// Absent stays `None`. An empty string becomes an empty list, which validation rejects
/// with the same message as an empty YAML list.
fn deserialize_model_list<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Seq(Vec<String>),
        Str(String),
    }
    Ok(match Option::<Raw>::deserialize(d)? {
        None => None,
        Some(Raw::Seq(v)) => Some(v),
        Some(Raw::Str(s)) => Some(s.split(',').map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).collect()),
    })
}

impl PrefixChainConfig {
    /// The rules that keep this from being a silent no-op:
    /// - `enabled: false` ignores everything else.
    /// - `enabled: true` requires the `clickhouse` section, a tokenizer-svc URL, a
    ///   well-formed key, a non-empty `models` list if one is given, and sane sink sizes.
    pub fn validate(&self, clickhouse: Option<&ClickhouseConfig>, tokenizer_url: &str) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if clickhouse.is_none() {
            return Err("prefix_chain.enabled is true but there is no clickhouse section; add one (endpoint_url, user, password) or set prefix_chain.enabled: false".to_string());
        }
        if tokenizer_url.trim().is_empty() {
            return Err("prefix_chain.enabled is true but cache.tokenizer_url is empty; per-block token counts need tokenizer-svc (set DWCTL_CACHE__TOKENIZER_URL)".to_string());
        }
        self.key_bytes()?;
        if let Some(models) = &self.models
            && models.is_empty()
        {
            return Err("prefix_chain.models is an empty list: an armed sink that records nothing is indistinguishable from a broken one. Omit models to capture every model, or set prefix_chain.enabled: false".to_string());
        }
        if !is_identifier(&self.table) {
            return Err(format!("prefix_chain.table {:?} is not a plain identifier", self.table));
        }
        if self.max_in_flight == 0 {
            return Err("prefix_chain.max_in_flight must be > 0".to_string());
        }
        self.sink_options().validate().map_err(|e| format!("prefix_chain: {e}"))
    }

    /// Decode the hex key. 32 bytes exactly: a shorter key is a typo, a longer one is
    /// probably the wrong secret.
    pub fn key_bytes(&self) -> Result<[u8; 32], String> {
        let raw = hex::decode(self.hmac_key.trim())
            .map_err(|_| "prefix_chain.hmac_key is not hex (expected 64 hex characters, DWCTL_PREFIX_CHAIN__HMAC_KEY)".to_string())?;
        <[u8; 32]>::try_from(raw).map_err(|v| format!("prefix_chain.hmac_key decodes to {} bytes, expected 32", v.len()))
    }

    pub fn sink_options(&self) -> SinkOptions {
        SinkOptions {
            table: self.table.clone(),
            flush_interval: self.flush_interval,
            max_batch_rows: self.max_batch_rows,
            queue_capacity: self.queue_capacity,
            retry_delay: self.retry_delay,
        }
    }
}

/// Which models the recorder captures. Built from `prefix_chain.models`.
#[derive(Debug, Clone)]
pub enum ModelFilter {
    All,
    Only(HashSet<String>),
}

impl ModelFilter {
    pub fn from_config(models: &Option<Vec<String>>) -> Self {
        match models {
            None => Self::All,
            Some(list) => Self::Only(list.iter().cloned().collect()),
        }
    }

    pub fn allows(&self, model: &str) -> bool {
        match self {
            Self::All => true,
            Self::Only(set) => set.contains(model),
        }
    }
}

/// One row of `clay.prompt_chains`. Field names are the column names; the sink inserts
/// it as `JSONEachRow`. `ts` serializes as RFC3339 (`...Z`), which the sink's
/// `date_time_input_format=best_effort` setting parses.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PromptChainRow {
    pub ts: DateTime<Utc>,
    pub instance_id: Uuid,
    pub correlation_id: i64,
    pub principal_id: Uuid,
    pub model: String,
    /// Hex, 32 characters each, one per block boundary in parse order.
    pub chain: Vec<String>,
    pub block_roles: Vec<String>,
    /// Empty when the model is unmapped on tokenizer-svc or the call failed.
    pub block_tokens: Vec<u32>,
    pub tokenizer_version: String,
    pub key_version: u8,
    pub v: u8,
}

/// The content-free view of one prompt, before tokenization: hashes and roles per
/// block, plus the block texts that go to tokenizer-svc and nowhere else.
pub struct Chain {
    pub hashes: Vec<String>,
    pub roles: Vec<String>,
    texts: Vec<String>,
}

impl Chain {
    pub fn len(&self) -> usize {
        self.hashes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }
}

/// What the analytics handler hands the recorder for one completed request.
pub struct Observation {
    pub instance_id: Uuid,
    pub correlation_id: i64,
    pub timestamp: DateTime<Utc>,
    /// The alias the request named, as `http_analytics.model` records it.
    pub model: Option<String>,
    /// The bearer token, resolved to the billing principal off the request path.
    pub api_key: Option<String>,
    pub path: String,
    pub body: Option<Bytes>,
}

/// The pure part: parse with the cache layer's parser, key each cumulative hash.
pub struct ChainHasher {
    key: [u8; 32],
    tier_policy: TierPolicy,
    telemetry: TelemetryPolicy,
}

impl ChainHasher {
    pub fn new(key: [u8; 32], tier_policy: TierPolicy, telemetry: TelemetryPolicy) -> Self {
        Self {
            key,
            tier_policy,
            telemetry,
        }
    }

    /// No I/O, no side effects. The block texts in the result go to tokenizer-svc and
    /// nowhere else.
    pub fn chain_for(&self, body: &[u8]) -> Result<Chain, ParseError> {
        let parsed = parse_chat_completions(body, &self.tier_policy, &self.telemetry)?;
        let hashes = parsed
            .cumulative_hashes
            .iter()
            .map(|h| hex::encode(&self.mac(h)[..HASH_BYTES]))
            .collect();
        let roles = parsed.blocks.iter().map(|b| b.role.clone()).collect();
        let texts = parsed.blocks.into_iter().map(|b| b.text).collect();
        Ok(Chain { hashes, roles, texts })
    }

    fn mac(&self, data: &[u8]) -> [u8; 32] {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).expect("a 32-byte key is always a valid HMAC key");
        mac.update(data);
        mac.finalize().into_bytes().into()
    }
}

pub struct PrefixChainRecorder {
    hasher: ChainHasher,
    key_version: u8,
    filter: ModelFilter,
    tokenizer: TokenizerClient,
    principal: PrincipalResolver,
    sink: SinkHandle<PromptChainRow>,
    /// Bounds the spawned tail (principal lookup, tokenization, send). See
    /// `PrefixChainConfig::max_in_flight`.
    in_flight: Arc<Semaphore>,
}

impl fmt::Debug for PrefixChainRecorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrefixChainRecorder")
            .field("key_version", &self.key_version)
            .field("filter", &self.filter)
            .finish_non_exhaustive()
    }
}

/// A fixed, content-free name for a parse failure. `ParseError`'s Display can echo the
/// client's own value (an invalid TTL string, an unsupported content type), so logs and
/// labels use this instead.
fn parse_error_kind(e: &ParseError) -> &'static str {
    match e {
        ParseError::Json(_) => "json",
        ParseError::TooManyBreakpoints => "too_many_breakpoints",
        ParseError::InvalidTtl(_) => "invalid_ttl",
        ParseError::UnsupportedType(_) => "unsupported_type",
        ParseError::DisabledTier(_) => "disabled_tier",
        ParseError::MalformedCacheControl => "malformed_cache_control",
        ParseError::AutomaticTtlConflict => "automatic_ttl_conflict",
        ParseError::NoAutomaticSlot => "automatic_no_slot",
    }
}

fn skipped(reason: &'static str) {
    counter!("dwctl_prefix_chain_skipped_total", "reason" => reason).increment(1);
}

impl PrefixChainRecorder {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        key: [u8; 32],
        key_version: u8,
        filter: ModelFilter,
        tier_policy: TierPolicy,
        telemetry: TelemetryPolicy,
        tokenizer: TokenizerClient,
        principal: PrincipalResolver,
        sink: SinkHandle<PromptChainRow>,
        max_in_flight: usize,
    ) -> Self {
        Self {
            hasher: ChainHasher::new(key, tier_policy, telemetry),
            key_version,
            filter,
            tokenizer,
            principal,
            sink,
            in_flight: Arc::new(Semaphore::new(max_in_flight.max(1))),
        }
    }

    /// Record one request. The synchronous part (path and model filter, parse, HMAC)
    /// is microseconds and runs inline; principal resolution, tokenization and the
    /// sink send are spawned so a slow tokenizer-svc never backs up the analytics
    /// handler. Every early exit increments `dwctl_prefix_chain_skipped_total`.
    pub fn observe(self: &Arc<Self>, obs: Observation) {
        if !obs.path.ends_with("/chat/completions") {
            skipped("not_chat");
            return;
        }
        let Some(model) = obs.model else {
            skipped("no_model");
            return;
        };
        if !self.filter.allows(&model) {
            skipped("model_filtered");
            return;
        }
        let Some(body) = obs.body else {
            skipped("no_body");
            return;
        };
        let Some(api_key) = obs.api_key else {
            skipped("no_api_key");
            return;
        };
        let chain = match self.chain_for(&body) {
            Ok(c) if !c.is_empty() => c,
            Ok(_) => {
                skipped("empty_prompt");
                return;
            }
            Err(e) => {
                // A body the cache parser rejects (malformed markers, not JSON). Only the
                // rule that fired is logged: some ParseError variants carry the offending
                // client value in their Display text, which must never reach a log.
                let kind = parse_error_kind(&e);
                debug!(
                    correlation_id = obs.correlation_id,
                    kind, "prefix chain: prompt not parseable; skipped"
                );
                skipped("parse_error");
                return;
            }
        };
        // Bound the spawned tail: the task holds the block texts and the key while it waits
        // on Postgres and tokenizer-svc, so under a slow dependency we drop, not queue.
        let Ok(permit) = Arc::clone(&self.in_flight).try_acquire_owned() else {
            skipped("backpressure");
            return;
        };
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let _permit = permit;
            this.finish(model, api_key, chain, obs.instance_id, obs.correlation_id, obs.timestamp)
                .await;
        });
    }

    /// See [`ChainHasher::chain_for`].
    pub fn chain_for(&self, body: &[u8]) -> Result<Chain, ParseError> {
        self.hasher.chain_for(body)
    }

    async fn finish(&self, model: String, api_key: String, chain: Chain, instance_id: Uuid, correlation_id: i64, ts: DateTime<Utc>) {
        let principal_id = match self.principal.resolve(&api_key).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                skipped("unknown_principal");
                return;
            }
            Err(e) => {
                crate::background_error!(PREFIX_CHAIN, "principal_lookup", Error, correlation_id, error = %e, "prefix chain: principal lookup failed; skipped");
                skipped("principal_error");
                return;
            }
        };

        let (block_tokens, tokenizer_version) = match self.tokenizer.tokenize(&model, &chain.texts).await {
            Ok(r) if r.segment_counts.len() == chain.len() => {
                counter!("dwctl_prefix_chain_tokenize_total", "outcome" => "ok").increment(1);
                (r.segment_counts, r.tokenizer_version)
            }
            Ok(r) => {
                warn!(
                    correlation_id,
                    got = r.segment_counts.len(),
                    want = chain.len(),
                    "prefix chain: tokenizer returned the wrong number of counts; recorded without counts"
                );
                counter!("dwctl_prefix_chain_tokenize_total", "outcome" => "mismatch").increment(1);
                (Vec::new(), String::new())
            }
            Err(TokenizerError::Unmapped(_)) => {
                counter!("dwctl_prefix_chain_tokenize_total", "outcome" => "unmapped").increment(1);
                (Vec::new(), String::new())
            }
            Err(e) => {
                crate::background_error!(PREFIX_CHAIN, "tokenize", Warning, correlation_id, error = %e, "prefix chain: tokenizer call failed; recorded without counts");
                counter!("dwctl_prefix_chain_tokenize_total", "outcome" => "error").increment(1);
                (Vec::new(), String::new())
            }
        };

        let row = PromptChainRow {
            ts,
            instance_id,
            correlation_id,
            principal_id,
            model,
            chain: chain.hashes,
            block_roles: chain.roles,
            block_tokens,
            tokenizer_version,
            key_version: self.key_version,
            v: RECORD_VERSION,
        };
        let outcome = if self.sink.send(row) { "recorded" } else { "dropped" };
        counter!("dwctl_prefix_chain_records_total", "outcome" => outcome).increment(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clickhouse::ClickhouseConfig;
    use tokio::sync::mpsc;

    fn key(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    fn policies() -> (TierPolicy, TelemetryPolicy) {
        (
            TierPolicy::from_config(&["5m".into(), "1h".into(), "24h".into()], "5m"),
            TelemetryPolicy::from_config(false, &[]),
        )
    }

    /// A recorder whose async tail can never succeed (unreachable tokenizer, resolver on
    /// the given pool) — enough for the pure `chain_for` and the inline filters.
    fn recorder_with(pool: sqlx::PgPool, key: [u8; 32], filter: ModelFilter) -> (Arc<PrefixChainRecorder>, mpsc::Receiver<PromptChainRow>) {
        let (tx, rx) = mpsc::channel(8);
        let (tier, telemetry) = policies();
        let rec = PrefixChainRecorder::new(
            key,
            1,
            filter,
            tier,
            telemetry,
            TokenizerClient::new("http://127.0.0.1:9"),
            PrincipalResolver::new(pool),
            SinkHandle::for_test(tx, "prompt_chains"),
            4,
        );
        (Arc::new(rec), rx)
    }

    /// The pure part on its own: no pool, no sink.
    fn hasher(key: [u8; 32]) -> ChainHasher {
        let (tier, telemetry) = policies();
        ChainHasher::new(key, tier, telemetry)
    }

    fn body(messages: &[(&str, &str)], tools: usize) -> Vec<u8> {
        let msgs: Vec<serde_json::Value> = messages.iter().map(|(r, c)| serde_json::json!({"role": r, "content": c})).collect();
        let tools: Vec<serde_json::Value> = (0..tools)
            .map(|i| serde_json::json!({"type": "function", "function": {"name": format!("t{i}"), "parameters": {"type": "object"}}}))
            .collect();
        let mut v = serde_json::json!({"model": "m", "messages": msgs});
        if !tools.is_empty() {
            v["tools"] = serde_json::Value::Array(tools);
        }
        serde_json::to_vec(&v).unwrap()
    }

    #[test]
    fn chain_is_one_entry_per_block_in_parse_order() {
        let rec = hasher(key(1));
        let c = rec
            .chain_for(&body(&[("system", "s"), ("user", "u"), ("assistant", "a")], 2))
            .unwrap();
        assert_eq!(c.roles, vec!["tool_definition", "tool_definition", "system", "user", "assistant"]);
        assert_eq!(c.hashes.len(), 5);
        assert!(c.hashes.iter().all(|h| h.len() == 32 && h.chars().all(|ch| ch.is_ascii_hexdigit())));
        assert_eq!(c.hashes.iter().collect::<HashSet<_>>().len(), 5, "cumulative hashes are distinct");
    }

    /// The property everything downstream rests on: extending a conversation keeps the
    /// earlier chain as a prefix, and a different earlier turn changes every later entry.
    #[test]
    fn extending_a_conversation_extends_the_chain() {
        let rec = hasher(key(1));
        let one = rec.chain_for(&body(&[("system", "s"), ("user", "u1")], 0)).unwrap();
        let two = rec
            .chain_for(&body(&[("system", "s"), ("user", "u1"), ("assistant", "a1"), ("user", "u2")], 0))
            .unwrap();
        assert_eq!(&two.hashes[..2], &one.hashes[..]);
        let other = rec
            .chain_for(&body(
                &[("system", "s"), ("user", "DIFFERENT"), ("assistant", "a1"), ("user", "u2")],
                0,
            ))
            .unwrap();
        assert_eq!(other.hashes[0], two.hashes[0], "shared system prompt");
        assert!(
            other.hashes[1..].iter().zip(&two.hashes[1..]).all(|(a, b)| a != b),
            "everything after the divergence differs"
        );
    }

    #[test]
    fn same_body_same_key_is_deterministic_and_a_different_key_is_unrelated() {
        let a = hasher(key(1));
        let b = hasher(key(1));
        let c = hasher(key(2));
        let payload = body(&[("user", "hello")], 0);
        assert_eq!(a.chain_for(&payload).unwrap().hashes, b.chain_for(&payload).unwrap().hashes);
        assert_ne!(a.chain_for(&payload).unwrap().hashes, c.chain_for(&payload).unwrap().hashes);
    }

    #[test]
    fn chain_never_contains_content() {
        let rec = hasher(key(1));
        let c = rec.chain_for(&body(&[("user", "the secret phrase")], 0)).unwrap();
        let serialized = serde_json::to_string(&c.hashes).unwrap() + &serde_json::to_string(&c.roles).unwrap();
        assert!(!serialized.contains("secret"));
    }

    #[test]
    fn unparseable_bodies_are_errors_not_panics() {
        let rec = hasher(key(1));
        assert!(rec.chain_for(b"not json").is_err());
        assert!(rec.chain_for(&body(&[], 0)).unwrap().is_empty(), "no blocks, empty chain");
    }

    #[test]
    fn model_filter_semantics() {
        assert!(ModelFilter::from_config(&None).allows("anything"));
        let only = ModelFilter::from_config(&Some(vec!["runware-zai/glm-5.2".into()]));
        assert!(only.allows("runware-zai/glm-5.2"));
        assert!(!only.allows("Runware-zai/glm-5.2"), "exact, case-sensitive");
        assert!(!only.allows("other"));
    }

    fn ch() -> ClickhouseConfig {
        ClickhouseConfig {
            endpoint_url: "https://x.clickhouse.cloud:8443".into(),
            database: "clay".into(),
            user: "dwctl".into(),
            password: "pw".into(),
            request_timeout: Duration::from_secs(30),
        }
    }

    fn enabled() -> PrefixChainConfig {
        PrefixChainConfig {
            enabled: true,
            hmac_key: "ab".repeat(32),
            ..Default::default()
        }
    }

    #[test]
    fn disabled_config_ignores_everything_else() {
        let mut c = PrefixChainConfig::default();
        c.hmac_key = "garbage".into();
        c.models = Some(vec![]);
        assert!(c.validate(None, "").is_ok());
    }

    #[test]
    fn enabled_config_requires_the_connection_the_tokenizer_and_a_real_key() {
        assert!(enabled().validate(Some(&ch()), "http://tokenizer-svc:8088").is_ok());
        assert!(
            enabled()
                .validate(None, "http://tokenizer-svc:8088")
                .unwrap_err()
                .contains("clickhouse section")
        );
        assert!(enabled().validate(Some(&ch()), "").unwrap_err().contains("tokenizer_url"));
        let mut c = enabled();
        c.hmac_key = "abc".into();
        assert!(c.validate(Some(&ch()), "http://t").unwrap_err().contains("hmac_key"));
        let mut c = enabled();
        c.hmac_key = "00".repeat(16);
        assert!(c.validate(Some(&ch()), "http://t").unwrap_err().contains("expected 32"));
    }

    #[test]
    fn empty_model_list_is_rejected_but_null_and_lists_are_fine() {
        let mut c = enabled();
        c.models = Some(vec![]);
        assert!(c.validate(Some(&ch()), "http://t").unwrap_err().contains("empty list"));
        c.models = None;
        assert!(c.validate(Some(&ch()), "http://t").is_ok());
        c.models = Some(vec!["a".into()]);
        assert!(c.validate(Some(&ch()), "http://t").is_ok());
    }

    #[test]
    fn models_deserializes_from_a_list_or_a_comma_string() {
        let from_yaml: PrefixChainConfig = serde_json::from_str(r#"{"models": ["a", "b"]}"#).unwrap();
        assert_eq!(from_yaml.models, Some(vec!["a".into(), "b".into()]));
        let from_env: PrefixChainConfig = serde_json::from_str(r#"{"models": "a, b ,c"}"#).unwrap();
        assert_eq!(from_env.models, Some(vec!["a".into(), "b".into(), "c".into()]));
        let empty: PrefixChainConfig = serde_json::from_str(r#"{"models": ""}"#).unwrap();
        assert_eq!(
            empty.models,
            Some(vec![]),
            "an empty string is the empty list, which validation rejects"
        );
        let absent: PrefixChainConfig = serde_json::from_str(r#"{"enabled": false}"#).unwrap();
        assert_eq!(absent.models, None);
        let null: PrefixChainConfig = serde_json::from_str(r#"{"models": null}"#).unwrap();
        assert_eq!(null.models, None);
    }

    #[test]
    fn parse_errors_log_a_fixed_kind_not_the_client_value() {
        let (tier, telemetry) = policies();
        let err = parse_chat_completions(
            br#"{"model":"m","tools":[{"type":"function","function":{"name":"t"},"cache_control":{"type":"SECRET-VALUE"}}],"messages":[{"role":"user","content":"x"}]}"#,
            &tier,
            &telemetry,
        )
        .unwrap_err();
        assert!(err.to_string().contains("SECRET-VALUE"), "Display echoes the client value: {err}");
        assert!(!parse_error_kind(&err).contains("SECRET"));
    }

    #[test]
    fn debug_output_redacts_the_key() {
        let s = format!("{:?}", enabled());
        assert!(s.contains("<redacted>"));
        assert!(!s.contains("abab"), "{s}");
    }

    fn observation(path: &str, model: Option<&str>, api_key: Option<&str>, body: Option<Vec<u8>>) -> Observation {
        Observation {
            instance_id: Uuid::nil(),
            correlation_id: 7,
            timestamp: Utc::now(),
            model: model.map(String::from),
            api_key: api_key.map(String::from),
            path: path.to_string(),
            body: body.map(Bytes::from),
        }
    }

    /// The inline part of `observe` filters without spawning: nothing reaches the sink
    /// for non-chat paths, filtered models, or missing pieces.
    #[sqlx::test]
    async fn observe_filters_before_spawning(pool: sqlx::PgPool) {
        let (rec, mut rx) = recorder_with(pool, key(1), ModelFilter::from_config(&Some(vec!["m".into()])));
        let b = body(&[("user", "hi")], 0);
        rec.observe(observation("/embeddings", Some("m"), Some("sk"), Some(b.clone())));
        rec.observe(observation("/chat/completions", Some("other"), Some("sk"), Some(b.clone())));
        rec.observe(observation("/chat/completions", None, Some("sk"), Some(b.clone())));
        rec.observe(observation("/chat/completions", Some("m"), None, Some(b.clone())));
        rec.observe(observation("/chat/completions", Some("m"), Some("sk"), None));
        rec.observe(observation("/chat/completions", Some("m"), Some("sk"), Some(b"nope".to_vec())));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_err(), "nothing recorded");
    }
}
