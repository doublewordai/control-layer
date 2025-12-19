//! Request logging utility functions.

use crate::request_logging::models::AiResponse;
use outlet_postgres::SerializationError;
use std::io::Read as _;

use super::models::{ChatCompletionChunk, SseParseError};

/// Parse a Server-Sent Events string into a vector of data chunks
///
/// Per the SSE specification, empty data fields (e.g., "data: \n\n") should
/// dispatch events with empty string data, not be ignored.
///
/// # Errors
/// - `SseParseError::InvalidFormat` if no valid SSE data fields found
fn parse_sse_chunks(body_str: &str) -> Result<Vec<String>, SseParseError> {
    let mut chunks = Vec::new();
    let mut current_event_data = String::new();
    let mut found_sse_data = false;
    let mut has_pending_data = false;

    for line in body_str.lines() {
        let trimmed = line.trim();

        // Handle both "data: value" and "data:value" formats
        if let Some(data_part) = trimmed.strip_prefix("data:") {
            // Found a data field - even if empty, it's valid per SSE spec
            // Strip leading space if present (e.g., "data: " vs "data:")
            let data_part = data_part.strip_prefix(' ').unwrap_or(data_part);
            current_event_data = data_part.to_string();
            found_sse_data = true;
            has_pending_data = true;
        } else if trimmed.is_empty() && has_pending_data {
            // End of event, add the accumulated data (even if empty)
            chunks.push(current_event_data.clone());
            current_event_data.clear();
            has_pending_data = false;
        }
    }

    // Process any remaining data (in case the stream doesn't end with empty line)
    if has_pending_data {
        chunks.push(current_event_data);
    }

    if !found_sse_data {
        return Err(SseParseError::InvalidFormat);
    }

    Ok(chunks)
}

/// Converts JSON strings to ChatCompletionChunk objects and wraps in AiResponse
fn process_sse_chunks(chunks: Vec<String>) -> AiResponse {
    let chunks = chunks
        .into_iter()
        .filter_map(|x| {
            // Handle the special [DONE] marker
            if x.trim() == "[DONE]" {
                Some(ChatCompletionChunk::Done)
            } else {
                // Try to parse as JSON
                serde_json::from_str::<ChatCompletionChunk>(&x).ok()
            }
        })
        .collect::<Vec<_>>();

    AiResponse::ChatCompletionsStream(chunks)
}

/// Parses streaming response body, trying SSE first then JSON fallback
///
/// # Errors
/// Returns error if both SSE parsing and JSON deserialization fail
pub(crate) fn parse_streaming_response(body_str: &str) -> Result<AiResponse, Box<dyn std::error::Error>> {
    // Streaming: expect SSE, fallback to JSON
    parse_sse_chunks(body_str)
        .map(process_sse_chunks)
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
        .or_else(|_| serde_json::from_str(body_str).map_err(|e| Box::new(e) as Box<dyn std::error::Error>))
}

/// Parses non-streaming response body, expecting JSON format only
///
/// # Errors
/// Returns error if JSON deserialization fails
pub(crate) fn parse_non_streaming_response(body_str: &str) -> Result<AiResponse, Box<dyn std::error::Error>> {
    serde_json::from_str(body_str).map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}

/// Decompress response body if it's compressed according to headers
///
/// # Errors
/// Returns `SerializationError` if brotli decompression fails
pub(crate) fn decompress_response_if_needed(
    bytes: &[u8],
    headers: &std::collections::HashMap<String, Vec<bytes::Bytes>>,
) -> Result<Vec<u8>, SerializationError> {
    // Check for content-encoding header
    let content_encoding = headers
        .get("content-encoding")
        .or_else(|| headers.get("Content-Encoding"))
        .and_then(|values| values.first())
        .map(|bytes| String::from_utf8_lossy(bytes))
        .map(|s| s.trim().to_lowercase());

    match content_encoding.as_deref() {
        Some("br") | Some("brotli") => {
            let mut decompressed = Vec::new();
            brotli::Decompressor::new(bytes, 4096)
                .read_to_end(&mut decompressed)
                .map_err(|e| SerializationError {
                    fallback_data: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes),
                    error: Box::new(e),
                })?;
            Ok(decompressed)
        }
        _ => Ok(bytes.to_vec()),
    }
}

/// Extract value from request headers
///
/// # Arguments
/// * `request_data` - The HTTP request data containing headers
///
/// # Returns
/// * `Some(Uuid)` - Successfully extracted and parsed UUID (either full or padded from 8-char hex)
/// * `None` - Header missing, empty, or invalid format
pub(crate) fn extract_header_value_as_uuid(request_data: &outlet::RequestData, header_name: &str) -> Option<uuid::Uuid> {
    request_data
        .headers
        .get(header_name)
        .and_then(|values| values.first())
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|s| {
            // Try parsing as full UUID first
            if let Ok(uuid) = uuid::Uuid::parse_str(s) {
                return Some(uuid);
            }

            None
        })
}

/// Extracts a header value as a raw string from request headers.
///
/// # Arguments
/// * `request_data` - The HTTP request data containing headers
/// * `header_name` - The name of the header to extract
///
/// # Returns
/// * `Some(String)` - Successfully extracted string value
/// * `None` - Header missing, empty, or invalid UTF-8
pub(crate) fn extract_header_value_as_string(request_data: &outlet::RequestData, header_name: &str) -> Option<String> {
    request_data
        .headers
        .get(header_name)
        .and_then(|values| values.first())
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        decompress_response_if_needed, extract_header_value_as_string, parse_non_streaming_response, parse_sse_chunks,
        parse_streaming_response, process_sse_chunks,
    };
    use crate::request_logging::models::{AiResponse, ChatCompletionChunk, SseParseError};
    use axum::http::{Method, Uri};
    use bytes::Bytes;
    use outlet::RequestData;
    use std::collections::HashMap;
    use std::time::SystemTime;

    #[test]
    fn test_parse_sse_chunks_valid() {
        let sse_data = "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\"}\n\ndata: {\"id\":\"chatcmpl-456\",\"object\":\"chat.completion.chunk\"}\n\n";

        let result = parse_sse_chunks(sse_data).unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result[0], "{\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\"}");
        assert_eq!(result[1], "{\"id\":\"chatcmpl-456\",\"object\":\"chat.completion.chunk\"}");
    }

    #[test]
    fn test_parse_sse_chunks_single_chunk() {
        let sse_data = "data: {\"test\":\"value\"}\n\n";

        let result = parse_sse_chunks(sse_data).unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "{\"test\":\"value\"}");
    }

    #[test]
    fn test_parse_sse_chunks_no_trailing_newline() {
        let sse_data = "data: {\"test\":\"value\"}";

        let result = parse_sse_chunks(sse_data).unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "{\"test\":\"value\"}");
    }

    #[test]
    fn test_parse_sse_chunks_invalid_format() {
        let invalid_data = "this is not sse format";

        let result = parse_sse_chunks(invalid_data);

        assert_eq!(result.unwrap_err(), SseParseError::InvalidFormat);
    }

    #[test]
    fn test_parse_sse_chunks_empty_data() {
        // Test case with valid SSE prefix but empty/whitespace-only data
        // Per SSE spec, this should dispatch an event with empty string data
        let sse_data = "data: \n\n";

        let result = parse_sse_chunks(sse_data).unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "");
    }

    #[test]
    fn test_parse_sse_chunks_with_extra_whitespace() {
        let sse_data = "  data: {\"test\":\"value\"}  \n\n";

        let result = parse_sse_chunks(sse_data).unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "{\"test\":\"value\"}");
    }

    #[test]
    fn test_process_sse_chunks_valid_json() {
        let chunks = vec![
            r#"{"id":"chatcmpl-123","object":"chat.completion.chunk","created":1677652288,"model":"gpt-3.5-turbo","choices":[]}"#
                .to_string(),
            "[DONE]".to_string(),
        ];

        let result = process_sse_chunks(chunks);

        match result {
            AiResponse::ChatCompletionsStream(parsed_chunks) => {
                assert_eq!(parsed_chunks.len(), 2); // One JSON chunk + [DONE] marker
            }
            _ => panic!("Expected ChatCompletionsStream variant"),
        }
    }

    #[test]
    fn test_process_sse_chunks_invalid_json() {
        let chunks = vec!["invalid json".to_string(), r#"{"valid":"json"}"#.to_string()];

        let result = process_sse_chunks(chunks);

        match result {
            AiResponse::ChatCompletionsStream(parsed_chunks) => {
                assert_eq!(parsed_chunks.len(), 0); // Both invalid as ChatCompletionChunk, so filtered out
            }
            _ => panic!("Expected ChatCompletionsStream variant"),
        }
    }

    #[test]
    fn test_process_sse_chunks_empty() {
        let chunks = vec![];

        let result = process_sse_chunks(chunks);

        match result {
            AiResponse::ChatCompletionsStream(parsed_chunks) => {
                assert_eq!(parsed_chunks.len(), 0);
            }
            _ => panic!("Expected ChatCompletionsStream variant"),
        }
    }

    #[test]
    fn test_process_sse_chunks_done_marker() {
        let chunks = vec!["[DONE]".to_string()];

        let result = process_sse_chunks(chunks);

        match result {
            AiResponse::ChatCompletionsStream(parsed_chunks) => {
                assert_eq!(parsed_chunks.len(), 1);
                match &parsed_chunks[0] {
                    ChatCompletionChunk::Done => {} // Expected
                    _ => panic!("Expected Done variant"),
                }
            }
            _ => panic!("Expected ChatCompletionsStream variant"),
        }
    }

    #[test]
    fn test_parse_streaming_response_sse_success() {
        let result =
            parse_streaming_response("data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\"}\n\ndata: [DONE]\n\n").unwrap();

        match result {
            AiResponse::ChatCompletionsStream(_) => {}
            _ => panic!("Expected ChatCompletionsStream variant"),
        }
    }

    #[test]
    fn test_parse_streaming_response_json_fallback() {
        let result = parse_streaming_response(r#"{"id":"chatcmpl-123","choices":[]}"#).unwrap();

        // Should succeed via JSON fallback
        matches!(result, AiResponse::Other(_));
    }

    #[test]
    fn test_parse_streaming_response_both_fail() {
        let result = parse_streaming_response("not sse and not json");

        assert!(result.is_err());
    }

    #[test]
    fn test_parse_non_streaming_response_json_success() {
        let result = parse_non_streaming_response(r#"{"id":"chatcmpl-123","choices":[]}"#).unwrap();

        // Should parse as JSON (Other variant)
        matches!(result, AiResponse::Other(_));
    }

    #[test]
    fn test_parse_non_streaming_response_sse_fails() {
        let sse_data = "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\"}\n\ndata: [DONE]\n\n";

        let result = parse_non_streaming_response(sse_data);

        // SSE data should fail since non-streaming only accepts JSON
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_non_streaming_response_invalid_json() {
        let invalid_data = "not json";

        let result = parse_non_streaming_response(invalid_data);

        assert!(result.is_err());
    }

    #[test]
    fn test_decompress_response_no_compression() {
        let data = b"hello world";
        let headers = HashMap::new();

        let result = decompress_response_if_needed(data, &headers).unwrap();

        assert_eq!(result, data);
    }

    #[test]
    fn test_decompress_response_unknown_encoding() {
        let data = b"hello world";
        let mut headers = HashMap::new();
        headers.insert("content-encoding".to_string(), vec![Bytes::from("gzip")]);

        let result = decompress_response_if_needed(data, &headers).unwrap();

        // Unknown encoding should pass through unchanged
        assert_eq!(result, data);
    }

    // ===== Fusillade Request ID Tests =====

    #[test]
    fn test_extract_fusillade_request_id() {
        // Test with full UUID format
        let test_uuid = uuid::Uuid::new_v4();
        let mut headers = HashMap::new();
        headers.insert("x-fusillade-request-id".to_string(), vec![Bytes::from(test_uuid.to_string())]);

        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers,
            body: None,
        };

        let result = extract_header_value_as_string(&request_data, "x-fusillade-request-id");

        assert!(result.is_some());
        assert_eq!(result.unwrap(), test_uuid);
    }

    #[test]
    fn test_extract_fusillade_request_id_missing_header() {
        // Test when header is not present
        let headers = HashMap::new();

        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers,
            body: None,
        };

        let result = extract_header_value_as_string(&request_data, "x-fusillade-request-id");

        assert!(result.is_none());
    }

    #[test]
    fn test_extract_fusillade_request_id_empty_value() {
        // Test when header has empty value
        let mut headers = HashMap::new();
        headers.insert("x-fusillade-request-id".to_string(), vec![]);

        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers,
            body: None,
        };

        let result = extract_header_value_as_string(&request_data, "x-fusillade-request-id");

        assert!(result.is_none());
    }

    #[test]
    fn test_extract_fusillade_request_id_invalid_uuid() {
        let mut headers = HashMap::new();
        headers.insert("x-fusillade-request-id".to_string(), vec![Bytes::from("notvalid")]);

        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers,
            body: None,
        };

        let result = extract_header_value_as_string(&request_data, "x-fusillade-request-id");

        assert!(result.is_none());
    }

    #[test]
    fn test_extract_fusillade_request_id_invalid_utf8() {
        // Test with invalid UTF-8 bytes
        let mut headers = HashMap::new();
        headers.insert("x-fusillade-request-id".to_string(), vec![Bytes::from(vec![0xFF, 0xFE, 0xFD])]);

        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers,
            body: None,
        };

        let result = extract_header_value_as_string(&request_data, "x-fusillade-request-id");

        assert!(result.is_none());
    }

    #[test]
    fn test_extract_fusillade_request_id_all_zeros() {
        // Test with 8 zeros (valid hex)
        let mut headers = HashMap::new();
        headers.insert(
            "x-fusillade-request-id".to_string(),
            vec![Bytes::from("00000000-0000-0000-0000-000000000000")],
        );

        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers,
            body: None,
        };

        let result = extract_header_value_as_string(&request_data, "x-fusillade-request-id");

        assert!(result.is_some());
        let uuid = result.unwrap();
        assert_eq!(uuid.to_string(), "00000000-0000-0000-0000-000000000000");
    }
}
