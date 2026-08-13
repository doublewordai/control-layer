pub mod analytics_handler;
pub mod batcher;
pub mod models;
pub mod serializers;
mod utils;

pub use analytics_handler::AnalyticsHandler;
pub use batcher::AnalyticsBatcher;
pub use models::{AiRequest, AiResponse, ParsedAIRequest};

use axum::{
    body::Body,
    extract::State,
    http::{HeaderName, Request},
    middleware::Next,
    response::Response,
};
use outlet_postgres::CapturePolicy;

use crate::config::RequestLoggingConfig;

pub(crate) fn build_capture_policy(config: &RequestLoggingConfig) -> anyhow::Result<(CapturePolicy, Option<HeaderName>)> {
    let mut policy = CapturePolicy::all();
    if let Some(headers) = &config.retained_request_headers {
        policy = policy.with_request_headers(headers)?;
    }
    if let Some(headers) = &config.retained_response_headers {
        policy = policy.with_response_headers(headers)?;
    }

    let subject_header = config
        .subject_header
        .as_deref()
        .map(|name| HeaderName::from_bytes(name.as_bytes()))
        .transpose()?;
    if let Some(header) = &subject_header {
        anyhow::ensure!(
            !["authorization", "proxy-authorization", "x-api-key", "api-key"]
                .iter()
                .any(|credential| header.as_str().eq_ignore_ascii_case(credential)),
            "subject_header must not use a credential header name"
        );
        anyhow::ensure!(
            !config.retained_request_headers.as_ref().is_some_and(|retained_headers| {
                retained_headers
                    .iter()
                    .any(|retained| retained.eq_ignore_ascii_case(header.as_str()))
            }),
            "subject_header must not also be retained as a request header"
        );
        policy = policy.with_subject_header(header.as_str())?;
    }

    Ok((policy, subject_header))
}

/// Remove the internal attribution carrier after Outlet has captured it and
/// before the request reaches protocol translation or an upstream service.
pub(crate) async fn strip_subject_carrier(State(subject_header): State<HeaderName>, mut request: Request<Body>, next: Next) -> Response {
    request.headers_mut().remove(subject_header);
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use axum::{Router, body::Body, http::Request, middleware, routing::post};
    use tower::ServiceExt;

    use super::build_capture_policy;
    use crate::config::RequestLoggingConfig;

    #[test]
    fn capture_policy_accepts_backwards_compatible_defaults() {
        assert!(build_capture_policy(&RequestLoggingConfig::default()).is_ok());
    }

    #[test]
    fn capture_policy_rejects_invalid_header_names() {
        let config = RequestLoggingConfig {
            retained_request_headers: Some(vec!["invalid header".to_owned()]),
            ..Default::default()
        };

        assert!(build_capture_policy(&config).is_err());
    }

    #[tokio::test]
    async fn subject_carrier_is_removed_before_the_inner_service() {
        let header = axum::http::HeaderName::from_static("x-example-subject");
        let inner_header = header.clone();
        let app = Router::new()
            .route(
                "/",
                post(move |request: Request<Body>| {
                    let inner_header = inner_header.clone();
                    async move { (!request.headers().contains_key(inner_header)).to_string() }
                }),
            )
            .layer(middleware::from_fn_with_state(header.clone(), super::strip_subject_carrier));
        let request = Request::post("/").header(header, "opaque-subject").body(Body::empty()).unwrap();

        let response = app.oneshot(request).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body, "true");
    }
}
