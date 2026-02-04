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
use crate::email::{BatchCompletionInfo, EmailService};
use crate::errors::Error;

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

        match request_manager.poll_completed_batches(config.hide_retriable_before_sla).await {
            Ok(batches) => {
                if !batches.is_empty() {
                    tracing::info!(count = batches.len(), "Found batches needing notification");
                }
                for batch in batches {
                    let batch_id_str = batch.id.to_string();
                    let created_by = match &batch.created_by {
                        Some(id) => id.clone(),
                        None => {
                            tracing::debug!(batch_id = %batch_id_str, "Batch has no creator, skipping notification");
                            continue;
                        }
                    };

                    // Look up creator's email from dwctl users table
                    let user_id: Uuid = match created_by.parse() {
                        Ok(id) => id,
                        Err(_) => {
                            tracing::warn!(batch_id = %batch_id_str, created_by = %created_by, "Invalid creator UUID, skipping notification");
                            continue;
                        }
                    };

                    let (email, display_name, notifications_enabled) = match get_user_info(&dwctl_pool, user_id).await {
                        Ok(Some(info)) => info,
                        Ok(None) => {
                            tracing::debug!(batch_id = %batch_id_str, user_id = %user_id, "Creator not found, skipping notification");
                            continue;
                        }
                        Err(e) => {
                            tracing::warn!(batch_id = %batch_id_str, error = %e, "Failed to look up creator, skipping notification");
                            continue;
                        }
                    };

                    if !notifications_enabled {
                        tracing::debug!(batch_id = %batch_id_str, user_id = %user_id, "User has notifications disabled, skipping");
                        continue;
                    }

                    // Determine status
                    let status = if batch.cancelled_at.is_some() {
                        "cancelled"
                    } else if batch.completed_at.is_some() {
                        "completed"
                    } else if batch.failed_at.is_some() {
                        "failed"
                    } else {
                        "completed"
                    };

                    let finished_at = batch.completed_at.or(batch.failed_at).or(batch.cancelled_at);

                    let full_batch_id = format!("{}", *batch.id);
                    let dashboard_link = format!("{}/batches/{}", config.dashboard_url.trim_end_matches('/'), full_batch_id);

                    let info = BatchCompletionInfo {
                        batch_id: full_batch_id,
                        endpoint: batch.endpoint.clone(),
                        status: status.to_string(),
                        created_at: batch.created_at,
                        finished_at,
                        total_requests: batch.total_requests,
                        completed_requests: batch.completed_requests,
                        failed_requests: batch.failed_requests,
                        canceled_requests: batch.canceled_requests,
                        dashboard_link,
                    };

                    if let Err(e) = email_service
                        .send_batch_completion_email(&email, display_name.as_deref(), &info)
                        .await
                    {
                        tracing::warn!(
                            batch_id = %batch_id_str,
                            email = %email,
                            error = %e,
                            "Failed to send batch completion email"
                        );
                    } else {
                        tracing::info!(
                            batch_id = %batch_id_str,
                            email = %email,
                            status = %status,
                            "Sent batch completion notification"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to poll for completed batches");
            }
        }
    }
}

/// Look up a user's email, display name, and notification preference by their UUID.
async fn get_user_info(pool: &PgPool, user_id: Uuid) -> Result<Option<(String, Option<String>, bool)>, Error> {
    let mut conn = pool.acquire().await.map_err(|e| Error::Internal {
        operation: format!("acquire connection for user lookup: {e}"),
    })?;

    let mut users = Users::new(&mut conn);
    match users.get_by_id(user_id).await {
        Ok(Some(user)) => Ok(Some((user.email, user.display_name, user.batch_notifications_enabled))),
        Ok(None) => Ok(None),
        Err(e) => Err(Error::Internal {
            operation: format!("look up user {user_id}: {e}"),
        }),
    }
}
