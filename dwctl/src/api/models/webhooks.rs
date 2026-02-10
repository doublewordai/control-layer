//! API request and response models for webhook endpoints.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::db::models::webhooks::{Webhook, WebhookId};
use crate::types::UserId;

/// Request to create a new webhook.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct WebhookCreate {
    /// HTTPS URL to receive webhook events
    pub url: String,
    /// Optional list of event types to receive (null for all events)
    #[serde(default)]
    pub event_types: Option<Vec<String>>,
    /// Optional description to identify this webhook
    #[serde(default)]
    pub description: Option<String>,
}

/// Request to update a webhook.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct WebhookUpdate {
    /// New URL (optional)
    #[serde(default)]
    pub url: Option<String>,
    /// Enable/disable the webhook (optional)
    #[serde(default)]
    pub enabled: Option<bool>,
    /// New event types filter (optional, null to receive all events)
    #[serde(default)]
    pub event_types: Option<Option<Vec<String>>>,
    /// New description (optional)
    #[serde(default)]
    pub description: Option<Option<String>>,
}

/// Response for a webhook (secret hidden except on create/rotate).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WebhookResponse {
    #[schema(value_type = String, format = "uuid")]
    pub id: WebhookId,
    #[schema(value_type = String, format = "uuid")]
    pub user_id: UserId,
    pub url: String,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_types: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub consecutive_failures: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<DateTime<Utc>>,
}

impl From<Webhook> for WebhookResponse {
    fn from(webhook: Webhook) -> Self {
        let event_types = webhook.event_types.and_then(|v| serde_json::from_value::<Vec<String>>(v).ok());

        Self {
            id: webhook.id,
            user_id: webhook.user_id,
            url: webhook.url,
            enabled: webhook.enabled,
            event_types,
            description: webhook.description,
            created_at: webhook.created_at,
            updated_at: webhook.updated_at,
            consecutive_failures: webhook.consecutive_failures,
            disabled_at: webhook.disabled_at,
        }
    }
}

/// Response for webhook create/rotate that includes the secret (shown only once).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WebhookWithSecretResponse {
    #[schema(value_type = String, format = "uuid")]
    pub id: WebhookId,
    #[schema(value_type = String, format = "uuid")]
    pub user_id: UserId,
    pub url: String,
    /// The webhook secret (only shown on create and rotate operations)
    pub secret: String,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_types: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<Webhook> for WebhookWithSecretResponse {
    fn from(webhook: Webhook) -> Self {
        let event_types = webhook.event_types.and_then(|v| serde_json::from_value::<Vec<String>>(v).ok());

        Self {
            id: webhook.id,
            user_id: webhook.user_id,
            url: webhook.url,
            secret: webhook.secret,
            enabled: webhook.enabled,
            event_types,
            description: webhook.description,
            created_at: webhook.created_at,
            updated_at: webhook.updated_at,
        }
    }
}

/// Response for a test webhook delivery.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct WebhookTestResponse {
    /// Whether the test delivery was successful
    pub success: bool,
    /// HTTP status code received (if any)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    /// Error message (if any)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Time taken for the request in milliseconds
    pub duration_ms: u64,
}

/// Path parameters for webhook endpoints.
#[derive(Debug, Clone, Deserialize)]
pub struct WebhookPathParams {
    pub user_id: Uuid,
    pub webhook_id: Uuid,
}

/// Path parameters for user-only webhook endpoints.
#[derive(Debug, Clone, Deserialize)]
pub struct UserWebhookPathParams {
    pub user_id: Uuid,
}
