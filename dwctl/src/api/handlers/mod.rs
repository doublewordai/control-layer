//! HTTP request handlers for all API endpoints.
//!
//! This module contains Axum route handlers organized by resource type.
//!
//! # Handler Modules
//!
//! - [`api_keys`]: API key creation, listing, and deletion for users
//! - [`auth`]: Authentication, login, registration, and password management
//! - [`batches`]: Batch request creation, monitoring, and cancellation
//! - [`config`]: Application configuration retrieval
//! - [`deployments`]: Model deployment CRUD operations and group assignments
//! - [`files`]: File upload, download, and management for batch processing
//! - [`groups`]: Group management, user memberships, and model access
//! - [`inference_endpoints`]: Inference endpoint CRUD and synchronization
//! - [`payments`]: Payment processing and checkout session creation
//! - [`probes`]: Health probe configuration, execution, and result retrieval
//! - [`requests`]: Request logging, analytics, and aggregation
//! - [`static_assets`]: Frontend asset serving and SPA routing
//! - [`transactions`]: Credit transaction creation and history
//! - [`users`]: User CRUD operations and profile management
//!
//! # Authentication
//!
//! Most handlers require authentication via session cookies, API keys or trusted headers. The
//! [`crate::auth`] module provides authentication utilities that handlers can use to access the
//! current user.
//!
//! # Error Handling
//!
//! Handlers return [`crate::errors::Error`] which automatically converts to
//! appropriate HTTP status codes and JSON error responses. See [`crate::errors`]
//! for details on error types and HTTP status mappings.

pub mod api_keys;
pub mod auth;
pub mod batches;
pub mod config;
pub mod daemons;
pub mod deployments;
pub mod files;
pub mod groups;
pub mod inference_endpoints;
pub mod payments;
pub mod probes;
pub mod queue;
pub mod requests;
pub mod sla_capacity;
pub mod static_assets;
pub mod transactions;
pub mod users;
