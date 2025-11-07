use clap::Parser;
use dwctl::{telemetry, Config};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse CLI args
    let args = dwctl::config::Args::parse();

    // Load configuration
    let config = Config::load(&args)?;

    // Initialize telemetry (tracing + optional OpenTelemetry)
    telemetry::init_telemetry(config.enable_otel_export)?;

    tracing::debug!("{:?}", args);

    // Run the application
    dwctl::run(config).await
}
