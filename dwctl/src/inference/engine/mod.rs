//! Multi-step Open Responses orchestration loop and daemon processor.
//!
//! Drives the warm-path (inline) and async (daemon) tool-calling loop that
//! turns a single `/v1/responses` request into a chain of model and tool
//! steps.
//!
//! - **transition**: given the current chain, decide the next step.
//! - **assembly**: assemble the final OpenAI Response JSON from the step chain.
//! - **loop_http_client**: `fusillade::HttpClient` that runs the multi-step
//!   loop instead of making a single HTTP call.
//! - **processor**: `fusillade::RequestProcessor` dispatcher (daemon side).
//! - **writer**: batched in-process persistence of lifecycle steps.
//! - **outlet_handler**: outlet `RequestHandler` that feeds the writer channel.

pub mod assembly;
pub mod loop_http_client;
pub mod outlet_handler;
pub mod processor;
pub mod transition;
pub mod writer;

pub use writer::RequestsWriterHandle;
