//! Postgres store for the reference cache: `reference_cache_entries`, keyed by
//! `(principal_id, virtual_model, prefix_hash)`.
//!
//! Three statements, each a single round trip whatever the prompt length:
//! [`ReferenceStore::lookup`] reads every live boundary of a request and slides the
//! expiry of the ones it found; [`ReferenceStore::upsert`] makes a request's boundaries
//! visible; [`ReferenceStore::sweep_expired`] deletes expired rows in bounded batches.

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::types::UserId;

/// One live boundary returned by [`ReferenceStore::lookup`].
#[derive(Debug, Clone, PartialEq)]
pub struct LiveEntry {
    pub prefix_hash: Vec<u8>,
    pub cumulative_bytes: i64,
    /// `None` while the writer is still streaming.
    pub tokens_per_byte: Option<f32>,
    /// The writer's exact `prompt_tokens`, when this boundary was its final one.
    pub exact_tokens: Option<i32>,
    pub visible_at: DateTime<Utc>,
}

/// Boundaries to make visible for one request.
pub struct EntryWrite<'a> {
    pub principal_id: UserId,
    pub virtual_model: &'a str,
    pub prefix_hashes: &'a [Vec<u8>],
    pub cumulative_bytes: &'a [i64],
    /// Parallel to `prefix_hashes`; set only on the request's final boundary.
    pub exact_tokens: &'a [Option<i32>],
    /// `None` for the provisional write at first token.
    pub tokens_per_byte: Option<f32>,
}

#[derive(Clone)]
pub struct ReferenceStore {
    /// Live provider (not a pinned pool): survives runtime pool swaps. Always the
    /// primary: the table is unlogged, so a replica has no rows.
    pool: sqlx_pool_router::DynPools,
    ttl: Duration,
}

impl ReferenceStore {
    pub fn new(pool: impl sqlx_pool_router::PoolProvider, ttl: Duration) -> Self {
        Self {
            pool: sqlx_pool_router::DynPools::new(pool),
            ttl,
        }
    }

    /// Every live entry among `prefix_hashes`, and slide their expiry to `now() + ttl`.
    ///
    /// The slide is skipped for an entry that still has more than 90% of its window
    /// left, so a hot prefix (a shared system prompt) is rewritten at most once per
    /// tenth of the TTL instead of on every request. The window therefore lags by at
    /// most 10%: an entry lives between 0.9 and 1.0 TTL after its last use.
    pub async fn lookup(
        &self,
        principal_id: UserId,
        virtual_model: &str,
        prefix_hashes: &[Vec<u8>],
    ) -> Result<Vec<LiveEntry>, sqlx::Error> {
        if prefix_hashes.is_empty() {
            return Ok(Vec::new());
        }
        // The UPDATE runs even though the final SELECT does not reference it: Postgres
        // always executes data-modifying CTEs.
        sqlx::query_as!(
            LiveEntry,
            r#"
            WITH live AS (
                SELECT prefix_hash, cumulative_bytes, tokens_per_byte, exact_tokens, visible_at, expires_at
                FROM reference_cache_entries
                WHERE principal_id = $1 AND virtual_model = $2
                  AND prefix_hash = ANY($3) AND expires_at > now()
            ), refreshed AS (
                UPDATE reference_cache_entries e
                SET expires_at = now() + make_interval(secs => $4::float8)
                FROM live
                WHERE e.principal_id = $1 AND e.virtual_model = $2 AND e.prefix_hash = live.prefix_hash
                  AND live.expires_at < now() + make_interval(secs => $4::float8 * 0.9)
                RETURNING e.prefix_hash
            )
            SELECT
                prefix_hash AS "prefix_hash!",
                cumulative_bytes AS "cumulative_bytes!",
                tokens_per_byte,
                exact_tokens,
                visible_at AS "visible_at!"
            FROM live
            "#,
            principal_id,
            virtual_model,
            prefix_hashes,
            self.ttl.as_secs_f64(),
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Make boundaries visible. Returns the rows inserted or updated.
    ///
    /// A boundary already live is left alone unless this write supplies something it
    /// lacks: the tokens per byte of a writer that was still streaming, or an exact count
    /// on a final boundary. Values already present are never replaced, so the provisional
    /// first-token write and the exact completion write give the same result in either
    /// order. An expired row the sweep has not reached yet is overwritten as if new.
    pub async fn upsert(&self, write: &EntryWrite<'_>) -> Result<u64, sqlx::Error> {
        if write.prefix_hashes.is_empty() {
            return Ok(0);
        }
        // The NOT EXISTS filter keeps rows that need nothing out of ON CONFLICT, which
        // would otherwise lock each of them. A row that turns live between the filter and
        // the insert just takes the CASE branches below.
        let result = sqlx::query!(
            r#"
            INSERT INTO reference_cache_entries AS e
                (principal_id, virtual_model, prefix_hash, cumulative_bytes,
                 tokens_per_byte, exact_tokens, visible_at, expires_at)
            SELECT $1, $2, b.prefix_hash, b.cumulative_bytes, $6::real, b.exact_tokens,
                   now(), now() + make_interval(secs => $7::float8)
            FROM UNNEST($3::bytea[], $4::bigint[], $5::int[]) AS b(prefix_hash, cumulative_bytes, exact_tokens)
            WHERE NOT EXISTS (
                SELECT 1 FROM reference_cache_entries x
                WHERE x.principal_id = $1 AND x.virtual_model = $2 AND x.prefix_hash = b.prefix_hash
                  AND x.expires_at > now()
                  AND (x.tokens_per_byte IS NOT NULL OR $6::real IS NULL)
                  AND (x.exact_tokens IS NOT NULL OR b.exact_tokens IS NULL)
            )
            ON CONFLICT (principal_id, virtual_model, prefix_hash) DO UPDATE SET
                cumulative_bytes = EXCLUDED.cumulative_bytes,
                tokens_per_byte = CASE WHEN e.expires_at <= now() THEN EXCLUDED.tokens_per_byte
                                       ELSE COALESCE(e.tokens_per_byte, EXCLUDED.tokens_per_byte) END,
                exact_tokens    = CASE WHEN e.expires_at <= now() THEN EXCLUDED.exact_tokens
                                       ELSE COALESCE(e.exact_tokens, EXCLUDED.exact_tokens) END,
                visible_at      = CASE WHEN e.expires_at <= now() THEN EXCLUDED.visible_at
                                       ELSE e.visible_at END,
                expires_at      = GREATEST(e.expires_at, EXCLUDED.expires_at)
            "#,
            write.principal_id,
            write.virtual_model,
            write.prefix_hashes,
            write.cumulative_bytes,
            write.exact_tokens as &[Option<i32>],
            write.tokens_per_byte,
            self.ttl.as_secs_f64(),
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Delete up to `batch_size` expired rows. `SKIP LOCKED` keeps the sweep off rows a
    /// request is writing at that moment.
    pub async fn sweep_expired(&self, batch_size: i64) -> Result<u64, sqlx::Error> {
        let result = sqlx::query!(
            r#"
            WITH victims AS (
                SELECT principal_id, virtual_model, prefix_hash
                FROM reference_cache_entries
                WHERE expires_at <= now()
                LIMIT $1
                FOR UPDATE SKIP LOCKED
            )
            DELETE FROM reference_cache_entries e
            USING victims v
            WHERE e.principal_id = v.principal_id AND e.virtual_model = v.virtual_model
              AND e.prefix_hash = v.prefix_hash
            "#,
            batch_size,
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashes(names: &[&str]) -> Vec<Vec<u8>> {
        names.iter().map(|n| n.as_bytes().to_vec()).collect()
    }

    fn write<'a>(
        principal_id: UserId,
        prefix_hashes: &'a [Vec<u8>],
        cumulative_bytes: &'a [i64],
        exact_tokens: &'a [Option<i32>],
        tokens_per_byte: Option<f32>,
    ) -> EntryWrite<'a> {
        EntryWrite {
            principal_id,
            virtual_model: "model-a",
            prefix_hashes,
            cumulative_bytes,
            exact_tokens,
            tokens_per_byte,
        }
    }

    #[dwctl_test_macros::test]
    async fn upsert_then_lookup_returns_only_live_entries_in_scope(pool: sqlx::PgPool) {
        let store = ReferenceStore::new(pool, Duration::from_secs(300));
        let principal = uuid::Uuid::new_v4();
        let hs = hashes(&["a", "ab"]);
        store
            .upsert(&write(principal, &hs, &[10, 20], &[None, Some(7)], Some(0.25)))
            .await
            .unwrap();

        let mut live = store.lookup(principal, "model-a", &hashes(&["a", "ab", "abc"])).await.unwrap();
        live.sort_by_key(|e| e.cumulative_bytes);
        assert_eq!(live.len(), 2);
        assert_eq!((live[0].cumulative_bytes, live[0].exact_tokens), (10, None));
        assert_eq!((live[1].cumulative_bytes, live[1].exact_tokens), (20, Some(7)));
        assert_eq!(live[1].tokens_per_byte, Some(0.25));

        assert!(store.lookup(uuid::Uuid::new_v4(), "model-a", &hs).await.unwrap().is_empty());
        assert!(store.lookup(principal, "model-b", &hs).await.unwrap().is_empty());
    }

    /// The provisional first-token write and the exact completion write land in either
    /// order with the same result, and neither overwrites what the other supplied.
    #[dwctl_test_macros::test]
    async fn provisional_and_exact_writes_commute(pool: sqlx::PgPool) {
        let store = ReferenceStore::new(pool, Duration::from_secs(300));
        let hs = hashes(&["a", "ab"]);
        for provisional_first in [true, false] {
            let principal = uuid::Uuid::new_v4();
            let provisional = write(principal, &hs, &[10, 20], &[None, None], None);
            let exact = write(principal, &hs, &[10, 20], &[None, Some(9)], Some(0.5));
            if provisional_first {
                store.upsert(&provisional).await.unwrap();
                assert_eq!(store.upsert(&exact).await.unwrap(), 2, "both rows are filled in");
            } else {
                store.upsert(&exact).await.unwrap();
                assert_eq!(store.upsert(&provisional).await.unwrap(), 0, "nothing to add");
            }
            let mut live = store.lookup(principal, "model-a", &hs).await.unwrap();
            live.sort_by_key(|e| e.cumulative_bytes);
            assert_eq!(live[0].tokens_per_byte, Some(0.5));
            assert_eq!(live[1].exact_tokens, Some(9));
        }
    }

    /// A later request's exact count does not replace the first writer's, and a live
    /// boundary keeps the time it first became visible.
    #[dwctl_test_macros::test]
    async fn live_entries_keep_their_first_values(pool: sqlx::PgPool) {
        let store = ReferenceStore::new(pool, Duration::from_secs(300));
        let principal = uuid::Uuid::new_v4();
        let hs = hashes(&["a"]);
        store.upsert(&write(principal, &hs, &[10], &[Some(5)], Some(0.5))).await.unwrap();
        let first = store.lookup(principal, "model-a", &hs).await.unwrap().remove(0);

        assert_eq!(store.upsert(&write(principal, &hs, &[10], &[Some(6)], Some(0.6))).await.unwrap(), 0);
        let again = store.lookup(principal, "model-a", &hs).await.unwrap().remove(0);
        assert_eq!(again, first);
    }

    #[dwctl_test_macros::test]
    async fn expired_entries_are_invisible_overwritten_and_swept(pool: sqlx::PgPool) {
        let store = ReferenceStore::new(pool.clone(), Duration::from_secs(300));
        let principal = uuid::Uuid::new_v4();
        let hs = hashes(&["a"]);
        store.upsert(&write(principal, &hs, &[10], &[Some(5)], Some(0.5))).await.unwrap();
        sqlx::query!(
            "UPDATE reference_cache_entries SET expires_at = now() - interval '1 second', visible_at = now() - interval '1 hour' WHERE principal_id = $1",
            principal
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(store.lookup(principal, "model-a", &hs).await.unwrap().is_empty());

        // A new write replaces the expired row wholesale, visibility included.
        store.upsert(&write(principal, &hs, &[10], &[Some(8)], Some(0.8))).await.unwrap();
        let live = store.lookup(principal, "model-a", &hs).await.unwrap();
        assert_eq!(live[0].exact_tokens, Some(8));
        assert!(live[0].visible_at > Utc::now() - chrono::Duration::minutes(1));

        sqlx::query!(
            "UPDATE reference_cache_entries SET expires_at = now() - interval '1 second' WHERE principal_id = $1",
            principal
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(store.sweep_expired(100).await.unwrap(), 1);
        assert_eq!(store.sweep_expired(100).await.unwrap(), 0);
    }

    /// Reading an entry slides its window; a read that would barely move it does not
    /// rewrite the row.
    #[dwctl_test_macros::test]
    async fn lookup_slides_the_window(pool: sqlx::PgPool) {
        let store = ReferenceStore::new(pool.clone(), Duration::from_secs(300));
        let principal = uuid::Uuid::new_v4();
        let hs = hashes(&["a"]);
        store.upsert(&write(principal, &hs, &[10], &[None], Some(0.5))).await.unwrap();
        let expiry = || async {
            sqlx::query_scalar!("SELECT expires_at FROM reference_cache_entries WHERE principal_id = $1", principal)
                .fetch_one(&pool)
                .await
                .unwrap()
        };

        let fresh = expiry().await;
        store.lookup(principal, "model-a", &hs).await.unwrap();
        assert_eq!(expiry().await, fresh, "a fresh entry is not rewritten");

        sqlx::query!(
            "UPDATE reference_cache_entries SET expires_at = now() + interval '10 seconds' WHERE principal_id = $1",
            principal
        )
        .execute(&pool)
        .await
        .unwrap();
        store.lookup(principal, "model-a", &hs).await.unwrap();
        assert!(
            expiry().await > Utc::now() + chrono::Duration::minutes(4),
            "a read slides it to a full window"
        );
    }
}
