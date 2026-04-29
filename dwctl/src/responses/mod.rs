//! Open Responses API lifecycle management.
//!
//! This module handles the full request lifecycle for the Open Responses API:
//!
//! - **middleware**: Routes requests by `service_tier` and `background`, enqueues
//!   `CreateResponseJob` via underway
//! - **jobs**: Underway jobs for creating and completing fusillade rows (with auth)
//! - **outlet_handler**: Outlet `RequestHandler` that enqueues `CompleteResponseJob`
//! - **store**: Functions for reading/writing fusillade rows + `ResponseStore` trait impl
//! - **handler**: `GET /ai/v1/responses/{id}` HTTP handler

pub mod handler;
pub mod jobs;
pub mod middleware;
pub mod outlet_handler;
pub mod store;
