//! Authentication and authorization system.
//!
//! This module provides a comprehensive auth system including:
//! - User authentication (session-based and API key-based)
//! - Password hashing and validation
//! - Session management with Redis-backed storage
//! - Permission checking and access control
//! - Middleware for protecting routes
//!
//! # Authentication Methods
//!
//! The system supports two authentication methods:
//!
//! ## 1. Session Authentication
//!
//! Browser-based authentication using secure HTTP-only cookies:
//! - Users log in via `/authentication/login` with email/password
//! - Session ID stored in secure, HTTP-only cookie
//! - Session data backed by Redis for scalability
//! - Automatic session expiration and renewal
//!
//! ## 2. API Key Authentication
//!
//! Token-based authentication for programmatic access:
//! - API keys created per-user via `/users/{id}/api-keys`
//! - Passed in `Authorization: Bearer <key>` header
//! - No expiration (manually revoked when needed)
//! - Scoped to individual users
//!
//! # Authorization
//!
//! Access control is managed through:
//! - **Roles**: Platform-wide permissions (PlatformManager, StandardUser, etc.)
//! - **Groups**: Resource-based access (users in groups can access group models)
//! - **Ownership**: Users can modify their own resources
//!
//! See [`permissions`] for details on the permission system.
//!
//! # Modules
//!
//! - [`current_user`]: Extractors for getting the authenticated user in handlers
//! - [`middleware`]: Route protection middleware
//! - [`password`]: Password hashing and verification using Argon2
//! - [`permissions`]: Permission checking and access control logic
//! - [`session`]: Session management and storage
//! - [`utils`]: Authentication helper functions
//!
//! # Usage in Handlers
//!
//! ## Session Authentication
//!
//! ```ignore
//! use dwctl::auth::current_user::CurrentUser;
//! use axum::extract::State;
//!
//! async fn protected_handler(
//!     CurrentUser(user): CurrentUser,
//!     State(state): State<AppState>,
//! ) -> Result<String, AppError> {
//!     Ok(format!("Hello, {}!", user.username))
//! }
//! ```
//!
//! ## API Key Authentication
//!
//! ```ignore
//! use dwctl::auth::current_user::ApiKeyUser;
//!
//! async fn api_handler(
//!     ApiKeyUser(user): ApiKeyUser,
//! ) -> Result<String, AppError> {
//!     Ok(format!("API access for user {}", user.id))
//! }
//! ```
//!
//! ## Permission Checking
//!
//! ```ignore
//! use dwctl::auth::permissions::check_model_access;
//!
//! // Check if user can access a specific model deployment
//! check_model_access(&mut tx, user.id, deployment_id).await?;
//! ```

pub mod current_user;
pub mod middleware;
pub mod password;
pub mod permissions;
pub mod session;
pub mod utils;
