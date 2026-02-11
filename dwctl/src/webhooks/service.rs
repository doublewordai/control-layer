//! Webhook delivery service for batch terminal events.
//!
//! Structured as a service (like `EmailService`) that is created once at poller
//! startup and called once per poll cycle with all batch infos and pre-fetched
//! webhooks.

use std::collections::HashSet;
use std::time::Duration;

use chrono::Utc;
use sqlx::PgPool;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::WebhookConfig;
use crate::db::handlers::Webhooks;
use crate::notifications::{BatchNotificationInfo, BatchOutcome};
use crate::webhooks::events::{WebhookEvent, WebhookEventType};
use crate::webhooks::signing;

pub struct WebhookService {
    pool: PgPool,
    http_client: reqwest::Client,
}

impl WebhookService {
    pub fn new(pool: PgPool, config: WebhookConfig) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .expect("Failed to create HTTP client");

        Self { pool, http_client }
    }

    /// Deliver webhooks for a batch of notifications.
    ///
    /// Fetches enabled webhooks for the relevant users, filters by event type,
    /// and delivers.
    pub(crate) async fn send_batch_webhooks(&self, infos: &[BatchNotificationInfo]) -> anyhow::Result<()> {
        if infos.is_empty() {
            return Ok(());
        }

        let mut conn = self.pool.acquire().await?;

        let user_ids: Vec<Uuid> = infos.iter().map(|i| i.user_id).collect::<HashSet<_>>().into_iter().collect();
        let webhooks_by_user = {
            let mut repo = Webhooks::new(&mut conn);
            repo.get_enabled_webhooks_for_users(user_ids).await?
        };

        let mut repo = Webhooks::new(&mut conn);

        for info in infos {
            let Some(webhooks) = webhooks_by_user.get(&info.user_id) else {
                debug!(user_id = %info.user_id, "No webhooks configured, skipping");
                continue;
            };

            let webhook_status = match info.outcome {
                BatchOutcome::Completed | BatchOutcome::PartiallyCompleted => WebhookEventType::BatchCompleted,
                BatchOutcome::Failed => WebhookEventType::BatchFailed,
            };

            let webhook_event = WebhookEvent::batch_terminal(webhook_status, info);
            let payload_json = serde_json::to_value(&webhook_event)?;
            let payload_str = serde_json::to_string(&webhook_event)?;

            for webhook in webhooks.iter().filter(|w| w.accepts_event(webhook_status)) {
                let event_id = Uuid::new_v4();

                let delivery_request = crate::db::models::webhooks::WebhookDeliveryCreateDBRequest {
                    webhook_id: webhook.id,
                    event_id,
                    event_type: webhook_status.to_string(),
                    payload: payload_json.clone(),
                    batch_id: info.batch_uuid,
                };

                let delivery = repo.create_delivery(&delivery_request).await?;

                let timestamp = Utc::now().timestamp();
                let msg_id = event_id.to_string();
                let signature = signing::sign_payload(&msg_id, timestamp, &payload_str, &webhook.secret)
                    .ok_or_else(|| anyhow::anyhow!("Failed to sign webhook payload"))?;

                let result = self
                    .http_client
                    .post(&webhook.url)
                    .header("Content-Type", "application/json")
                    .header("webhook-id", &msg_id)
                    .header("webhook-timestamp", timestamp.to_string())
                    .header("webhook-signature", &signature)
                    .body(payload_str.clone())
                    .send()
                    .await;

                match result {
                    Ok(response) if response.status().is_success() => {
                        let status_code = response.status().as_u16();
                        repo.mark_delivered(delivery.id, status_code as i32).await?;
                        repo.reset_failures(webhook.id).await?;
                        info!(
                            webhook_id = %webhook.id,
                            batch_id = %info.batch_uuid,
                            status = status_code,
                            "Webhook delivered successfully"
                        );
                    }
                    Ok(response) => {
                        let status_code = response.status().as_u16();
                        let error_msg = format!("HTTP {}", status_code);
                        repo.mark_failed(delivery.id, Some(status_code as i32), &error_msg, 0).await?;
                        repo.increment_failures(webhook.id).await?;
                        warn!(
                            webhook_id = %webhook.id,
                            batch_id = %info.batch_uuid,
                            status = status_code,
                            "Webhook delivery failed"
                        );
                    }
                    Err(e) => {
                        let error_msg = e.to_string();
                        repo.mark_failed(delivery.id, None, &error_msg, 0).await?;
                        repo.increment_failures(webhook.id).await?;
                        warn!(
                            webhook_id = %webhook.id,
                            batch_id = %info.batch_uuid,
                            error = %error_msg,
                            "Webhook delivery failed (network error)"
                        );
                    }
                }
            }
        }

        Ok(())
    }
}
