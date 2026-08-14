//! Reconstructing a request's cache read/creation split as it stood at a past instant.
//!
//! The serving classifier already does every hard part — normalise the body, derive
//! breakpoints, compute cumulative prefix hashes, render exact token counts — and takes its
//! index as `Arc<dyn CacheIndex>`. So historical reconstruction needs no reimplementation of
//! that pipeline: swap the index for one that answers "was this prefix live at `at`" instead
//! of "is it live now", and the same code produces the split for a past request.
//!
//! Reimplementing the hashing would defeat the purpose. A reconstruction that derives hashes
//! differently from the serving path cannot verify the serving path — it can only disagree
//! with it for reasons nobody can attribute.
//!
//! # Two bounds on the answer
//!
//! - **Retention.** `prompt_cache_entries` currently keeps expired rows, so all history is
//!   reachable. Once the sweeper ships with its grace period, requests older than the grace
//!   have no entries left and the split cannot be reconstructed at all.
//! - **Accuracy before the episode-per-row cutover.** Today a post-expiry write revives the
//!   same row in place and keeps its original `created_at`, so `[created_at, expires_at]`
//!   can span invisible dead gaps. Liveness is therefore *over*-approximated: some creations
//!   read back as reads, which under-bills. Answers are marked [`Fidelity::Approximate`].

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::prompt_cache::{CacheEntry, CacheError, CacheIndex, CacheMatch, CacheResult, IndexScope, PrefixHash, TtlTier};

/// How much the reconstructed split can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fidelity {
    /// Every episode covering the instant has an exact window.
    Exact,
    /// At least one entry predates the episode-per-row cutover, so its window
    /// over-approximates liveness and the split may under-count creations.
    Approximate,
}

/// A [`CacheIndex`] that answers as of a fixed past instant.
///
/// Writes are refused rather than ignored. A recompute is read-only, and silently accepting
/// a write here would let a reconstruction mutate the very index it is reading — corrupting
/// the evidence for every later run over the same period.
pub struct HistoricalIndex {
    pool: PgPool,
    at: DateTime<Utc>,
}

impl HistoricalIndex {
    pub fn new(pool: PgPool, at: DateTime<Utc>) -> Self {
        Self { pool, at }
    }
}

struct LookupRow {
    prefix_hash: Vec<u8>,
    cumulative_token_count: i32,
    ttl_tier: String,
    expires_at: DateTime<Utc>,
}

#[async_trait]
impl CacheIndex for HistoricalIndex {
    async fn lookup(&self, scope: &IndexScope, candidate_hashes: &[PrefixHash]) -> CacheResult<Vec<CacheMatch>> {
        if candidate_hashes.is_empty() {
            return Ok(Vec::new());
        }
        // The live path asks `expires_at > now()`. The historical question is whether an
        // episode *covered* the instant, which needs both ends of the window.
        let rows = sqlx::query_as!(
            LookupRow,
            r#"
            SELECT prefix_hash AS "prefix_hash!", cumulative_token_count AS "cumulative_token_count!",
                   ttl_tier AS "ttl_tier!", expires_at AS "expires_at!"
            FROM prompt_cache_entries
            WHERE principal_id = $1 AND virtual_model = $2 AND tokenizer_version = $3
              AND prefix_hash = ANY($4)
              AND created_at <= $5 AND expires_at >= $5
            "#,
            scope.principal_id,
            scope.virtual_model,
            scope.tokenizer_version,
            candidate_hashes,
            self.at,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .filter_map(|r| {
                TtlTier::parse(&r.ttl_tier).map(|tier| CacheMatch {
                    prefix_hash: r.prefix_hash,
                    cumulative_token_count: r.cumulative_token_count as u32,
                    ttl_tier: tier,
                    expires_at: r.expires_at,
                })
            })
            .collect())
    }

    async fn write(&self, _entry: &CacheEntry) -> CacheResult<()> {
        Err(CacheError::Invalid(
            "a historical cache index is read-only: reconstruction must never mutate the index it reads".into(),
        ))
    }

    async fn refresh(&self, _scope: &IndexScope, _hash: &PrefixHash, _new_expires_at: DateTime<Utc>) -> CacheResult<()> {
        Err(CacheError::Invalid(
            "a historical cache index is read-only: refreshing an entry would slide a window that already closed".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    async fn seed_entry(pool: &PgPool, principal: Uuid, hash: &[u8], created: DateTime<Utc>, expires: DateTime<Utc>) {
        sqlx::query!(
            "INSERT INTO prompt_cache_entries
               (principal_id, virtual_model, tokenizer_version, prefix_hash,
                cumulative_token_count, ttl_tier, created_at, expires_at)
             VALUES ($1,'m','tok-v1',$2,4096,'5m',$3,$4)",
            principal,
            hash,
            created,
            expires,
        )
        .execute(pool)
        .await
        .unwrap();
    }

    fn scope(principal: Uuid) -> IndexScope {
        IndexScope {
            principal_id: principal,
            virtual_model: "m".to_string(),
            tokenizer_version: "tok-v1".to_string(),
        }
    }

    /// The whole point: an entry that expired long ago is invisible to the live index but
    /// must be found when asking about an instant it covered.
    #[sqlx::test]
    async fn an_expired_entry_is_found_at_an_instant_it_covered(pool: PgPool) {
        let principal = Uuid::new_v4();
        let hash = b"prefix-a".to_vec();
        let created = Utc::now() - chrono::Duration::days(3);
        let expires = created + chrono::Duration::minutes(5);
        seed_entry(&pool, principal, &hash, created, expires).await;

        // Mid-window: a read.
        let at = created + chrono::Duration::minutes(2);
        let hits = HistoricalIndex::new(pool.clone(), at)
            .lookup(&scope(principal), std::slice::from_ref(&hash))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "the episode covered this instant");
        assert_eq!(hits[0].cumulative_token_count, 4096);

        // After the window closed: a creation, not a read.
        let after = expires + chrono::Duration::minutes(1);
        let miss = HistoricalIndex::new(pool.clone(), after)
            .lookup(&scope(principal), std::slice::from_ref(&hash))
            .await
            .unwrap();
        assert!(miss.is_empty(), "the episode had closed by then");

        // Before it was ever written: also a creation.
        let before = created - chrono::Duration::minutes(1);
        let miss = HistoricalIndex::new(pool, before)
            .lookup(&scope(principal), std::slice::from_ref(&hash))
            .await
            .unwrap();
        assert!(miss.is_empty(), "the entry did not exist yet");
    }

    /// Scope is part of the key: another principal's identical prefix is not a hit.
    #[sqlx::test]
    async fn lookup_is_scoped_to_the_principal(pool: PgPool) {
        let mine = Uuid::new_v4();
        let theirs = Uuid::new_v4();
        let hash = b"shared-content".to_vec();
        let created = Utc::now() - chrono::Duration::hours(1);
        seed_entry(&pool, theirs, &hash, created, created + chrono::Duration::hours(2)).await;

        let hits = HistoricalIndex::new(pool, Utc::now())
            .lookup(&scope(mine), std::slice::from_ref(&hash))
            .await
            .unwrap();
        assert!(hits.is_empty(), "another principal's cache is not mine to read");
    }

    /// A reconstruction that wrote to the index would corrupt the evidence for every later
    /// run over the same period, so writes fail loudly rather than being ignored.
    #[sqlx::test]
    async fn writes_are_refused_not_silently_dropped(pool: PgPool) {
        let idx = HistoricalIndex::new(pool, Utc::now());
        let entry = CacheEntry {
            scope: scope(Uuid::new_v4()),
            prefix_hash: b"x".to_vec(),
            cumulative_token_count: 1,
            ttl_tier: TtlTier::FiveMinutes,
            expires_at: Utc::now(),
        };
        assert!(idx.write(&entry).await.is_err());
        assert!(idx.refresh(&scope(Uuid::new_v4()), &b"x".to_vec(), Utc::now()).await.is_err());
    }
}

/// A cache split reconstructed from the prefix index as it stood at a past instant.
#[derive(Debug, Clone)]
pub struct ReconstructedSplit {
    pub read: u64,
    pub creation_5m: u64,
    pub creation_1h: u64,
    pub creation_24h: u64,
    /// The exact chat-templated prompt total from `/v1/render`, when the classifier counted
    /// that way. This is the tokenizer's independent answer for the whole prompt — the thing
    /// that makes a reconstruction a *verification* rather than a restatement.
    pub render_total: Option<u64>,
    pub fidelity: Fidelity,
}

impl ReconstructedSplit {
    pub fn creations_total(&self) -> u64 {
        self.creation_5m + self.creation_1h + self.creation_24h
    }
}

/// Reconstruct one request's split by running the serving classifier against a
/// [`HistoricalIndex`].
///
/// `classifier` must have been built with that index; this call never commits, so nothing is
/// written. `principal` is supplied directly because the bearer token behind a historical
/// row may since have been rotated.
pub async fn reconstruct_split(
    classifier: &crate::prompt_cache::Classifier,
    virtual_model: &str,
    request_body: &[u8],
    principal: crate::types::UserId,
    at: DateTime<Utc>,
) -> CacheResult<Option<ReconstructedSplit>> {
    let outcome = classifier
        .classify(crate::prompt_cache::ClassifyRequest {
            virtual_model,
            body: request_body,
            api_key: None,
            principal: Some(principal),
        })
        .await?;

    if !outcome.active {
        // Model was not cache-enabled for this principal: there is no split to reconstruct,
        // which is different from a split of zero.
        return Ok(None);
    }
    if outcome.degraded {
        // The classifier's counting infrastructure failed *now* (tokenizer-svc unreachable,
        // render/tokenize transport error) and it degraded to zeros — correct for serving,
        // but as a reconstruction it would read as "the serving path billed cache tokens the
        // request never had". Measured: a dead tokenizer port-forward turned a healthy
        // 25-row corpus into 25 confident false disagreements. Refuse instead.
        return Err(CacheError::Invalid(
            "classifier degraded (tokenizer unavailable): a zero split from a degraded classify is not evidence".into(),
        ));
    }

    let s = outcome.stats;
    Ok(Some(ReconstructedSplit {
        read: s.read,
        creation_5m: s.creation_5m,
        creation_1h: s.creation_1h,
        creation_24h: s.creation_24h,
        render_total: s.render_total,
        // Every entry reachable today predates the episode-per-row cutover, so any window
        // covering `at` over-approximates liveness. Revisit once that ships.
        fidelity: if at < Utc::now() { Fidelity::Approximate } else { Fidelity::Exact },
    }))
}
