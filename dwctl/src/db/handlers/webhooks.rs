//! Database repository for webhook configuration and delivery tracking.

use chrono::{Duration, Utc};
use sqlx::PgConnection;
use tracing::instrument;

use crate::db::errors::Result;
use crate::db::models::webhooks::{
    ClaimedDelivery, DeliveryId, DeliveryStatus, Webhook, WebhookCreateDBRequest, WebhookDelivery, WebhookDeliveryCreateDBRequest,
    WebhookId, WebhookUpdateDBRequest,
};
use crate::types::{UserId, abbrev_uuid};

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

    /// Get all enabled webhooks for a set of users, grouped by user ID.
    #[instrument(skip(self), err)]
    pub async fn get_enabled_webhooks_for_users(
        &mut self,
        user_ids: Vec<UserId>,
    ) -> Result<std::collections::HashMap<UserId, Vec<Webhook>>> {
        let webhooks = sqlx::query_as!(
            Webhook,
            r#"
            SELECT * FROM user_webhooks
            WHERE user_id = ANY($1)
              AND enabled = true
              AND disabled_at IS NULL
            "#,
            &user_ids,
        )
        .fetch_all(&mut *self.db)
        .await?;

        let mut by_user: std::collections::HashMap<UserId, Vec<Webhook>> = std::collections::HashMap::new();
        for webhook in webhooks {
            by_user.entry(webhook.user_id).or_default().push(webhook);
        }

        Ok(by_user)
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
            INSERT INTO webhook_deliveries (webhook_id, event_id, event_type, payload, batch_id, next_attempt_at)
            VALUES ($1, $2, $3, $4, $5, COALESCE($6, now()))
            RETURNING *
            "#,
            request.webhook_id,
            request.event_id,
            request.event_type,
            request.payload,
            request.batch_id,
            request.next_attempt_at,
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(delivery)
    }

    /// Atomically claim retriable deliveries for processing.
    ///
    /// Uses SELECT ... FOR UPDATE SKIP LOCKED to prevent multi-replica races.
    ///
    /// The 5-minute `next_attempt_at` bump is a crash safety net, NOT the retry
    /// backoff. The normal flow is:
    ///
    /// 1. This method claims the delivery and bumps `next_attempt_at` by 5 min
    /// 2. The sender task performs the HTTP request
    /// 3. `mark_failed()` overwrites `next_attempt_at` with the real backoff
    ///    from `RETRY_DELAYS_SECS` (5s, 5m, 30m, 2h, 8h, 24h)
    ///
    /// If the server crashes between steps 1 and 3, the delivery becomes
    /// claimable again after 5 minutes. This also means graceful shutdown
    /// doesn't need to drain in-flight results — unprocessed deliveries
    /// just get re-claimed on the next startup.
    #[instrument(skip(self), err)]
    pub async fn claim_retriable_deliveries(&mut self, limit: i64) -> Result<Vec<ClaimedDelivery>> {
        let deliveries = sqlx::query_as!(
            ClaimedDelivery,
            r#"
            WITH claimed AS (
                SELECT id FROM webhook_deliveries
                WHERE status IN ('pending', 'failed')
                  AND next_attempt_at <= now()
                ORDER BY next_attempt_at ASC
                LIMIT $1
                FOR UPDATE SKIP LOCKED
            ),
            updated AS (
                UPDATE webhook_deliveries wd
                SET next_attempt_at = now() + interval '5 minutes'
                FROM claimed
                WHERE wd.id = claimed.id
                RETURNING wd.*
            )
            SELECT u.*,
                   w.url AS webhook_url,
                   w.secret AS webhook_secret,
                   w.enabled AS webhook_enabled
            FROM updated u
            LEFT JOIN user_webhooks w ON w.id = u.webhook_id
            "#,
            limit,
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(deliveries)
    }

    /// Mark a delivery as successful.
    #[instrument(skip(self), fields(delivery_id = %abbrev_uuid(&id)), err)]
    pub async fn mark_delivered(&mut self, id: DeliveryId, status_code: i32) -> Result<()> {
        sqlx::query!(
            r#"
            UPDATE webhook_deliveries
            SET
                status = 'delivered',
                attempt_count = attempt_count + 1,
                last_status_code = $2,
                last_error = NULL
            WHERE id = $1
            "#,
            id,
            status_code,
        )
        .execute(&mut *self.db)
        .await?;

        Ok(())
    }

    /// Mark a delivery as failed and schedule retry.
    #[instrument(skip(self, error), fields(delivery_id = %abbrev_uuid(&id)), err)]
    pub async fn mark_failed(
        &mut self,
        id: DeliveryId,
        status_code: Option<i32>,
        error: &str,
        current_attempt: i32,
        retry_schedule: &[i64],
    ) -> Result<()> {
        let new_attempt = current_attempt + 1;
        let max_attempts = retry_schedule.len() as i32;

        // Determine next status and retry time
        let (new_status, next_attempt_at) = if new_attempt >= max_attempts {
            (DeliveryStatus::Exhausted.as_str(), Utc::now())
        } else {
            let delay_secs = retry_schedule.get(new_attempt as usize).copied().unwrap_or(86400);
            let next = Utc::now() + Duration::seconds(delay_secs);
            (DeliveryStatus::Failed.as_str(), next)
        };

        sqlx::query!(
            r#"
            UPDATE webhook_deliveries
            SET
                status = $2,
                attempt_count = $3,
                next_attempt_at = $4,
                last_status_code = $5,
                last_error = $6
            WHERE id = $1
            "#,
            id,
            new_status,
            new_attempt,
            next_attempt_at,
            status_code,
            error,
        )
        .execute(&mut *self.db)
        .await?;

        Ok(())
    }

    /// Mark a delivery as exhausted (e.g., webhook was disabled).
    #[instrument(skip(self), fields(delivery_id = %abbrev_uuid(&id)), err)]
    pub async fn mark_exhausted(&mut self, id: DeliveryId) -> Result<()> {
        sqlx::query!(
            r#"
            UPDATE webhook_deliveries
            SET status = 'exhausted',
                last_error = 'webhook disabled'
            WHERE id = $1
            "#,
            id,
        )
        .execute(&mut *self.db)
        .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::users::Role;
    use crate::db::models::webhooks::{WebhookCreateDBRequest, WebhookDeliveryCreateDBRequest};
    use crate::test::utils::create_test_user;
    use crate::webhooks::signing::generate_secret;
    use chrono::DateTime;
    use sqlx::PgPool;

    /// Default retry schedule matching production config default.
    const SCHEDULE_7: &[i64] = &[0, 5, 300, 1800, 7200, 28800, 86400];
    /// Short 3-attempt schedule for lifecycle tests.
    const SCHEDULE_3: &[i64] = &[0, 5, 300];

    /// Create a webhook for testing delivery operations.
    async fn create_test_webhook(pool: &PgPool) -> (Webhook, uuid::Uuid) {
        let user = create_test_user(pool, Role::StandardUser).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Webhooks::new(&mut conn);

        let webhook = repo
            .create(&WebhookCreateDBRequest {
                user_id: user.id,
                url: "https://example.com/webhook".to_string(),
                secret: generate_secret(),
                event_types: None,
                description: None,
            })
            .await
            .unwrap();

        (webhook, user.id)
    }

    /// Create a delivery record for a webhook.
    async fn create_test_delivery(pool: &PgPool, webhook_id: WebhookId, next_attempt_at: Option<DateTime<Utc>>) -> WebhookDelivery {
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Webhooks::new(&mut conn);

        repo.create_delivery(&WebhookDeliveryCreateDBRequest {
            webhook_id,
            event_id: uuid::Uuid::new_v4(),
            event_type: "batch.completed".to_string(),
            payload: serde_json::json!({"type": "batch.completed", "data": {}}),
            batch_id: uuid::Uuid::new_v4(),
            next_attempt_at,
        })
        .await
        .unwrap()
    }

    /// Set next_attempt_at to the past to simulate time passing.
    async fn time_travel_delivery(pool: &PgPool, delivery_id: DeliveryId) {
        sqlx::query!(
            "UPDATE webhook_deliveries SET next_attempt_at = now() - interval '1 second' WHERE id = $1",
            delivery_id,
        )
        .execute(pool)
        .await
        .unwrap();
    }

    /// Read back a delivery row to verify state after mutations.
    async fn get_delivery(pool: &PgPool, id: DeliveryId) -> WebhookDelivery {
        let mut conn = pool.acquire().await.unwrap();
        sqlx::query_as!(WebhookDelivery, "SELECT * FROM webhook_deliveries WHERE id = $1", id)
            .fetch_one(&mut *conn)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn test_claim_picks_up_due_deliveries(pool: PgPool) {
        let (webhook, _) = create_test_webhook(&pool).await;
        let delivery = create_test_delivery(&pool, webhook.id, None).await;
        assert_eq!(delivery.status, "pending");

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Webhooks::new(&mut conn);
        let claimed = repo.claim_retriable_deliveries(10).await.unwrap();

        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, delivery.id);
    }

    #[sqlx::test]
    async fn test_claim_skips_future_deliveries(pool: PgPool) {
        let (webhook, _) = create_test_webhook(&pool).await;

        // Create delivery with next_attempt_at 1 hour in the future
        let future = Utc::now() + Duration::hours(1);
        create_test_delivery(&pool, webhook.id, Some(future)).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Webhooks::new(&mut conn);
        let claimed = repo.claim_retriable_deliveries(10).await.unwrap();

        assert_eq!(claimed.len(), 0);
    }

    #[sqlx::test]
    async fn test_claim_bumps_next_attempt_at(pool: PgPool) {
        let (webhook, _) = create_test_webhook(&pool).await;
        create_test_delivery(&pool, webhook.id, None).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Webhooks::new(&mut conn);
        let claimed = repo.claim_retriable_deliveries(10).await.unwrap();
        let claimed_delivery = &claimed[0];

        // next_attempt_at should now be ~5 minutes in the future
        let diff = claimed_delivery.next_attempt_at - Utc::now();
        assert!(diff.num_seconds() > 200 && diff.num_seconds() <= 300);

        // Claiming again should return nothing (bumped to future)
        let claimed_again = repo.claim_retriable_deliveries(10).await.unwrap();
        assert_eq!(claimed_again.len(), 0);
    }

    #[sqlx::test]
    async fn test_claim_skips_delivered(pool: PgPool) {
        let (webhook, _) = create_test_webhook(&pool).await;
        let delivery = create_test_delivery(&pool, webhook.id, None).await;

        // Mark as delivered
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Webhooks::new(&mut conn);
        repo.mark_delivered(delivery.id, 200).await.unwrap();

        // Claim should return nothing
        let claimed = repo.claim_retriable_deliveries(10).await.unwrap();
        assert_eq!(claimed.len(), 0);
    }

    #[sqlx::test]
    async fn test_claim_skips_exhausted(pool: PgPool) {
        let (webhook, _) = create_test_webhook(&pool).await;
        let delivery = create_test_delivery(&pool, webhook.id, None).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Webhooks::new(&mut conn);
        repo.mark_exhausted(delivery.id).await.unwrap();

        let claimed = repo.claim_retriable_deliveries(10).await.unwrap();
        assert_eq!(claimed.len(), 0);
    }

    #[sqlx::test]
    async fn test_mark_failed_sets_retry_schedule(pool: PgPool) {
        let (webhook, _) = create_test_webhook(&pool).await;
        let delivery = create_test_delivery(&pool, webhook.id, None).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Webhooks::new(&mut conn);

        // Attempt 0 → 1: next retry in ~5 seconds
        repo.mark_failed(delivery.id, Some(500), "HTTP 500", 0, SCHEDULE_7).await.unwrap();
        let d = get_delivery(&pool, delivery.id).await;
        assert_eq!(d.status, "failed");
        assert_eq!(d.attempt_count, 1);
        let delay = (d.next_attempt_at - Utc::now()).num_seconds();
        assert!(delay >= 3 && delay <= 7, "expected ~5s delay, got {}s", delay);

        // Time travel and fail again: attempt 1 → 2: next retry in ~5 minutes
        time_travel_delivery(&pool, delivery.id).await;
        repo.mark_failed(delivery.id, Some(502), "HTTP 502", 1, SCHEDULE_7).await.unwrap();
        let d = get_delivery(&pool, delivery.id).await;
        assert_eq!(d.attempt_count, 2);
        let delay = (d.next_attempt_at - Utc::now()).num_seconds();
        assert!(delay >= 280 && delay <= 320, "expected ~300s delay, got {}s", delay);
    }

    #[sqlx::test]
    async fn test_mark_failed_exhausts_at_max_retries(pool: PgPool) {
        let (webhook, _) = create_test_webhook(&pool).await;
        let delivery = create_test_delivery(&pool, webhook.id, None).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Webhooks::new(&mut conn);

        // Attempt 6 → 7 with 7-entry schedule: should exhaust
        repo.mark_failed(delivery.id, Some(500), "HTTP 500", 6, SCHEDULE_7).await.unwrap();
        let d = get_delivery(&pool, delivery.id).await;
        assert_eq!(d.status, "exhausted");
        assert_eq!(d.attempt_count, 7);
    }

    #[sqlx::test]
    async fn test_mark_failed_respects_custom_max_retries(pool: PgPool) {
        let (webhook, _) = create_test_webhook(&pool).await;
        let delivery = create_test_delivery(&pool, webhook.id, None).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Webhooks::new(&mut conn);

        // With 3-entry schedule, attempt 2 → 3 should exhaust
        repo.mark_failed(delivery.id, Some(500), "HTTP 500", 2, SCHEDULE_3).await.unwrap();
        let d = get_delivery(&pool, delivery.id).await;
        assert_eq!(d.status, "exhausted");
        assert_eq!(d.attempt_count, 3);
    }

    #[sqlx::test]
    async fn test_full_retry_lifecycle(pool: PgPool) {
        let (webhook, _) = create_test_webhook(&pool).await;
        let delivery = create_test_delivery(&pool, webhook.id, None).await;
        assert_eq!(delivery.status, "pending");
        assert_eq!(delivery.attempt_count, 0);

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Webhooks::new(&mut conn);

        // Attempt 1: claim, fail, check state
        let claimed = repo.claim_retriable_deliveries(10).await.unwrap();
        assert_eq!(claimed.len(), 1);

        repo.mark_failed(delivery.id, Some(500), "HTTP 500", 0, SCHEDULE_3).await.unwrap();
        let d = get_delivery(&pool, delivery.id).await;
        assert_eq!(d.status, "failed");
        assert_eq!(d.attempt_count, 1);
        repo.increment_failures(webhook.id).await.unwrap();

        // Attempt 2: time travel, claim, fail
        time_travel_delivery(&pool, delivery.id).await;
        let claimed = repo.claim_retriable_deliveries(10).await.unwrap();
        assert_eq!(claimed.len(), 1);

        repo.mark_failed(delivery.id, Some(503), "HTTP 503", 1, SCHEDULE_3).await.unwrap();
        let d = get_delivery(&pool, delivery.id).await;
        assert_eq!(d.status, "failed");
        assert_eq!(d.attempt_count, 2);
        repo.increment_failures(webhook.id).await.unwrap();

        // Attempt 3: time travel, claim, fail → exhausted
        time_travel_delivery(&pool, delivery.id).await;
        let claimed = repo.claim_retriable_deliveries(10).await.unwrap();
        assert_eq!(claimed.len(), 1);

        repo.mark_failed(delivery.id, Some(500), "HTTP 500", 2, SCHEDULE_3).await.unwrap();
        let d = get_delivery(&pool, delivery.id).await;
        assert_eq!(d.status, "exhausted");
        assert_eq!(d.attempt_count, 3);

        // No more claims
        time_travel_delivery(&pool, delivery.id).await;
        let claimed = repo.claim_retriable_deliveries(10).await.unwrap();
        assert_eq!(claimed.len(), 0);
    }

    #[sqlx::test]
    async fn test_successful_delivery_resets_failures(pool: PgPool) {
        let (webhook, _) = create_test_webhook(&pool).await;
        let delivery = create_test_delivery(&pool, webhook.id, None).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Webhooks::new(&mut conn);

        // Fail once to increment failures
        repo.mark_failed(delivery.id, Some(500), "HTTP 500", 0, SCHEDULE_7).await.unwrap();
        let w = repo.increment_failures(webhook.id).await.unwrap();
        assert_eq!(w.consecutive_failures, 1);

        // Succeed on retry
        time_travel_delivery(&pool, delivery.id).await;
        let claimed = repo.claim_retriable_deliveries(10).await.unwrap();
        assert_eq!(claimed.len(), 1);

        repo.mark_delivered(delivery.id, 200).await.unwrap();
        repo.reset_failures(webhook.id).await.unwrap();

        // Verify webhook failures reset
        let w = repo.get_by_id(webhook.id).await.unwrap().unwrap();
        assert_eq!(w.consecutive_failures, 0);

        // Delivery should not be claimable
        let claimed = repo.claim_retriable_deliveries(10).await.unwrap();
        assert_eq!(claimed.len(), 0);
    }

    #[sqlx::test]
    async fn test_mark_exhausted_is_terminal(pool: PgPool) {
        let (webhook, _) = create_test_webhook(&pool).await;
        let delivery = create_test_delivery(&pool, webhook.id, None).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Webhooks::new(&mut conn);

        repo.mark_exhausted(delivery.id).await.unwrap();
        let d = get_delivery(&pool, delivery.id).await;
        assert_eq!(d.status, "exhausted");
        assert_eq!(d.last_error.as_deref(), Some("webhook disabled"));

        // Not claimable
        time_travel_delivery(&pool, delivery.id).await;
        let claimed = repo.claim_retriable_deliveries(10).await.unwrap();
        assert_eq!(claimed.len(), 0);
    }
}
