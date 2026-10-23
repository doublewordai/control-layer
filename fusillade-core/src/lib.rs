//! Core domain types and storage traits for Fusillade.
//!
//! This crate contains the stable type universe shared by storage
//! implementations and the scheduling daemon.

/// Implements `Debug` for a struct that holds an `api_key` secret, printing
/// every other field normally and the key as `<redacted>`.
///
/// Debug output reaches logs through `#[tracing::instrument]` arguments and
/// `?` fields, so a derived impl leaks the key. List every field except
/// `api_key`: the impl destructures the struct exhaustively, so adding a field
/// fails to compile until it is listed here.
macro_rules! debug_with_redacted_api_key {
    ($ty:ident { $($field:ident),* $(,)? }) => {
        impl std::fmt::Debug for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let Self { api_key: _, $($field),* } = self;
                f.debug_struct(stringify!($ty))
                    $(.field(stringify!($field), $field))*
                    .field("api_key", &"<redacted>")
                    .finish()
            }
        }
    };
}

pub mod batch;
pub mod daemon_record;
pub mod error;
pub mod manager;
pub mod request;

pub use batch::*;
pub use daemon_record::{
    AnyDaemonRecord, DaemonData, DaemonRecord, DaemonState, DaemonStats, DaemonStatus, Dead,
    Initializing, Running,
};
pub use error::{FusilladeError, Result};
pub use manager::{
    DaemonStorage, ModelFilter, ModelFilterState, RetainedResponseArchiveOutcome,
    RetainedResponseMaintenanceError, RetainedResponsePartitionRunway,
    RetainedResponseRetirementOutcome, RetentionPolicy, Storage, TrailingDemandCount,
};
pub use request::*;

#[cfg(test)]
mod redacted_api_key_tests {
    use std::collections::HashMap;

    use chrono::Utc;
    use uuid::Uuid;

    use super::*;

    const SECRET: &str = "sk-test-do-not-log";

    fn assert_redacted(value: &impl std::fmt::Debug) {
        let output = format!("{value:?}");
        assert!(!output.contains(SECRET), "secret leaked: {output}");
        assert!(output.contains("api_key: \"<redacted>\""), "{output}");
    }

    #[test]
    fn batch_types_redact_the_api_key() {
        let now = Utc::now();
        assert_redacted(&RequestTemplate {
            id: TemplateId::from(Uuid::new_v4()),
            file_id: FileId::from(Uuid::new_v4()),
            custom_id: None,
            endpoint: "http://localhost".to_string(),
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            body: "{}".to_string(),
            model: "model-a".to_string(),
            api_key: SECRET.to_string(),
            created_at: now,
            updated_at: now,
        });
        assert_redacted(&RequestTemplateInput {
            custom_id: None,
            endpoint: "http://localhost".to_string(),
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            body: "{}".to_string(),
            model: "model-a".to_string(),
            api_key: SECRET.to_string(),
        });
        assert_redacted(&BatchInput {
            file_id: FileId::from(Uuid::new_v4()),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
            created_by: None,
            api_key_id: None,
            api_key: Some(SECRET.to_string()),
            total_requests: None,
        });
        assert_redacted(&BackgroundBatchInput {
            file_id: FileId::from(Uuid::new_v4()),
            endpoint: "/v1/chat/completions".to_string(),
            metadata: None,
            created_by: None,
            api_key_id: None,
            api_key: Some(SECRET.to_string()),
            total_requests: None,
        });
    }

    #[test]
    fn request_types_redact_the_api_key() {
        let now = Utc::now();
        assert_redacted(&RequestData {
            id: RequestId::from(Uuid::new_v4()),
            batch_id: None,
            template_id: TemplateId::from(Uuid::new_v4()),
            custom_id: None,
            endpoint: "http://localhost".to_string(),
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            body: "{}".to_string(),
            model: "model-a".to_string(),
            api_key: SECRET.to_string(),
            created_by: "user".to_string(),
            batch_metadata: HashMap::new(),
        });
        assert_redacted(&CreateRealtimeInput {
            request_id: Uuid::new_v4(),
            body: "{}".to_string(),
            model: "model-a".to_string(),
            endpoint: "http://localhost".to_string(),
            method: "POST".to_string(),
            path: "/v1/responses".to_string(),
            api_key: SECRET.to_string(),
            created_by: "user".to_string(),
        });
        assert_redacted(&PersistCompletedRealtimeInput {
            request_id: Uuid::new_v4(),
            response_body: "{}".to_string(),
            status_code: 200,
            request_body: "{}".to_string(),
            model: "model-a".to_string(),
            endpoint: "http://localhost".to_string(),
            method: "POST".to_string(),
            path: "/v1/responses".to_string(),
            api_key: SECRET.to_string(),
            created_by: "user".to_string(),
            started_at: now,
            completed_at: now,
        });
        assert_redacted(&CreateFlexInput {
            request_id: Uuid::new_v4(),
            body: "{}".to_string(),
            model: "model-a".to_string(),
            endpoint: "http://localhost".to_string(),
            method: "POST".to_string(),
            path: "/v1/responses".to_string(),
            api_key: SECRET.to_string(),
            created_by: "user".to_string(),
            metadata: None,
        });
        assert_redacted(&CreateBackgroundInput {
            request_id: Uuid::new_v4(),
            body: "{}".to_string(),
            model: "model-a".to_string(),
            endpoint: "http://localhost".to_string(),
            method: "POST".to_string(),
            path: "/v1/responses".to_string(),
            api_key: SECRET.to_string(),
            created_by: "user".to_string(),
            metadata: None,
        });
    }
}
