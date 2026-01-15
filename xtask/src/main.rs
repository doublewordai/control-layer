//! Development tasks for control-layer
//!
//! This binary provides development tooling that doesn't require a running database
//! to compile. It uses embedded PostgreSQL to bootstrap the environment for SQLx
//! compile-time query verification.
//!
//! ## Commands
//!
//! - `xtask prepare` - Start embedded postgres, run migrations, run `cargo sqlx prepare`, exit
//! - `xtask serve` - Start embedded postgres and keep it running for interactive development

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use postgresql_embedded::{PostgreSQL, Settings, V16};
use std::path::PathBuf;
use std::process::Command;

#[derive(Parser)]
#[command(name = "xtask")]
#[command(about = "Development tasks for control-layer")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start embedded postgres, run migrations, generate SQLx prepared queries, then exit
    Prepare {
        /// Keep postgres running after prepare (useful for subsequent cargo builds)
        #[arg(long)]
        keep_running: bool,
    },
    /// Start embedded postgres and keep it running for interactive development
    Serve,
}

struct EmbeddedPostgres {
    postgres: PostgreSQL,
    port: u16,
}

impl EmbeddedPostgres {
    async fn start() -> Result<Self> {
        let data_dir = Self::data_dir();

        println!("Starting embedded PostgreSQL...");
        println!("  Data directory: {}", data_dir.display());

        let settings = Settings {
            version: V16.clone(),
            port: 0, // Ephemeral port
            username: "postgres".to_string(),
            password: "password".to_string(),
            temporary: false, // Persist data for faster subsequent starts
            installation_dir: data_dir.join("installation"),
            data_dir: data_dir.join("data"),
            ..Default::default()
        };

        let mut postgres = PostgreSQL::new(settings);

        postgres
            .setup()
            .await
            .context("Failed to setup embedded PostgreSQL")?;

        postgres
            .start()
            .await
            .context("Failed to start embedded PostgreSQL")?;

        let port = postgres.settings().port;
        println!("  PostgreSQL started on port {}", port);

        // Create the dwctl database
        match postgres.create_database("dwctl").await {
            Ok(_) => println!("  Created database 'dwctl'"),
            Err(e) if e.to_string().contains("already exists") => {
                println!("  Database 'dwctl' already exists")
            }
            Err(e) => return Err(e).context("Failed to create dwctl database"),
        }

        Ok(Self { postgres, port })
    }

    fn data_dir() -> PathBuf {
        // Use a dedicated directory for xtask's embedded postgres
        if let Some(home) = std::env::home_dir() {
            home.join(".dwctl_xtask").join("postgres")
        } else {
            PathBuf::from(".dwctl_xtask/postgres")
        }
    }

    fn database_url(&self) -> String {
        // Match the format used in db-setup justfile target
        format!(
            "postgres://postgres:password@127.0.0.1:{}/dwctl?options=-c%20search_path%3Dfusillade%2Cpublic",
            self.port
        )
    }

    async fn stop(self) -> Result<()> {
        println!("Stopping embedded PostgreSQL...");
        self.postgres
            .stop()
            .await
            .context("Failed to stop embedded PostgreSQL")?;
        println!("  PostgreSQL stopped");
        Ok(())
    }
}

fn run_migrations(database_url: &str) -> Result<()> {
    println!("Running migrations...");

    let status = Command::new("sqlx")
        .args(["migrate", "run"])
        .current_dir("dwctl")
        .env("DATABASE_URL", database_url)
        .status()
        .context("Failed to run sqlx migrate")?;

    if !status.success() {
        anyhow::bail!("sqlx migrate failed with status: {}", status);
    }

    println!("  Migrations complete");
    Ok(())
}

fn run_sqlx_prepare(database_url: &str) -> Result<()> {
    println!("Running cargo sqlx prepare...");

    let status = Command::new("cargo")
        .args(["sqlx", "prepare", "--workspace"])
        .env("DATABASE_URL", database_url)
        .status()
        .context("Failed to run cargo sqlx prepare")?;

    if !status.success() {
        anyhow::bail!("cargo sqlx prepare failed with status: {}", status);
    }

    println!("  SQLx prepare complete");
    Ok(())
}

fn write_env_file(database_url: &str) -> Result<()> {
    std::fs::write("dwctl/.env", format!("DATABASE_URL={}\n", database_url))
        .context("Failed to write dwctl/.env")?;
    println!("  Wrote DATABASE_URL to dwctl/.env");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Prepare { keep_running } => {
            let pg = EmbeddedPostgres::start().await?;
            let database_url = pg.database_url();

            write_env_file(&database_url)?;
            run_migrations(&database_url)?;
            run_sqlx_prepare(&database_url)?;

            if keep_running {
                println!();
                println!("Embedded PostgreSQL is running.");
                println!("  DATABASE_URL={}", database_url);
                println!();
                println!("Press Ctrl+C to stop...");

                tokio::signal::ctrl_c()
                    .await
                    .context("Failed to listen for Ctrl+C")?;
            }

            pg.stop().await?;
            println!();
            println!("Done! SQLx prepared queries are ready.");
            println!(
                "You can now compile with SQLX_OFFLINE=true or use 'just db-start' for development."
            );
        }
        Commands::Serve => {
            let pg = EmbeddedPostgres::start().await?;
            let database_url = pg.database_url();

            write_env_file(&database_url)?;
            run_migrations(&database_url)?;

            println!();
            println!("Embedded PostgreSQL is running for development.");
            println!();
            println!("  DATABASE_URL={}", database_url);
            println!();
            println!("You can now run:");
            println!("  cargo build           # Compile with live database");
            println!("  cargo test            # Run tests");
            println!("  cargo run             # Start the server");
            println!();
            println!("Press Ctrl+C to stop...");

            tokio::signal::ctrl_c()
                .await
                .context("Failed to listen for Ctrl+C")?;

            pg.stop().await?;
        }
    }

    Ok(())
}
