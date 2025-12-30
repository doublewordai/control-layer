//! Database pool abstraction supporting read replicas.
//!
//! This module provides [`DbPools`], a wrapper around SQLx connection pools that
//! supports routing queries to read replicas for improved scalability.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────┐
//! │   DbPools   │
//! └──────┬──────┘
//!        │
//!   ┌────┴────┐
//!   ↓         ↓
//! ┌─────┐  ┌─────────┐
//! │Primary│  │ Replica │ (optional)
//! └─────┘  └─────────┘
//! ```
//!
//! # Usage
//!
//! `DbPools` implements `Deref<Target = PgPool>`, so existing code that uses
//! `&state.db` directly will continue to work (routing to primary).
//!
//! For explicit routing:
//! - Use `.read()` for read-only operations (uses replica if available)
//! - Use `.write()` for write operations (always uses primary)
//! - Use `.begin()` for transactions (always uses primary)
//!
//! # Example
//!
//! ```ignore
//! // Existing code works unchanged (routes to primary via Deref)
//! let mut tx = state.db.begin().await?;
//!
//! // Explicit read routing (uses replica if configured)
//! let users = list_users(state.db.read()).await?;
//!
//! // Explicit write routing
//! let user = create_user(state.db.write()).await?;
//! ```

use sqlx::PgPool;
use std::ops::Deref;

/// Database pool abstraction supporting read replicas.
///
/// Wraps primary and optional replica pools, providing methods for
/// explicit read/write routing while maintaining backwards compatibility
/// through `Deref<Target = PgPool>`.
#[derive(Clone, Debug)]
pub struct DbPools {
    primary: PgPool,
    replica: Option<PgPool>,
}

impl DbPools {
    /// Create a new DbPools with only a primary pool.
    pub fn new(primary: PgPool) -> Self {
        Self { primary, replica: None }
    }

    /// Create a new DbPools with primary and replica pools.
    pub fn with_replica(primary: PgPool, replica: PgPool) -> Self {
        Self {
            primary,
            replica: Some(replica),
        }
    }

    /// Get a pool for read-only operations.
    ///
    /// Returns the replica pool if configured, otherwise falls back to primary.
    /// Use this for queries that can tolerate slight staleness (eventual consistency).
    ///
    /// # Examples
    ///
    /// - Listing users, groups, models
    /// - Analytics and reporting queries
    /// - Dashboard data
    /// - Search operations
    pub fn read(&self) -> &PgPool {
        self.replica.as_ref().unwrap_or(&self.primary)
    }

    /// Get a pool for write operations or reads requiring strong consistency.
    ///
    /// Always returns the primary pool. Use this for:
    /// - Creating, updating, or deleting records
    /// - Operations that read-after-write
    /// - Credit/balance operations (require serializable consistency)
    /// - Any operation using advisory locks
    ///
    /// Note: For most write operations, you can use Deref coercion directly
    /// (e.g., `state.db.begin()` or `&*state.db`).
    pub fn write(&self) -> &PgPool {
        &self.primary
    }

    /// Check if a replica pool is configured.
    pub fn has_replica(&self) -> bool {
        self.replica.is_some()
    }

    /// Close all database connections.
    ///
    /// Closes both primary and replica pools (if configured).
    pub async fn close(&self) {
        self.primary.close().await;
        if let Some(replica) = &self.replica {
            replica.close().await;
        }
    }
}

/// Dereferences to the primary pool.
///
/// This allows natural usage like `state.db.begin()`, `state.db.acquire()`,
/// or `&*state.db` when you need a `&PgPool`. Use `.read()` when you
/// explicitly want to route to the replica for read-heavy operations.
impl Deref for DbPools {
    type Target = PgPool;

    fn deref(&self) -> &Self::Target {
        &self.primary
    }
}
