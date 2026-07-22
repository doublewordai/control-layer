//! Storage trait for multi-step Open Responses orchestration.
//!
//! [`MultiStepStore`] is the storage half of the contract that
//! [`crate::run_response_loop`] drives. It owns:
//!
//! - the **transition function** ([`MultiStepStore::next_action_for`]) that
//!   decides what the next step is given the chain so far;
//! - **CRUD primitives** for persisting and updating step rows
//!   ([`record_step`](MultiStepStore::record_step) /
//!   [`mark_step_processing`](MultiStepStore::mark_step_processing) /
//!   [`complete_step`](MultiStepStore::complete_step) /
//!   [`fail_step`](MultiStepStore::fail_step));
//! - the **chain walk** ([`list_chain`](MultiStepStore::list_chain)) so the
//!   transition function can read state without coupling onwards to any
//!   particular storage backend's chain primitive;
//! - **assembly** ([`assemble_response`](MultiStepStore::assemble_response))
//!   from the chain to the final OpenAI Response JSON.
//!
//! Execution lives in the existing [`super::ToolExecutor`] trait (for
//! tools) and in the loop itself (for model calls). There is no
//! dedicated multi-step execution trait — onwards reuses what's already
//! there.
//!
//! Implementing this trait is opt-in: consumers that don't need multi-step
//! support (legacy callers using only [`super::ResponseStore`]) don't need
//! to touch [`MultiStepStore`] at all.

use async_trait::async_trait;
use fusillade::RequestId;
use std::fmt;

use super::response_store::StoreError;

/// Discrete kinds of work a response step can represent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    /// Upstream LLM invocation.
    ModelCall,
    /// Server-side tool invocation. The dispatch decision (HTTP tool,
    /// sub-agent, MCP, etc.) lives entirely in the [`super::StepExecutor`]
    /// implementation — onwards remains agnostic.
    ToolCall,
}

/// Lifecycle state of a step. Mirrors the canonical fusillade values so
/// implementations backed by `response_steps` can pass them through
/// directly without translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Processing,
    Completed,
    Failed,
    Canceled,
}

impl StepState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Canceled)
    }
}

/// Description of a single step the transition function wants the loop to
/// execute next. Returned (one or more, for fan-out) inside
/// [`NextAction::AppendSteps`].
///
/// Note the absence of an `is_subagent` field: tool dispatch is decided
/// by the tool's [`super::ToolKind`] on its schema, surfaced by the
/// `ToolExecutor` implementation. The transition function decides *what*
/// step to run; the dispatch (HTTP fire vs sub-agent recursion) follows
/// from the tool's declared kind.
#[derive(Debug, Clone)]
pub struct StepDescriptor {
    pub kind: StepKind,
    /// Step-specific input payload (instructions, tool name + arguments,
    /// etc.). Persisted verbatim into the storage layer's `request_payload`
    /// column.
    pub request_payload: serde_json::Value,
}

/// The transition function's verdict for the next iteration.
#[derive(Debug, Clone)]
pub enum NextAction {
    /// Append one or more steps. A single descriptor = serial; multiple =
    /// parallel fan-out (the loop fires them concurrently via
    /// `futures_util::future::join_all`).
    AppendSteps(Vec<StepDescriptor>),
    /// Stop — the response is complete with the given final payload.
    /// `run_response_loop` returns this payload to the caller.
    Complete(serde_json::Value),
    /// Stop — the response failed with the given structured error.
    /// `run_response_loop` returns `LoopError::Failed`.
    Fail(serde_json::Value),
}

/// A step row recorded by [`MultiStepStore::record_step`]. Carries both
/// the assigned id and its monotonic sequence value so the loop can chain
/// siblings without a separate sequence-allocation round-trip.
///
/// For `model_call` steps, `sub_request_id` carries the
/// `fusillade.requests` row id that the implementor created for this
/// step's HTTP fire — the loop stamps it as `X-Fusillade-Request-Id` on
/// the outgoing call so the `http_analytics` row produced by outlet
/// middleware lines up with the right row in `requests`. For `tool_call`
/// steps it's `None` (per the `response_steps` CHECK constraint —
/// tool dispatch has its own analytics path).
#[derive(Debug, Clone)]
pub struct RecordedStep {
    pub id: String,
    pub sequence: i64,
    pub sub_request_id: Option<RequestId>,
}

/// A read-only projection of a step row, returned by
/// [`MultiStepStore::list_chain`]. Carries everything the implementor's
/// transition function needs to reason about the chain so far.
#[derive(Debug, Clone)]
pub struct ChainStep {
    pub id: String,
    pub kind: StepKind,
    pub state: StepState,
    pub sequence: i64,
    /// `prev_step_id` within the same scope. `None` = first in the scope.
    pub prev_step_id: Option<String>,
    /// `parent_step_id` — `None` for top-level steps, `Some` for steps in
    /// a sub-agent loop spawned by that parent.
    pub parent_step_id: Option<String>,
    /// Output payload for `Completed` steps; `None` otherwise.
    pub response_payload: Option<serde_json::Value>,
    /// Structured error payload for `Failed` steps; `None` otherwise.
    pub error: Option<serde_json::Value>,
}

/// Storage trait for multi-step Open Responses orchestration.
///
/// All methods take string-typed IDs at the boundary so the trait stays
/// storage-agnostic: any implementor can use whatever native id type
/// (UUID, ULID, integer surrogate, etc.) under the hood.
#[async_trait]
pub trait MultiStepStore: Send + Sync {
    /// Decide the next action for a request given the chain so far.
    ///
    /// Called once per loop iteration. Implementations typically:
    /// 1. Call [`list_chain`](Self::list_chain) for the same
    ///    `(request_id, scope_parent)` to read what's been done.
    /// 2. Inspect the most recent completed step's `response_payload`.
    /// 3. Decide whether to append more steps, complete the response, or
    ///    fail it.
    ///
    /// `scope_parent`:
    /// - `None`  — the top-level chain (user-visible response).
    /// - `Some(step_id)` — the sub-loop spawned by that step (sub-agent).
    async fn next_action_for(
        &self,
        request_id: &str,
        scope_parent: Option<&str>,
    ) -> Result<NextAction, StoreError>;

    /// Persist a new step in `pending` state, allocating its sequence
    /// atomically. Returns the assigned id and sequence.
    ///
    /// Folding sequence allocation into the insert avoids a separate
    /// round-trip per step (a fan-out of N siblings used to require N+1
    /// queries before any work was done; now it's just N).
    async fn record_step(
        &self,
        request_id: &str,
        scope_parent: Option<&str>,
        prev_step: Option<&str>,
        descriptor: &StepDescriptor,
    ) -> Result<RecordedStep, StoreError>;

    /// Transition a `pending` step to `processing`.
    async fn mark_step_processing(&self, step_id: &str) -> Result<(), StoreError>;

    /// Persist a step's terminal output (transition to `completed`).
    async fn complete_step(
        &self,
        step_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(), StoreError>;

    /// Persist a step's terminal error (transition to `failed`).
    async fn fail_step(&self, step_id: &str, error: &serde_json::Value) -> Result<(), StoreError>;

    /// Walk the chain for a single scope, ordered by sequence.
    ///
    /// `scope_parent` works the same as in [`next_action_for`]: `None` =
    /// top-level chain, `Some(step_id)` = sub-loop scope. The transition
    /// function uses this to inspect siblings and decide what to do next
    /// without needing direct access to the underlying storage.
    async fn list_chain(
        &self,
        request_id: &str,
        scope_parent: Option<&str>,
    ) -> Result<Vec<ChainStep>, StoreError>;

    /// Materialize the final `Response` JSON (per the OpenAI Responses
    /// API contract) from the chain of completed steps. The implementor
    /// owns the format because the assembly is purely Open Responses
    /// domain logic.
    async fn assemble_response(&self, request_id: &str) -> Result<serde_json::Value, StoreError>;
}

/// Errors specific to step execution. Distinct from
/// [`StoreError`](crate::traits::StoreError) so storage failures and
/// execution failures are separable in the loop's error type.
///
/// Used by [`crate::run_response_loop`] to wrap failures from
/// [`super::ToolExecutor::execute`] and from the loop's internal model
/// HTTP fire.
#[derive(Debug, Clone)]
pub enum ExecutorError {
    /// Tool/model is not registered or is unknown to the executor.
    NotFound(String),
    /// Execution failed (upstream HTTP error, timeout, etc.).
    ExecutionError(String),
}

impl fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(name) => write!(f, "executor target not found: {}", name),
            Self::ExecutionError(msg) => write!(f, "executor error: {}", msg),
        }
    }
}

impl std::error::Error for ExecutorError {}
