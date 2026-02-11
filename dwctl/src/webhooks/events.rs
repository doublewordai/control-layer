//! Webhook event types and payload builders.
//!
//! Defines the event types and payload structures for webhook notifications.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::notifications::BatchNotificationInfo;

/// Webhook event types for batch processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WebhookEventType {
    /// Batch completed successfully (may have some failed requests)
    #[serde(rename = "batch.completed")]
    BatchCompleted,
    /// Batch failed entirely
    #[serde(rename = "batch.failed")]
    BatchFailed,
    /// Batch was cancelled
    #[serde(rename = "batch.cancelled")]
    BatchCancelled,
}

impl std::fmt::Display for WebhookEventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BatchCompleted => write!(f, "batch.completed"),
            Self::BatchFailed => write!(f, "batch.failed"),
            Self::BatchCancelled => write!(f, "batch.cancelled"),
        }
    }
}

impl std::str::FromStr for WebhookEventType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "batch.completed" => Ok(Self::BatchCompleted),
            "batch.failed" => Ok(Self::BatchFailed),
            "batch.cancelled" => Ok(Self::BatchCancelled),
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

/// Batch event data included in webhook payload.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
struct BatchEventData {
    batch_id: String,
    status: String,
    request_counts: RequestCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_file_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_file_id: Option<String>,
    created_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
}

/// Complete webhook event payload.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WebhookEvent {
    /// Event type (e.g., "batch.completed")
    #[serde(rename = "type")]
    pub event_type: String,
    /// When the event occurred
    pub timestamp: DateTime<Utc>,
    /// Event-specific data
    data: BatchEventData,
}

impl WebhookEvent {
    /// Create a new webhook event for a batch terminal state.
    pub fn batch_terminal(event_type: WebhookEventType, info: &BatchNotificationInfo) -> Self {
        let status = match event_type {
            WebhookEventType::BatchCompleted => "completed",
            WebhookEventType::BatchFailed => "failed",
            WebhookEventType::BatchCancelled => "cancelled",
        };

        let finished_at = info.finished_at.unwrap_or_else(Utc::now);

        Self {
            event_type: event_type.to_string(),
            timestamp: Utc::now(),
            data: BatchEventData {
                batch_id: format!("batch_{}", info.batch_uuid),
                status: status.to_string(),
                request_counts: RequestCounts {
                    total: info.total_requests,
                    completed: info.completed_requests,
                    failed: info.failed_requests,
                    cancelled: info.cancelled_requests,
                },
                output_file_id: info.output_file_id.map(|id| format!("file_{}", id)),
                error_file_id: info.error_file_id.map(|id| format!("file_{}", id)),
                created_at: info.created_at,
                finished_at,
            },
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
        assert!("invalid".parse::<WebhookEventType>().is_err());
    }

    #[test]
    fn test_webhook_event_serialization() {
        let info = BatchNotificationInfo {
            batch_id: "batch_00000000-0000-0000-0000-000000000000".to_string(),
            batch_uuid: uuid::Uuid::nil(),
            user_id: uuid::Uuid::nil(),
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
            output_file_id: Some(uuid::Uuid::nil()),
            error_file_id: Some(uuid::Uuid::nil()),
        };

        let event = WebhookEvent::batch_terminal(WebhookEventType::BatchCompleted, &info);

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("batch.completed"));
        assert!(json.contains("batch_00000000-0000-0000-0000-000000000000"));
    }
}
