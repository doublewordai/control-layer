//! Database models for webhook configuration and delivery tracking.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use crate::types::UserId;
use crate::webhooks::WebhookEventType;

/// Webhook ID type alias for type safety.
pub type WebhookId = Uuid;

/// Delivery ID type alias for type safety.
pub type DeliveryId = Uuid;

/// Database model for a user webhook configuration.
#[derive(Debug, Clone, FromRow)]
pub struct Webhook {
    pub id: WebhookId,
    pub user_id: UserId,
    pub url: String,
    pub secret: String,
    pub enabled: bool,
    pub event_types: Option<serde_json::Value>,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub consecutive_failures: i32,
    pub disabled_at: Option<DateTime<Utc>>,
}

impl Webhook {
    /// Check if this webhook should receive the given event type.
    pub fn accepts_event(&self, event_type: WebhookEventType) -> bool {
        if !self.enabled {
            return false;
        }

        // If event_types is null, accept all events
        let Some(ref types) = self.event_types else {
            return true;
        };

        // Check if the event type is in the list
        if let Some(arr) = types.as_array() {
            let event_str = event_type.as_str();
            arr.iter().any(|v| v.as_str() == Some(event_str))
        } else {
            // Invalid format, accept all
            true
        }
    }
}

/// Delivery status for webhook deliveries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryStatus {
    /// Pending first delivery attempt
    Pending,
    /// Successfully delivered
    Delivered,
    /// Failed but will retry
    Failed,
    /// All retries exhausted
    Exhausted,
}

impl DeliveryStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
            Self::Exhausted => "exhausted",
        }
    }
}

impl std::str::FromStr for DeliveryStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "delivered" => Ok(Self::Delivered),
            "failed" => Ok(Self::Failed),
            "exhausted" => Ok(Self::Exhausted),
            _ => Err(format!("Unknown delivery status: {}", s)),
        }
    }
}

/// Database model for a webhook delivery attempt.
#[derive(Debug, Clone, FromRow)]
pub struct WebhookDelivery {
    pub id: DeliveryId,
    pub webhook_id: WebhookId,
    pub event_id: Uuid,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub status: String,
    pub attempt_count: i32,
    pub next_attempt_at: DateTime<Utc>,
    pub batch_id: Uuid,
    pub last_status_code: Option<i32>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl WebhookDelivery {
    /// Get the parsed delivery status.
    pub fn delivery_status(&self) -> DeliveryStatus {
        self.status.parse().unwrap_or(DeliveryStatus::Pending)
    }
}

/// Request to create a new webhook.
#[derive(Debug, Clone)]
pub struct WebhookCreateDBRequest {
    pub user_id: UserId,
    pub url: String,
    pub secret: String,
    pub event_types: Option<Vec<String>>,
    pub description: Option<String>,
}

/// Request to update a webhook.
#[derive(Debug, Clone, Default)]
pub struct WebhookUpdateDBRequest {
    pub url: Option<String>,
    pub enabled: Option<bool>,
    pub event_types: Option<Option<Vec<String>>>,
    pub description: Option<Option<String>>,
}

/// Request to create a webhook delivery.
#[derive(Debug, Clone)]
pub struct WebhookDeliveryCreateDBRequest {
    pub webhook_id: WebhookId,
    pub event_id: Uuid,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub batch_id: Uuid,
}
