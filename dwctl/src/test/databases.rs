//! Test utilities for multi-database testing.
//!
//! Provides helpers for testing the dedicated database feature where
//! fusillade and outlet can use separate databases instead of schemas.

use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};

/// Helper for creating and managing multiple isolated test databases.
///
/// Creates empty databases that the Application will run migrations on.
/// Used to test the dedicated database configuration where fusillade
/// and outlet use separate databases instead of schemas within the main DB.
pub struct TestDatabases {
    /// Pool connected to 'postgres' database for admin operations
    admin_pool: PgPool,
    /// Name of the created fusillade database
    fusillade_db_name: String,
    /// Name of the created outlet database
    outlet_db_name: String,
    /// Connection URL for fusillade database
    pub fusillade_url: String,
    /// Connection URL for outlet database
    pub outlet_url: String,
}

impl TestDatabases {
    /// Create empty test databases for fusillade and outlet.
    ///
    /// The Application will run migrations when it starts.
    /// Cleans up any existing databases with the same names first (idempotent).
    ///
    /// # Arguments
    ///
    /// * `main_pool` - Pool from `#[sqlx::test]` to extract connection info
    /// * `test_prefix` - Prefix for database names (e.g., test function name)
    pub async fn new(main_pool: &PgPool, test_prefix: &str) -> anyhow::Result<Self> {
        // Sanitize prefix to be a valid database name component
        let safe_prefix: String = test_prefix.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();

        let fusillade_db_name = format!("test_{}_fusillade", safe_prefix);
        let outlet_db_name = format!("test_{}_outlet", safe_prefix);

        // Extract connection options from main pool
        let connect_opts = main_pool.connect_options();
        let opts = connect_opts.as_ref();

        // Build base URL for connecting to postgres database (for admin operations)
        let base_url = build_connection_url(opts, "postgres");
        let admin_pool = PgPoolOptions::new().max_connections(2).connect(&base_url).await?;

        // Clean up any existing databases first (idempotent)
        Self::drop_database_if_exists(&admin_pool, &fusillade_db_name).await?;
        Self::drop_database_if_exists(&admin_pool, &outlet_db_name).await?;

        // Create fresh empty databases
        admin_pool
            .execute(format!("CREATE DATABASE {}", fusillade_db_name).as_str())
            .await?;
        admin_pool.execute(format!("CREATE DATABASE {}", outlet_db_name).as_str()).await?;

        // Build connection URLs for the new databases
        let fusillade_url = build_connection_url(opts, &fusillade_db_name);
        let outlet_url = build_connection_url(opts, &outlet_db_name);

        Ok(Self {
            admin_pool,
            fusillade_db_name,
            outlet_db_name,
            fusillade_url,
            outlet_url,
        })
    }

    /// Clean up the test databases.
    ///
    /// Drops the databases. Call this at the end of tests.
    pub async fn cleanup(self) -> anyhow::Result<()> {
        Self::drop_database_if_exists(&self.admin_pool, &self.fusillade_db_name).await?;
        Self::drop_database_if_exists(&self.admin_pool, &self.outlet_db_name).await?;
        self.admin_pool.close().await;
        Ok(())
    }

    /// Drop a database if it exists, terminating any active connections first.
    async fn drop_database_if_exists(admin_pool: &PgPool, db_name: &str) -> anyhow::Result<()> {
        // Terminate any existing connections to the database
        admin_pool
            .execute(
                format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{}'",
                    db_name
                )
                .as_str(),
            )
            .await
            .ok(); // Ignore errors (database might not exist)

        // Drop the database
        admin_pool.execute(format!("DROP DATABASE IF EXISTS {}", db_name).as_str()).await?;

        Ok(())
    }
}

/// Build a connection URL from PgConnectOptions for a specific database.
fn build_connection_url(opts: &sqlx::postgres::PgConnectOptions, database: &str) -> String {
    let host = opts.get_host();
    let port = opts.get_port();
    let username = opts.get_username();

    // For tests, DATABASE_URL typically includes credentials
    if let Ok(base_url) = std::env::var("DATABASE_URL") {
        // Parse the base URL and replace the database name
        if let Ok(mut url) = url::Url::parse(&base_url) {
            url.set_path(&format!("/{}", database));
            return url.to_string();
        }
    }

    // Fallback: construct URL without password (relies on .pgpass or trust auth)
    format!("postgres://{}@{}:{}/{}", username, host, port, database)
}
