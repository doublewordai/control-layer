//! OpenTelemetry GenAI metrics implementation
//!
//! This module implements the OpenTelemetry Semantic Conventions for Generative AI,
//! providing standardized metrics for monitoring AI model requests through the proxy.

mod gen_ai;
mod recorder;

pub use gen_ai::GenAiMetrics;
pub use recorder::MetricsRecorder;
