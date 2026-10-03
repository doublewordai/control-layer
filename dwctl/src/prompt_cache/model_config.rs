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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ModelConfigKey {
    Alias(String),
    Class(uuid::Uuid, uuid::Uuid),
}

/// Resolves a virtual model (alias) to its [`ModelCacheConfig`], read-through cached.
#[derive(Clone)]
pub struct ModelConfigResolver {
    /// Live provider (not a pinned pool): survives runtime pool swaps.
    pool: sqlx_pool_router::DynPools,
    cache: Cache<ModelConfigKey, ModelCacheConfig>,
}

impl ModelConfigResolver {
    pub fn new(pool: impl sqlx_pool_router::PoolProvider) -> Self {
        let cache = Cache::builder()
            .max_capacity(10_000)
            // Short — config is mutable (an operator can expire / insert a tariff version).
            .time_to_live(Duration::from_secs(60))
            .build();
        Self {
            pool: sqlx_pool_router::DynPools::new(pool),
            cache,
        }
    }

    /// An active public all-class row enables caching. A public class row may
    /// override its floor; class/account rows cannot independently enable it.
    pub async fn resolve_class(&self, class: &onwards::serving::ClassRouteIdentity) -> CacheResult<ModelCacheConfig> {
        let key = ModelConfigKey::Class(class.model_id, class.class_id);
        if let Some(c) = self.cache.get(&key).await {
            cache_metrics::record_model_config_resolve("hit");
            return Ok(c);
        }
        cache_metrics::record_model_config_resolve("miss");
        let floor = sqlx::query_scalar!(
            r#"SELECT min_prefix_tokens FROM model_cache_tariffs
               WHERE deployed_model_id=$1 AND user_id IS NULL
                 AND (serving_class=$2 OR serving_class IS NULL)
                 AND valid_from<=now() AND (valid_until IS NULL OR valid_until>now())
                 AND EXISTS (
                     SELECT 1 FROM model_cache_tariffs general
                     WHERE general.deployed_model_id=$1 AND general.user_id IS NULL
                       AND general.serving_class IS NULL AND general.valid_from<=now()
                       AND (general.valid_until IS NULL OR general.valid_until>now())
                 )
               ORDER BY (serving_class IS NOT NULL) DESC, valid_from DESC LIMIT 1"#,
            class.model_id,
            class.class_key,
        )
        .fetch_optional(&self.pool)
        .await?;
        let config = floor.map_or(ModelCacheConfig::DISABLED, |floor| ModelCacheConfig {
            enabled: true,
            min_prefix_tokens: floor.max(0) as u32,
        });
        self.cache.insert(key, config).await;
        Ok(config)
    }

    /// Resolve the cache config for `virtual_model` (the `deployed_models.alias`).
    pub async fn resolve(&self, virtual_model: &str) -> CacheResult<ModelCacheConfig> {
        let key = ModelConfigKey::Alias(virtual_model.to_owned());
        if let Some(c) = self.cache.get(&key).await {
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
             AND mct.user_id IS NULL AND mct.serving_class IS NULL
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

        self.cache.insert(key, config).await;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::utils::{create_test_endpoint, create_test_model, create_test_user};

    /// Insert a one-row cache tariff (all tiers present) for a model, optionally expired.
    async fn add_tariff(pool: &sqlx::PgPool, model_id: uuid::Uuid, min_prefix: i32, expired: bool) {
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

    #[dwctl_test_macros::test]
    async fn disabled_without_tariff_and_unknown_model(pool: sqlx::PgPool) {
        let user = create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let _ = create_test_model(&pool, "m1", "alias-default", endpoint, user.id).await;

        let r = ModelConfigResolver::new(pool);
        // No tariff row → disabled.
        assert!(!r.resolve("alias-default").await.unwrap().enabled);
        // Unknown alias → disabled.
        assert_eq!(r.resolve("nope").await.unwrap(), ModelCacheConfig::DISABLED);
    }

    #[dwctl_test_macros::test]
    async fn active_tariff_enables_with_its_floor(pool: sqlx::PgPool) {
        let user = create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let id = create_test_model(&pool, "m2", "alias-on", endpoint, user.id).await;
        add_tariff(&pool, id, 2048, false).await;

        let cfg = ModelConfigResolver::new(pool).resolve("alias-on").await.unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.min_prefix_tokens, 2048);
    }

    #[dwctl_test_macros::test]
    async fn expired_tariff_is_disabled(pool: sqlx::PgPool) {
        let user = create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let id = create_test_model(&pool, "m3", "alias-expired", endpoint, user.id).await;
        add_tariff(&pool, id, 1024, true).await; // valid_until in the past

        let cfg = ModelConfigResolver::new(pool).resolve("alias-expired").await.unwrap();
        assert!(!cfg.enabled, "an expired tariff version no longer enables caching");
    }
    #[dwctl_test_macros::test]
    async fn alias_and_class_configuration_cache_keys_cannot_collide(pool: sqlx::PgPool) {
        let user = create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let endpoint = create_test_endpoint(&pool, "gateway", user.id).await;
        let model_id = create_test_model(&pool, "model", "example/model", endpoint, user.id).await;
        add_tariff(&pool, model_id, 1024, false).await;
        let class = onwards::serving::ClassRouteIdentity {
            model_id,
            class_id: uuid::Uuid::new_v4(),
            canonical_alias: "example/model".into(),
            class_key: "fast".into(),
            endpoint_id: endpoint,
            upstream_model_name: "gateway/fast".into(),
        };
        let collision = format!("class:{}:{}", model_id, class.class_id);
        create_test_model(&pool, "other", &collision, endpoint, user.id).await;
        let resolver = ModelConfigResolver::new(pool);
        assert!(resolver.resolve_class(&class).await.unwrap().enabled);
        assert_eq!(resolver.resolve(&collision).await.unwrap(), ModelCacheConfig::DISABLED);
        assert!(resolver.resolve_class(&class).await.unwrap().enabled);
    }
    #[dwctl_test_macros::test]
    async fn class_cache_overrides_require_active_general_enablement(pool: sqlx::PgPool) {
        use crate::db::handlers::model_class_routes::ModelClassRoutes;
        let user = create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let endpoint = create_test_endpoint(&pool, "gateway", user.id).await;
        let model_id = create_test_model(&pool, "model", "example/model", endpoint, user.id).await;
        let class_id = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO model_serving_classes (id,deployed_model_id,class_key,display_name,inference_endpoint_id,upstream_model_name) VALUES ($1,$2,'fast','Fast',$3,'gateway/fast')")
            .bind(class_id).bind(model_id).bind(endpoint).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO model_cache_tariffs (deployed_model_id,serving_class,write_multiplier_5m,write_multiplier_1h,write_multiplier_24h,min_prefix_tokens) VALUES ($1,'fast',1,1,1,4096)")
            .bind(model_id).execute(&pool).await.unwrap();
        let class = onwards::serving::ClassRouteIdentity {
            model_id,
            class_id,
            canonical_alias: "example/model".into(),
            class_key: "fast".into(),
            endpoint_id: endpoint,
            upstream_model_name: "gateway/fast".into(),
        };
        for (start, end, enabled) in [
            (None, None, false),
            (Some("-2 hours"), Some("-1 hour"), false),
            (Some("1 hour"), None, false),
            (Some("-1 hour"), Some("1 hour"), true),
            (Some("-2 hours"), Some("-1 hour"), false),
        ] {
            if let Some(start) = start {
                sqlx::query("DELETE FROM model_cache_tariffs WHERE deployed_model_id=$1 AND serving_class IS NULL")
                    .bind(model_id)
                    .execute(&pool)
                    .await
                    .unwrap();
                sqlx::query("INSERT INTO model_cache_tariffs (deployed_model_id,write_multiplier_5m,write_multiplier_1h,write_multiplier_24h,min_prefix_tokens,valid_from,valid_until) VALUES ($1,1,1,1,1024,now()+$2::interval,now()+$3::interval)")
                    .bind(model_id).bind(start).bind(end).execute(&pool).await.unwrap();
            }
            let config = ModelConfigResolver::new(pool.clone()).resolve_class(&class).await.unwrap();
            assert_eq!(config.enabled, enabled);
            if enabled {
                assert_eq!(config.min_prefix_tokens, 4096);
            }
            let mut conn = pool.acquire().await.unwrap();
            let views = ModelClassRoutes::new(&mut conn).list_for_models(&[model_id]).await.unwrap();
            assert_eq!(views[0].cache_pricing.enabled, enabled);
            assert_eq!(views[0].cache_pricing.min_prefix_tokens, enabled.then_some(4096));
        }
    }
}
