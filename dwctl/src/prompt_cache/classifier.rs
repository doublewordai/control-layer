//! The classify orchestration: turn a request into a neutral
//! [`CacheStats`] split plus the [`PendingWrite`] to commit on success.
//!
//! Flow: resolve principal → per-model gate → parse markers → find the longest cached
//! prefix (read, via the 20-block walk-back) → tokenize the new suffix (write) →
//! enforce the min-prefix floor → assemble. Reads need no tokenization (the count is
//! stored on the entry); only the new write span is tokenized, and it runs in parallel
//! with generation. Any recoverable failure (tokenizer down, model unmapped, parse
//! error, no principal) degrades to all-zero "no caching" — never an error to the
//! customer (best-effort; a reconciliation pass backstops residual overcharges).
//!
//! v1 scope: chat-completions message bodies. Tool-using multi-step Responses are a
//! fast-follow; image tokens fall into the uncached tail.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;

use super::index::{CacheEntry, CacheIndex, CacheResult, IndexScope, PrefixHash, TierPolicy};
use super::metrics as cache_metrics;
use super::model_config::ModelConfigResolver;
use super::parse::{ParseError, TelemetryPolicy, parse_chat_completions};
use super::principal::PrincipalResolver;
use super::stats::{CacheStats, PendingWrite};
use super::tokenizer::{TokenizerClient, TokenizerError};

/// What `classify` needs from the request.
pub struct ClassifyRequest<'a> {
    /// The virtual model (the `deployed_models.alias` = the cache key dimension).
    pub virtual_model: &'a str,
    /// The raw request body (with `cache_control` markers intact).
    pub body: &'a [u8],
    /// The validated bearer token, or `None` (un-scopable → no caching).
    pub api_key: Option<&'a str>,
}

/// Version pair from tokenizer-svc `/v1/models`: the tokenizer hash the index has always
/// keyed on, plus (render-capable aliases only) the chat-template hash.
#[derive(Debug, Clone)]
pub struct TokenizerVersions {
    pub tokenizer: String,
    pub template: Option<String>,
}

/// Outcome of the exact-counting attempt (see [`Classifier::render_counts`]).
enum RenderCountsOutcome {
    /// Cumulative counts aligned with `parsed.breakpoints` (`None` = unpriceable), plus
    /// the full-render total (the engine's prompt view, for the drift alarm).
    Counts { per_breakpoint: Vec<Option<u64>>, total: u64 },
    /// Exact counting unavailable for this request — use raw-segment counting.
    Fallback,
    /// Counting failed outright — no caching for this request.
    Skip,
}

/// The result of [`Classifier::classify`].
///
/// `active` is true once the per-model gate passes (the model is cache-enabled),
/// independent of whether this particular prompt cached anything. It drives the
/// uniform-zeros injection: an enabled model always gets the `cache_*` usage
/// fields on its response — zeroed when nothing cached — so the cohort has one
/// response shape; a disabled model's response is left untouched. `stats`/`pending`
/// carry the actual read/write split (both zero when `active` but nothing cached).
pub struct ClassifyOutcome {
    pub stats: CacheStats,
    pub pending: PendingWrite,
    pub active: bool,
}

impl ClassifyOutcome {
    /// Model not cache-enabled (or unscopable) — leave the response untouched.
    pub(crate) fn inactive() -> Self {
        Self {
            stats: CacheStats::default(),
            pending: PendingWrite::default(),
            active: false,
        }
    }

    /// Enabled, but this prompt cached nothing (no markers, below floor, tokenizer
    /// degraded, …) — inject uniform zeros, commit nothing.
    fn zero_active() -> Self {
        Self {
            stats: CacheStats::default(),
            pending: PendingWrite::default(),
            active: true,
        }
    }

    /// Enabled with a real read/write split.
    fn active(stats: CacheStats, pending: PendingWrite) -> Self {
        Self {
            stats,
            pending,
            active: true,
        }
    }
}

/// Owns the classify engine's dependencies. Cheap to clone (everything inside is
/// `Arc`/pool/cache-backed).
#[derive(Clone)]
pub struct Classifier {
    principal: PrincipalResolver,
    model_config: ModelConfigResolver,
    tokenizer: TokenizerClient,
    index: Arc<dyn CacheIndex>,
    /// alias → versions (from tokenizer-svc `/v1/models`); `None` = unmapped. The
    /// template version is present only for render-capable aliases (svc ≥ 0.3.0).
    versions: moka::future::Cache<String, Option<TokenizerVersions>>,
    /// Exact chat-templated counting via `/v1/render` (config `cache.render_counting`).
    /// Per-alias it additionally requires a template_version from `/v1/models`; aliases
    /// without one keep today's raw-segment counting.
    render_counting: bool,
    /// Enabled TTL tiers + the default-ttl tier. Shared with the layer's request-path marker
    /// validation, so a no-ttl marker resolves to the same tier here as it was validated against.
    tier_policy: TierPolicy,
    /// Provider-telemetry-block handling. Used here to exclude telemetry from the cache prefix,
    /// and exposed to the layer's outbound sanitiser to (in strip mode) drop it from the forward.
    telemetry: TelemetryPolicy,
}

impl Classifier {
    pub fn new(
        principal: PrincipalResolver,
        model_config: ModelConfigResolver,
        tokenizer: TokenizerClient,
        index: Arc<dyn CacheIndex>,
        tier_policy: TierPolicy,
        telemetry: TelemetryPolicy,
        render_counting: bool,
    ) -> Self {
        let versions = moka::future::Cache::builder()
            .max_capacity(10_000)
            .time_to_live(std::time::Duration::from_secs(300))
            .build();
        Self {
            principal,
            model_config,
            tokenizer,
            index,
            versions,
            render_counting,
            tier_policy,
            telemetry,
        }
    }

    /// The configured TTL-tier policy (enabled tiers + default ttl), exposed for the cache
    /// layer's synchronous request-path marker validation.
    pub fn tier_policy(&self) -> &TierPolicy {
        &self.tier_policy
    }

    /// The provider-telemetry policy, exposed for the layer's outbound sanitiser.
    pub fn telemetry_policy(&self) -> &TelemetryPolicy {
        &self.telemetry
    }

    /// Classify a request into its read/write split + the entries to commit on success.
    ///
    /// Pre-`cfg.enabled` bails are `inactive` (unscopable / disabled model → response
    /// untouched). Once the model is enabled, every bail is `zero_active` (uniform
    /// zeros injected, nothing committed) — so enabled models present one shape.
    pub async fn classify(&self, req: ClassifyRequest<'_>) -> CacheResult<ClassifyOutcome> {
        let start = std::time::Instant::now();
        let out = self.classify_inner(req).await;
        cache_metrics::record_classify_duration(start.elapsed().as_secs_f64());
        out
    }

    async fn classify_inner(&self, req: ClassifyRequest<'_>) -> CacheResult<ClassifyOutcome> {
        // Gates that fire *before* we know the model is cache-enabled → inactive.
        let Some(api_key) = req.api_key else {
            return Ok(ClassifyOutcome::inactive());
        };
        let Some(principal_id) = self.principal.resolve(api_key).await? else {
            return Ok(ClassifyOutcome::inactive());
        };
        let cfg = self.model_config.resolve(req.virtual_model).await?;
        if !cfg.enabled {
            return Ok(ClassifyOutcome::inactive());
        }

        // From here the model is cache-enabled: any bail is `zero_active`.
        // (The layer already 400-rejected disabled/malformed markers synchronously before the
        // fork, so on the live path these errors won't fire — this stays a defensive fallback.)
        let parsed = match parse_chat_completions(req.body, &self.tier_policy, &self.telemetry) {
            Ok(p) => p,
            Err(e) => {
                // Marker validation rejections (anti-abuse) vs a genuine unparseable body.
                match &e {
                    ParseError::TooManyBreakpoints => cache_metrics::record_markers_rejected("too_many_breakpoints"),
                    ParseError::InvalidTtl(_) => cache_metrics::record_markers_rejected("invalid_ttl"),
                    ParseError::UnsupportedType(_) => cache_metrics::record_markers_rejected("unsupported_type"),
                    ParseError::DisabledTier(_) => cache_metrics::record_markers_rejected("tier_disabled"),
                    ParseError::MalformedCacheControl => cache_metrics::record_markers_rejected("malformed_cache_control"),
                    ParseError::AutomaticTtlConflict => cache_metrics::record_markers_rejected("automatic_ttl_conflict"),
                    ParseError::NoAutomaticSlot => cache_metrics::record_markers_rejected("automatic_no_slot"),
                    ParseError::Json(_) => cache_metrics::record_skip("unparseable"),
                }
                // Best-effort: degrade to no caching (uniform zeros). Log at debug so the
                // silent degradation is diagnosable without warn-level noise on every odd body.
                tracing::debug!(error = %e, virtual_model = req.virtual_model, "cache classify: body not cacheable (invalid cache_control markers / unparseable JSON)");
                return Ok(ClassifyOutcome::zero_active());
            }
        };
        if parsed.breakpoints.is_empty() {
            cache_metrics::record_skip("no_markers");
            return Ok(ClassifyOutcome::zero_active()); // markers are required to cache
        }
        let Some(versions) = self.tokenizer_versions(req.virtual_model).await? else {
            cache_metrics::record_skip("tokenizer_unmapped");
            return Ok(ClassifyOutcome::zero_active()); // model not mapped in tokenizer-svc
        };
        let render_mode = self.render_counting && versions.template.is_some();
        // Under exact counting the stored entry counts mean something different (templated
        // vs raw-content tokens), and change again if the template changes — fold the
        // template version into the scope so mismatched-era entries age out instead of
        // mispricing reads (expect a one-time miss churn on rollout, like the tools[] one).
        let tokenizer_version = match (&render_mode, &versions.template) {
            (true, Some(template)) => format!("{}+{}", versions.tokenizer, template),
            _ => versions.tokenizer.clone(),
        };
        let scope = IndexScope {
            principal_id,
            virtual_model: req.virtual_model.to_string(),
            tokenizer_version,
        };

        // Longest cached prefix across all breakpoints' walk-back windows.
        let read = self.find_longest_read(&scope, &parsed).await?;
        let read_block = read.as_ref().map(|r| r.block); // index into parsed.blocks
        let read_tokens = read.as_ref().map(|r| r.tokens).unwrap_or(0);

        // Parse guarantees ≥1 breakpoint here (the is-empty check above bails early), but
        // degrade to no-caching rather than panic if that invariant ever drifts in a refactor.
        let Some(deepest_bp) = parsed.breakpoints.last() else {
            return Ok(ClassifyOutcome::zero_active());
        };
        let deepest = deepest_bp.block_index;

        let mut stats = CacheStats {
            read: read_tokens as u64,
            ..Default::default()
        };
        let mut pending = PendingWrite::default();

        // Refresh the matched read entry's TTL (sliding window).
        if let Some(r) = &read {
            pending.refresh = Some((scope.clone(), r.hash.clone(), Utc::now() + r.duration));
        }

        // Pure read: the deepest declared prefix is already cached → no write.
        if read_block == Some(deepest) {
            // Floor is a write-time gate; a live read entry was already above it.
            return Ok(ClassifyOutcome::active(stats, pending));
        }

        // Per-breakpoint cumulative counts, by mode. `None` at a breakpoint = unpriceable
        // under exact counting (no create at it; its span merges into the next priceable
        // one). Exact counting can fall back to raw counting per the error ladder.
        let counts = if render_mode {
            match self.render_counts(&req, &parsed, read_block).await? {
                RenderCountsOutcome::Counts { per_breakpoint, total } => {
                    stats.render_total = (total > 0).then_some(total);
                    Some(per_breakpoint)
                }
                RenderCountsOutcome::Fallback => None, // raw counting below
                RenderCountsOutcome::Skip => return Ok(ClassifyOutcome::zero_active()),
            }
        } else {
            None
        };
        let per_bp: Vec<Option<u64>> = match counts {
            Some(c) => c,
            None => match self.segment_counts(&req, &parsed, read_block, read_tokens, deepest).await? {
                Some(c) => c,
                None => return Ok(ClassifyOutcome::zero_active()),
            },
        };

        // Floor: the deepest priceable declared prefix must clear the per-model minimum.
        let Some(total_prefix) = per_bp.iter().rev().find_map(|c| *c) else {
            // No breakpoint is priceable (e.g. all unpriceable under exact counting) —
            // nothing to write; treat as no caching for this request.
            cache_metrics::record_skip("render_failed");
            return Ok(ClassifyOutcome::zero_active());
        };
        if total_prefix < cfg.min_prefix_tokens as u64 {
            cache_metrics::record_skip("below_floor");
            return Ok(ClassifyOutcome::zero_active()); // below the per-model floor → no caching
        }

        // Each priceable breakpoint beyond the read is its own cached prefix; the segment
        // it closes is creation under its tier. (`block_index > read_block`, treating a
        // no-read as -1, selects exactly the breakpoints within the write span.)
        let mut prev_boundary = read_tokens as u64;
        let now = Utc::now();
        let read_block_idx: isize = read_block.map(|b| b as isize).unwrap_or(-1);
        for (bp, bp_cumulative) in parsed
            .breakpoints
            .iter()
            .zip(per_bp.iter())
            .filter(|(bp, _)| bp.block_index as isize > read_block_idx)
            .filter_map(|(bp, c)| c.map(|c| (bp, c)))
        {
            let segment_tokens = bp_cumulative.saturating_sub(prev_boundary);
            stats.add_creation(bp.ttl_tier, segment_tokens);
            pending.writes.push(CacheEntry {
                scope: scope.clone(),
                prefix_hash: parsed.cumulative_hashes[bp.block_index].clone(),
                // Cap at u32::MAX — a prefix exceeding ~4.3B tokens is beyond any model's
                // context window; if that ever becomes realistic the column needs BIGINT.
                cumulative_token_count: bp_cumulative.min(u32::MAX as u64) as u32,
                ttl_tier: bp.ttl_tier,
                expires_at: now + bp.ttl_tier.duration(),
            });
            prev_boundary = bp_cumulative;
        }

        Ok(ClassifyOutcome::active(stats, pending))
    }

    /// Raw-segment counting (`/v1/tokenize`): today's path, and the fallback under exact
    /// counting. Returns cumulative counts aligned with `parsed.breakpoints` (always
    /// `Some` per breakpoint), or `None` when counting failed (degrade to no caching).
    async fn segment_counts(
        &self,
        req: &ClassifyRequest<'_>,
        parsed: &crate::prompt_cache::parse::ParsedPrompt,
        read_block: Option<usize>,
        read_tokens: u32,
        deepest: usize,
    ) -> CacheResult<Option<Vec<Option<u64>>>> {
        // Write span: blocks after the matched read, up to the deepest breakpoint.
        let write_start = read_block.map(|b| b + 1).unwrap_or(0);
        let segments: Vec<String> = parsed.blocks[write_start..=deepest].iter().map(|b| b.text.clone()).collect();

        // Tokenize the suffix (the only tokenization; reads needed none). Failure →
        // degrade to no caching (safe under the best-effort contract).
        let tok = match self.tokenizer.tokenize(req.virtual_model, &segments).await {
            Ok(tok) => tok,
            Err(e) => {
                cache_metrics::record_skip("tokenize_failed");
                tracing::debug!(error = %e, virtual_model = req.virtual_model, "cache classify: tokenize failed, degrading to no write");
                return Ok(None);
            }
        };
        if tok.cumulative.len() != segments.len() {
            // The tokenizer returned a different number of cumulative counts than segments we
            // sent — we can't map tokens to blocks safely, so bail (no write) rather than guess.
            cache_metrics::record_skip("count_mismatch");
            tracing::debug!(
                segments = segments.len(),
                cumulative = tok.cumulative.len(),
                virtual_model = req.virtual_model,
                "cache classify: tokenizer segment-count mismatch, degrading to no write"
            );
            return Ok(None);
        }
        let cumulative_at = |block: usize| -> u64 { read_tokens as u64 + tok.cumulative[block - write_start] as u64 };
        Ok(Some(
            parsed
                .breakpoints
                .iter()
                .map(|bp| {
                    if bp.block_index >= write_start {
                        Some(cumulative_at(bp.block_index))
                    } else {
                        // At or before the matched read: not in the write span; the read
                        // token count already prices it.
                        Some(read_tokens as u64)
                    }
                })
                .collect(),
        ))
    }

    /// Exact chat-templated counting (`/v1/render`): one render per breakpoint, each a
    /// RECONSTRUCTED prefix (its tools subset + full messages + any partial message),
    /// generation prompt OFF — precisely what the marker covers, for every marker
    /// position we price today (tool definitions and mid-message markers included).
    /// Plus one render of the FULL body (generation prompt on) for the drift total.
    ///
    /// Per-breakpoint failures fall back to that breakpoint's raw-segment count, so no
    /// marker position can regress below today's accuracy; whole-request failures follow
    /// the ladder (unsupported → raw counting; transport → no caching).
    async fn render_counts(
        &self,
        req: &ClassifyRequest<'_>,
        parsed: &crate::prompt_cache::parse::ParsedPrompt,
        read_block: Option<usize>,
    ) -> CacheResult<RenderCountsOutcome> {
        // Slice the marker-stripped body. Telemetry blocks stay in place here so the
        // PrefixSpec ordinals (recorded against the original arrays) line up; the drift
        // render below uses the fully-stripped body (the engine's exact bytes). The
        // difference is a telemetry line or two — acceptable approximation.
        let keep_telemetry = super::parse::TelemetryPolicy::from_config(false, &[] as &[String]);
        let stripped = super::inject::strip_cache_control(req.body, &keep_telemetry).0;
        let body: &[u8] = stripped.as_deref().unwrap_or(req.body);
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
            metrics::counter!("dwctl_cache_render_fallback_total", "reason" => "unparseable").increment(1);
            return Ok(RenderCountsOutcome::Fallback);
        };
        let empty = Vec::new();
        let messages = v.get("messages").and_then(|m| m.as_array()).unwrap_or(&empty);
        let tools = v.get("tools").and_then(|t| t.as_array());

        let read_block_idx: isize = read_block.map(|b| b as isize).unwrap_or(-1);
        let mut per_breakpoint: Vec<Option<u64>> = Vec::with_capacity(parsed.breakpoints.len());
        for bp in &parsed.breakpoints {
            // Breakpoints at or before the matched read need no count: the read entry's
            // stored count already prices that prefix, and the write loop skips them.
            if bp.block_index as isize <= read_block_idx {
                per_breakpoint.push(None);
                continue;
            }
            let spec = &bp.prefix;
            // Reconstruct the prefix request.
            let prefix_tools: Option<serde_json::Value> = tools.and_then(|t| {
                let kept = spec.tools_kept.min(t.len());
                (kept > 0).then(|| serde_json::Value::Array(t[..kept].to_vec()))
            });
            let mut prefix_messages: Vec<serde_json::Value> = messages.iter().take(spec.full_messages).cloned().collect();
            if let Some(partial) = &spec.partial
                && let Some(msg) = messages.get(spec.full_messages)
            {
                let mut truncated = msg.clone();
                if let Some(obj) = truncated.as_object_mut() {
                    if let Some(serde_json::Value::Array(parts)) = obj.get_mut("content") {
                        parts.truncate(partial.content_parts);
                    }
                    if partial.tool_calls == 0 {
                        obj.remove("tool_calls");
                    } else if let Some(serde_json::Value::Array(calls)) = obj.get_mut("tool_calls") {
                        calls.truncate(partial.tool_calls);
                    }
                }
                prefix_messages.push(truncated);
            }

            let count = match self
                .tokenizer
                .render(
                    req.virtual_model,
                    &serde_json::Value::Array(prefix_messages),
                    prefix_tools.as_ref(),
                    false, // a prefix is not a generation view
                )
                .await
            {
                Ok(r) => Some(u64::from(r.total)),
                Err(TokenizerError::Unmapped(_)) => {
                    cache_metrics::record_skip("tokenizer_unmapped");
                    return Ok(RenderCountsOutcome::Skip);
                }
                Err(TokenizerError::RenderUnsupported(..)) => {
                    // e.g. a tools-only prefix the template can't express, or a shape it
                    // rejects: this BREAKPOINT falls back to its raw-segment count below.
                    metrics::counter!("dwctl_cache_render_fallback_total", "reason" => "prefix_unsupported").increment(1);
                    None
                }
                Err(e) => {
                    // Transport/5xx: degrade exactly as tokenize failures do — no caching.
                    cache_metrics::record_skip("render_failed");
                    tracing::debug!(error = %e, virtual_model = req.virtual_model, "cache classify: render failed, degrading to no write");
                    return Ok(RenderCountsOutcome::Skip);
                }
            };
            per_breakpoint.push(count);
        }

        // Raw-segment backfill for any breakpoint the template couldn't price: one
        // tokenize of all blocks gives cumulative raw counts (today's accuracy).
        if per_breakpoint.iter().any(Option::is_none) {
            let segments: Vec<String> = parsed.blocks.iter().map(|b| b.text.clone()).collect();
            match self.tokenizer.tokenize(req.virtual_model, &segments).await {
                Ok(tok) if tok.cumulative.len() == segments.len() => {
                    for (slot, bp) in per_breakpoint.iter_mut().zip(parsed.breakpoints.iter()) {
                        if slot.is_none() {
                            *slot = Some(u64::from(tok.cumulative[bp.block_index]));
                        }
                    }
                }
                _ => {
                    // Backfill unavailable: those breakpoints stay unpriced this request
                    // (their span merges into the next priced one).
                }
            }
        }

        // The FULL body (engine bytes: telemetry-stripped, generation prompt on) for the
        // drift measurement against the engine-reported prompt_tokens.
        let full_stripped = super::inject::strip_cache_control(req.body, &self.telemetry).0;
        let full_body: &[u8] = full_stripped.as_deref().unwrap_or(req.body);
        let total = match serde_json::from_slice::<serde_json::Value>(full_body) {
            Ok(fv) => {
                let msgs = fv.get("messages").cloned().unwrap_or(serde_json::Value::Array(vec![]));
                let tls = fv.get("tools").cloned();
                match self.tokenizer.render(req.virtual_model, &msgs, tls.as_ref(), true).await {
                    Ok(r) => u64::from(r.total),
                    Err(_) => 0, // drift sample unavailable; counts above still stand
                }
            }
            Err(_) => 0,
        };

        Ok(RenderCountsOutcome::Counts { per_breakpoint, total })
    }

    /// Commit a [`PendingWrite`] to the index — the success-gated, post-response step
    /// the cache layer runs on a 2xx: upsert the new write entries and
    /// slide the matched read's TTL.
    pub async fn commit(&self, pending: &PendingWrite) -> CacheResult<()> {
        for entry in &pending.writes {
            self.index.write(entry).await?;
        }
        if let Some((scope, hash, new_expires_at)) = &pending.refresh {
            self.index.refresh(scope, hash, *new_expires_at).await?;
        }
        Ok(())
    }

    /// alias → versions (cached from `/v1/models`); `None` if unmapped or the
    /// service is unreachable (→ no caching).
    async fn tokenizer_versions(&self, alias: &str) -> CacheResult<Option<TokenizerVersions>> {
        if let Some(v) = self.versions.get(alias).await {
            cache_metrics::record_tokenizer_version_cache("hit");
            return Ok(v);
        }
        cache_metrics::record_tokenizer_version_cache("miss");
        let Ok(models) = self.tokenizer.models().await else {
            // Service unreachable → deliberately NOT memoised. A genuine "model not in the
            // list" result IS cached below (a stable fact), but an outage is transient: leaving
            // it uncached means caching resumes the instant tokenizer-svc recovers rather than
            // staying dark for the cache TTL. The cost is one cheap failed `/v1/models` GET per
            // cacheable request during the outage — best-effort, off the user path. (Caching
            // None at the 300s TTL here would instead blind the cache for up to 5 min after
            // recovery, which is worse than the redundant probes.)
            return Ok(None);
        };
        let mut found = None;
        for m in models {
            let v = TokenizerVersions {
                tokenizer: m.tokenizer_version,
                template: m.template_version,
            };
            if m.alias == alias {
                found = Some(v.clone());
            }
            self.versions.insert(m.alias, Some(v)).await;
        }
        if found.is_none() {
            self.versions.insert(alias.to_string(), None).await;
        }
        Ok(found)
    }

    /// Find the longest cached prefix: union the walk-back candidates across all
    /// breakpoints, look them up, and pick the match at the deepest block.
    async fn find_longest_read(&self, scope: &IndexScope, parsed: &super::parse::ParsedPrompt) -> CacheResult<Option<ReadHit>> {
        let mut candidates: Vec<PrefixHash> = Vec::new();
        let mut seen: HashSet<PrefixHash> = HashSet::new();
        for bp in &parsed.breakpoints {
            for h in parsed.read_candidates(bp) {
                if seen.insert(h.clone()) {
                    candidates.push(h);
                }
            }
        }
        let lookup_start = std::time::Instant::now();
        let matches = self.index.lookup(scope, &candidates).await;
        // Record before propagating so a slow-then-failing lookup (the unhealthy-DB case the
        // metric most needs to surface) still lands in the histogram, not just successes.
        cache_metrics::record_lookup_duration(lookup_start.elapsed().as_secs_f64());
        let matches = matches?;
        if matches.is_empty() {
            return Ok(None);
        }
        let hash_to_block: HashMap<&[u8], usize> = parsed
            .cumulative_hashes
            .iter()
            .enumerate()
            .map(|(i, h)| (h.as_slice(), i))
            .collect();

        let mut best: Option<ReadHit> = None;
        for m in matches {
            if let Some(&block) = hash_to_block.get(m.prefix_hash.as_slice())
                && best.as_ref().is_none_or(|b| block > b.block)
            {
                best = Some(ReadHit {
                    block,
                    tokens: m.cumulative_token_count,
                    hash: m.prefix_hash,
                    duration: m.ttl_tier.duration(),
                });
            }
        }
        Ok(best)
    }
}

struct ReadHit {
    block: usize,
    tokens: u32,
    hash: PrefixHash,
    duration: chrono::Duration,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::users::Role;
    use crate::prompt_cache::{CacheEntry, IndexScope, PostgresIndex, TtlTier, parse_chat_completions};
    use crate::test::utils::{create_test_api_key_for_user, create_test_endpoint, create_test_model, create_test_user};
    use sqlx::PgPool;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ALIAS: &str = "cache-model";
    const TOK_VER: &str = "sha256:v1";

    /// One marked system block (1h) + an unmarked user block. The prefix is block 0.
    fn body() -> Vec<u8> {
        serde_json::json!({
            "model": ALIAS,
            "messages": [
                {"role":"system","content":[
                    {"type":"text","text":"a long static system prompt","cache_control":{"type":"ephemeral","ttl":"1h"}}
                ]},
                {"role":"user","content":"hello"}
            ]
        })
        .to_string()
        .into_bytes()
    }

    fn all_tiers() -> TierPolicy {
        TierPolicy::from_config(&["5m".to_string(), "1h".to_string(), "24h".to_string()], "5m")
    }

    fn prefix_hash() -> PrefixHash {
        parse_chat_completions(&body(), &all_tiers(), &TelemetryPolicy::default())
            .unwrap()
            .cumulative_hashes[0]
            .clone()
    }

    struct H {
        classifier: Classifier,
        secret: String,
        principal_id: uuid::Uuid,
        pool: PgPool,
        _server: MockServer,
    }

    async fn harness(pool: &PgPool, enabled: bool, tokenize_total: u32, min_prefix: i32) -> H {
        let user = create_test_user(pool, Role::StandardUser).await;
        let key = create_test_api_key_for_user(pool, user.id).await;
        let endpoint = create_test_endpoint(pool, "ep", user.id).await;
        let id = create_test_model(pool, "m", ALIAS, endpoint, user.id).await;
        // Presence of a cache-tariff row IS the enable gate: insert one only when enabled.
        if enabled {
            sqlx::query!(
                r#"INSERT INTO model_cache_tariffs
                     (deployed_model_id, write_multiplier_5m, write_multiplier_1h, write_multiplier_24h, min_prefix_tokens)
                   VALUES ($1, 1.25, 2.0, 2.5, $2)"#,
                id,
                min_prefix
            )
            .execute(pool)
            .await
            .unwrap();
        }

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{"alias": ALIAS, "hf_repo": "org/m", "tokenizer_version": TOK_VER}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/tokenize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "virtual_model": ALIAS, "tokenizer_version": TOK_VER,
                "segment_counts": [tokenize_total], "cumulative": [tokenize_total], "total": tokenize_total
            })))
            .mount(&server)
            .await;

        let classifier = Classifier::new(
            PrincipalResolver::new(pool.clone()),
            ModelConfigResolver::new(pool.clone()),
            TokenizerClient::new(server.uri()),
            Arc::new(PostgresIndex::new(pool.clone(), 0)),
            all_tiers(),
            TelemetryPolicy::default(),
            false,
        );
        H {
            classifier,
            secret: key.secret,
            principal_id: user.id,
            pool: pool.clone(),
            _server: server,
        }
    }

    const TMPL_VER: &str = "sha256:tmpl1";

    /// Harness with a render-capable alias: `/v1/models` advertises a template_version and
    /// `/v1/render` responds as given. `/v1/tokenize` stays mounted (fallback path).
    async fn render_harness(pool: &PgPool, min_prefix: i32, tokenize_total: u32, render_response: ResponseTemplate) -> H {
        let user = create_test_user(pool, Role::StandardUser).await;
        let key = create_test_api_key_for_user(pool, user.id).await;
        let endpoint = create_test_endpoint(pool, "ep", user.id).await;
        let id = create_test_model(pool, "m", ALIAS, endpoint, user.id).await;
        sqlx::query!(
            r#"INSERT INTO model_cache_tariffs
                 (deployed_model_id, write_multiplier_5m, write_multiplier_1h, write_multiplier_24h, min_prefix_tokens)
               VALUES ($1, 1.25, 2.0, 2.5, $2)"#,
            id,
            min_prefix
        )
        .execute(pool)
        .await
        .unwrap();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{"alias": ALIAS, "hf_repo": "org/m", "tokenizer_version": TOK_VER, "template_version": TMPL_VER}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/render"))
            .respond_with(render_response)
            .mount(&server)
            .await;
        // The raw-segment backfill tokenizes ALL blocks (body() has two: the marked
        // system block and the user block) — respond with matching per-segment counts.
        Mock::given(method("POST"))
            .and(path("/v1/tokenize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "virtual_model": ALIAS, "tokenizer_version": TOK_VER,
                "segment_counts": [tokenize_total, 10],
                "cumulative": [tokenize_total, tokenize_total + 10],
                "total": tokenize_total + 10
            })))
            .mount(&server)
            .await;

        let classifier = Classifier::new(
            PrincipalResolver::new(pool.clone()),
            ModelConfigResolver::new(pool.clone()),
            TokenizerClient::new(server.uri()),
            Arc::new(PostgresIndex::new(pool.clone(), 0)),
            all_tiers(),
            TelemetryPolicy::default(),
            true,
        );
        H {
            classifier,
            secret: key.secret,
            principal_id: user.id,
            pool: pool.clone(),
            _server: server,
        }
    }

    #[sqlx::test]
    async fn render_mode_counts_exact_boundaries(pool: PgPool) {
        // The marked system block is the last block of message 0 → boundary 1. Exact
        // count at the boundary (1600) drives the write; `total` (1900) feeds the drift
        // alarm; the scope folds the template version so raw-era entries can't match.
        let h = render_harness(
            &pool,
            1024,
            0, // tokenize must not be consulted
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "virtual_model": ALIAS, "tokenizer_version": TOK_VER, "template_version": TMPL_VER,
                "total": 1600
            })),
        )
        .await;
        let b = body();
        let ClassifyOutcome { stats, pending, active } = h.classifier.classify(req(&h.secret, &b)).await.unwrap();
        assert!(active);
        assert_eq!(stats.creation_1h, 1600, "the prefix render's exact count is the write");
        assert_eq!(stats.render_total, Some(1600), "full-render total feeds the drift alarm");
        assert_eq!(pending.writes.len(), 1);
        assert_eq!(pending.writes[0].cumulative_token_count, 1600);
        assert_eq!(
            pending.writes[0].scope.tokenizer_version,
            format!("{TOK_VER}+{TMPL_VER}"),
            "template version folds into the scope"
        );
    }

    #[sqlx::test]
    async fn render_prices_tool_definition_markers(pool: PgPool) {
        // A marker on a tool DEFINITION is priced by rendering the tools-only prefix —
        // every marker position raw counting prices, exact counting prices too.
        let h = render_harness(
            &pool,
            1024,
            0,
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "virtual_model": ALIAS, "tokenizer_version": TOK_VER, "template_version": TMPL_VER,
                "total": 1400
            })),
        )
        .await;
        let b = serde_json::json!({
            "model": ALIAS,
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {}},
                       "cache_control": {"type": "ephemeral", "ttl": "1h"}}],
            "messages": [{"role": "user", "content": "hello"}]
        })
        .to_string()
        .into_bytes();
        let ClassifyOutcome { stats, pending, active } = h.classifier.classify(req(&h.secret, &b)).await.unwrap();
        assert!(active);
        assert_eq!(stats.creation_1h, 1400, "tools-only prefix priced via render");
        assert_eq!(pending.writes.len(), 1);
        assert_eq!(pending.writes[0].cumulative_token_count, 1400);
    }

    #[sqlx::test]
    async fn render_unsupported_falls_back_to_raw_counting(pool: PgPool) {
        // 400 TEMPLATE_RENDER_FAILED → today's raw-segment counting takes over.
        let h = render_harness(
            &pool,
            1024,
            1500,
            ResponseTemplate::new(400).set_body_string(r#"{"error":"TEMPLATE_RENDER_FAILED"}"#),
        )
        .await;
        let b = body();
        let ClassifyOutcome { stats, pending, active } = h.classifier.classify(req(&h.secret, &b)).await.unwrap();
        assert!(active);
        assert_eq!(stats.creation_1h, 1500, "raw-segment fallback count");
        assert_eq!(stats.render_total, None, "no drift sample without a render");
        assert_eq!(pending.writes.len(), 1);
    }

    #[sqlx::test]
    async fn render_transport_error_degrades_to_no_cache(pool: PgPool) {
        // 503 → exactly like a tokenize outage: no caching for this request.
        let h = render_harness(&pool, 1024, 1500, ResponseTemplate::new(503)).await;
        let b = body();
        let out = h.classifier.classify(req(&h.secret, &b)).await.unwrap();
        assert!(out.active);
        assert!(out.stats.is_zero());
        assert!(out.pending.is_empty());
    }

    #[sqlx::test]
    async fn render_floor_uses_exact_count(pool: PgPool) {
        // Boundary count under the floor → no caching, even if raw counting would clear it.
        let h = render_harness(
            &pool,
            1024,
            5000,
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "virtual_model": ALIAS, "tokenizer_version": TOK_VER, "template_version": TMPL_VER,
                "total": 800
            })),
        )
        .await;
        let b = body();
        let out = h.classifier.classify(req(&h.secret, &b)).await.unwrap();
        assert!(out.active);
        assert!(out.stats.is_zero(), "800 < 1024 floor under exact counting");
        assert!(out.pending.is_empty());
    }

    fn req<'a>(secret: &'a str, body: &'a [u8]) -> ClassifyRequest<'a> {
        ClassifyRequest {
            virtual_model: ALIAS,
            body,
            api_key: Some(secret),
        }
    }

    #[sqlx::test]
    async fn no_prior_entry_is_all_creation(pool: PgPool) {
        let h = harness(&pool, true, 1500, 1024).await;
        let b = body();
        let ClassifyOutcome { stats, pending, active } = h.classifier.classify(req(&h.secret, &b)).await.unwrap();

        assert!(active, "enabled model is active");
        assert_eq!(stats.read, 0);
        assert_eq!(stats.creation_1h, 1500);
        assert_eq!(stats.creation_total(), 1500);
        assert_eq!(pending.writes.len(), 1);
        assert_eq!(pending.writes[0].cumulative_token_count, 1500);
        assert_eq!(pending.writes[0].ttl_tier, TtlTier::OneHour);
        assert_eq!(pending.writes[0].prefix_hash, prefix_hash());
        assert!(pending.refresh.is_none());
    }

    #[sqlx::test]
    async fn read_hit_is_pure_read(pool: PgPool) {
        let h = harness(&pool, true, 1500, 1024).await;
        // Seed the entry this prefix would write, as if a prior request created it.
        let scope = IndexScope {
            principal_id: h.principal_id,
            virtual_model: ALIAS.to_string(),
            tokenizer_version: TOK_VER.to_string(),
        };
        PostgresIndex::new(h.pool.clone(), 0)
            .write(&CacheEntry {
                scope: scope.clone(),
                prefix_hash: prefix_hash(),
                cumulative_token_count: 1500,
                ttl_tier: TtlTier::OneHour,
                expires_at: Utc::now() + chrono::Duration::hours(1),
            })
            .await
            .unwrap();

        let b = body();
        let ClassifyOutcome { stats, pending, active } = h.classifier.classify(req(&h.secret, &b)).await.unwrap();
        assert!(active);
        assert_eq!(stats.read, 1500);
        assert_eq!(stats.creation_total(), 0, "a full read writes nothing");
        assert!(pending.writes.is_empty());
        assert!(pending.refresh.is_some(), "a read slides the entry's TTL");
    }

    #[sqlx::test]
    async fn below_floor_is_no_cache(pool: PgPool) {
        let h = harness(&pool, true, 500, 1024).await; // 500 < 1024
        let b = body();
        // Enabled but below the floor → active (uniform zeros) with nothing to commit.
        let out = h.classifier.classify(req(&h.secret, &b)).await.unwrap();
        assert!(out.active, "an enabled model stays active even below the floor");
        assert!(out.stats.is_zero());
        assert!(out.pending.is_empty());
    }

    #[sqlx::test]
    async fn disabled_model_is_inactive(pool: PgPool) {
        let h = harness(&pool, false, 1500, 1024).await; // not enabled
        let b = body();
        let out = h.classifier.classify(req(&h.secret, &b)).await.unwrap();
        assert!(!out.active, "a disabled model is inactive → response left untouched");
        assert!(out.stats.is_zero());
        assert!(out.pending.is_empty());
    }

    #[sqlx::test]
    async fn automatic_marker_caches_on_last_block(pool: PgPool) {
        // A top-level (automatic) cache_control with NO block markers synthesizes a breakpoint on the
        // last block and writes the prefix at the marker's tier. (Single block so the write span is
        // one segment, matching the harness's one-count tokenizer mock; multi-block last-block
        // positioning is covered by the parse unit tests.)
        let h = harness(&pool, true, 1500, 1024).await;
        let b = serde_json::json!({
            "model": ALIAS,
            "cache_control": {"type": "ephemeral", "ttl": "1h"},
            "messages": [{"role":"user","content":"a long static prompt plus the question"}]
        })
        .to_string()
        .into_bytes();

        let ClassifyOutcome { stats, pending, active } = h.classifier.classify(req(&h.secret, &b)).await.unwrap();
        assert!(active);
        assert_eq!(stats.read, 0);
        assert_eq!(stats.creation_1h, 1500, "automatic marker writes the prefix at its tier");
        assert_eq!(pending.writes.len(), 1);
        // The write is keyed at the synthesized breakpoint (the last block).
        let parsed = parse_chat_completions(&b, &all_tiers(), &TelemetryPolicy::default()).unwrap();
        assert_eq!(parsed.breakpoints.len(), 1);
        assert_eq!(
            parsed.breakpoints[0].block_index, 0,
            "breakpoint synthesized on the (only) last block"
        );
        assert_eq!(pending.writes[0].prefix_hash, parsed.cumulative_hashes[0]);
    }

    #[sqlx::test]
    async fn no_markers_is_zero_active(pool: PgPool) {
        let h = harness(&pool, true, 1500, 1024).await;
        let b = serde_json::json!({
            "model": ALIAS,
            "messages": [{"role":"user","content":"hi, no markers here"}]
        })
        .to_string()
        .into_bytes();
        // Enabled model, no markers → active (uniform zeros), nothing committed.
        let out = h.classifier.classify(req(&h.secret, &b)).await.unwrap();
        assert!(out.active, "enabled model with no markers still presents zero cache fields");
        assert!(out.stats.is_zero());
        assert!(out.pending.is_empty());
    }
}
