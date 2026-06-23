//! The classify orchestration (plan §6.3 branch B): turn a request into a neutral
//! [`CacheStats`] split plus the [`PendingWrite`] to commit on success.
//!
//! Flow: resolve principal → per-model gate → parse markers → find the longest cached
//! prefix (read, via the 20-block walk-back) → tokenize the new suffix (write) →
//! enforce the min-prefix floor → assemble. Reads need no tokenization (the count is
//! stored on the entry); only the new write span is tokenized, and it runs in parallel
//! with generation. Any recoverable failure (tokenizer down, model unmapped, parse
//! error, no principal) degrades to all-zero "no caching" — never an error to the
//! customer (best-effort; §11 reconciliation backstops residual overcharges).
//!
//! v1 scope: chat-completions message bodies. Tool-using multi-step Responses are a
//! fast-follow (§0); image tokens fall into the uncached tail (§19).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;

use super::index::{CacheEntry, CacheIndex, CacheResult, IndexScope, PrefixHash};
use super::model_config::ModelConfigResolver;
use super::parse::parse_chat_completions;
use super::principal::PrincipalResolver;
use super::stats::{CacheStats, PendingWrite};
use super::tokenizer::TokenizerClient;

/// What `classify` needs from the request.
pub struct ClassifyRequest<'a> {
    /// The virtual model (the `deployed_models.alias` = the cache key dimension).
    pub virtual_model: &'a str,
    /// The raw request body (with `cache_control` markers intact).
    pub body: &'a [u8],
    /// The validated bearer token, or `None` (un-scopable → no caching).
    pub api_key: Option<&'a str>,
}

/// The result of [`Classifier::classify`].
///
/// `active` is true once the per-model gate passes (the model is cache-enabled),
/// independent of whether this particular prompt cached anything. It drives the
/// uniform-zeros injection (§0.2): an enabled model always gets the `cache_*` usage
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
    /// alias → tokenizer_version (from tokenizer-svc `/v1/models`); `None` = unmapped.
    versions: moka::future::Cache<String, Option<String>>,
}

impl Classifier {
    pub fn new(
        principal: PrincipalResolver,
        model_config: ModelConfigResolver,
        tokenizer: TokenizerClient,
        index: Arc<dyn CacheIndex>,
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
        }
    }

    /// Classify a request into its read/write split + the entries to commit on success.
    ///
    /// Pre-`cfg.enabled` bails are `inactive` (unscopable / disabled model → response
    /// untouched). Once the model is enabled, every bail is `zero_active` (uniform
    /// zeros injected, nothing committed) — so enabled models present one shape.
    pub async fn classify(&self, req: ClassifyRequest<'_>) -> CacheResult<ClassifyOutcome> {
        // Gates that fire *before* we know the model is cache-enabled → inactive.
        let Some(api_key) = req.api_key else {
            return Ok(ClassifyOutcome::inactive());
        };
        let Some(org_id) = self.principal.resolve(api_key).await? else {
            return Ok(ClassifyOutcome::inactive());
        };
        let cfg = self.model_config.resolve(req.virtual_model).await?;
        if !cfg.enabled {
            return Ok(ClassifyOutcome::inactive());
        }

        // From here the model is cache-enabled: any bail is `zero_active`.
        let Ok(parsed) = parse_chat_completions(req.body) else {
            return Ok(ClassifyOutcome::zero_active()); // unparseable / >4 breakpoints
        };
        if parsed.breakpoints.is_empty() {
            return Ok(ClassifyOutcome::zero_active()); // markers are required to cache (§1)
        }
        let Some(tokenizer_version) = self.tokenizer_version(req.virtual_model).await? else {
            return Ok(ClassifyOutcome::zero_active()); // model not mapped in tokenizer-svc
        };
        let scope = IndexScope {
            org_id,
            virtual_model: req.virtual_model.to_string(),
            tokenizer_version,
        };

        // Longest cached prefix across all breakpoints' walk-back windows.
        let read = self.find_longest_read(&scope, &parsed).await?;
        let read_block = read.as_ref().map(|r| r.block); // index into parsed.blocks
        let read_tokens = read.as_ref().map(|r| r.tokens).unwrap_or(0);

        let deepest = parsed.breakpoints.last().expect("non-empty checked above").block_index;

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

        // Write span: blocks after the matched read, up to the deepest breakpoint.
        let write_start = read_block.map(|b| b + 1).unwrap_or(0);
        let segments: Vec<String> = parsed.blocks[write_start..=deepest].iter().map(|b| b.text.clone()).collect();

        // Tokenize the suffix (the only tokenization; reads needed none). Failure →
        // degrade to no caching (safe under the best-effort contract).
        let Ok(tok) = self.tokenizer.tokenize(req.virtual_model, &segments).await else {
            return Ok(ClassifyOutcome::zero_active());
        };
        if tok.cumulative.len() != segments.len() {
            return Ok(ClassifyOutcome::zero_active()); // shape mismatch — bail safely
        }

        // cumulative token count *at* each block in the write span (with the read offset).
        let cumulative_at = |block: usize| -> u64 { read_tokens as u64 + tok.cumulative[block - write_start] as u64 };
        let total_prefix = cumulative_at(deepest);
        if total_prefix < cfg.min_prefix_tokens as u64 {
            return Ok(ClassifyOutcome::zero_active()); // below the per-model floor → no caching
        }

        // Each breakpoint beyond the read is its own cached prefix; the segment it
        // closes is creation under its tier. (`block_index > read_block`, treating a
        // no-read as -1, selects exactly the breakpoints within the write span.)
        let mut prev_boundary = read_tokens as u64;
        let now = Utc::now();
        let read_block_idx: isize = read_block.map(|b| b as isize).unwrap_or(-1);
        for bp in parsed.breakpoints.iter().filter(|bp| bp.block_index as isize > read_block_idx) {
            let bp_cumulative = cumulative_at(bp.block_index);
            let segment_tokens = bp_cumulative.saturating_sub(prev_boundary);
            stats.add_creation(bp.ttl_tier, segment_tokens);
            pending.writes.push(CacheEntry {
                scope: scope.clone(),
                prefix_hash: parsed.cumulative_hashes[bp.block_index].clone(),
                cumulative_token_count: bp_cumulative.min(u32::MAX as u64) as u32,
                ttl_tier: bp.ttl_tier,
                expires_at: now + bp.ttl_tier.duration(),
            });
            prev_boundary = bp_cumulative;
        }

        Ok(ClassifyOutcome::active(stats, pending))
    }

    /// Commit a [`PendingWrite`] to the index — the success-gated, post-response step
    /// the cache layer runs on a 2xx (§6.3 step 8): upsert the new write entries and
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

    /// alias → tokenizer_version (cached from `/v1/models`); `None` if unmapped or the
    /// service is unreachable (→ no caching).
    async fn tokenizer_version(&self, alias: &str) -> CacheResult<Option<String>> {
        if let Some(v) = self.versions.get(alias).await {
            return Ok(v);
        }
        let Ok(models) = self.tokenizer.models().await else {
            return Ok(None);
        };
        let mut found = None;
        for m in models {
            if m.alias == alias {
                found = Some(m.tokenizer_version.clone());
            }
            self.versions.insert(m.alias, Some(m.tokenizer_version)).await;
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
        let matches = self.index.lookup(scope, &candidates).await?;
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

    fn prefix_hash() -> PrefixHash {
        parse_chat_completions(&body()).unwrap().cumulative_hashes[0].clone()
    }

    struct H {
        classifier: Classifier,
        secret: String,
        org_id: uuid::Uuid,
        pool: PgPool,
        _server: MockServer,
    }

    async fn harness(pool: &PgPool, enabled: bool, tokenize_total: u32, min_prefix: i32) -> H {
        let user = create_test_user(pool, Role::StandardUser).await;
        let key = create_test_api_key_for_user(pool, user.id).await;
        let endpoint = create_test_endpoint(pool, "ep", user.id).await;
        let id = create_test_model(pool, "m", ALIAS, endpoint, user.id).await;
        if enabled {
            sqlx::query!("UPDATE deployed_models SET cache_pricing_enabled = true WHERE id = $1", id)
                .execute(pool)
                .await
                .unwrap();
        }
        sqlx::query!(
            r#"INSERT INTO model_cache_tariffs (deployed_model_id, ttl_tier, write_multiplier, min_prefix_tokens)
               VALUES ($1, '1h', 2.0, $2)"#,
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
            Arc::new(PostgresIndex::new(pool.clone())),
        );
        H {
            classifier,
            secret: key.secret,
            org_id: user.id,
            pool: pool.clone(),
            _server: server,
        }
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
            org_id: h.org_id,
            virtual_model: ALIAS.to_string(),
            tokenizer_version: TOK_VER.to_string(),
        };
        PostgresIndex::new(h.pool.clone())
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
