//! Fire-and-forget platform event delivery.
//!
//! Provides [`emit_platform_event`] for creating webhook delivery records
//! for platform-scoped events. Follows the same fire-and-forget pattern as
//! `maybe_update_last_login` — the calling handler doesn't wait for delivery
//! creation, and failures are logged but never propagate to the API response.

use sqlx::PgPool;
use uuid::Uuid;

use crate::db::handlers::Webhooks;
use crate::db::models::webhooks::WebhookDeliveryCreateDBRequest;

use super::events::{WebhookEvent, WebhookEventType};

/// Emit a platform event to all eligible PlatformManager webhooks.
///
/// This spawns a background task that:
/// 1. Queries all enabled platform-scoped webhooks owned by PlatformManagers
/// 2. Filters by event type subscription
/// 3. Creates delivery records for the webhook dispatcher to pick up
///
/// The delivery records are processed by the existing dispatcher on its
/// next tick (~30s polling interval).
pub fn emit_platform_event(pool: &PgPool, event: WebhookEvent, event_type: WebhookEventType, resource_id: Option<Uuid>) {
    let pool = pool.clone();
    tokio::spawn(async move {
        let mut conn = match pool.acquire().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to acquire connection for platform webhook delivery");
                return;
            }
        };
        let mut repo = Webhooks::new(&mut conn);

        let pm_webhooks = match repo.get_enabled_platform_webhooks().await {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to fetch platform webhooks");
                return;
            }
        };

        if pm_webhooks.is_empty() {
            return;
        }

        let payload = match serde_json::to_value(&event) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to serialize webhook event");
                return;
            }
        };

        for webhook in pm_webhooks.iter().filter(|w| w.accepts_event(event_type)) {
            let delivery = WebhookDeliveryCreateDBRequest {
                webhook_id: webhook.id,
                event_id: Uuid::new_v4(),
                event_type: event_type.to_string(),
                payload: payload.clone(),
                resource_id,
                next_attempt_at: None,
            };
            if let Err(e) = repo.create_delivery(&delivery).await {
                tracing::warn!(
                    error = %e,
                    webhook_id = %webhook.id,
                    event_type = %event_type,
                    "Failed to create platform webhook delivery"
                );
            }
        }
    });
}
