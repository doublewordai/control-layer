//! Bound inference bodies before middleware parses, copies, or logs them.

use axum::{
    Json,
    body::{Body, Bytes, to_bytes},
    extract::State,
    http::{HeaderMap, Request, StatusCode, header::CONTENT_LENGTH},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Install outside all inference middleware, including request logging and
/// daemon dispatch bypasses. File uploads use a separate router and limit.
/// A zero limit retains the configured unlimited behavior.
pub async fn limit_inference_body(State(max_bytes): State<usize>, request: Request<Body>, next: Next) -> Response {
    if max_bytes == 0 {
        return next.run(request).await;
    }

    let (parts, body) = request.into_parts();
    match read_inference_body(&parts.headers, body, max_bytes).await {
        Ok(bytes) => next.run(Request::from_parts(parts, Body::from(bytes))).await,
        Err(crate::errors::Error::PayloadTooLarge { .. }) => oversized_body(max_bytes),
        Err(_) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": {
                "message": "Failed to read request body",
                "type": "invalid_request_error",
                "code": "body_read_failed"
            }})),
        )
            .into_response(),
    }
}

/// Shared with the authenticated playground proxy, whose model parser runs
/// before URI rewriting reaches the inference router.
pub(crate) async fn read_inference_body(headers: &HeaderMap, body: Body, max_bytes: usize) -> Result<Bytes, crate::errors::Error> {
    let too_large = || crate::errors::Error::PayloadTooLarge {
        message: format!("Request body exceeds the maximum size of {max_bytes} bytes"),
    };
    // Reject known oversized bodies without reading them. Also enforce the
    // actual byte count below: Content-Length may be absent (chunked requests).
    if max_bytes != 0
        && headers
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(too_large());
    }

    to_bytes(body, if max_bytes == 0 { usize::MAX } else { max_bytes })
        .await
        .map_err(|error| {
            if std::error::Error::source(&error).is_some_and(|source| source.is::<http_body_util::LengthLimitError>()) {
                too_large()
            } else {
                // Transport errors can contain user-controlled text. Do not
                // log or echo their details.
                crate::errors::Error::BadRequest {
                    message: "Failed to read request body".to_string(),
                }
            }
        })
}

fn oversized_body(max_bytes: usize) -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        Json(serde_json::json!({"error": {
            "message": format!("Request body exceeds the maximum size of {max_bytes} bytes"),
            "type": "request_too_large",
            "code": "request_too_large"
        }})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Bytes, http::StatusCode, middleware, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tower::ServiceExt;

    fn app(limit: usize) -> Router {
        Router::new()
            .route("/chat/completions", post(|body: Bytes| async move { body }))
            .layer(middleware::from_fn_with_state(limit, limit_inference_body))
    }

    #[tokio::test]
    async fn rejects_oversized_body_before_the_handler() {
        let response = app(4)
            .oneshot(Request::post("/chat/completions").body(Body::from("12345")).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn rejects_content_length_without_polling_the_body() {
        let polls = Arc::new(AtomicUsize::new(0));
        let observed = polls.clone();
        let body = Body::from_stream(futures::stream::poll_fn(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
            std::task::Poll::Ready(None::<Result<Bytes, std::io::Error>>)
        }));
        let response = app(4)
            .oneshot(
                Request::post("/chat/completions")
                    .header("content-length", "10000000")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn stops_streaming_at_the_limit_even_without_an_honest_content_length() {
        for length in [None, Some("1")] {
            let polls = Arc::new(AtomicUsize::new(0));
            let observed = polls.clone();
            let body = Body::from_stream(futures::stream::poll_fn(move |_| {
                let n = observed.fetch_add(1, Ordering::SeqCst);
                assert!(n < 2, "must stop reading as soon as the limit is exceeded");
                std::task::Poll::Ready(Some(Ok::<_, std::io::Error>(Bytes::from_static(b"abc"))))
            }));
            let mut request = Request::post("/chat/completions");
            if let Some(length) = length {
                request = request.header("content-length", length);
            }
            let response = app(4).oneshot(request.body(body).unwrap()).await.unwrap();
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
            assert_eq!(polls.load(Ordering::SeqCst), 2);
        }
    }

    #[tokio::test]
    async fn preserves_bodies_at_the_limit_and_when_limits_are_disabled() {
        for (limit, input) in [(4, "1234"), (0, "12345")] {
            let response = app(limit)
                .oneshot(Request::post("/chat/completions").body(Body::from(input)).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(axum::body::to_bytes(response.into_body(), 100).await.unwrap(), input);
        }
    }

    #[tokio::test]
    async fn distinguishes_transport_errors_without_exposing_their_contents() {
        let body = Body::from_stream(futures::stream::once(async {
            Err::<Bytes, _>(std::io::Error::other("sensitive transport detail"))
        }));
        let response = app(4)
            .oneshot(Request::post("/chat/completions").body(body).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), 1000).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "body_read_failed");
        assert!(!body.to_string().contains("sensitive transport detail"));
    }
}
