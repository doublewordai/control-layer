//! Request persistence for the realtime and flex inference paths.
//!
//! - **writer**: batched in-process persistence of completed requests.
//! - **outlet_handler**: outlet `RequestHandler` that feeds the writer channel.
//! - **dispatch_processor**: pre-dispatch preparation (ZDR decrypt, JIT image
//!   signing) for daemon-claimed requests, before the loopback call.

pub mod dispatch_processor;
pub mod outlet_handler;
pub mod writer;
