//! Repository implementations for database access.
//!
//! This module provides repository structs for each major entity in the system.
//! Repositories follow a consistent pattern and implement the [`Repository`] trait.
//!
//! # Design Pattern
//!
//! Each repository:
//! - Wraps a SQLx connection or transaction
//! - Provides strongly-typed CRUD operations
//! - Handles query construction and parameter binding
//! - Returns domain models from [`crate::db::models`]
//! - Uses the connection's transaction for ACID guarantees
//!
//! # Available Repositories
//!
//! - [`Users`]: User account management and authentication
//! - [`Groups`]: Group definitions and user memberships
//! - [`Deployments`]: Model deployment configurations
//! - [`InferenceEndpoints`]: Backend inference endpoint management
//! - [`Credits`]: Credit balance tracking and transactions
//! - [`PasswordResetTokens`]: Password reset token lifecycle
//! - [`analytics`]: Request logging and analytics queries
//! - [`api_keys`]: API key management (not re-exported)
//!
//! # Common Pattern
//!
//! All repositories follow this usage pattern:
//!
//! ```ignore
//! use dwctl::db::handlers::{Users, Repository};
//!
//! async fn example(pool: &sqlx::PgPool) -> Result<(), Box<dyn std::error::Error>> {
//!     // Start a transaction
//!     let mut tx = pool.begin().await?;
//!
//!     // Create repository from transaction
//!     let mut repo = Users::new(&mut tx);
//!
//!     // Perform operations
//!     let users = repo.list(None, None).await?;
//!
//!     // Commit or rollback
//!     tx.commit().await?;
//!     Ok(())
//! }
//! ```
//!
//! # The Repository Trait
//!
//! The [`Repository`] trait defines common CRUD operations that all repositories
//! should implement:
//!
//! - `new()`: Create a new repository instance
//! - `create()`: Insert a new record
//! - `get()`: Fetch a record by ID
//! - `list()`: List records with pagination
//! - `delete()`: Delete a record by ID

pub mod analytics;
pub mod api_keys;
pub mod credits;
pub mod deployments;
pub mod groups;
pub mod inference_endpoints;
pub mod password_reset_tokens;
pub mod repository;
pub mod tariffs;
pub mod users;
pub mod webhooks;

pub use credits::Credits;
pub use deployments::Deployments;
pub use groups::Groups;
pub use inference_endpoints::InferenceEndpoints;
pub use password_reset_tokens::PasswordResetTokens;
pub use repository::Repository;
pub use tariffs::Tariffs;
pub use users::Users;
pub use webhooks::Webhooks;
