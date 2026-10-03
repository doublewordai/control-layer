//! Zero-valued 5xx series for management routes.
//!
//! axum-prometheus creates each `{prefix}_http_requests_total` series on its first increment, and
//! PromQL `increase()` cannot see a series go from absent to 1. A route's first 5xx on a pod is
//! therefore invisible to rate-based alerts. This module registers the 500 and 503 series at
//! 0 for every management route in the OpenAPI docs when the router is built, and for any other
//! management route on its first request, so a first error is a visible 0 -> 1 step.

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use metrics::counter;
use std::sync::Once;
use utoipa::OpenApi;
use utoipa::openapi::path::PathItem;

use crate::openapi::{AdminApiDoc, AiApiDoc};

/// 500 is the bug signal; 503 is the retryable "dependency unavailable" response on uploads.
const STATUSES: [&str; 2] = ["500", "503"];

/// Routes covered by the control-layer system-error alerts.
fn is_management_route(endpoint: &str) -> bool {
    endpoint.starts_with("/admin/") || endpoint.starts_with("/ai/v1/files") || endpoint.starts_with("/ai/v1/batches")
}

fn register(method: &str, endpoint: &str) {
    for status in STATUSES {
        // Same metric name and label keys as axum-prometheus, so this is the series it increments.
        counter!(
            axum_prometheus::utils::requests_total_name(),
            "method" => method.to_owned(),
            "status" => status,
            "endpoint" => endpoint.to_owned()
        )
        .increment(0);
    }
}

/// `(method, endpoint)` for every management route in the OpenAPI docs, with each doc's server
/// prefix applied so the endpoint matches axum's `MatchedPath`.
fn documented_routes() -> Vec<(&'static str, String)> {
    let mut routes = Vec::new();
    for doc in [AdminApiDoc::openapi(), AiApiDoc::openapi()] {
        let prefix = doc
            .servers
            .as_ref()
            .and_then(|s| s.first())
            .map(|s| s.url.clone())
            .unwrap_or_default();
        for (path, item) in &doc.paths.paths {
            let endpoint = format!("{prefix}{path}");
            if is_management_route(&endpoint) {
                routes.extend(methods(item).map(|m| (m, endpoint.clone())));
            }
        }
    }
    routes
}

fn methods(item: &PathItem) -> impl Iterator<Item = &'static str> + '_ {
    [
        ("GET", &item.get),
        ("POST", &item.post),
        ("PUT", &item.put),
        ("PATCH", &item.patch),
        ("DELETE", &item.delete),
    ]
    .into_iter()
    .filter(|(_, op)| op.is_some())
    .map(|(method, _)| method)
}

/// Register the series for every documented management route. Call after the Prometheus layer is
/// built, so the metric name carries its prefix.
pub fn register_documented_routes() {
    static REGISTERED: Once = Once::new();
    REGISTERED.call_once(|| {
        for (method, endpoint) in documented_routes() {
            register(method, &endpoint);
        }
    });
}

/// Covers management routes missing from the OpenAPI docs, from their first request.
pub async fn register_error_series(req: Request, next: Next) -> Response {
    if let Some(path) = req.extensions().get::<MatchedPath>()
        && is_management_route(path.as_str())
    {
        register(req.method().as_str(), path.as_str());
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

    #[test]
    fn documented_routes_are_management_routes_with_their_server_prefix() {
        let routes = documented_routes();
        assert!(routes.iter().any(|(m, e)| *m == "POST" && e == "/ai/v1/files"), "{routes:?}");
        assert!(routes.iter().any(|(_, e)| e.starts_with("/admin/api/v1/")), "{routes:?}");
        assert!(routes.iter().all(|(_, e)| is_management_route(e)), "{routes:?}");
        assert!(!routes.iter().any(|(_, e)| e == "/ai/v1/chat/completions"), "{routes:?}");
    }
}
