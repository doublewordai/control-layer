//! Embedded PostgreSQL database support
//!
//! This module provides functionality to run a bundled PostgreSQL instance
//! that can be started and stopped with the application. This is useful for
//! single-binary distributions where you don't want to require external
//! database setup.
//!
//! When built with the `embedded-db` feature, PostgreSQL binaries are bundled
//! into the binary at compile time. Set POSTGRESQL_VERSION environment variable
//! during build to specify the version (e.g., "16.4.0").

#[cfg(feature = "embedded-db")]
use postgresql_embedded::{PostgreSQL, Settings, V16};
#[cfg(feature = "embedded-db")]
use std::path::PathBuf;
#[cfg(feature = "embedded-db")]
use tracing::{debug, info};

#[cfg(feature = "embedded-db")]
pub struct EmbeddedDatabase {
    postgres: PostgreSQL,
    connection_string: String,
}

#[cfg(feature = "embedded-db")]
impl EmbeddedDatabase {
    /// Create and start a new embedded PostgreSQL instance
    ///
    /// Uses an ephemeral port (assigned by the OS) to avoid conflicts.
    ///
    /// # Arguments
    /// * `data_dir` - Directory where PostgreSQL data will be stored (default: `$HOME/clay_data/postgres`)
    /// * `persistent` - Whether to persist data between restarts (default: false/ephemeral)
    ///
    /// # Returns
    /// A running EmbeddedDatabase instance with connection string containing the actual port
    pub async fn start(data_dir: Option<PathBuf>, persistent: bool) -> anyhow::Result<Self> {
        let data_dir = data_dir.unwrap_or_else(|| {
            // Default to $HOME/clay_data/postgres, fallback to ./clay_data/postgres if HOME not available
            if let Some(home) = std::env::home_dir() {
                home.join(".clay_data").join("postgres")
            } else {
                PathBuf::from("clay_data/postgres")
            }
        });

        if persistent {
            use tracing::debug;

            debug!("Starting embedded PostgreSQL with data directory: {}", data_dir.display());
        } else {
            debug!("Starting ephemeral embedded PostgreSQL");
        }

        // Create settings for the embedded PostgreSQL instance
        let settings = Settings {
            version: V16.clone(), // Use PostgreSQL 16 - set POSTGRESQL_VERSION at build time for specific version
            port: 0,              // Use ephemeral port (OS will assign)
            username: "postgres".to_string(),
            password: "password".to_string(),
            temporary: !persistent, // If persistent=false, temporary=true (ephemeral)
            installation_dir: data_dir.join("installation"),
            data_dir: data_dir.join("data"),
            ..Default::default()
        };

        // Create and setup the PostgreSQL instance
        let mut postgres = PostgreSQL::new(settings);

        // Setup downloads binaries (if not bundled) and initializes the database
        postgres
            .setup()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to setup embedded PostgreSQL: {}", e))?;

        // Start the PostgreSQL server
        postgres
            .start()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to start embedded PostgreSQL: {}", e))?;

        // Get the actual port that was assigned
        let actual_port = postgres.settings().port;

        // Create the default database
        let database_name = "clay";
        postgres
            .create_database(database_name)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create database '{}': {}", database_name, e))?;

        let connection_string = postgres.settings().url(database_name);

        info!("Embedded PostgreSQL started successfully on port {}", actual_port);

        Ok(Self {
            postgres,
            connection_string,
        })
    }

    /// Get the connection string for this embedded database
    pub fn connection_string(&self) -> &str {
        &self.connection_string
    }

    /// Stop the embedded PostgreSQL instance
    pub async fn stop(self) -> anyhow::Result<()> {
        info!("Stopping embedded PostgreSQL...");
        self.postgres
            .stop()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to stop embedded PostgreSQL: {}", e))?;
        info!("Embedded PostgreSQL stopped");
        Ok(())
    }
}

#[cfg(not(feature = "embedded-db"))]
pub struct EmbeddedDatabase;

#[cfg(not(feature = "embedded-db"))]
#[allow(dead_code)]
impl EmbeddedDatabase {
    pub async fn start(_data_dir: Option<std::path::PathBuf>, _persistent: bool) -> anyhow::Result<Self> {
        anyhow::bail!(
            "Embedded database feature is not enabled. \
             Rebuild with --features embedded-db to use this feature."
        )
    }

    pub fn connection_string(&self) -> &str {
        ""
    }

    pub fn port(&self) -> u16 {
        0
    }

    pub async fn stop(self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[cfg(feature = "embedded-db")]
mod tests {
    use super::*;

    #[test]
    fn test_mask_password() {
        let url = "postgresql://user:secret@localhost:5432/dbname";
        let masked = mask_password(url);
        assert_eq!(masked, "postgresql://user:****@localhost:5432/dbname");
    }

    #[test]
    fn test_mask_password_no_password() {
        let url = "postgresql://localhost:5432/dbname";
        let masked = mask_password(url);
        assert_eq!(masked, url);
    }
}
