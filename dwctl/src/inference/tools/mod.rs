//! Server-side tool resolution and execution for the inference path.
//!
//! - **injection**: resolves the effective tool set from the database (the
//!   intersection of deployment- and group-attached tool sources) at request
//!   time, plus the standalone Tower layer that injects it into request
//!   extensions for the single-step path.
//! - **executor**: [`ResolvedToolSet`] / [`ResolvedTools`] - the resolved tool
//!   set and its request-extension wrapper, rendered into the OpenAI `tools`
//!   array by the injection layer.

pub mod executor;
pub mod injection;

pub use executor::{ResolvedToolSet, ResolvedTools, ToolDefinition};
pub use injection::{ToolInjectionState, resolve_tools_for_request, tool_injection_middleware};
