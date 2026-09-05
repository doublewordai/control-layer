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
pub mod pools;

// Re-export only the metrics types (not DbPools/PoolProvider - use sqlx_pool_router directly)
pub use pools::{LabeledPool, PoolMetricsConfig, run_pool_metrics_sampler};

/// `PgPoolOptions` for `settings`: the one place the pool knobs are translated.
pub fn pool_options(settings: &crate::config::PoolSettings) -> sqlx::postgres::PgPoolOptions {
    use std::time::Duration;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(settings.max_connections)
        .min_connections(settings.min_connections)
        .acquire_timeout(Duration::from_secs(settings.acquire_timeout_secs))
        .idle_timeout((settings.idle_timeout_secs > 0).then(|| Duration::from_secs(settings.idle_timeout_secs)))
        .max_lifetime((settings.max_lifetime_secs > 0).then(|| Duration::from_secs(settings.max_lifetime_secs)))
}

/// The two ways a component reaches one database: `pooled` (behind a
/// transaction-mode pooler, for all query traffic) and `direct` (real backend
/// connections, for session-scoped work only: LISTEN, advisory locks held
/// across statements, migrations). With no pooled endpoint configured both
/// fields are the same `DbPools`, which is exactly the pre-pooling behaviour.
#[derive(Clone, Debug)]
pub struct PoolPair {
    pub pooled: sqlx_pool_router::DbPools,
    pub direct: sqlx_pool_router::DbPools,
}

impl PoolPair {
    /// One `DbPools` serving both roles (no pooler configured).
    pub fn unsplit(pools: sqlx_pool_router::DbPools) -> Self {
        Self {
            pooled: pools.clone(),
            direct: pools,
        }
    }

    /// Whether a distinct pooled endpoint is in use.
    pub fn is_split(&self) -> bool {
        !std::ptr::eq(
            std::sync::Arc::as_ptr(&self.pooled.write().connect_options()),
            std::sync::Arc::as_ptr(&self.direct.write().connect_options()),
        )
    }
}

/// Every database this process talks to, each as a pooled/direct pair.
#[derive(Clone, Debug)]
pub struct DatabasePools {
    pub main: PoolPair,
    pub fusillade: PoolPair,
    pub outlet: Option<PoolPair>,
}
