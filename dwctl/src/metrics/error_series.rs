//! Zero-valued 5xx series for management routes.
//!
//! axum-prometheus creates each `{prefix}_http_requests_total` series on its first increment, and
//! PromQL `increase()` cannot see a series go from absent to 1. A route's first 5xx on a pod is
//! therefore invisible to rate-based alerts. This middleware registers the 500 and 503 series at
//! 0 when a management route serves a request, so a first error is a visible 0 -> 1 step.

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use metrics::counter;

/// 500 is the bug signal; 503 is the retryable "dependency unavailable" response on uploads.
const STATUSES: [&str; 2] = ["500", "503"];

/// Routes covered by the control-layer system-error alerts.
fn is_management_route(endpoint: &str) -> bool {
    endpoint.starts_with("/admin/") || endpoint.starts_with("/ai/v1/files") || endpoint.starts_with("/ai/v1/batches")
}

pub async fn register_error_series(req: Request, next: Next) -> Response {
    if let Some(path) = req.extensions().get::<MatchedPath>()
        && is_management_route(path.as_str())
    {
        let method = req.method().as_str().to_owned();
        for status in STATUSES {
            // Same metric name and label keys as axum-prometheus, so this is the series it increments.
            counter!(
                axum_prometheus::utils::requests_total_name(),
                "method" => method.clone(),
                "status" => status,
                "endpoint" => path.as_str().to_owned()
            )
            .increment(0);
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use tower::ServiceExt;

    #[tokio::test]
    async fn management_route_registers_zero_error_series() {
        let handle = crate::get_or_install_prometheus_handle();
        let app = Router::new()
            .route("/admin/api/v1/error-series-test/{id}", get(|| async { "ok" }))
            .route("/ai/v1/error-series-test-inference", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(register_error_series));

        for uri in ["/admin/api/v1/error-series-test/7", "/ai/v1/error-series-test-inference"] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert!(res.status().is_success());
        }

        let output = handle.render();
        let name = axum_prometheus::utils::requests_total_name();
        let lines: Vec<_> = output
            .lines()
            .filter(|l| l.starts_with(name) && l.contains("error-series-test"))
            .collect();
        for status in STATUSES {
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains(r#"endpoint="/admin/api/v1/error-series-test/{id}""#)
                        && l.contains(&format!(r#"status="{status}""#))
                        && l.contains(r#"method="GET""#)
                        && l.ends_with(" 0")),
                "missing zero {status} series in {lines:?}"
            );
        }
        assert!(
            !lines.iter().any(|l| l.contains("error-series-test-inference")),
            "inference routes stay out of scope: {lines:?}"
        );
    }
}
