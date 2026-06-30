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
//! v1 scope: text content blocks on chat-completions messages. Tools-level markers
//! and image-token caching are deferred — image blocks still contribute to the
//! prefix hash but carry no text, so their tokens fall into the uncached tail.

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

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("request body is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("too many cache_control breakpoints: {found} (max {MAX_BREAKPOINTS})")]
    TooManyBreakpoints { found: usize },
    #[error("invalid cache_control ttl: {0:?}")]
    InvalidTtl(String),
    #[error("unsupported cache_control type: {0:?} (only \"ephemeral\")")]
    UnsupportedType(String),
    #[error("cache_control ttl tier '{}' is not currently available", .0.as_str())]
    DisabledTier(TtlTier),
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

/// Parse a chat-completions body into its cache primitives. Errors are surfaced for
/// the caller to treat as "no cache" (safe) — never breaking the customer request.
pub fn parse_chat_completions(body: &[u8], policy: &TierPolicy) -> Result<ParsedPrompt, ParseError> {
    let v: serde_json::Value = serde_json::from_slice(body)?;

    let mut blocks: Vec<Block> = Vec::new();
    let mut breakpoints: Vec<Breakpoint> = Vec::new();
    let mut cumulative_hashes: Vec<Vec<u8>> = Vec::new();
    let mut hasher = Sha256::new();

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
                        let ttl = match block.get("cache_control") {
                            Some(cc) => Some(parse_ttl(cc, policy)?),
                            None => None,
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
        return Err(ParseError::TooManyBreakpoints { found: breakpoints.len() });
    }

    Ok(ParsedPrompt {
        blocks,
        cumulative_hashes,
        breakpoints,
    })
}

/// `cache_control: { type: "ephemeral", ttl: "5m"|"1h"|"24h" }`. A missing `ttl` defaults to
/// `policy.default_ttl` (Anthropic-style; configurable). Errors:
/// - non-`ephemeral` `type` → [`ParseError::UnsupportedType`]
/// - an unknown `ttl` string → [`ParseError::InvalidTtl`]
/// - a valid tier the platform has disabled (not in `enabled_ttls`) → [`ParseError::DisabledTier`]
fn parse_ttl(cache_control: &serde_json::Value, policy: &TierPolicy) -> Result<TtlTier, ParseError> {
    if let Some(t) = cache_control.get("type").and_then(|t| t.as_str())
        && t != "ephemeral"
    {
        return Err(ParseError::UnsupportedType(t.to_string()));
    }
    let tier = match cache_control.get("ttl").and_then(|t| t.as_str()) {
        Some(ttl) => TtlTier::parse(ttl).ok_or_else(|| ParseError::InvalidTtl(ttl.to_string()))?,
        None => policy.default_ttl(),
    };
    if !policy.is_enabled(tier) {
        return Err(ParseError::DisabledTier(tier));
    }
    Ok(tier)
}

/// Synchronous request-path validation of `cache_control` markers — WITHOUT the hashing the
/// full [`parse_chat_completions`] does, so it's cheap enough to run before forwarding. Every
/// marker must be ephemeral, name an *enabled* tier (or default to one), and there must be
/// `≤ MAX_BREAKPOINTS`. A failure is turned into a 400 by the layer: the request is rejected
/// (like a bad parameter) rather than silently un-cached, so the client learns immediately and
/// isn't billed full price thinking it cached. Takes the body `Value` the layer already parses
/// to extract `model` — no extra deserialization.
pub fn validate_markers(body: &serde_json::Value, policy: &TierPolicy) -> Result<(), ParseError> {
    let mut breakpoints = 0usize;
    if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
        for msg in messages {
            if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
                for block in arr {
                    if let Some(cc) = block.get("cache_control") {
                        parse_ttl(cc, policy)?;
                        breakpoints += 1;
                    }
                }
            }
        }
    }
    if breakpoints > MAX_BREAKPOINTS {
        return Err(ParseError::TooManyBreakpoints { found: breakpoints });
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

    fn parse(body: serde_json::Value) -> ParsedPrompt {
        parse_chat_completions(body.to_string().as_bytes(), &all_tiers()).unwrap()
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
        )
        .unwrap_err();
        assert!(matches!(err, ParseError::TooManyBreakpoints { found: 5 }));
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
        let err2 = parse_chat_completions(body.to_string().as_bytes(), &policy).unwrap_err();
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
        let p = parse_chat_completions(body.to_string().as_bytes(), &policy).unwrap();
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
            ParseError::TooManyBreakpoints { found: 5 }
        ));
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
}
