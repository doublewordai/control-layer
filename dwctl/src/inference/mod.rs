//! Inference request dispatch and lifecycle (the `/ai/v1/*` proxy path).
//!
//! Handles the full request lifecycle for the OpenAI-compatible inference
//! surfaces (`/v1/responses`, `/v1/chat/completions`, `/v1/embeddings`):
//!
//! - **middleware**: routes requests by `service_tier` and `background`, writes
//!   the fusillade `processing` row inline for background-realtime, no-ops for
//!   non-background realtime (the row appears at completion), and enqueues
//!   `flex` requests for the daemon.
//! - **store**: read/write fusillade rows, the `ResponseStore` trait impl, and
//!   the per-surface response renderers (`detail_to_*_object`).
//! - **streaming**: inline multi-step (warm-path) streaming/blocking responses.
//! - **handler**: `GET /ai/v1/responses/{id}` HTTP handler.
//! - **image_normalizer_middleware**: body-rewriting image normalisation shared
//!   by the chat-completions and responses surfaces.
//! - **tools**: server-side tool resolution (injection) and execution (executor).
//! - **engine**: the multi-step Open Responses orchestration loop and the
//!   daemon-side request processor.

pub mod handler;
pub mod image_normalizer_middleware;
pub mod middleware;
pub mod store;
pub mod streaming;

pub mod engine;
pub mod tools;
