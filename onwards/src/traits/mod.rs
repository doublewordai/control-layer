//! Extensibility traits for Open Responses adapter
//!
//! This module provides traits that users can implement to extend the adapter's capabilities.
//! By default, the adapter operates statelessly, but these traits enable:
//!
//! - **ResponseStore**: Persistent storage for `previous_response_id` support
//! - **ToolExecutor**: Server-side tool execution during agent loops
//! - **MultiStepStore**: Multi-step Open Responses orchestration storage.
//!   Drives [`crate::run_response_loop`] alongside the existing
//!   `ToolExecutor` trait — there is deliberately no parallel multi-step
//!   execution trait. Tool dispatch goes through `ToolExecutor::execute`,
//!   sub-agent recursion is signalled by `ToolKind::Agent` on the
//!   tool's schema, and model calls are fired by the loop directly via
//!   the configured HTTP client.

#[cfg(feature = "multi-step")]
mod multi_step_store;
mod response_store;
mod tool_executor;

#[cfg(feature = "multi-step")]
pub use multi_step_store::{
    ChainStep, ExecutorError, MultiStepStore, NextAction, RecordedStep, StepDescriptor, StepKind,
    StepState,
};
pub use response_store::{NoOpResponseStore, ResponseStore, StoreError};
pub use tool_executor::{
    NoOpToolExecutor, RequestContext, ToolError, ToolExecutor, ToolKind, ToolSchema,
};
