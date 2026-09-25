use std::path::PathBuf;

use anyhow::{Result, ensure};
use clap::{Parser, Subcommand};
use dwctl::{model_provisioning::Catalog, org_overlays::OrgCatalog};

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
    /// Validate org overlays and references against the complete model catalog, without a database.
    ValidateOrgOverlays {
        directory: PathBuf,
        #[arg(long)]
        models: PathBuf,
    },
    /// Print the JSON Schema for one organisation document.
    OrgSchema,
    /// Print the JSON Schema for one model document.
    Schema,
}

fn main() -> Result<()> {
    match Args::parse().command {
        Command::Validate { directory } => {
            Catalog::load(&directory)?;
            println!("model provisioning catalog is valid: {}", directory.display());
        }
        Command::ValidateOrgOverlays { directory, models } => {
            ensure!(directory.is_dir(), "org overlay directory does not exist: {}", directory.display());
            ensure!(models.is_dir(), "model directory does not exist: {}", models.display());
            let overlays = OrgCatalog::load(&directory)?;
            overlays.validate_models(&Catalog::load(&models)?)?;
            println!("org overlay catalog is valid: {}", directory.display());
        }
        Command::OrgSchema => println!("{}", OrgCatalog::json_schema()?),
        Command::Schema => println!("{}", Catalog::json_schema()?),
    }
    Ok(())
}
