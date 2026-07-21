//! Shared utilities for response ID override.
//!
//! When [`AppState::response_id_header`] is configured, the caller can supply a
//! response ID via a request header. These helpers extract the override value
//! and patch it into the HTTP response body, handling gzip/brotli decompression
//! and returning the body uncompressed (with `Content-Encoding` stripped).

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Response, header};
use axum::response::IntoResponse;
use std::io::Read as _;
use tracing::debug;

use crate::handlers::MAX_BUFFERED_UPSTREAM_RESPONSE_SIZE;

fn replace_with_bad_gateway(response: &mut Response<Body>) {
    *response = crate::errors::OnwardsErrorResponse::bad_gateway().into_response();
}

fn read_bounded<R: std::io::Read>(reader: R, limit: usize) -> std::io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    let mut reader = reader.take((limit as u64).saturating_add(1));
    reader.read_to_end(&mut buf)?;
    Ok((buf.len() <= limit).then_some(buf))
}

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
pub async fn patch_response_body_id(response: &mut Response<Body>, override_id: String) {
    let is_json = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("application/json"));

    if !is_json {
        return;
    }

    let content_encoding = response
        .headers()
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_lowercase());

    let bytes = match axum::body::to_bytes(
        std::mem::take(response.body_mut()),
        MAX_BUFFERED_UPSTREAM_RESPONSE_SIZE,
    )
    .await
    {
        Ok(b) => b,
        Err(_) => {
            replace_with_bad_gateway(response);
            return;
        }
    };

    // Decompress if needed.
    let decompressed = match content_encoding.as_deref() {
        Some("gzip") => {
            match read_bounded(
                flate2::read::GzDecoder::new(&bytes[..]),
                MAX_BUFFERED_UPSTREAM_RESPONSE_SIZE,
            ) {
                Ok(Some(buf)) => buf,
                Ok(None) => {
                    replace_with_bad_gateway(response);
                    return;
                }
                Err(_) => {
                    debug!("Failed to gzip-decompress response for ID patching, passing through");
                    *response.body_mut() = Body::from(bytes);
                    return;
                }
            }
        }
        Some("br") | Some("brotli") => {
            match read_bounded(
                brotli::Decompressor::new(&bytes[..], 4096),
                MAX_BUFFERED_UPSTREAM_RESPONSE_SIZE,
            ) {
                Ok(Some(buf)) => buf,
                Ok(None) => {
                    replace_with_bad_gateway(response);
                    return;
                }
                Err(_) => {
                    debug!("Failed to brotli-decompress response for ID patching, passing through");
                    *response.body_mut() = Body::from(bytes);
                    return;
                }
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
    async fn patch_rejects_response_that_decompresses_past_buffer_limit() {
        const EXPECTED_BUFFER_LIMIT: usize = 64 * 1024 * 1024;
        let oversized = serde_json::json!({
            "id": "original",
            "provider_padding": "x".repeat(EXPECTED_BUFFER_LIMIT),
        })
        .to_string();
        let compressed = gzip_compress(oversized.as_bytes());
        assert!(compressed.len() < EXPECTED_BUFFER_LIMIT);

        let mut response = Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .header("content-encoding", "gzip")
            .body(Body::from(compressed))
            .unwrap();

        patch_response_body_id(&mut response, "resp_override".to_string()).await;

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("generic gateway error should be small");
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"]["code"], "internal_error");
        assert_eq!(error["error"]["type"], "internal_error");
    }

    #[tokio::test]
    async fn patch_uncompressed_json() {
        let body = br#"{"id":"original","model":"gpt-4","status":"completed"}"#;
        let mut response = build_json_response(body, "application/json");

        patch_response_body_id(&mut response, "resp_override".to_string()).await;

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

        patch_response_body_id(&mut response, "resp_patched".to_string()).await;

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

        patch_response_body_id(&mut response, "resp_br_patched".to_string()).await;

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

        patch_response_body_id(&mut response, "resp_ignored".to_string()).await;

        let result = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(result.as_ref(), b"not json");
    }

    #[tokio::test]
    async fn patch_preserves_body_without_id_field() {
        let body = br#"{"model":"gpt-4","status":"completed"}"#;
        let mut response = build_json_response(body, "application/json");

        patch_response_body_id(&mut response, "resp_noop".to_string()).await;

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

        patch_response_body_id(&mut response, "resp_much-longer-id-value".to_string()).await;

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
}
