//! Batch completion notification poller.
//!
//! Polls fusillade for completed/failed/cancelled batches and sends notifications
//! to batch creators. When webhooks are enabled, delivers webhooks; otherwise sends
//! email notifications. Uses atomic `notification_sent_at` claiming to prevent
//! duplicate notifications across replicas.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use fusillade::manager::postgres::PostgresRequestManager;
use sqlx::PgPool;
use sqlx_pool_router::DbPools;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::NotificationsConfig;
use crate::db::handlers::repository::Repository;
use crate::db::handlers::users::Users;
use crate::email::EmailService;
use crate::webhooks::WebhookService;

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
    let webhook_service = if config.webhooks.enabled {
        Some(WebhookService::new(dwctl_pool.clone(), config.webhooks.clone()))
    } else {
        None
    };

    let email_service = match EmailService::new(&app_config) {
        Ok(svc) => Some(svc),
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

        match request_manager.poll_completed_batches().await {
            Ok(batches) => {
                if batches.is_empty() {
                    continue;
                }
                tracing::info!(count = batches.len(), "Found batches needing notification");

                let infos: Vec<_> = batches.iter().filter_map(BatchNotificationInfo::try_from_batch).collect();

                // Deliver webhooks (service fetches its own webhook configs)
                if let Some(ref webhook_service) = webhook_service
                    && let Err(e) = webhook_service.send_batch_webhooks(&infos).await
                {
                    tracing::warn!(error = %e, "Failed to deliver batch webhooks");
                }

                // Send email notifications
                if let Some(ref email_service) = email_service {
                    let user_ids: Vec<Uuid> = infos.iter().map(|i| i.user_id).collect::<HashSet<_>>().into_iter().collect();

                    let mut conn = match dwctl_pool.acquire().await {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(error = %e, "Failed to acquire database connection");
                            continue;
                        }
                    };

                    let users_by_id = {
                        let mut users = Users::new(&mut conn);
                        match users.get_bulk(user_ids).await {
                            Ok(u) => u,
                            Err(e) => {
                                tracing::warn!(error = %e, "Failed to fetch users for email notifications");
                                continue;
                            }
                        }
                    };

                    for info in &infos {
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
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to poll for completed batches");
            }
        }
    }
}
