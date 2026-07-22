//! Request aggregate - domain model and state transitions.
//!
//! This module contains the core domain logic for requests:
//! - Request types and states (typestate pattern)
//! - State transition methods
//! - Value objects (RequestData, etc.)

pub mod query;
pub mod transitions;
pub mod types;

// Re-export commonly used types
#[allow(deprecated)]
pub use query::RequestSummaryWithCount;
pub use query::{
    CreateFlexInput, CreateRealtimeInput, CreateResponseInput, ListRequestsFilter,
    PersistCompletedRealtimeInput, RequestDetail, RequestListResult, RequestSummary,
    ServiceTierFilter,
};
pub use transitions::CancellationReason;
pub use types::*;
