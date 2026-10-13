//! `X-Request-ID` is minted by dwctl for every request, echoed on every
//! response and forwarded to providers; inbound values are never trusted.

use opentelemetry_sdk::{
    error::OTelSdkResult,
    trace::{SpanData, SpanExporter},
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Clone, Debug, Default)]
struct Exported(Arc<Mutex<Vec<SpanData>>>);
impl SpanExporter for Exported {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        self.0.lock().unwrap().extend(batch);
        Ok(())
    }
}

/// Collects formatted log output in memory.
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);
impl LogBuffer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}
impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn response_id(response: &axum_test::TestResponse) -> Uuid {
    let value = response.header("x-request-id");
    let id = Uuid::parse_str(value.to_str().expect("ascii header")).expect("x-request-id is a UUID");
    assert_eq!(id.get_version_num(), 7, "x-request-id should be a UUIDv7");
    id
}

#[dwctl_test_macros::test]
async fn response_carries_minted_id_not_inbound(pool: sqlx::PgPool) {
    let mock = wiremock::MockServer::start().await;
    super::mount_chat_completions_mock(&mock).await;
    let (server, key, _bg) = super::setup_ai_test(pool, &mock, true).await;
    let body = json!({"model":"gpt-4o", "messages":[{"role":"user","content":"hello"}]});

    let inbound = "edge-supplied-id-123";
    let ok = server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {key}"))
        .add_header("x-request-id", inbound)
        .json(&body)
        .await;
    ok.assert_status_ok();
    let first = response_id(&ok);
    assert_ne!(first.to_string(), inbound, "inbound value must not be echoed");

    // The provider receives our ID, not the client's.
    let upstream = mock.received_requests().await.unwrap();
    let sent = upstream
        .last()
        .unwrap()
        .headers
        .get("x-request-id")
        .expect("provider receives x-request-id")
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(sent, first.to_string());

    // Every request gets a fresh ID.
    let second = server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&body)
        .await;
    assert_ne!(response_id(&second), first);
}

#[dwctl_test_macros::test]
async fn rejected_and_unrouted_responses_carry_id(pool: sqlx::PgPool) {
    let mock = wiremock::MockServer::start().await;
    super::mount_chat_completions_mock(&mock).await;
    let (server, _key, _bg) = super::setup_ai_test(pool, &mock, true).await;

    // Rejected by onwards authentication.
    let unauthorized = server
        .post("/ai/v1/chat/completions")
        .add_header("x-request-id", "client-chosen")
        .json(&json!({"model":"gpt-4o","messages":[]}))
        .await;
    unauthorized.assert_status(axum::http::StatusCode::UNAUTHORIZED);
    response_id(&unauthorized);

    // Unknown route, and an admin API route.
    let missing = server.get("/does-not-exist").await;
    response_id(&missing);
    let admin = server.get("/admin/api/v1/users").await;
    response_id(&admin);
}

/// The root request span carries our ID and the sanitised edge ID, both as
/// log fields and as exported OTel attributes.
#[dwctl_test_macros::test]
async fn root_span_records_request_ids(pool: sqlx::PgPool) {
    use opentelemetry::trace::TracerProvider;
    use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
    use tracing_subscriber::layer::SubscriberExt;

    let exporter = Exported::default();
    let provider = SdkTracerProvider::builder()
        .with_sampler(Sampler::AlwaysOn)
        .with_simple_exporter(exporter.clone())
        .build();
    // Plain-text log output, as production writes it, to check the field format.
    let logs = LogBuffer::default();
    // Thread-local default: the in-process test transport serves the request on
    // this task, so the request span is created under this subscriber.
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("request-id-test")))
            .with(
                tracing_subscriber::fmt::layer()
                    .compact()
                    .with_ansi(false)
                    .with_writer(logs.clone()),
            ),
    );

    let mock = wiremock::MockServer::start().await;
    let (server, _key, _bg) = super::setup_ai_test(pool, &mock, true).await;
    exporter.0.lock().unwrap().clear();

    let attr = |span: &SpanData, key: &str| {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.to_string())
    };
    let root_for = |path: &str| {
        exporter
            .0
            .lock()
            .unwrap()
            .clone()
            .into_iter()
            .find(|s| attr(s, "url.path").as_deref() == Some(path))
            .expect("root request span exported")
    };

    let response = server.get("/missing-a").add_header("x-request-id", "edge-123").await;
    let id = response.header("x-request-id").to_str().unwrap().to_owned();
    let span = root_for("/missing-a");
    assert_eq!(attr(&span, "x_request_id").as_deref(), Some(id.as_str()));
    assert_eq!(attr(&span, "edge_request_id").as_deref(), Some("edge-123"));
    // Log lines carry the IDs unquoted, so `|= "x_request_id=<id>"` matches.
    let text = logs.text();
    assert!(
        text.contains(&format!("x_request_id={id}")),
        "unquoted x_request_id in logs:\n{text}"
    );
    assert!(
        text.contains("edge_request_id=edge-123"),
        "unquoted edge_request_id in logs:\n{text}"
    );

    // Unusable inbound values are dropped rather than recorded.
    server.get("/missing-b").add_header("x-request-id", "x".repeat(129)).await;
    let span = root_for("/missing-b");
    assert!(attr(&span, "x_request_id").is_some());
    assert_eq!(attr(&span, "edge_request_id"), None);
}
