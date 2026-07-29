//! Extensibility traits for Open Responses adapter
//!
//! This module provides traits that users can implement to extend the adapter's capabilities.
//! By default, the adapter operates statelessly, but these traits enable:
//!
//! - **ResponseStore**: Persistent storage for `previous_response_id` support
//! - **ToolExecutor**: Server-side tool execution during agent loops
mod response_store;
mod tool_executor;

pub use response_store::{NoOpResponseStore, ResponseStore, StoreError};
pub use tool_executor::{
    NoOpToolExecutor, RequestContext, ToolError, ToolExecutor, ToolKind, ToolSchema,
};
