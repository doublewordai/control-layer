//! Database layer for data persistence and access.
//!
//! This module implements the data access layer using SQLx with PostgreSQL.
//! It follows the Repository pattern to provide clean abstractions over database operations.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────┐
//! │  Handlers   │  (API request handlers)
//! └──────┬──────┘
//!        │
//!        ↓
//! ┌─────────────┐
//! │ Repositories│  (db::handlers - business logic & queries)
//! └──────┬──────┘
//!        │
//!        ↓
//! ┌─────────────┐
//! │   Models    │  (db::models - database records)
//! └──────┬──────┘
//!        │
//!        ↓
//! ┌─────────────┐
//! │  PostgreSQL │
//! └─────────────┘
//! ```
//!
//! # Modules
//!
//! - [`handlers`]: Repository implementations for CRUD operations
//! - [`models`]: Database record structures matching table schemas
//! - [`errors`]: Database-specific error types
//! - [`embedded`]: Embedded PostgreSQL database support (optional feature)
//!
//! # Repository Pattern
//!
//! The [`handlers`] module provides repository traits and implementations
//! for each database table. Repositories encapsulate all database access
//! for a specific entity type.
//!
//! ## Example Usage
//!
//! ```ignore
//! use dwctl::db::handlers::{Users, Repository};
//!
//! async fn example(pool: &sqlx::PgPool) -> Result<(), Box<dyn std::error::Error>> {
//!     let mut tx = pool.begin().await?;
//!     let mut users_repo = Users::new(&mut tx);
//!
//!     // Create a user
//!     let user = users_repo.create(&create_request).await?;
//!
//!     // Fetch by email
//!     if let Some(user) = users_repo.get_user_by_email("user@example.com").await? {
//!         println!("Found user: {}", user.username);
//!     }
//!
//!     tx.commit().await?;
//!     Ok(())
//! }
//! ```
//!
//! # Transactions
//!
//! Repositories work with SQLx transactions to ensure ACID properties.
//! Always create repositories from a transaction, not directly from the pool:
//!
//! ```ignore
//! // Good: using a transaction
//! let mut tx = pool.begin().await?;
//! let mut repo = Users::new(&mut tx);
//! // ... operations ...
//! tx.commit().await?;
//!
//! // Bad: using pool directly (only for read-only operations)
//! let mut conn = pool.acquire().await?;
//! let mut repo = Users::new(&mut conn);
//! ```
//!
//! # Migrations
//!
//! Database migrations are managed by SQLx and located in the `migrations/` directory.
//! The [`crate::migrator`] function provides access to the migrator:
//!
//! ```ignore
//! dwctl::migrator().run(&pool).await?;
//! ```

pub mod embedded;
pub mod errors;
pub mod handlers;
pub mod models;
