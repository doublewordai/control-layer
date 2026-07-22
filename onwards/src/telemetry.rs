//! Optional OpenTelemetry tracing initialization for onwards.
//!
//! OTel export is enabled when `OTEL_EXPORTER_OTLP_ENDPOINT` is set.
//! This mirrors dwctl's approach but uses env-var presence detection
//! instead of a config flag (since onwards uses CLI args, not a config file).
//!
//! Standard OpenTelemetry env vars:
//! - `OTEL_EXPORTER_OTLP_ENDPOINT` — OTLP endpoint (e.g. `http://localhost:4318`)
//! - `OTEL_EXPORTER_OTLP_PROTOCOL` — Protocol: `http/protobuf` (default), `http/json`
//! - `OTEL_EXPORTER_OTLP_HEADERS` — Comma-separated `key=value` pairs (supports `%20` for spaces)
//! - `OTEL_SERVICE_NAME` — Service name (default: `onwards`)

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
pub use opentelemetry_sdk::trace::SdkTracerProvider;
use std::collections::HashMap;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Initialize tracing with optional OpenTelemetry OTLP export.
///
/// If `OTEL_EXPORTER_OTLP_ENDPOINT` is set, traces are exported via OTLP HTTP.
/// Otherwise, only console (fmt) logging is used.
///
/// Returns the tracer provider if OTLP export was enabled. The caller must
/// hold this and call `provider.shutdown()` before exit to flush pending spans.
pub fn init_telemetry() -> anyhow::Result<Option<SdkTracerProvider>> {
    let env_filter =
        EnvFilter::new(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()));

    if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        let (tracer, provider) = create_otlp_tracer()?;

        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().compact())
            .with(tracing_opentelemetry::layer().with_tracer(tracer))
            .try_init()?;

        info!("Telemetry initialized with OTLP export enabled");
        Ok(Some(provider))
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().compact())
            .try_init()?;

        info!(
            "Telemetry initialized (OTLP export disabled — set OTEL_EXPORTER_OTLP_ENDPOINT to enable)"
        );
        Ok(None)
    }
}

fn create_otlp_tracer() -> anyhow::Result<(opentelemetry_sdk::trace::Tracer, SdkTracerProvider)> {
    let service_name = std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "onwards".to_string());

    let base = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:4318".to_string());
    let endpoint = if base.ends_with("/v1/traces") {
        base
    } else {
        format!("{}/v1/traces", base.trim_end_matches('/'))
    };

    let mut headers = HashMap::new();
    if let Ok(headers_str) = std::env::var("OTEL_EXPORTER_OTLP_HEADERS") {
        let decoded = headers_str.replace("%20", " ");
        for pair in decoded.split(',') {
            if let Some((key, value)) = pair.split_once('=') {
                headers.insert(key.trim().to_string(), value.trim().to_string());
            }
        }
    }

    let protocol = match std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL")
        .as_deref()
        .unwrap_or("http/protobuf")
    {
        "http/protobuf" => Protocol::HttpBinary,
        "http/json" => Protocol::HttpJson,
        _ => Protocol::HttpBinary,
    };

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(&endpoint)
        .with_protocol(protocol)
        .with_headers(headers)
        .build()?;

    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name(service_name.clone())
        .build();

    let tracer_provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let tracer = tracer_provider.tracer(service_name);

    Ok((tracer, tracer_provider))
}
