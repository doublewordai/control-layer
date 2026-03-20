//! Webhook event types and payload builders.
//!
//! Defines the event types, scopes, and payload structures for webhook notifications.
//! Events are categorised into scopes:
//! - **Own**: Events about the webhook owner's own resources (e.g., batch completion)
//! - **Platform**: Platform-wide events visible to PlatformManagers (e.g., user creation)

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::notifications::BatchNotificationInfo;
use crate::types::UserId;

/// Webhook scope — determines visibility of events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookScope {
    /// Events about the webhook owner's own resources
    Own,
    /// Platform-wide events visible to PlatformManagers
    Platform,
}

/// Webhook event types.
///
/// Each event type belongs to a scope, which determines whether it can be
/// subscribed to by own-scoped or platform-scoped webhooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WebhookEventType {
    // Own-scope events
    /// Batch completed successfully (may have some failed requests)
    #[serde(rename = "batch.completed")]
    BatchCompleted,
    /// Batch failed entirely
    #[serde(rename = "batch.failed")]
    BatchFailed,

    // Platform-scope events
    /// A new user was created
    #[serde(rename = "user.created")]
    UserCreated,
    /// A new batch was created
    #[serde(rename = "batch.created")]
    BatchCreated,
    /// A new API key was created
    #[serde(rename = "api_key.created")]
    ApiKeyCreated,
}

impl WebhookEventType {
    /// Which scope this event type belongs to.
    pub fn scope(&self) -> WebhookScope {
        match self {
            Self::BatchCompleted | Self::BatchFailed => WebhookScope::Own,
            Self::UserCreated | Self::BatchCreated | Self::ApiKeyCreated => WebhookScope::Platform,
        }
    }
}

impl std::fmt::Display for WebhookEventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BatchCompleted => write!(f, "batch.completed"),
            Self::BatchFailed => write!(f, "batch.failed"),
            Self::UserCreated => write!(f, "user.created"),
            Self::BatchCreated => write!(f, "batch.created"),
            Self::ApiKeyCreated => write!(f, "api_key.created"),
        }
    }
}

impl std::str::FromStr for WebhookEventType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "batch.completed" => Ok(Self::BatchCompleted),
            "batch.failed" => Ok(Self::BatchFailed),
            "user.created" => Ok(Self::UserCreated),
            "batch.created" => Ok(Self::BatchCreated),
            "api_key.created" => Ok(Self::ApiKeyCreated),
            _ => Err(format!("Unknown event type: {}", s)),
        }
    }
}

/// Request counts in a batch.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RequestCounts {
    pub total: i64,
    pub completed: i64,
    pub failed: i64,
    pub cancelled: i64,
}

/// Complete webhook event payload.
///
/// The `data` field contains event-specific data as a JSON value,
/// allowing different payload shapes per event type.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WebhookEvent {
    /// Event type (e.g., "batch.completed", "user.created")
    #[serde(rename = "type")]
    pub event_type: String,
    /// When the event occurred
    pub timestamp: DateTime<Utc>,
    /// Event-specific data
    pub data: serde_json::Value,
}

impl WebhookEvent {
    /// Create a new webhook event for a batch terminal state.
    pub fn batch_terminal(event_type: WebhookEventType, info: &BatchNotificationInfo) -> Self {
        let status = match event_type {
            WebhookEventType::BatchCompleted => "completed",
            WebhookEventType::BatchFailed => "failed",
            _ => "unknown",
        };

        let finished_at = info.finished_at.unwrap_or_else(Utc::now);

        Self {
            event_type: event_type.to_string(),
            timestamp: Utc::now(),
            data: serde_json::json!({
                "batch_id": format!("batch_{}", info.batch_uuid),
                "status": status,
                "request_counts": {
                    "total": info.total_requests,
                    "completed": info.completed_requests,
                    "failed": info.failed_requests,
                    "cancelled": info.cancelled_requests,
                },
                "output_file_id": info.output_file_id.map(|id| format!("file_{}", id)),
                "error_file_id": info.error_file_id.map(|id| format!("file_{}", id)),
                "created_at": info.created_at,
                "finished_at": finished_at,
            }),
        }
    }

    /// Create a webhook event for a new user creation.
    pub fn user_created(user_id: UserId, email: &str, auth_source: &str) -> Self {
        Self {
            event_type: WebhookEventType::UserCreated.to_string(),
            timestamp: Utc::now(),
            data: serde_json::json!({
                "user_id": user_id,
                "email": email,
                "auth_source": auth_source,
            }),
        }
    }

    /// Create a webhook event for a new batch creation.
    pub fn batch_created(batch_id: Uuid, user_id: UserId, endpoint: &str) -> Self {
        Self {
            event_type: WebhookEventType::BatchCreated.to_string(),
            timestamp: Utc::now(),
            data: serde_json::json!({
                "batch_id": format!("batch_{}", batch_id),
                "user_id": user_id,
                "endpoint": endpoint,
            }),
        }
    }

    /// Create a webhook event for a new API key creation.
    pub fn api_key_created(key_id: Uuid, user_id: UserId, name: &str) -> Self {
        Self {
            event_type: WebhookEventType::ApiKeyCreated.to_string(),
            timestamp: Utc::now(),
            data: serde_json::json!({
                "api_key_id": key_id,
                "user_id": user_id,
                "name": name,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_type_from_str() {
        assert_eq!(
            "batch.completed".parse::<WebhookEventType>().unwrap(),
            WebhookEventType::BatchCompleted
        );
        assert_eq!("user.created".parse::<WebhookEventType>().unwrap(), WebhookEventType::UserCreated);
        assert_eq!("batch.created".parse::<WebhookEventType>().unwrap(), WebhookEventType::BatchCreated);
        assert_eq!(
            "api_key.created".parse::<WebhookEventType>().unwrap(),
            WebhookEventType::ApiKeyCreated
        );
        assert!("invalid".parse::<WebhookEventType>().is_err());
    }

    #[test]
    fn test_event_type_scope() {
        assert_eq!(WebhookEventType::BatchCompleted.scope(), WebhookScope::Own);
        assert_eq!(WebhookEventType::BatchFailed.scope(), WebhookScope::Own);
        assert_eq!(WebhookEventType::UserCreated.scope(), WebhookScope::Platform);
        assert_eq!(WebhookEventType::BatchCreated.scope(), WebhookScope::Platform);
        assert_eq!(WebhookEventType::ApiKeyCreated.scope(), WebhookScope::Platform);
    }

    #[test]
    fn test_webhook_event_serialization() {
        let info = BatchNotificationInfo {
            batch_id: "batch_00000000-0000-0000-0000-000000000000".to_string(),
            batch_uuid: Uuid::nil(),
            user_id: Uuid::nil(),
            endpoint: "test".to_string(),
            model: "test-model".to_string(),
            outcome: crate::notifications::BatchOutcome::Completed,
            created_at: Utc::now(),
            finished_at: Some(Utc::now()),
            total_requests: 100,
            completed_requests: 98,
            failed_requests: 2,
            cancelled_requests: 0,
            completion_window: "24h".to_string(),
            filename: None,
            description: None,
            output_file_id: Some(Uuid::nil()),
            error_file_id: Some(Uuid::nil()),
        };

        let event = WebhookEvent::batch_terminal(WebhookEventType::BatchCompleted, &info);

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("batch.completed"));
        assert!(json.contains("batch_00000000-0000-0000-0000-000000000000"));
    }

    #[test]
    fn test_user_created_event() {
        let event = WebhookEvent::user_created(Uuid::nil(), "test@example.com", "native");
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("user.created"));
        assert!(json.contains("test@example.com"));
        assert!(json.contains("native"));
    }
}
