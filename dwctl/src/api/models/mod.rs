//! API request and response data models.
//!
//! This module contains the data structures used for HTTP request deserialization
//! and response serialization. These models define the public API contract.
//!
//! # Design Principles
//!
//! - **Separation of Concerns**: API models are distinct from database models,
//!   allowing independent evolution of API and storage representations
//! - **Validation**: Models use serde for deserialization and validation
//! - **OpenAPI**: All models are annotated with `utoipa` for automatic API docs
//! - **Type Safety**: Strong typing with newtype wrappers for IDs
//!
//! # Model Categories
//!
//! ## Resource Models
//!
//! - [`users`]: User profiles, roles, and creation/update requests
//! - [`groups`]: Group definitions and membership relationships
//! - [`deployments`]: Model deployment configurations
//! - [`inference_endpoints`]: Backend inference endpoint configurations
//! - [`api_keys`]: API key metadata (secrets are never returned)
//! - [`probes`]: Health probe definitions and results
//!
//! ## Transaction Models
//!
//! - [`transactions`]: Credit allocation and usage transactions
//! - [`requests`]: Request logs and analytics data
//! - [`batches`]: Batch request specifications and status
//! - [`files`]: File metadata for batch processing
//!
//! ## Authentication Models
//!
//! - [`auth`]: Login, registration, and password management payloads
//!
//! # Example
//!
//! ```ignore
//! use dwctl::api::models::users::{UserCreate, UserResponse};
//!
//! // Deserialize from JSON
//! let create_req: UserCreate = serde_json::from_str(json_str)?;
//!
//! // Serialize to JSON
//! let response = UserResponse { /* ... */ };
//! let json = serde_json::to_string(&response)?;
//! ```

pub mod api_keys;
pub mod auth;
pub mod batches;
pub mod deployments;
pub mod files;
pub mod groups;
pub mod inference_endpoints;
pub mod probes;
pub mod requests;
pub mod transactions;
pub mod users;
