//! Core domain types and storage traits for Fusillade.
//!
//! This crate contains the stable type universe shared by storage
//! implementations and the scheduling daemon.

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
