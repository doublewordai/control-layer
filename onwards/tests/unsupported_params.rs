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

const EXEMPT_HEADER: &str = "x-batch-created-at";

fn server(alias: &str, mock: MockHttpClient, rejected: &[&str]) -> TestServer {
    LazyLock::force(&METRICS);
    let state = AppState::with_client(targets(alias), mock)
        .with_first_token_timeout_exempt_header(EXEMPT_HEADER)
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
    count_with(alias, &[("param", param), ("action", action)])
}

fn count_with(alias: &str, labels: &[(&str, &str)]) -> Option<f64> {
    METRICS
        .render()
        .lines()
        .find(|line| {
            line.starts_with("onwards_unsupported_params_total{")
                && line.contains(&format!("model=\"{alias}\""))
                && labels
                    .iter()
                    .all(|(name, value)| line.contains(&format!("{name}=\"{value}\"")))
        })
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse().ok())
}

fn counted(alias: &str) -> bool {
    METRICS.render().lines().any(|line| {
        line.starts_with("onwards_unsupported_params_total{")
            && line.contains(&format!("model=\"{alias}\""))
    })
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
    assert!(!counted(alias));
}

#[tokio::test]
async fn thinking_only_template_args_are_forwarded_uncounted() {
    let alias = "thinking-args-model";
    let mock = MockHttpClient::new(StatusCode::OK, COMPLETION);
    let server = server(alias, mock.clone(), &["chat_template_args"]);
    let body = request(
        alias,
        json!({"chat_template_args": {"enable_thinking": false}}),
    );

    let response = server.post("/chat/completions").json(&body).await;

    response.assert_status_ok();
    assert_eq!(mock.get_requests().len(), 1);
    assert!(!counted(alias));
}

#[tokio::test]
async fn params_are_counted_before_reasoning_validation_refuses_the_request() {
    let alias = "kwargs-model";
    let mock = MockHttpClient::new(StatusCode::OK, COMPLETION);
    let server = server(alias, mock.clone(), &[]);
    let body = request(
        alias,
        json!({"logprobs": true, "chat_template_kwargs": {"custom_flag": 1}}),
    );

    let response = server.post("/chat/completions").json(&body).await;

    response.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        response.json::<Value>()["error"]["param"],
        "chat_template_kwargs"
    );
    assert!(mock.get_requests().is_empty());
    assert_eq!(count(alias, "logprobs", "logged"), Some(1.0));
}

#[tokio::test]
async fn batch_traffic_is_counted_as_dispatched() {
    let alias = "dispatched-model";
    let mock = MockHttpClient::new(StatusCode::OK, COMPLETION);
    let server = server(alias, mock.clone(), &[]);
    let body = request(alias, json!({"n": 2}));

    server
        .post("/chat/completions")
        .add_header(EXEMPT_HEADER, "1700000000")
        .json(&body)
        .await
        .assert_status_ok();
    server
        .post("/chat/completions")
        .json(&body)
        .await
        .assert_status_ok();

    assert_eq!(
        count_with(alias, &[("param", "n"), ("traffic", "dispatched")]),
        Some(1.0)
    );
    assert_eq!(
        count_with(alias, &[("param", "n"), ("traffic", "realtime")]),
        Some(1.0)
    );
}

#[tokio::test]
async fn native_responses_requests_are_not_checked() {
    let alias = "native-responses-model";
    let mock = MockHttpClient::new(StatusCode::OK, "{}");
    let server = server(alias, mock.clone(), &["top_logprobs"]);
    let body = json!({
        "model": alias,
        "input": "Hello",
        "include": ["message.output_text.logprobs"],
        "top_logprobs": 3
    });

    server.post("/responses").json(&body).await;

    assert_eq!(mock.get_requests().len(), 1);
    assert!(!counted(alias));
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
