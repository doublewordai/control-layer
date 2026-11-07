//! Postgres store for the reference cache: `reference_cache_entries`, keyed by
//! `(principal_id, virtual_model, prefix_hash)`.
//!
//! Three bounded operations:
//! [`ReferenceStore::lookup`] reads every live boundary of a request and slides the
//! expiry of the ones it found; [`ReferenceStore::upsert`] makes a request's boundaries
//! visible; [`ReferenceStore::sweep_expired`] deletes expired rows in bounded batches.
//!
//! Optional cache work uses a separate bounded pool with server-side deadlines. Its
//! connection budget remains enforced even when callers abandon their query futures.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx_pool_router::PoolProvider;

use super::pools::CachePools;
use crate::types::UserId;

/// This is optional shadow telemetry. Do not send unbounded arrays to PostgreSQL for
/// unusually long conversations; leave their estimate unavailable instead.
const MAX_PREFIXES_PER_OPERATION: usize = 1_024;

fn check_prefix_count(count: usize) -> Result<(), sqlx::Error> {
    if count > MAX_PREFIXES_PER_OPERATION {
        return Err(sqlx::Error::Protocol("reference-cache prefix budget exceeded".into()));
    }
    Ok(())
}

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
    pool: CachePools,
    ttl: Duration,
}

impl ReferenceStore {
    pub fn new(pool: impl PoolProvider, ttl: Duration, max_connections: usize) -> Self {
        Self {
            pool: CachePools::new(pool, max_connections),
            ttl,
        }
    }

    pub(super) fn pools(&self) -> CachePools {
        self.pool.clone()
    }

    /// Every entry among `prefix_hashes` that was live when the request arrived, and slide
    /// the expiry of those also visible by then to `now() + ttl`.
    ///
    /// Liveness is judged at `arrival`, not now: a request that arrived inside an entry's
    /// window still uses it when it completes after the window closed (the sweep keeps
    /// expired rows for a grace period for this). Entries that became visible after
    /// `arrival` are returned, so the completion write knows they exist, but not
    /// refreshed: the request cannot use them, so it must not keep them alive either.
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
        arrival: DateTime<Utc>,
    ) -> Result<Vec<LiveEntry>, sqlx::Error> {
        if prefix_hashes.is_empty() {
            return Ok(Vec::new());
        }
        check_prefix_count(prefix_hashes.len())?;
        let mut tx = self.pool.begin().await?;
        // Lock only rows needing refresh, in the same prefix order as upsert. Merely
        // sorting UPDATE's input does not guarantee its lock order: pre-lock explicitly.
        // Postgres executes the modifying CTE even though SELECT does not reference it.
        let live = sqlx::query_as!(
            LiveEntry,
            r#"
            WITH live AS MATERIALIZED (
                SELECT prefix_hash, cumulative_bytes, tokens_per_byte, exact_tokens, visible_at, expires_at
                FROM reference_cache_entries
                WHERE principal_id = $1 AND virtual_model = $2
                  AND prefix_hash = ANY($3) AND expires_at > $5
            ), to_refresh AS MATERIALIZED (
                SELECT e.prefix_hash
                FROM reference_cache_entries e
                JOIN live ON live.prefix_hash = e.prefix_hash
                WHERE e.principal_id = $1 AND e.virtual_model = $2
                  AND live.visible_at <= $5
                  AND e.expires_at < now() + make_interval(secs => $4::float8 * 0.9)
                ORDER BY e.prefix_hash
                FOR UPDATE OF e
            ), refreshed AS (
                UPDATE reference_cache_entries e
                SET expires_at = now() + make_interval(secs => $4::float8)
                FROM to_refresh
                WHERE e.principal_id = $1 AND e.virtual_model = $2 AND e.prefix_hash = to_refresh.prefix_hash
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
            arrival,
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(live)
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
        check_prefix_count(write.prefix_hashes.len())?;
        if write.cumulative_bytes.len() != write.prefix_hashes.len() || write.exact_tokens.len() != write.prefix_hashes.len() {
            return Err(sqlx::Error::Protocol(
                "reference-cache boundary arrays must have equal lengths".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        // The NOT EXISTS filter keeps rows that need nothing out of ON CONFLICT, which
        // would otherwise lock each of them. A row that turns live between the filter and
        // the insert just takes the CASE branches below. Sort the INSERT input so
        // overlapping batches cannot acquire conflicting row locks in opposite orders.
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
            ORDER BY b.prefix_hash
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
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }

    /// Delete up to `batch_size` rows expired for longer than `grace`. The grace keeps a
    /// row for requests that arrived before it expired and complete after (see
    /// [`Self::lookup`]). `SKIP LOCKED` keeps the sweep off rows a request is writing at
    /// that moment.
    pub async fn sweep_expired(&self, batch_size: i64, grace: Duration) -> Result<u64, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query!(
            r#"
            WITH victims AS (
                SELECT principal_id, virtual_model, prefix_hash
                FROM reference_cache_entries
                WHERE expires_at <= now() - make_interval(secs => $2::float8)
                ORDER BY principal_id, virtual_model, prefix_hash
                LIMIT $1
                FOR UPDATE SKIP LOCKED
            )
            DELETE FROM reference_cache_entries e
            USING victims v
            WHERE e.principal_id = v.principal_id AND e.virtual_model = v.virtual_model
              AND e.prefix_hash = v.prefix_hash
            "#,
            batch_size,
            grace.as_secs_f64(),
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashes(names: &[&str]) -> Vec<Vec<u8>> {
        names.iter().map(|n| n.as_bytes().to_vec()).collect()
    }

    #[dwctl_test_macros::test]
    async fn oversized_prefix_batches_do_not_open_a_cache_connection(pool: sqlx::PgPool) {
        let store = ReferenceStore::new(pool, Duration::from_secs(300), 1);
        let principal = uuid::Uuid::new_v4();
        let hs = vec![vec![0]; MAX_PREFIXES_PER_OPERATION + 1];
        let bytes = vec![1; hs.len()];
        let exact = vec![None; hs.len()];
        assert!(store.upsert(&write(principal, &hs, &bytes, &exact, None)).await.is_err());
        assert!(store.lookup(principal, "model-a", &hs, Utc::now()).await.is_err());
        // UNNEST pads short arrays to the longest array's length. Check every input
        // length so a malformed write cannot bypass the prefix budget that way.
        assert!(store.upsert(&write(principal, &hs[..1], &bytes, &exact, None)).await.is_err());
        assert_eq!(store.pool.write().size(), 0);
        store.pool.write().close().await;
    }

    #[dwctl_test_macros::test]
    async fn opposing_upserts_and_refreshes_lock_prefixes_in_the_same_order(pool: sqlx::PgPool) {
        let store = ReferenceStore::new(pool.clone(), Duration::from_secs(300), 2);
        // Make conflicting operations overlap after taking their first row lock. With
        // the old UNNEST order and unordered UPDATE this produces an a->b / b->a cycle.
        sqlx::raw_sql(
            "CREATE FUNCTION slow_reference_update() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN PERFORM pg_sleep(0.03); RETURN NEW; END $$; \
             CREATE TRIGGER slow_reference_update BEFORE UPDATE ON reference_cache_entries \
             FOR EACH ROW EXECUTE FUNCTION slow_reference_update();",
        )
        .execute(&pool)
        .await
        .unwrap();
        let forward = hashes(&["a", "b"]);
        let reverse = hashes(&["b", "a"]);
        // Warm both connections so connection setup cannot serialize the operations.
        let one = store.pool.write().acquire().await.unwrap();
        let two = store.pool.write().acquire().await.unwrap();
        drop((one, two));
        for _ in 0..4 {
            let principal = uuid::Uuid::new_v4();
            store
                .upsert(&write(principal, &forward, &[10, 20], &[None, None], None))
                .await
                .unwrap();
            sqlx::query("UPDATE reference_cache_entries SET expires_at = now() + interval '10 seconds' WHERE principal_id = $1")
                .bind(principal)
                .execute(&pool)
                .await
                .unwrap();
            let input = write(principal, &reverse, &[20, 10], &[Some(20), Some(10)], Some(1.0));
            let (upsert, lookup) = tokio::join!(store.upsert(&input), store.lookup(principal, "model-a", &forward, Utc::now()),);
            upsert.expect("opposing input order must not deadlock the upsert");
            assert_eq!(lookup.expect("expiry refresh must use the same lock order").len(), 2);
            let live = store.lookup(principal, "model-a", &forward, Utc::now()).await.unwrap();
            assert!(
                live.iter()
                    .all(|entry| entry.tokens_per_byte == Some(1.0) && entry.exact_tokens.is_some())
            );
        }
        store.pool.write().close().await;
    }

    #[dwctl_test_macros::test]
    async fn lock_deadline_skips_cache_work_without_changing_main_session_settings(pool: sqlx::PgPool) {
        let store = ReferenceStore::new(pool.clone(), Duration::from_secs(300), 1);
        let principal = uuid::Uuid::new_v4();
        let hs = hashes(&["a"]);
        store.upsert(&write(principal, &hs, &[10], &[None], None)).await.unwrap();
        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM reference_cache_entries WHERE principal_id = $1 FOR UPDATE")
            .bind(principal)
            .execute(&mut *blocker)
            .await
            .unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            store.upsert(&write(principal, &hs, &[10], &[Some(10)], Some(1.0))),
        )
        .await
        .expect("the server lock deadline must stop waiting")
        .unwrap_err();
        assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("55P03"));
        let main_lock_timeout: String = sqlx::query_scalar("SHOW lock_timeout").fetch_one(&mut *blocker).await.unwrap();
        assert_eq!(
            main_lock_timeout, "0",
            "cache session settings must not leak into the application pool"
        );
        blocker.rollback().await.unwrap();
        store.upsert(&write(principal, &hs, &[10], &[Some(10)], Some(1.0))).await.unwrap();
        store.pool.write().close().await;
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
        let store = ReferenceStore::new(pool, Duration::from_secs(300), 2);
        let principal = uuid::Uuid::new_v4();
        let hs = hashes(&["a", "ab"]);
        store
            .upsert(&write(principal, &hs, &[10, 20], &[None, Some(7)], Some(0.25)))
            .await
            .unwrap();

        let mut live = store
            .lookup(principal, "model-a", &hashes(&["a", "ab", "abc"]), Utc::now())
            .await
            .unwrap();
        live.sort_by_key(|e| e.cumulative_bytes);
        assert_eq!(live.len(), 2);
        assert_eq!((live[0].cumulative_bytes, live[0].exact_tokens), (10, None));
        assert_eq!((live[1].cumulative_bytes, live[1].exact_tokens), (20, Some(7)));
        assert_eq!(live[1].tokens_per_byte, Some(0.25));

        assert!(
            store
                .lookup(uuid::Uuid::new_v4(), "model-a", &hs, Utc::now())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(store.lookup(principal, "model-b", &hs, Utc::now()).await.unwrap().is_empty());
    }

    /// The provisional first-token write and the exact completion write land in either
    /// order with the same result, and neither overwrites what the other supplied.
    #[dwctl_test_macros::test]
    async fn provisional_and_exact_writes_commute(pool: sqlx::PgPool) {
        let store = ReferenceStore::new(pool, Duration::from_secs(300), 2);
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
            let mut live = store.lookup(principal, "model-a", &hs, Utc::now()).await.unwrap();
            live.sort_by_key(|e| e.cumulative_bytes);
            assert_eq!(live[0].tokens_per_byte, Some(0.5));
            assert_eq!(live[1].exact_tokens, Some(9));
        }
    }

    /// A later request's exact count does not replace the first writer's, and a live
    /// boundary keeps the time it first became visible.
    #[dwctl_test_macros::test]
    async fn live_entries_keep_their_first_values(pool: sqlx::PgPool) {
        let store = ReferenceStore::new(pool, Duration::from_secs(300), 2);
        let principal = uuid::Uuid::new_v4();
        let hs = hashes(&["a"]);
        store.upsert(&write(principal, &hs, &[10], &[Some(5)], Some(0.5))).await.unwrap();
        let first = store.lookup(principal, "model-a", &hs, Utc::now()).await.unwrap().remove(0);

        assert_eq!(store.upsert(&write(principal, &hs, &[10], &[Some(6)], Some(0.6))).await.unwrap(), 0);
        let again = store.lookup(principal, "model-a", &hs, Utc::now()).await.unwrap().remove(0);
        assert_eq!(again, first);
    }

    #[dwctl_test_macros::test]
    async fn expired_entries_are_invisible_overwritten_and_swept(pool: sqlx::PgPool) {
        let store = ReferenceStore::new(pool.clone(), Duration::from_secs(300), 2);
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
        assert!(store.lookup(principal, "model-a", &hs, Utc::now()).await.unwrap().is_empty());
        let arrived_in_window = Utc::now() - chrono::Duration::seconds(30);
        assert_eq!(
            store.lookup(principal, "model-a", &hs, arrived_in_window).await.unwrap().len(),
            1,
            "a request that arrived before the expiry still finds it at completion"
        );
        assert_eq!(
            store.lookup(principal, "model-a", &hs, Utc::now()).await.unwrap().len(),
            1,
            "and its use at arrival slid the window"
        );
        sqlx::query!(
            "UPDATE reference_cache_entries SET expires_at = now() - interval '1 second' WHERE principal_id = $1",
            principal
        )
        .execute(&pool)
        .await
        .unwrap();

        // A new write replaces the expired row wholesale, visibility included.
        store.upsert(&write(principal, &hs, &[10], &[Some(8)], Some(0.8))).await.unwrap();
        let live = store.lookup(principal, "model-a", &hs, Utc::now()).await.unwrap();
        assert_eq!(live[0].exact_tokens, Some(8));
        assert!(live[0].visible_at > Utc::now() - chrono::Duration::minutes(1));

        sqlx::query!(
            "UPDATE reference_cache_entries SET expires_at = now() - interval '1 second' WHERE principal_id = $1",
            principal
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            store.sweep_expired(100, Duration::from_secs(600)).await.unwrap(),
            0,
            "kept through the grace period"
        );
        assert_eq!(store.sweep_expired(100, Duration::ZERO).await.unwrap(), 1);
        assert_eq!(store.sweep_expired(100, Duration::ZERO).await.unwrap(), 0);
    }

    /// Reading an entry slides its window; a read that would barely move it does not
    /// rewrite the row.
    #[dwctl_test_macros::test]
    async fn lookup_slides_the_window(pool: sqlx::PgPool) {
        let store = ReferenceStore::new(pool.clone(), Duration::from_secs(300), 2);
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
        store.lookup(principal, "model-a", &hs, Utc::now()).await.unwrap();
        assert_eq!(expiry().await, fresh, "a fresh entry is not rewritten");

        sqlx::query!(
            "UPDATE reference_cache_entries SET expires_at = now() + interval '10 seconds' WHERE principal_id = $1",
            principal
        )
        .execute(&pool)
        .await
        .unwrap();
        store.lookup(principal, "model-a", &hs, Utc::now()).await.unwrap();
        assert!(
            expiry().await > Utc::now() + chrono::Duration::minutes(4),
            "a read slides it to a full window"
        );

        sqlx::query!(
            "UPDATE reference_cache_entries SET expires_at = now() + interval '10 seconds' WHERE principal_id = $1",
            principal
        )
        .execute(&pool)
        .await
        .unwrap();
        let early = Utc::now() - chrono::Duration::hours(1);
        let live = store.lookup(principal, "model-a", &hs, early).await.unwrap();
        assert_eq!(live.len(), 1, "an entry visible after arrival is still returned");
        assert!(
            expiry().await < Utc::now() + chrono::Duration::minutes(1),
            "but a request that arrived before it cannot keep it alive"
        );
    }
}
