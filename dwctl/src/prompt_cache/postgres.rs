//! Postgres baseline for the [`CacheIndex`]: always correct, the single
//! source of truth. A Redis accelerator (later) write-behinds to this and reads
//! through it; nothing is ever *reliant* on Redis.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tracing::instrument;

use super::index::{CacheEntry, CacheError, CacheIndex, CacheMatch, CacheResult, IndexScope, PrefixHash, TtlTier};

/// Postgres-backed prefix index over `prompt_cache_entries`.
#[derive(Clone)]
pub struct PostgresIndex {
    pool: PgPool,
}

impl PostgresIndex {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl CacheIndex for PostgresIndex {
    #[instrument(skip_all, fields(model = %scope.virtual_model, candidates = candidate_hashes.len()), err)]
    async fn lookup(&self, scope: &IndexScope, candidate_hashes: &[PrefixHash]) -> CacheResult<Vec<CacheMatch>> {
        if candidate_hashes.is_empty() {
            return Ok(Vec::new());
        }
        // Point lookup on the (org, model, tok, hash) unique btree, filtered to live
        // entries. now() is applied here (it can't sit in a partial-index predicate).
        let rows = sqlx::query!(
            r#"
            SELECT prefix_hash, cumulative_token_count, ttl_tier, expires_at
            FROM prompt_cache_entries
            WHERE principal_id = $1 AND virtual_model = $2 AND tokenizer_version = $3
              AND prefix_hash = ANY($4) AND expires_at > now()
            "#,
            scope.principal_id,
            scope.virtual_model,
            scope.tokenizer_version,
            candidate_hashes,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|r| {
                let ttl_tier =
                    TtlTier::parse(&r.ttl_tier).ok_or_else(|| CacheError::Invalid(format!("unknown ttl_tier {:?}", r.ttl_tier)))?;
                Ok(CacheMatch {
                    prefix_hash: r.prefix_hash,
                    cumulative_token_count: r.cumulative_token_count.max(0) as u32,
                    ttl_tier,
                    expires_at: r.expires_at,
                })
            })
            .collect()
    }

    #[instrument(skip_all, fields(model = %entry.scope.virtual_model, ttl = entry.ttl_tier.as_str()), err)]
    async fn write(&self, entry: &CacheEntry) -> CacheResult<()> {
        // Upsert: a re-write of the same prefix refreshes its count/tier/expiry.
        sqlx::query!(
            r#"
            INSERT INTO prompt_cache_entries
              (principal_id, virtual_model, tokenizer_version, prefix_hash,
               cumulative_token_count, ttl_tier, expires_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (principal_id, virtual_model, tokenizer_version, prefix_hash)
            DO UPDATE SET
              cumulative_token_count = EXCLUDED.cumulative_token_count,
              ttl_tier               = EXCLUDED.ttl_tier,
              expires_at             = EXCLUDED.expires_at
            "#,
            entry.scope.principal_id,
            entry.scope.virtual_model,
            entry.scope.tokenizer_version,
            entry.prefix_hash,
            i32::try_from(entry.cumulative_token_count).unwrap_or(i32::MAX),
            entry.ttl_tier.as_str(),
            entry.expires_at,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    #[instrument(skip_all, fields(model = %scope.virtual_model), err)]
    async fn refresh(&self, scope: &IndexScope, prefix_hash: &PrefixHash, new_expires_at: DateTime<Utc>) -> CacheResult<()> {
        // Slide the window forward (the sliding-TTL refresh). In pure Postgres this
        // is a direct UPDATE per read; the Redis accelerator debounces it.
        sqlx::query!(
            r#"
            UPDATE prompt_cache_entries
            SET expires_at = $5
            WHERE principal_id = $1 AND virtual_model = $2 AND tokenizer_version = $3
              AND prefix_hash = $4
            "#,
            scope.principal_id,
            scope.virtual_model,
            scope.tokenizer_version,
            prefix_hash,
            new_expires_at,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;

    fn scope() -> IndexScope {
        IndexScope {
            principal_id: uuid::Uuid::new_v4(),
            virtual_model: "test-model".to_string(),
            tokenizer_version: "sha256:abc".to_string(),
        }
    }

    fn entry(scope: &IndexScope, hash: &[u8], tokens: u32, tier: TtlTier) -> CacheEntry {
        CacheEntry {
            scope: scope.clone(),
            prefix_hash: hash.to_vec(),
            cumulative_token_count: tokens,
            ttl_tier: tier,
            expires_at: Utc::now() + tier.duration(),
        }
    }

    #[sqlx::test]
    async fn write_then_lookup_returns_match(pool: PgPool) {
        let idx = PostgresIndex::new(pool);
        let s = scope();
        idx.write(&entry(&s, b"hash-a", 1024, TtlTier::OneHour)).await.unwrap();

        let hits = idx.lookup(&s, &[b"hash-a".to_vec(), b"hash-missing".to_vec()]).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].prefix_hash, b"hash-a");
        assert_eq!(hits[0].cumulative_token_count, 1024);
        assert_eq!(hits[0].ttl_tier, TtlTier::OneHour);
    }

    #[sqlx::test]
    async fn lookup_excludes_expired_and_other_scopes(pool: PgPool) {
        let idx = PostgresIndex::new(pool);
        let s = scope();

        // Expired entry must not match.
        let mut expired = entry(&s, b"old", 10, TtlTier::FiveMinutes);
        expired.expires_at = Utc::now() - chrono::Duration::seconds(1);
        idx.write(&expired).await.unwrap();
        assert!(idx.lookup(&s, &[b"old".to_vec()]).await.unwrap().is_empty());

        // Same hash under a different org is a different entry.
        idx.write(&entry(&s, b"shared", 5, TtlTier::OneHour)).await.unwrap();
        let other = IndexScope {
            principal_id: uuid::Uuid::new_v4(),
            ..s.clone()
        };
        assert!(idx.lookup(&other, &[b"shared".to_vec()]).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn refresh_slides_expiry(pool: PgPool) {
        let idx = PostgresIndex::new(pool);
        let s = scope();
        let mut e = entry(&s, b"refreshable", 7, TtlTier::FiveMinutes);
        e.expires_at = Utc::now() + chrono::Duration::seconds(2);
        idx.write(&e).await.unwrap();

        let new_expiry = Utc::now() + chrono::Duration::hours(1);
        idx.refresh(&s, &b"refreshable".to_vec(), new_expiry).await.unwrap();

        let hits = idx.lookup(&s, &[b"refreshable".to_vec()]).await.unwrap();
        assert_eq!(hits.len(), 1);
        // Expiry moved out to ~1h, well beyond the original 2s.
        assert!(hits[0].expires_at > Utc::now() + chrono::Duration::minutes(30));
    }

    #[sqlx::test]
    async fn write_upserts_on_conflict(pool: PgPool) {
        let idx = PostgresIndex::new(pool);
        let s = scope();
        idx.write(&entry(&s, b"dup", 100, TtlTier::FiveMinutes)).await.unwrap();
        idx.write(&entry(&s, b"dup", 200, TtlTier::OneHour)).await.unwrap();

        let hits = idx.lookup(&s, &[b"dup".to_vec()]).await.unwrap();
        assert_eq!(hits.len(), 1, "upsert must not create a duplicate row");
        assert_eq!(hits[0].cumulative_token_count, 200);
        assert_eq!(hits[0].ttl_tier, TtlTier::OneHour);
    }
}
