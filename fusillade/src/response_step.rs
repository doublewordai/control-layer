//! Response step storage primitives for multi-step Open Responses orchestration.
//!
//! A response step is a discrete unit of work inside a multi-step response: a
//! single upstream model call or tool call. Steps are linked into a linear chain
//! via `prev_step_id` and may be nested under a `parent_step_id` to express
//! sub-agent recursion. The orchestration loop (in `onwards`) decides what the
//! next step is given the chain so far; the storage layer here is purely
//! infrastructure — no Open Responses domain knowledge.
//!
//! See `docs/plans/2026-04-28-multi-step-responses.md`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::Result;
use crate::request::RequestId;

/// Identifier of a single response step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StepId(pub Uuid);

impl std::fmt::Display for StepId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", &self.0.to_string()[..8])
    }
}

impl From<Uuid> for StepId {
    fn from(uuid: Uuid) -> Self {
        StepId(uuid)
    }
}

impl std::ops::Deref for StepId {
    type Target = Uuid;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Discrete kinds of work a response step can represent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// Upstream LLM invocation.
    ModelCall,
    /// Server-side tool invocation (HTTP tool or sub-agent dispatch).
    ToolCall,
}

impl StepKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            StepKind::ModelCall => "model_call",
            StepKind::ToolCall => "tool_call",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "model_call" => Some(StepKind::ModelCall),
            "tool_call" => Some(StepKind::ToolCall),
            _ => None,
        }
    }
}

/// Lifecycle state of a step.
///
/// Mirrors a subset of the `requests.state` lifecycle. `claimed` is omitted
/// because steps do not carry their own lease — serialized access is provided
/// by the parent request's lease, held by the worker driving the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Pending,
    Processing,
    Completed,
    Failed,
    Canceled,
}

impl StepState {
    pub fn as_str(&self) -> &'static str {
        match self {
            StepState::Pending => "pending",
            StepState::Processing => "processing",
            StepState::Completed => "completed",
            StepState::Failed => "failed",
            StepState::Canceled => "canceled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(StepState::Pending),
            "processing" => Some(StepState::Processing),
            "completed" => Some(StepState::Completed),
            "failed" => Some(StepState::Failed),
            "canceled" => Some(StepState::Canceled),
            _ => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            StepState::Completed | StepState::Failed | StepState::Canceled
        )
    }
}

/// A row from the `response_steps` table.
///
/// `request_id` is `Some` for `model_call` steps (each model_call has a
/// dedicated `requests` row created for its upstream HTTP fire) and `None`
/// for `tool_call` steps (tool dispatch lives outside `requests`; the
/// per-tool analytics live in `tool_call_analytics`).
///
/// `parent_step_id` is the chain identifier — it points at the head
/// (root) step of the user-visible response. It is `None` only on the
/// head itself.
///
/// `prev_step_id` is a tree edge: the step that immediately precedes
/// this one. Multiple steps may share a `prev_step_id` (parallel
/// tool_calls; or, when sub-agent dispatch is wired, the sub-agent's
/// head + the outer continuation after a tool_call).
#[derive(Debug, Clone, Serialize)]
pub struct ResponseStep {
    pub id: StepId,
    pub request_id: Option<RequestId>,
    pub prev_step_id: Option<StepId>,
    pub parent_step_id: Option<StepId>,
    pub step_kind: StepKind,
    pub step_sequence: i64,
    pub request_payload: serde_json::Value,
    pub response_payload: Option<serde_json::Value>,
    pub state: StepState,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
    pub canceled_at: Option<DateTime<Utc>>,
    pub retry_attempt: i32,
    pub error: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Input to [`ResponseStepStore::create_step`].
///
/// `step_sequence` is monotonic per chain (head step) across the whole
/// response tree. Callers (the orchestration loop) are responsible for
/// picking the next sequence value.
///
/// Idempotency under crash recovery is the caller's responsibility:
/// walk the existing chain via [`ResponseStepStore::list_chain`] and
/// only emit successors that aren't already present. There is no
/// database-side unique constraint on `(parent_step_id, prev_step_id,
/// step_kind)` — branching is intrinsic to the data model (a model_call
/// returning multiple `tool_calls` emits sibling rows with that exact
/// tuple identical), so any uniqueness on those columns rejects
/// ordinary parallel tool dispatch.
#[derive(Debug, Clone)]
pub struct CreateStepInput {
    /// Optional pre-generated step UUID. When `Some`, becomes the step's
    /// primary key — useful when the caller needs to reference the id
    /// before the row is committed (e.g., emitting an SSE event with the
    /// step id while the row is still being inserted).
    pub id: Option<Uuid>,
    /// FK to `requests.id` for the upstream HTTP fire this step
    /// represents. `None` on `tool_call` steps.
    pub request_id: Option<RequestId>,
    pub prev_step_id: Option<StepId>,
    pub parent_step_id: Option<StepId>,
    pub step_kind: StepKind,
    pub step_sequence: i64,
    pub request_payload: serde_json::Value,
}

/// Storage trait for response step persistence.
///
/// Mirrors the shape of [`crate::Storage`] for requests but scoped to the
/// `response_steps` table. Implementations must be safe to call from multiple
/// concurrent tasks within a single worker (tool fan-out within a single
/// response holds the parent request lease, so no cross-worker coordination
/// is required).
#[async_trait]
pub trait ResponseStepStore: Send + Sync {
    /// Insert a new step in `pending` state.
    ///
    /// No database-side idempotency constraint. Callers that need
    /// idempotent recovery should walk the chain first via
    /// [`ResponseStepStore::list_chain`] and skip emission of
    /// successors that already exist.
    async fn create_step(&self, input: CreateStepInput) -> Result<StepId>;

    /// Fetch a single step by id. Returns `None` if not present.
    async fn get_step(&self, id: StepId) -> Result<Option<ResponseStep>>;

    /// Fetch the step (if any) whose `request_id` matches the given
    /// fusillade request id. Used by analytics + request-logging plumbing to
    /// resolve "which step does this upstream HTTP fire belong to".
    /// Returns `None` for `tool_call` steps (which don't carry a
    /// `request_id`) and for any non-multi-step fusillade row.
    async fn get_step_by_request(&self, request_id: RequestId) -> Result<Option<ResponseStep>>;

    /// List every step in a response chain identified by its head step.
    ///
    /// Includes the head itself + every descendant (parallel tool_calls
    /// and any future sub-agent recursion via `prev_step_id` branching).
    /// Ordered by `step_sequence`.
    async fn list_chain(&self, head_step_id: StepId) -> Result<Vec<ResponseStep>>;

    /// Mark a `pending` step as `processing`, recording `started_at`.
    ///
    /// Idempotent: if the step is already in a non-pending state the call
    /// returns `Ok(())` without modifying the row, so crash recovery can
    /// resume safely.
    async fn mark_step_processing(&self, id: StepId) -> Result<()>;

    /// Mark a step as `completed` with the given `response_payload`.
    async fn complete_step(&self, id: StepId, response: serde_json::Value) -> Result<()>;

    /// Mark a step as `failed` with the given structured error payload.
    async fn fail_step(&self, id: StepId, error: serde_json::Value) -> Result<()>;

    /// Mark a step as `canceled`.
    async fn cancel_step(&self, id: StepId) -> Result<()>;

    /// Increment a step's `retry_attempt` and reset it to `pending` for re-
    /// firing under crash recovery. Used when a worker picks up a step
    /// that was left in `processing` by a dead worker.
    async fn requeue_step_for_retry(&self, id: StepId) -> Result<()>;
}
