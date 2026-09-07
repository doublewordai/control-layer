use axum::{Router, routing::get};
use clap::Parser as _;
use onwards::{
    AppState, build_metrics_layer_and_handle, build_metrics_router, build_router, client,
    config::Config,
    create_openai_sanitizer,
    strict::build_strict_router,
    target::{Targets, WatchedFile},
    telemetry,
};
use tokio::net::TcpListener;
use tracing::{info, instrument};

mod server;
#[tokio::main]
#[instrument]
pub async fn main() -> anyhow::Result<()> {
    // Initialize tracing (with optional OTLP export if OTEL_EXPORTER_OTLP_ENDPOINT is set)
    let tracer_provider = telemetry::init_telemetry()?;

    let result = run().await;

    // Flush pending spans before exit
    if let Some(provider) = tracer_provider {
        if let Err(e) = provider.shutdown() {
            eprintln!("Failed to shutdown tracer provider: {e}");
        }
    }

    result
}

async fn run() -> anyhow::Result<()> {
    let config = Config::parse().validate()?;
    info!("Starting AI Gateway with config: {:?}", config);

    let targets = Targets::from_config_file(&config.targets)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create targets from config: {}", e))?;

    // Check if strict mode is enabled
    let strict_mode = targets.strict_mode;

    // Start file watcher if a config file was specified
    if config.watch {
        targets
            .receive_updates(WatchedFile(config.targets.clone()))
            .await?;
    }

    // Install OS signal handlers before opening either listener.
    let shutdown_signal = server::shutdown_signal()?;
    // If we are running with metrics enabled, set up the metrics layer and router.
    let (prometheus_layer, metrics_router) = if config.metrics {
        let (prometheus_layer, prometheus_handle) =
            build_metrics_layer_and_handle(config.metrics_prefix.clone());

        let metrics_router = build_metrics_router(prometheus_handle);
        (Some(prometheus_layer), metrics_router)
    } else {
        info!("Metrics endpoint disabled");
        (None, Router::new())
    };

    // Register the sanitizer globally - per-target sanitize_response flag controls when it's applied
    let app_state = AppState::new(targets).with_response_transform(create_openai_sanitizer());

    // Use strict router if strict_mode is enabled, otherwise use standard router
    let mut router = if strict_mode {
        use onwards::strict::handlers::models_handler;
        info!("Strict mode enabled - using typed request validation");
        Router::new()
            // Preserve /models alias at root for backwards compatibility
            .route("/models", get(models_handler::<client::HyperClient>))
            .with_state(app_state.clone())
            .nest("/v1", build_strict_router(app_state))
    } else {
        build_router(app_state)
    };
    // If we have a metrics layer, add it to the router.
    if let Some(prometheus_layer) = prometheus_layer {
        router = router.layer(prometheus_layer)
    };
    let bind_addr = format!("0.0.0.0:{}", config.port);
    let listener = TcpListener::bind(&bind_addr).await?;
    let metrics_addr = format!("0.0.0.0:{}", config.metrics_port);
    let metrics_listener = TcpListener::bind(&metrics_addr).await?;
    info!("AI Gateway listening on {}", bind_addr);
    info!("Metrics and health endpoint listening on {}", metrics_addr);

    server::serve(
        listener,
        router,
        metrics_listener,
        metrics_router,
        &config,
        shutdown_signal,
    )
    .await
}
