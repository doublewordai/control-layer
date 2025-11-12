//! API layer for HTTP request handling and data models.
//!
//! This module contains the REST API implementation, organized into:
//!
//! - **[`handlers`]**: Axum route handlers for all API endpoints
//! - **[`models`]**: Request/response data structures for API communication
//!
//! # API Structure
//!
//! The API is divided into several functional areas:
//!
//! - **Authentication** (`/authentication/*`): Login, registration, password management
//! - **Users** (`/admin/api/v1/users/*`): User management and API keys
//! - **Groups** (`/admin/api/v1/groups/*`): Group management and memberships
//! - **Deployments** (`/admin/api/v1/models/*`): Model deployment configuration
//! - **Endpoints** (`/admin/api/v1/endpoints/*`): Inference endpoint management
//! - **Transactions** (`/admin/api/v1/transactions/*`): Credit transaction management
//! - **Probes** (`/admin/api/v1/probes/*`): Health probe configuration
//! - **Files & Batches** (`/admin/api/v1/files/*`, `/admin/api/v1/batches/*`): Batch processing
//! - **AI Proxy** (`/ai/v1/*`): OpenAI-compatible proxy endpoints
//!
//! # OpenAPI Documentation
//!
//! All endpoints are documented with OpenAPI/Swagger annotations using `utoipa`.
//! API documentation is available at `/admin/docs` when the server is running.

pub mod handlers;
pub mod models;
