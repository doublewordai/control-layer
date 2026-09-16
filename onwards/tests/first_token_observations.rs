use std::{collections::HashMap, sync::LazyLock, time::Duration};

use axum::http::StatusCode;
use axum_prometheus::metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use axum_test::TestServer;
use onwards::{
    AppState, build_router,
    target::{RequestClass, TargetPools, Targets},
    test_utils::MockHttpClient,
};
use serde_json::json;

static METRICS: LazyLock<PrometheusHandle> = LazyLock::new(|| {
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    metrics::set_global_recorder(recorder).unwrap();
    handle
});

const CONTENT: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";

fn config(alias: &str, strict: bool) -> serde_json::Value {
    json!({
        "strict_mode": strict,
        "targets": {
            alias: {
                "strategy": "priority",
                "fallback": {"enabled": true, "on_status": [502], "first_token_timeout_ms": 100},
                "providers": [
                    {"url": "https://preferred.example.com"},
                    {"url": "https://alternate.example.com"}
                ]
            }
        }
    })
}

fn targets(alias: &str, strict: bool) -> Targets {
    Targets::from_config(serde_json::from_value(config(alias, strict)).unwrap()).unwrap()
}

fn server(alias: &str, mock: MockHttpClient, strict: bool) -> TestServer {
    LazyLock::force(&METRICS);
    TestServer::new(build_router(AppState::with_client(
        targets(alias, strict),
        mock,
    )))
    .unwrap()
}

fn value(metric: &str, alias: &str, pool: &str, role: &str) -> Option<f64> {
    METRICS
        .render()
        .lines()
        .find(|line| {
            line.starts_with(&format!("{metric}{{"))
                && line.contains(&format!("model=\"{alias}\""))
                && line.contains(&format!("pool=\"{pool}\""))
                && line.contains(&format!("role=\"{role}\""))
        })
        .map(|line| line.split_whitespace().last().unwrap().parse().unwrap())
}

#[tokio::test]
async fn observes_first_data_once_and_preserves_stream() {
    let alias = "first-data";
    let body = format!(": keep-alive\n\n{CONTENT}{CONTENT}data: [DONE]\n\n");
    let mock = MockHttpClient::new_streaming(StatusCode::OK, vec![body.clone()]);
    let server = server(alias, mock.clone(), true);
    let response = server
        .post("/v1/chat/completions")
        .json(&json!({"model": alias, "stream": true}))
        .await;
    response.assert_status_ok();
    assert_eq!(response.text(), body);
    assert_eq!(mock.get_requests().len(), 1);
    assert_eq!(
        value(
            "onwards_first_token_seconds_count",
            alias,
            "default",
            "preferred"
        ),
        Some(1.0)
    );
    assert_eq!(
        value(
            "onwards_first_token_breaches_total",
            alias,
            "default",
            "preferred"
        ),
        None
    );
}

#[tokio::test]
async fn done_only_is_neither_a_sample_nor_retryable_empty() {
    let alias = "done-only";
    let body = ": keep-alive\n\ndata: [DONE]\n\n";
    let mock = MockHttpClient::new_streaming(StatusCode::OK, vec![body.to_string()]);
    let server = server(alias, mock.clone(), true);
    let response = server
        .post("/v1/chat/completions")
        .json(&json!({"model": alias, "stream": true}))
        .await;
    response.assert_status_ok();
    assert_eq!(response.text(), body);
    assert_eq!(mock.get_requests().len(), 1);
    assert_eq!(
        value(
            "onwards_first_token_seconds_count",
            alias,
            "default",
            "preferred"
        ),
        None
    );
    assert_eq!(
        value(
            "onwards_first_token_breaches_total",
            alias,
            "default",
            "preferred"
        ),
        None
    );
}

#[tokio::test]
async fn body_deadline_is_censored_and_fallback_has_its_own_role() {
    let alias = "body-deadline";
    let mock = MockHttpClient::new_timed_streaming_sequence(
        StatusCode::OK,
        vec![
            vec![
                (Duration::ZERO, ": keep-alive\n\n".to_string()),
                (Duration::from_secs(30), CONTENT.to_string()),
            ],
            vec![(Duration::from_millis(20), CONTENT.to_string())],
        ],
    );
    let server = server(alias, mock.clone(), true);
    let response = server
        .post("/v1/chat/completions")
        .json(&json!({"model": alias, "stream": true}))
        .await;
    response.assert_status_ok();
    assert_eq!(response.text(), CONTENT);
    assert_eq!(mock.get_requests().len(), 2);
    assert_eq!(
        value(
            "onwards_first_token_breaches_total",
            alias,
            "default",
            "preferred"
        ),
        Some(1.0)
    );
    assert_eq!(
        value(
            "onwards_first_token_seconds_count",
            alias,
            "default",
            "preferred"
        ),
        None
    );
    assert_eq!(
        value(
            "onwards_first_token_breaches_total",
            alias,
            "default",
            "alternate"
        ),
        None
    );
    assert_eq!(
        value(
            "onwards_first_token_seconds_count",
            alias,
            "default",
            "alternate"
        ),
        Some(1.0)
    );
    assert!(
        value(
            "onwards_first_token_seconds_sum",
            alias,
            "default",
            "alternate"
        )
        .unwrap()
            >= 0.020
    );
}

#[tokio::test]
async fn non_strict_sse_and_non_sse_have_no_first_token_samples() {
    for (alias, strict, mock) in [
        (
            "non-strict",
            false,
            MockHttpClient::new_streaming(StatusCode::OK, vec![CONTENT.to_string()]),
        ),
        (
            "non-sse",
            true,
            MockHttpClient::new(StatusCode::OK, "{\"choices\":[]}"),
        ),
    ] {
        let server = server(alias, mock.clone(), strict);
        server
            .post("/v1/chat/completions")
            .json(&json!({"model": alias, "stream": true}))
            .await
            .assert_status_ok();
        assert_eq!(mock.get_requests().len(), 1);
        assert_eq!(
            value(
                "onwards_first_token_seconds_count",
                alias,
                "default",
                "preferred"
            ),
            None
        );
        assert_eq!(
            value(
                "onwards_first_token_breaches_total",
                alias,
                "default",
                "preferred"
            ),
            None
        );
    }
}

#[tokio::test]
async fn upstream_errors_do_not_become_latency_observations() {
    for (alias, mock) in [
        (
            "http-error",
            MockHttpClient::new(StatusCode::BAD_GATEWAY, "upstream unavailable"),
        ),
        (
            "embedded-error",
            MockHttpClient::new_streaming(
                StatusCode::OK,
                vec![
                    "data: {\"error\":{\"code\":502,\"message\":\"unavailable\"}}\n\n".to_string(),
                ],
            ),
        ),
        (
            "empty-stream",
            MockHttpClient::new_streaming(StatusCode::OK, vec![]),
        ),
    ] {
        let server = server(alias, mock.clone(), true);
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({"model": alias, "stream": true}))
            .await;
        assert!(response.status_code().is_server_error());
        assert_eq!(mock.get_requests().len(), 2);
        for role in ["preferred", "alternate"] {
            assert_eq!(
                value("onwards_first_token_seconds_count", alias, "default", role),
                None
            );
            assert_eq!(
                value("onwards_first_token_breaches_total", alias, "default", role),
                None
            );
        }
    }
}

#[tokio::test]
async fn named_pools_export_separate_series() {
    let alias = "named-pools";
    LazyLock::force(&METRICS);
    let targets = targets(alias, true);
    let default = targets
        .targets
        .get(alias)
        .unwrap()
        .resolve(RequestClass::Normal)
        .clone();
    targets.targets.insert(
        alias.to_string(),
        TargetPools::with_pools(
            default.clone(),
            HashMap::from([("completions".to_string(), default)]),
        ),
    );
    let mock = MockHttpClient::new_streaming(StatusCode::OK, vec![CONTENT.to_string()]);
    let server = TestServer::new(build_router(AppState::with_client(targets, mock))).unwrap();
    for path in ["/v1/chat/completions", "/v1/completions"] {
        server
            .post(path)
            .json(&json!({"model": alias, "stream": true}))
            .await
            .assert_status_ok();
    }
    for pool in ["default", "completions"] {
        assert_eq!(
            value(
                "onwards_first_token_seconds_count",
                alias,
                pool,
                "preferred"
            ),
            Some(1.0)
        );
    }
}

#[derive(Debug, Clone)]
struct HeaderClient {
    mock: MockHttpClient,
    network_error: bool,
    header_delay: Option<Duration>,
}

#[async_trait::async_trait]
impl onwards::client::HttpClient for HeaderClient {
    async fn request(
        &self,
        req: axum::extract::Request,
    ) -> Result<axum::response::Response, Box<dyn std::error::Error + Send + Sync>> {
        if req.uri().host() == Some("preferred.example.com") {
            if let Some(delay) = self.header_delay {
                tokio::time::sleep(delay).await;
            } else if self.network_error {
                return Err(std::io::Error::other("connection failed").into());
            } else {
                return std::future::pending().await;
            }
        }
        self.mock.request(req).await
    }
}

#[tokio::test]
async fn header_deadline_counts_but_network_and_request_timeouts_do_not() {
    LazyLock::force(&METRICS);
    for (alias, network_error, request_timeout, strict, expected_breaches) in [
        ("header-deadline", false, None, true, Some(1.0)),
        ("non-strict-header-deadline", false, None, false, Some(1.0)),
        ("network-error", true, None, true, None),
        ("request-timeout", false, Some(0), true, None),
    ] {
        let mut config = config(alias, strict);
        if let Some(seconds) = request_timeout {
            config["targets"][alias]["providers"][0]["request_timeout_secs"] = json!(seconds);
        }
        let targets = Targets::from_config(serde_json::from_value(config).unwrap()).unwrap();
        let client = HeaderClient {
            mock: MockHttpClient::new_streaming(StatusCode::OK, vec![CONTENT.to_string()]),
            network_error,
            header_delay: None,
        };
        let server = TestServer::new(build_router(AppState::with_client(targets, client))).unwrap();
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({"model": alias, "stream": true}))
            .await;
        response.assert_status_ok();
        assert_eq!(response.text(), CONTENT);
        assert_eq!(
            value(
                "onwards_first_token_breaches_total",
                alias,
                "default",
                "preferred"
            ),
            expected_breaches
        );
        assert_eq!(
            value(
                "onwards_first_token_seconds_count",
                alias,
                "default",
                "preferred"
            ),
            None
        );
        assert_eq!(
            value(
                "onwards_first_token_seconds_count",
                alias,
                "default",
                "alternate"
            ),
            strict.then_some(1.0)
        );
    }
}

#[tokio::test]
async fn latency_includes_both_headers_and_first_frame_wait() {
    let alias = "headers-and-frame";
    LazyLock::force(&METRICS);
    // Disarm failover: this test measures an observation, not a deadline.
    let mut config = config(alias, true);
    config["targets"][alias]["fallback"]["first_token_timeout_ms"] = json!(0);
    let targets = Targets::from_config(serde_json::from_value(config).unwrap()).unwrap();
    let client = HeaderClient {
        mock: MockHttpClient::new_delayed_streaming_sequence(
            StatusCode::OK,
            vec![(Duration::from_millis(20), vec![CONTENT.to_string()])],
        ),
        network_error: false,
        header_delay: Some(Duration::from_millis(20)),
    };
    let server = TestServer::new(build_router(AppState::with_client(targets, client))).unwrap();
    server
        .post("/v1/chat/completions")
        .json(&json!({"model": alias, "stream": true}))
        .await
        .assert_status_ok();
    assert_eq!(
        value(
            "onwards_first_token_seconds_count",
            alias,
            "default",
            "preferred"
        ),
        Some(1.0)
    );
    assert!(
        value(
            "onwards_first_token_seconds_sum",
            alias,
            "default",
            "preferred"
        )
        .unwrap()
            >= 0.040
    );
}

#[tokio::test]
async fn strict_stream_beyond_peek_limit_is_forwarded_without_a_sample() {
    let alias = "peek-limit";
    LazyLock::force(&METRICS);
    let mut config = config(alias, true);
    config["targets"][alias]["fallback"]["first_token_timeout_ms"] = json!(0);
    let targets = Targets::from_config(serde_json::from_value(config).unwrap()).unwrap();
    let body = format!("{}{CONTENT}", ": keep-alive\n\n".repeat(5));
    let mock = MockHttpClient::new_streaming(StatusCode::OK, vec![body.clone()]);
    let server =
        TestServer::new(build_router(AppState::with_client(targets, mock.clone()))).unwrap();
    let response = server
        .post("/v1/chat/completions")
        .json(&json!({"model": alias, "stream": true}))
        .await;
    response.assert_status_ok();
    assert_eq!(response.text(), body);
    assert_eq!(mock.get_requests().len(), 1);
    assert_eq!(
        value(
            "onwards_first_token_seconds_count",
            alias,
            "default",
            "preferred"
        ),
        None
    );
    assert_eq!(
        value(
            "onwards_first_token_breaches_total",
            alias,
            "default",
            "preferred"
        ),
        None
    );
}
