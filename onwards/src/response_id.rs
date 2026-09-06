//! Shared utilities for response ID override.
//!
//! When [`AppState::response_id_header`] is configured, the caller can supply a
//! response ID via a request header. These helpers extract the override value
//! and patch it into the HTTP response body, handling gzip/brotli decompression
//! and returning the body uncompressed (with `Content-Encoding` stripped).

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Response, header};
use std::io::Read as _;
use tracing::debug;

/// Extract a response ID override from request headers.
///
/// Looks up the header named `header_name` and normalises the value with a
/// `resp_` prefix (added if not already present).
pub fn extract_override_id(headers: &HeaderMap, header_name: &str) -> Option<String> {
    headers
        .get(header_name)
        .and_then(|v| v.to_str().ok())
        .map(|id| {
            if id.starts_with("resp_") {
                id.to_string()
            } else {
                format!("resp_{id}")
            }
        })
}

/// Returns true if response bodies at this path expose a top-level `id` field
/// that the caller may want to override via the configured header.
///
/// Currently: `/v1/responses` (Open Responses API) and `/v1/chat/completions`
/// (OpenAI-compatible chat completions). Query strings and trailing slashes
/// are ignored; substring matches on other routes (e.g. `/responses-logs`)
/// are rejected.
pub fn path_supports_id_override(path_and_query: &str) -> bool {
    let path = path_and_query.split('?').next().unwrap_or(path_and_query);
    let path = path.trim_end_matches('/');
    path.ends_with("/responses") || path.ends_with("/chat/completions")
}

/// Patch the `id` field in a JSON response body with the given override.
///
/// Handles `Content-Encoding: gzip` and `Content-Encoding: br` transparently:
/// the body is decompressed, the `id` field is overwritten, and the response is
/// returned **uncompressed** (with `Content-Encoding` / `Transfer-Encoding`
/// stripped and `Content-Length` updated).
///
/// If the body is not JSON, cannot be decompressed, or does not contain an `id`
/// field, the response is returned unchanged.
///
/// # Errors
///
/// Returns `Err` if the response body could not be buffered (for example,
/// because the upstream connection dropped mid-body). In that case
/// [`axum::body::to_bytes`] consumed the body without materialising any bytes,
/// so the original body is gone and cannot be restored — shipping it as an
/// empty `200` would leave the upstream's `content-length` / `content-encoding`
/// framing describing a body the response no longer carries. The caller must
/// surface an error response (the handler converts this into a `500`) rather
/// than emit a malformed `200`.
pub async fn patch_response_body_id(
    response: &mut Response<Body>,
    override_id: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let is_json = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("application/json"));

    if !is_json {
        return Ok(());
    }

    let content_encoding = response
        .headers()
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_lowercase());

    let bytes = match axum::body::to_bytes(std::mem::take(response.body_mut()), usize::MAX).await {
        Ok(b) => b,
        // The body was moved out by `mem::take` and consumed by `to_bytes`
        // without yielding any bytes, so it cannot be restored. Propagate the
        // failure rather than shipping an empty body with the upstream's stale
        // framing headers (content-length / content-encoding / content-type).
        Err(e) => return Err(e.into()),
    };

    // Decompress if needed.
    let decompressed = match content_encoding.as_deref() {
        Some("gzip") => {
            let mut decoder = flate2::read::GzDecoder::new(&bytes[..]);
            let mut buf = Vec::new();
            if decoder.read_to_end(&mut buf).is_ok() {
                buf
            } else {
                debug!("Failed to gzip-decompress response for ID patching, passing through");
                *response.body_mut() = Body::from(bytes);
                return Ok(());
            }
        }
        Some("br") | Some("brotli") => {
            let mut buf = Vec::new();
            if brotli::Decompressor::new(&bytes[..], 4096)
                .read_to_end(&mut buf)
                .is_ok()
            {
                buf
            } else {
                debug!("Failed to brotli-decompress response for ID patching, passing through");
                *response.body_mut() = Body::from(bytes);
                return Ok(());
            }
        }
        _ => bytes.to_vec(),
    };

    // Parse, patch, rewrite.
    if let Ok(mut json) = serde_json::from_slice::<serde_json::Value>(&decompressed) {
        if json.get("id").is_some() {
            json["id"] = serde_json::Value::String(override_id);
        }
        let patched = serde_json::to_vec(&json).unwrap_or(decompressed);
        let content_length = patched.len();
        *response.body_mut() = Body::from(patched);

        // Return uncompressed — the body size changed so the original encoding
        // is no longer valid. Strip encoding headers and set Content-Length.
        response.headers_mut().remove(header::CONTENT_ENCODING);
        response.headers_mut().remove(header::TRANSFER_ENCODING);
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, HeaderValue::from(content_length));
    } else {
        // Not valid JSON — restore the original (possibly compressed) body.
        *response.body_mut() = Body::from(bytes);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write as _;

    // --- extract_override_id ---

    #[test]
    fn extract_override_id_adds_prefix() {
        let mut headers = HeaderMap::new();
        headers.insert("x-custom-id", HeaderValue::from_static("abc-123"));
        assert_eq!(
            extract_override_id(&headers, "x-custom-id"),
            Some("resp_abc-123".to_string())
        );
    }

    #[test]
    fn extract_override_id_preserves_existing_prefix() {
        let mut headers = HeaderMap::new();
        headers.insert("x-custom-id", HeaderValue::from_static("resp_abc-123"));
        assert_eq!(
            extract_override_id(&headers, "x-custom-id"),
            Some("resp_abc-123".to_string())
        );
    }

    #[test]
    fn extract_override_id_missing_header() {
        let headers = HeaderMap::new();
        assert_eq!(extract_override_id(&headers, "x-custom-id"), None);
    }

    #[test]
    fn extract_override_id_wrong_header_name() {
        let mut headers = HeaderMap::new();
        headers.insert("x-other", HeaderValue::from_static("abc"));
        assert_eq!(extract_override_id(&headers, "x-custom-id"), None);
    }

    // --- path_supports_id_override ---

    #[test]
    fn path_supports_id_override_responses() {
        assert!(path_supports_id_override("/v1/responses"));
        assert!(path_supports_id_override("/responses"));
    }

    #[test]
    fn path_supports_id_override_chat_completions() {
        assert!(path_supports_id_override("/v1/chat/completions"));
        assert!(path_supports_id_override("/chat/completions"));
    }

    #[test]
    fn path_supports_id_override_with_query_string() {
        assert!(path_supports_id_override(
            "/v1/chat/completions?stream=true"
        ));
        assert!(path_supports_id_override("/v1/responses?foo=bar"));
    }

    #[test]
    fn path_supports_id_override_trailing_slash() {
        assert!(path_supports_id_override("/v1/chat/completions/"));
        assert!(path_supports_id_override("/v1/responses/"));
    }

    #[test]
    fn path_supports_id_override_rejects_other_paths() {
        assert!(!path_supports_id_override("/v1/embeddings"));
        assert!(!path_supports_id_override("/v1/completions"));
        assert!(!path_supports_id_override("/v1/models"));
        assert!(!path_supports_id_override("/"));
        assert!(!path_supports_id_override(""));
    }

    #[test]
    fn path_supports_id_override_rejects_substring_lookalikes() {
        // The old `.contains()` implementation matched these; `.ends_with()` should not.
        assert!(!path_supports_id_override("/v1/responses-logs"));
        assert!(!path_supports_id_override("/v1/chat/completions/history"));
        assert!(!path_supports_id_override("/responses_v2"));
    }

    // --- patch_response_body_id ---

    fn build_json_response(body: &[u8], content_type: &str) -> Response<Body> {
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", content_type)
            .body(Body::from(body.to_vec()))
            .unwrap()
    }

    fn gzip_compress(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn brotli_compress(data: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut writer = brotli::CompressorWriter::new(&mut buf, 4096, 4, 22);
            writer.write_all(data).unwrap();
        }
        buf
    }

    #[tokio::test]
    async fn patch_uncompressed_json() {
        let body = br#"{"id":"original","model":"gpt-4","status":"completed"}"#;
        let mut response = build_json_response(body, "application/json");

        patch_response_body_id(&mut response, "resp_override".to_string())
            .await
            .unwrap();

        let result = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(json["id"], "resp_override");
        assert_eq!(json["model"], "gpt-4");
    }

    #[tokio::test]
    async fn patch_gzip_compressed_json() {
        let body = br#"{"id":"original","model":"gpt-4","status":"completed"}"#;
        let compressed = gzip_compress(body);
        let mut response = Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .header("content-encoding", "gzip")
            .body(Body::from(compressed))
            .unwrap();

        patch_response_body_id(&mut response, "resp_patched".to_string())
            .await
            .unwrap();

        // Should be decompressed after patching
        assert!(response.headers().get("content-encoding").is_none());
        let result = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(json["id"], "resp_patched");
    }

    #[tokio::test]
    async fn patch_brotli_compressed_json() {
        let body = br#"{"id":"original","model":"gpt-4","status":"completed"}"#;
        let compressed = brotli_compress(body);
        let mut response = Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .header("content-encoding", "br")
            .body(Body::from(compressed))
            .unwrap();

        patch_response_body_id(&mut response, "resp_br_patched".to_string())
            .await
            .unwrap();

        assert!(response.headers().get("content-encoding").is_none());
        let result = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(json["id"], "resp_br_patched");
    }

    #[tokio::test]
    async fn patch_skips_non_json() {
        let body = b"not json";
        let mut response = build_json_response(body, "text/event-stream");

        patch_response_body_id(&mut response, "resp_ignored".to_string())
            .await
            .unwrap();

        let result = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(result.as_ref(), b"not json");
    }

    #[tokio::test]
    async fn patch_preserves_body_without_id_field() {
        let body = br#"{"model":"gpt-4","status":"completed"}"#;
        let mut response = build_json_response(body, "application/json");

        patch_response_body_id(&mut response, "resp_noop".to_string())
            .await
            .unwrap();

        let result = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert!(json.get("id").is_none());
    }

    #[tokio::test]
    async fn patch_sets_content_length() {
        let body = br#"{"id":"short"}"#;
        let mut response = build_json_response(body, "application/json");

        patch_response_body_id(&mut response, "resp_much-longer-id-value".to_string())
            .await
            .unwrap();

        let cl: usize = response
            .headers()
            .get("content-length")
            .unwrap()
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let result = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(cl, result.len());
    }

    // --- to_bytes error path (upstream body read failure) ---
    //
    // These cover the regression fixed when `patch_response_body_id` started
    // surfacing the `to_bytes` error instead of `Err(_) => return`. The body is
    // moved out by `mem::take` and consumed on `Err`, so it cannot be restored;
    // the function must return `Err` so the caller ships a 500 rather than a
    // `200` with stale framing headers.

    fn error_body_stream(immediate: bool) -> Body {
        use futures_util::stream;
        // A body stream that errors — either immediately (headers + drop) or
        // after a partial first chunk (mid-body drop).
        let items: Vec<Result<bytes::Bytes, std::io::Error>> = if immediate {
            vec![Err(std::io::Error::other("upstream dropped mid-body"))]
        } else {
            vec![
                Ok(bytes::Bytes::from_static(b"{\"id\":\"orig")),
                Err(std::io::Error::other("connection reset mid-body")),
            ]
        };
        Body::from_stream(stream::iter(items))
    }

    #[tokio::test]
    async fn patch_returns_err_when_upstream_body_read_fails() {
        // Upstream sent 200 + application/json (gzip) headers, then the body
        // stream errored before any bytes were materialised.
        let mut response = Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .header("content-encoding", "gzip")
            .header("content-length", "42")
            .body(error_body_stream(true))
            .unwrap();

        let result = patch_response_body_id(&mut response, "resp_override".to_string()).await;

        assert!(
            result.is_err(),
            "a body-read failure must surface as Err, not continue into a 200"
        );
        // The body was moved out by `mem::take` and consumed; nothing restorable
        // remains. Shipping this as a 200 would advertise content-length: 42
        // for a 0-byte body.
        let remaining = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(remaining.is_empty());
    }

    #[tokio::test]
    async fn patch_returns_err_on_mid_body_drop_after_partial_chunk() {
        // Upstream sent a partial JSON body, then the connection dropped.
        // `to_bytes` cannot complete and returns Err.
        let mut response = Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .header("content-encoding", "gzip")
            .body(error_body_stream(false))
            .unwrap();

        let result = patch_response_body_id(&mut response, "resp_x".to_string()).await;

        assert!(
            result.is_err(),
            "a mid-body drop must surface as Err even after a partial chunk"
        );
    }

    #[tokio::test]
    async fn patch_preserves_stale_framing_on_err_so_caller_must_not_ship_it() {
        // Documents *why* the caller must surface an error rather than the
        // response: on `to_bytes` Err the original framing headers are left in
        // place while the body is gone. Asserting this pins the contract so a
        // future "restore the body" attempt doesn't silently regress.
        let mut response = Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .header("content-encoding", "gzip")
            .header("content-length", "42")
            .body(error_body_stream(true))
            .unwrap();

        assert!(
            patch_response_body_id(&mut response, "resp_z".to_string())
                .await
                .is_err()
        );

        // The framing headers still describe a body that no longer exists —
        // exactly the malformed state the handler must not return as a 200.
        assert_eq!(response.headers().get("content-length").unwrap(), "42");
        assert_eq!(response.headers().get("content-encoding").unwrap(), "gzip");
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/json"
        );
    }
}
