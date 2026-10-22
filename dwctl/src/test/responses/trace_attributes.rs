//! Customer attributes on the root request span and onwards' request span.
//!
//! Like `trace_binding`, the app spawns tasks that only see the process-wide
//! subscriber, so the scenarios run in a re-executed child that owns it.

use opentelemetry::trace::TracerProvider;
use opentelemetry_sdk::{
    error::OTelSdkResult,
    trace::{Sampler, SdkTracerProvider, SpanData, SpanExporter},
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

#[derive(Clone, Debug, Default)]
struct Exported(Arc<Mutex<Vec<SpanData>>>);
impl SpanExporter for Exported {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        self.0.lock().unwrap().extend(batch);
        Ok(())
    }
}

fn attr(span: &SpanData, key: &str) -> Option<String> {
    span.attributes.iter().find(|a| a.key.as_str() == key).map(|a| a.value.to_string())
}

/// Flushes and takes everything exported since the last call.
fn take(provider: &SdkTracerProvider, exported: &Exported) -> Vec<SpanData> {
    provider.force_flush().unwrap();
    std::mem::take(&mut *exported.0.lock().unwrap())
}

/// The single root `request` span for `url.path`, once its response is recorded.
fn root<'a>(spans: &'a [SpanData], url_path: &str) -> &'a SpanData {
    let mut roots = spans
        .iter()
        .filter(|s| attr(s, "url.path").as_deref() == Some(url_path) && attr(s, "http.response.status_code").is_some());
    let found = roots.next().expect("root request span was not exported");
    assert!(roots.next().is_none(), "expected exactly one root span for {url_path}");
    found
}

fn onwards_request<'a>(spans: &'a [SpanData], root: &SpanData) -> &'a SpanData {
    spans
        .iter()
        .find(|s| s.name == "onwards.request" && s.span_context.trace_id() == root.span_context.trace_id())
        .expect("onwards.request span was not exported")
}

#[dwctl_test_macros::test]
async fn customer_attributes_on_request_spans(pool: sqlx::PgPool) {
    const CHILD: &str = "DWCTL_TRACE_ATTRIBUTES_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        pool.close().await;
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "test::responses::trace_attributes::customer_attributes_on_request_spans",
                "--nocapture",
            ])
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
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("trace-attributes-test")))
            .with(tracing_subscriber::fmt::layer().with_filter(tracing_subscriber::filter::LevelFilter::WARN)),
    )
    .expect("isolated tracing test owns its subscriber");

    let mock = wiremock::MockServer::start().await;
    super::mount_chat_completions_mock(&mock).await;
    let (server, key, bg) = super::setup_ai_test(pool.clone(), &mock, true).await;
    let (owner, key_id): (uuid::Uuid, uuid::Uuid) = sqlx::query_as("SELECT user_id, id FROM api_keys WHERE secret = $1")
        .bind(&key)
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET display_name = 'Example Trading Ltd' WHERE id = $1")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    bg.sync_onwards_config(&pool).await.unwrap();
    let auth = format!("Bearer {key}");
    let path_chat = "/ai/v1/chat/completions";
    let chat = json!({"model":"gpt-4o", "messages":[{"role":"user","content":"hello"}]});

    // Successful realtime request: identity, model and traffic on the root
    // span, no error classification.
    server
        .post(path_chat)
        .add_header("Authorization", &auth)
        .json(&chat)
        .await
        .assert_status_ok();
    let spans = take(&provider, &exported);
    let request = root(&spans, path_chat);
    assert_eq!(attr(request, "doubleword.account_id"), Some(owner.to_string()));
    assert_eq!(attr(request, "doubleword.account_name").as_deref(), Some("Example Trading Ltd"));
    assert_eq!(attr(request, "doubleword.api_key_id"), Some(key_id.to_string()));
    assert_eq!(attr(request, "doubleword.key_purpose").as_deref(), Some("realtime"));
    assert_eq!(attr(request, "gen_ai.request.model").as_deref(), Some("gpt-4o"));
    assert_eq!(attr(request, "doubleword.traffic").as_deref(), Some("realtime"));
    assert_eq!(attr(request, "doubleword.error.source"), None);
    let inner = onwards_request(&spans, request);
    assert_eq!(attr(inner, "onwards.account"), Some(owner.to_string()));
    assert_eq!(attr(inner, "onwards.account_name").as_deref(), Some("Example Trading Ltd"));
    assert_eq!(attr(inner, "onwards.api_key_id"), Some(key_id.to_string()));
    assert_eq!(attr(inner, "onwards.key_purpose").as_deref(), Some("realtime"));
    assert_eq!(attr(inner, "http.response.status_code").as_deref(), Some("200"));

    // The batch daemon's headers from a realtime key do not make it batch
    // traffic: any caller can send them.
    let dispatched = |key: &str| {
        server
            .post(path_chat)
            .add_header("Authorization", format!("Bearer {key}"))
            .add_header("x-fusillade-batch-created-at", "2026-01-01T00:00:00Z")
            .add_header("x-fusillade-batch-completion-window", "24h")
            .json(&chat)
    };
    dispatched(&key).await.assert_status_ok();
    let spans = take(&provider, &exported);
    let request = root(&spans, path_chat);
    assert_eq!(attr(request, "doubleword.traffic").as_deref(), Some("realtime"));
    assert_eq!(attr(request, "doubleword.batch.completion_window"), None);

    // Dispatched by the batch daemon with the account's batch key: traffic is
    // batch, with its window.
    let set_purpose = |purpose: &'static str| {
        sqlx::query("UPDATE api_keys SET purpose = $1 WHERE id = $2")
            .bind(purpose)
            .bind(key_id)
            .execute(&pool)
    };
    set_purpose("batch").await.unwrap();
    bg.sync_onwards_config(&pool).await.unwrap();
    dispatched(&key).await.assert_status_ok();
    let spans = take(&provider, &exported);
    let request = root(&spans, path_chat);
    assert_eq!(attr(request, "doubleword.traffic").as_deref(), Some("batch"));
    assert_eq!(attr(request, "doubleword.batch.completion_window").as_deref(), Some("24h"));
    assert_eq!(attr(request, "doubleword.account_id"), Some(owner.to_string()));
    set_purpose("realtime").await.unwrap();
    bg.sync_onwards_config(&pool).await.unwrap();

    // A gateway refusal (unknown model): source and code are recorded next to
    // the account, and onwards' own span keeps its rejection attributes.
    let refused = server
        .post(path_chat)
        .add_header("Authorization", &auth)
        .json(&json!({"model":"no-such-model", "messages":[{"role":"user","content":"hello"}]}))
        .await;
    assert_eq!(refused.status_code().as_u16(), 404);
    let spans = take(&provider, &exported);
    let request = root(&spans, path_chat);
    assert_eq!(attr(request, "doubleword.account_id"), Some(owner.to_string()));
    assert_eq!(attr(request, "doubleword.error.source").as_deref(), Some("gateway"));
    assert_eq!(attr(request, "doubleword.rejection.code").as_deref(), Some("model_not_found"));
    assert_eq!(attr(request, "http.response.status_code").as_deref(), Some("404"));
    let inner = onwards_request(&spans, request);
    assert_eq!(attr(inner, "error.type").as_deref(), Some("model_not_found"));
    assert_eq!(attr(inner, "onwards.account"), Some(owner.to_string()));
    assert_eq!(attr(inner, "onwards.api_key_id"), Some(key_id.to_string()));
    assert_eq!(attr(inner, "http.response.status_code").as_deref(), Some("404"));

    // A dwctl layer's refusal, before onwards runs, is a gateway decision too,
    // and names the caller.
    let invalid = server
        .post(path_chat)
        .add_header("Authorization", &auth)
        .content_type("application/json")
        .text("not json")
        .await;
    assert_eq!(invalid.status_code().as_u16(), 400);
    let spans = take(&provider, &exported);
    let request = root(&spans, path_chat);
    assert_eq!(attr(request, "doubleword.error.source").as_deref(), Some("gateway"));
    assert_eq!(attr(request, "doubleword.rejection.code").as_deref(), Some("invalid_json"));
    assert_eq!(attr(request, "doubleword.account_id"), Some(owner.to_string()));

    // A body strict mode refuses before forwarding (valid JSON, wrong shape).
    let wrong_shape = server
        .post(path_chat)
        .add_header("Authorization", &auth)
        .json(&json!({"model":"gpt-4o", "messages":"hello"}))
        .await;
    assert_eq!(wrong_shape.status_code().as_u16(), 422);
    let spans = take(&provider, &exported);
    let request = root(&spans, path_chat);
    assert_eq!(attr(request, "doubleword.error.source").as_deref(), Some("gateway"));
    assert_eq!(attr(request, "doubleword.rejection.code").as_deref(), Some("schema_mismatch"));
    assert_eq!(attr(request, "doubleword.account_id"), Some(owner.to_string()));
    assert_eq!(attr(request, "gen_ai.request.model").as_deref(), Some("gpt-4o"));

    // A provider-side 4xx: source is provider, with account and model on the
    // same span, and no gateway rejection code.
    mock.reset().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({"error":{"message":"bad input"}})))
        .mount(&mock)
        .await;
    let rejected = server.post(path_chat).add_header("Authorization", &auth).json(&chat).await;
    assert_eq!(rejected.status_code().as_u16(), 422);
    let spans = take(&provider, &exported);
    let request = root(&spans, path_chat);
    assert_eq!(attr(request, "http.response.status_code").as_deref(), Some("422"));
    assert_eq!(attr(request, "doubleword.error.source").as_deref(), Some("provider"));
    assert_eq!(attr(request, "doubleword.rejection.code"), None);
    assert_eq!(attr(request, "doubleword.account_id"), Some(owner.to_string()));
    assert_eq!(attr(request, "gen_ai.request.model").as_deref(), Some("gpt-4o"));
    let inner = onwards_request(&spans, request);
    assert_eq!(attr(inner, "http.response.status_code").as_deref(), Some("422"));

    // Access revoked: onwards no longer routes this key to the model, and dwctl
    // replaces its bare 403 with an explanation. The refusal still names the
    // account, and is classed as a gateway decision with a specific code.
    sqlx::query("DELETE FROM deployment_groups").execute(&pool).await.unwrap();
    bg.sync_onwards_config(&pool).await.unwrap();
    let denied = server.post(path_chat).add_header("Authorization", &auth).json(&chat).await;
    assert_eq!(denied.status_code().as_u16(), 403);
    let spans = take(&provider, &exported);
    let request = root(&spans, path_chat);
    assert_eq!(attr(request, "doubleword.account_id"), Some(owner.to_string()));
    assert_eq!(attr(request, "doubleword.account_name").as_deref(), Some("Example Trading Ltd"));
    assert_eq!(attr(request, "doubleword.api_key_id"), Some(key_id.to_string()));
    assert_eq!(attr(request, "gen_ai.request.model").as_deref(), Some("gpt-4o"));
    assert_eq!(attr(request, "doubleword.error.source").as_deref(), Some("gateway"));
    assert_eq!(attr(request, "doubleword.rejection.code").as_deref(), Some("model_access_denied"));

    // A key onwards does not know is refused (403) with no identity.
    let unknown = server
        .post(path_chat)
        .add_header("Authorization", "Bearer sk-not-a-key")
        .json(&chat)
        .await;
    assert_eq!(unknown.status_code().as_u16(), 403);
    let spans = take(&provider, &exported);
    let request = root(&spans, path_chat);
    assert_eq!(attr(request, "doubleword.account_id"), None);

    bg.shutdown().await;
}
