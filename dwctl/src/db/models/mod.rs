//! Database record models matching table schemas.
//!
//! This module contains struct definitions that directly correspond to database
//! table rows. These models are used by repositories to return query results
//! and accept insertion/update data.
//!
//! # Design Principles
//!
//! - **Schema Mapping**: Each model struct matches a database table schema
//! - **SQLx Integration**: Models derive `sqlx::FromRow` for query results
//! - **Separation**: Database models are distinct from API models to allow
//!   independent evolution of storage and API representations
//! - **Type Safety**: Uses newtype wrappers for IDs (UserId, GroupId, etc.)
//!
//! # Model Categories
//!
//! ## Core Resources
//!
//! - [`users`]: User accounts, authentication, and profiles
//! - [`groups`]: Group definitions and user-group memberships
//! - [`deployments`]: Model deployment configurations and routing
//! - [`inference_endpoints`]: Backend inference service endpoints
//!
//! ## Access Control
//!
//! - [`api_keys`]: API keys for programmatic access
//! - [`password_reset_tokens`]: Time-limited password reset tokens
//!
//! ## Operations
//!
//! - [`credits`]: Credit balance tracking and transaction history
//! - [`probes`]: Health probe definitions and execution results
//!
//! # Conversion to API Models
//!
//! Database models typically implement `From` or `Into` conversions to API models:
//!
//! ```ignore
//! use dwctl::db::models::users::User as DbUser;
//! use dwctl::api::models::users::UserResponse;
//!
//! let db_user: DbUser = /* ... */;
//! let api_response: UserResponse = db_user.into();
//! ```
//!
//! # Example
//!
//! ```ignore
//! use dwctl::db::models::users::User;
//! use sqlx::PgPool;
//!
//! async fn fetch_user(pool: &PgPool, email: &str) -> Result<Option<User>, sqlx::Error> {
//!     sqlx::query_as!(
//!         User,
//!         "SELECT * FROM users WHERE email = $1",
//!         email
//!     )
//!     .fetch_optional(pool)
//!     .await
//! }
//! ```

pub mod api_keys;
pub mod credits;
pub mod deployments;
pub mod groups;
pub mod inference_endpoints;
pub mod password_reset_tokens;
pub mod probes;
pub mod tariffs;
pub mod users;
pub mod webhooks;
