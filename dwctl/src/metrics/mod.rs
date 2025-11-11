//! OpenTelemetry GenAI metrics implementation
//!
//! This module implements the OpenTelemetry Semantic Conventions for Generative AI,
//! providing standardized metrics for monitoring AI model requests through the proxy.

mod credits;
mod gen_ai;
mod recorder;

pub use credits::{record_credit_deduction, record_credit_deduction_error};
pub use gen_ai::GenAiMetrics;
pub use recorder::MetricsRecorder;
