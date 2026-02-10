//! Batch completion notification poller.
//!
//! Polls fusillade for completed/failed/cancelled batches and sends email notifications
//! to batch creators. Uses atomic `notification_sent_at` claiming to prevent duplicate
//! emails across replicas.

use std::sync::Arc;

use fusillade::manager::postgres::PostgresRequestManager;
use sqlx::PgPool;
use sqlx_pool_router::DbPools;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::NotificationsConfig;
use crate::db::handlers::repository::Repository;
use crate::db::handlers::users::Users;
use crate::email::{BatchCompletionInfo, BatchOutcome, EmailService};

pub async fn run_notification_poller(
    config: NotificationsConfig,
    app_config: crate::config::Config,
    request_manager: Arc<PostgresRequestManager<DbPools, fusillade::http::ReqwestHttpClient>>,
    dwctl_pool: PgPool,
    shutdown: CancellationToken,
) {
    let email_service = match EmailService::new(&app_config) {
        Ok(svc) => svc,
        Err(e) => {
            tracing::error!(error = %e, "Failed to create email service for notifications, disabling");
            return;
        }
    };

    tracing::info!(
        poll_interval = ?config.poll_interval,
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

                // Collect unique creator user IDs and bulk-fetch from dwctl
                let user_ids: Vec<Uuid> = batches
                    .iter()
                    .filter_map(|n| n.batch.created_by.as_ref()?.parse().ok())
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();

                let mut conn = match dwctl_pool.acquire().await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to acquire connection for notifications");
                        continue;
                    }
                };

                let users_by_id = {
                    let mut users = Users::new(&mut conn);
                    match users.get_bulk(user_ids).await {
                        Ok(map) => map,
                        Err(e) => {
                            tracing::warn!(error = %e, "Failed to bulk-fetch users for notifications");
                            continue;
                        }
                    }
                };

                for notif in batches {
                    let batch = &notif.batch;
                    let batch_id_str = batch.id.to_string();
                    let created_by = match &batch.created_by {
                        Some(id) => id.clone(),
                        None => {
                            tracing::debug!(batch_id = %batch_id_str, "Batch has no creator, skipping notification");
                            continue;
                        }
                    };

                    let user_id: Uuid = match created_by.parse() {
                        Ok(id) => id,
                        Err(_) => {
                            tracing::warn!(batch_id = %batch_id_str, created_by = %created_by, "Invalid creator UUID, skipping notification");
                            continue;
                        }
                    };

                    let user = match users_by_id.get(&user_id) {
                        Some(u) => u,
                        None => {
                            tracing::debug!(batch_id = %batch_id_str, user_id = %user_id, "Creator not found, skipping notification");
                            continue;
                        }
                    };

                    let outcome = if batch.completed_at.is_none() && batch.failed_at.is_none() {
                        tracing::warn!(batch_id = %batch_id_str, "Batch has no outcome, skipping notification");
                        continue;
                    } else if batch.failed_requests == 0 {
                        BatchOutcome::Completed
                    } else if batch.completed_requests == 0 {
                        BatchOutcome::Failed
                    } else {
                        BatchOutcome::PartiallyCompleted
                    };

                    // First-batch email only applies to successful batches
                    let is_first_batch = !user.first_batch_email_sent && outcome == BatchOutcome::Completed;

                    if !is_first_batch && !user.batch_notifications_enabled {
                        tracing::debug!(batch_id = %batch_id_str, user_id = %user_id, "User has notifications disabled, skipping");
                        continue;
                    }

                    let finished_at = batch.completed_at.or(batch.failed_at);

                    let info = BatchCompletionInfo {
                        batch_id: format!("{}", *batch.id),
                        endpoint: batch.endpoint.clone(),
                        model: notif.model.clone(),
                        outcome,
                        created_at: batch.created_at,
                        finished_at,
                        total_requests: batch.total_requests,
                        completed_requests: batch.completed_requests,
                        failed_requests: batch.failed_requests,
                        dashboard_url: config.dashboard_url.clone(),
                        completion_window: batch.completion_window.clone(),
                        filename: notif.input_file_name.clone(),
                        description: notif.input_file_description.clone(),
                    };

                    let name = user.display_name.as_deref().unwrap_or(&user.username);

                    let send_result = email_service
                        .send_batch_completion_email(&user.email, Some(name), &info, is_first_batch)
                        .await;

                    if let Err(e) = send_result {
                        tracing::warn!(
                            batch_id = %batch_id_str,
                            email = %user.email,
                            error = %e,
                            first_batch = is_first_batch,
                            "Failed to send batch completion email"
                        );
                    } else {
                        tracing::debug!(
                            batch_id = %batch_id_str,
                            email = %user.email,
                            outcome = ?outcome,
                            first_batch = is_first_batch,
                            "Sent batch completion notification"
                        );

                        if is_first_batch {
                            let mut users = Users::new(&mut conn);
                            if let Err(e) = users.mark_first_batch_email_sent(user_id).await {
                                tracing::warn!(
                                    user_id = %user_id,
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
