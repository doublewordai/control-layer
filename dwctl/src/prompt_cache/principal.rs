//! Resolve a request's (validated) bearer token to its billing principal — the
//! cache scope's `org_id` (= `api_keys.user_id`, the `target_user_id`: org or
//! personal user). Two-tier, per the design:
//!
//! - **L1**: an in-process memo. The `api_key -> user_id` mapping is **immutable**
//!   (a key's `user_id` never changes), so the memo needs no invalidation; a modest
//!   TTL is kept only for memory hygiene. onwards passes only *validated* tokens and
//!   a deleted key fails that validation, so a stale memo entry can never be used.
//! - **Read-through** to the DB on a miss via `get_user_id_by_secret` — one indexed
//!   point lookup. No bulk key sync (that LISTEN/NOTIFY load is what we avoid).
//!
//! Scoping on `user_id` (not the key) is deliberate: all of a customer's modalities
//! (realtime / batch / playground keys) share one `user_id`, so their requests cache
//! against each other; for org keys, `user_id` is the org, giving org-scoped caching.

use std::time::Duration;

use moka::future::Cache;
use sqlx::PgPool;

use crate::db::handlers::api_keys::ApiKeys;
use crate::types::UserId;

use super::index::CacheResult;

/// Resolves bearer tokens to billing principals, backed by an in-process L1 memo
/// over a DB read-through.
#[derive(Clone)]
pub struct PrincipalResolver {
    pool: PgPool,
    l1: Cache<String, Option<UserId>>,
}

impl PrincipalResolver {
    pub fn new(pool: PgPool) -> Self {
        Self::with_capacity(pool, 100_000)
    }

    pub fn with_capacity(pool: PgPool, max_entries: u64) -> Self {
        let l1 = Cache::builder()
            .max_capacity(max_entries)
            // Hygiene only — the mapping is immutable, so correctness doesn't need it.
            .time_to_live(Duration::from_secs(3600))
            .build();
        Self { pool, l1 }
    }

    /// Resolve a validated bearer token to its billing principal. `None` => the token
    /// is not a live key (→ un-scopable → no caching). Hits and misses are both
    /// memoised; both are safe (immutable mapping; secrets are never reused).
    pub async fn resolve(&self, token: &str) -> CacheResult<Option<UserId>> {
        if let Some(cached) = self.l1.get(token).await {
            return Ok(cached);
        }
        let mut conn = self.pool.acquire().await?;
        let user_id = ApiKeys::new(&mut conn).get_user_id_by_secret(token).await?;
        self.l1.insert(token.to_string(), user_id).await;
        Ok(user_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::users::Role;
    use crate::test::utils::{create_test_api_key_for_user, create_test_user};

    #[sqlx::test]
    async fn resolves_validated_key_to_user(pool: PgPool) {
        let user = create_test_user(&pool, Role::StandardUser).await;
        let key = create_test_api_key_for_user(&pool, user.id).await;

        let resolver = PrincipalResolver::new(pool);
        assert_eq!(resolver.resolve(&key.secret).await.unwrap(), Some(user.id));
        // Second call is served from the L1 memo — still correct.
        assert_eq!(resolver.resolve(&key.secret).await.unwrap(), Some(user.id));
    }

    #[sqlx::test]
    async fn unknown_token_resolves_none(pool: PgPool) {
        let resolver = PrincipalResolver::new(pool);
        assert_eq!(resolver.resolve("not-a-real-secret").await.unwrap(), None);
    }
}
