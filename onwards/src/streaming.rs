//! Streaming primitives for [`crate::run_response_loop`].
//!
//! When a multi-step response is run with `stream: true`, the loop fires
//! model_call HTTP with `stream: true` upstream, parses the SSE response
//! chunk-by-chunk via [`eventsource_stream`], reassembles the final
//! body via [`openai_reassembler`], and forwards live events through an
//! [`EventSink`] to whatever consumer holds the user's HTTP response
//! (typically dwctl's warm-path POST handler wrapping an axum SSE
//! stream).
//!
//! The [`EventSink`] trait is the only seam between the loop and the
//! delivery transport. Tests use a recording sink; production wraps a
//! `tokio::sync::mpsc::Sender<axum::response::sse::Event>`.
//!
//! ## Event vocabulary (OpenAI Responses-aligned)
//!
//! - [`LoopEvent::Created`] — once at the start of the response.
//! - [`LoopEvent::OutputItemAdded`] — when a model_call's text or
//!   tool-call output item begins, or when a tool_call step starts
//!   producing its `function_call_output` item.
//! - [`LoopEvent::OutputTextDelta`] — per-chunk text token deltas
//!   from a model_call's assistant content.
//! - [`LoopEvent::FunctionCallArgumentsDelta`] — per-chunk argument
//!   deltas from a model_call's tool_calls.
//! - [`LoopEvent::OutputItemDone`] — when an item finishes (model_call
//!   message/function_call complete, or tool_call output complete).
//! - [`LoopEvent::Completed`] — once at end of response.
//! - [`LoopEvent::Failed`] — once when a transition function returns
//!   `Fail` or a cap fires.
//!
//! Each event carries its originating `step_sequence` (or `0` for
//! `Created`, `step_sequence + 1` for terminal events) as the
//! Last-Event-ID cursor.

use async_trait::async_trait;
use serde_json::Value;

/// A streaming event the loop emits to the [`EventSink`].
///
/// String-typed event names match the OpenAI Responses API streaming
/// vocabulary; the carried `data` is the JSON payload that should be
/// emitted to the client SSE response with `event: <kind>`,
/// `id: <sequence>`, `data: <data>`.
#[derive(Debug, Clone)]
pub struct LoopEvent {
    pub sequence: i64,
    pub kind: LoopEventKind,
    pub data: Value,
}

/// Discrete event kinds the loop emits. Mapped 1:1 to OpenAI Responses
/// API event names by [`LoopEventKind::as_str`] — clients see those
/// names verbatim in the `event:` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopEventKind {
    Created,
    OutputItemAdded,
    OutputTextDelta,
    FunctionCallArgumentsDelta,
    OutputItemDone,
    Completed,
    Failed,
}

impl LoopEventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "response.created",
            Self::OutputItemAdded => "response.output_item.added",
            Self::OutputTextDelta => "response.output_text.delta",
            Self::FunctionCallArgumentsDelta => "response.function_call_arguments.delta",
            Self::OutputItemDone => "response.output_item.done",
            Self::Completed => "response.completed",
            Self::Failed => "response.failed",
        }
    }
}

/// Sink the loop forwards [`LoopEvent`]s to during a streaming run.
///
/// Implementors handle delivery: tests record events into a `Vec`,
/// production wraps an `mpsc::Sender` that feeds an axum SSE response.
///
/// `emit` returning `Err` does not abort the loop — the loop logs and
/// continues. Storage and execution proceed regardless of whether the
/// client is still listening.
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn emit(&self, event: LoopEvent) -> Result<(), EventSinkError>;
}

/// Failures emitting an event. Surfaces whatever the underlying
/// transport reports (e.g. SSE channel closed because the client
/// disconnected). The loop logs and continues.
#[derive(Debug, Clone)]
pub struct EventSinkError(pub String);

impl std::fmt::Display for EventSinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "event sink error: {}", self.0)
    }
}

impl std::error::Error for EventSinkError {}

/// Best-effort emit: log and swallow errors so a closed client
/// connection doesn't abort the loop's storage work.
pub(crate) async fn try_emit(sink: &dyn EventSink, event: LoopEvent) {
    if let Err(e) = sink.emit(event).await {
        tracing::debug!(error = %e, "event sink emit failed; continuing");
    }
}

/// Recording sink for tests. Wraps a `Mutex<Vec<LoopEvent>>`.
#[cfg(test)]
#[derive(Default)]
pub struct RecordingSink {
    events: std::sync::Mutex<Vec<LoopEvent>>,
}

#[cfg(test)]
impl RecordingSink {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn events(&self) -> Vec<LoopEvent> {
        self.events.lock().unwrap().clone()
    }
}

#[cfg(test)]
#[async_trait]
impl EventSink for RecordingSink {
    async fn emit(&self, event: LoopEvent) -> Result<(), EventSinkError> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}
