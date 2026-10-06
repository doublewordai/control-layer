use std::sync::LazyLock;

use axum::http::StatusCode;
use axum_prometheus::metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use axum_test::TestServer;
use onwards::{AppState, strict::build_strict_router, target::Targets, test_utils::MockHttpClient};
use serde_json::{Value, json};

static METRICS: LazyLock<PrometheusHandle> = LazyLock::new(|| {
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    metrics::set_global_recorder(recorder).unwrap();
    handle
});

const COMPLETION: &str = r#"{"id":"chatcmpl-1","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"Hi"},"finish_reason":"stop"}]}"#;

fn targets(alias: &str) -> Targets {
    Targets::from_config(
        serde_json::from_value(json!({
            "strict_mode": true,
            "targets": {alias: {"url": "https://upstream.example.com/v1/"}}
        }))
        .unwrap(),
    )
    .unwrap()
}

fn server(alias: &str, mock: MockHttpClient, rejected: &[&str]) -> TestServer {
    LazyLock::force(&METRICS);
    let state = AppState::with_client(targets(alias), mock)
        .with_rejected_params(rejected)
        .unwrap();
    TestServer::new(build_strict_router(state)).unwrap()
}

fn request(alias: &str, fields: Value) -> Value {
    let mut body = json!({
        "model": alias,
        "messages": [{"role": "user", "content": "Hello"}]
    });
    body.as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    body
}

fn count(alias: &str, param: &str, action: &str) -> Option<f64> {
    METRICS
        .render()
        .lines()
        .find(|line| {
            line.starts_with("onwards_unsupported_params_total{")
                && line.contains(&format!("model=\"{alias}\""))
                && line.contains(&format!("param=\"{param}\""))
                && line.contains(&format!("action=\"{action}\""))
        })
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse().ok())
}

#[tokio::test]
async fn flagged_params_are_counted_and_forwarded_unchanged() {
    let alias = "log-only-model";
    let mock = MockHttpClient::new(StatusCode::OK, COMPLETION);
    let server = server(alias, mock.clone(), &[]);
    let body = request(alias, json!({"logprobs": true, "top_logprobs": 3}));

    let response = server.post("/chat/completions").json(&body).await;

    response.assert_status_ok();
    let forwarded = mock.get_requests();
    assert_eq!(forwarded.len(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&forwarded[0].body).unwrap(),
        body
    );
    assert_eq!(count(alias, "logprobs", "logged"), Some(1.0));
    assert_eq!(count(alias, "top_logprobs", "logged"), Some(1.0));
}

#[tokio::test]
async fn rejected_params_get_a_400_and_are_not_forwarded() {
    let alias = "reject-model";
    let mock = MockHttpClient::new(StatusCode::OK, COMPLETION);
    let server = server(alias, mock.clone(), &["logprobs"]);
    let body = request(alias, json!({"logprobs": true, "top_logprobs": 3}));

    let response = server.post("/chat/completions").json(&body).await;

    response.assert_status(StatusCode::BAD_REQUEST);
    let error = response.json::<Value>()["error"].clone();
    assert_eq!(error["message"], "Unsupported parameter(s): `logprobs`");
    assert_eq!(error["param"], "logprobs");
    assert_eq!(error["code"], "unsupported_parameter");
    assert!(mock.get_requests().is_empty());
    assert_eq!(count(alias, "logprobs", "rejected"), Some(1.0));
    assert_eq!(count(alias, "top_logprobs", "logged"), Some(1.0));
}

#[tokio::test]
async fn neutral_values_are_neither_counted_nor_rejected() {
    let alias = "neutral-model";
    let mock = MockHttpClient::new(StatusCode::OK, COMPLETION);
    let server = server(alias, mock.clone(), &["logprobs", "top_logprobs", "n"]);
    let body = request(alias, json!({"n": 1, "logprobs": false, "top_logprobs": 0}));

    let response = server.post("/chat/completions").json(&body).await;

    response.assert_status_ok();
    assert_eq!(mock.get_requests().len(), 1);
    assert!(
        !METRICS
            .render()
            .lines()
            .any(|line| line.starts_with("onwards_unsupported_params_total{")
                && line.contains(&format!("model=\"{alias}\"")))
    );
}

#[test]
fn unknown_rejected_param_is_a_configuration_error() {
    let result = AppState::with_client(
        targets("any-model"),
        MockHttpClient::new(StatusCode::OK, ""),
    )
    .with_rejected_params(["logprobs", "top_k"]);
    assert_eq!(
        result.unwrap_err(),
        "unknown parameter in the reject list: top_k"
    );
}
