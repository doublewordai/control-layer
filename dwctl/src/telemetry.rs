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
//! # What gets exported
//!
//! Logs and traces are filtered separately. `RUST_LOG` decides what reaches the
//! log output, as usual. [`OTEL_TRACES_FILTER`] (our own variable, not an
//! OpenTelemetry standard one) takes the same directive syntax and decides which
//! spans and events reach the trace backend; it defaults to
//! [`DEFAULT_OTEL_TRACES_FILTER`]. `RUST_LOG` still applies first, so the
//! exporter never sees more than the logs do, but a `debug` log level no longer
//! exports every repository span and every debug line (including sqlx's
//! per-query statement events) as part of the trace.
//!
//! Spans are exported without the layer's default per-span extras
//! (`code.*` source location, `thread.*`, `busy_ns`/`idle_ns`); nothing we
//! query uses them and they were a sizeable share of each span's bytes.
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
use opentelemetry_sdk::trace::Tracer;
use std::collections::HashMap;
use tracing::{Subscriber, info};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Environment variable holding the filter directives for the span exporter.
/// Same syntax as `RUST_LOG`, applied to the trace backend only.
pub const OTEL_TRACES_FILTER: &str = "OTEL_TRACES_FILTER";

/// Exporter filter when [`OTEL_TRACES_FILTER`] is unset: request-level spans
/// and events, no repository spans or debug lines.
pub const DEFAULT_OTEL_TRACES_FILTER: &str = "info";

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
    let env_filter = EnvFilter::new(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()));

    if enable_otel_export {
        let (tracer, provider) = create_otlp_tracer()?;
        let traces_filter = std::env::var(OTEL_TRACES_FILTER).unwrap_or_else(|_| DEFAULT_OTEL_TRACES_FILTER.to_string());

        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().compact())
            .with(otel_layer(tracer, &traces_filter))
            .try_init()?;

        info!(traces_filter, "Telemetry initialized with OTLP export enabled");
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

/// The span export layer: `tracer` behind its own `directives` filter, without
/// the per-span source location, thread and timing attributes.
///
/// A span the filter excludes is invisible to this layer only; the fmt layer
/// still logs it, and an event inside it (such as the error an `err`
/// instrumented repository method records) is exported on the nearest
/// exported ancestor span instead.
fn otel_layer<S>(tracer: Tracer, directives: &str) -> impl Layer<S>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_location(false)
        .with_threads(false)
        .with_tracked_inactivity(false)
        .with_filter(EnvFilter::new(directives))
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
fn create_otlp_tracer() -> anyhow::Result<(Tracer, SdkTracerProvider)> {
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
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::trace::{SpanData, SpanExporter};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Debug, Default)]
    struct Exported(Arc<Mutex<Vec<SpanData>>>);

    impl SpanExporter for Exported {
        async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
            self.0.lock().unwrap().extend(batch);
            Ok(())
        }
    }

    /// With logs at `debug` and the exporter at `info`, a request span is
    /// exported without its debug events or debug child spans, the error
    /// recorded inside a debug child lands on the request span, and none of
    /// the layer's per-span extras are attached.
    #[test]
    fn exporter_filter_is_independent_of_the_log_filter() {
        let exported = Exported::default();
        let provider = SdkTracerProvider::builder().with_simple_exporter(exported.clone()).build();
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new("debug"))
            .with(otel_layer(provider.tracer("test"), "info"));

        tracing::subscriber::with_default(subscriber, || {
            let request = tracing::info_span!("request");
            let _request = request.enter();
            tracing::debug!(db.statement = "SELECT 1", "query");
            tracing::info!("finished");
            let repository = tracing::debug_span!("repository");
            let _repository = repository.enter();
            tracing::error!("failed");
        });
        provider.shutdown().unwrap();

        let spans = exported.0.lock().unwrap();
        let names: Vec<&str> = spans.iter().map(|span| span.name.as_ref()).collect();
        assert_eq!(names, ["request"]);

        let request = &spans[0];
        let events: Vec<&str> = request.events.iter().map(|event| event.name.as_ref()).collect();
        assert_eq!(events, ["finished", "failed"]);

        let keys: Vec<&str> = request.attributes.iter().map(|kv| kv.key.as_str()).collect();
        for extra in [
            "code.file.path",
            "code.line.number",
            "code.module.name",
            "thread.id",
            "thread.name",
            "busy_ns",
            "idle_ns",
        ] {
            assert!(!keys.contains(&extra), "{extra} exported: {keys:?}");
        }
    }

    #[test]
    fn otlp_tracer_builds_with_http_client() {
        let (_, provider) = create_otlp_tracer().expect(
            "OTLP tracer failed to build - likely a feature flag conflict \
             (reqwest-client vs reqwest-blocking-client)",
        );
        provider.shutdown().ok();
    }

    /// Verify the reqwest client used by opentelemetry-otlp can make HTTPS requests.
    /// This catches TLS misconfigurations (e.g. reqwest compiled without a TLS backend)
    /// which only manifest at runtime when connecting to an HTTPS endpoint.
    #[tokio::test]
    async fn otlp_reqwest_supports_https() {
        let client = reqwest::Client::new();
        let result = client.get("https://example.com").send().await;
        assert!(
            result.is_ok(),
            "reqwest cannot make HTTPS requests — TLS not configured: {}",
            result.unwrap_err()
        );
    }
}
