//! Redaction of credentials from log and trace output.
//!
//! Every log line and exported span passes through the subscriber that
//! [`crate::telemetry::init_telemetry`] installs, whether it comes from our
//! code, from fusillade or onwards, or from a dependency. Redacting there,
//! rather than at each call site, covers all of them: a `tracing::debug!`
//! that formats a struct holding a key, a `#[tracing::instrument]` argument
//! recorded by its `Debug`, an error chain that quotes a header.
//!
//! [`redact_secrets`] matches credentials by their shape, so it catches
//! formats it knows: dwctl API keys (`sk-` followed by base64url characters,
//! see [`crate::crypto::generate_api_key`]) and the token after `Bearer`. A
//! secret in any other format is not caught here, so types that hold one
//! should still keep it out of their `Debug` output.
//!
//! [`RedactingMakeWriter`] applies it to the log output and
//! [`RedactingSpanExporter`] to span names, attributes, events and statuses
//! before they are exported. Output that bypasses the subscriber
//! (`println!`, `eprintln!`, `dbg!`) is denied by the workspace clippy lints.

use std::borrow::Cow;
use std::io;
use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::trace::Status;
use opentelemetry::{Array, KeyValue, StringValue, Value};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use regex::Regex;
use tracing::Metadata;
use tracing_subscriber::fmt::MakeWriter;

/// What a redacted credential is replaced with.
pub const REDACTED: &str = "<redacted>";

/// dwctl API keys are `sk-` and 43 or more base64url characters; 20 also
/// covers shorter test and provider keys of the same shape (`sk-proj-...`).
/// Bearer tokens are matched up to the first character a token can't contain.
static SECRET_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?P<bearer>\bbearer\s+)[A-Za-z0-9._~+/=-]+|\bsk-[A-Za-z0-9_-]{20,}").expect("secret pattern is valid")
});

/// `text` with every API key and bearer token replaced by [`REDACTED`].
/// Borrows `text` when it holds none.
pub fn redact_secrets(text: &str) -> Cow<'_, str> {
    SECRET_PATTERN.replace_all(text, |captures: &regex::Captures<'_>| match captures.name("bearer") {
        Some(prefix) => format!("{}{REDACTED}", prefix.as_str()),
        None => REDACTED.to_string(),
    })
}

/// A [`MakeWriter`] whose writers pass everything through [`redact_secrets`].
///
/// The fmt layer formats each event into a buffer and writes it with one
/// `write_all`, so a whole line is redacted at once and a credential is never
/// split across two writes.
pub struct RedactingMakeWriter<M>(pub M);

impl<'a, M: MakeWriter<'a>> MakeWriter<'a> for RedactingMakeWriter<M> {
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter(self.0.make_writer())
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> Self::Writer {
        RedactingWriter(self.0.make_writer_for(meta))
    }
}

/// The writer [`RedactingMakeWriter`] makes.
pub struct RedactingWriter<W>(W);

impl<W: io::Write> io::Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write_all(redact_secrets(&String::from_utf8_lossy(buf)).as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// A [`SpanExporter`] that passes each span's name, string attributes, events
/// and status description through [`redact_secrets`] before handing the batch
/// to the wrapped exporter.
#[derive(Debug)]
pub struct RedactingSpanExporter<E>(pub E);

impl<E: SpanExporter> SpanExporter for RedactingSpanExporter<E> {
    async fn export(&self, mut batch: Vec<SpanData>) -> OTelSdkResult {
        batch.iter_mut().for_each(redact_span);
        self.0.export(batch).await
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.0.set_resource(resource);
    }
}

fn redact_span(span: &mut SpanData) {
    redact_cow(&mut span.name);
    redact_attributes(&mut span.attributes);
    for event in span.events.events.iter_mut() {
        redact_cow(&mut event.name);
        redact_attributes(&mut event.attributes);
    }
    if let Status::Error { description } = &mut span.status {
        redact_cow(description);
    }
}

fn redact_cow(text: &mut Cow<'static, str>) {
    if let Cow::Owned(redacted) = redact_secrets(text) {
        *text = Cow::Owned(redacted);
    }
}

fn redact_attributes(attributes: &mut [KeyValue]) {
    for attribute in attributes {
        match &mut attribute.value {
            Value::String(value) => redact_string_value(value),
            Value::Array(Array::String(values)) => values.iter_mut().for_each(redact_string_value),
            _ => {}
        }
    }
}

fn redact_string_value(value: &mut StringValue) {
    if let Cow::Owned(redacted) = redact_secrets(value.as_str()) {
        *value = redacted.into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key of the shape `generate_api_key` produces.
    const KEY: &str = "sk-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-AbCdE";

    #[test]
    fn redacts_api_keys_wherever_they_appear() {
        assert_eq!(redact_secrets(KEY), REDACTED);
        assert_eq!(
            redact_secrets(&format!(r#"BatchInput {{ api_key: Some("{KEY}") }}"#)),
            r#"BatchInput { api_key: Some("<redacted>") }"#
        );
        assert_eq!(
            redact_secrets(&format!(r#"{{"api_key":"{KEY}","n":1}}"#)),
            r#"{"api_key":"<redacted>","n":1}"#
        );
    }

    #[test]
    fn redacts_bearer_tokens_and_keeps_the_scheme() {
        assert_eq!(
            redact_secrets("authorization: Bearer abc.def-ghi"),
            "authorization: Bearer <redacted>"
        );
        assert_eq!(redact_secrets("authorization: bearer abc"), "authorization: bearer <redacted>");
    }

    #[test]
    fn leaves_text_without_credentials_borrowed() {
        for text in ["sk-short", "risk-assessment", "task-1234", "no secrets here"] {
            assert!(matches!(redact_secrets(text), Cow::Borrowed(t) if t == text), "{text}");
        }
    }

    #[test]
    fn writer_redacts_each_write() {
        let mut out = Vec::new();
        io::Write::write_all(&mut RedactingWriter(&mut out), format!("key={KEY}\n").as_bytes()).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "key=<redacted>\n");
    }

    #[test]
    fn exporter_redacts_name_attributes_events_and_status() {
        let mut span = test_span_data();
        span.name = format!("POST {KEY}").into();
        span.attributes = vec![
            KeyValue::new("input", format!("api_key: {KEY:?}")),
            KeyValue::new("keys", Value::Array(Array::String(vec![KEY.to_string().into()]))),
            KeyValue::new("count", 3),
        ];
        span.events.events.push(opentelemetry::trace::Event::new(
            format!("failed with {KEY}"),
            std::time::SystemTime::now(),
            vec![KeyValue::new("authorization", format!("Bearer {KEY}"))],
            0,
        ));
        span.status = Status::error(format!("rejected {KEY}"));

        redact_span(&mut span);

        let debug = format!("{span:?}");
        assert!(!debug.contains(KEY), "secret exported: {debug}");
        assert_eq!(span.name, "POST <redacted>");
        assert_eq!(span.attributes[2], KeyValue::new("count", 3));
    }

    fn test_span_data() -> SpanData {
        use opentelemetry::InstrumentationScope;
        use opentelemetry::trace::{SpanContext, SpanId, SpanKind, TraceFlags, TraceId, TraceState};
        use opentelemetry_sdk::trace::{SpanEvents, SpanLinks};

        SpanData {
            span_context: SpanContext::new(TraceId::from(1), SpanId::from(1), TraceFlags::SAMPLED, false, TraceState::default()),
            parent_span_id: SpanId::INVALID,
            parent_span_is_remote: false,
            span_kind: SpanKind::Internal,
            name: "span".into(),
            start_time: std::time::SystemTime::now(),
            end_time: std::time::SystemTime::now(),
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            events: SpanEvents::default(),
            links: SpanLinks::default(),
            status: Status::Unset,
            instrumentation_scope: InstrumentationScope::builder("test").build(),
        }
    }
}
