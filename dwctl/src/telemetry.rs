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
//! # Log format
//!
//! `log_format` in the config (`DWCTL_LOG_FORMAT`) selects `compact` lines,
//! the default, or `json`: one object per event, for log aggregators. Alert
//! queries that read log lines depend on the JSON field names described on
//! [`fmt_layer`], and in JSON mode panics are written in the same shape.
//!
//! # Credentials
//!
//! Log lines, JSON panic lines and exported spans pass through
//! [`crate::log_redaction`], which replaces API keys and bearer tokens with
//! `<redacted>` however they got into the output.
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
//! Span kinds are set with an `otel.kind` *field* on the span (the layer
//! ignores a raw `otel.kind` attribute and exports the span as `internal`):
//! the root `request` span is a `server` span and `fusillade.batch_finalized`
//! a `producer` span, so backends that derive RED metrics from server spans
//! count every request.
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
//! `http.response.status_code`, plus `error.type`, `onwards.rejection.param`
//! and, for a body that doesn't match its schema, `onwards.rejection.reason`
//! for gateway rejections. dwctl's own layers record the same attributes on
//! the span current when they refuse.
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
//! counted against the batch), `batch.finalized_at`, `batch.size_class`
//! (`small` under 10 requests, else `large`), and, for batches with a deadline,
//! `batch.deadline` (`met` | `missed`, comparing the finish time with
//! `expires_at`) and `batch.late_seconds` (whole seconds past the deadline, 0
//! when on time). Failed batches also carry `batch.sample_error` (an error
//! class, never a message).
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
use chrono::{SecondsFormat, Utc};
use onwards::{ErrorOrigin, RequestIdentity, ServedBy};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
pub use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::trace::{SpanExporter, Tracer};
use serde_json::{Map, Value, json};
use std::backtrace::{Backtrace, BacktraceStatus};
use std::collections::HashMap;
use std::io::Write;
use std::panic::PanicHookInfo;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber, info};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::fmt::format::{Format, Json, JsonFields, Writer};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, FormattedFields, MakeWriter};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::{LookupSpan, Registry, SpanRef};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::log_redaction::{RedactingMakeWriter, RedactingSpanExporter, redact_secrets};

/// Environment variable holding the filter directives for the span exporter.
/// Same syntax as `RUST_LOG`, applied to the trace backend only.
pub const OTEL_TRACES_FILTER: &str = "OTEL_TRACES_FILTER";

/// Exporter filter when [`OTEL_TRACES_FILTER`] is unset: request-level spans
/// and events, no repository spans or debug lines.
pub const DEFAULT_OTEL_TRACES_FILTER: &str = "info";

use crate::config::LogFormat;
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
/// - Log output on stdout, in `log_format` (see [`fmt_layer`])
/// - OpenTelemetry OTLP export (only if `enable_otel_export` is true and configured via environment variables)
///
/// With [`LogFormat::Json`] it also replaces the panic hook, so a panic is
/// written as a JSON error line rather than as plain text (see
/// [`write_panic_as_json`]).
///
/// Parameters:
/// - `enable_otel_export`: If true, attempts to configure OTLP export using environment variables
/// - `log_format`: How log events are written
///
/// Returns the tracer provider if OTLP export was successfully enabled. The caller should
/// store this and call `provider.shutdown()` before application exit to flush pending spans.
///
/// If OTLP export fails to set up, logging is still installed (without
/// export) before the error is returned, so the caller can log the error in
/// `log_format`.
pub fn init_telemetry(enable_otel_export: bool, log_format: LogFormat) -> anyhow::Result<Option<SdkTracerProvider>> {
    let env_filter = EnvFilter::new(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()));
    let traces_filter = std::env::var(OTEL_TRACES_FILTER).unwrap_or_else(|_| DEFAULT_OTEL_TRACES_FILTER.to_string());
    let (otel, provider, otlp_settings, otlp_error) = match enable_otel_export.then(create_otlp_tracer) {
        Some(Ok((tracer, provider, settings))) => (Some(otel_layer(tracer, &traces_filter)), Some(provider), Some(settings), None),
        Some(Err(error)) => (None, None, None, Some(error)),
        None => (None, None, None, None),
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer(log_format, std::io::stdout))
        .with(otel)
        .try_init()?;

    if log_format == LogFormat::Json {
        std::panic::set_hook(Box::new(write_panic_as_json));
    }

    if let Some(error) = otlp_error {
        return Err(error.context("Failed to set up OTLP export"));
    }
    match otlp_settings {
        Some(settings) => info!(
            service_name = settings.service_name,
            endpoint = settings.endpoint,
            custom_headers = settings.header_count,
            traces_filter,
            ?log_format,
            "Telemetry initialized with OTLP export enabled"
        ),
        None => info!(?log_format, "Telemetry initialized (OTLP export disabled)"),
    }

    Ok(provider)
}

/// The log output layer, writing to `writer` in `format`.
///
/// JSON events carry their fields at the top level (`message`, `error`,
/// `request_model`, ...) beside `timestamp`, `level` and `target`, the
/// innermost span under `span` and the full span list, root first, under
/// `spans`. Loki derives `detected_level` from `level`. The
/// [`CORRELATION_FIELDS`] of the spans an event is in are repeated at the top
/// level, so `| json | x_request_id="<id>"` finds every line of a request.
pub(crate) fn fmt_layer<S, W>(format: LogFormat, writer: W) -> Box<dyn Layer<S> + Send + Sync>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    let layer = tracing_subscriber::fmt::layer().with_writer(RedactingMakeWriter(writer));
    match format {
        LogFormat::Compact => layer.compact().boxed(),
        LogFormat::Json => CorrelationLayer
            .and_then(
                layer
                    .json()
                    .event_format(JsonWithCorrelation(tracing_subscriber::fmt::format().json().flatten_event(true))),
            )
            .boxed(),
    }
}

/// Span fields that identify a request across services. In JSON output each
/// is repeated at the top level of every event inside a span that recorded
/// it, because Loki's `json` parser does not look inside the `spans` array.
/// `x_request_id` is the key other services in the request path log the same
/// ID under, so one key:value query matches all of them.
const CORRELATION_FIELDS: [&str; 4] = ["x_request_id", "edge_request_id", "provider_request_id", "trace_id"];

/// The [`CORRELATION_FIELDS`] a span has recorded, kept in its extensions by
/// [`CorrelationLayer`].
#[derive(Default)]
struct CorrelationIds(Vec<(&'static str, String)>);

impl Visit for CorrelationIds {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.set(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // A `display` value's Debug is its Display, so IDs recorded with
        // `display` (as the request span's are) are not quoted.
        self.set(field, format!("{value:?}"));
    }
}

impl CorrelationIds {
    fn set(&mut self, field: &Field, value: String) {
        let Some(name) = CORRELATION_FIELDS.into_iter().find(|name| *name == field.name()) else {
            return;
        };
        match self.0.iter_mut().find(|(existing, _)| *existing == name) {
            Some((_, existing)) => *existing = value,
            None => self.0.push((name, value)),
        }
    }
}

/// Records each span's [`CORRELATION_FIELDS`], at creation and when they are
/// recorded later, for [`JsonWithCorrelation`] and the panic hook to read.
struct CorrelationLayer;

impl<S> Layer<S> for CorrelationLayer
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut ids = CorrelationIds::default();
        attrs.record(&mut ids);
        if !ids.0.is_empty()
            && let Some(span) = ctx.span(id)
        {
            span.extensions_mut().insert(ids);
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut extensions = span.extensions_mut();
        match extensions.get_mut::<CorrelationIds>() {
            Some(ids) => values.record(ids),
            None => {
                let mut ids = CorrelationIds::default();
                values.record(&mut ids);
                if !ids.0.is_empty() {
                    extensions.insert(ids);
                }
            }
        }
    }
}

/// The correlation IDs of a span scope given leaf first: the innermost value
/// of each field wins, and fields `skip` names are left out. In
/// [`CORRELATION_FIELDS`] order, so lines read the same way every time.
fn correlation_ids<'a, R: LookupSpan<'a> + 'a>(
    leaf_first: impl Iterator<Item = SpanRef<'a, R>>,
    skip: impl Fn(&str) -> bool,
) -> Vec<(&'static str, String)> {
    let mut found: Vec<(&'static str, String)> = Vec::new();
    for span in leaf_first {
        if let Some(ids) = span.extensions().get::<CorrelationIds>() {
            for (name, value) in &ids.0 {
                if !skip(name) && !found.iter().any(|(existing, _)| existing == name) {
                    found.push((name, value.clone()));
                }
            }
        }
    }
    found.sort_by_key(|(name, _)| CORRELATION_FIELDS.iter().position(|field| field == name));
    found
}

/// The JSON event format, with the event's correlation IDs (see
/// [`CORRELATION_FIELDS`]) added at the top level. An event that sets one of
/// those fields itself keeps its own value.
struct JsonWithCorrelation(Format<Json>);

impl<S, N> FormatEvent<S, N> for JsonWithCorrelation
where
    S: Subscriber + for<'span> LookupSpan<'span>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn format_event(&self, ctx: &FmtContext<'_, S, N>, mut writer: Writer<'_>, event: &Event<'_>) -> std::fmt::Result {
        let fields = event.metadata().fields();
        let ids = ctx
            .event_scope()
            .map(|scope| correlation_ids(scope, |name| fields.field(name).is_some()))
            .unwrap_or_default();
        if ids.is_empty() {
            return self.0.format_event(ctx, writer, event);
        }
        let mut line = String::new();
        self.0.format_event(ctx, Writer::new(&mut line), event)?;
        // The JSON format writes one object and a newline; add the IDs as
        // the object's last members.
        let Some(close) = line.trim_end().strip_suffix('}').map(str::len) else {
            return writer.write_str(&line);
        };
        let mut members = String::new();
        for (name, value) in ids {
            members.push(',');
            members.push_str(&Value::from(name).to_string());
            members.push(':');
            members.push_str(&Value::from(value).to_string());
        }
        line.insert_str(close, &members);
        writer.write_str(&line)
    }
}

/// Panic hook for JSON logging: writes the panic to stderr as one JSON error
/// line, in the shape [`fmt_layer`] gives events.
///
/// It writes the line itself rather than emitting a tracing event, because an
/// event can be lost exactly when it matters: `RUST_LOG` directives that don't
/// cover the `panic` target filter it out, and a panic raised while the
/// subscriber is handling an event is dispatched to no subscriber at all.
fn write_panic_as_json(info: &PanicHookInfo<'_>) {
    let line = panic_line(info, &Backtrace::capture(), current_spans());
    let _ = writeln!(std::io::stderr().lock(), "{}", redact_secrets(&line.to_string()));
}

/// The span the panicking thread is in and its scope, root first, shaped
/// like the `span` and `spans` of a JSON event. The hook runs on that thread
/// before unwinding, so this is the span of the request or task that
/// panicked. `None` outside any span, or when the panic happened inside the
/// subscriber, which then dispatches to no subscriber.
fn current_spans() -> Option<PanicSpans> {
    tracing::dispatcher::get_default(|dispatch| {
        let registry = dispatch.downcast_ref::<Registry>()?;
        let id = dispatch.current_span().id()?.clone();
        let span = registry.span(&id)?;
        Some(PanicSpans {
            span: span_json(&span),
            spans: span.scope().from_root().map(|span| span_json(&span)).collect(),
            correlation_ids: correlation_ids(span.scope(), |_| false),
        })
    })
}

/// The span context [`current_spans`] found for a panic.
struct PanicSpans {
    span: Value,
    spans: Vec<Value>,
    correlation_ids: Vec<(&'static str, String)>,
}

/// A span as the JSON layer writes it: its fields, then its `name`.
fn span_json(span: &SpanRef<'_, Registry>) -> Value {
    let mut object = span
        .extensions()
        .get::<FormattedFields<JsonFields>>()
        .and_then(|fields| serde_json::from_str::<Map<String, Value>>(fields).ok())
        .unwrap_or_default();
    object.insert("name".to_string(), span.name().into());
    Value::Object(object)
}

/// The JSON line [`write_panic_as_json`] writes. The message reads like
/// Rust's default panic output, so searches for `panicked at` still match.
/// The backtrace is included when `RUST_BACKTRACE` enables capturing one.
fn panic_line(info: &PanicHookInfo<'_>, backtrace: &Backtrace, spans: Option<PanicSpans>) -> Value {
    let payload = info
        .payload()
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
        .unwrap_or("Box<dyn Any>");
    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("<unnamed>");
    let location = info.location().map(ToString::to_string);
    let message = match &location {
        Some(location) => format!("thread '{thread}' panicked at {location}: {payload}"),
        None => format!("thread '{thread}' panicked: {payload}"),
    };
    let mut line = json!({
        "timestamp": Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true),
        "level": "ERROR",
        "message": message,
        "thread": thread,
        "location": location,
        "target": "panic",
    });
    if let Some(spans) = spans {
        line["span"] = spans.span;
        line["spans"] = spans.spans.into();
        for (name, value) in spans.correlation_ids {
            line[name] = value.into();
        }
    }
    if backtrace.status() == BacktraceStatus::Captured {
        line["backtrace"] = backtrace.to_string().into();
    }
    line
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
fn create_otlp_tracer() -> anyhow::Result<(Tracer, SdkTracerProvider, OtlpSettings)> {
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
    }

    let headers_len = headers.len();

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

    let tracer_provider = tracer_provider(exporter, &service_name);
    let tracer = tracer_provider.tracer(service_name.clone());
    let settings = OtlpSettings {
        service_name,
        endpoint,
        header_count: headers_len,
    };

    Ok((tracer, tracer_provider, settings))
}

/// A tracer provider that batches spans to `exporter`, redacting credentials
/// from them first (see [`crate::log_redaction`]).
fn tracer_provider(exporter: impl SpanExporter + 'static, service_name: &str) -> SdkTracerProvider {
    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name(service_name.to_string())
        .build();
    SdkTracerProvider::builder()
        .with_batch_exporter(RedactingSpanExporter(exporter))
        .with_resource(resource)
        .build()
}

/// What [`create_otlp_tracer`] configured, logged once logging is up.
struct OtlpSettings {
    service_name: String,
    endpoint: String,
    header_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::trace::SpanData;
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
        let (_, provider, _) = create_otlp_tracer().expect(
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

    /// Writes JSON lines into a buffer, for tests that read them back.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Captured {
        fn lines(&self) -> Vec<String> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    /// A key of the shape `generate_api_key` produces.
    const KEY: &str = "sk-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-AbCdE";

    /// An input whose derived `Debug` prints its key, as fusillade's batch
    /// input once did.
    #[derive(Debug)]
    #[allow(dead_code, reason = "read only through Debug")]
    struct LeakyInput {
        file_id: u32,
        api_key: String,
    }

    #[tracing::instrument(level = "debug")]
    fn create_record(input: LeakyInput) {
        tracing::warn!(authorization = %format!("Bearer {KEY}"), "upstream rejected the request");
        tracing::error!(error = %format!("invalid key {KEY}"), "create failed");
    }

    /// A key reaching the subscriber through an instrumented argument's
    /// `Debug`, a header field or an error message is redacted in every log
    /// format and in the exported spans.
    #[tokio::test]
    async fn credentials_are_redacted_from_logs_and_exported_spans() {
        let exported = Exported::default();
        let provider = tracer_provider(exported.clone(), "test");
        let json = Captured::default();
        let compact = Captured::default();
        let (json_writer, compact_writer) = (json.clone(), compact.clone());
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new("debug"))
            .with(fmt_layer(LogFormat::Json, move || json_writer.clone()))
            .with(fmt_layer(LogFormat::Compact, move || compact_writer.clone()))
            .with(otel_layer(provider.tracer("test"), "debug"));

        tracing::subscriber::with_default(subscriber, || {
            create_record(LeakyInput {
                file_id: 7,
                api_key: KEY.to_string(),
            });
        });
        provider.shutdown().unwrap();

        for (format, lines) in [("json", json.lines()), ("compact", compact.lines())] {
            assert_eq!(lines.len(), 2, "{format}: {lines:?}");
            for line in &lines {
                assert!(!line.contains(KEY), "{format} line leaked the key: {line}");
                assert!(line.contains("<redacted>"), "{format}: {line}");
            }
        }

        let spans = exported.0.lock().unwrap();
        assert_eq!(spans.len(), 1, "{spans:?}");
        let span = format!("{:?}", spans[0]);
        assert!(!span.contains(KEY), "exported span leaked the key: {span}");
        assert!(span.contains("file_id: 7"), "the rest of the input is still recorded: {span}");
    }

    /// In JSON, the request IDs recorded on the spans an event is in are
    /// repeated at the top level, the innermost value winning and recorded
    /// late (as the request span records them) as well as at creation. An
    /// event that sets one itself keeps its own value, and an event outside
    /// any span gets none.
    #[test]
    fn json_events_carry_their_spans_request_ids_at_the_top_level() {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new("info"))
            .with(fmt_layer(LogFormat::Json, move || writer.clone()));
        tracing::subscriber::with_default(subscriber, || {
            let request = tracing::info_span!("request", x_request_id = tracing::field::Empty, trace_id = "trace-1");
            request.record("x_request_id", tracing::field::display("req-outer"));
            request.in_scope(|| {
                tracing::info_span!("handler", edge_request_id = "edge-1").in_scope(|| {
                    tracing::info!("in request");
                    tracing::info_span!("retry", x_request_id = "req-inner").in_scope(|| tracing::info!("in retry"));
                    tracing::info!(x_request_id = "own", "sets its own");
                });
            });
            tracing::info!("outside");
        });

        let lines = captured.lines();
        let parsed: Vec<serde_json::Value> = lines.iter().map(|line| serde_json::from_str(line).unwrap()).collect();
        let [in_request, in_retry, own, outside] = parsed.as_slice() else {
            panic!("expected four lines: {lines:?}");
        };
        assert_eq!(in_request["x_request_id"], "req-outer");
        assert_eq!(in_request["edge_request_id"], "edge-1");
        assert_eq!(in_request["trace_id"], "trace-1");
        assert_eq!(in_request["spans"][0]["x_request_id"], "req-outer", "the span list is unchanged");
        assert_eq!(in_retry["x_request_id"], "req-inner");
        assert_eq!(own["x_request_id"], "own");
        assert_eq!(
            lines[2].matches(r#""x_request_id":"own""#).count(),
            1,
            "the event's own field is not repeated: {}",
            lines[2]
        );
        assert_eq!(own["edge_request_id"], "edge-1");
        assert!(outside.get("x_request_id").is_none(), "{outside}");
    }

    /// Run by [`json_logging_writes_every_error_as_one_json_line`] in a child
    /// process, which owns the global subscriber and panic hook. Does nothing
    /// in a normal test run.
    #[test]
    fn json_logging_child() {
        if std::env::var_os(JSON_LOGGING_CHILD).is_none() {
            return;
        }
        // Nothing listens on the endpoint; setting up export doesn't connect.
        let provider = init_telemetry(true, LogFormat::Json).unwrap();
        assert!(provider.is_some());
        tracing::info_span!("request").in_scope(|| {
            tracing::error!(error = "first line\nsecond line", request_model = Some("model-a"), "tracing error");
        });
        log::error!("log crate error");
        let _ = std::thread::spawn(|| {
            tracing::info_span!("request", request_id = 7, x_request_id = "req-1").in_scope(|| {
                tracing::info_span!("worker").in_scope(|| panic!("worker panic"));
            });
        })
        .join();
    }

    const JSON_LOGGING_CHILD: &str = "DWCTL_JSON_LOGGING_TEST_CHILD";

    /// With `log_format: json`, a tracing error (multi-line message included),
    /// a `log`-crate error from a dependency and a panic each reach the output
    /// as a single JSON line at level `ERROR`, the panic with the spans it
    /// happened in, and setting up OTLP export prints nothing outside the log.
    /// Runs the real initialization in a child process, since it installs the
    /// global subscriber and panic hook.
    #[test]
    fn json_logging_writes_every_error_as_one_json_line() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "telemetry::tests::json_logging_child", "--nocapture"])
            .env(JSON_LOGGING_CHILD, "1")
            .env("RUST_LOG", "info")
            .env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:9")
            .output()
            .expect("start json logging child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "json logging child failed:\n{stdout}\n{stderr}");

        let lines: Vec<serde_json::Value> = stdout
            .lines()
            .chain(stderr.lines())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let find = |message: &str| {
            lines
                .iter()
                .find(|line| line["message"].as_str().is_some_and(|m| m.contains(message)))
                .unwrap_or_else(|| panic!("no JSON line for {message:?}:\n{stdout}\n{stderr}"))
        };

        let tracing_error = find("tracing error");
        assert_eq!(tracing_error["level"], "ERROR");
        assert_eq!(tracing_error["error"], "first line\nsecond line");
        assert_eq!(tracing_error["request_model"], "model-a");
        assert_eq!(tracing_error["spans"][0]["name"], "request");

        assert_eq!(find("log crate error")["level"], "ERROR");

        let panic = find("worker panic");
        assert_eq!(panic["level"], "ERROR");
        assert_eq!(panic["target"], "panic");
        assert!(
            panic["message"].as_str().unwrap().contains("panicked at"),
            "panic message keeps the default wording: {panic}"
        );
        assert_eq!(panic["span"]["name"], "worker");
        assert_eq!(panic["spans"][0]["name"], "request");
        assert_eq!(panic["spans"][0]["request_id"], 7);
        assert_eq!(panic["spans"][1]["name"], "worker");
        assert_eq!(panic["x_request_id"], "req-1");

        let initialized = find("OTLP export enabled");
        assert_eq!(initialized["endpoint"], "http://127.0.0.1:9/v1/traces");
        assert!(
            !stdout.contains("[OTLP]") && !stderr.contains("[OTLP]"),
            "OTLP setup is logged as an event, not printed: {stderr}"
        );
    }
}
