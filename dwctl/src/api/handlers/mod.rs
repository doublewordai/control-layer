//! HTTP request handlers for all API endpoints.
//!
//! This module contains Axum route handlers organized by resource type.
//! Each handler is responsible for:
//! - Request validation and deserialization
//! - Authentication and authorization checks
//! - Business logic execution via database repositories
//! - Response serialization
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
//! - [`probes`]: Health probe configuration, execution, and result retrieval
//! - [`requests`]: Request logging, analytics, and aggregation
//! - [`static_assets`]: Frontend asset serving and SPA routing
//! - [`transactions`]: Credit transaction creation and history
//! - [`users`]: User CRUD operations and profile management
//!
//! # Authentication
//!
//! Most handlers require authentication via session cookies or API keys.
//! The [`crate::auth::middleware`] module provides authentication extractors
//! that handlers can use to access the current user.
//!
//! # Error Handling
//!
//! Handlers return [`crate::errors::AppError`] which automatically converts to
//! appropriate HTTP status codes and JSON error responses.

pub mod api_keys;
pub mod auth;
pub mod batches;
pub mod config;
pub mod deployments;
pub mod files;
pub mod groups;
pub mod inference_endpoints;
pub mod probes;
pub mod requests;
pub mod static_assets;
pub mod transactions;
pub mod users;
