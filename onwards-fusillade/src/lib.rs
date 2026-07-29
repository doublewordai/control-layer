//! Private multi-step response integration between Onwards and Fusillade.

pub mod response_loop;
pub mod streaming;
pub mod traits;

pub use response_loop::{LoopConfig, LoopError, UpstreamTarget, run_response_loop};
pub use streaming::{EventSink, EventSinkError, LoopEvent, LoopEventKind};
pub use traits::{
    ChainStep, ExecutorError, MultiStepStore, NextAction, RecordedStep, StepDescriptor, StepKind,
    StepState,
};
