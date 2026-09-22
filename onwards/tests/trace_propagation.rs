//! Trace-context propagation through the real routing and forwarding path,
//! using an in-memory span exporter and a header-recording downstream.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::StatusCode;
use axum::{Router, body::Body, extract::Request, http::HeaderMap, response::Response};
use onwards::{
    AppState, build_router, client::HttpClient, strict::build_strict_router, target::Targets,
};
use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::{TraceContextExt, TracerProvider};
use opentelemetry_sdk::{
    error::OTelSdkResult,
    trace::{Sampler, SdkTracerProvider, SpanData, SpanExporter},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use tracing::{Dispatch, Instrument, instrument::WithSubscriber};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::layer::SubscriberExt;

const TRACEPARENT: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
const TRACESTATE: &str = "vendor=value";

fn router(client: impl HttpClient + Clone + Send + Sync + 'static, target: Value) -> Router {
    let targets = Targets::from_config(
        serde_json::from_value(json!({"targets": {"test-model": target}})).unwrap(),
    )
    .unwrap();
    build_router(AppState::with_client(targets, client))
}

// Same span capabilities as standalone init_telemetry() with OTLP disabled.
fn console_only() -> Dispatch {
    Dispatch::new(
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::sink)),
    )
}

// Embedded requests carry no W3C headers: the embedding gateway strips them
// once its own request span has adopted them.
fn request() -> Request {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"model":"test-model"}"#))
        .unwrap()
}

#[derive(Debug, Clone, Default)]
struct SpanRecorder(Arc<Mutex<Vec<SpanData>>>);

impl SpanExporter for SpanRecorder {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        self.0.lock().unwrap().extend(batch);
        Ok(())
    }
}

fn traced() -> (Dispatch, SdkTracerProvider, SpanRecorder) {
    let spans = SpanRecorder::default();
    let provider = SdkTracerProvider::builder()
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::AlwaysOn)))
        .with_simple_exporter(spans.clone())
        .build();
    let dispatch = Dispatch::new(
        tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("cor-678-test"))),
    );
    (dispatch, provider, spans)
}

// A distinct subscriber per hop models an uninstrumented intermediate process.
#[derive(Clone, Debug)]
struct Hop(Router);

#[async_trait]
impl HttpClient for Hop {
    async fn request(
        &self,
        req: Request,
    ) -> Result<Response, Box<dyn std::error::Error + Send + Sync>> {
        Ok(self
            .0
            .clone()
            .oneshot(req)
            .with_subscriber(console_only())
            .await?)
    }
}

#[derive(Clone, Debug, Default)]
struct FallbackRecorder(Arc<Mutex<Vec<HeaderMap>>>);

#[async_trait]
impl HttpClient for FallbackRecorder {
    async fn request(
        &self,
        req: Request,
    ) -> Result<Response, Box<dyn std::error::Error + Send + Sync>> {
        let mut headers = self.0.lock().unwrap();
        headers.push(req.headers().clone());
        Ok(Response::builder()
            .status(if headers.len() == 1 { 503 } else { 200 })
            .header("content-type", "application/json")
            .body(Body::from(r#"{"choices":[]}"#))
            .unwrap())
    }
}

#[tokio::test]
async fn gateway_fallback_attempts_keep_distinct_parent_ids_through_untraced_hop() {
    let (dispatch, provider, spans) = traced();
    let recorder = FallbackRecorder::default();
    let intermediate = router(
        recorder.clone(),
        json!({
            "url":"http://frontend/v1", "propagate_trace_context":true
        }),
    );
    let gateway = router(
        Hop(intermediate),
        json!({
            "strategy":"priority", "fallback":{"enabled":true,"on_status":[500,502,503]},
            "providers":[
                {"url":"http://internal-a/v1","propagate_trace_context":true},
                {"url":"http://internal-b/v1","propagate_trace_context":true}
            ]
        }),
    );
    let (response, anchor) = async {
        let span = tracing::info_span!("gateway_request");
        let headers = std::collections::HashMap::from([
            ("traceparent".to_owned(), TRACEPARENT.to_owned()),
            ("tracestate".to_owned(), TRACESTATE.to_owned()),
        ]);
        span.set_parent(
            opentelemetry_sdk::propagation::TraceContextPropagator::new().extract(&headers),
        )
        .unwrap();
        let anchor = span.context().span().span_context().span_id().to_string();
        (
            gateway.oneshot(request()).instrument(span).await.unwrap(),
            anchor,
        )
    }
    .with_subscriber(dispatch)
    .await;
    assert!(response.status().is_success());
    // Drain the body so spans held by streaming response wrappers can close.
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    provider.force_flush().unwrap();
    let headers = recorder.0.lock().unwrap();
    assert_eq!(headers.len(), 2);
    let parents: Vec<_> = headers
        .iter()
        .map(|h| h["traceparent"].to_str().unwrap())
        .collect();
    assert_ne!(parents[0], parents[1]);
    let spans = spans.0.lock().unwrap();
    for (index, parent) in parents.iter().enumerate() {
        let parts: Vec<_> = parent.split('-').collect();
        assert_eq!(parts[1], TRACEPARENT.split('-').nth(1).unwrap());
        assert_eq!(parts[3], "01");
        assert_eq!(headers[index]["tracestate"], TRACESTATE);
        let attempt = spans
            .iter()
            .find(|s| s.span_context.span_id().to_string() == parts[2])
            .unwrap();
        assert_eq!(attempt.name, "onwards.provider_attempt");
        assert!(is_descendant(&spans, parts[2], &anchor));
        assert!(
            attempt
                .attributes
                .iter()
                .any(|a| a.key.as_str() == "attempt"
                    && a.value.to_string() == (index + 1).to_string())
        );
    }
    assert_eq!(
        spans
            .iter()
            .filter(|s| s.name == "onwards.provider_attempt")
            .count(),
        2
    );
}

fn is_descendant(spans: &[SpanData], child: &str, ancestor: &str) -> bool {
    let mut cursor = child.to_owned();
    for _ in 0..spans.len() {
        if cursor == ancestor {
            return true;
        }
        let Some(span) = spans
            .iter()
            .find(|s| s.span_context.span_id().to_string() == cursor)
        else {
            return false;
        };
        cursor = span.parent_span_id.to_string();
    }
    false
}

#[rstest::rstest]
#[case(
    "/v1/chat/completions",
    r#"{"model":"test-model","messages":[{"role":"user","content":"hello"}]}"#
)]
#[case("/v1/responses", r#"{"model":"test-model","input":"hello"}"#)]
#[case("/v1/embeddings", r#"{"model":"test-model","input":"hello"}"#)]
#[case("/v1/completions", r#"{"model":"test-model","prompt":"hello"}"#)]
#[tokio::test]
async fn strict_handler_keeps_gateway_ancestry(#[case] path: &str, #[case] body: &str) {
    let (dispatch, provider, spans) = traced();
    let recorder = FallbackRecorder::default();
    let mut targets = Targets::from_config(
        serde_json::from_value(json!({
            "targets":{"test-model":{"url":"http://frontend/v1","propagate_trace_context":true}}
        }))
        .unwrap(),
    )
    .unwrap();
    targets.strict_mode = true;
    let app = Router::new().nest(
        "/v1",
        build_strict_router(AppState::with_client(targets, recorder.clone())),
    );
    let anchor = async {
        let span = tracing::info_span!("gateway_request");
        let headers =
            std::collections::HashMap::from([("traceparent".to_owned(), TRACEPARENT.to_owned())]);
        span.set_parent(
            opentelemetry_sdk::propagation::TraceContextPropagator::new().extract(&headers),
        )
        .unwrap();
        let anchor = span.context().span().span_context().span_id().to_string();
        let mut req = request();
        *req.uri_mut() = path.parse().unwrap();
        *req.body_mut() = Body::from(body.to_owned());
        let response = app.oneshot(req).instrument(span).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        anchor
    }
    .with_subscriber(dispatch)
    .await;
    provider.force_flush().unwrap();
    let headers = recorder.0.lock().unwrap();
    assert_eq!(headers.len(), 1);
    let parts: Vec<_> = headers[0]["traceparent"]
        .to_str()
        .unwrap()
        .split('-')
        .collect();
    assert!(is_descendant(&spans.0.lock().unwrap(), parts[2], &anchor));
}
