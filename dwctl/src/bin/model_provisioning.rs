use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use dwctl::model_provisioning::Catalog;

#[derive(Debug, Parser)]
#[command(about = "Validate and describe dwctl model provisioning catalogs")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate every YAML document in a provisioning directory as one catalog.
    Validate { directory: PathBuf },
    /// Print the JSON Schema for one model document.
    Schema,
}

fn main() -> Result<()> {
    match Args::parse().command {
        Command::Validate { directory } => {
            Catalog::load(&directory)?;
            println!("model provisioning catalog is valid: {}", directory.display());
        }
        Command::Schema => println!("{}", Catalog::json_schema()?),
    }
    Ok(())
}
