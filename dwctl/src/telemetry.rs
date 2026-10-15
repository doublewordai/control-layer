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
//! The span batch processor honours the standard `OTEL_BSP_*` variables
//! (`OTEL_BSP_MAX_QUEUE_SIZE`, `OTEL_BSP_MAX_EXPORT_BATCH_SIZE`,
//! `OTEL_BSP_SCHEDULE_DELAY`, `OTEL_BSP_EXPORT_TIMEOUT`). Spans that arrive while
//! the queue is full are dropped, so the queue must absorb bursts (a large batch
//! dispatching tens of thousands of requests at once). The SDK default of 2048
//! dropped most spans in such bursts; this module defaults to a queue of
//! [`DEFAULT_BSP_MAX_QUEUE_SIZE`] spans and exports in batches of
//! [`DEFAULT_BSP_MAX_EXPORT_BATCH_SIZE`]. Memory is the trade-off: a full queue
//! holds up to that many finished spans (roughly 1-2 KiB each, so about 16-32 MiB
//! at the default) per process. Raise the queue for burstier traffic or lower it on
//! small replicas.
//!
//! # Span attributes for customer-facing alerts
//!
//! Alert and dashboard queries (for example TraceQL metrics) depend on these
//! attribute names, so treat them as a contract. `doubleword.*` are our domain
//! attributes; `gen_ai.*`, `http.*` and `error.type` follow OpenTelemetry
//! semantic conventions.
//!
//! Root `request` span, for authenticated AI requests (set when the response is
//! produced; see [`record_ai_response_attributes`]):
//!
//! | attribute | meaning |
//! |---|---|
//! | `doubleword.account_id` | owning account (the organization for organization keys) |
//! | `doubleword.account_name` | account display name, falling back to its username; at most 128 characters |
//! | `doubleword.api_key_id` | the API key used |
//! | `doubleword.key_purpose` | the key's purpose (`realtime`, `batch`, ...) |
//! | `gen_ai.request.model` | the model alias requested |
//! | `doubleword.traffic` | `realtime`, or `batch` for requests dispatched by the batch daemon |
//! | `doubleword.batch.completion_window` | the dispatching batch's completion window (batch traffic only) |
//! | `doubleword.error.source` | for 4xx/5xx that passed through onwards: `gateway` (decided here: auth, limits, validation) or `provider` (the status is the upstream's) |
//! | `doubleword.rejection.code` | for gateway errors: the error code, e.g. `unsupported_parameter`, `inflight_limit_exceeded` |
//! | `http.response.status_code`, `error.type` | final status, and the status as the error type for 4xx/5xx |
//!
//! `onwards.request` span: `onwards.account`, `onwards.account_name`,
//! `onwards.api_key_id`, `onwards.key_purpose`, `gen_ai.request.model`,
//! `http.response.status_code`, plus `error.type` and `onwards.rejection.param`
//! for gateway rejections.
//!
//! `fusillade.process_request` span (one per batch request attempt):
//! `doubleword.account_id`, `completion_window`, `custom_id`, `model`, `outcome`.
//!
//! `fusillade.batch_finalized` span (one per batch, emitted by the notification
//! poller when the batch is claimed after finishing): `batch_id`,
//! `doubleword.account_id`, `doubleword.account_name`, `gen_ai.request.model`
//! (every model, joined with `, `, for display), `batch.models` (the same
//! models as a string array, for filtering),
//! `completion_window`, `batch.total_requests`, `batch.completed_requests`,
//! `batch.failed_requests`, `batch.canceled_requests`, `batch.outcome`
//! (`completed` | `partially_completed` | `failed`, with cancelled requests
//! counted against the batch), `batch.finalized_at` and, for failed batches,
//! `batch.sample_error` (an error class, never a message).
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

use axum::{extract::Request, middleware::Next, response::Response};
use onwards::{ErrorOrigin, RequestIdentity, ServedBy};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
pub use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::trace::{BatchConfig, BatchConfigBuilder, BatchSpanProcessor};
use std::collections::HashMap;
use tracing::info;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::inference::rejections::RejectionCode;

/// Default span queue size when `OTEL_BSP_MAX_QUEUE_SIZE` is unset. The SDK
/// default (2048) dropped most spans in a large batch's dispatch burst.
pub const DEFAULT_BSP_MAX_QUEUE_SIZE: usize = 16_384;
/// Default export batch size when `OTEL_BSP_MAX_EXPORT_BATCH_SIZE` is unset.
pub const DEFAULT_BSP_MAX_EXPORT_BATCH_SIZE: usize = 1_024;

/// Batch processor settings: the SDK's `OTEL_BSP_*` environment handling, with
/// larger defaults for the queue and export batch size.
fn batch_config() -> BatchConfig {
    let mut builder = BatchConfigBuilder::default();
    if std::env::var_os("OTEL_BSP_MAX_QUEUE_SIZE").is_none() {
        builder = builder.with_max_queue_size(DEFAULT_BSP_MAX_QUEUE_SIZE);
    }
    if std::env::var_os("OTEL_BSP_MAX_EXPORT_BATCH_SIZE").is_none() {
        builder = builder.with_max_export_batch_size(DEFAULT_BSP_MAX_EXPORT_BATCH_SIZE);
    }
    builder.build()
}

/// Longest account name recorded on a span. Display names are free text, and
/// the name rides on every authenticated request's spans.
const MAX_SPAN_ACCOUNT_NAME_CHARS: usize = 128;

/// The name an account goes by on spans: its display name, or its username
/// when it has none or it is blank, cut to [`MAX_SPAN_ACCOUNT_NAME_CHARS`].
pub fn span_account_name(display_name: Option<&str>, username: &str) -> String {
    display_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(username)
        .chars()
        .take(MAX_SPAN_ACCOUNT_NAME_CHARS)
        .collect()
}

/// The completion window the batch daemon stamps on a request it dispatches,
/// carried onto the response so the root span can record it once the key
/// behind the request says it is batch traffic.
#[derive(Clone, Debug)]
pub struct BatchCompletionWindow(String);

/// Copies `x-fusillade-batch-completion-window` onto the response as a
/// [`BatchCompletionWindow`].
pub async fn carry_batch_completion_window(request: Request, next: Next) -> Response {
    let window = request
        .headers()
        .get("x-fusillade-batch-completion-window")
        .and_then(|value| value.to_str().ok())
        .map(|window| BatchCompletionWindow(window.to_string()));
    let mut response = next.run(request).await;
    if let Some(window) = window {
        response.extensions_mut().insert(window);
    }
    response
}

/// Records who made an AI request, and where an error came from, on the root
/// request span. Reads the extensions onwards attaches to its responses, so it
/// covers successes, gateway refusals and provider errors alike, and the code
/// and caller a dwctl layer refused a request with. A request whose key was not
/// recognised (401) has no identity and records none, and an error neither
/// onwards nor a dwctl layer decided on has no source.
pub fn record_ai_response_attributes<B>(response: &axum::http::Response<B>, span: &tracing::Span, status: u16) {
    let extensions = response.extensions();
    if let Some(identity) = extensions.get::<RequestIdentity>() {
        // Batch keys are hidden and held only by the batch daemon, so the key,
        // not a header any caller can send, says whether this is batch traffic.
        let batch = identity.key_purpose.as_deref() == Some("batch");
        span.set_attribute("doubleword.traffic", if batch { "batch" } else { "realtime" });
        if batch && let Some(BatchCompletionWindow(window)) = extensions.get() {
            span.set_attribute("doubleword.batch.completion_window", window.clone());
        }
        let attrs = [
            ("doubleword.account_id", identity.account_id.clone()),
            ("doubleword.account_name", identity.account_name.clone()),
            ("doubleword.api_key_id", identity.api_key_id.map(|id| id.to_string())),
            ("doubleword.key_purpose", identity.key_purpose.clone()),
            ("gen_ai.request.model", identity.model.clone()),
        ];
        for (key, value) in attrs {
            if let Some(value) = value {
                span.set_attribute(key, value);
            }
        }
    }
    if status >= 400 {
        let (source, code) = match (extensions.get::<ErrorOrigin>(), extensions.get::<RejectionCode>()) {
            (Some(ErrorOrigin::Provider), _) => ("provider", None),
            (Some(ErrorOrigin::Gateway { code }), _) => ("gateway", Some(code.clone())),
            // Refused by a dwctl layer, before or instead of onwards.
            (None, Some(rejection)) => ("gateway", Some(rejection.code().to_string())),
            // An upstream's response passed through unchanged.
            (None, None) if extensions.get::<ServedBy>().is_some() => ("provider", None),
            // Not produced by onwards or a dwctl layer (admin API, or refused
            // before either ran).
            (None, None) => return,
        };
        span.set_attribute("doubleword.error.source", source);
        if let Some(code) = code.filter(|code| !code.is_empty()) {
            span.set_attribute("doubleword.rejection.code", code);
        }
    }
}

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

    let batch_processor = BatchSpanProcessor::builder(exporter).with_batch_config(batch_config()).build();
    let tracer_provider = SdkTracerProvider::builder()
        .with_span_processor(batch_processor)
        .with_resource(resource)
        .build();

    let tracer = tracer_provider.tracer(service_name);

    Ok((tracer, tracer_provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_account_name_falls_back_and_is_bounded() {
        assert_eq!(span_account_name(Some(" Example Co "), "user"), "Example Co");
        assert_eq!(span_account_name(Some("   "), "user"), "user");
        assert_eq!(span_account_name(None, "user"), "user");
        let long = "é".repeat(MAX_SPAN_ACCOUNT_NAME_CHARS + 10);
        assert_eq!(span_account_name(Some(&long), "user").chars().count(), MAX_SPAN_ACCOUNT_NAME_CHARS);
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
