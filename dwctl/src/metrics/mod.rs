//! OpenTelemetry GenAI metrics implementation
//!
//! This module implements the OpenTelemetry Semantic Conventions for Generative AI,
//! providing standardized metrics for monitoring AI model requests through the proxy.
//!
//! Additional metrics (credits, analytics lag) are recorded inline using the `metrics`
//! facade in the request_logging module.

mod cache_info;
mod gen_ai;
mod recorder;

pub use cache_info::update_cache_info_metrics;
pub use gen_ai::GenAiMetrics;
pub use recorder::MetricsRecorder;
