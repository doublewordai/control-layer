//! Requests dwctl's own inference layers refuse are counted with onwards'
//! (`onwards_rejections_total`) through the full router.

use crate::test::utils::create_test_config;
use sqlx::PgPool;

#[dwctl_test_macros::test]
async fn test_dwctl_layer_rejections_are_counted(pool: PgPool) {
    let mut config = create_test_config();
    config.onwards.strict_mode = true;
    config.enable_metrics = true;
    config.cache.enabled = false;
    config.limits.requests.max_body_size = 64;
    let app = crate::Application::new_with_pool(config, Some(pool), None)
        .await
        .expect("Failed to create application");
    let (server, _services) = app.into_test_server();

    // Body limit, outermost.
    let response = server.post("/ai/v1/chat/completions").bytes(vec![b'x'; 65].into()).await;
    response.assert_status(axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    // Inference routing, before the body is read.
    let response = server
        .post("/ai/v1/chat/completions")
        .add_header("Model-Override", "other")
        .json(&serde_json::json!({"model": "m"}))
        .await;
    response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    // Inference routing, after the body is parsed.
    let response = server
        .post("/ai/v1/responses")
        .json(&serde_json::json!({"model": "m", "include": ["x"]}))
        .await;
    response.assert_status(axum::http::StatusCode::BAD_REQUEST);

    let metrics = server.get("/internal/metrics").await.text();
    let count = |code: &str, status: &str| {
        metrics
            .lines()
            .filter(|line| {
                line.starts_with("onwards_rejections_total{")
                    && line.contains(&format!("code=\"{code}\""))
                    && line.contains(&format!("status=\"{status}\""))
                    && line.contains("traffic=\"realtime\"")
            })
            .filter_map(|line| line.rsplit(' ').next()?.parse::<f64>().ok())
            .sum::<f64>()
    };
    assert_eq!(count("request_too_large", "413"), 1.0, "{metrics}");
    assert_eq!(count("unsupported_header", "400"), 1.0, "{metrics}");
    assert_eq!(count("invalid_parameter", "400"), 1.0, "{metrics}");
}
