//! Background service for webhook delivery with retry logic.
//!
//! This service:
//! - Exposes a function to trigger webhooks for batch terminal events
//! - Retries failed deliveries on a schedule
//! - Implements circuit breaker pattern for consistently failing webhooks

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

use crate::db::handlers::Webhooks;
use crate::webhooks::events::{RequestCounts, WebhookEvent, WebhookEventType};
use crate::webhooks::signing;

/// Retry loop interval for checking pending deliveries
const RETRY_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum number of deliveries to fetch in each retry loop iteration
const MAX_PENDING_DELIVERIES: i64 = 100;

/// Configuration for the webhook delivery service.
#[derive(Debug, Clone)]
pub struct WebhookServiceConfig {
    /// Whether the service is enabled
    pub enabled: bool,
    /// HTTP timeout for webhook deliveries
    pub timeout_secs: u64,
    /// Maximum retry attempts (default: 7)
    pub max_retries: i32,
    /// Circuit breaker threshold (default: 10)
    pub circuit_breaker_threshold: i32,
}

impl Default for WebhookServiceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            timeout_secs: 30,
            max_retries: 7,
            circuit_breaker_threshold: 10,
        }
    }
}

/// Details about a completed batch for webhook delivery.
#[derive(Debug, Clone)]
pub struct BatchWebhookEvent {
    pub batch_id: Uuid,
    pub user_id: Uuid,
    pub status: WebhookEventType,
    pub request_counts: RequestCounts,
    pub output_file_id: Option<Uuid>,
    pub error_file_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
}

/// Background service for delivering webhooks.
pub struct WebhookDeliveryService {
    pool: PgPool,
    http_client: reqwest::Client,
    config: WebhookServiceConfig,
}

impl WebhookDeliveryService {
    /// Create a new webhook delivery service.
    pub fn new(pool: PgPool, config: WebhookServiceConfig) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .expect("Failed to create HTTP client");

        Self { pool, http_client, config }
    }

    /// Start the webhook delivery service.
    ///
    /// This runs the retry loop for failed deliveries.
    #[instrument(skip(self, shutdown_token), err)]
    pub async fn start(self, shutdown_token: CancellationToken) -> anyhow::Result<()> {
        if !self.config.enabled {
            info!("Webhook delivery service is disabled");
            return Ok(());
        }

        info!("Starting webhook delivery service (retry loop)");
        let service = Arc::new(self);
        service.run_retry_loop(shutdown_token).await;
        Ok(())
    }

    /// Trigger webhooks for a batch terminal event.
    ///
    /// Call this when a batch reaches a terminal state (completed, failed, cancelled).
    /// This will find all matching webhooks for the user and attempt delivery.
    #[instrument(skip(self), fields(batch_id = %event.batch_id, user_id = %event.user_id, status = ?event.status), err)]
    pub async fn trigger_batch_webhooks(&self, event: &BatchWebhookEvent) -> anyhow::Result<()> {
        let mut conn = self.pool.acquire().await?;
        let mut repo = Webhooks::new(&mut conn);

        let webhooks = repo.get_enabled_webhooks_for_event(event.user_id, event.status.as_str()).await?;

        if webhooks.is_empty() {
            debug!("No webhooks configured for user {} and event {}", event.user_id, event.status);
            return Ok(());
        }

        // Create the webhook event payload
        let webhook_event = WebhookEvent::batch_terminal(
            event.status,
            event.batch_id,
            event.request_counts.clone(),
            event.output_file_id,
            event.error_file_id,
            event.created_at,
            event.finished_at,
        );

        let payload_json = serde_json::to_value(&webhook_event)?;

        // Create delivery records and attempt immediate delivery
        for webhook in webhooks {
            let event_id = Uuid::new_v4();

            let delivery_request = crate::db::models::webhooks::WebhookDeliveryCreateDBRequest {
                webhook_id: webhook.id,
                event_id,
                event_type: event.status.as_str().to_string(),
                payload: payload_json.clone(),
                batch_id: event.batch_id,
            };

            let delivery = repo.create_delivery(&delivery_request).await?;

            // Attempt immediate delivery
            let payload_str = serde_json::to_string(&webhook_event)?;
            match self
                .deliver_webhook(&webhook.url, &webhook.secret, &event_id.to_string(), &payload_str)
                .await
            {
                Ok(status_code) => {
                    repo.mark_delivered(delivery.id, status_code as i32).await?;
                    repo.reset_failures(webhook.id).await?;
                    info!(
                        "Webhook delivered successfully: webhook={}, batch={}, status={}",
                        webhook.id, event.batch_id, status_code
                    );
                }
                Err(e) => {
                    let (status_code, error_msg) = match &e {
                        DeliveryError::HttpError { status, message } => (Some(*status as i32), message.clone()),
                        DeliveryError::NetworkError(msg) => (None, msg.clone()),
                    };

                    repo.mark_failed(delivery.id, status_code, &error_msg, 0).await?;
                    repo.increment_failures(webhook.id).await?;

                    warn!(
                        "Webhook delivery failed: webhook={}, batch={}, error={}",
                        webhook.id, event.batch_id, error_msg
                    );
                }
            }
        }

        Ok(())
    }

    /// Run the retry loop for failed deliveries.
    async fn run_retry_loop(&self, shutdown_token: CancellationToken) {
        loop {
            tokio::select! {
                _ = shutdown_token.cancelled() => {
                    info!("Webhook retry loop shutting down");
                    return;
                }
                _ = tokio::time::sleep(RETRY_CHECK_INTERVAL) => {
                    if let Err(e) = self.process_pending_deliveries().await {
                        error!("Failed to process pending deliveries: {}", e);
                    }
                }
            }
        }
    }

    /// Process pending webhook deliveries that are due for retry.
    #[instrument(skip(self), err)]
    async fn process_pending_deliveries(&self) -> anyhow::Result<()> {
        let mut conn = self.pool.acquire().await?;
        let mut repo = Webhooks::new(&mut conn);

        let deliveries = repo.get_pending_deliveries(MAX_PENDING_DELIVERIES).await?;

        if deliveries.is_empty() {
            return Ok(());
        }

        debug!("Processing {} pending webhook deliveries", deliveries.len());

        for delivery in deliveries {
            // Get the webhook to check if it's still enabled
            let Some(webhook) = repo.get_by_id(delivery.webhook_id).await? else {
                // Webhook was deleted, skip
                continue;
            };

            if !webhook.enabled || webhook.disabled_at.is_some() {
                // Webhook is disabled, mark as exhausted
                repo.mark_failed(delivery.id, None, "Webhook disabled", self.config.max_retries)
                    .await?;
                continue;
            }

            let payload_str = serde_json::to_string(&delivery.payload)?;
            match self
                .deliver_webhook(&webhook.url, &webhook.secret, &delivery.event_id.to_string(), &payload_str)
                .await
            {
                Ok(status_code) => {
                    repo.mark_delivered(delivery.id, status_code as i32).await?;
                    repo.reset_failures(webhook.id).await?;
                    info!(
                        "Webhook retry delivered: delivery={}, webhook={}, status={}",
                        delivery.id, webhook.id, status_code
                    );
                }
                Err(e) => {
                    let (status_code, error_msg) = match &e {
                        DeliveryError::HttpError { status, message } => (Some(*status as i32), message.clone()),
                        DeliveryError::NetworkError(msg) => (None, msg.clone()),
                    };

                    repo.mark_failed(delivery.id, status_code, &error_msg, delivery.attempt_count)
                        .await?;
                    repo.increment_failures(webhook.id).await?;

                    debug!(
                        "Webhook retry failed: delivery={}, webhook={}, attempt={}, error={}",
                        delivery.id,
                        webhook.id,
                        delivery.attempt_count + 1,
                        error_msg
                    );
                }
            }
        }

        Ok(())
    }

    /// Deliver a webhook payload to a URL.
    async fn deliver_webhook(&self, url: &str, secret: &str, msg_id: &str, payload: &str) -> Result<u16, DeliveryError> {
        let timestamp = Utc::now().timestamp();

        let signature = signing::sign_payload(msg_id, timestamp, payload, secret)
            .ok_or_else(|| DeliveryError::NetworkError("Failed to sign webhook payload".to_string()))?;

        let response = self
            .http_client
            .post(url)
            .header("Content-Type", "application/json")
            .header("webhook-id", msg_id)
            .header("webhook-timestamp", timestamp.to_string())
            .header("webhook-signature", &signature)
            .body(payload.to_string())
            .send()
            .await
            .map_err(|e| DeliveryError::NetworkError(e.to_string()))?;

        let status = response.status();

        if status.is_success() {
            Ok(status.as_u16())
        } else {
            Err(DeliveryError::HttpError {
                status: status.as_u16(),
                message: format!("HTTP {}", status),
            })
        }
    }
}

/// Error type for webhook delivery.
#[derive(Debug)]
enum DeliveryError {
    HttpError { status: u16, message: String },
    NetworkError(String),
}
