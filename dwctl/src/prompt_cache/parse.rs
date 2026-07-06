//! Parse an OpenAI chat-completions request into the cache primitives
//!: the ordered content blocks, a cumulative content
//! hash at every block boundary, and the explicit `cache_control` breakpoints (≤4)
//! with their TTL tiers.
//!
//! Reads and writes derive from these: a breakpoint's prefix hash — plus up to a
//! 20-block walk-back of earlier boundary hashes — is looked up for read hits; the
//! span beyond the matched read is tokenized for the write. The hash **excludes** the
//! `cache_control` directive itself, so identical content carrying different markers
//! matches, and the hashed bytes are exactly what onwards forwards after stripping.
//!
//! Scope: text content blocks on chat-completions messages, plus **tool definitions**
//! (the `tools` array). Tools are hashed **before** `messages`; within each array, blocks
//! are hashed in the order sent — we do NOT reorder or partition out `system`. So the
//! canonical `tools → system → messages` hierarchy is a convention the caller must follow
//! (send `system` first) for stable cache keys; order-normalization will come with a single
//! canonical internal representation.
//! A tool's write-side token count is an *estimate*: we tokenize the tool's JSON, not the
//! model's chat-template rendering of it (which adds scaffolding we don't count) — the same
//! content-vs-rendered approximation already used for message text. Image-token caching is
//! deferred — image blocks still contribute to the prefix hash but carry no text, so their
//! tokens fall into the uncached tail.
//!
//! Provider-injected per-request *telemetry* blocks — e.g. the Claude Code SDK's
//! `x-anthropic-billing-header` line, whose `cch=<nonce>` changes on every request — are
//! excluded from this view (see [`TelemetryPolicy`]) so they don't poison the prefix hash and
//! force write-only caching. In strip mode they're also removed from the forwarded request.

use sha2::{Digest, Sha256};

use super::index::{TierPolicy, TtlTier};

/// Per-request breakpoint cap. Mirrors Anthropic's public limit
/// of **4 `cache_control` breakpoints** per request — enough for the common
/// tools→system→history→latest-turn pattern; more than that is rejected as abuse.
pub const MAX_BREAKPOINTS: usize = 4;
/// Walk-back window per breakpoint. Mirrors Anthropic's documented **20-block**
/// look-back: from a breakpoint we search up to 20 earlier block boundaries for a prior
/// write before giving up (a match further back is a genuine miss — the documented fix is
/// a second breakpoint). Bounds the per-request lookup-candidate set.
pub const WALK_BACK: usize = 20;

/// Synthetic role for tool-definition blocks (the `tools` array). Distinct from the `tool`
/// *message* role (tool results) so the two hash into different cache entries, and included
/// in the block hash like any other role.
const TOOL_DEFINITION_ROLE: &str = "tool_definition";

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("request body is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("too many cache_control breakpoints (max {MAX_BREAKPOINTS})")]
    TooManyBreakpoints,
    #[error("invalid cache_control ttl: {0:?}")]
    InvalidTtl(String),
    #[error("unsupported cache_control type: {0:?} (only \"ephemeral\")")]
    UnsupportedType(String),
    #[error("cache_control ttl tier '{}' is not currently available", .0.as_str())]
    DisabledTier(TtlTier),
    #[error("cache_control must be an object with a string \"type\": \"ephemeral\" (and an optional string \"ttl\")")]
    MalformedCacheControl,
}

/// A single content block in canonical order.
#[derive(Debug, Clone)]
pub struct Block {
    pub role: String,
    /// Text content for tokenization (the write-side segment). Empty for non-text
    /// blocks (e.g. images), which then bill as uncached.
    pub text: String,
}

/// An explicit cache breakpoint (a `cache_control`-marked block).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breakpoint {
    /// Index into `blocks` of the marked block (the inclusive prefix end).
    pub block_index: usize,
    pub ttl_tier: TtlTier,
}

/// The parsed cache view of a request.
#[derive(Debug, Clone)]
pub struct ParsedPrompt {
    pub blocks: Vec<Block>,
    /// Cumulative content hash *after* each block (`len() == blocks.len()`).
    pub cumulative_hashes: Vec<Vec<u8>>,
    /// Explicit breakpoints in block order, guaranteed `≤ MAX_BREAKPOINTS`.
    pub breakpoints: Vec<Breakpoint>,
}

impl ParsedPrompt {
    /// Walk-back read candidates for a breakpoint: the cumulative hash at the
    /// breakpoint, then each earlier boundary back through a 20-block window —
    /// **longest prefix first**, so the first index hit is the longest read.
    pub fn read_candidates(&self, bp: &Breakpoint) -> Vec<Vec<u8>> {
        let i = bp.block_index;
        // The range `start..=i` is inclusive on both ends, so subtract `WALK_BACK - 1`
        // (not `WALK_BACK`) to get exactly `WALK_BACK` candidates (e.g. i=24 → 5..=24 = 20).
        let start = i.saturating_sub(WALK_BACK - 1);
        (start..=i).rev().map(|j| self.cumulative_hashes[j].clone()).collect()
    }
}

/// Parse a chat-completions body into its cache primitives. Callers decide what a `ParseError`
/// means: the classifier degrades to "no cache" (the request is forwarded untouched), while the
/// request-path [`validate_markers`] surfaces it to the client as a 400.
pub fn parse_chat_completions(body: &[u8], policy: &TierPolicy, telemetry: &TelemetryPolicy) -> Result<ParsedPrompt, ParseError> {
    let v: serde_json::Value = serde_json::from_slice(body)?;

    let mut blocks: Vec<Block> = Vec::new();
    let mut breakpoints: Vec<Breakpoint> = Vec::new();
    let mut cumulative_hashes: Vec<Vec<u8>> = Vec::new();
    let mut hasher = Sha256::new();

    // Tools come FIRST in the canonical tools → system → messages order, so hash them before
    // the messages. Each tool definition is one block; a `cache_control` on the tool object
    // (`tools[i].cache_control` — the slot OpenAI clients set directly and the Anthropic
    // ingress translation maps its native tool marker to) marks a breakpoint. The block text
    // is the tool's JSON, tokenized as an estimate of the tool's contribution.
    if let Some(tools) = v.get("tools").and_then(|t| t.as_array()) {
        for tool in tools {
            let ttl = match tool.get("cache_control") {
                Some(cc) if !cc.is_null() => Some(parse_ttl(cc, policy)?),
                _ => None,
            };
            let stripped = strip_cache_control(tool);
            // The whole tool schema is the write-side text (no single "text" field). A
            // serialization failure here is not expected for a `serde_json::Value`, but if it
            // ever happens we propagate it (→ ParseError::Json → degrade to no-cache) rather
            // than silently caching an empty tool and undercounting its tokens.
            let text = serde_json::to_string(&stripped)?;

            let canonical = canonical_block_bytes(TOOL_DEFINITION_ROLE, &stripped);
            hasher.update(&canonical);
            cumulative_hashes.push(hasher.clone().finalize().to_vec());

            let block_index = blocks.len();
            blocks.push(Block {
                role: TOOL_DEFINITION_ROLE.to_string(),
                text,
            });
            if let Some(ttl_tier) = ttl {
                breakpoints.push(Breakpoint { block_index, ttl_tier });
            }
        }
    }

    if let Some(messages) = v.get("messages").and_then(|m| m.as_array()) {
        for msg in messages {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("").to_string();

            match msg.get("content") {
                // String content: one implicit text block, no marker possible.
                Some(serde_json::Value::String(s)) => {
                    let canonical = canonical_block_bytes(&role, &serde_json::json!({ "type": "text", "text": s }));
                    hasher.update(&canonical);
                    cumulative_hashes.push(hasher.clone().finalize().to_vec());
                    blocks.push(Block {
                        role: role.clone(),
                        text: s.clone(),
                    });
                }
                // Array content: a sequence of blocks, each possibly marked.
                Some(serde_json::Value::Array(arr)) => {
                    for block in arr {
                        // Exclude provider-injected telemetry blocks (e.g. the Claude Code SDK's
                        // `x-anthropic-billing-header` line with its per-request `cch` nonce) from the
                        // cache prefix: left in, they'd change the prefix hash every turn and force
                        // write-only caching. `excludes_block` only matches UNMARKED blocks, so a
                        // caller's `cache_control` breakpoint is never dropped. (In strip mode the
                        // outbound sanitiser also removes them from the forwarded request.)
                        if telemetry.excludes_block(&role, block) {
                            continue;
                        }
                        let ttl = match block.get("cache_control") {
                            // An explicit `null` (or absent) is "no marker"; anything else is
                            // validated by `parse_ttl` (which rejects non-object values).
                            Some(cc) if !cc.is_null() => Some(parse_ttl(cc, policy)?),
                            _ => None,
                        };
                        let stripped = strip_cache_control(block);
                        let text = stripped.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();

                        let canonical = canonical_block_bytes(&role, &stripped);
                        hasher.update(&canonical);
                        cumulative_hashes.push(hasher.clone().finalize().to_vec());

                        let block_index = blocks.len();
                        blocks.push(Block { role: role.clone(), text });
                        if let Some(ttl_tier) = ttl {
                            breakpoints.push(Breakpoint { block_index, ttl_tier });
                        }
                    }
                }
                _ => {}
            }
        }
    }

    if breakpoints.len() > MAX_BREAKPOINTS {
        return Err(ParseError::TooManyBreakpoints);
    }

    Ok(ParsedPrompt {
        blocks,
        cumulative_hashes,
        breakpoints,
    })
}

/// `cache_control: { type: "ephemeral"[, ttl: "5m"|"1h"|"24h"] }`. `type` is required; a missing
/// `ttl` defaults to `policy.default_ttl` (Anthropic-style; configurable). Errors:
/// - a non-object marker, a missing/non-string `type`, or a non-string `ttl` → [`ParseError::MalformedCacheControl`]
/// - a string `type` that isn't `"ephemeral"` → [`ParseError::UnsupportedType`]
/// - an unknown `ttl` string → [`ParseError::InvalidTtl`]
/// - a valid tier the platform has disabled (not in `enabled_ttls`) → [`ParseError::DisabledTier`]
fn parse_ttl(cache_control: &serde_json::Value, policy: &TierPolicy) -> Result<TtlTier, ParseError> {
    use serde_json::Value;
    // Must be an object; a present `type`/`ttl` must be a *string*. A non-string field is
    // malformed, not "absent" — otherwise `.as_str()` would return `None` and e.g. `ttl: 123`
    // would silently default rather than being surfaced as a 400. (An explicit `null` marker is
    // filtered out as "no marker" before we get here.)
    if !cache_control.is_object() {
        return Err(ParseError::MalformedCacheControl);
    }
    // `type` is REQUIRED and must be the string "ephemeral" — Anthropic mandates it even though
    // it's the only valid value. Missing or non-string → malformed; a different string → unsupported.
    match cache_control.get("type") {
        Some(Value::String(t)) if t == "ephemeral" => {}
        Some(Value::String(t)) => return Err(ParseError::UnsupportedType(t.clone())),
        _ => return Err(ParseError::MalformedCacheControl),
    }
    let tier = match cache_control.get("ttl") {
        None => policy.default_ttl(),
        Some(Value::String(ttl)) => TtlTier::parse(ttl).ok_or_else(|| ParseError::InvalidTtl(ttl.clone()))?,
        Some(_) => return Err(ParseError::MalformedCacheControl),
    };
    if !policy.is_enabled(tier) {
        return Err(ParseError::DisabledTier(tier));
    }
    Ok(tier)
}

/// Synchronous request-path validation of `cache_control` markers — it skips the hashing that
/// the full [`parse_chat_completions`] does, so it's cheap enough to run before forwarding. Every
/// marker must be ephemeral, name an *enabled* tier (or default to one), and there must be
/// `≤ MAX_BREAKPOINTS`. A failure is turned into a 400 by the layer: the request is rejected
/// (like a bad parameter) rather than silently un-cached, so the client learns immediately and
/// isn't billed full price thinking it cached. Takes the body `Value` the layer already parses
/// to extract `model` — no extra deserialization.
pub fn validate_markers(body: &serde_json::Value, policy: &TierPolicy) -> Result<(), ParseError> {
    let mut breakpoints = 0usize;
    // Tool-definition markers (`tools[i].cache_control`) count toward the same breakpoint cap
    // and are validated identically — a malformed/disabled marker on a tool is a 400, not a
    // silent no-cache.
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        for tool in tools {
            match tool.get("cache_control") {
                Some(cc) if !cc.is_null() => {
                    parse_ttl(cc, policy)?;
                    breakpoints += 1;
                    if breakpoints > MAX_BREAKPOINTS {
                        return Err(ParseError::TooManyBreakpoints);
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
        for msg in messages {
            if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
                for block in arr {
                    match block.get("cache_control") {
                        // Explicit `null` (or absent) is "no marker"; a non-object value is
                        // rejected by parse_ttl as malformed.
                        Some(cc) if !cc.is_null() => {
                            parse_ttl(cc, policy)?;
                            breakpoints += 1;
                            // Short-circuit on the request path — stop the moment the cap is exceeded.
                            // The error reports "max N", not an exact count (we don't keep scanning).
                            if breakpoints > MAX_BREAKPOINTS {
                                return Err(ParseError::TooManyBreakpoints);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

fn strip_cache_control(block: &serde_json::Value) -> serde_json::Value {
    let mut b = block.clone();
    if let Some(obj) = b.as_object_mut() {
        obj.remove("cache_control");
    }
    b
}

/// Runtime policy for provider-injected *telemetry* blocks, built from `cache.telemetry_blocks`.
///
/// A content block is "telemetry" when its text starts with one of `prefixes` (e.g. the Claude Code
/// SDK's `x-anthropic-billing-header:` line, whose `cch=<nonce>` changes every request; real
/// Anthropic strips it before caching). Such a block sits ahead of the caller's `cache_control`
/// breakpoint, so left in the hashed prefix the cache can only ever WRITE, never READ (the write-only
/// bug). Matched **unmarked** blocks are always excluded from the cache prefix ([`excludes_block`]);
/// when `strip_from_prompt` is set the outbound sanitiser ([`super::inject::strip_cache_control`])
/// also removes them from the forwarded request, which additionally lets the upstream KV/prefix cache
/// see a stable prompt. An empty `prefixes` list disables the feature.
///
/// [`excludes_block`]: TelemetryPolicy::excludes_block
#[derive(Debug, Clone, Default)]
pub struct TelemetryPolicy {
    /// Also remove matched blocks from the forwarded request, not just the cache prefix.
    pub strip_from_prompt: bool,
    prefixes: Vec<String>,
}

impl TelemetryPolicy {
    pub fn from_config(strip_from_prompt: bool, prefixes: &[String]) -> Self {
        // Drop empty prefixes: `"".starts_with(..)` matches everything, so a stray empty entry
        // would (catastrophically) treat every unmarked system block as telemetry.
        Self {
            strip_from_prompt,
            prefixes: prefixes.iter().filter(|p| !p.is_empty()).cloned().collect(),
        }
    }

    /// Whether a `role` message's `block` is an UNMARKED telemetry block to exclude from the cache
    /// prefix (and, in strip mode, from the forwarded prompt). Constrained to the **system** role —
    /// that's where providers inject these blocks (e.g. the Claude Code SDK prepends its
    /// `x-anthropic-billing-header` to `system`) — so a user/assistant block that happens to start
    /// with a configured prefix is never silently excluded from the cache or stripped from the
    /// prompt. Only unmarked blocks match, so a caller's `cache_control` breakpoint is never dropped.
    /// Always `false` when `prefixes` is empty. The same predicate is used by parsing and outbound
    /// sanitisation, so the two stay consistent.
    pub fn excludes_block(&self, role: &str, block: &serde_json::Value) -> bool {
        if self.prefixes.is_empty() || role != TELEMETRY_ROLE {
            return false;
        }
        let unmarked = block.get("cache_control").is_none_or(|cc| cc.is_null());
        if !unmarked {
            return false;
        }
        let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let t = text.trim_start();
        self.prefixes.iter().any(|prefix| t.starts_with(prefix.as_str()))
    }
}

/// The one message role providers inject per-request telemetry blocks into. Telemetry handling is
/// restricted to it so non-system content that coincidentally starts with a configured prefix is
/// never excluded from the cache or stripped from the forwarded prompt. If a future provider injects
/// elsewhere, promote this to a configurable set.
const TELEMETRY_ROLE: &str = "system";

/// `role` + canonical JSON of the marker-stripped block. The role is included so the
/// same text under different roles hashes differently.
///
/// "Canonical" here relies on `serde_json` being built **without** the `preserve_order`
/// feature: `Value::Object` is then a `BTreeMap`, so `to_vec` emits keys in sorted order
/// and two blocks that differ only in key insertion order (common across SDKs/languages)
/// hash identically. If a dependency ever enables `preserve_order` (→ `IndexMap`,
/// insertion order), this would need explicit key-sorting to keep the cache-hit rate up.
fn canonical_block_bytes(role: &str, stripped_block: &serde_json::Value) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(role.as_bytes());
    out.push(0x00);
    out.extend_from_slice(&serde_json::to_vec(stripped_block).unwrap_or_default());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_tiers() -> TierPolicy {
        TierPolicy::from_config(&["5m".to_string(), "1h".to_string(), "24h".to_string()], "5m")
    }

    /// Telemetry handling disabled (empty prefixes) — the default for tests unconcerned with it.
    fn no_telemetry() -> TelemetryPolicy {
        TelemetryPolicy::default()
    }

    /// Telemetry handling enabled with the Claude Code SDK prefix.
    fn telemetry() -> TelemetryPolicy {
        TelemetryPolicy::from_config(true, &["x-anthropic-billing-header:".to_string()])
    }

    fn parse(body: serde_json::Value) -> ParsedPrompt {
        parse_chat_completions(body.to_string().as_bytes(), &all_tiers(), &no_telemetry()).unwrap()
    }

    fn parse_with(body: serde_json::Value, tele: &TelemetryPolicy) -> ParsedPrompt {
        parse_chat_completions(body.to_string().as_bytes(), &all_tiers(), tele).unwrap()
    }

    #[test]
    fn no_markers_no_breakpoints() {
        let p = parse(serde_json::json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "you are helpful"},
                {"role": "user", "content": "hi"}
            ]
        }));
        assert_eq!(p.blocks.len(), 2);
        assert_eq!(p.cumulative_hashes.len(), 2);
        assert!(p.breakpoints.is_empty());
        assert_ne!(p.cumulative_hashes[0], p.cumulative_hashes[1]);
    }

    #[test]
    fn single_marker_with_default_ttl() {
        let p = parse(serde_json::json!({
            "messages": [
                {"role": "system", "content": [
                    {"type": "text", "text": "long ctx", "cache_control": {"type": "ephemeral"}}
                ]},
                {"role": "user", "content": "q"}
            ]
        }));
        assert_eq!(p.breakpoints.len(), 1);
        assert_eq!(p.breakpoints[0].block_index, 0);
        assert_eq!(p.breakpoints[0].ttl_tier, TtlTier::FiveMinutes);
    }

    #[test]
    fn ttl_tiers_parse() {
        for (ttl, tier) in [
            ("5m", TtlTier::FiveMinutes),
            ("1h", TtlTier::OneHour),
            ("24h", TtlTier::TwentyFourHours),
        ] {
            let p = parse(serde_json::json!({
                "messages": [{"role": "system", "content": [
                    {"type": "text", "text": "x", "cache_control": {"type": "ephemeral", "ttl": ttl}}
                ]}]
            }));
            assert_eq!(p.breakpoints[0].ttl_tier, tier);
        }
    }

    #[test]
    fn invalid_ttl_errors() {
        let err = parse_chat_completions(
            serde_json::json!({
                "messages": [{"role": "system", "content": [
                    {"type": "text", "text": "x", "cache_control": {"type": "ephemeral", "ttl": "2h"}}
                ]}]
            })
            .to_string()
            .as_bytes(),
            &all_tiers(),
            &no_telemetry(),
        )
        .unwrap_err();
        assert!(matches!(err, ParseError::InvalidTtl(t) if t == "2h"));
    }

    #[test]
    fn unsupported_cache_control_type_errors() {
        let err = parse_chat_completions(
            serde_json::json!({
                "messages": [{"role": "system", "content": [
                    {"type": "text", "text": "x", "cache_control": {"type": "persistent"}}
                ]}]
            })
            .to_string()
            .as_bytes(),
            &all_tiers(),
            &no_telemetry(),
        )
        .unwrap_err();
        assert!(matches!(err, ParseError::UnsupportedType(t) if t == "persistent"));
    }

    #[test]
    fn more_than_four_breakpoints_errors() {
        let blocks: Vec<_> = (0..5)
            .map(|i| serde_json::json!({"type": "text", "text": format!("b{i}"), "cache_control": {"type": "ephemeral"}}))
            .collect();
        let err = parse_chat_completions(
            serde_json::json!({ "messages": [{"role": "user", "content": blocks}] })
                .to_string()
                .as_bytes(),
            &all_tiers(),
            &no_telemetry(),
        )
        .unwrap_err();
        assert!(matches!(err, ParseError::TooManyBreakpoints));
    }

    #[test]
    fn disabled_tier_rejected_by_validate_and_parse() {
        // Policy enables only 5m + 1h; a 24h marker is a valid-but-disabled tier.
        let policy = TierPolicy::from_config(&["5m".to_string(), "1h".to_string()], "5m");
        let body = serde_json::json!({
            "messages": [{"role": "system", "content": [
                {"type": "text", "text": "x", "cache_control": {"type": "ephemeral", "ttl": "24h"}}
            ]}]
        });
        let err = validate_markers(&body, &policy).unwrap_err();
        assert!(matches!(err, ParseError::DisabledTier(TtlTier::TwentyFourHours)));
        // The full parse rejects it identically (the two share parse_ttl, so they can't diverge).
        let err2 = parse_chat_completions(body.to_string().as_bytes(), &policy, &no_telemetry()).unwrap_err();
        assert!(matches!(err2, ParseError::DisabledTier(TtlTier::TwentyFourHours)));
    }

    #[test]
    fn validate_markers_default_ttl_honours_policy() {
        // No explicit ttl → defaults to the policy default. With default "1h", a no-ttl marker
        // becomes a 1h breakpoint (and passes because 1h is enabled).
        let policy = TierPolicy::from_config(&["1h".to_string()], "1h");
        let body = serde_json::json!({
            "messages": [{"role": "system", "content": [
                {"type": "text", "text": "x", "cache_control": {"type": "ephemeral"}}
            ]}]
        });
        assert!(validate_markers(&body, &policy).is_ok());
        let p = parse_chat_completions(body.to_string().as_bytes(), &policy, &no_telemetry()).unwrap();
        assert_eq!(p.breakpoints[0].ttl_tier, TtlTier::OneHour);
    }

    #[test]
    fn validate_markers_ok_and_counts_breakpoints() {
        let ok = serde_json::json!({
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "a", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
                {"type": "text", "text": "q"}
            ]}]
        });
        assert!(validate_markers(&ok, &all_tiers()).is_ok());

        // validate_markers enforces the breakpoint cap too (not just the full parse).
        let blocks: Vec<_> = (0..5)
            .map(|i| serde_json::json!({"type": "text", "text": format!("b{i}"), "cache_control": {"type": "ephemeral"}}))
            .collect();
        let too_many = serde_json::json!({ "messages": [{"role": "user", "content": blocks}] });
        assert!(matches!(
            validate_markers(&too_many, &all_tiers()).unwrap_err(),
            ParseError::TooManyBreakpoints
        ));
    }

    #[test]
    fn non_object_cache_control_is_malformed() {
        // A bare string (or any non-object) must NOT slip through as the default tier.
        let body = serde_json::json!({
            "messages": [{"role": "system", "content": [
                {"type": "text", "text": "x", "cache_control": "persistent"}
            ]}]
        });
        assert!(matches!(
            validate_markers(&body, &all_tiers()).unwrap_err(),
            ParseError::MalformedCacheControl
        ));
        assert!(matches!(
            parse_chat_completions(body.to_string().as_bytes(), &all_tiers(), &no_telemetry()).unwrap_err(),
            ParseError::MalformedCacheControl
        ));
    }

    #[test]
    fn non_string_type_or_ttl_is_malformed() {
        // A present-but-non-string `type`/`ttl` must not be treated as absent (which would
        // silently default e.g. `ttl: 123` into the default tier).
        let bad_ttl = serde_json::json!({
            "messages": [{"role": "system", "content": [
                {"type": "text", "text": "x", "cache_control": {"type": "ephemeral", "ttl": 123}}
            ]}]
        });
        assert!(matches!(
            validate_markers(&bad_ttl, &all_tiers()).unwrap_err(),
            ParseError::MalformedCacheControl
        ));

        let bad_type = serde_json::json!({
            "messages": [{"role": "system", "content": [
                {"type": "text", "text": "x", "cache_control": {"type": true}}
            ]}]
        });
        assert!(matches!(
            validate_markers(&bad_type, &all_tiers()).unwrap_err(),
            ParseError::MalformedCacheControl
        ));
    }

    #[test]
    fn missing_type_is_malformed() {
        // `type` is required (Anthropic mandates it, even though "ephemeral" is the only value).
        let no_type = serde_json::json!({
            "messages": [{"role": "system", "content": [
                {"type": "text", "text": "x", "cache_control": {"ttl": "1h"}}
            ]}]
        });
        assert!(matches!(
            validate_markers(&no_type, &all_tiers()).unwrap_err(),
            ParseError::MalformedCacheControl
        ));
        // An empty cache_control object (no type) is malformed too.
        let empty = serde_json::json!({
            "messages": [{"role": "system", "content": [
                {"type": "text", "text": "x", "cache_control": {}}
            ]}]
        });
        assert!(matches!(
            parse_chat_completions(empty.to_string().as_bytes(), &all_tiers(), &no_telemetry()).unwrap_err(),
            ParseError::MalformedCacheControl
        ));
    }

    #[test]
    fn null_cache_control_is_no_marker() {
        // An explicit `null` means "no marker": no breakpoint, no error.
        let body = serde_json::json!({
            "messages": [{"role": "system", "content": [
                {"type": "text", "text": "x", "cache_control": null}
            ]}]
        });
        assert!(validate_markers(&body, &all_tiers()).is_ok());
        let p = parse_chat_completions(body.to_string().as_bytes(), &all_tiers(), &no_telemetry()).unwrap();
        assert!(p.breakpoints.is_empty());
    }

    #[test]
    fn cache_control_excluded_from_hash() {
        // Same content, one with a marker and one without — the cumulative hash at
        // that block must be identical (the marker is stripped before hashing).
        let marked = parse(serde_json::json!({
            "messages": [{"role": "system", "content": [
                {"type": "text", "text": "shared prefix", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
            ]}]
        }));
        let unmarked = parse(serde_json::json!({
            "messages": [{"role": "system", "content": [
                {"type": "text", "text": "shared prefix"}
            ]}]
        }));
        assert_eq!(marked.cumulative_hashes[0], unmarked.cumulative_hashes[0]);
        // ...and different content differs.
        let other = parse(serde_json::json!({
            "messages": [{"role": "system", "content": [{"type": "text", "text": "different"}]}]
        }));
        assert_ne!(marked.cumulative_hashes[0], other.cumulative_hashes[0]);
    }

    #[test]
    fn walk_back_candidates_longest_first_bounded() {
        // 25 blocks, breakpoint on the last → candidates are the 20 most recent
        // boundary hashes, longest (the breakpoint) first.
        let blocks: Vec<_> = (0..25)
            .map(|i| {
                let mut b = serde_json::json!({"type": "text", "text": format!("b{i}")});
                if i == 24 {
                    b["cache_control"] = serde_json::json!({"type": "ephemeral"});
                }
                b
            })
            .collect();
        let p = parse(serde_json::json!({ "messages": [{"role": "user", "content": blocks}] }));
        let cands = p.read_candidates(&p.breakpoints[0]);
        assert_eq!(cands.len(), WALK_BACK);
        assert_eq!(cands[0], p.cumulative_hashes[24]); // breakpoint itself, longest
        assert_eq!(cands[1], p.cumulative_hashes[23]);
        assert_eq!(cands[WALK_BACK - 1], p.cumulative_hashes[5]); // 24 - 19
    }

    #[test]
    fn deterministic() {
        let body = serde_json::json!({
            "messages": [{"role": "system", "content": [{"type": "text", "text": "abc"}]}]
        });
        assert_eq!(parse(body.clone()).cumulative_hashes, parse(body).cumulative_hashes);
    }

    #[test]
    fn tool_definition_marker_creates_breakpoint() {
        let p = parse(serde_json::json!({
            "tools": [
                {"type": "function", "function": {"name": "lookup", "parameters": {}},
                 "cache_control": {"type": "ephemeral", "ttl": "1h"}}
            ],
            "messages": [{"role": "user", "content": "q"}]
        }));
        // The tool is block 0 (hashed before the message), and carries a breakpoint.
        assert_eq!(p.blocks.len(), 2);
        assert_eq!(p.blocks[0].role, "tool_definition");
        assert_eq!(p.blocks[1].role, "user");
        assert_eq!(p.breakpoints.len(), 1);
        assert_eq!(p.breakpoints[0].block_index, 0);
        assert_eq!(p.breakpoints[0].ttl_tier, TtlTier::OneHour);
        // The tool's JSON is its write-side text (tokenized as the estimate).
        assert!(p.blocks[0].text.contains("lookup"));
    }

    #[test]
    fn tools_hashed_before_messages() {
        let with_tool = parse(serde_json::json!({
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {}}}],
            "messages": [{"role": "system", "content": "sys"}, {"role": "user", "content": "q"}]
        }));
        let without = parse(serde_json::json!({
            "messages": [{"role": "system", "content": "sys"}, {"role": "user", "content": "q"}]
        }));
        // tool + 2 messages = 3 blocks, tool first (tools → system → messages).
        assert_eq!(with_tool.blocks.len(), 3);
        assert_eq!(with_tool.blocks[0].role, "tool_definition");
        assert_eq!(with_tool.blocks[1].role, "system");
        // Adding a tool changes the system-block prefix hash — tools participate in the prefix.
        assert_ne!(with_tool.cumulative_hashes[1], without.cumulative_hashes[0]);
    }

    #[test]
    fn tool_marker_excluded_from_hash() {
        // Same tool, one marked and one not — the marker is stripped before hashing, so a
        // marked write and an unmarked follow-up (or vice-versa) still match.
        let marked = parse(serde_json::json!({
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {}},
                       "cache_control": {"type": "ephemeral", "ttl": "1h"}}]
        }));
        let unmarked = parse(serde_json::json!({
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {}}}]
        }));
        assert_eq!(marked.cumulative_hashes[0], unmarked.cumulative_hashes[0]);
    }

    #[test]
    fn tool_markers_count_toward_breakpoint_cap() {
        // 3 tool markers + 2 content markers = 5 > MAX_BREAKPOINTS, on both paths.
        let tools: Vec<_> = (0..3)
            .map(|i| {
                serde_json::json!({
                "type": "function", "function": {"name": format!("f{i}"), "parameters": {}},
                "cache_control": {"type": "ephemeral"}})
            })
            .collect();
        let body = serde_json::json!({
            "tools": tools,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "b", "cache_control": {"type": "ephemeral"}}
            ]}]
        });
        assert!(matches!(
            validate_markers(&body, &all_tiers()).unwrap_err(),
            ParseError::TooManyBreakpoints
        ));
        assert!(matches!(
            parse_chat_completions(body.to_string().as_bytes(), &all_tiers(), &no_telemetry()).unwrap_err(),
            ParseError::TooManyBreakpoints
        ));
    }

    #[test]
    fn validate_markers_accepts_tool_marker() {
        let body = serde_json::json!({
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {}},
                       "cache_control": {"type": "ephemeral", "ttl": "1h"}}],
            "messages": [{"role": "user", "content": "q"}]
        });
        assert!(validate_markers(&body, &all_tiers()).is_ok());
    }

    #[test]
    fn provider_telemetry_block_excluded_from_cache_view() {
        // A telemetry-only block (no marker) is invisible to the cache: the stable prefix hashes
        // identically whether or not it's present, and it doesn't occupy a block slot.
        let tele = telemetry();
        let with = parse_with(
            serde_json::json!({
                "messages": [{"role": "system", "content": [
                    {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1; cch=abc123;"},
                    {"type": "text", "text": "stable system prompt"}
                ]}]
            }),
            &tele,
        );
        let without = parse_with(
            serde_json::json!({
                "messages": [{"role": "system", "content": [
                    {"type": "text", "text": "stable system prompt"}
                ]}]
            }),
            &tele,
        );
        assert_eq!(with.blocks.len(), 1, "telemetry block is skipped");
        assert_eq!(with.blocks[0].text, "stable system prompt");
        assert_eq!(with.cumulative_hashes[0], without.cumulative_hashes[0]);
    }

    #[test]
    fn telemetry_kept_when_feature_disabled() {
        // Empty prefixes (default `TelemetryPolicy`) = feature off: the block is normal content.
        let p = parse(serde_json::json!({
            "messages": [{"role": "system", "content": [
                {"type": "text", "text": "x-anthropic-billing-header: cch=abc;"},
                {"type": "text", "text": "stable"}
            ]}]
        }));
        assert_eq!(p.blocks.len(), 2, "feature off → telemetry block not excluded");
    }

    #[test]
    fn telemetry_only_excluded_from_system_role() {
        // The prefix in a non-system (user) block must NOT be excluded — telemetry lives in system,
        // so coincidental user/assistant content is never silently dropped from the cache view.
        let p = parse_with(
            serde_json::json!({
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "x-anthropic-billing-header: cch=abc;"},
                    {"type": "text", "text": "actual question"}
                ]}]
            }),
            &telemetry(),
        );
        assert_eq!(p.blocks.len(), 2, "user-role block starting with the prefix is not excluded");
    }

    #[test]
    fn empty_prefix_is_dropped_not_match_all() {
        // A stray empty-string prefix must NOT turn into "match every block" (`"".starts_with`).
        let tele = TelemetryPolicy::from_config(true, &["".to_string()]);
        let p = parse_with(
            serde_json::json!({
                "messages": [{"role": "system", "content": [{"type": "text", "text": "anything at all"}]}]
            }),
            &tele,
        );
        assert_eq!(p.blocks.len(), 1, "empty prefix must not exclude blocks");
    }

    #[test]
    fn telemetry_nonce_does_not_change_prefix_hash() {
        // The Claude Code SDK write-only bug: the leading telemetry block's `cch` nonce changes
        // every turn. Excluded, a marked stable prefix hashes the SAME across nonces → reads hit.
        let mk = |nonce: &str| {
            serde_json::json!({
                "messages": [
                    {"role": "system", "content": [
                        {"type": "text", "text": format!("x-anthropic-billing-header: cc_entrypoint=sdk-py; cch={nonce};")},
                        {"type": "text", "text": "long stable system prompt", "cache_control": {"type": "ephemeral", "ttl": "5m"}}
                    ]},
                    {"role": "user", "content": "hello"}
                ]
            })
        };
        let a = parse_with(mk("47d6f"), &telemetry());
        let b = parse_with(mk("b1b38"), &telemetry());
        // Blocks: [system(stable), user] — telemetry excluded; breakpoint on the stable block.
        assert_eq!(a.blocks.len(), 2);
        assert_eq!(a.breakpoints.len(), 1);
        assert_eq!(a.breakpoints[0].block_index, 0);
        // Different nonce, identical cached-prefix hash — the fix.
        assert_eq!(a.cumulative_hashes[0], b.cumulative_hashes[0]);
    }

    #[test]
    fn marked_block_is_not_treated_as_telemetry() {
        // Defensive: a block starting with the telemetry prefix but carrying a real marker is NOT
        // dropped — we never silently discard a caller's breakpoint.
        let p = parse_with(
            serde_json::json!({
                "messages": [{"role": "system", "content": [
                    {"type": "text", "text": "x-anthropic-billing-header: keep me", "cache_control": {"type": "ephemeral"}}
                ]}]
            }),
            &telemetry(),
        );
        assert_eq!(p.blocks.len(), 1);
        assert_eq!(p.breakpoints.len(), 1);
    }

    #[test]
    fn disabled_tier_on_tool_marker_rejected() {
        // A 24h marker on a tool is rejected identically to one on a content block.
        let policy = TierPolicy::from_config(&["5m".to_string(), "1h".to_string()], "5m");
        let body = serde_json::json!({
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {}},
                       "cache_control": {"type": "ephemeral", "ttl": "24h"}}]
        });
        assert!(matches!(
            validate_markers(&body, &policy).unwrap_err(),
            ParseError::DisabledTier(TtlTier::TwentyFourHours)
        ));
    }
}
