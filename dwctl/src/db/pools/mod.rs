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

pub mod metrics;

use sqlx::PgPool;
use std::ops::Deref;

pub use metrics::{LabeledPool, PoolMetricsConfig, run_pool_metrics_sampler};

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

    /// Get replica connection options if a replica pool is configured.
    ///
    /// Returns `None` if no replica is configured. This is useful for creating
    /// schema-based pools that mirror the primary/replica structure.
    pub fn replica_connect_options(&self) -> Option<sqlx::postgres::PgConnectOptions> {
        self.replica.as_ref().map(|pool| pool.connect_options().as_ref().clone())
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

/// Implement fusillade's PoolProvider trait.
///
/// This allows DbPools to be used with fusillade's PostgresRequestManager,
/// enabling read/write pool separation for batch processing operations.
impl fusillade::PoolProvider for DbPools {
    fn read(&self) -> &PgPool {
        self.read()
    }

    fn write(&self) -> &PgPool {
        self.write()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    /// Helper to create a test database and return its pool and name
    async fn create_test_db(admin_pool: &PgPool, suffix: &str) -> (PgPool, String) {
        let db_name = format!("test_dbpools_{}", suffix);

        // Clean up if exists
        sqlx::query(&format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{}'",
            db_name
        ))
        .execute(admin_pool)
        .await
        .ok();
        sqlx::query(&format!("DROP DATABASE IF EXISTS {}", db_name))
            .execute(admin_pool)
            .await
            .unwrap();

        // Create fresh database
        sqlx::query(&format!("CREATE DATABASE {}", db_name))
            .execute(admin_pool)
            .await
            .unwrap();

        // Connect to it
        let url = build_test_url(&db_name);
        let pool = PgPoolOptions::new().max_connections(2).connect(&url).await.unwrap();

        // Create a marker table to identify which database we're connected to
        sqlx::query("CREATE TABLE db_marker (name TEXT)").execute(&pool).await.unwrap();
        sqlx::query(&format!("INSERT INTO db_marker VALUES ('{}')", db_name))
            .execute(&pool)
            .await
            .unwrap();

        (pool, db_name)
    }

    async fn drop_test_db(admin_pool: &PgPool, db_name: &str) {
        sqlx::query(&format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{}'",
            db_name
        ))
        .execute(admin_pool)
        .await
        .ok();
        sqlx::query(&format!("DROP DATABASE IF EXISTS {}", db_name))
            .execute(admin_pool)
            .await
            .ok();
    }

    fn build_test_url(database: &str) -> String {
        if let Ok(base_url) = std::env::var("DATABASE_URL")
            && let Ok(mut url) = url::Url::parse(&base_url)
        {
            url.set_path(&format!("/{}", database));
            return url.to_string();
        }
        format!("postgres://postgres:password@localhost:5432/{}", database)
    }

    #[sqlx::test]
    async fn test_dbpools_without_replica(pool: PgPool) {
        let db_pools = DbPools::new(pool.clone());

        // Without replica, read() should return primary
        assert!(!db_pools.has_replica());

        // Both read and write should work
        let read_result: (i32,) = sqlx::query_as("SELECT 1").fetch_one(db_pools.read()).await.unwrap();
        assert_eq!(read_result.0, 1);

        let write_result: (i32,) = sqlx::query_as("SELECT 2").fetch_one(db_pools.write()).await.unwrap();
        assert_eq!(write_result.0, 2);

        // Deref should also work
        let deref_result: (i32,) = sqlx::query_as("SELECT 3").fetch_one(&*db_pools).await.unwrap();
        assert_eq!(deref_result.0, 3);
    }

    #[sqlx::test]
    async fn test_dbpools_with_replica_routes_correctly(_pool: PgPool) {
        // Create admin connection to postgres database
        let admin_url = build_test_url("postgres");
        let admin_pool = PgPoolOptions::new().max_connections(2).connect(&admin_url).await.unwrap();

        // Create two separate databases to simulate primary and replica
        let (primary_pool, primary_name) = create_test_db(&admin_pool, "primary").await;
        let (replica_pool, replica_name) = create_test_db(&admin_pool, "replica").await;

        let db_pools = DbPools::with_replica(primary_pool.clone(), replica_pool.clone());
        assert!(db_pools.has_replica());

        // read() should return replica
        let read_marker: (String,) = sqlx::query_as("SELECT name FROM db_marker")
            .fetch_one(db_pools.read())
            .await
            .unwrap();
        assert_eq!(read_marker.0, replica_name, "read() should route to replica");

        // write() should return primary
        let write_marker: (String,) = sqlx::query_as("SELECT name FROM db_marker")
            .fetch_one(db_pools.write())
            .await
            .unwrap();
        assert_eq!(write_marker.0, primary_name, "write() should route to primary");

        // Deref should return primary
        let deref_marker: (String,) = sqlx::query_as("SELECT name FROM db_marker").fetch_one(&*db_pools).await.unwrap();
        assert_eq!(deref_marker.0, primary_name, "deref should route to primary");

        // Cleanup
        primary_pool.close().await;
        replica_pool.close().await;
        drop_test_db(&admin_pool, &primary_name).await;
        drop_test_db(&admin_pool, &replica_name).await;
    }

    #[sqlx::test]
    async fn test_dbpools_close(pool: PgPool) {
        let db_pools = DbPools::new(pool);

        // Close should not panic
        db_pools.close().await;
    }
}
