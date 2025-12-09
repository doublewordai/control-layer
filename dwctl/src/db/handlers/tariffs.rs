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
                api_key_purpose, valid_from
            )
            VALUES ($1, $2, $3, $4, $5, COALESCE($6, NOW()))
            RETURNING id, deployed_model_id, name, input_price_per_token, output_price_per_token,
                      valid_from, valid_until, api_key_purpose as "api_key_purpose: _"
            "#,
            request.deployed_model_id,
            request.name,
            request.input_price_per_token,
            request.output_price_per_token,
            purpose_str,
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
                   valid_from, valid_until, api_key_purpose as "api_key_purpose: _"
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
                   valid_from, valid_until, api_key_purpose as "api_key_purpose: _"
            FROM model_tariffs
            WHERE deployed_model_id = $1 AND valid_until IS NULL
            ORDER BY api_key_purpose ASC NULLS LAST, name ASC
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
                   valid_from, valid_until, api_key_purpose as "api_key_purpose: _"
            FROM model_tariffs
            WHERE deployed_model_id = $1
            ORDER BY valid_from DESC, api_key_purpose ASC NULLS LAST, name ASC
            "#,
            deployed_model_id
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(tariffs)
    }

    /// Get the pricing for a specific tariff that was valid at a given timestamp
    /// This is used for historical chargeback calculations
    ///
    /// Uses an optimized two-step lookup:
    /// 1. First checks the current (active) tariff (WHERE valid_until IS NULL)
    /// 2. If the timestamp is >= current tariff's valid_from, uses it (fast path)
    /// 3. Otherwise, does a full historical lookup with temporal constraints
    #[instrument(skip(self), err)]
    pub async fn get_pricing_at_timestamp(
        &mut self,
        deployed_model_id: DeploymentId,
        tariff_name: &str,
        timestamp: DateTime<Utc>,
    ) -> Result<Option<(Decimal, Decimal)>> {
        // Step 1: Check current (active) tariff - this is the fast path for recent requests
        let current_tariff = sqlx::query!(
            r#"
            SELECT input_price_per_token, output_price_per_token, valid_from
            FROM model_tariffs
            WHERE deployed_model_id = $1
              AND name = $2
              AND valid_until IS NULL
            LIMIT 1
            "#,
            deployed_model_id,
            tariff_name
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
              AND name = $2
              AND valid_from <= $3
              AND (valid_until IS NULL OR valid_until > $3)
            ORDER BY valid_from DESC
            LIMIT 1
            "#,
            deployed_model_id,
            tariff_name,
            timestamp
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(result.map(|r| (r.input_price_per_token, r.output_price_per_token)))
    }


    /// Close multiple tariffs by setting valid_until to the current time
    /// More efficient than calling close_tariff in a loop
    #[instrument(skip(self), fields(count = ids.len()), err)]
    pub async fn close_tariffs_batch(&mut self, ids: &[Uuid]) -> Result<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query!(
            "UPDATE model_tariffs SET valid_until = NOW() WHERE id = ANY($1)",
            ids
        )
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
