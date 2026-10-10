use std::sync::LazyLock;

use axum::{body::Body, extract::Request, http::StatusCode};
use axum_prometheus::metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use onwards::{AppState, build_router, target::Targets, test_utils::MockHttpClient};
use serde_json::json;
use tower::ServiceExt;

static METRICS: LazyLock<PrometheusHandle> = LazyLock::new(|| {
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    metrics::set_global_recorder(recorder).unwrap();
    handle
});

const COMPLETION: &str = r#"{"id":"chatcmpl-1","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"Hi"},"finish_reason":"stop"}]}"#;
const MODEL: &str = "inflight-metrics-model";

fn targets() -> Targets {
    Targets::from_config(
        serde_json::from_value(json!({
            "auth": {
                "global_keys": [],
                "key_definitions": {
                    "acme_backend": { "key": "sk-acme-1", "labels": { "account": "acme" } },
                    "acme_worker": { "key": "sk-acme-2", "labels": { "account": "acme" } },
                    "globex_backend": { "key": "sk-globex-1", "labels": { "account": "globex" } }
                }
            },
            "targets": {
                MODEL: {
                    "inflight_limit": 1,
                    "providers": [{ "url": "https://upstream.example.com/v1/" }]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap()
}

fn chat(key: &str) -> Request {
    Request::builder()
        .uri("/v1/chat/completions")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::from(
            json!({"model": MODEL, "messages": []}).to_string(),
        ))
        .unwrap()
}

fn count(metric: &str, account: &str) -> f64 {
    let series = format!("{metric}{{");
    let model = format!("model=\"{MODEL}\"");
    let account = format!("account=\"{account}\"");
    METRICS
        .render()
        .lines()
        .find(|line| line.starts_with(&series) && line.contains(&model) && line.contains(&account))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0.0)
}

#[tokio::test]
async fn inflight_checks_and_refusals_are_counted_per_account() {
    LazyLock::force(&METRICS);
    let router = build_router(AppState::with_client(
        targets(),
        MockHttpClient::new(StatusCode::OK, COMPLETION),
    ));
    for account in ["acme", "globex"] {
        assert_eq!(count("onwards_inflight_limit_checks_total", account), 0.0);
        assert_eq!(count("onwards_inflight_limit_refusals_total", account), 0.0);
    }

    let held = router.clone().oneshot(chat("sk-acme-1")).await.unwrap();
    assert_eq!(held.status(), StatusCode::OK);
    assert_eq!(count("onwards_inflight_limit_checks_total", "acme"), 1.0);
    assert_eq!(count("onwards_inflight_limit_refusals_total", "acme"), 0.0);

    let refused = router.clone().oneshot(chat("sk-acme-2")).await.unwrap();
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(count("onwards_inflight_limit_checks_total", "acme"), 2.0);
    assert_eq!(count("onwards_inflight_limit_refusals_total", "acme"), 1.0);

    let other_account = router.clone().oneshot(chat("sk-globex-1")).await.unwrap();
    assert_eq!(other_account.status(), StatusCode::OK);
    assert_eq!(count("onwards_inflight_limit_checks_total", "globex"), 1.0);
    assert_eq!(
        count("onwards_inflight_limit_refusals_total", "globex"),
        0.0
    );
    assert_eq!(count("onwards_inflight_limit_refusals_total", "acme"), 1.0);
}
