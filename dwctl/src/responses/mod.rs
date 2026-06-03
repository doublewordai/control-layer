//! Open Responses API lifecycle management.
//!
//! This module handles the full request lifecycle for the Open Responses API:
//!
//! - **middleware**: Routes requests by `service_tier` and `background`, writes
//!   the fusillade `processing` row inline for background-realtime, no-ops for
//!   non-background realtime (the row appears at completion).
//! - **outlet_handler**: Outlet `RequestHandler` that sends a completion record
//!   into the in-process `RequestsWriter` channel after the proxied response
//!   comes back.
//! - **writer**: Batched in-process consumer that drains the channel and
//!   flushes completion records to fusillade in one transaction per batch.
//!   Replaces the previous underway `create-response` / `complete-response` jobs.
//! - **store**: Functions for reading/writing fusillade rows + `ResponseStore` trait impl
//! - **handler**: `GET /ai/v1/responses/{id}` HTTP handler

pub mod assembly;
pub mod handler;
pub mod image_normalizer_middleware;
pub mod loop_http_client;
pub mod middleware;
pub mod outlet_handler;
pub mod processor;
pub mod store;
pub mod streaming;
pub mod transition;
pub mod writer;
