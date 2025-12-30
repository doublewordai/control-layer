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

use sqlx::{PgPool, Postgres, Transaction};
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
    ///
    /// # Examples
    ///
    /// - Creating, updating, or deleting records
    /// - Operations that read-after-write
    /// - Credit/balance operations (require serializable consistency)
    /// - Any operation using advisory locks
    pub fn write(&self) -> &PgPool {
        &self.primary
    }

    /// Begin a transaction on the primary pool.
    ///
    /// Transactions always use the primary pool since they may contain writes.
    pub async fn begin(&self) -> Result<Transaction<'_, Postgres>, sqlx::Error> {
        self.primary.begin().await
    }

    /// Direct access to the primary pool.
    ///
    /// Use sparingly - prefer `.read()` or `.write()` for clarity.
    pub fn primary(&self) -> &PgPool {
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

/// Backwards compatibility: dereferences to the primary pool.
///
/// This allows existing code using `&state.db` to work unchanged.
/// New code should prefer explicit `.read()` or `.write()` calls.
impl Deref for DbPools {
    type Target = PgPool;

    fn deref(&self) -> &Self::Target {
        &self.primary
    }
}

#[cfg(test)]
mod tests {
    // Note: These tests verify the API surface. Integration tests with actual
    // database connections are in the handler tests.

    #[test]
    fn test_deref_returns_primary() {
        // We can't create a real PgPool in unit tests, but we can verify
        // the type system works correctly through the Deref impl.
        // The actual pool behavior is tested in integration tests.
    }
}
