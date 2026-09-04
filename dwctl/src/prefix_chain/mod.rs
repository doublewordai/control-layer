//! Prefix-chain capture: a content-free record of each chat-completions prompt's
//! structure, written to ClickHouse.
//!
//! Per served request: one entry per content block of the prompt in parse order (tool
//! definitions, system, messages, tool calls), each an HMAC-SHA256 over the cumulative
//! content up to that block, truncated to 16 bytes and hex-encoded; the block roles; and
//! each block's own token count from tokenizer-svc. Equal prefixes give equal chain
//! prefixes, so shared prefixes and multi-turn continuations are recoverable without
//! storing content, and without the key the table is structure only.
//!
//! The block split and the cumulative hashes come from the prompt-cache parser
//! ([`crate::prompt_cache::parse_chat_completions`]) under the same policies the cache
//! classifier uses. Capture runs in the outlet analytics handler after the response, off
//! the request path; the classify path is not involved. Any failure yields no record and
//! a counter. Delivery is best effort (see [`crate::clickhouse`]).

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

use crate::clickhouse::{ClickhouseConfig, SinkHandle, SinkOptions};
use crate::metrics::errors::component::PREFIX_CHAIN;
use crate::prompt_cache::{
    ParseError, PrincipalResolver, TelemetryPolicy, TierPolicy, TokenizerClient, TokenizerError, parse_chat_completions,
};

/// Schema version of [`PromptChainRow`]. Bump when the row shape changes.
pub const RECORD_VERSION: u8 = 1;
/// Bytes of the HMAC kept per chain entry (hex-encoded to 32 characters in the row).
const HASH_BYTES: usize = 16;

/// Table the rows land in; its shape is fixed by the warehouse migration.
const TABLE: &str = "prompt_chains";
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
const MAX_BATCH_ROWS: usize = 5_000;
const QUEUE_CAPACITY: usize = 20_000;
const RETRY_DELAY: Duration = Duration::from_millis(500);
/// Cap on requests whose principal lookup and tokenization are in flight at once; beyond
/// it a request is skipped (counted `backpressure`) rather than queued holding its prompt.
const MAX_IN_FLIGHT: usize = 256;

/// The `prefix_chain` config section.
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PrefixChainConfig {
    /// `false` is the only off state.
    pub enabled: bool,
    /// `null`: every model. A non-empty list: exactly these dwctl aliases, matched exactly
    /// and case-sensitively. An empty list is a startup error. Accepts a YAML list or a
    /// comma-separated string (env).
    #[serde(deserialize_with = "deserialize_model_list")]
    pub models: Option<Vec<String>>,
    /// 32-byte key, hex-encoded (64 characters). `DWCTL_PREFIX_CHAIN__HMAC_KEY`.
    pub hmac_key: String,
}

impl fmt::Debug for PrefixChainConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrefixChainConfig")
            .field("enabled", &self.enabled)
            .field("models", &self.models)
            .field("hmac_key", &if self.hmac_key.is_empty() { "<unset>" } else { "<redacted>" })
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
    /// `enabled: false` ignores everything else. `enabled: true` requires the `clickhouse`
    /// section, a tokenizer-svc URL, a 32-byte key, and a non-empty `models` list if one
    /// is given.
    pub fn validate(&self, clickhouse: Option<&ClickhouseConfig>, tokenizer_url: &str) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if clickhouse.is_none() {
            return Err("prefix_chain.enabled is true but there is no clickhouse section".to_string());
        }
        if tokenizer_url.trim().is_empty() {
            return Err("prefix_chain.enabled is true but cache.tokenizer_url is empty".to_string());
        }
        self.key_bytes()?;
        if let Some(models) = &self.models
            && models.is_empty()
        {
            return Err(
                "prefix_chain.models is an empty list; omit it to capture every model, or set prefix_chain.enabled: false".to_string(),
            );
        }
        Ok(())
    }

    pub fn key_bytes(&self) -> Result<[u8; 32], String> {
        let raw =
            hex::decode(self.hmac_key.trim()).map_err(|_| "prefix_chain.hmac_key is not hex (expected 64 hex characters)".to_string())?;
        <[u8; 32]>::try_from(raw).map_err(|v| format!("prefix_chain.hmac_key decodes to {} bytes, expected 32", v.len()))
    }
}

/// Sink settings for the chain table.
pub fn sink_options() -> SinkOptions {
    SinkOptions {
        table: TABLE.to_string(),
        flush_interval: FLUSH_INTERVAL,
        max_batch_rows: MAX_BATCH_ROWS,
        queue_capacity: QUEUE_CAPACITY,
        retry_delay: RETRY_DELAY,
    }
}

/// Which key produced a chain: the first four bytes of SHA-256 over the key, big-endian.
/// Rows stamped with different ids never join, and nobody has to remember to bump a
/// version on rotation.
pub fn key_id(key: &[u8; 32]) -> u32 {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(key);
    u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]])
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
    /// See [`key_id`]; stored in the `key_version` column.
    pub key_version: u32,
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
    key_id: u32,
    filter: ModelFilter,
    tokenizer: TokenizerClient,
    principal: PrincipalResolver,
    sink: SinkHandle<PromptChainRow>,
    in_flight: Arc<Semaphore>,
}

impl fmt::Debug for PrefixChainRecorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrefixChainRecorder")
            .field("key_id", &self.key_id)
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
    pub fn new(
        key: [u8; 32],
        filter: ModelFilter,
        tier_policy: TierPolicy,
        telemetry: TelemetryPolicy,
        tokenizer: TokenizerClient,
        principal: PrincipalResolver,
        sink: SinkHandle<PromptChainRow>,
    ) -> Self {
        Self {
            key_id: key_id(&key),
            hasher: ChainHasher::new(key, tier_policy, telemetry),
            filter,
            tokenizer,
            principal,
            sink,
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
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
            key_version: self.key_id,
            v: RECORD_VERSION,
        };
        let outcome = if self.sink.send(row) { "recorded" } else { "dropped" };
        counter!("dwctl_prefix_chain_records_total", "outcome" => outcome).increment(1);
    }
}

/// The outlet handler: sees every captured request and response, and hands served
/// chat-completions requests to the recorder.
pub struct PrefixChainHandler {
    recorder: Arc<PrefixChainRecorder>,
    /// The analytics instance id, so rows join `http_analytics` on
    /// `(instance_id, correlation_id)`.
    instance_id: Uuid,
    config: crate::config::Config,
}

impl PrefixChainHandler {
    pub fn new(recorder: Arc<PrefixChainRecorder>, instance_id: Uuid, config: crate::config::Config) -> Self {
        Self {
            recorder,
            instance_id,
            config,
        }
    }
}

impl outlet::RequestHandler for PrefixChainHandler {
    async fn handle_request(&self, _data: outlet::RequestData) {}

    async fn handle_response(&self, request_data: outlet::RequestData, response_data: outlet::ResponseData) {
        // Only served requests: a rejected one (bad key, unknown model, rate limit) never
        // becomes a principal lookup, a tokenizer call or a row.
        if !response_data.status.is_success() {
            return;
        }
        // The path check comes before the body parse: every other route is skipped for
        // the cost of a string compare, not a JSON parse of its request body.
        if !request_data.uri.path().ends_with("/chat/completions") {
            skipped("not_chat");
            return;
        }
        let api_key = match crate::request_logging::serializers::Auth::from_request(&request_data, &self.config) {
            crate::request_logging::serializers::Auth::ApiKey { bearer_token } => Some(bearer_token),
            crate::request_logging::serializers::Auth::None => None,
        };
        let model = request_data
            .body
            .as_ref()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok())
            .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from));
        self.recorder.observe(Observation {
            instance_id: self.instance_id,
            correlation_id: request_data.correlation_id as i64,
            timestamp: DateTime::<Utc>::from(request_data.timestamp),
            model,
            api_key,
            path: request_data.uri.path().to_string(),
            body: request_data.body.clone(),
        });
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
            filter,
            tier,
            telemetry,
            TokenizerClient::new("http://127.0.0.1:9"),
            PrincipalResolver::new(pool),
            SinkHandle::for_test(tx, "prompt_chains"),
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
        let only = ModelFilter::from_config(&Some(vec!["vendor/model-a".into()]));
        assert!(only.allows("vendor/model-a"));
        assert!(!only.allows("Vendor/model-a"), "exact, case-sensitive");
        assert!(!only.allows("other"));
    }

    fn ch() -> ClickhouseConfig {
        ClickhouseConfig {
            endpoint_url: "https://x.clickhouse.cloud:8443".into(),
            database: "clay".into(),
            user: "dwctl".into(),
            password: "pw".into(),
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
    fn key_id_is_derived_from_the_key() {
        assert_eq!(key_id(&key(1)), key_id(&key(1)));
        assert_ne!(key_id(&key(1)), key_id(&key(2)));
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
        let nothing = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await;
        assert!(nothing.is_err(), "nothing recorded");
    }
}
