//! Postgres baseline for the [`CacheIndex`]: always correct, the single
//! source of truth. A Redis accelerator (later) write-behinds to this and reads
//! through it; nothing is ever *reliant* on Redis.
//!
//! ## Connection-error retry
//!
//! Every op retries up to `cache.index_conn_retries` times (default 1, backed off
//! 100ms·2^n) on a connection-class failure. Evidence (2026-07): 100% of the
//! ~500-1000/day classify errors occur on the fusillade-batch pod, which idles between
//! batches then fires ~100 concurrent loopback requests in a second. Neon's proxy reaps
//! idle connections sooner than the pool's `idle_timeout`, so the burst is handed
//! already-severed conns ("expected to read 5 bytes, got 0") while simultaneously
//! cold-starting new ones (TLS EOF / auth timeout) — instant-fail errors, not slow queries.
//! A retry acquires a fresh connection and typically succeeds in milliseconds, well
//! inside the classify deadline (which still bounds the caller — a retry never extends it).
//! Non-connection errors (constraint violations, bad data) are NOT retried. This mirrors
//! the fix the batch daemon's own queries received for the same severed-conn failure mode.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tracing::instrument;

use super::index::{CacheEntry, CacheError, CacheIndex, CacheMatch, CacheResult, IndexScope, PrefixHash, TtlTier};
use super::metrics as cache_metrics;

/// Postgres-backed prefix index over `prompt_cache_entries`.
#[derive(Clone)]
pub struct PostgresIndex {
    pool: PgPool,
    /// Connection-error retries per op (`cache.index_conn_retries`; 0 = never retry).
    conn_retries: u32,
}

impl PostgresIndex {
    pub fn new(pool: PgPool, conn_retries: u32) -> Self {
        Self { pool, conn_retries }
    }
}

/// A failure of the CONNECTION, not the query: a severed pooled conn (Io/Protocol), a
/// TLS handshake killed mid-setup (Tls), or the upstream proxy timing out authentication
/// on a fresh conn (surfaces as a database error with this message on Neon). These are
/// instant-fail and safe to retry; `PoolTimedOut` is deliberately excluded — by the time
/// the acquire timeout fires, the classify deadline has long passed and a retry would
/// just burn another acquire cycle.
fn is_transient_connection_error(e: &sqlx::Error) -> bool {
    match e {
        sqlx::Error::Io(_) | sqlx::Error::Tls(_) | sqlx::Error::Protocol(_) => true,
        sqlx::Error::Database(db) => db.message().contains("Authentication timed out"),
        _ => false,
    }
}

/// Run `op` up to `1 + $retries` times: retry iff an attempt failed with a connection-class
/// error, backing off 100ms·2^n + 0..20% jitter between attempts so a herd of simultaneous
/// failures — the observed burst shape — doesn't re-storm connection setup in lockstep.
/// Each retry is recorded so dashboards see the underlying churn even when the retry
/// succeeds. The classify deadline still bounds the caller regardless of retries.
macro_rules! with_conn_retry {
    ($op_name:literal, $retries:expr, $op:expr) => {{
        let mut attempt: u32 = 0;
        loop {
            match $op.await {
                Err(e) if is_transient_connection_error(&e) && attempt < $retries => {
                    cache_metrics::record_index_conn_retry($op_name);
                    tracing::debug!(op = $op_name, attempt, error = %e, "cache index connection error — retrying with a fresh connection");
                    // 0..20% positive jitter (same scheme as image_normalizer's fetch retry):
                    // the observed failure shape is a ~100-request herd failing in the same
                    // instant, so identical deterministic sleeps would re-storm connection
                    // setup in lockstep.
                    let base_ms = 100u64 << attempt.min(4);
                    let jitter_ms = rand::prelude::RngExt::random_range(&mut rand::rng(), 0..(base_ms / 5 + 1));
                    tokio::time::sleep(std::time::Duration::from_millis(base_ms + jitter_ms)).await;
                    attempt += 1;
                }
                other => break other,
            }
        }
    }};
}

impl PostgresIndex {
    async fn lookup_once(&self, scope: &IndexScope, candidate_hashes: &[PrefixHash]) -> Result<Vec<LookupRow>, sqlx::Error> {
        // Point lookup on the (org, model, tok, hash) unique btree, filtered to live
        // entries. now() is applied here (it can't sit in a partial-index predicate).
        sqlx::query_as!(
            LookupRow,
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
        .await
    }

    async fn write_once(&self, entry: &CacheEntry) -> Result<(), sqlx::Error> {
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
        .await
        .map(|_| ())
    }

    async fn refresh_once(&self, scope: &IndexScope, prefix_hash: &PrefixHash, new_expires_at: DateTime<Utc>) -> Result<(), sqlx::Error> {
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
        .await
        .map(|_| ())
    }
}

/// Row shape shared by the lookup attempts.
struct LookupRow {
    prefix_hash: Vec<u8>,
    cumulative_token_count: i32,
    ttl_tier: String,
    expires_at: DateTime<Utc>,
}

#[async_trait]
impl CacheIndex for PostgresIndex {
    #[instrument(skip_all, fields(model = %scope.virtual_model, candidates = candidate_hashes.len()), err)]
    async fn lookup(&self, scope: &IndexScope, candidate_hashes: &[PrefixHash]) -> CacheResult<Vec<CacheMatch>> {
        if candidate_hashes.is_empty() {
            return Ok(Vec::new());
        }
        let rows = with_conn_retry!("lookup", self.conn_retries, self.lookup_once(scope, candidate_hashes))?;

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
        with_conn_retry!("write", self.conn_retries, self.write_once(entry))?;
        Ok(())
    }

    #[instrument(skip_all, fields(model = %scope.virtual_model), err)]
    async fn refresh(&self, scope: &IndexScope, prefix_hash: &PrefixHash, new_expires_at: DateTime<Utc>) -> CacheResult<()> {
        with_conn_retry!("refresh", self.conn_retries, self.refresh_once(scope, prefix_hash, new_expires_at))?;
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
        let idx = PostgresIndex::new(pool, 1);
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
        let idx = PostgresIndex::new(pool, 1);
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
        let idx = PostgresIndex::new(pool, 1);
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
        let idx = PostgresIndex::new(pool, 1);
        let s = scope();
        idx.write(&entry(&s, b"dup", 100, TtlTier::FiveMinutes)).await.unwrap();
        idx.write(&entry(&s, b"dup", 200, TtlTier::OneHour)).await.unwrap();

        let hits = idx.lookup(&s, &[b"dup".to_vec()]).await.unwrap();
        assert_eq!(hits.len(), 1, "upsert must not create a duplicate row");
        assert_eq!(hits[0].cumulative_token_count, 200);
        assert_eq!(hits[0].ttl_tier, TtlTier::OneHour);
    }

    #[test]
    fn transient_connection_errors_are_classified_for_retry() {
        // The three flavors observed in prod (severed idle conn, TLS handshake EOF, Neon auth
        // timeout) must retry; query-level and pool-exhaustion errors must NOT.
        use std::io;
        assert!(is_transient_connection_error(&sqlx::Error::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "expected to read 5 bytes, got 0 bytes at EOF"
        ))));
        assert!(is_transient_connection_error(&sqlx::Error::Protocol("unexpected EOF".into())));
        assert!(!is_transient_connection_error(&sqlx::Error::PoolTimedOut));
        assert!(!is_transient_connection_error(&sqlx::Error::RowNotFound));
    }
}
