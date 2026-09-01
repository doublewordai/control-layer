//! Persisted daemon lifecycle record types and transitions.

pub mod transitions;
pub mod types;

pub use types::{
    AnyDaemonRecord, DaemonData, DaemonRecord, DaemonState, DaemonStats, DaemonStatus, Dead,
    Initializing, Running,
};
