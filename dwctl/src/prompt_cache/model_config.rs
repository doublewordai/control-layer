//! Per-model cache configuration: the §6.6 enablement gate + the §1 minimum-prefix
//! floor, resolved from a **virtual model** (alias) and cached in-process.
//!
//! Cached like the principal resolver (moka) — model config changes rarely, and a
//! short TTL bounds staleness after an operator flips `cache_pricing_enabled` or
//! edits a tariff. A model with no row, or caching disabled, resolves to disabled.

use std::time::Duration;

use moka::future::Cache;
use sqlx::PgPool;

use super::index::CacheResult;

/// Conservative default floor when a model is enabled but has no tariff row yet.
pub const DEFAULT_MIN_PREFIX_TOKENS: u32 = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCacheConfig {
    /// `deployed_models.cache_pricing_enabled` — when false, the classifier skips this
    /// model entirely (markers accepted but no-op'd: no cache, full price, no error).
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
            // Short — config is mutable (an operator can toggle enablement / tariffs).
            .time_to_live(Duration::from_secs(60))
            .build();
        Self { pool, cache }
    }

    /// Resolve the cache config for `virtual_model` (the `deployed_models.alias`).
    pub async fn resolve(&self, virtual_model: &str) -> CacheResult<ModelCacheConfig> {
        if let Some(c) = self.cache.get(virtual_model).await {
            return Ok(c);
        }

        // An alias may map to >1 deployed_models row (variants sharing a base model):
        // enabled if ANY is enabled; floor = the smallest active tariff min across them
        // (a NULL aggregate over no rows means "no such model").
        let row = sqlx::query!(
            r#"
            SELECT bool_or(dm.cache_pricing_enabled) AS enabled,
                   MIN(mct.min_prefix_tokens)        AS min_prefix
            FROM deployed_models dm
            LEFT JOIN model_cache_tariffs mct
              ON mct.deployed_model_id = dm.id
             AND (mct.valid_until IS NULL OR mct.valid_until > now())
            WHERE dm.alias = $1 AND dm.deleted = false
            "#,
            virtual_model,
        )
        .fetch_one(&self.pool)
        .await?;

        let config = match row.enabled {
            Some(true) => ModelCacheConfig {
                enabled: true,
                min_prefix_tokens: row.min_prefix.map(|m| m.max(0) as u32).unwrap_or(DEFAULT_MIN_PREFIX_TOKENS),
            },
            // No rows (None) or all-disabled (Some(false)) → disabled.
            _ => ModelCacheConfig::DISABLED,
        };

        self.cache.insert(virtual_model.to_string(), config).await;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::utils::{create_test_endpoint, create_test_model, create_test_user};

    #[sqlx::test]
    async fn disabled_by_default_and_unknown_model(pool: PgPool) {
        let user = create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let _ = create_test_model(&pool, "m1", "alias-default", endpoint, user.id).await;

        let r = ModelConfigResolver::new(pool);
        // New model: cache_pricing_enabled defaults to false.
        assert!(!r.resolve("alias-default").await.unwrap().enabled);
        // Unknown alias → disabled.
        assert_eq!(r.resolve("nope").await.unwrap(), ModelCacheConfig::DISABLED);
    }

    #[sqlx::test]
    async fn enabled_with_tariff_floor(pool: PgPool) {
        let user = create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let id = create_test_model(&pool, "m2", "alias-on", endpoint, user.id).await;
        sqlx::query!("UPDATE deployed_models SET cache_pricing_enabled = true WHERE id = $1", id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query!(
            r#"INSERT INTO model_cache_tariffs (deployed_model_id, ttl_tier, write_multiplier, min_prefix_tokens)
               VALUES ($1, '1h', 2.0, 2048)"#,
            id
        )
        .execute(&pool)
        .await
        .unwrap();

        let cfg = ModelConfigResolver::new(pool).resolve("alias-on").await.unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.min_prefix_tokens, 2048);
    }

    #[sqlx::test]
    async fn enabled_without_tariff_uses_default_floor(pool: PgPool) {
        let user = create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let id = create_test_model(&pool, "m3", "alias-nofloor", endpoint, user.id).await;
        sqlx::query!("UPDATE deployed_models SET cache_pricing_enabled = true WHERE id = $1", id)
            .execute(&pool)
            .await
            .unwrap();

        let cfg = ModelConfigResolver::new(pool).resolve("alias-nofloor").await.unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.min_prefix_tokens, DEFAULT_MIN_PREFIX_TOKENS);
    }
}
