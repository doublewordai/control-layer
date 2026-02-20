//! Batch completion notification poller.
//!
//! Polls fusillade for completed/failed/cancelled batches and:
//! 1. Creates webhook delivery records for matching webhooks
//! 2. Ticks the webhook dispatcher (claim → sign → send → process results)
//! 3. Sends email notifications
//!
//! Uses atomic `notification_sent_at` claiming to prevent duplicate
//! notifications across replicas.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use fusillade::manager::postgres::PostgresRequestManager;
use sqlx::PgPool;
use sqlx_pool_router::DbPools;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::NotificationsConfig;
use crate::db::handlers::Webhooks;
use crate::db::handlers::repository::Repository;
use crate::db::handlers::users::Users;
use crate::db::models::webhooks::WebhookDeliveryCreateDBRequest;
use crate::email::EmailService;
use crate::webhooks::WebhookDispatcher;
use crate::webhooks::events::{WebhookEvent, WebhookEventType};

/// Outcome of a completed batch for notification purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchOutcome {
    Completed,
    PartiallyCompleted,
    Failed,
}

/// Unified batch notification info used by both email and webhook delivery.
pub struct BatchNotificationInfo {
    pub batch_id: String,
    pub batch_uuid: Uuid,
    pub user_id: Uuid,
    pub endpoint: String,
    pub model: String,
    pub outcome: BatchOutcome,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub total_requests: i64,
    pub completed_requests: i64,
    pub failed_requests: i64,
    pub cancelled_requests: i64,
    pub completion_window: String,
    pub filename: Option<String>,
    pub description: Option<String>,
    pub output_file_id: Option<Uuid>,
    pub error_file_id: Option<Uuid>,
}

impl BatchNotificationInfo {
    /// Build from a fusillade batch notification, returning `None` for batches
    /// that can't be notified about (no creator, invalid UUID, no outcome).
    fn try_from_batch(notif: &fusillade::batch::BatchNotification) -> Option<Self> {
        let batch = &notif.batch;
        let batch_id_str = batch.id.to_string();

        let created_by = match &batch.created_by {
            Some(id) => id.clone(),
            None => {
                tracing::debug!(batch_id = %batch_id_str, "Batch has no creator, skipping notification");
                return None;
            }
        };

        let user_id: Uuid = match created_by.parse() {
            Ok(id) => id,
            Err(_) => {
                tracing::warn!(batch_id = %batch_id_str, created_by = %created_by, "Invalid creator UUID, skipping notification");
                return None;
            }
        };

        let outcome = if batch.completed_at.is_none() && batch.failed_at.is_none() {
            tracing::warn!(batch_id = %batch_id_str, "Batch has no outcome, skipping notification");
            return None;
        } else if batch.failed_requests == 0 {
            BatchOutcome::Completed
        } else if batch.completed_requests == 0 {
            BatchOutcome::Failed
        } else {
            BatchOutcome::PartiallyCompleted
        };

        Some(Self {
            batch_id: format!("{}", *batch.id),
            batch_uuid: *batch.id,
            user_id,
            endpoint: batch.endpoint.clone(),
            model: notif.model.clone(),
            outcome,
            created_at: batch.created_at,
            finished_at: batch.completed_at.or(batch.failed_at),
            total_requests: batch.total_requests,
            completed_requests: batch.completed_requests,
            failed_requests: batch.failed_requests,
            cancelled_requests: batch.canceled_requests,
            completion_window: batch.completion_window.clone(),
            filename: notif.input_file_name.clone(),
            description: notif.input_file_description.clone(),
            output_file_id: batch.output_file_id.map(|f| f.0),
            error_file_id: batch.error_file_id.map(|f| f.0),
        })
    }
}

pub async fn run_notification_poller(
    config: NotificationsConfig,
    app_config: crate::config::Config,
    request_manager: Arc<PostgresRequestManager<DbPools, fusillade::http::ReqwestHttpClient>>,
    dwctl_pool: PgPool,
    shutdown: CancellationToken,
) {
    let mut dispatcher = if config.webhooks.enabled {
        Some(WebhookDispatcher::spawn(dwctl_pool.clone(), &config.webhooks, shutdown.clone()))
    } else {
        None
    };

    let email_service = match EmailService::new(&app_config) {
        Ok(svc) => {
            tracing::info!("Launched email service successfully");
            Some(svc)
        },
        Err(e) => {
            tracing::warn!(error = %e, "Failed to create email service, email notifications disabled");
            None
        }
    };

    tracing::info!(
        poll_interval = ?config.poll_interval,
        webhooks = config.webhooks.enabled,
        email = email_service.is_some(),
        "Starting batch notification poller"
    );

    loop {
        tokio::select! {
            _ = tokio::time::sleep(config.poll_interval) => {}
            _ = shutdown.cancelled() => {
                tracing::info!("Batch notification poller shutting down");
                return;
            }
        }

        tracing::debug!("Notification poller tick");

        // === Step 1: Poll fusillade for completed batches ===
        match request_manager.poll_completed_batches().await {
            Ok(batches) => {
                if !batches.is_empty() {
                    tracing::info!(count = batches.len(), "Found batches needing notification");

                    let infos: Vec<_> = batches.iter().filter_map(BatchNotificationInfo::try_from_batch).collect();

                    // === Step 2: Create webhook delivery records ===
                    if dispatcher.is_some() {
                        let _ = create_batch_deliveries(&dwctl_pool, &infos)
                            .await
                            .inspect_err(|e| tracing::warn!(error = %e, "Failed to create webhook delivery records"));
                    }

                    // === Step 3: Send email notifications ===
                    if let Some(ref email_service) = email_service {
                        send_email_notifications(email_service, &infos, &dwctl_pool).await;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to poll for completed batches");
            }
        }

        // === Step 4: Dispatch webhooks (claim → sign → send → process results) ===
        if let Some(ref mut dispatcher) = dispatcher {
            dispatcher.tick().await;
        }
    }
}

/// Create webhook delivery records for a batch of notifications.
///
/// Deliveries are created with `next_attempt_at = now()` so the dispatcher's
/// claim mechanism picks them up immediately.
async fn create_batch_deliveries(pool: &PgPool, infos: &[BatchNotificationInfo]) -> anyhow::Result<()> {
    if infos.is_empty() {
        return Ok(());
    }

    let mut conn = pool.acquire().await?;

    let user_ids: Vec<Uuid> = infos.iter().map(|i| i.user_id).collect::<HashSet<_>>().into_iter().collect();
    let webhooks_by_user = {
        let mut repo = Webhooks::new(&mut conn);
        repo.get_enabled_webhooks_for_users(user_ids).await?
    };

    let mut repo = Webhooks::new(&mut conn);

    for info in infos {
        let Some(webhooks) = webhooks_by_user.get(&info.user_id) else {
            tracing::debug!(user_id = %info.user_id, "No webhooks configured, skipping");
            continue;
        };

        let webhook_status = match info.outcome {
            BatchOutcome::Completed | BatchOutcome::PartiallyCompleted => WebhookEventType::BatchCompleted,
            BatchOutcome::Failed => WebhookEventType::BatchFailed,
        };

        let webhook_event = WebhookEvent::batch_terminal(webhook_status, info);
        let payload_json = serde_json::to_value(&webhook_event)?;

        for webhook in webhooks.iter().filter(|w| w.accepts_event(webhook_status)) {
            let event_id = Uuid::new_v4();

            let delivery_request = WebhookDeliveryCreateDBRequest {
                webhook_id: webhook.id,
                event_id,
                event_type: webhook_status.to_string(),
                payload: payload_json.clone(),
                batch_id: info.batch_uuid,
                next_attempt_at: None, // defaults to now() — claimed immediately
            };

            repo.create_delivery(&delivery_request).await?;

            tracing::debug!(
                webhook_id = %webhook.id,
                batch_id = %info.batch_uuid,
                "Webhook delivery record created"
            );
        }
    }

    Ok(())
}

/// Send email notifications for completed batches.
async fn send_email_notifications(email_service: &EmailService, infos: &[BatchNotificationInfo], pool: &PgPool) {
    let user_ids: Vec<Uuid> = infos.iter().map(|i| i.user_id).collect::<HashSet<_>>().into_iter().collect();

    let mut conn = match pool.acquire().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to acquire database connection");
            return;
        }
    };

    let users_by_id = {
        let mut users = Users::new(&mut conn);
        match users.get_bulk(user_ids).await {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to fetch users for email notifications");
                return;
            }
        }
    };

    for info in infos {
        let Some(user) = users_by_id.get(&info.user_id) else {
            continue;
        };

        let is_first_batch = !user.first_batch_email_sent && info.outcome == BatchOutcome::Completed;

        if !is_first_batch && !user.batch_notifications_enabled {
            continue;
        }

        let name = user.display_name.as_deref().unwrap_or(&user.username);

        if let Err(e) = email_service
            .send_batch_completion_email(&user.email, Some(name), info, is_first_batch)
            .await
        {
            tracing::warn!(
                batch_id = %info.batch_id,
                email = %user.email,
                error = %e,
                first_batch = is_first_batch,
                "Failed to send batch completion email"
            );
            continue;
        }

        tracing::debug!(
            batch_id = %info.batch_id,
            email = %user.email,
            outcome = ?info.outcome,
            first_batch = is_first_batch,
            "Sent batch completion notification"
        );

        if is_first_batch {
            let mut users = Users::new(&mut conn);
            if let Err(e) = users.mark_first_batch_email_sent(info.user_id).await {
                tracing::warn!(
                    user_id = %info.user_id,
                    error = %e,
                    "Failed to mark first batch email as sent"
                );
            }
        }
    }
}
