//! Webhook event types and payload builders.
//!
//! Defines the event types and payload structures for webhook notifications.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

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

impl WebhookEventType {
    /// Get the string representation of the event type.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BatchCompleted => "batch.completed",
            Self::BatchFailed => "batch.failed",
            Self::BatchCancelled => "batch.cancelled",
        }
    }

    /// Parse from a status string (completed, failed, cancelled).
    pub fn from_status(status: &str) -> Option<Self> {
        match status {
            "completed" => Some(Self::BatchCompleted),
            "failed" => Some(Self::BatchFailed),
            "cancelled" => Some(Self::BatchCancelled),
            _ => None,
        }
    }

    /// Get all event types.
    pub fn all() -> &'static [Self] {
        &[Self::BatchCompleted, Self::BatchFailed, Self::BatchCancelled]
    }
}

impl std::fmt::Display for WebhookEventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
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
pub struct BatchEventData {
    pub batch_id: String,
    pub status: String,
    pub request_counts: RequestCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_file_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_file_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
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
    pub data: BatchEventData,
}

impl WebhookEvent {
    /// Create a new webhook event for a batch terminal state.
    pub fn batch_terminal(
        event_type: WebhookEventType,
        batch_id: uuid::Uuid,
        request_counts: RequestCounts,
        output_file_id: Option<uuid::Uuid>,
        error_file_id: Option<uuid::Uuid>,
        created_at: DateTime<Utc>,
        finished_at: DateTime<Utc>,
    ) -> Self {
        let status = match event_type {
            WebhookEventType::BatchCompleted => "completed",
            WebhookEventType::BatchFailed => "failed",
            WebhookEventType::BatchCancelled => "cancelled",
        };

        Self {
            event_type: event_type.as_str().to_string(),
            timestamp: Utc::now(),
            data: BatchEventData {
                batch_id: format!("batch_{}", batch_id),
                status: status.to_string(),
                request_counts,
                output_file_id: output_file_id.map(|id| format!("file_{}", id)),
                error_file_id: error_file_id.map(|id| format!("file_{}", id)),
                created_at,
                finished_at,
            },
        }
    }

    /// Serialize the event to JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_type_as_str() {
        assert_eq!(WebhookEventType::BatchCompleted.as_str(), "batch.completed");
        assert_eq!(WebhookEventType::BatchFailed.as_str(), "batch.failed");
        assert_eq!(WebhookEventType::BatchCancelled.as_str(), "batch.cancelled");
    }

    #[test]
    fn test_event_type_from_status() {
        assert_eq!(WebhookEventType::from_status("completed"), Some(WebhookEventType::BatchCompleted));
        assert_eq!(WebhookEventType::from_status("failed"), Some(WebhookEventType::BatchFailed));
        assert_eq!(WebhookEventType::from_status("cancelled"), Some(WebhookEventType::BatchCancelled));
        assert_eq!(WebhookEventType::from_status("unknown"), None);
    }

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
        let event = WebhookEvent::batch_terminal(
            WebhookEventType::BatchCompleted,
            uuid::Uuid::nil(),
            RequestCounts {
                total: 100,
                completed: 98,
                failed: 2,
                cancelled: 0,
            },
            Some(uuid::Uuid::nil()),
            Some(uuid::Uuid::nil()),
            Utc::now(),
            Utc::now(),
        );

        let json = event.to_json().unwrap();
        assert!(json.contains("batch.completed"));
        assert!(json.contains("batch_00000000-0000-0000-0000-000000000000"));
    }
}
