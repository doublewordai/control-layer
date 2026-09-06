//! End-to-end reachability for the response-ID override path.
//!
//! Drives the real [`onwards::build_router`] with an upstream that returns 2xx
//! headers and then drops the body mid-stream, reproducing the conditions in
//! which `patch_response_body_id` used to swallow the `to_bytes` error and ship
//! a `200` with an empty body and stale framing headers (the upstream's
//! `content-length` / `content-encoding`). The fix surfaces the upstream
//! body-read failure as a `500`, matching the `sanitize_response` block's
//! handling of the same failure.
//!
//! Defaults mirror dwctl: `strict_mode=false`, `sanitize_response` off, and the
//! response-ID override header configured — so the override block runs against
//! the live upstream streaming body (no prior buffering).

use std::error::Error;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use dashmap::DashMap;
use onwards::client::HttpClient;
use onwards::target::{Target, Targets};
use onwards::{AppState, build_router};
use serde_json::{Value, json};

const OVERRIDE_HEADER: &str = "x-test-request-id";
const OVERRIDE_VALUE: &str = "trace-abc-123";
/// `extract_override_id` prefixes the value with `resp_` when not already
/// prefixed, so the patched `id` becomes `resp_<value>`.
const PATCHED_ID: &str = "resp_trace-abc-123";

fn single_target(alias: &str) -> Targets {
    let targets_map = Arc::new(DashMap::new());
    let target = Target::builder()
        .url("https://upstream.example.com/".parse().unwrap())
        .build();
    targets_map.insert(alias.to_string(), target.into_pool());
    Targets {
        targets: targets_map,
        key_rate_limiters: Arc::new(DashMap::new()),
        key_concurrency_limiters: Arc::new(DashMap::new()),
        key_labels: Arc::new(DashMap::new()),
        strict_mode: false,
        http_pool_config: None,
    }
}

fn gzip_compress(data: &[u8]) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write as _;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// What the mock upstream serves for a single attempt.
#[derive(Clone, Debug)]
enum Upstream {
    /// `200` + `application/json` + `content-encoding: gzip`, then a body stream
    /// that errors immediately (upstream dropped the connection mid-body).
    /// `advertise_content_length` toggles the two real-world framing variants.
    FailingBody { advertise_content_length: bool },
    /// `200` + `application/json` with a valid uncompressed JSON body.
    OkJson { body: String },
    /// `200` + `application/json` + `content-encoding: gzip` with a valid
    /// gzipped JSON body.
    OkGzipJson { compressed: Vec<u8> },
    /// `200` + `text/event-stream` — a streamed (SSE) response that the
    /// override code must not buffer.
    OkSse { body: String },
}

#[derive(Clone, Debug)]
struct UpstreamClient {
    upstream: Upstream,
}

#[async_trait]
impl HttpClient for UpstreamClient {
    async fn request(
        &self,
        req: axum::extract::Request,
    ) -> Result<Response, Box<dyn Error + Send + Sync>> {
        // Drain and discard the request body so the request can complete.
        let _ = axum::body::to_bytes(req.into_body(), usize::MAX).await;
        let response = match &self.upstream {
            Upstream::FailingBody {
                advertise_content_length,
            } => {
                let stream = futures_util::stream::iter(vec![Err::<bytes::Bytes, std::io::Error>(
                    std::io::Error::other("upstream dropped mid-body"),
                )]);
                let mut builder = Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .header("content-encoding", "gzip");
                if *advertise_content_length {
                    builder = builder.header("content-length", "42");
                }
                builder.body(Body::from_stream(stream)).unwrap()
            }
            Upstream::OkJson { body } => Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(Body::from(body.clone()))
                .unwrap(),
            Upstream::OkGzipJson { compressed } => Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .header("content-encoding", "gzip")
                .body(Body::from(compressed.clone()))
                .unwrap(),
            Upstream::OkSse { body } => Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from(body.clone()))
                .unwrap(),
        };
        Ok(response)
    }
}

fn build_server(upstream: Upstream) -> axum_test::TestServer {
    let targets = single_target("gpt-4");
    let app_state = AppState::with_client(targets, UpstreamClient { upstream })
        .with_response_id_header(OVERRIDE_HEADER);
    axum_test::TestServer::new(build_router(app_state)).unwrap()
}

fn chat_body() -> Value {
    json!({
        "model": "gpt-4",
        "messages": [{"role": "user", "content": "hi"}]
    })
}

/// Emit the same client-facing diagnostic line the bug report uses, so a human
/// running with `--nocapture` sees the framing the client actually received.
fn print_diag(label: &str, response: &axum_test::TestResponse) {
    let bytes = response.as_bytes();
    eprintln!(
        "END-TO-END ({label}): status={} content-length={:?} content-encoding={:?} content-type={:?} body_len={}",
        response.status_code(),
        response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok()),
        response
            .headers()
            .get("content-encoding")
            .and_then(|v| v.to_str().ok()),
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        bytes.len(),
    );
}

// --- the fix: upstream mid-body drop must NOT become a 200 ---

#[tokio::test]
async fn override_block_surfaces_500_on_upstream_mid_body_error_content_length_variant() {
    // Upstream advertises `content-length: 42` + `content-encoding: gzip`,
    // then errors the body stream. Pre-fix the client saw `200` with
    // `content-length: 42` and a 0-byte body (a truncated content-length read).
    let server = build_server(Upstream::FailingBody {
        advertise_content_length: true,
    });
    let response = server
        .post("/v1/chat/completions")
        .add_header(OVERRIDE_HEADER, OVERRIDE_VALUE)
        .json(&chat_body())
        .await;

    print_diag("content-length variant", &response);
    assert_ne!(
        response.status_code(),
        200,
        "an upstream mid-body drop must not become an empty 200"
    );
    assert_eq!(response.status_code(), 500);

    // Stale upstream framing must not survive onto the error response: the
    // 500 carries its own application/json error body, not the upstream's
    // `content-encoding: gzip`.
    assert!(
        response.headers().get("content-encoding").is_none(),
        "stale content-encoding must not leak onto the error response"
    );
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );

    let body: Value = response.json();
    assert_eq!(body["error"]["code"], "internal_error");
    assert_eq!(body["error"]["type"], "internal_error");
}

#[tokio::test]
async fn override_block_surfaces_500_on_upstream_mid_body_error_chunked_variant() {
    // Upstream uses chunked framing (no content-length) + `content-encoding:
    // gzip`, then errors the body stream. Pre-fix the client saw `200` with no
    // content-length and a 0-byte body whose stale `content-encoding: gzip`
    // failed to decompress.
    let server = build_server(Upstream::FailingBody {
        advertise_content_length: false,
    });
    let response = server
        .post("/v1/chat/completions")
        .add_header(OVERRIDE_HEADER, OVERRIDE_VALUE)
        .json(&chat_body())
        .await;

    print_diag("chunked variant", &response);
    assert_ne!(
        response.status_code(),
        200,
        "an upstream mid-body drop must not become an empty 200"
    );
    assert_eq!(response.status_code(), 500);
    assert!(
        response.headers().get("content-encoding").is_none(),
        "stale content-encoding must not leak onto the error response"
    );
    let body: Value = response.json();
    assert_eq!(body["error"]["code"], "internal_error");
}

// --- regressions: the happy path is unchanged ---

#[tokio::test]
async fn happy_path_uncompressed_json_patches_id_and_strips_nothing() {
    let body = r#"{"id":"original","model":"gpt-4","choices":[]}"#;
    let server = build_server(Upstream::OkJson {
        body: body.to_string(),
    });
    let response = server
        .post("/v1/chat/completions")
        .add_header(OVERRIDE_HEADER, OVERRIDE_VALUE)
        .json(&chat_body())
        .await;

    assert_eq!(response.status_code(), 200);
    let parsed: Value = response.json();
    assert_eq!(parsed["id"], PATCHED_ID, "id must be overridden");
    assert_eq!(parsed["model"], "gpt-4");
}

#[tokio::test]
async fn happy_path_gzip_json_patches_id_and_strips_encoding() {
    let raw = br#"{"id":"original","model":"gpt-4","choices":[]}"#;
    let server = build_server(Upstream::OkGzipJson {
        compressed: gzip_compress(raw),
    });
    let response = server
        .post("/v1/chat/completions")
        .add_header(OVERRIDE_HEADER, OVERRIDE_VALUE)
        .json(&chat_body())
        .await;

    assert_eq!(response.status_code(), 200);
    // Decompression + patching strips content-encoding and recomputes
    // content-length for the uncompressed patched body.
    assert!(
        response.headers().get("content-encoding").is_none(),
        "content-encoding must be stripped after decompression + patching"
    );
    let content_length: usize = response
        .headers()
        .get("content-length")
        .expect("content-length must be set on the uncompressed body")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let parsed: Value = response.json();
    assert_eq!(parsed["id"], PATCHED_ID);
    assert_eq!(
        serde_json::to_vec(&parsed).unwrap().len(),
        content_length,
        "content-length must describe the actual patched body"
    );
}

#[tokio::test]
async fn no_override_header_leaves_id_unchanged_on_success() {
    // When the caller does not supply the override header, the override block
    // is skipped entirely, so the upstream body passes through unchanged.
    let body = r#"{"id":"original","model":"gpt-4","choices":[]}"#;
    let server = build_server(Upstream::OkJson {
        body: body.to_string(),
    });
    let response = server.post("/v1/chat/completions").json(&chat_body()).await;

    assert_eq!(response.status_code(), 200);
    let parsed: Value = response.json();
    assert_eq!(
        parsed["id"], "original",
        "id must be untouched without the override header"
    );
}

#[tokio::test]
async fn non_json_content_type_skips_override_and_streams_through() {
    // `text/event-stream` is not `application/json`, so `patch_response_body_id`
    // bails before buffering. An SSE response must stream through untouched,
    // not be buffered into a 500 — guarding against the fix over-reaching.
    let sse = "data: {\"id\":\"original\"}\n\ndata: [DONE]\n\n";
    let server = build_server(Upstream::OkSse {
        body: sse.to_string(),
    });
    let response = server
        .post("/v1/chat/completions")
        .add_header(OVERRIDE_HEADER, OVERRIDE_VALUE)
        .json(&chat_body())
        .await;

    assert_eq!(response.status_code(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(response.as_bytes(), sse.as_bytes());
}
