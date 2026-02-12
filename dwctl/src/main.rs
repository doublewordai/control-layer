use clap::Parser;
use dwctl::{Application, Config, telemetry};

/// Wait for shutdown signal (SIGTERM or Ctrl+C)
async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c().await.expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            tracing::info!("Received Ctrl+C, shutting down gracefully...");
        },
        _ = terminate => {
            tracing::info!("Received SIGTERM, shutting down gracefully...");
        },
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Install rustls crypto provider before anything else that might build a TLS client
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Parse CLI args
    let args = dwctl::config::Args::parse();

    // Load configuration
    let config = Config::load(&args)?;

    // If --validate flag is set, exit successfully after config validation
    if args.validate {
        println!("Configuration is valid.");
        return Ok(());
    }

    // Initialize telemetry (tracing + optional OpenTelemetry)
    let tracer_provider = telemetry::init_telemetry(config.enable_otel_export)?;

    tracing::debug!("{:?}", args);

    // Run the application with graceful shutdown on SIGTERM/Ctrl+C
    let shutdown = shutdown_signal();
    Application::new(config, tracer_provider).await?.serve(shutdown).await
}
