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

fn targets(model: &str) -> Targets {
    Targets::from_config(
        serde_json::from_value(json!({
            "auth": {
                "global_keys": [],
                "key_definitions": {
                    "acme_backend": { "key": "sk-acme-1", "labels": { "account": "acme", "account_name": "acme.example" } },
                    "acme_worker": { "key": "sk-acme-2", "labels": { "account": "acme", "account_name": "acme.example" } },
                    "globex_backend": { "key": "sk-globex-1", "labels": { "account": "globex" } }
                }
            },
            "targets": {
                model: {
                    "inflight_limit": 1,
                    "account_inflight_limits": { "globex": 3 },
                    "providers": [{ "url": "https://upstream.example.com/v1/" }]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap()
}

fn router(model: &str) -> axum::Router {
    LazyLock::force(&METRICS);
    build_router(AppState::with_client(
        targets(model),
        MockHttpClient::new(StatusCode::OK, COMPLETION),
    ))
}

fn chat(model: &str, key: &str) -> Request {
    Request::builder()
        .uri("/v1/chat/completions")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::from(
            json!({"model": model, "messages": []}).to_string(),
        ))
        .unwrap()
}

fn value(metric: &str, labels: &[(&str, &str)]) -> Option<f64> {
    let series = format!("{metric}{{");
    METRICS
        .render()
        .lines()
        .find(|line| {
            line.starts_with(&series)
                && labels
                    .iter()
                    .all(|(name, value)| line.contains(&format!("{name}=\"{value}\"")))
        })
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse().ok())
}

#[tokio::test]
async fn inflight_checks_and_refusals_are_counted_per_account() {
    let model = "inflight-counts-model";
    let router = router(model);
    let count = |metric: &str, account: &str| {
        value(metric, &[("model", model), ("account", account)]).unwrap_or(0.0)
    };
    for account in ["acme", "globex"] {
        assert_eq!(count("onwards_inflight_limit_checks_total", account), 0.0);
        assert_eq!(count("onwards_inflight_limit_refusals_total", account), 0.0);
    }

    let held = router
        .clone()
        .oneshot(chat(model, "sk-acme-1"))
        .await
        .unwrap();
    assert_eq!(held.status(), StatusCode::OK);
    assert_eq!(count("onwards_inflight_limit_checks_total", "acme"), 1.0);
    assert_eq!(count("onwards_inflight_limit_refusals_total", "acme"), 0.0);

    let refused = router
        .clone()
        .oneshot(chat(model, "sk-acme-2"))
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(count("onwards_inflight_limit_checks_total", "acme"), 2.0);
    assert_eq!(count("onwards_inflight_limit_refusals_total", "acme"), 1.0);

    let other_account = router
        .clone()
        .oneshot(chat(model, "sk-globex-1"))
        .await
        .unwrap();
    assert_eq!(other_account.status(), StatusCode::OK);
    assert_eq!(count("onwards_inflight_limit_checks_total", "globex"), 1.0);
    assert_eq!(
        count("onwards_inflight_limit_refusals_total", "globex"),
        0.0
    );
    assert_eq!(count("onwards_inflight_limit_refusals_total", "acme"), 1.0);
}

#[tokio::test]
async fn inflight_series_carry_the_account_name_and_the_account_limit() {
    let model = "inflight-names-model";
    let router = router(model);
    let acme = [
        ("model", model),
        ("account", "acme"),
        ("account_name", "acme.example"),
    ];
    let globex = [
        ("model", model),
        ("account", "globex"),
        ("account_name", ""),
    ];
    assert_eq!(value("onwards_inflight_account_limit", &acme), None);
    assert_eq!(value("onwards_inflight_account_limit", &globex), None);

    let held = router
        .clone()
        .oneshot(chat(model, "sk-acme-1"))
        .await
        .unwrap();
    let refused = router
        .clone()
        .oneshot(chat(model, "sk-acme-2"))
        .await
        .unwrap();
    let other_account = router
        .clone()
        .oneshot(chat(model, "sk-globex-1"))
        .await
        .unwrap();
    assert_eq!(held.status(), StatusCode::OK);
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(other_account.status(), StatusCode::OK);

    assert_eq!(
        value("onwards_inflight_limit_checks_total", &acme),
        Some(2.0)
    );
    assert_eq!(
        value("onwards_inflight_limit_refusals_total", &acme),
        Some(1.0)
    );
    assert_eq!(value("onwards_inflight_account_limit", &acme), Some(1.0));
    assert_eq!(
        value("onwards_inflight_limit_checks_total", &globex),
        Some(1.0)
    );
    assert_eq!(value("onwards_inflight_account_limit", &globex), Some(3.0));
}
