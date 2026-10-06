use std::sync::{Arc, LazyLock, Mutex};

use axum::http::StatusCode;
use axum_prometheus::metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use axum_test::TestServer;
use onwards::{AppState, build_router, target::Targets};
use serde_json::json;

static METRICS: LazyLock<PrometheusHandle> = LazyLock::new(|| {
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    metrics::set_global_recorder(recorder).unwrap();
    handle
});

const COMPLETION: &str = r#"{"id":"chatcmpl-1","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"Hi"},"finish_reason":"stop"}]}"#;

/// Answers the preferred host with a fixed status and every other host with
/// `alternate`. A 502 stands for a connection error.
#[derive(Debug, Clone)]
struct HostClient {
    preferred: StatusCode,
    alternate: StatusCode,
    calls: Arc<Mutex<Vec<String>>>,
}

impl HostClient {
    fn new(preferred: u16, alternate: u16) -> Self {
        Self {
            preferred: StatusCode::from_u16(preferred).unwrap(),
            alternate: StatusCode::from_u16(alternate).unwrap(),
            calls: Default::default(),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl onwards::client::HttpClient for HostClient {
    async fn request(
        &self,
        req: axum::extract::Request,
    ) -> Result<axum::response::Response, Box<dyn std::error::Error + Send + Sync>> {
        let host = req.uri().host().unwrap_or_default().to_string();
        self.calls.lock().unwrap().push(host.clone());
        let status = if host == "preferred.example.com" {
            self.preferred
        } else {
            self.alternate
        };
        if status == StatusCode::BAD_GATEWAY {
            return Err("connection refused".into());
        }
        let body = if status.is_success() {
            COMPLETION
        } else {
            r#"{"error":{"message":"refused"}}"#
        };
        Ok(axum::response::Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .unwrap())
    }
}

fn server(alias: &str, client: HostClient) -> TestServer {
    LazyLock::force(&METRICS);
    let targets = Targets::from_config(
        serde_json::from_value(json!({
            "targets": {
                alias: {
                    "strategy": "priority",
                    "fallback": {"enabled": true, "on_status": [400, 529]},
                    "providers": [
                        {"url": "https://preferred.example.com"},
                        {"url": "https://alternate.example.com"}
                    ]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let state =
        AppState::with_client(targets, client).with_first_token_timeout_exempt_header("x-batch");
    TestServer::new(build_router(state)).unwrap()
}

fn count(metric: &str, alias: &str, cause: &str, traffic: &str) -> Option<f64> {
    METRICS
        .render()
        .lines()
        .find(|line| {
            line.starts_with(&format!("{metric}{{"))
                && line.contains(&format!("model=\"{alias}\""))
                && line.contains(&format!("cause=\"{cause}\""))
                && line.contains(&format!("traffic=\"{traffic}\""))
        })
        .map(|line| line.split_whitespace().last().unwrap().parse().unwrap())
}

fn counted(metric: &str, alias: &str) -> bool {
    METRICS.render().lines().any(|line| {
        line.starts_with(&format!("{metric}{{")) && line.contains(&format!("model=\"{alias}\""))
    })
}

async fn post(server: &TestServer, alias: &str, dispatched: bool) -> axum_test::TestResponse {
    let request = server
        .post("/v1/chat/completions")
        .json(&json!({"model": alias, "messages": [{"role": "user", "content": "Hello"}]}));
    if dispatched {
        request.add_header("x-batch", "true").await
    } else {
        request.await
    }
}

#[tokio::test]
async fn a_request_answered_after_failing_over_is_a_rescue() {
    let alias = "rescued-400";
    let client = HostClient::new(400, 200);
    let server = server(alias, client.clone());

    post(&server, alias, false).await.assert_status_ok();

    assert_eq!(
        client.calls(),
        ["preferred.example.com", "alternate.example.com"]
    );
    assert_eq!(
        count("onwards_failovers_total", alias, "400", "realtime"),
        Some(1.0)
    );
    assert_eq!(
        count("onwards_fallback_rescues_total", alias, "400", "realtime"),
        Some(1.0)
    );
}

#[tokio::test]
async fn rescues_carry_the_first_failure_and_the_traffic_class() {
    let alias = "rescued-529";
    let server = server(alias, HostClient::new(529, 200));

    post(&server, alias, true).await.assert_status_ok();

    assert_eq!(
        count("onwards_failovers_total", alias, "529", "dispatched"),
        Some(1.0)
    );
    assert_eq!(
        count("onwards_fallback_rescues_total", alias, "529", "dispatched"),
        Some(1.0)
    );
}

#[tokio::test]
async fn failing_over_without_an_answer_is_not_a_rescue() {
    let alias = "exhausted";
    let client = HostClient::new(529, 529);
    let server = server(alias, client.clone());

    let response = post(&server, alias, false).await;

    assert!(!response.status_code().is_success());
    assert_eq!(client.calls().len(), 2);
    assert_eq!(
        count("onwards_failovers_total", alias, "529", "realtime"),
        Some(2.0)
    );
    assert!(!counted("onwards_fallback_rescues_total", alias));
}

#[tokio::test]
async fn a_first_attempt_success_counts_nothing() {
    let alias = "first-try";
    let server = server(alias, HostClient::new(200, 200));

    post(&server, alias, false).await.assert_status_ok();

    assert!(!counted("onwards_failovers_total", alias));
    assert!(!counted("onwards_fallback_rescues_total", alias));
}

#[tokio::test]
async fn failures_without_a_status_are_named() {
    let alias = "rescued-network";
    let server = server(alias, HostClient::new(502, 200));

    post(&server, alias, false).await.assert_status_ok();

    assert_eq!(
        count(
            "onwards_fallback_rescues_total",
            alias,
            "network_error",
            "realtime"
        ),
        Some(1.0)
    );
}
