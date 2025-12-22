//! OpenAPI/Swagger documentation configuration.
//!
//! This module provides OpenAPI documentation for the two main API surfaces:
//! - [`admin::AdminApiDoc`]: Management API at `/admin/api/v1/*`
//! - [`ai::AiApiDoc`]: OpenAI-compatible API at `/ai/v1/*`

pub mod admin;
pub mod ai;
mod extra_types;

pub use admin::AdminApiDoc;
pub use ai::AiApiDoc;
