//! Span attributes for the requests onwards rejects itself, using an
//! in-memory span exporter.

use std::sync::{Arc, Mutex};

use axum::{Router, body::Body, extract::Request, http::StatusCode};
use onwards::{AppState, strict::build_strict_router, target::Targets, test_utils::MockHttpClient};
use opentelemetry::{Value as AttributeValue, trace::TracerProvider};
use opentelemetry_sdk::{
    error::OTelSdkResult,
    trace::{Sampler, SdkTracerProvider, SpanData, SpanExporter},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use tracing::{Dispatch, Instrument, instrument::WithSubscriber};
use tracing_subscriber::layer::SubscriberExt;

const ALIAS: &str = "span-model";
const KEY: &str = "sk-span-test";
const ACCOUNT: &str = "account-1";
const API_KEY_ID: &str = "6f1c2a7e-0b5d-4c1e-9a3f-2d8e7b6a5c41";

#[derive(Debug, Clone, Default)]
struct SpanRecorder(Arc<Mutex<Vec<SpanData>>>);

impl SpanExporter for SpanRecorder {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        self.0.lock().unwrap().extend(batch);
        Ok(())
    }
}

fn router() -> Router {
    let mut targets = Targets::from_config(
        serde_json::from_value(json!({
            "auth": {
                "global_keys": [KEY],
                "key_definitions": {"span-key": {
                    "key": KEY,
                    "labels": {"account": ACCOUNT, "api_key_id": API_KEY_ID}
                }}
            },
            "targets": {ALIAS: {"url": "https://upstream.example.com/v1/"}}
        }))
        .unwrap(),
    )
    .unwrap();
    targets.strict_mode = true;
    let mock = MockHttpClient::new(StatusCode::OK, "{}");
    build_strict_router(AppState::with_client(targets, mock))
}

/// Sends `body` to `path` inside an outer span standing in for an embedding
/// gateway's request span, and returns the response status and every span.
async fn send(path: &str, body: Value) -> (StatusCode, Vec<SpanData>) {
    let spans = SpanRecorder::default();
    let provider = SdkTracerProvider::builder()
        .with_sampler(Sampler::AlwaysOn)
        .with_simple_exporter(spans.clone())
        .build();
    let dispatch = Dispatch::new(
        tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("rejection-test"))),
    );
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {KEY}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let status = async {
        router()
            .oneshot(request)
            .instrument(tracing::info_span!("gateway_request"))
            .await
            .unwrap()
            .status()
    }
    .with_subscriber(dispatch)
    .await;
    provider.force_flush().unwrap();
    let spans = spans.0.lock().unwrap().clone();
    (status, spans)
}

fn attribute(span: &SpanData, key: &str) -> Option<AttributeValue> {
    span.attributes
        .iter()
        .find(|attribute| attribute.key.as_str() == key)
        .map(|attribute| attribute.value.clone())
}

fn span<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
    spans
        .iter()
        .find(|span| span.name == name)
        .unwrap_or_else(|| panic!("no span named {name}"))
}

fn assert_caller(span: &SpanData) {
    assert_eq!(
        attribute(span, "onwards.account"),
        Some(AttributeValue::from(ACCOUNT))
    );
    assert_eq!(
        attribute(span, "onwards.api_key_id"),
        Some(AttributeValue::from(API_KEY_ID))
    );
}

#[tokio::test]
async fn handler_rejections_are_recorded_on_the_request_span() {
    let (status, spans) = send(
        "/chat/completions",
        json!({
            "model": ALIAS,
            "messages": [{"role": "user", "content": "Hello"}],
            "chat_template_kwargs": {"custom_flag": 1}
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let request = span(&spans, "onwards.request");
    assert_eq!(
        attribute(request, "http.response.status_code"),
        Some(AttributeValue::from("400"))
    );
    assert_eq!(
        attribute(request, "error.type"),
        Some(AttributeValue::from("unsupported_parameter"))
    );
    assert_eq!(
        attribute(request, "onwards.rejection.param"),
        Some(AttributeValue::from("chat_template_kwargs"))
    );
    assert_caller(request);
}

#[tokio::test]
async fn early_strict_rejections_are_recorded_on_the_enclosing_span() {
    let (status, spans) = send(
        "/chat/completions",
        json!({"model": ALIAS, "messages": "not a list"}),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(spans.iter().all(|span| span.name != "onwards.request"));
    let gateway = span(&spans, "gateway_request");
    assert_eq!(
        attribute(gateway, "error.type"),
        Some(AttributeValue::from("schema_mismatch"))
    );
    assert_eq!(attribute(gateway, "onwards.rejection.param"), None);
    assert_caller(gateway);
}
