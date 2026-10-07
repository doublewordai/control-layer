use std::sync::LazyLock;

use axum::http::StatusCode;
use axum_prometheus::metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use axum_test::TestServer;
use onwards::{
    AppState, build_router, strict::build_strict_router, target::Targets,
    test_utils::MockHttpClient,
};
use serde_json::{Value, json};

static METRICS: LazyLock<PrometheusHandle> = LazyLock::new(|| {
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    metrics::set_global_recorder(recorder).unwrap();
    handle
});

const COMPLETION: &str = r#"{"id":"chatcmpl-1","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"Hi"},"finish_reason":"stop"}]}"#;
const UPSTREAM_ERROR: &str =
    r#"{"error":{"message":"bad","type":"invalid_request_error","code":null}}"#;
const EXEMPT_HEADER: &str = "x-batch-created-at";

fn targets(alias: &str, strict_mode: bool) -> Targets {
    Targets::from_config(
        serde_json::from_value(json!({
            "strict_mode": strict_mode,
            "targets": {alias: {
                "url": "https://upstream.example.com/v1/",
                "sanitize_response": true
            }}
        }))
        .unwrap(),
    )
    .unwrap()
}

fn state(alias: &str, strict_mode: bool, mock: MockHttpClient) -> AppState<MockHttpClient> {
    LazyLock::force(&METRICS);
    AppState::with_client(targets(alias, strict_mode), mock)
        .with_first_token_timeout_exempt_header(EXEMPT_HEADER)
}

fn strict_server(alias: &str, mock: MockHttpClient) -> TestServer {
    TestServer::new(build_strict_router(state(alias, true, mock))).unwrap()
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

fn count(labels: &[(&str, &str)]) -> Option<f64> {
    METRICS
        .render()
        .lines()
        .find(|line| {
            line.starts_with("onwards_rejections_total{")
                && labels
                    .iter()
                    .all(|(name, value)| line.contains(&format!("{name}=\"{value}\"")))
        })
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse().ok())
}

#[tokio::test]
async fn reasoning_rejections_are_counted_by_model_and_code() {
    let alias = "reasoning-rejection-model";
    let mock = MockHttpClient::new(StatusCode::OK, COMPLETION);
    let server = strict_server(alias, mock.clone());
    let body = request(alias, json!({"chat_template_kwargs": {"custom_flag": 1}}));

    let response = server.post("/chat/completions").json(&body).await;

    response.assert_status(StatusCode::BAD_REQUEST);
    let code = response.json::<Value>()["error"]["code"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(mock.get_requests().is_empty());
    assert_eq!(
        count(&[
            ("model", alias),
            ("status", "400"),
            ("code", &code),
            ("traffic", "realtime"),
        ]),
        Some(1.0)
    );
}

#[tokio::test]
async fn unknown_models_are_counted_without_the_model_name() {
    let mock = MockHttpClient::new(StatusCode::OK, COMPLETION);
    let server = strict_server("configured-model", mock);
    let body = request("unconfigured-model", json!({}));

    let response = server.post("/chat/completions").json(&body).await;

    response.assert_status(StatusCode::NOT_FOUND);
    assert!(!METRICS.render().contains("model=\"unconfigured-model\""));
    assert_eq!(
        count(&[
            ("model", ""),
            ("status", "404"),
            ("code", "model_not_found")
        ]),
        Some(1.0)
    );
}

#[tokio::test]
async fn schema_errors_are_counted_before_forwarding() {
    let alias = "schema-rejection-model";
    let mock = MockHttpClient::new(StatusCode::OK, COMPLETION);
    let server = strict_server(alias, mock.clone());

    let response = server
        .post("/chat/completions")
        .add_header(EXEMPT_HEADER, "1700000000")
        .json(&json!({"model": alias, "messages": "not a list"}))
        .await;

    response.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(mock.get_requests().is_empty());
    assert_eq!(
        count(&[
            ("model", alias),
            ("status", "422"),
            ("code", "schema_mismatch"),
            ("traffic", "dispatched"),
        ]),
        Some(1.0)
    );
}

#[tokio::test]
async fn completions_parameter_refusals_are_counted() {
    let alias = "completions-rejection-model";
    let mock = MockHttpClient::new(StatusCode::OK, COMPLETION);
    let server = strict_server(alias, mock.clone());

    let response = server
        .post("/completions")
        .json(&json!({"model": alias, "prompt": "Hi", "reasoning_effort": "low"}))
        .await;

    response.assert_status(StatusCode::BAD_REQUEST);
    assert!(mock.get_requests().is_empty());
    assert_eq!(
        count(&[("model", alias), ("code", "unsupported_parameter")]),
        Some(1.0)
    );
}

#[tokio::test]
async fn serving_class_suffixes_are_counted_under_the_alias() {
    let alias = "suffixed-rejection-model";
    let mock = MockHttpClient::new(StatusCode::OK, COMPLETION);
    let server = strict_server(alias, mock.clone());

    let response = server
        .post("/completions")
        .json(&json!({
            "model": format!("{alias}:interactive"),
            "prompt": "Hi",
            "reasoning_effort": "low"
        }))
        .await;

    response.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        count(&[("model", alias), ("code", "unsupported_parameter")]),
        Some(1.0)
    );
}

#[tokio::test]
async fn responses_schema_errors_keep_the_model() {
    let alias = "responses-schema-model";
    let mock = MockHttpClient::new(StatusCode::OK, "{}");
    let server = strict_server(alias, mock.clone());

    let response = server
        .post("/responses")
        .json(&json!({"model": alias, "input": 42}))
        .await;

    response.assert_status(StatusCode::BAD_REQUEST);
    assert!(mock.get_requests().is_empty());
    assert_eq!(
        count(&[("model", alias), ("code", "schema_mismatch")]),
        Some(1.0)
    );
}

#[tokio::test]
async fn responses_bodies_not_sent_as_json_are_counted_as_such() {
    let alias = "responses-content-type-model";
    let mock = MockHttpClient::new(StatusCode::OK, "{}");
    let server = strict_server(alias, mock.clone());

    let response = server
        .post("/responses")
        .text(json!({"model": alias, "input": "Hello"}).to_string())
        .await;

    response.assert_status(StatusCode::BAD_REQUEST);
    assert!(mock.get_requests().is_empty());
    assert_eq!(
        count(&[("model", alias), ("code", "invalid_content_type")]),
        Some(1.0)
    );
}

#[tokio::test]
async fn upstream_client_errors_are_not_counted() {
    let alias = "upstream-error-model";
    let mock = MockHttpClient::new(StatusCode::BAD_REQUEST, UPSTREAM_ERROR);
    let server = TestServer::new(build_router(state(alias, false, mock.clone()))).unwrap();

    let response = server
        .post("/v1/chat/completions")
        .json(&request(alias, json!({})))
        .await;

    response.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(response.json::<Value>()["error"]["code"], "upstream_error");
    assert_eq!(mock.get_requests().len(), 1);
    assert_eq!(count(&[("model", alias)]), None);
}
