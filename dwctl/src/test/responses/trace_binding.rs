use opentelemetry::trace::TracerProvider;
use opentelemetry_sdk::{
    error::OTelSdkResult,
    trace::{Sampler, SdkTracerProvider, SpanData, SpanExporter},
};
use serde_json::json;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;

#[derive(Clone, Debug, Default)]
struct Exported(Arc<Mutex<Vec<SpanData>>>);
impl SpanExporter for Exported {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        self.0.lock().unwrap().extend(batch);
        Ok(())
    }
}

fn descendant(spans: &[SpanData], child: &str, ancestor: &str) -> bool {
    let mut current = child.to_owned();
    for _ in 0..spans.len() {
        if current == ancestor {
            return true;
        }
        let Some(s) = spans.iter().find(|s| s.span_context.span_id().to_string() == current) else {
            return false;
        };
        current = s.parent_span_id.to_string();
    }
    false
}

#[derive(sqlx::FromRow, Debug)]
struct Capture {
    trace_id: Option<String>,
    gateway_span_id: Option<String>,
    fusillade_request_id: Option<uuid::Uuid>,
}

async fn captures(pool: &sqlx::PgPool, expected: usize) -> Vec<Capture> {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let rows = sqlx::query_as::<_, Capture>(
                "SELECT trace_id,gateway_span_id,fusillade_request_id FROM http_analytics WHERE model='gpt-4o' ORDER BY id",
            )
            .fetch_all(pool)
            .await
            .unwrap();
            if rows.len() >= expected {
                return rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("analytics captures did not arrive")
}

// Full Application / router, actual Outlet 0.11 handler, durable outbox and
// projector, database-backed routing configuration, and a local fake provider.
#[sqlx::test]
async fn gateway_capture_anchors(pool: sqlx::PgPool) {
    // Application tasks run on multiple threads. Give this test its own process-wide
    // subscriber without competing with test-log or other parallel tests.
    const CHILD: &str = "DWCTL_TRACE_BINDING_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        pool.close().await;
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "test::responses::trace_binding::gateway_capture_anchors", "--nocapture"])
            .env(CHILD, "1")
            .output()
            .expect("start isolated tracing test");
        assert!(
            output.status.success(),
            "isolated tracing test failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "child test filter must execute the tracing test"
        );
        return;
    }
    let exported = Exported::default();
    let provider = SdkTracerProvider::builder()
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::AlwaysOn)))
        .with_simple_exporter(exported.clone())
        .build();
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("cor678-local")))
            .with(tracing_subscriber::fmt::layer().with_filter(tracing_subscriber::filter::LevelFilter::WARN)),
    )
    .expect("isolated tracing test owns its subscriber");
    let mock = wiremock::MockServer::start().await;
    super::mount_chat_completions_mock(&mock).await;
    let (server, key, bg) = super::setup_ai_test(pool.clone(), &mock, true).await;
    sqlx::query("UPDATE deployed_models SET trusted=true WHERE alias='gpt-4o'")
        .execute(&pool)
        .await
        .unwrap();
    bg.sync_onwards_config(&pool).await.unwrap();
    let auth = format!("Bearer {key}");
    let traceparent = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
    let body = json!({"model":"gpt-4o", "messages":[{"role":"user","content":"hello"}]});
    // Verify the trace actually sent downstream belongs to the stored gateway
    // capture, including when a customer supplied a different remote parent.
    server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", &auth)
        .add_header("traceparent", traceparent)
        .json(&body)
        .await
        .assert_status_ok();
    let first = captures(&pool, 1).await;

    let background = server
        .post("/ai/v1/responses")
        .add_header("Authorization", &auth)
        .json(&json!({"model":"gpt-4o","input":"hello","background":true,"service_tier":"priority"}))
        .await;
    background.assert_status(axum::http::StatusCode::ACCEPTED);
    captures(&pool, 2).await;

    // The daemon path reuses the logical UUID instead of generating a new one.
    let logical = first[0].fusillade_request_id.unwrap().to_string();
    server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", &auth)
        .add_header("traceparent", traceparent)
        .add_header("x-fusillade-request-id", &logical)
        .json(&body)
        .await
        .assert_status_ok();
    captures(&pool, 3).await;
    let mut upstream = mock.received_requests().await.unwrap();
    mock.reset().await;
    let sse = "data: {\"id\":\"chatcmpl-probe\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"chatcmpl-probe\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\ndata: [DONE]\n\n";
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw(sse.as_bytes().to_vec(), "text/event-stream"))
        .mount(&mock)
        .await;
    for (path, body) in [
        ("/ai/v1/responses", json!({"model":"gpt-4o","stream":true,"input":"hello"})),
        (
            "/ai/v1/messages",
            json!({"model":"gpt-4o","stream":true,"max_tokens":20,"messages":[{"role":"user","content":"hello"}]}),
        ),
    ] {
        server
            .post(path)
            .add_header("Authorization", &auth)
            .add_header("anthropic-version", "2023-06-01")
            .add_header("traceparent", traceparent)
            .json(&body)
            .await
            .assert_status_ok();
    }
    let rows = captures(&pool, 5).await;
    upstream.extend(mock.received_requests().await.unwrap());
    assert_eq!(upstream.len(), 5);
    provider.force_flush().unwrap();
    {
        let spans = exported.0.lock().unwrap();
        let attempts: Vec<_> = spans.iter().filter(|s| s.name == "onwards.provider_attempt").collect();
        assert_eq!(attempts.len(), 5);
        for row in &rows {
            let anchor_id = row.gateway_span_id.as_deref().expect("missing capture span");
            let anchor = spans
                .iter()
                .find(|s| s.span_context.span_id().to_string() == anchor_id)
                .expect("captured span not exported");
            assert_eq!(anchor.name, "inference_middleware");
            assert_eq!(Some(anchor.span_context.trace_id().to_string()), row.trace_id);
            let logical = row.fusillade_request_id.unwrap().to_string();
            assert!(
                anchor
                    .attributes
                    .iter()
                    .any(|a| a.key.as_str() == "doubleword.request_id" && a.value.to_string() == logical)
            );
            assert_eq!(
                attempts
                    .iter()
                    .filter(|s| descendant(&spans, &s.span_context.span_id().to_string(), anchor_id))
                    .count(),
                1
            );
        }
        // Match each outgoing W3C parent to the exported attempt and its exact
        // persisted gateway ancestor, rather than just checking header presence.
        let mut sent_parents = HashSet::new();
        for request in upstream {
            let parent: Vec<_> = request.headers["traceparent"].to_str().unwrap().split('-').collect();
            assert!(
                sent_parents.insert(parent[2].to_owned()),
                "each dispatch must send its own attempt span"
            );
            let attempt = attempts
                .iter()
                .find(|s| s.span_context.span_id().to_string() == parent[2])
                .expect("downstream must receive the provider-attempt span");
            assert_eq!(attempt.span_context.trace_id().to_string(), parent[1]);
            assert_eq!(
                rows.iter()
                    .filter(|r| r.trace_id.as_deref() == Some(parent[1])
                        && descendant(&spans, parent[2], r.gateway_span_id.as_deref().unwrap()))
                    .count(),
                1,
                "downstream trace must connect to exactly one stored gateway capture"
            );
        }
    }
    bg.shutdown().await;
}
