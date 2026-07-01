//! Per-model cache configuration: the enablement gate + the minimum-prefix
//! floor, resolved from a **virtual model** (alias) and cached in-process.
//!
//! There is no separate enable flag: a model has caching ON iff it has a
//! `model_cache_tariffs` row valid right now (the ledger row carries the floor and the
//! multipliers, all NOT NULL — so an enabled model can never be partially configured).
//! Cached like the principal resolver (moka) with a short TTL so an operator expiring
//! or inserting a tariff version takes effect within a minute. A model with no active
//! row resolves to disabled (markers accepted but no-op'd: no cache, full price, no error).

use std::time::Duration;

use moka::future::Cache;
use sqlx::PgPool;

use super::index::CacheResult;
use super::metrics as cache_metrics;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCacheConfig {
    /// True iff the model has a cache-tariff row valid now. When false the classifier
    /// skips this model entirely (markers accepted but no-op'd).
    pub enabled: bool,
    /// Minimum cacheable prefix length in tokens; below it the request is processed
    /// without caching (no error) — the same posture as a disabled model.
    pub min_prefix_tokens: u32,
}

impl ModelCacheConfig {
    pub const DISABLED: Self = Self {
        enabled: false,
        min_prefix_tokens: u32::MAX,
    };
}

/// Resolves a virtual model (alias) to its [`ModelCacheConfig`], read-through cached.
#[derive(Clone)]
pub struct ModelConfigResolver {
    pool: PgPool,
    cache: Cache<String, ModelCacheConfig>,
}

impl ModelConfigResolver {
    pub fn new(pool: PgPool) -> Self {
        let cache = Cache::builder()
            .max_capacity(10_000)
            // Short — config is mutable (an operator can expire / insert a tariff version).
            .time_to_live(Duration::from_secs(60))
            .build();
        Self { pool, cache }
    }

    /// Resolve the cache config for `virtual_model` (the `deployed_models.alias`).
    pub async fn resolve(&self, virtual_model: &str) -> CacheResult<ModelCacheConfig> {
        if let Some(c) = self.cache.get(virtual_model).await {
            cache_metrics::record_model_config_resolve("hit");
            return Ok(c);
        }
        cache_metrics::record_model_config_resolve("miss");

        // Caching is ON iff the model has a cache-tariff row valid now. An alias may map
        // to >1 deployed_models row (variants sharing a base model): enabled if ANY has an
        // active row; floor = the smallest active min across them. MIN over no rows is
        // NULL → no active tariff → disabled.
        let row = sqlx::query!(
            r#"
            SELECT MIN(mct.min_prefix_tokens) AS min_prefix
            FROM deployed_models dm
            JOIN model_cache_tariffs mct
              ON mct.deployed_model_id = dm.id
             AND mct.valid_from <= now()
             AND (mct.valid_until IS NULL OR mct.valid_until > now())
            WHERE dm.alias = $1 AND dm.deleted = false
            "#,
            virtual_model,
        )
        .fetch_one(&self.pool)
        .await?;

        let config = match row.min_prefix {
            Some(m) => ModelCacheConfig {
                enabled: true,
                min_prefix_tokens: m.max(0) as u32,
            },
            None => ModelCacheConfig::DISABLED,
        };

        self.cache.insert(virtual_model.to_string(), config).await;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::utils::{create_test_endpoint, create_test_model, create_test_user};

    /// Insert a one-row cache tariff (all tiers present) for a model, optionally expired.
    async fn add_tariff(pool: &PgPool, model_id: uuid::Uuid, min_prefix: i32, expired: bool) {
        sqlx::query!(
            r#"INSERT INTO model_cache_tariffs
                 (deployed_model_id, write_multiplier_5m, write_multiplier_1h, write_multiplier_24h, min_prefix_tokens, valid_until)
               VALUES ($1, 1.25, 2.0, 2.5, $2, CASE WHEN $3 THEN now() - interval '1 hour' ELSE NULL END)"#,
            model_id,
            min_prefix,
            expired,
        )
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    async fn disabled_without_tariff_and_unknown_model(pool: PgPool) {
        let user = create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let _ = create_test_model(&pool, "m1", "alias-default", endpoint, user.id).await;

        let r = ModelConfigResolver::new(pool);
        // No tariff row → disabled.
        assert!(!r.resolve("alias-default").await.unwrap().enabled);
        // Unknown alias → disabled.
        assert_eq!(r.resolve("nope").await.unwrap(), ModelCacheConfig::DISABLED);
    }

    #[sqlx::test]
    async fn active_tariff_enables_with_its_floor(pool: PgPool) {
        let user = create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let id = create_test_model(&pool, "m2", "alias-on", endpoint, user.id).await;
        add_tariff(&pool, id, 2048, false).await;

        let cfg = ModelConfigResolver::new(pool).resolve("alias-on").await.unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.min_prefix_tokens, 2048);
    }

    #[sqlx::test]
    async fn expired_tariff_is_disabled(pool: PgPool) {
        let user = create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let id = create_test_model(&pool, "m3", "alias-expired", endpoint, user.id).await;
        add_tariff(&pool, id, 1024, true).await; // valid_until in the past

        let cfg = ModelConfigResolver::new(pool).resolve("alias-expired").await.unwrap();
        assert!(!cfg.enabled, "an expired tariff version no longer enables caching");
    }
}
