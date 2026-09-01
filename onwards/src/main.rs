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
use tokio::{net::TcpListener, task::JoinSet};
use tracing::{error, info, instrument};
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

    let mut serves = JoinSet::new();
    // If we are running with metrics enabled, set up the metrics layer and router.
    let prometheus_layer = if config.metrics {
        let (prometheus_layer, prometheus_handle) =
            build_metrics_layer_and_handle(config.metrics_prefix.clone());

        let metrics_router = build_metrics_router(prometheus_handle);
        let bind_addr = format!("0.0.0.0:{}", config.metrics_port);
        let listener = TcpListener::bind(&bind_addr).await?;
        serves.spawn(axum::serve(listener, metrics_router).into_future());
        info!("Metrics endpoint enabled on {}", bind_addr);
        Some(prometheus_layer)
    } else {
        info!("Metrics endpoint disabled");
        None
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
    serves.spawn(axum::serve(listener, router).into_future());
    info!("AI Gateway listening on {}", bind_addr);

    // Wait for all servers to complete
    if let Some(result) = serves.join_next().await {
        result?.map_err(anyhow::Error::from)
    } else {
        error!("No server tasks were spawned");
        Err(anyhow::anyhow!("No server tasks were spawned"))
    }
}
