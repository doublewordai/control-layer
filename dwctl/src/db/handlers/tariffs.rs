//! Database repository for model tariffs.

use crate::{
    db::{
        errors::Result,
        models::tariffs::{ModelTariff, TariffCreateDBRequest, TariffDBResponse, TariffUpdateDBRequest},
    },
    types::DeploymentId,
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::{PgConnection, PgPool};
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
}

impl<'c> Tariffs<'c> {
    /// Create a new tariff for a deployed model
    #[instrument(skip(self, request), fields(deployed_model_id = %request.deployed_model_id, name = %request.name), err)]
    pub async fn create(&mut self, request: &TariffCreateDBRequest) -> Result<TariffDBResponse> {
        let tariff = sqlx::query_as!(
            ModelTariff,
            r#"
            INSERT INTO model_tariffs (
                deployed_model_id, name, input_price_per_token, output_price_per_token,
                is_default, valid_from
            )
            VALUES ($1, $2, $3, $4, $5, COALESCE($6, NOW()))
            RETURNING id, deployed_model_id, name, input_price_per_token, output_price_per_token,
                      is_default, valid_from, valid_until, created_at, updated_at
            "#,
            request.deployed_model_id,
            request.name,
            request.input_price_per_token,
            request.output_price_per_token,
            request.is_default,
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
                   is_default, valid_from, valid_until, created_at, updated_at
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
                   is_default, valid_from, valid_until, created_at, updated_at
            FROM model_tariffs
            WHERE deployed_model_id = $1 AND valid_until IS NULL
            ORDER BY is_default DESC, name ASC
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
                   is_default, valid_from, valid_until, created_at, updated_at
            FROM model_tariffs
            WHERE deployed_model_id = $1
            ORDER BY valid_from DESC, name ASC
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
        if let Some(current) = current_tariff {
            if timestamp >= current.valid_from {
                // Fast path - use current pricing
                return Ok(Some((current.input_price_per_token, current.output_price_per_token)));
            }
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

    /// Update a tariff (only updates pricing and is_default)
    /// Note: To change pricing, you should typically close the current tariff and create a new one
    /// to maintain historical accuracy. This method is for minor corrections.
    #[instrument(skip(self, request), err)]
    pub async fn update(&mut self, id: Uuid, request: &TariffUpdateDBRequest) -> Result<Option<TariffDBResponse>> {
        let tariff = sqlx::query_as!(
            ModelTariff,
            r#"
            UPDATE model_tariffs
            SET input_price_per_token = COALESCE($2, input_price_per_token),
                output_price_per_token = COALESCE($3, output_price_per_token),
                is_default = COALESCE($4, is_default),
                updated_at = NOW()
            WHERE id = $1
            RETURNING id, deployed_model_id, name, input_price_per_token, output_price_per_token,
                      is_default, valid_from, valid_until, created_at, updated_at
            "#,
            id,
            request.input_price_per_token,
            request.output_price_per_token,
            request.is_default,
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(tariff)
    }

    /// Close out a tariff by setting valid_until to the current time
    /// This is the recommended way to "update" pricing - close the old tariff and create a new one
    #[instrument(skip(self), err)]
    pub async fn close_tariff(&mut self, id: Uuid) -> Result<Option<TariffDBResponse>> {
        let tariff = sqlx::query_as!(
            ModelTariff,
            r#"
            UPDATE model_tariffs
            SET valid_until = NOW(),
                updated_at = NOW()
            WHERE id = $1 AND valid_until IS NULL
            RETURNING id, deployed_model_id, name, input_price_per_token, output_price_per_token,
                      is_default, valid_from, valid_until, created_at, updated_at
            "#,
            id
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(tariff)
    }

    /// Close out a tariff by name and model, then create a new one with updated pricing
    /// This maintains historical accuracy while updating pricing
    #[instrument(skip(self), err)]
    pub async fn replace_tariff(
        &mut self,
        deployed_model_id: DeploymentId,
        name: &str,
        new_input_price: Decimal,
        new_output_price: Decimal,
        is_default: bool,
    ) -> Result<TariffDBResponse> {
        // Close out the current tariff
        sqlx::query!(
            r#"
            UPDATE model_tariffs
            SET valid_until = NOW(),
                updated_at = NOW()
            WHERE deployed_model_id = $1 AND name = $2 AND valid_until IS NULL
            "#,
            deployed_model_id,
            name
        )
        .execute(&mut *self.db)
        .await?;

        // Create the new tariff
        let tariff = sqlx::query_as!(
            ModelTariff,
            r#"
            INSERT INTO model_tariffs (
                deployed_model_id, name, input_price_per_token, output_price_per_token,
                is_default, valid_from
            )
            VALUES ($1, $2, $3, $4, $5, NOW())
            RETURNING id, deployed_model_id, name, input_price_per_token, output_price_per_token,
                      is_default, valid_from, valid_until, created_at, updated_at
            "#,
            deployed_model_id,
            name,
            new_input_price,
            new_output_price,
            is_default
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(tariff)
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
