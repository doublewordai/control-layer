//! Database repository for cache-pricing tariffs (the `model_cache_tariffs` ledger).
//!
//! Enabling caching on a model == inserting a tariff row for it (its presence is the
//! enable gate; see `prompt_cache::model_config`). The row holds all three TTL tiers, so
//! it is complete by construction. Any field the caller doesn't supply is filled from the
//! global defaults in [`crate::config::CachePricingConfig`], so "enable caching on this
//! model" is a one-liner. The table is an append-only ledger: changing pricing expires the
//! current version and inserts a new one; disabling expires it with no successor. Billing
//! resolves the version valid as of inference time, so history is never lost.

use crate::config::CachePricingConfig;
use crate::db::errors::Result;
use crate::types::DeploymentId;
use rust_decimal::Decimal;
use sqlx::{Connection, PgConnection};
use tracing::instrument;

/// Optional per-tier overrides for [`CacheTariffs::enable`]. Any `None` field is filled
/// from the global [`CachePricingConfig`] defaults.
#[derive(Debug, Default, Clone)]
pub struct CacheTariffOverrides {
    pub write_multiplier_5m: Option<Decimal>,
    pub write_multiplier_1h: Option<Decimal>,
    pub write_multiplier_24h: Option<Decimal>,
    pub read_multiplier: Option<Decimal>,
    pub min_prefix_tokens: Option<i32>,
}

pub struct CacheTariffs<'c> {
    db: &'c mut PgConnection,
}

impl<'c> CacheTariffs<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    /// Enable (or re-price) caching for a model: expire the current active version, then
    /// insert a new one from `defaults` + `overrides`. Idempotent in spirit — calling it
    /// again just supersedes the previous version, keeping the old one for audit.
    #[instrument(skip(self, defaults, overrides), fields(deployed_model_id = %model_id), err)]
    pub async fn enable(&mut self, model_id: DeploymentId, defaults: &CachePricingConfig, overrides: CacheTariffOverrides) -> Result<()> {
        // Atomic: expire the active version and insert the new one in one transaction, so a
        // failed insert can't leave the model unintentionally disabled (ledger: never edit a
        // version in place). Two concurrent enables can't both land an active row — the
        // `idx_model_cache_tariffs_unique_active` partial unique index (migration 104) fails
        // the loser's INSERT; the transaction just keeps each enable all-or-nothing.
        let mut tx = self.db.begin().await?;

        // Expire the version that is active *now* — same as-of-now predicate the resolver uses,
        // so a future-dated version (valid_from > now()) isn't expired before it takes effect.
        sqlx::query!(
            r#"UPDATE model_cache_tariffs SET valid_until = now()
               WHERE deployed_model_id = $1
                 AND valid_from <= now()
                 AND (valid_until IS NULL OR valid_until > now())"#,
            model_id,
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query!(
            r#"INSERT INTO model_cache_tariffs
                 (deployed_model_id, write_multiplier_5m, write_multiplier_1h, write_multiplier_24h,
                  read_multiplier, min_prefix_tokens)
               VALUES ($1, $2, $3, $4, $5, $6)"#,
            model_id,
            overrides.write_multiplier_5m.unwrap_or(defaults.default_write_multiplier_5m),
            overrides.write_multiplier_1h.unwrap_or(defaults.default_write_multiplier_1h),
            overrides.write_multiplier_24h.unwrap_or(defaults.default_write_multiplier_24h),
            overrides.read_multiplier.unwrap_or(defaults.default_read_multiplier),
            overrides.min_prefix_tokens.unwrap_or(defaults.default_min_prefix_tokens),
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Disable caching for a model: expire its active tariff version (config retained for
    /// audit). Returns true if a version was active. Caching turns off within the
    /// resolver's cache TTL.
    #[instrument(skip(self), fields(deployed_model_id = %model_id), err)]
    pub async fn disable(&mut self, model_id: DeploymentId) -> Result<bool> {
        // Only expire the currently-effective version (valid_from <= now()), matching the
        // resolver's as-of-now active check — a future-dated version is left intact.
        let res = sqlx::query!(
            r#"UPDATE model_cache_tariffs SET valid_until = now()
               WHERE deployed_model_id = $1
                 AND valid_from <= now()
                 AND (valid_until IS NULL OR valid_until > now())"#,
            model_id,
        )
        .execute(&mut *self.db)
        .await?;
        Ok(res.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt_cache::ModelConfigResolver;
    use crate::test::utils::{create_test_endpoint, create_test_model, create_test_user};
    use sqlx::PgPool;

    #[sqlx::test]
    async fn enable_then_disable_toggles_resolver(pool: PgPool) {
        let user = create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let id = create_test_model(&pool, "m", "enable-alias", endpoint, user.id).await;
        let defaults = CachePricingConfig::default();

        // Before enabling: disabled.
        let resolver = ModelConfigResolver::new(pool.clone());
        assert!(!resolver.resolve("enable-alias").await.unwrap().enabled);

        // Enable with defaults → a fresh resolver (no cache) sees it on, with the default floor.
        {
            let mut conn = pool.acquire().await.unwrap();
            CacheTariffs::new(&mut conn)
                .enable(id, &defaults, CacheTariffOverrides::default())
                .await
                .unwrap();
        }
        let cfg = ModelConfigResolver::new(pool.clone()).resolve("enable-alias").await.unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.min_prefix_tokens, defaults.default_min_prefix_tokens.max(0) as u32);

        // Disable → expires the active version → disabled again.
        {
            let mut conn = pool.acquire().await.unwrap();
            assert!(CacheTariffs::new(&mut conn).disable(id).await.unwrap());
        }
        assert!(!ModelConfigResolver::new(pool).resolve("enable-alias").await.unwrap().enabled);
    }

    #[sqlx::test]
    async fn enable_twice_supersedes_keeping_history(pool: PgPool) {
        let user = create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let id = create_test_model(&pool, "m", "reprice-alias", endpoint, user.id).await;
        let defaults = CachePricingConfig::default();

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = CacheTariffs::new(&mut conn);
        repo.enable(id, &defaults, CacheTariffOverrides::default()).await.unwrap();
        repo.enable(
            id,
            &defaults,
            CacheTariffOverrides {
                min_prefix_tokens: Some(2048),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Two rows total (history retained); exactly one currently active.
        let total = sqlx::query_scalar!("SELECT COUNT(*) FROM model_cache_tariffs WHERE deployed_model_id = $1", id)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        let active = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM model_cache_tariffs WHERE deployed_model_id = $1 AND valid_until IS NULL",
            id
        )
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        assert_eq!(total, Some(2), "old version retained for audit");
        assert_eq!(active, Some(1), "exactly one active version");
    }

    #[sqlx::test]
    async fn partial_unique_index_rejects_two_active_versions(pool: PgPool) {
        let user = create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let endpoint = create_test_endpoint(&pool, "ep", user.id).await;
        let id = create_test_model(&pool, "m", "dup-active-alias", endpoint, user.id).await;

        const INSERT_ACTIVE: &str = "INSERT INTO model_cache_tariffs
               (deployed_model_id, write_multiplier_5m, write_multiplier_1h, write_multiplier_24h, min_prefix_tokens)
             VALUES ($1, 1.25, 2.0, 2.5, 1024)";

        // First active row is fine.
        sqlx::query(INSERT_ACTIVE)
            .bind(id)
            .execute(&pool)
            .await
            .expect("first active row inserts");

        // A second active row (valid_until NULL) for the same model must violate the partial
        // unique index — the backstop that stops two concurrent enable()s double-activating.
        let err = sqlx::query(INSERT_ACTIVE)
            .bind(id)
            .execute(&pool)
            .await
            .expect_err("second active row must be rejected");
        assert!(
            err.as_database_error().is_some_and(|e| e.is_unique_violation()),
            "expected a unique violation from idx_model_cache_tariffs_unique_active, got: {err:?}"
        );
    }
}
