//! The client's view of usage on streamed chat and legacy completions.
//!
//! Streaming requests go upstream with `stream_options.include_usage` forced on,
//! because billing reads the usage (see [`super::outbound_request`]). What the
//! client receives follows OpenAI's contract for the field instead:
//!
//! - requested: every chunk carries `usage`, `null` until the final usage-only
//!   chunk (`choices: []`) that holds the totals;
//! - missing or false: no chunk carries `usage` and the usage-only chunk is not
//!   sent, so code that reads `chunk.choices[0]` on every chunk keeps working.
//!
//! Applied outside request logging, which must still see the usage.

use axum::body::{Body, Bytes};
use axum::http::header;
use axum::response::Response;
use futures::StreamExt;
use onwards::sse::SseBufferedStream;
use serde_json::{Map, Value};

/// The client's `stream_options.include_usage` for a streamed chat or legacy
/// completions request, or `None` for any other request.
pub fn requested_usage(path: &str, request: &Value) -> Option<bool> {
    if !path.trim_end_matches('/').ends_with("/completions") || request.get("stream").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    Some(request.pointer("/stream_options/include_usage").and_then(Value::as_bool) == Some(true))
}

/// Rewrite a successful event-stream response to the client's usage choice.
/// Anything else passes through untouched.
pub fn apply(response: Response, include_usage: bool, sse_buffer_limit: usize) -> Response {
    let is_event_stream = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("text/event-stream"));
    if !response.status().is_success() || !is_event_stream {
        return response;
    }

    let (mut parts, body) = response.into_parts();
    parts.headers.remove(header::CONTENT_LENGTH);
    let events = SseBufferedStream::with_limit(body.into_data_stream(), sse_buffer_limit).filter_map(move |event| {
        std::future::ready(match event {
            Ok(event) => rewrite_event(event, include_usage).map(Ok),
            Err(e) => Some(Err(e)),
        })
    });
    Response::from_parts(parts, Body::from_stream(events))
}

/// Rewrite one SSE event, or `None` to drop it. Events that aren't a single
/// completion chunk (`[DONE]`, error frames, comments) pass through as they are.
fn rewrite_event(event: Bytes, include_usage: bool) -> Option<Bytes> {
    if !include_usage && !contains(&event, b"\"usage\"") {
        return Some(event);
    }
    let Ok(text) = std::str::from_utf8(&event) else {
        return Some(event);
    };
    let Some(payload) = data_payload(text) else {
        return Some(event);
    };
    let Ok(Value::Object(mut chunk)) = serde_json::from_str::<Value>(&text[payload.clone()]) else {
        return Some(event);
    };
    if !chunk.contains_key("choices") {
        return Some(event);
    }

    if !rewrite_chunk(&mut chunk, include_usage)? {
        return Some(event);
    }
    let json = Value::Object(chunk).to_string();
    Some(Bytes::from(format!("{}{json}{}", &text[..payload.start], &text[payload.end..])))
}

/// Apply the usage contract to one chunk. Returns `Some(changed)`, or `None`
/// when the chunk is the usage-only chunk the client didn't ask for.
fn rewrite_chunk(chunk: &mut Map<String, Value>, include_usage: bool) -> Option<bool> {
    if include_usage {
        if chunk.contains_key("usage") {
            return Some(false);
        }
        chunk.insert("usage".to_string(), Value::Null);
        return Some(true);
    }
    let usage_only = chunk.get("choices").and_then(Value::as_array).is_some_and(Vec::is_empty);
    match chunk.shift_remove("usage") {
        None => Some(false),
        Some(_) if usage_only => None,
        Some(_) => Some(true),
    }
}

/// Byte range of the JSON in the event's single `data:` line. An event with no
/// `data:` line, several of them, or the `[DONE]` sentinel has none.
fn data_payload(text: &str) -> Option<std::ops::Range<usize>> {
    let mut offset = 0;
    let mut payload = None;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        if let Some(rest) = content.strip_prefix("data:") {
            if payload.is_some() {
                return None;
            }
            let lead = usize::from(rest.starts_with(' '));
            payload = Some(offset + "data:".len() + lead..offset + content.len());
        }
        offset += line.len();
    }
    payload.filter(|range| text[range.clone()].trim() != "[DONE]")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use serde_json::json;

    const CONTENT: &str =
        r#"{"id":"resp_1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"hi"}}]}"#;
    const TOOL: &str = r#"{"id":"resp_1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"f","arguments":"{\"a\":1}"}}]}}]}"#;
    const FINISH: &str = r#"{"id":"resp_1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
    const USAGE: &str = r#"{"id":"resp_1","object":"chat.completion.chunk","created":1,"model":"m","choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}"#;
    const ERROR: &str = r#"{"error":{"message":"upstream failed","type":"server_error","code":502}}"#;

    fn sse(payloads: &[&str]) -> String {
        payloads.iter().map(|p| format!("data: {p}\n\n")).collect()
    }

    /// Runs `body` through [`apply`] as the given transport reads.
    async fn run(reads: Vec<String>, include_usage: bool) -> String {
        let stream = futures::stream::iter(reads.into_iter().map(|r| Ok::<_, std::io::Error>(Bytes::from(r))));
        let response = Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap();
        let body = apply(response, include_usage, onwards::sse::DEFAULT_SSE_BUFFER_LIMIT).into_body();
        String::from_utf8(axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec()).unwrap()
    }

    fn chunks(body: &str) -> Vec<Value> {
        body.split("\n\n")
            .filter_map(|event| event.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(data).unwrap())
            .collect()
    }

    #[test]
    fn requested_usage_covers_streamed_chat_and_completions_only() {
        let streamed = json!({"stream": true});
        assert_eq!(requested_usage("/chat/completions", &streamed), Some(false));
        assert_eq!(requested_usage("/completions", &streamed), Some(false));
        let opted_in = json!({"stream": true, "stream_options": {"include_usage": true}});
        assert_eq!(requested_usage("/chat/completions", &opted_in), Some(true));
        let opted_out = json!({"stream": true, "stream_options": {"include_usage": false}});
        assert_eq!(requested_usage("/chat/completions", &opted_out), Some(false));

        assert_eq!(requested_usage("/chat/completions", &json!({})), None);
        assert_eq!(requested_usage("/responses", &streamed), None);
        assert_eq!(requested_usage("/messages", &streamed), None);
        assert_eq!(requested_usage("/embeddings", &streamed), None);
    }

    #[tokio::test]
    async fn unrequested_usage_is_never_sent() {
        let upstream = sse(&[CONTENT, TOOL, FINISH, USAGE, "[DONE]"]);
        let body = run(vec![upstream], false).await;

        assert_eq!(
            body,
            sse(&[CONTENT, TOOL, FINISH, "[DONE]"]),
            "content passes through byte for byte"
        );
    }

    #[tokio::test]
    async fn requested_usage_is_null_until_the_usage_chunk() {
        let upstream = sse(&[CONTENT, TOOL, FINISH, USAGE, "[DONE]"]);
        let body = run(vec![upstream], true).await;

        let chunks = chunks(&body);
        assert_eq!(chunks.len(), 4);
        for chunk in &chunks[..3] {
            assert_eq!(chunk.get("usage"), Some(&Value::Null), "{chunk}");
        }
        assert_eq!(chunks[3], serde_json::from_str::<Value>(USAGE).unwrap());
        assert_eq!(
            chunks[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"a\":1}"
        );
        assert!(body.ends_with("data: [DONE]\n\n"));
    }

    #[tokio::test]
    async fn usage_on_content_chunks_is_stripped_when_unrequested() {
        // Usage on every chunk (`continuous_usage_stats`), or on the finish chunk.
        let finish_with_usage = r#"{"id":"resp_1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}"#;
        let body = run(vec![sse(&[finish_with_usage, "[DONE]"])], false).await;

        assert_eq!(chunks(&body), vec![serde_json::from_str::<Value>(FINISH).unwrap()]);
    }

    #[tokio::test]
    async fn events_split_across_reads_are_reassembled() {
        let upstream = sse(&[CONTENT, USAGE, "[DONE]"]);
        let reads: Vec<String> = upstream
            .as_bytes()
            .chunks(7)
            .map(|c| String::from_utf8(c.to_vec()).unwrap())
            .collect();

        assert_eq!(run(reads.clone(), false).await, sse(&[CONTENT, "[DONE]"]));
        assert_eq!(chunks(&run(reads, true).await).len(), 2);
    }

    #[tokio::test]
    async fn error_frames_and_comments_pass_through() {
        let upstream = format!(": keep-alive\n\n{}", sse(&[CONTENT, ERROR, "[DONE]"]));
        for include_usage in [false, true] {
            let body = run(vec![upstream.clone()], include_usage).await;
            assert!(body.starts_with(": keep-alive\n\n"), "{body}");
            assert!(body.contains(&format!("data: {ERROR}\n\n")), "{body}");
        }
    }

    #[tokio::test]
    async fn non_stream_responses_pass_through() {
        let json_body = Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(USAGE))
            .unwrap();
        let body = apply(json_body, false, onwards::sse::DEFAULT_SSE_BUFFER_LIMIT).into_body();
        assert_eq!(axum::body::to_bytes(body, usize::MAX).await.unwrap(), USAGE.as_bytes());

        let failed_stream = Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(sse(&[USAGE])))
            .unwrap();
        let body = apply(failed_stream, false, onwards::sse::DEFAULT_SSE_BUFFER_LIMIT).into_body();
        assert_eq!(axum::body::to_bytes(body, usize::MAX).await.unwrap(), sse(&[USAGE]).as_bytes());
    }
}
