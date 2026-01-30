//! Telemetry initialization module for OpenTelemetry-compatible tracing (+normal rust tracing, fmt
//! subscriber, etc.)
//!
//! This module provides functionality to initialize OpenTelemetry tracing with OTLP exporters.
//! OTLP export is **disabled by default** and must be explicitly enabled via the `enable_otel_export`
//! configuration flag.
//!
//! When enabled, configuration is done via standard OpenTelemetry environment variables:
//!
//! - `OTEL_EXPORTER_OTLP_ENDPOINT` - The OTLP endpoint URL
//! - `OTEL_EXPORTER_OTLP_PROTOCOL` - Protocol (grpc, http/protobuf, http/json)
//! - `OTEL_EXPORTER_OTLP_HEADERS` - Headers as comma-separated key=value pairs. The values can have their spaces encoded URL style - i.e. replace %20 with space.
//! - `OTEL_SERVICE_NAME` - Service name for resource identification
//!
//! Example - to enable OTLP export and send traces to a custom OTLP HTTP endpoint with basic authorization header:
//!
//! In config.yaml:
//! ```yaml
//! enable_otel_export: true
//! ```
//!
//! Environment variables:
//! ```bash
//! export OTEL_SERVICE_NAME="dwctl"
//! export OTEL_EXPORTER_OTLP_PROTOCOL="http/protobuf"
//! export OTEL_EXPORTER_OTLP_ENDPOINT="https://otlp-gateway.example.com/otlp"
//! export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Basic%20<token>"
//! ```
//!
//! ## OpenTelemetry SDK 0.29 Changes
//!
//! This module uses opentelemetry_sdk 0.29+ which has several API changes from earlier versions:
//!
//! - **TracerProvider renamed**: `TracerProvider` is now `SdkTracerProvider` to distinguish the SDK
//!   implementation from the trait.
//!
//! - **Resource builder pattern**: `Resource::new(vec![...])` replaced with
//!   `Resource::builder().with_attribute(...).build()` for clearer construction.
//!
//! - **Batch exporter simplified**: `with_batch_exporter(exporter, runtime)` no longer requires
//!   the runtime parameter - the SDK handles async internally.
//!
//! - **Shutdown handling**: `opentelemetry::global::shutdown_tracer_provider()` was removed in 0.28+.
//!   We now store the provider in a `OnceLock` and call `.shutdown()` directly. This is required
//!   because `tracing-opentelemetry` clones the tracer (not the provider), so we must keep our own
//!   reference to ensure proper shutdown and span flushing. See opentelemetry-rust#1961.

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _; // Trait for .tracer() method
use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::trace::SdkTracerProvider; // Renamed from TracerProvider in 0.29
use std::collections::HashMap;
use std::sync::OnceLock;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Global tracer provider reference for shutdown.
///
/// Required since opentelemetry 0.28+ removed `global::shutdown_tracer_provider()`.
/// We store our own reference to call `.shutdown()` directly, ensuring all pending
/// spans are flushed before application exit.
static TRACER_PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

/// Initialize tracing with optional OpenTelemetry support
///
/// This function sets up tracing-subscriber with:
/// - Console output (fmt layer)
/// - OpenTelemetry OTLP export (only if `enable_otel_export` is true and configured via environment variables)
///
/// Parameters:
/// - `enable_otel_export`: If true, attempts to configure OTLP export using environment variables
pub fn init_telemetry(enable_otel_export: bool) -> anyhow::Result<()> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    if enable_otel_export {
        // Try to create OTLP tracer - if env vars aren't set, this will fail gracefully
        match create_otlp_tracer() {
            Ok(tracer) => {
                // Build subscriber with both fmt and OpenTelemetry layers
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(tracing_subscriber::fmt::layer())
                    .with(tracing_opentelemetry::layer().with_tracer(tracer))
                    .try_init()?;

                info!("Telemetry initialized with OTLP export enabled");
            }
            Err(e) => {
                // If OTLP setup fails, just use fmt layer without OpenTelemetry
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(tracing_subscriber::fmt::layer())
                    .try_init()?;

                info!("Telemetry initialized without OTLP export: {}", e);
            }
        }
    } else {
        // OTLP export disabled - use only console logging
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer())
            .try_init()?;

        info!("Telemetry initialized (OTLP export disabled)");
    }

    Ok(())
}

/// Create an OpenTelemetry tracer with OTLP exporter
///
/// This respects standard OpenTelemetry environment variables for configuration.
/// The OTLP library will automatically read:
/// - OTEL_EXPORTER_OTLP_ENDPOINT
/// - OTEL_EXPORTER_OTLP_PROTOCOL
/// - OTEL_EXPORTER_OTLP_HEADERS
/// - OTEL_SERVICE_NAME
fn create_otlp_tracer() -> anyhow::Result<opentelemetry_sdk::trace::Tracer> {
    // Get service name from env or use default
    let service_name = std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "dwctl".to_string());

    // Get endpoint
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").unwrap_or_else(|_| "http://localhost:4318".to_string());

    eprintln!("[OTLP] Initializing OTLP tracer with the following configuration:");
    eprintln!("[OTLP] Service Name: {}", service_name);
    eprintln!("[OTLP] Endpoint: {}", endpoint);
    // Parse headers from environment variable
    let mut headers = HashMap::new();
    if let Ok(headers_str) = std::env::var("OTEL_EXPORTER_OTLP_HEADERS") {
        // Parse comma-separated key=value pairs
        // Handle URL encoding (%20 -> space). I'm not sure how necessary this is, but sometimes
        // headers have spaces in them, and environment variables and spaces don't mix that well.
        // I think the python OTEL impl supports this.
        let decoded = headers_str.replace("%20", " ");
        for pair in decoded.split(',') {
            if let Some((key, value)) = pair.split_once('=') {
                let key = key.trim().to_string();
                let value = value.trim().to_string();
                headers.insert(key, value);
            }
        }
        eprintln!("[OTLP] Custom headers, length: {}", headers.len());
    }

    // Determine protocol
    let protocol = match std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL").as_deref().unwrap_or("http/protobuf") {
        "http/protobuf" => Protocol::HttpBinary,
        "http/json" => Protocol::HttpJson,
        _ => Protocol::HttpBinary,
    };

    // Create OTLP exporter with explicit configuration
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(&endpoint)
        .with_protocol(protocol)
        .with_headers(headers)
        .build()?;

    // Create tracer provider with resource
    // - SdkTracerProvider::builder() is the 0.29+ API (was TracerProvider::builder())
    let tracer_provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_attribute(KeyValue::new("service.name", service_name.clone()))
                .build(),
        )
        .build();

    // Get a tracer from the provider - this is what tracing-opentelemetry uses
    let tracer = tracer_provider.tracer(service_name);

    // Store provider reference for shutdown. This is critical because:
    // 1. global::shutdown_tracer_provider() was removed in 0.28+
    // 2. tracing-opentelemetry clones the Tracer, not the Provider
    // 3. Without our own reference, we can't flush pending spans on shutdown
    let _ = TRACER_PROVIDER.set(tracer_provider);

    Ok(tracer)
}

/// Shutdown the global tracer provider gracefully
///
/// Should be called before application exit to flush any pending spans
pub fn shutdown_telemetry() {
    if let Some(provider) = TRACER_PROVIDER.get()
        && let Err(e) = provider.shutdown()
    {
        tracing::error!("Failed to shutdown tracer provider: {}", e);
    }
}
