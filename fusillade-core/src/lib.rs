//! Core domain types and storage traits for Fusillade.
//!
//! This crate contains the stable type universe shared by storage
//! implementations and the scheduling daemon.

pub mod batch;
pub mod daemon_record;
pub mod error;
pub mod manager;
pub mod request;
pub mod response_step;

pub use batch::*;
pub use daemon_record::{
    AnyDaemonRecord, DaemonData, DaemonRecord, DaemonState, DaemonStats, DaemonStatus, Dead,
    Initializing, Running,
};
pub use error::{FusilladeError, Result};
pub use manager::{DaemonStorage, ModelFilter, ModelFilterState, Storage};
pub use request::*;
pub use response_step::{
    CreateStepInput, ResponseStep, ResponseStepStore, StepId, StepKind, StepState,
};
