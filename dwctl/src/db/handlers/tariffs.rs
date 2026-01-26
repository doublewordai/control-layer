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
        });

        let tariff = sqlx::query_as!(
            ModelTariff,
            r#"
            INSERT INTO model_tariffs (
                deployed_model_id, name, input_price_per_token, output_price_per_token,
                api_key_purpose, completion_window, valid_from
            )
            VALUES ($1, $2, $3, $4, $5, $6, COALESCE($7, NOW()))
            RETURNING id, deployed_model_id, name, input_price_per_token, output_price_per_token,
                      valid_from, valid_until, api_key_purpose as "api_key_purpose: _", completion_window
            "#,
            request.deployed_model_id,
            request.name,
            request.input_price_per_token,
            request.output_price_per_token,
            purpose_str,
            request.completion_window,
            request.valid_from,
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
                   valid_from, valid_until, api_key_purpose as "api_key_purpose: _", completion_window
            FROM model_tariffs
            WHERE id = $1
            "#,
            id
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(tariff)
    }

    /// List all current (active) tariffs for a deployed model
    #[instrument(skip(self), err)]
    pub async fn list_current_by_model(&mut self, deployed_model_id: DeploymentId) -> Result<Vec<TariffDBResponse>> {
        let tariffs = sqlx::query_as!(
            ModelTariff,
            r#"
            SELECT id, deployed_model_id, name, input_price_per_token, output_price_per_token,
                   valid_from, valid_until, api_key_purpose as "api_key_purpose: _", completion_window
            FROM model_tariffs
            WHERE deployed_model_id = $1 AND valid_until IS NULL
            ORDER BY api_key_purpose ASC NULLS LAST, completion_window ASC NULLS LAST, name ASC
            "#,
            deployed_model_id
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(tariffs)
    }

    /// List all tariffs (including historical) for a deployed model
    #[instrument(skip(self), err)]
    pub async fn list_all_by_model(&mut self, deployed_model_id: DeploymentId) -> Result<Vec<TariffDBResponse>> {
        let tariffs = sqlx::query_as!(
            ModelTariff,
            r#"
            SELECT id, deployed_model_id, name, input_price_per_token, output_price_per_token,
                   valid_from, valid_until, api_key_purpose as "api_key_purpose: _", completion_window
            FROM model_tariffs
            WHERE deployed_model_id = $1
            ORDER BY valid_from DESC, api_key_purpose ASC NULLS LAST, completion_window ASC NULLS LAST, name ASC
            "#,
            deployed_model_id
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(tariffs)
    }

    /// Get pricing with fallback support
    ///
    /// Tries to get pricing for the preferred API key purpose, falling back to the
    /// fallback purpose if the preferred one is not found.
    ///
    /// # Arguments
    /// * `deployed_model_id` - The model to get pricing for
    /// * `preferred_purpose` - Optional preferred API key purpose
    /// * `fallback_purpose` - Fallback API key purpose to use if preferred is not found
    /// * `timestamp` - The timestamp to get pricing for (for historical accuracy)
    ///
    /// # Returns
    /// * `Ok(Some((input_price, output_price)))` - Found pricing
    /// * `Ok(None)` - Neither preferred nor fallback tariff found
    #[instrument(skip(self), err)]
    pub async fn get_pricing_at_timestamp_with_fallback(
        &mut self,
        deployed_model_id: DeploymentId,
        preferred_purpose: Option<&crate::db::models::api_keys::ApiKeyPurpose>,
        fallback_purpose: &crate::db::models::api_keys::ApiKeyPurpose,
        timestamp: DateTime<Utc>,
        completion_window: Option<&str>,
    ) -> Result<Option<(Decimal, Decimal)>> {
        // Try preferred purpose first if specified
        if let Some(preferred) = preferred_purpose
            && let Some(pricing) = self
                .get_pricing_at_timestamp(deployed_model_id, preferred, timestamp, completion_window)
                .await?
        {
            return Ok(Some(pricing));
        }

        // Fall back to fallback purpose (completion_window not relevant for fallback)
        self.get_pricing_at_timestamp(deployed_model_id, fallback_purpose, timestamp, None)
            .await
    }

    /// Get the pricing for a specific API key purpose that was valid at a given timestamp
    /// This is used for historical chargeback calculations
    ///
    /// For batch tariffs, optionally filters by completion_window (SLA) to match the specific
    /// batch pricing tier.
    ///
    /// Uses an optimized two-step lookup:
    /// 1. First checks the current (active) tariff (WHERE valid_until IS NULL)
    /// 2. If the timestamp is >= current tariff's valid_from, uses it (fast path)
    /// 3. Otherwise, does a full historical lookup with temporal constraints
    #[instrument(skip(self), err)]
    pub async fn get_pricing_at_timestamp(
        &mut self,
        deployed_model_id: DeploymentId,
        api_key_purpose: &crate::db::models::api_keys::ApiKeyPurpose,
        timestamp: DateTime<Utc>,
        completion_window: Option<&str>,
    ) -> Result<Option<(Decimal, Decimal)>> {
        // Convert enum to string for database query
        let purpose_str = match api_key_purpose {
            crate::db::models::api_keys::ApiKeyPurpose::Realtime => "realtime",
            crate::db::models::api_keys::ApiKeyPurpose::Batch => "batch",
            crate::db::models::api_keys::ApiKeyPurpose::Playground => "playground",
            crate::db::models::api_keys::ApiKeyPurpose::Platform => "platform",
        };

        // Step 1: Check current (active) tariff - this is the fast path for recent requests
        // For batch tariffs with completion_window specified, filter by it
        let current_tariff = sqlx::query!(
            r#"
            SELECT input_price_per_token, output_price_per_token, valid_from
            FROM model_tariffs
            WHERE deployed_model_id = $1
              AND api_key_purpose = $2
              AND valid_until IS NULL
              AND ($3::VARCHAR IS NULL OR completion_window = $3 OR api_key_purpose != 'batch')
            LIMIT 1
            "#,
            deployed_model_id,
            purpose_str,
            completion_window
        )
        .fetch_optional(&mut *self.db)
        .await?;

        // Step 2: If current tariff exists and request is after its valid_from, use it
        if let Some(current) = current_tariff
            && timestamp >= current.valid_from
        {
            // Fast path - use current pricing
            return Ok(Some((current.input_price_per_token, current.output_price_per_token)));
        }

        // Step 3: Either no current tariff exists, or request is older than current tariff
        // Do full historical lookup
        let result = sqlx::query!(
            r#"
            SELECT input_price_per_token, output_price_per_token
            FROM model_tariffs
            WHERE deployed_model_id = $1
              AND api_key_purpose = $2
              AND valid_from <= $3
              AND (valid_until IS NULL OR valid_until > $3)
              AND ($4::VARCHAR IS NULL OR completion_window = $4 OR api_key_purpose != 'batch')
            ORDER BY valid_from DESC
            LIMIT 1
            "#,
            deployed_model_id,
            purpose_str,
            timestamp,
            completion_window
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(result.map(|r| (r.input_price_per_token, r.output_price_per_token)))
    }

    /// Get bulk pricing for multiple deployments at once.
    ///
    /// This is an optimization to avoid N+1 queries when computing cost estimates
    /// for multiple files/deployments. It fetches current batch tariffs for all
    /// provided deployment IDs, with optional fallback to realtime pricing.
    ///
    /// Returns a map of deployment_id -> (input_price, output_price)
    #[instrument(skip(self, deployment_ids), fields(deployment_count = deployment_ids.len()), err)]
    pub async fn get_bulk_pricing_for_deployments(
        &mut self,
        deployment_ids: &[DeploymentId],
        completion_window: Option<&str>,
    ) -> Result<std::collections::HashMap<DeploymentId, (Decimal, Decimal)>> {
        use std::collections::HashMap;

        if deployment_ids.is_empty() {
            return Ok(HashMap::new());
        }

        // Fetch current batch tariffs for all deployments in one query
        // Uses window function to rank tariffs and pick the best match:
        // 1. Prefer batch tariffs with matching completion_window
        // 2. Fall back to batch tariffs without completion_window
        // 3. Fall back to realtime tariffs
        let records = sqlx::query!(
            r#"
            WITH ranked_tariffs AS (
                SELECT
                    deployed_model_id,
                    input_price_per_token,
                    output_price_per_token,
                    ROW_NUMBER() OVER (
                        PARTITION BY deployed_model_id
                        ORDER BY
                            -- Prefer batch purpose
                            CASE WHEN api_key_purpose = 'batch' THEN 0 ELSE 1 END,
                            -- Prefer matching completion_window for batch tariffs
                            CASE
                                WHEN api_key_purpose = 'batch' AND completion_window = $2 THEN 0
                                WHEN api_key_purpose = 'batch' AND completion_window IS NULL THEN 1
                                ELSE 2
                            END
                    ) as rn
                FROM model_tariffs
                WHERE deployed_model_id = ANY($1)
                  AND valid_until IS NULL
                  AND api_key_purpose IN ('batch', 'realtime')
            )
            SELECT deployed_model_id, input_price_per_token, output_price_per_token
            FROM ranked_tariffs
            WHERE rn = 1
            "#,
            deployment_ids as &[DeploymentId],
            completion_window
        )
        .fetch_all(&mut *self.db)
        .await?;

        let mut result = HashMap::new();
        for record in records {
            result.insert(
                record.deployed_model_id,
                (record.input_price_per_token, record.output_price_per_token),
            );
        }

        Ok(result)
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
