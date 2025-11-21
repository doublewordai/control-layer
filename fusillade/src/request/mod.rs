//! Request aggregate - domain model and state transitions.
//!
//! This module contains the core domain logic for requests:
//! - Request types and states (typestate pattern)
//! - State transition methods
//! - Value objects (RequestData, etc.)

pub mod transitions;
pub mod types;

// Re-export commonly used types
pub use transitions::CancellationReason;
pub use types::*;
