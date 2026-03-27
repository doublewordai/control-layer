//! Notification poller and webhook event processor.
//!
//! Centralizes all webhook delivery creation in a single place:
//!
//! **Polled events** (detected each tick via database queries):
//! - `batch.completed` / `batch.failed`: Polls fusillade for terminal batches
//! - `batch.created`: Polls fusillade for new batches without existing deliveries
//!
//! **Reactive events** (triggered via PostgreSQL LISTEN/NOTIFY):
//! - `user.created`: PG trigger on `users` INSERT
//! - `api_key.created`: PG trigger on `api_keys` INSERT
//!
//! The webhook dispatcher (claim → sign → send → process results) runs on
//! each tick when `webhooks.enabled` is true. Email notifications are gated
//! on `notifications.enabled` separately.
//!
//! Uses atomic `notification_sent_at` claiming to prevent duplicate
//! notifications across replicas for batch completion events. Platform events
//! use a unique partial index on `webhook_deliveries(webhook_id, event_type,
//! resource_id)` for deduplication.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use fusillade::manager::postgres::PostgresRequestManager;
use metrics::counter;
use rust_decimal::prelude::ToPrimitive;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use sqlx_pool_router::DbPools;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::NotificationsConfig;
use crate::db::handlers::repository::Repository;
use crate::db::handlers::users::{AutoTopupUser, LowBalanceUser, Users};
use crate::db::handlers::{Credits, Webhooks};
use crate::db::models::credits::{CreditTransactionCreateDBRequest, CreditTransactionType};
use crate::db::models::webhooks::WebhookDeliveryCreateDBRequest;
use crate::email::EmailService;
use crate::payment_providers::{self, PaymentProvider};
use crate::webhooks::WebhookDispatcher;
use crate::webhooks::events::{WebhookEvent, WebhookEventType};

/// PostgreSQL NOTIFY channel for webhook events (user.created, api_key.created).
const WEBHOOK_EVENT_CHANNEL: &str = "webhook_event";

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

        let created_by = &batch.created_by;
        if created_by.is_empty() {
            tracing::debug!(batch_id = %batch_id_str, "Batch has no creator, skipping notification");
            return None;
        }

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
    // Webhook dispatcher runs independently of email notifications
    let mut dispatcher = if config.webhooks.enabled {
        Some(WebhookDispatcher::spawn(dwctl_pool.clone(), &config.webhooks, shutdown.clone()))
    } else {
        None
    };

    let payment_provider: Option<Box<dyn PaymentProvider>> =
        app_config.payment.as_ref().map(|pc| payment_providers::create_provider(pc.clone()));

    let email_service = if config.enabled {
        match EmailService::new(&app_config) {
            Ok(svc) => {
                tracing::info!("Launched email service successfully");
                Some(svc)
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to create email service, email notifications disabled");
                None
            }
        }
    } else {
        None
    };

    // Set up PG listener for platform webhook events (user.created, api_key.created)
    let mut listener = if dispatcher.is_some() {
        match PgListener::connect_with(&dwctl_pool).await {
            Ok(mut l) => match l.listen(WEBHOOK_EVENT_CHANNEL).await {
                Ok(()) => {
                    tracing::info!("Listening on PG channel '{WEBHOOK_EVENT_CHANNEL}' for platform webhook events");
                    Some(l)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to subscribe to {WEBHOOK_EVENT_CHANNEL} channel");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "Failed to connect PG listener for webhook events");
                None
            }
        }
    } else {
        None
    };

    // Buffer for pending webhook event notifications received between ticks
    let mut pending_webhook_events: Vec<(String, Uuid)> = Vec::new();

    tracing::info!(
        poll_interval = ?config.poll_interval,
        notifications = config.enabled,
        webhooks = dispatcher.is_some(),
        email = email_service.is_some(),
        webhook_listener = listener.is_some(),
        "Starting notification poller"
    );

    loop {
        // Wait for either the poll interval or a webhook event notification
        tokio::select! {
            _ = tokio::time::sleep(config.poll_interval) => {}
            result = async {
                match listener.as_mut() {
                    Some(l) => l.try_recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match result {
                    Ok(Some(notification)) => {
                        if let Some((table, id)) = parse_webhook_event_payload(notification.payload()) {
                            pending_webhook_events.push((table, id));
                        }
                        // Drain any additional buffered notifications (with timeout
                        // since try_recv blocks when buffer is empty, not returns None)
                        if let Some(ref mut l) = listener {
                            while let Ok(Ok(Some(notification))) = tokio::time::timeout(Duration::from_millis(10), l.try_recv()).await {
                                if let Some((table, id)) = parse_webhook_event_payload(notification.payload()) {
                                    pending_webhook_events.push((table, id));
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        tracing::warn!("Webhook event listener connection lost, will reconnect");
                        listener = None;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Webhook event listener error, will reconnect");
                        listener = None;
                    }
                }
            }
            _ = shutdown.cancelled() => {
                tracing::info!("Notification poller shutting down");
                return;
            }
        }

        // Reconnect listener if disconnected
        if listener.is_none()
            && dispatcher.is_some()
            && let Ok(mut l) = PgListener::connect_with(&dwctl_pool).await
            && l.listen(WEBHOOK_EVENT_CHANNEL).await.is_ok()
        {
            tracing::info!("Reconnected webhook event listener");
            listener = Some(l);
        }

        tracing::debug!("Notification poller tick");

        let mut conn = match dwctl_pool.acquire().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to acquire database connection for notification poller tick");
                continue;
            }
        };

        // === Step 1: Process platform webhook events (user.created, api_key.created) ===
        if dispatcher.is_some() && !pending_webhook_events.is_empty() {
            let events = std::mem::take(&mut pending_webhook_events);
            let _ = process_platform_events(&mut conn, &events)
                .await
                .inspect_err(|e| tracing::warn!(error = %e, "Failed to process platform webhook events"));
        }

        // === Step 2: Poll fusillade for completed batches ===
        match request_manager.poll_completed_batches().await {
            Ok(batches) => {
                if !batches.is_empty() {
                    tracing::info!(count = batches.len(), "Found terminal batches to finalize");

                    let infos: Vec<_> = batches.iter().filter_map(BatchNotificationInfo::try_from_batch).collect();

                    // === Step 3: Create webhook delivery records for batch completion ===
                    if dispatcher.is_some() {
                        let _ = create_batch_deliveries(&mut conn, &infos)
                            .await
                            .inspect_err(|e| tracing::warn!(error = %e, "Failed to create webhook delivery records"));
                    }

                    // === Step 4: Send email notifications ===
                    if let Some(ref email_service) = email_service {
                        send_email_notifications(email_service, &infos, &mut conn).await;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to poll for completed batches");
            }
        }

        // === Step 5: Poll for new batches (batch.created) ===
        if dispatcher.is_some() {
            let _ = process_new_batches(&mut conn)
                .await
                .inspect_err(|e| tracing::warn!(error = %e, "Failed to process new batch webhooks"));
        }

        // === Step 6: Low-balance notifications ===
        if let Some(ref email_service) = email_service {
            // 1. Get users with thresholds
            let candidates = {
                let mut users = Users::new(&mut conn);
                users.users_with_low_balance_threshold().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to fetch low-balance threshold users");
                    vec![]
                })
            };

            if !candidates.is_empty() {
                // 2. Refresh checkpoints only for users near their threshold or missing one.
                //    Users well above threshold use the cached checkpoint balance directly.
                const REFRESH_MARGIN: rust_decimal::Decimal = rust_decimal::Decimal::from_parts(30, 0, 0, false, 0);

                let needs_refresh: Vec<Uuid> = candidates
                    .iter()
                    .filter(|u| match u.checkpoint_balance {
                        Some(b) => (b - u.low_balance_threshold) < REFRESH_MARGIN,
                        None => true,
                    })
                    .map(|u| u.id)
                    .collect();

                let refreshed = if !needs_refresh.is_empty() {
                    let mut credits = Credits::new(&mut conn);
                    credits.get_users_balances_bulk(&needs_refresh, Some(1)).await.unwrap_or_else(|e| {
                        tracing::warn!(error = %e, "Failed to refresh balances for low-balance users");
                        Default::default()
                    })
                } else {
                    Default::default()
                };

                // Look up balance: prefer refreshed, fall back to checkpoint
                let balance_for =
                    |u: &LowBalanceUser| -> Option<rust_decimal::Decimal> { refreshed.get(&u.id).copied().or(u.checkpoint_balance) };

                // 3. Send notifications for users below threshold who haven't been notified
                let to_notify: Vec<_> = candidates
                    .iter()
                    .filter(|u| !u.low_balance_notification_sent && balance_for(u).map(|b| b < u.low_balance_threshold).unwrap_or(false))
                    .collect();

                if !to_notify.is_empty() {
                    tracing::info!(count = to_notify.len(), "Found users with low balance");
                    send_low_balance_notifications(email_service, &to_notify, &balance_for, &mut conn).await;
                }

                // 4. Clear recovered flags (after emails sent so we don't lose state on failure)
                let recovered: Vec<Uuid> = candidates
                    .iter()
                    .filter(|u| u.low_balance_notification_sent && balance_for(u).map(|b| b >= u.low_balance_threshold).unwrap_or(false))
                    .map(|u| u.id)
                    .collect();

                if !recovered.is_empty() {
                    let mut users = Users::new(&mut conn);
                    let _ = users
                        .clear_low_balance_notification_sent(&recovered)
                        .await
                        .inspect_err(|e| tracing::warn!(error = %e, "Failed to clear recovered low-balance flags"));
                }
            }
        }

        // === Step 7: Auto top-up charges ===
        if let Some(ref provider) = payment_provider {
            process_auto_topups(provider.as_ref(), &mut conn, email_service.as_ref()).await;
        }

        // === Step 8: Dispatch webhooks (claim → sign → send → process results) ===
        if let Some(ref mut dispatcher) = dispatcher {
            dispatcher.tick().await;
        }
    }
}

/// Create webhook delivery records for a batch of notifications.
///
/// Deliveries are created with `next_attempt_at = now()` so the dispatcher's
/// claim mechanism picks them up immediately.
async fn create_batch_deliveries(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>,
    infos: &[BatchNotificationInfo],
) -> anyhow::Result<()> {
    if infos.is_empty() {
        return Ok(());
    }

    let user_ids: Vec<Uuid> = infos.iter().map(|i| i.user_id).collect::<HashSet<_>>().into_iter().collect();
    let webhooks_by_user = {
        let mut repo = Webhooks::new(&mut *conn);
        repo.get_enabled_webhooks_for_users(user_ids).await?
    };

    let mut repo = Webhooks::new(&mut *conn);

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
                resource_id: Some(info.batch_uuid),
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

/// Parse a webhook event NOTIFY payload.
///
/// Expected format: `"table_name:record_uuid"`
fn parse_webhook_event_payload(payload: &str) -> Option<(String, Uuid)> {
    let (table, id_str) = payload.split_once(':')?;
    let id = Uuid::parse_str(id_str).ok()?;
    Some((table.to_string(), id))
}

/// Process platform webhook events received via LISTEN/NOTIFY.
///
/// For each (table, record_id) pair, queries the source table for record details,
/// builds the webhook event payload, and creates delivery records for all eligible
/// platform webhooks.
async fn process_platform_events(conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>, events: &[(String, Uuid)]) -> anyhow::Result<()> {
    let pm_webhooks = {
        let mut repo = Webhooks::new(&mut *conn);
        repo.get_enabled_platform_webhooks().await?
    };

    if pm_webhooks.is_empty() {
        return Ok(());
    }

    for (table, id) in events {
        let (event, event_type) = match table.as_str() {
            "users" => {
                let row = sqlx::query!(r#"SELECT id, email, auth_source FROM users WHERE id = $1"#, id,)
                    .fetch_optional(&mut **conn)
                    .await?;

                let Some(row) = row else {
                    tracing::debug!(user_id = %id, "User not found for webhook event, skipping");
                    continue;
                };

                (
                    WebhookEvent::user_created(row.id, &row.email, &row.auth_source),
                    WebhookEventType::UserCreated,
                )
            }
            "api_keys" => {
                let row = sqlx::query!(r#"SELECT id, user_id, created_by, name FROM api_keys WHERE id = $1"#, id,)
                    .fetch_optional(&mut **conn)
                    .await?;

                let Some(row) = row else {
                    tracing::debug!(api_key_id = %id, "API key not found for webhook event, skipping");
                    continue;
                };

                (
                    WebhookEvent::api_key_created(row.id, row.user_id, row.created_by, &row.name),
                    WebhookEventType::ApiKeyCreated,
                )
            }
            _ => {
                tracing::warn!(table = %table, "Unknown table in webhook event notification, skipping");
                continue;
            }
        };

        let payload = serde_json::to_value(&event)?;

        let mut repo = Webhooks::new(&mut *conn);
        for webhook in pm_webhooks.iter().filter(|w| w.accepts_event(event_type)) {
            let delivery_request = WebhookDeliveryCreateDBRequest {
                webhook_id: webhook.id,
                event_id: Uuid::new_v4(),
                event_type: event_type.to_string(),
                payload: payload.clone(),
                resource_id: Some(*id),
                next_attempt_at: None,
            };

            let _ = repo.try_create_delivery(&delivery_request).await.inspect_err(|e| {
                tracing::warn!(
                    error = %e,
                    webhook_id = %webhook.id,
                    event_type = %event_type,
                    "Failed to create platform webhook delivery"
                );
            });
        }

        tracing::debug!(
            table = %table,
            resource_id = %id,
            event_type = %event_type,
            webhooks = pm_webhooks.iter().filter(|w| w.accepts_event(event_type)).count(),
            "Platform webhook event processed"
        );
    }

    Ok(())
}

/// A new batch record from fusillade for webhook processing.
struct NewBatch {
    id: Uuid,
    created_by: Option<String>,
    endpoint: String,
}

/// Poll for newly created batches and create `batch.created` webhook deliveries.
///
/// Queries fusillade for recent batches that don't yet have a `batch.created`
/// delivery record, then creates deliveries for all eligible platform webhooks.
///
/// Uses runtime-checked `sqlx::query()` because the fusillade schema is managed
/// by an external crate and not available to sqlx's compile-time validation.
async fn process_new_batches(conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>) -> anyhow::Result<()> {
    // Find recent batches without a batch.created delivery
    let rows = sqlx::query_as::<_, (Uuid, Option<String>, String)>(
        r#"
        SELECT b.id, b.created_by, b.endpoint
        FROM fusillade.batches b
        LEFT JOIN webhook_deliveries wd
            ON wd.resource_id = b.id AND wd.event_type = 'batch.created'
        WHERE wd.id IS NULL
          AND b.created_at > now() - interval '5 minutes'
          AND b.created_by IS NOT NULL
        ORDER BY b.created_at
        LIMIT 100
        "#,
    )
    .fetch_all(&mut **conn)
    .await?;

    let new_batches: Vec<NewBatch> = rows
        .into_iter()
        .map(|(id, created_by, endpoint)| NewBatch { id, created_by, endpoint })
        .collect();

    if new_batches.is_empty() {
        return Ok(());
    }

    tracing::info!(count = new_batches.len(), "Found new batches for webhook delivery");

    let pm_webhooks = {
        let mut repo = Webhooks::new(&mut *conn);
        repo.get_enabled_platform_webhooks().await?
    };

    if pm_webhooks.is_empty() {
        return Ok(());
    }

    let event_type = WebhookEventType::BatchCreated;

    for batch in &new_batches {
        let Some(ref created_by) = batch.created_by else {
            continue;
        };
        let user_id: Uuid = match created_by.parse() {
            Ok(id) => id,
            Err(_) => continue,
        };

        let event = WebhookEvent::batch_created(batch.id, user_id, &batch.endpoint);
        let payload = serde_json::to_value(&event)?;

        let mut repo = Webhooks::new(&mut *conn);
        for webhook in pm_webhooks.iter().filter(|w| w.accepts_event(event_type)) {
            let delivery_request = WebhookDeliveryCreateDBRequest {
                webhook_id: webhook.id,
                event_id: Uuid::new_v4(),
                event_type: event_type.to_string(),
                payload: payload.clone(),
                resource_id: Some(batch.id),
                next_attempt_at: None,
            };

            let _ = repo.try_create_delivery(&delivery_request).await.inspect_err(|e| {
                tracing::warn!(
                    error = %e,
                    webhook_id = %webhook.id,
                    batch_id = %batch.id,
                    "Failed to create batch.created webhook delivery"
                );
            });
        }
    }

    Ok(())
}

/// Send email notifications for completed batches.
async fn send_email_notifications(
    email_service: &EmailService,
    infos: &[BatchNotificationInfo],
    conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>,
) {
    let user_ids: Vec<Uuid> = infos.iter().map(|i| i.user_id).collect::<HashSet<_>>().into_iter().collect();

    let users_by_id = {
        let mut users = Users::new(&mut *conn);
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
            let mut users = Users::new(&mut *conn);
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

/// Process auto top-up charges for users whose balance has dropped below their threshold.
///
/// For each eligible user:
/// 1. Refreshes balance to get an accurate reading
/// 2. Checks if balance is below the user's configured threshold
/// 3. Charges the saved payment method via the payment provider
/// 4. Creates a credit transaction (idempotent via unique source_id)
///
/// **Rate limit**: At most one charge per user per minute, enforced via a deterministic
/// `source_id` of the form `auto_topup_{user_id}_{YYYY-MM-DDTHH:MM}` and the unique
/// constraint on `credits_transactions.source_id`.
async fn process_auto_topups(
    provider: &dyn PaymentProvider,
    conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>,
    email_service: Option<&EmailService>,
) {
    // 1. Get users with auto top-up configured
    let candidates = {
        let mut users = Users::new(&mut *conn);
        users.users_with_auto_topup_enabled().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to fetch auto-topup eligible users");
            vec![]
        })
    };

    if candidates.is_empty() {
        return;
    }

    // 2. Always refresh balances for auto-topup candidates.
    //    Unlike low-balance notifications (which use a margin-based heuristic), auto-topup
    //    needs accurate balances because a stale checkpoint can cause us to miss charges entirely.
    //    The candidate set is typically small, so the cost of refreshing all is negligible.
    let all_ids: Vec<Uuid> = candidates.iter().map(|u| u.id).collect();

    let refreshed = {
        let mut credits = Credits::new(&mut *conn);
        credits.get_users_balances_bulk(&all_ids, Some(1)).await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to refresh balances for auto-topup users");
            Default::default()
        })
    };

    let balance_for = |u: &AutoTopupUser| -> Option<rust_decimal::Decimal> { refreshed.get(&u.id).copied().or(u.checkpoint_balance) };

    // 3. Filter to users below their threshold
    let to_charge: Vec<_> = candidates
        .iter()
        .filter(|u| balance_for(u).map(|b| b < u.auto_topup_threshold).unwrap_or(false))
        .collect();

    if to_charge.is_empty() {
        return;
    }

    tracing::info!(count = to_charge.len(), "Found users eligible for auto top-up");

    // 4. Batch-fetch monthly auto-topup spend for users with a monthly limit
    let limit_user_ids: Vec<Uuid> = to_charge
        .iter()
        .filter(|u| u.auto_topup_monthly_limit.is_some())
        .map(|u| u.id)
        .collect();

    let monthly_spends = if !limit_user_ids.is_empty() {
        let mut credits = Credits::new(&mut *conn);
        match credits.get_monthly_auto_topup_spend_bulk(&limit_user_ids).await {
            Ok(spends) => spends,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to batch-fetch monthly auto-topup spend, aborting auto-topup run");
                counter!("dwctl_auto_topup_errors_total", "stage" => "monthly_limit_check").increment(1);
                return;
            }
        }
    } else {
        Default::default()
    };

    // 5. Charge each user
    for user in &to_charge {
        // Determine effective charge amount, capping to monthly limit headroom if applicable
        let (charge_amount, description) = if let Some(monthly_limit) = user.auto_topup_monthly_limit {
            let monthly_spend = monthly_spends.get(&user.id).copied().unwrap_or(rust_decimal::Decimal::ZERO);
            let headroom = monthly_limit - monthly_spend;

            if headroom <= rust_decimal::Decimal::ZERO {
                // Limit fully exhausted — skip and notify
                tracing::info!(
                    user_id = %user.id,
                    monthly_spend = %monthly_spend,
                    monthly_limit = %monthly_limit,
                    "Auto top-up skipped: monthly limit fully exhausted"
                );
                counter!("dwctl_auto_topup_limit_reached_total").increment(1);
                if !user.auto_topup_limit_notification_sent
                    && let Some(email_svc) = email_service
                {
                    let name = user.display_name.as_deref().unwrap_or(&user.username);
                    let balance = balance_for(user).unwrap_or_default();
                    if let Err(e) = email_svc
                        .send_auto_topup_limit_reached_email(&user.email, Some(name), &monthly_limit, &balance)
                        .await
                    {
                        tracing::warn!(user_id = %user.id, error = %e, "Failed to send auto top-up limit reached email");
                    } else {
                        let mut users = Users::new(&mut *conn);
                        let _ = users.mark_auto_topup_limit_notification_sent(&[user.id]).await.inspect_err(
                            |e| tracing::warn!(user_id = %user.id, error = %e, "Failed to mark auto-topup limit notification as sent"),
                        );
                    }
                }
                continue;
            } else if headroom < user.auto_topup_amount {
                // Partial charge — cap to remaining headroom
                tracing::info!(
                    user_id = %user.id,
                    requested = %user.auto_topup_amount,
                    capped_to = %headroom,
                    monthly_spend = %monthly_spend,
                    monthly_limit = %monthly_limit,
                    "Auto top-up capped to remaining monthly headroom"
                );
                counter!("dwctl_auto_topup_limit_reached_total").increment(1);
                (headroom, "Automatic top-up (capped by monthly limit)")
            } else {
                if user.auto_topup_limit_notification_sent {
                    // Spend is back under the limit (new month or limit raised) — clear the flag
                    let mut users = Users::new(&mut *conn);
                    let _ = users.clear_auto_topup_limit_notification_sent(&[user.id]).await.inspect_err(
                        |e| tracing::warn!(user_id = %user.id, error = %e, "Failed to clear auto-topup limit notification flag"),
                    );
                }
                (user.auto_topup_amount, "Automatic top-up")
            }
        } else {
            (user.auto_topup_amount, "Automatic top-up")
        };

        let amount_cents = (charge_amount * rust_decimal::Decimal::new(100, 0))
            .round_dp(0)
            .to_i64()
            .unwrap_or(0);

        if amount_cents <= 0 {
            tracing::warn!(user_id = %user.id, "Auto top-up amount resolved to zero cents, skipping");
            continue;
        }

        // Use a deterministic source_id so duplicate charges within a short window
        // are caught by the unique constraint on credits_transactions.source_id.
        // We include the minute so a user can be charged again in a subsequent minute if
        // they drop below threshold again (at most once per minute per user).
        let now_minute = chrono::Utc::now().format("%Y-%m-%dT%H:%M");
        let source_id = format!("auto_topup_{}_{}", user.id, now_minute);

        // Check idempotency: skip if we already charged this user this minute
        {
            let mut credits = Credits::new(&mut *conn);
            match credits.transaction_exists_by_source_id(&source_id).await {
                Ok(true) => {
                    tracing::warn!(user_id = %user.id, "Auto top-up already processed this minute, skipping");
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(user_id = %user.id, error = %e, "Failed to check auto-topup idempotency");
                    counter!("dwctl_auto_topup_errors_total", "stage" => "idempotency_check").increment(1);
                    continue;
                }
            }
        }

        // Fetch the customer's default payment method from the provider
        let payment_method_id = match provider.get_default_payment_method(&user.payment_provider_id).await {
            Ok(Some(pm_id)) => pm_id,
            Ok(None) => {
                tracing::warn!(user_id = %user.id, "No default payment method found, skipping auto top-up");
                counter!("dwctl_auto_topup_charge_failures_total").increment(1);
                if let Some(email_svc) = email_service {
                    let name = user.display_name.as_deref().unwrap_or(&user.username);
                    if let Err(e) = email_svc
                        .send_auto_topup_failed_email(&user.email, Some(name), &user.auto_topup_amount, &user.auto_topup_threshold)
                        .await
                    {
                        tracing::warn!(user_id = %user.id, error = %e, "Failed to send auto top-up failure email");
                    }
                }
                continue;
            }
            Err(e) => {
                tracing::warn!(user_id = %user.id, error = %e, "Failed to fetch default payment method");
                counter!("dwctl_auto_topup_errors_total", "stage" => "payment_method_lookup").increment(1);
                continue;
            }
        };

        // Charge the payment provider
        let payment_intent_id = match provider
            .charge_auto_topup(amount_cents, &user.payment_provider_id, &payment_method_id, &source_id)
            .await
        {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(
                    user_id = %user.id,
                    error = %e,
                    "Failed to charge auto top-up"
                );
                counter!("dwctl_auto_topup_charge_failures_total").increment(1);
                if let Some(email_svc) = email_service {
                    let name = user.display_name.as_deref().unwrap_or(&user.username);
                    if let Err(e) = email_svc
                        .send_auto_topup_failed_email(&user.email, Some(name), &user.auto_topup_amount, &user.auto_topup_threshold)
                        .await
                    {
                        tracing::warn!(user_id = %user.id, error = %e, "Failed to send auto top-up failure email");
                    }
                }
                continue;
            }
        };

        // Create the credit transaction
        let request = CreditTransactionCreateDBRequest {
            user_id: user.id,
            transaction_type: CreditTransactionType::Purchase,
            amount: charge_amount,
            source_id,
            description: Some(description.to_string()),
            fusillade_batch_id: None,
            api_key_id: None,
        };

        let mut credits = Credits::new(&mut *conn);
        match credits.create_transaction(&request).await {
            Ok(_) => {
                tracing::info!(
                    user_id = %user.id,
                    amount = %charge_amount,
                    "Auto top-up charged successfully"
                );
                counter!("dwctl_auto_topup_success_total").increment(1);
                if let Some(email_svc) = email_service {
                    let name = user.display_name.as_deref().unwrap_or(&user.username);
                    let new_balance = balance_for(user).unwrap_or_default() + charge_amount;
                    if let Err(e) = email_svc
                        .send_auto_topup_success_email(&user.email, Some(name), &charge_amount, &user.auto_topup_threshold, &new_balance)
                        .await
                    {
                        tracing::warn!(user_id = %user.id, error = %e, "Failed to send auto top-up success email");
                    }
                }
            }
            Err(crate::db::errors::DbError::UniqueViolation { constraint, .. })
                if constraint.as_deref() == Some("credits_transactions_source_id_unique") =>
            {
                // Another poller instance already inserted the credit — this is fine.
                tracing::info!(user_id = %user.id, "Auto top-up credit transaction already exists (race), treating as success");
            }
            Err(e) => {
                tracing::error!(
                    user_id = %user.id,
                    payment_intent_id = %payment_intent_id,
                    amount = %user.auto_topup_amount,
                    error = %e,
                    "CRITICAL: Auto top-up payment succeeded but credit transaction failed. \
                     User was charged but did not receive credits. Manual reconciliation required."
                );
                counter!("dwctl_auto_topup_credit_failures_total").increment(1);
                // Do NOT send "payment failed" email here — the payment actually succeeded.
                // The user was charged but credits weren't recorded due to a DB error.
                // The CRITICAL log + metric above should trigger an alert for manual reconciliation.
            }
        }
    }
}

/// Send low-balance notification emails to the given users.
async fn send_low_balance_notifications(
    email_service: &EmailService,
    users: &[&LowBalanceUser],
    balance_for: &impl Fn(&LowBalanceUser) -> Option<rust_decimal::Decimal>,
    conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>,
) {
    let mut sent_ids = Vec::new();

    for user in users {
        let Some(balance) = balance_for(user) else { continue };
        let name = user.display_name.as_deref().unwrap_or(&user.username);

        if let Err(e) = email_service.send_low_balance_email(&user.email, Some(name), &balance).await {
            tracing::warn!(
                user_id = %user.id,
                email = %user.email,
                error = %e,
                "Failed to send low-balance notification email"
            );
            continue;
        }

        tracing::info!(user_id = %user.id, email = %user.email, balance = %balance, "Sent low-balance notification");
        sent_ids.push(user.id);
    }

    // Bulk-mark all successfully sent notifications
    if !sent_ids.is_empty() {
        let mut users_repo = Users::new(&mut *conn);
        if let Err(e) = users_repo.mark_low_balance_notification_sent(&sent_ids).await {
            tracing::warn!(error = %e, "Failed to mark low-balance notifications as sent");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::users::Role;
    use crate::config::DummyConfig;
    use crate::payment_providers;
    use rust_decimal::Decimal;
    use sqlx::PgPool;

    #[sqlx::test]
    async fn test_process_auto_topups_charges_below_threshold(pool: PgPool) {
        // Create a test user with auto top-up configured
        let user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

        // Set up auto top-up: threshold $10, amount $25, with payment IDs
        sqlx::query!(
            r#"UPDATE users SET
                auto_topup_amount = 25.0,
                auto_topup_threshold = 10.0,
                payment_provider_id = 'cus_test_456'
            WHERE id = $1"#,
            user.id
        )
        .execute(&pool)
        .await
        .unwrap();

        // User has $0 balance (below $10 threshold) — should trigger charge

        let provider = payment_providers::create_provider(crate::config::PaymentConfig::Dummy(DummyConfig {
            amount: Decimal::new(100, 0),
        }));

        let mut conn = pool.acquire().await.unwrap();
        process_auto_topups(provider.as_ref(), &mut conn, None).await;

        // Verify a credit transaction was created
        let txn = sqlx::query!("SELECT amount, source_id FROM credits_transactions WHERE user_id = $1", user.id)
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(txn.amount, Decimal::new(2500, 2)); // $25.00
        assert!(txn.source_id.starts_with("auto_topup_"), "source_id should start with auto_topup_");
        assert!(txn.source_id.contains(&user.id.to_string()), "source_id should contain user ID");
    }

    #[sqlx::test]
    async fn test_process_auto_topups_skips_above_threshold(pool: PgPool) {
        let user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

        // Set up auto top-up: threshold $5, amount $25
        sqlx::query!(
            r#"UPDATE users SET
                auto_topup_amount = 25.0,
                auto_topup_threshold = 5.0,
                payment_provider_id = 'cus_test_456'
            WHERE id = $1"#,
            user.id
        )
        .execute(&pool)
        .await
        .unwrap();

        // Give user $50 balance (well above $5 threshold)
        let mut conn = pool.acquire().await.unwrap();
        {
            let mut credits = Credits::new(&mut conn);
            credits
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: user.id,
                    transaction_type: CreditTransactionType::Purchase,
                    amount: Decimal::new(5000, 2),
                    source_id: "seed_balance".to_string(),
                    description: Some("Test seed".to_string()),
                    fusillade_batch_id: None,
                    api_key_id: None,
                })
                .await
                .unwrap();
        }

        let provider = payment_providers::create_provider(crate::config::PaymentConfig::Dummy(DummyConfig {
            amount: Decimal::new(100, 0),
        }));

        process_auto_topups(provider.as_ref(), &mut conn, None).await;

        // Should only have the seed transaction, no auto-topup
        let count = sqlx::query!("SELECT COUNT(*) as count FROM credits_transactions WHERE user_id = $1", user.id)
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(count.count.unwrap(), 1, "Should only have the seed transaction");
    }

    #[sqlx::test]
    async fn test_process_auto_topups_idempotent(pool: PgPool) {
        let user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

        sqlx::query!(
            r#"UPDATE users SET
                auto_topup_amount = 25.0,
                auto_topup_threshold = 10.0,
                payment_provider_id = 'cus_test_456'
            WHERE id = $1"#,
            user.id
        )
        .execute(&pool)
        .await
        .unwrap();

        let provider = payment_providers::create_provider(crate::config::PaymentConfig::Dummy(DummyConfig {
            amount: Decimal::new(100, 0),
        }));

        // Run twice
        let mut conn = pool.acquire().await.unwrap();
        process_auto_topups(provider.as_ref(), &mut conn, None).await;
        process_auto_topups(provider.as_ref(), &mut conn, None).await;

        // Should only have one transaction (idempotent via source_id)
        let count = sqlx::query!(
            "SELECT COUNT(*) as count FROM credits_transactions WHERE user_id = $1 AND source_id LIKE 'auto_topup_%'",
            user.id
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(
            count.count.unwrap(),
            1,
            "Should only create one transaction per minute (idempotent)"
        );
    }

    #[sqlx::test]
    async fn test_process_auto_topups_respects_monthly_limit(pool: PgPool) {
        let user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

        // Set up auto top-up with a $50 monthly limit
        sqlx::query!(
            r#"UPDATE users SET
                auto_topup_amount = 25.0,
                auto_topup_threshold = 10.0,
                auto_topup_monthly_limit = 50.0,
                payment_provider_id = 'cus_test_456'
            WHERE id = $1"#,
            user.id
        )
        .execute(&pool)
        .await
        .unwrap();

        // Seed an existing auto top-up transaction for $25 this month,
        // then drain the balance below threshold so the next charge triggers
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            credits
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: user.id,
                    transaction_type: CreditTransactionType::Purchase,
                    amount: Decimal::new(2500, 2), // $25
                    source_id: format!("auto_topup_{}_2026-03-01T00:00", user.id),
                    description: Some("Auto top-up".to_string()),
                    fusillade_batch_id: None,
                    api_key_id: None,
                })
                .await
                .unwrap();
            // Drain balance below threshold via admin removal ($20 removed, leaving $5 < $10 threshold)
            credits
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: user.id,
                    transaction_type: CreditTransactionType::AdminRemoval,
                    amount: Decimal::new(2000, 2), // $20 removal, leaving $5 balance
                    source_id: "drain_1".to_string(),
                    description: Some("Drain".to_string()),
                    fusillade_batch_id: None,
                    api_key_id: None,
                })
                .await
                .unwrap();
        }

        // Monthly spend is $25, limit is $50, next charge is $25 -> $25 + $25 = $50 <= $50 -> should charge
        let provider = payment_providers::create_provider(crate::config::PaymentConfig::Dummy(DummyConfig {
            amount: Decimal::new(100, 0),
        }));

        let mut conn = pool.acquire().await.unwrap();
        process_auto_topups(provider.as_ref(), &mut conn, None).await;

        let count = sqlx::query!(
            "SELECT COUNT(*) as count FROM credits_transactions WHERE user_id = $1 AND source_id LIKE 'auto_topup_%'",
            user.id
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count.count.unwrap(), 2, "Should have charged (total $50 = limit)");
    }

    #[sqlx::test]
    async fn test_process_auto_topups_blocks_when_limit_exceeded(pool: PgPool) {
        let user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

        // Set up auto top-up with a $40 monthly limit
        sqlx::query!(
            r#"UPDATE users SET
                auto_topup_amount = 25.0,
                auto_topup_threshold = 10.0,
                auto_topup_monthly_limit = 40.0,
                payment_provider_id = 'cus_test_456'
            WHERE id = $1"#,
            user.id
        )
        .execute(&pool)
        .await
        .unwrap();

        // Seed an existing auto top-up transaction for $25 this month,
        // then drain the balance below threshold so the charge would trigger
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            credits
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: user.id,
                    transaction_type: CreditTransactionType::Purchase,
                    amount: Decimal::new(2500, 2), // $25
                    source_id: format!("auto_topup_{}_2026-03-01T00:00", user.id),
                    description: Some("Auto top-up".to_string()),
                    fusillade_batch_id: None,
                    api_key_id: None,
                })
                .await
                .unwrap();
            // Drain balance below threshold via admin removal ($20 removed, leaving $5 < $10 threshold)
            credits
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: user.id,
                    transaction_type: CreditTransactionType::AdminRemoval,
                    amount: Decimal::new(2000, 2), // $20 removal, leaving $5 balance
                    source_id: "drain_2".to_string(),
                    description: Some("Drain".to_string()),
                    fusillade_batch_id: None,
                    api_key_id: None,
                })
                .await
                .unwrap();
        }

        // Monthly spend is $25, limit is $40, next charge is $25 -> would exceed, so should charge partial: $40 - $25 = $15
        let provider = payment_providers::create_provider(crate::config::PaymentConfig::Dummy(DummyConfig {
            amount: Decimal::new(100, 0),
        }));

        let mut conn = pool.acquire().await.unwrap();
        process_auto_topups(provider.as_ref(), &mut conn, None).await;

        // Should have charged a partial amount ($15) instead of skipping
        let rows = sqlx::query!(
            "SELECT amount, description FROM credits_transactions WHERE user_id = $1 AND source_id LIKE 'auto_topup_%' ORDER BY created_at",
            user.id
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2, "Should have charged partial amount to stay within limit");
        // Second transaction should be the capped amount ($15 = $40 limit - $25 already spent)
        assert_eq!(rows[1].amount, Decimal::new(1500, 2));
        assert_eq!(rows[1].description.as_deref(), Some("Automatic top-up (capped by monthly limit)"));
    }

    #[sqlx::test]
    async fn test_process_auto_topups_skips_when_limit_fully_exhausted(pool: PgPool) {
        let user = crate::test::utils::create_test_user(&pool, Role::StandardUser).await;

        // Set up auto top-up with a $25 monthly limit (exactly what we've already spent)
        sqlx::query!(
            r#"UPDATE users SET
                auto_topup_amount = 25.0,
                auto_topup_threshold = 10.0,
                auto_topup_monthly_limit = 25.0,
                payment_provider_id = 'cus_test_456'
            WHERE id = $1"#,
            user.id
        )
        .execute(&pool)
        .await
        .unwrap();

        // Seed an existing auto top-up transaction for $25 this month,
        // then drain the balance below threshold
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut credits = Credits::new(&mut conn);
            credits
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: user.id,
                    transaction_type: CreditTransactionType::Purchase,
                    amount: Decimal::new(2500, 2), // $25
                    source_id: format!("auto_topup_{}_2026-03-01T00:00", user.id),
                    description: Some("Auto top-up".to_string()),
                    fusillade_batch_id: None,
                    api_key_id: None,
                })
                .await
                .unwrap();
            credits
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: user.id,
                    transaction_type: CreditTransactionType::AdminRemoval,
                    amount: Decimal::new(2000, 2),
                    source_id: "drain_3".to_string(),
                    description: Some("Drain".to_string()),
                    fusillade_batch_id: None,
                    api_key_id: None,
                })
                .await
                .unwrap();
        }

        // Monthly spend is $25, limit is $25, headroom = $0 -> should skip entirely
        let provider = payment_providers::create_provider(crate::config::PaymentConfig::Dummy(DummyConfig {
            amount: Decimal::new(100, 0),
        }));

        let mut conn = pool.acquire().await.unwrap();
        process_auto_topups(provider.as_ref(), &mut conn, None).await;

        let count = sqlx::query!(
            "SELECT COUNT(*) as count FROM credits_transactions WHERE user_id = $1 AND source_id LIKE 'auto_topup_%'",
            user.id
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            count.count.unwrap(),
            1,
            "Should NOT have charged (limit fully exhausted, zero headroom)"
        );
    }
}
