//! Database repository for webhook configuration and delivery tracking.

use chrono::{Duration, Utc};
use sqlx::PgConnection;
use tracing::instrument;

use crate::db::errors::Result;
use crate::db::models::webhooks::{
    DeliveryId, DeliveryStatus, Webhook, WebhookCreateDBRequest, WebhookDelivery, WebhookDeliveryCreateDBRequest, WebhookId,
    WebhookUpdateDBRequest,
};
use crate::types::{UserId, abbrev_uuid};

/// Retry schedule in seconds: 0s → 5s → 5m → 30m → 2h → 8h → 24h
const RETRY_DELAYS_SECS: &[i64] = &[
    0,     // Attempt 1: immediate
    5,     // Attempt 2: 5 seconds
    300,   // Attempt 3: 5 minutes
    1800,  // Attempt 4: 30 minutes
    7200,  // Attempt 5: 2 hours
    28800, // Attempt 6: 8 hours
    86400, // Attempt 7: 24 hours
];

/// Maximum number of delivery attempts.
pub const MAX_RETRY_ATTEMPTS: i32 = RETRY_DELAYS_SECS.len() as i32;

/// Circuit breaker threshold for consecutive failures.
pub const CIRCUIT_BREAKER_THRESHOLD: i32 = 10;

/// Repository for webhook operations.
pub struct Webhooks<'c> {
    db: &'c mut PgConnection,
}

impl<'c> Webhooks<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    /// Create a new webhook for a user.
    #[instrument(skip(self, request), fields(user_id = %abbrev_uuid(&request.user_id)), err)]
    pub async fn create(&mut self, request: &WebhookCreateDBRequest) -> Result<Webhook> {
        let event_types_json = request
            .event_types
            .as_ref()
            .map(|types| serde_json::to_value(types).unwrap_or(serde_json::Value::Null));

        let webhook = sqlx::query_as!(
            Webhook,
            r#"
            INSERT INTO user_webhooks (user_id, url, secret, event_types, description)
            VALUES ($1, $2, $3, $4, $5)
            RETURNING *
            "#,
            request.user_id,
            request.url,
            request.secret,
            event_types_json,
            request.description,
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(webhook)
    }

    /// Get a webhook by ID.
    #[instrument(skip(self), fields(webhook_id = %abbrev_uuid(&id)), err)]
    pub async fn get_by_id(&mut self, id: WebhookId) -> Result<Option<Webhook>> {
        let webhook = sqlx::query_as!(Webhook, r#"SELECT * FROM user_webhooks WHERE id = $1"#, id)
            .fetch_optional(&mut *self.db)
            .await?;

        Ok(webhook)
    }

    /// List webhooks for a user.
    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn list_by_user(&mut self, user_id: UserId) -> Result<Vec<Webhook>> {
        let webhooks = sqlx::query_as!(
            Webhook,
            r#"
            SELECT * FROM user_webhooks
            WHERE user_id = $1
            ORDER BY created_at DESC
            "#,
            user_id
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(webhooks)
    }

    /// Update a webhook.
    #[instrument(skip(self, request), fields(webhook_id = %abbrev_uuid(&id)), err)]
    pub async fn update(&mut self, id: WebhookId, request: &WebhookUpdateDBRequest) -> Result<Option<Webhook>> {
        // Convert event_types Option<Option<Vec>> to Option<Option<Value>> for SQL
        let event_types_json: Option<Option<serde_json::Value>> = request.event_types.as_ref().map(|opt| {
            opt.as_ref()
                .map(|types| serde_json::to_value(types).unwrap_or(serde_json::Value::Null))
        });

        let webhook = sqlx::query_as!(
            Webhook,
            r#"
            UPDATE user_webhooks
            SET
                url = COALESCE($2, url),
                enabled = COALESCE($3, enabled),
                event_types = CASE
                    WHEN $4::boolean THEN $5
                    ELSE event_types
                END,
                description = CASE
                    WHEN $6::boolean THEN $7
                    ELSE description
                END,
                -- Clear disabled_at when re-enabling
                disabled_at = CASE
                    WHEN $3 = true THEN NULL
                    ELSE disabled_at
                END,
                -- Reset consecutive failures when re-enabling
                consecutive_failures = CASE
                    WHEN $3 = true THEN 0
                    ELSE consecutive_failures
                END
            WHERE id = $1
            RETURNING *
            "#,
            id,
            request.url,
            request.enabled,
            event_types_json.is_some(),
            event_types_json.flatten(),
            request.description.is_some(),
            request.description.clone().flatten(),
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(webhook)
    }

    /// Delete a webhook.
    #[instrument(skip(self), fields(webhook_id = %abbrev_uuid(&id)), err)]
    pub async fn delete(&mut self, id: WebhookId) -> Result<bool> {
        let result = sqlx::query!("DELETE FROM user_webhooks WHERE id = $1", id)
            .execute(&mut *self.db)
            .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Rotate a webhook's secret.
    #[instrument(skip(self, new_secret), fields(webhook_id = %abbrev_uuid(&id)), err)]
    pub async fn rotate_secret(&mut self, id: WebhookId, new_secret: String) -> Result<Option<Webhook>> {
        let webhook = sqlx::query_as!(
            Webhook,
            r#"
            UPDATE user_webhooks
            SET secret = $2
            WHERE id = $1
            RETURNING *
            "#,
            id,
            new_secret,
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(webhook)
    }

    /// Get enabled webhooks for a user that accept a specific event type.
    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn get_enabled_webhooks_for_event(&mut self, user_id: UserId, event_type: &str) -> Result<Vec<Webhook>> {
        let webhooks = sqlx::query_as!(
            Webhook,
            r#"
            SELECT * FROM user_webhooks
            WHERE user_id = $1
              AND enabled = true
              AND disabled_at IS NULL
              AND (
                  event_types IS NULL
                  OR event_types @> $2::jsonb
              )
            "#,
            user_id,
            serde_json::json!([event_type]),
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(webhooks)
    }

    /// Increment consecutive failures and potentially trip circuit breaker.
    #[instrument(skip(self), fields(webhook_id = %abbrev_uuid(&id)), err)]
    pub async fn increment_failures(&mut self, id: WebhookId) -> Result<Webhook> {
        let webhook = sqlx::query_as!(
            Webhook,
            r#"
            UPDATE user_webhooks
            SET
                consecutive_failures = consecutive_failures + 1,
                enabled = CASE
                    WHEN consecutive_failures + 1 >= $2 THEN false
                    ELSE enabled
                END,
                disabled_at = CASE
                    WHEN consecutive_failures + 1 >= $2 THEN now()
                    ELSE disabled_at
                END
            WHERE id = $1
            RETURNING *
            "#,
            id,
            CIRCUIT_BREAKER_THRESHOLD,
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(webhook)
    }

    /// Reset consecutive failures on successful delivery.
    #[instrument(skip(self), fields(webhook_id = %abbrev_uuid(&id)), err)]
    pub async fn reset_failures(&mut self, id: WebhookId) -> Result<()> {
        sqlx::query!(
            r#"
            UPDATE user_webhooks
            SET consecutive_failures = 0
            WHERE id = $1
            "#,
            id,
        )
        .execute(&mut *self.db)
        .await?;

        Ok(())
    }

    // ===== Delivery methods =====

    /// Create a new delivery record.
    #[instrument(skip(self, request), fields(webhook_id = %abbrev_uuid(&request.webhook_id)), err)]
    pub async fn create_delivery(&mut self, request: &WebhookDeliveryCreateDBRequest) -> Result<WebhookDelivery> {
        let delivery = sqlx::query_as!(
            WebhookDelivery,
            r#"
            INSERT INTO webhook_deliveries (webhook_id, event_id, event_type, payload, batch_id)
            VALUES ($1, $2, $3, $4, $5)
            RETURNING *
            "#,
            request.webhook_id,
            request.event_id,
            request.event_type,
            request.payload,
            request.batch_id,
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(delivery)
    }

    /// Get pending deliveries that are due for retry.
    #[instrument(skip(self), err)]
    pub async fn get_pending_deliveries(&mut self, limit: i64) -> Result<Vec<WebhookDelivery>> {
        let deliveries = sqlx::query_as!(
            WebhookDelivery,
            r#"
            SELECT * FROM webhook_deliveries
            WHERE status IN ('pending', 'failed')
              AND next_attempt_at <= now()
            ORDER BY next_attempt_at ASC
            LIMIT $1
            "#,
            limit,
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(deliveries)
    }

    /// Mark a delivery as successful.
    #[instrument(skip(self), fields(delivery_id = %abbrev_uuid(&id)), err)]
    pub async fn mark_delivered(&mut self, id: DeliveryId, status_code: i32) -> Result<WebhookDelivery> {
        let delivery = sqlx::query_as!(
            WebhookDelivery,
            r#"
            UPDATE webhook_deliveries
            SET
                status = 'delivered',
                attempt_count = attempt_count + 1,
                last_status_code = $2,
                last_error = NULL
            WHERE id = $1
            RETURNING *
            "#,
            id,
            status_code,
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(delivery)
    }

    /// Mark a delivery as failed and schedule retry.
    #[instrument(skip(self, error), fields(delivery_id = %abbrev_uuid(&id)), err)]
    pub async fn mark_failed(
        &mut self,
        id: DeliveryId,
        status_code: Option<i32>,
        error: &str,
        current_attempt: i32,
    ) -> Result<WebhookDelivery> {
        let new_attempt = current_attempt + 1;

        // Determine next status and retry time
        let (new_status, next_attempt_at) = if new_attempt >= MAX_RETRY_ATTEMPTS {
            (DeliveryStatus::Exhausted.as_str(), Utc::now())
        } else {
            let delay_secs = RETRY_DELAYS_SECS.get(new_attempt as usize).copied().unwrap_or(86400);
            let next = Utc::now() + Duration::seconds(delay_secs);
            (DeliveryStatus::Failed.as_str(), next)
        };

        let delivery = sqlx::query_as!(
            WebhookDelivery,
            r#"
            UPDATE webhook_deliveries
            SET
                status = $2,
                attempt_count = $3,
                next_attempt_at = $4,
                last_status_code = $5,
                last_error = $6
            WHERE id = $1
            RETURNING *
            "#,
            id,
            new_status,
            new_attempt,
            next_attempt_at,
            status_code,
            error,
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(delivery)
    }

    /// Get a delivery by ID.
    #[instrument(skip(self), fields(delivery_id = %abbrev_uuid(&id)), err)]
    pub async fn get_delivery_by_id(&mut self, id: DeliveryId) -> Result<Option<WebhookDelivery>> {
        let delivery = sqlx::query_as!(WebhookDelivery, r#"SELECT * FROM webhook_deliveries WHERE id = $1"#, id)
            .fetch_optional(&mut *self.db)
            .await?;

        Ok(delivery)
    }
}
