//! Request persistence for the realtime and flex inference paths.
//!
//! - **writer**: batched in-process persistence of completed requests.
//! - **outlet_handler**: outlet `RequestHandler` that feeds the writer channel.

pub mod outlet_handler;
pub mod writer;
