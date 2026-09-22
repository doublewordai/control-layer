//! Database repository for model tariffs.

use crate::{
    db::{
        errors::Result,
        models::tariffs::{ModelTariff, TariffCreateDBRequest, TariffDBResponse},
    },
    types::DeploymentId,
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgConnection;
use tracing::instrument;
use uuid::Uuid;

pub struct Tariffs<'c> {
    db: &'c mut PgConnection,
}

impl<'c> Tariffs<'c> {
    /// Create a new Tariffs repository instance
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    /// Create a new tariff for a deployed model
    #[instrument(skip(self, request), fields(deployed_model_id = %request.deployed_model_id, name = %request.name), err)]
    pub async fn create(&mut self, request: &TariffCreateDBRequest) -> Result<TariffDBResponse> {
        // Convert ApiKeyPurpose enum to string for database
        let purpose_str = request.api_key_purpose.as_ref().map(|p| match p {
            crate::db::models::api_keys::ApiKeyPurpose::Realtime => "realtime",
            crate::db::models::api_keys::ApiKeyPurpose::Batch => "batch",
            crate::db::models::api_keys::ApiKeyPurpose::Playground => "playground",
            crate::db::models::api_keys::ApiKeyPurpose::Platform => "platform",
            crate::db::models::api_keys::ApiKeyPurpose::Continuation => "continuation",
        });

        let tariff = sqlx::query_as!(
            ModelTariff,
            r#"
            INSERT INTO model_tariffs (
                deployed_model_id, name, input_price_per_token, output_price_per_token,
                api_key_purpose, completion_window, valid_from, user_id
            )
            VALUES ($1, $2, $3, $4, $5, $6, COALESCE($7, NOW()), $8)
            RETURNING id, deployed_model_id, name, input_price_per_token, output_price_per_token,
                      valid_from, valid_until, api_key_purpose as "api_key_purpose: _", completion_window, user_id, serving_class
            "#,
            request.deployed_model_id,
            request.name,
            request.input_price_per_token,
            request.output_price_per_token,
            purpose_str,
            request.completion_window,
            request.valid_from,
            request.user_id,
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(tariff)
    }

    /// Get a tariff by ID
    #[instrument(skip(self), err)]
    pub async fn get_by_id(&mut self, id: Uuid) -> Result<Option<TariffDBResponse>> {
        let tariff = sqlx::query_as!(
            ModelTariff,
            r#"
            SELECT id, deployed_model_id, name, input_price_per_token, output_price_per_token,
                   valid_from, valid_until, api_key_purpose as "api_key_purpose: _", completion_window, user_id, serving_class
            FROM model_tariffs
            WHERE id = $1
            "#,
            id
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(tariff)
    }

    /// List the current (active) GENERAL tariffs for a deployed model: the model's
    /// own price, without any organisation's rows.
    #[instrument(skip(self), err)]
    pub async fn list_current_by_model(&mut self, deployed_model_id: DeploymentId) -> Result<Vec<TariffDBResponse>> {
        let tariffs = sqlx::query_as!(
            ModelTariff,
            r#"
            SELECT id, deployed_model_id, name, input_price_per_token, output_price_per_token,
                   valid_from, valid_until, api_key_purpose as "api_key_purpose: _", completion_window, user_id, serving_class
            FROM model_tariffs
            WHERE deployed_model_id = $1 AND (api_key_purpose IS NULL OR api_key_purpose IN ('realtime','batch','playground')) AND valid_from <= NOW() AND (valid_until IS NULL OR valid_until > NOW()) AND user_id IS NULL
            ORDER BY api_key_purpose ASC NULLS LAST, completion_window ASC NULLS LAST, name ASC
            "#,
            deployed_model_id
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(tariffs)
    }

    /// List every current (active) tariff for a deployed model, general and
    /// organisation-scoped alike. Operator views group them by `user_id`.
    #[instrument(skip(self), err)]
    pub async fn list_current_by_model_all_scopes(&mut self, deployed_model_id: DeploymentId) -> Result<Vec<TariffDBResponse>> {
        let tariffs = sqlx::query_as!(
            ModelTariff,
            r#"
            SELECT id, deployed_model_id, name, input_price_per_token, output_price_per_token,
                   valid_from, valid_until, api_key_purpose as "api_key_purpose: _", completion_window, user_id, serving_class
            FROM model_tariffs
            WHERE deployed_model_id = $1 AND (api_key_purpose IS NULL OR api_key_purpose IN ('realtime','batch','playground')) AND valid_from <= NOW() AND (valid_until IS NULL OR valid_until > NOW())
            ORDER BY user_id ASC NULLS FIRST, api_key_purpose ASC NULLS LAST, completion_window ASC NULLS LAST, name ASC
            "#,
            deployed_model_id
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(tariffs)
    }

    /// Load all current price scopes for a model list in one round trip.
    pub async fn list_current_all_scopes_bulk(&mut self, model_ids: &[DeploymentId]) -> Result<Vec<TariffDBResponse>> {
        Ok(sqlx::query_as::<_, ModelTariff>(
            "SELECT id, deployed_model_id, name, input_price_per_token, output_price_per_token,
                    valid_from, valid_until, api_key_purpose, completion_window, user_id, serving_class
             FROM model_tariffs WHERE deployed_model_id = ANY($1)
               AND (api_key_purpose IS NULL OR api_key_purpose IN ('realtime','batch','playground')) AND valid_from <= NOW() AND (valid_until IS NULL OR valid_until > NOW())
             ORDER BY deployed_model_id, user_id NULLS FIRST, api_key_purpose, completion_window, name",
        )
        .bind(model_ids)
        .fetch_all(&mut *self.db)
        .await?)
    }

    /// List one organisation's current (active) tariffs across every model.
    #[instrument(skip(self), err)]
    pub async fn list_current_by_account(&mut self, user_id: Uuid) -> Result<Vec<TariffDBResponse>> {
        let tariffs = sqlx::query_as!(
            ModelTariff,
            r#"
            SELECT id, deployed_model_id, name, input_price_per_token, output_price_per_token,
                   valid_from, valid_until, api_key_purpose as "api_key_purpose: _", completion_window, user_id, serving_class
            FROM model_tariffs
            WHERE user_id = $1 AND (api_key_purpose IS NULL OR api_key_purpose IN ('realtime','batch','playground')) AND valid_from <= NOW() AND (valid_until IS NULL OR valid_until > NOW())
              AND EXISTS (SELECT 1 FROM deployed_models dm WHERE dm.id = model_tariffs.deployed_model_id AND dm.deleted = FALSE)
            ORDER BY deployed_model_id ASC, api_key_purpose ASC NULLS LAST, completion_window ASC NULLS LAST, name ASC
            "#,
            user_id
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(tariffs)
    }

    /// The tariffs a caller billed to `account` effectively pays on these models: the
    /// organisation's active rows where it has them (per purpose and completion window),
    /// the general active rows otherwise. Mirrors the billing assigner's preference.
    #[instrument(skip(self), err)]
    pub async fn list_effective_for_account(
        &mut self,
        deployed_model_ids: &[DeploymentId],
        account: Uuid,
    ) -> Result<Vec<TariffDBResponse>> {
        let tariffs = sqlx::query_as!(
            ModelTariff,
            r#"
            WITH relevant AS (
                SELECT deployed_model_id, api_key_purpose, completion_window, serving_class
                FROM model_tariffs
                WHERE deployed_model_id = ANY($1) AND (user_id IS NULL OR user_id = $2)
                  AND valid_from <= NOW() AND (valid_until IS NULL OR valid_until > NOW())
                  AND api_key_purpose IN ('realtime','batch','playground')
            ), selectors AS (
                SELECT DISTINCT deployed_model_id, api_key_purpose, completion_window FROM relevant
            ), classes AS (
                SELECT DISTINCT deployed_model_id, serving_class FROM relevant
                UNION SELECT DISTINCT deployed_model_id, NULL::text FROM relevant
            )
            SELECT DISTINCT t.id as "id!", t.deployed_model_id as "deployed_model_id!", t.name as "name!",
                   t.input_price_per_token as "input_price_per_token!", t.output_price_per_token as "output_price_per_token!",
                   t.valid_from as "valid_from!", t.valid_until, t.api_key_purpose as "api_key_purpose: _",
                   t.completion_window, t.user_id, t.serving_class
            FROM selectors s JOIN classes c USING (deployed_model_id)
            CROSS JOIN LATERAL effective_model_tariff(s.deployed_model_id, $2, s.api_key_purpose,
                s.completion_window, CASE WHEN s.api_key_purpose = 'batch' THEN 'standard' ELSE c.serving_class END, NOW()) t
            WHERE s.api_key_purpose <> 'batch' OR c.serving_class IS NULL
            ORDER BY "deployed_model_id!", t.serving_class NULLS FIRST, "api_key_purpose: _", t.completion_window
            "#,
            deployed_model_ids,
            account
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(tariffs)
    }

    pub async fn get_effective_pricing_at_timestamp(
        &mut self,
        model: DeploymentId,
        account: Option<Uuid>,
        purpose: &str,
        window: Option<&str>,
        class: Option<&str>,
        timestamp: DateTime<Utc>,
    ) -> Result<Option<(Decimal, Decimal)>> {
        Ok(sqlx::query_as::<_, (Decimal, Decimal)>(
            "SELECT input_price_per_token, output_price_per_token FROM effective_model_tariff($1,$2,$3,$4,$5,$6)",
        )
        .bind(model)
        .bind(account)
        .bind(purpose)
        .bind(window)
        .bind(class)
        .bind(timestamp)
        .fetch_optional(&mut *self.db)
        .await?)
    }

    /// List all tariffs (including historical) for a deployed model
    #[instrument(skip(self), err)]
    pub async fn list_all_by_model(&mut self, deployed_model_id: DeploymentId) -> Result<Vec<TariffDBResponse>> {
        let tariffs = sqlx::query_as!(
            ModelTariff,
            r#"
            SELECT id, deployed_model_id, name, input_price_per_token, output_price_per_token,
                   valid_from, valid_until, api_key_purpose as "api_key_purpose: _", completion_window, user_id, serving_class
            FROM model_tariffs
            WHERE deployed_model_id = $1 AND user_id IS NULL
            ORDER BY valid_from DESC, api_key_purpose ASC NULLS LAST, completion_window ASC NULLS LAST, name ASC
            "#,
            deployed_model_id
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(tariffs)
    }

    /// Compatibility wrapper: playground may fall back to realtime; batch stays
    /// within its purpose and window. All callers share the effective resolver.
    #[instrument(skip(self), err)]
    pub async fn get_pricing_at_timestamp_with_fallback(
        &mut self,
        deployed_model_id: DeploymentId,
        preferred_purpose: Option<&crate::db::models::api_keys::ApiKeyPurpose>,
        fallback_purpose: &crate::db::models::api_keys::ApiKeyPurpose,
        timestamp: DateTime<Utc>,
        completion_window: Option<&str>,
    ) -> Result<Option<(Decimal, Decimal)>> {
        self.get_pricing_at_timestamp(
            deployed_model_id,
            preferred_purpose.unwrap_or(fallback_purpose),
            timestamp,
            completion_window,
        )
        .await
    }

    /// Resolve the general model price at the request's original timestamp.
    #[instrument(skip(self), err)]
    pub async fn get_pricing_at_timestamp(
        &mut self,
        deployed_model_id: DeploymentId,
        api_key_purpose: &crate::db::models::api_keys::ApiKeyPurpose,
        timestamp: DateTime<Utc>,
        completion_window: Option<&str>,
    ) -> Result<Option<(Decimal, Decimal)>> {
        use crate::db::models::api_keys::ApiKeyPurpose;
        let purpose = match api_key_purpose {
            ApiKeyPurpose::Realtime => "realtime",
            ApiKeyPurpose::Batch => "batch",
            ApiKeyPurpose::Playground => "playground",
            ApiKeyPurpose::Platform | ApiKeyPurpose::Continuation => return Ok(None),
        };
        self.get_effective_pricing_at_timestamp(deployed_model_id, None, purpose, completion_window, None, timestamp)
            .await
    }

    /// Close multiple tariffs by setting valid_until to the current time
    /// More efficient than calling close_tariff in a loop
    #[instrument(skip(self), fields(count = ids.len()), err)]
    pub async fn close_tariffs_batch(&mut self, ids: &[Uuid]) -> Result<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query!("UPDATE model_tariffs SET valid_until = NOW() WHERE id = ANY($1)", ids)
            .execute(&mut *self.db)
            .await?;
        Ok(result.rows_affected())
    }

    /// Delete a tariff (hard delete - only use for mistakes, prefer close_tariff for normal operations)
    #[instrument(skip(self), err)]
    pub async fn delete(&mut self, id: Uuid) -> Result<bool> {
        let result = sqlx::query!(
            r#"
            DELETE FROM model_tariffs
            WHERE id = $1
            "#,
            id
        )
        .execute(&mut *self.db)
        .await?;

        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::api_keys::ApiKeyPurpose;
    use crate::types::DeploymentId;
    use rust_decimal::Decimal;
    use sqlx::PgPool;
    use std::str::FromStr;

    #[sqlx::test]
    async fn test_multiple_batch_tariffs_per_sla(pool: PgPool) {
        // Seed the database with test infrastructure
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
            default_models: None,
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        // Create a test user
        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let test_endpoint_id = crate::test::utils::get_test_endpoint_id(&pool).await;

        // Create a test deployment
        let deployment_id = DeploymentId::new_v4();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query!(
            "INSERT INTO deployed_models (id, model_name, alias, hosted_on, created_by) VALUES ($1, 'test-model', 'test-alias', $2, $3)",
            deployment_id,
            test_endpoint_id,
            user.id
        )
        .execute(&mut *tx)
        .await
        .unwrap();

        let mut tariffs = Tariffs::new(&mut tx);

        // Create first batch tariff with 24h SLA
        let tariff_24h = TariffCreateDBRequest {
            deployed_model_id: deployment_id,
            name: "Batch 24h".to_string(),
            input_price_per_token: Decimal::from_str("0.001").unwrap(),
            output_price_per_token: Decimal::from_str("0.002").unwrap(),
            api_key_purpose: Some(ApiKeyPurpose::Batch),
            completion_window: Some("24h".to_string()),
            valid_from: None,
            user_id: None,
        };
        let created_24h = tariffs.create(&tariff_24h).await.unwrap();
        assert_eq!(created_24h.completion_window, Some("24h".to_string()));

        // Create second batch tariff with 1h SLA - should succeed (different completion_window)
        let tariff_1h = TariffCreateDBRequest {
            deployed_model_id: deployment_id,
            name: "Batch 1h".to_string(),
            input_price_per_token: Decimal::from_str("0.002").unwrap(),
            output_price_per_token: Decimal::from_str("0.004").unwrap(),
            api_key_purpose: Some(ApiKeyPurpose::Batch),
            completion_window: Some("1h".to_string()),
            valid_from: None,
            user_id: None,
        };
        let created_1h = tariffs.create(&tariff_1h).await.unwrap();
        assert_eq!(created_1h.completion_window, Some("1h".to_string()));

        // Verify both tariffs exist
        let current_tariffs = tariffs.list_current_by_model(deployment_id).await.unwrap();
        assert_eq!(current_tariffs.len(), 2);

        // Verify we can find each tariff
        let tariff_24h_found = current_tariffs
            .iter()
            .find(|t| t.completion_window == Some("24h".to_string()))
            .unwrap();
        assert_eq!(tariff_24h_found.name, "Batch 24h");

        let tariff_1h_found = current_tariffs
            .iter()
            .find(|t| t.completion_window == Some("1h".to_string()))
            .unwrap();
        assert_eq!(tariff_1h_found.name, "Batch 1h");
    }

    #[sqlx::test]
    async fn test_duplicate_batch_tariff_same_sla_rejected(pool: PgPool) {
        // Seed the database with test infrastructure
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
            default_models: None,
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        // Create a test user
        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let test_endpoint_id = crate::test::utils::get_test_endpoint_id(&pool).await;

        // Create a test deployment
        let deployment_id = DeploymentId::new_v4();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query!(
            "INSERT INTO deployed_models (id, model_name, alias, hosted_on, created_by) VALUES ($1, 'test-model', 'test-alias', $2, $3)",
            deployment_id,
            test_endpoint_id,
            user.id
        )
        .execute(&mut *tx)
        .await
        .unwrap();

        let mut tariffs = Tariffs::new(&mut tx);

        // Create first batch tariff with 24h SLA
        let tariff_24h = TariffCreateDBRequest {
            deployed_model_id: deployment_id,
            name: "Batch 24h".to_string(),
            input_price_per_token: Decimal::from_str("0.001").unwrap(),
            output_price_per_token: Decimal::from_str("0.002").unwrap(),
            api_key_purpose: Some(ApiKeyPurpose::Batch),
            completion_window: Some("24h".to_string()),
            valid_from: None,
            user_id: None,
        };
        tariffs.create(&tariff_24h).await.unwrap();

        // Try to create duplicate batch tariff with same 24h SLA - should fail
        let duplicate_tariff = TariffCreateDBRequest {
            deployed_model_id: deployment_id,
            name: "Batch 24h Duplicate".to_string(),
            input_price_per_token: Decimal::from_str("0.003").unwrap(),
            output_price_per_token: Decimal::from_str("0.006").unwrap(),
            api_key_purpose: Some(ApiKeyPurpose::Batch),
            completion_window: Some("24h".to_string()),
            valid_from: None,
            user_id: None,
        };
        let result = tariffs.create(&duplicate_tariff).await;
        assert!(result.is_err(), "Should not allow duplicate batch tariff with same SLA");
    }

    #[sqlx::test]
    async fn test_single_realtime_tariff_still_enforced(pool: PgPool) {
        // Seed the database with test infrastructure
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
            default_models: None,
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        // Create a test user
        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let test_endpoint_id = crate::test::utils::get_test_endpoint_id(&pool).await;

        // Create a test deployment
        let deployment_id = DeploymentId::new_v4();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query!(
            "INSERT INTO deployed_models (id, model_name, alias, hosted_on, created_by) VALUES ($1, 'test-model', 'test-alias', $2, $3)",
            deployment_id,
            test_endpoint_id,
            user.id
        )
        .execute(&mut *tx)
        .await
        .unwrap();

        let mut tariffs = Tariffs::new(&mut tx);

        // Create realtime tariff
        let realtime_tariff = TariffCreateDBRequest {
            deployed_model_id: deployment_id,
            name: "Realtime".to_string(),
            input_price_per_token: Decimal::from_str("0.001").unwrap(),
            output_price_per_token: Decimal::from_str("0.002").unwrap(),
            api_key_purpose: Some(ApiKeyPurpose::Realtime),
            completion_window: None,
            valid_from: None,
            user_id: None,
        };
        tariffs.create(&realtime_tariff).await.unwrap();

        // Try to create duplicate realtime tariff - should fail
        let duplicate_realtime = TariffCreateDBRequest {
            deployed_model_id: deployment_id,
            name: "Realtime 2".to_string(),
            input_price_per_token: Decimal::from_str("0.003").unwrap(),
            output_price_per_token: Decimal::from_str("0.006").unwrap(),
            api_key_purpose: Some(ApiKeyPurpose::Realtime),
            completion_window: None,
            valid_from: None,
            user_id: None,
        };
        let result = tariffs.create(&duplicate_realtime).await;
        assert!(result.is_err(), "Should still enforce single realtime tariff per model");
    }

    #[sqlx::test]
    async fn test_batch_tariff_without_completion_window_rejected(pool: PgPool) {
        // Seed the database with test infrastructure
        let base_url = url::Url::parse("http://localhost:8080").unwrap();
        let sources = vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: base_url.clone(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(3600),
            default_models: None,
        }];
        crate::seed_database(&sources, &pool).await.unwrap();

        // Create a test user
        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let test_endpoint_id = crate::test::utils::get_test_endpoint_id(&pool).await;

        // Create a test deployment
        let deployment_id = DeploymentId::new_v4();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query!(
            "INSERT INTO deployed_models (id, model_name, alias, hosted_on, created_by) VALUES ($1, 'test-model', 'test-alias', $2, $3)",
            deployment_id,
            test_endpoint_id,
            user.id
        )
        .execute(&mut *tx)
        .await
        .unwrap();

        let mut tariffs = Tariffs::new(&mut tx);

        // Try to create batch tariff without completion_window - should fail
        let batch_without_sla = TariffCreateDBRequest {
            deployed_model_id: deployment_id,
            name: "Batch No SLA".to_string(),
            input_price_per_token: Decimal::from_str("0.001").unwrap(),
            output_price_per_token: Decimal::from_str("0.002").unwrap(),
            api_key_purpose: Some(ApiKeyPurpose::Batch),
            completion_window: None, // This should be rejected by CHECK constraint
            valid_from: None,
            user_id: None,
        };
        let result = tariffs.create(&batch_without_sla).await;
        assert!(result.is_err(), "Should not allow batch tariff without completion_window");

        // Verify error is due to constraint violation
        if let Err(e) = result {
            let error_msg = format!("{:?}", e);
            assert!(
                error_msg.contains("batch_tariffs_must_have_completion_window") || error_msg.contains("constraint"),
                "Error should be due to CHECK constraint violation, got: {}",
                error_msg
            );
        }
    }
}
