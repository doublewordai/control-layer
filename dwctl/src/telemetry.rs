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

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
pub use opentelemetry_sdk::trace::SdkTracerProvider;
use std::collections::HashMap;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Initialize tracing with optional OpenTelemetry support
///
/// This function sets up tracing-subscriber with:
/// - Console output (fmt layer)
/// - OpenTelemetry OTLP export (only if `enable_otel_export` is true and configured via environment variables)
///
/// Parameters:
/// - `enable_otel_export`: If true, attempts to configure OTLP export using environment variables
///
/// Returns the tracer provider if OTLP export was successfully enabled. The caller should
/// store this and call `provider.shutdown()` before application exit to flush pending spans.
pub fn init_telemetry(enable_otel_export: bool) -> anyhow::Result<Option<SdkTracerProvider>> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    if enable_otel_export {
        let (tracer, provider) = create_otlp_tracer()?;

        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().compact())
            .with(tracing_opentelemetry::layer().with_tracer(tracer))
            .try_init()?;

        info!("Telemetry initialized with OTLP export enabled");
        return Ok(Some(provider));
    } else {
        // OTLP export disabled - use only console logging
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().compact())
            .try_init()?;

        info!("Telemetry initialized (OTLP export disabled)");
    }

    Ok(None)
}

/// Create an OpenTelemetry tracer with OTLP exporter
///
/// This respects standard OpenTelemetry environment variables for configuration.
/// The OTLP library will automatically read:
/// - OTEL_EXPORTER_OTLP_ENDPOINT
/// - OTEL_EXPORTER_OTLP_PROTOCOL
/// - OTEL_EXPORTER_OTLP_HEADERS
/// - OTEL_SERVICE_NAME
///
/// Returns both the tracer and provider. The provider must be retained for shutdown.
fn create_otlp_tracer() -> anyhow::Result<(opentelemetry_sdk::trace::Tracer, SdkTracerProvider)> {
    // Get service name from env or use default
    let service_name = std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "dwctl".to_string());

    // Get endpoint — append /v1/traces since with_endpoint() treats it as a
    // signal-specific URL (doesn't auto-append like the SDK would for the base env var)
    let base = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").unwrap_or_else(|_| "http://localhost:4318".to_string());
    let endpoint = if base.ends_with("/v1/traces") {
        base
    } else {
        format!("{}/v1/traces", base.trim_end_matches('/'))
    };

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otlp_tracer_builds_with_http_client() {
        let (_, provider) = create_otlp_tracer().expect(
            "OTLP tracer failed to build - likely a feature flag conflict \
             (reqwest-client vs reqwest-blocking-client)",
        );
        provider.shutdown().ok();
    }
}
