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
use opentelemetry_sdk::trace::Tracer;
use std::collections::HashMap;
use tracing::{Subscriber, info};
use tracing_opentelemetry::OpenTelemetrySpanExt;
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

use crate::inference::rejections::RejectionCode;

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
    use tracing::Instrument;

    #[derive(Clone, Debug, Default)]
    struct Exported(Arc<Mutex<Vec<SpanData>>>);

    impl SpanExporter for Exported {
        async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
            self.0.lock().unwrap().extend(batch);
            Ok(())
        }
    }

    /// A repository method as instrumented after DW-931: a debug span whose
    /// `err` emits an ERROR event (`error = %e`) when it fails.
    #[tracing::instrument(level = "debug", err)]
    async fn repository_method() -> Result<(), &'static str> {
        Err("connection reset")
    }

    /// With logs at `debug` and the exporter at `info`, a request span is
    /// exported without its debug events or debug child spans, the error
    /// event a failing `err`-instrumented debug method emits lands on the
    /// request span with its message, and none of the layer's per-span extras
    /// are attached.
    #[tokio::test]
    async fn exporter_filter_is_independent_of_the_log_filter() {
        let exported = Exported::default();
        let provider = SdkTracerProvider::builder().with_simple_exporter(exported.clone()).build();
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new("debug"))
            .with(otel_layer(provider.tracer("test"), "info"));

        let _guard = tracing::subscriber::set_default(subscriber);
        async {
            tracing::debug!(db.statement = "SELECT 1", "query");
            tracing::info!("finished");
            let _ = repository_method().await;
        }
        .instrument(tracing::info_span!("request"))
        .await;
        drop(_guard);
        provider.shutdown().unwrap();

        let spans = exported.0.lock().unwrap();
        let names: Vec<&str> = spans.iter().map(|span| span.name.as_ref()).collect();
        assert_eq!(names, ["request"]);

        let request = &spans[0];
        // The layer maps an event carrying `error = %e` to the OTel exception
        // convention: named `exception`, with the message in `exception.message`.
        let events: Vec<(&str, Option<String>)> = request
            .events
            .iter()
            .map(|event| {
                let message = event
                    .attributes
                    .iter()
                    .find(|kv| kv.key.as_str() == "exception.message")
                    .map(|kv| kv.value.to_string());
                (event.name.as_ref(), message)
            })
            .collect();
        assert_eq!(
            events,
            [("finished", None), ("exception", Some("connection reset".to_string()))],
            "the repository method's error event must land on the request span: {:?}",
            request.events
        );

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
