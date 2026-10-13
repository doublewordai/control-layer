//! The client's view of usage on streamed chat and legacy completions.
//!
//! Streaming requests go upstream with `stream_options.include_usage` forced on,
//! because billing reads the usage (see [`super::outbound_request`]). What the
//! client receives follows OpenAI's contract for the field instead:
//!
//! - requested: every chunk carries `usage`, `null` until one final usage-only
//!   chunk (`choices: []`) holds the totals. Totals an upstream puts on a content
//!   chunk are moved to that final chunk;
//! - missing or false: no chunk carries `usage` and the usage-only chunk is not
//!   sent, so code that reads `chunk.choices[0]` on every chunk keeps working.
//!
//! A client that also sets `continuous_usage_stats` (a vLLM extension Dynamo
//! honours) asked for usage on every chunk and gets the stream unchanged.
//!
//! Applied outside request logging, which must still see the usage.

use std::ops::Range;

use axum::body::{Body, Bytes};
use axum::http::header;
use axum::response::Response;
use futures::StreamExt;
use serde_json::{Map, Value};

use crate::prompt_cache::sse::SseBufferedStream;

/// What a streaming client asked for in `stream_options`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientUsage {
    /// `include_usage` missing or false.
    Omitted,
    /// `include_usage: true`.
    Final,
    /// `include_usage` and `continuous_usage_stats` both true.
    Continuous,
}

/// The usage a client asked for on a streamed chat or legacy completions
/// request, or `None` for any other request.
pub fn requested_usage(path: &str, request: &Value) -> Option<ClientUsage> {
    if !path.trim_end_matches('/').ends_with("/completions") || request.get("stream").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let option = |name: &str| request.get("stream_options").and_then(|o| o.get(name)).and_then(Value::as_bool) == Some(true);
    Some(match (option("include_usage"), option("continuous_usage_stats")) {
        (false, _) => ClientUsage::Omitted,
        (true, false) => ClientUsage::Final,
        (true, true) => ClientUsage::Continuous,
    })
}

/// Rewrite a successful event-stream response to the client's usage choice.
/// Anything else passes through untouched.
pub fn apply(response: Response, usage: ClientUsage, sse_buffer_limit: usize) -> Response {
    let is_event_stream = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("text/event-stream"));
    if usage == ClientUsage::Continuous || !response.status().is_success() || !is_event_stream {
        return response;
    }

    let (mut parts, body) = response.into_parts();
    parts.headers.remove(header::CONTENT_LENGTH);
    let reads = body.into_data_stream().map(|read| read.map_err(std::io::Error::other));
    let mut events = SseBufferedStream::with_limit(reads, sse_buffer_limit);
    let stream = async_stream::stream! {
        // Totals taken off a content chunk, sent on their own unless the
        // upstream sends a usage-only chunk after all.
        let mut pending: Option<Map<String, Value>> = None;
        while let Some(event) = events.next().await {
            let event = match event {
                Ok(event) => event,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };
            if usage == ClientUsage::Omitted && !contains(&event, b"\"usage\"") {
                yield Ok(event);
                continue;
            }
            let (payload, mut chunk) = match parse(&event) {
                Frame::Chunk(payload, chunk) => (payload, chunk),
                Frame::Done => {
                    if let Some(totals) = pending.take() {
                        yield Ok(sse_event(totals));
                    }
                    yield Ok(event);
                    continue;
                }
                Frame::Other => {
                    yield Ok(event);
                    continue;
                }
            };

            let usage_only = chunk.get("choices").and_then(Value::as_array).is_some_and(Vec::is_empty);
            match usage {
                ClientUsage::Omitted => {
                    if chunk.shift_remove("usage").is_none() {
                        yield Ok(event);
                        continue;
                    }
                    if usage_only {
                        continue;
                    }
                }
                _ if usage_only => {
                    if chunk.get("usage").is_some_and(|u| !u.is_null()) {
                        pending = None;
                    }
                    yield Ok(event);
                    continue;
                }
                _ => match chunk.insert("usage".to_string(), Value::Null) {
                    Some(Value::Null) => {
                        yield Ok(event);
                        continue;
                    }
                    Some(totals) => {
                        let mut totals_chunk = chunk.clone();
                        totals_chunk.insert("choices".to_string(), Value::Array(Vec::new()));
                        totals_chunk.insert("usage".to_string(), totals);
                        pending = Some(totals_chunk);
                    }
                    None => {}
                },
            }
            yield Ok(replace_payload(&event, payload, &chunk));
        }
        if let Some(totals) = pending.take() {
            yield Ok(sse_event(totals));
        }
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

enum Frame {
    /// A completion chunk, with the byte range of its JSON in the event.
    Chunk(Range<usize>, Map<String, Value>),
    /// The `[DONE]` sentinel.
    Done,
    /// Anything else: error envelopes, comments, events with several `data:` lines.
    Other,
}

fn parse(event: &[u8]) -> Frame {
    let Ok(text) = std::str::from_utf8(event) else {
        return Frame::Other;
    };
    let Some(payload) = data_payload(text) else {
        return Frame::Other;
    };
    let data = &text[payload.clone()];
    if data.trim() == "[DONE]" {
        return Frame::Done;
    }
    match serde_json::from_str::<Value>(data) {
        Ok(Value::Object(chunk)) if chunk.contains_key("choices") && !chunk.contains_key("error") => Frame::Chunk(payload, chunk),
        _ => Frame::Other,
    }
}

/// Byte range of the data in the event's single `data:` line; `None` for an
/// event without exactly one.
fn data_payload(text: &str) -> Option<Range<usize>> {
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
    payload
}

/// The event with its JSON replaced, keeping its framing and line endings.
fn replace_payload(event: &[u8], payload: Range<usize>, chunk: &Map<String, Value>) -> Bytes {
    let mut out = Vec::with_capacity(event.len() + 16);
    out.extend_from_slice(&event[..payload.start]);
    out.extend_from_slice(Value::Object(chunk.clone()).to_string().as_bytes());
    out.extend_from_slice(&event[payload.end..]);
    Bytes::from(out)
}

fn sse_event(chunk: Map<String, Value>) -> Bytes {
    Bytes::from(format!("data: {}\n\n", Value::Object(chunk)))
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
    const FINISH_WITH_USAGE: &str = r#"{"id":"resp_1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}"#;
    const USAGE: &str = r#"{"id":"resp_1","object":"chat.completion.chunk","created":1,"model":"m","choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}"#;
    const ERROR: &str = r#"{"error":{"message":"upstream failed","type":"server_error","code":502}}"#;
    // OpenRouter's shape: an error alongside otherwise empty chunk fields.
    const CHUNK_ERROR: &str =
        r#"{"id":"gen-1","object":"chat.completion.chunk","choices":[],"usage":null,"error":{"message":"upstream failed","code":502}}"#;

    fn sse(payloads: &[&str]) -> String {
        payloads.iter().map(|p| format!("data: {p}\n\n")).collect()
    }

    /// Runs the given transport reads through [`apply`].
    async fn run(reads: Vec<String>, usage: ClientUsage) -> String {
        let stream = futures::stream::iter(reads.into_iter().map(|r| Ok::<_, std::io::Error>(Bytes::from(r))));
        let response = Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap();
        let body = apply(response, usage, onwards::sse::DEFAULT_SSE_BUFFER_LIMIT).into_body();
        String::from_utf8(axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec()).unwrap()
    }

    fn chunks(body: &str) -> Vec<Value> {
        body.split("\n\n")
            .filter_map(|event| event.trim_start().strip_prefix("data: "))
            .map(|data| data.trim_end())
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(data).unwrap())
            .collect()
    }

    fn value(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn requested_usage_covers_streamed_chat_and_completions_only() {
        let streamed = json!({"stream": true});
        assert_eq!(requested_usage("/chat/completions", &streamed), Some(ClientUsage::Omitted));
        assert_eq!(requested_usage("/completions", &streamed), Some(ClientUsage::Omitted));
        let opted_out = json!({"stream": true, "stream_options": {"include_usage": false}});
        assert_eq!(requested_usage("/chat/completions", &opted_out), Some(ClientUsage::Omitted));
        let opted_in = json!({"stream": true, "stream_options": {"include_usage": true}});
        assert_eq!(requested_usage("/chat/completions", &opted_in), Some(ClientUsage::Final));
        let continuous = json!({"stream": true, "stream_options": {"include_usage": true, "continuous_usage_stats": true}});
        assert_eq!(requested_usage("/chat/completions", &continuous), Some(ClientUsage::Continuous));
        let continuous_only = json!({"stream": true, "stream_options": {"continuous_usage_stats": true}});
        assert_eq!(requested_usage("/chat/completions", &continuous_only), Some(ClientUsage::Omitted));

        assert_eq!(requested_usage("/chat/completions", &json!({})), None);
        assert_eq!(requested_usage("/responses", &streamed), None);
        assert_eq!(requested_usage("/messages", &streamed), None);
        assert_eq!(requested_usage("/embeddings", &streamed), None);
    }

    #[tokio::test]
    async fn unrequested_usage_is_never_sent() {
        let body = run(vec![sse(&[CONTENT, TOOL, FINISH, USAGE, "[DONE]"])], ClientUsage::Omitted).await;
        assert_eq!(
            body,
            sse(&[CONTENT, TOOL, FINISH, "[DONE]"]),
            "content passes through byte for byte"
        );

        // Totals on the finish chunk, or on every chunk.
        let body = run(vec![sse(&[FINISH_WITH_USAGE, "[DONE]"])], ClientUsage::Omitted).await;
        assert_eq!(chunks(&body), vec![value(FINISH)]);
    }

    #[tokio::test]
    async fn requested_usage_is_null_until_the_usage_chunk() {
        let body = run(vec![sse(&[CONTENT, TOOL, FINISH, USAGE, "[DONE]"])], ClientUsage::Final).await;

        let chunks = chunks(&body);
        assert_eq!(chunks.len(), 4);
        for chunk in &chunks[..3] {
            assert_eq!(chunk.get("usage"), Some(&Value::Null), "{chunk}");
        }
        assert_eq!(chunks[3], value(USAGE));
        assert_eq!(
            chunks[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"a\":1}"
        );
        assert!(body.ends_with("data: [DONE]\n\n"));
    }

    #[tokio::test]
    async fn totals_on_a_content_chunk_move_to_their_own_chunk() {
        let body = run(vec![sse(&[CONTENT, FINISH_WITH_USAGE, "[DONE]"])], ClientUsage::Final).await;

        let sent = chunks(&body);
        assert_eq!(sent.len(), 3, "{body}");
        assert_eq!(sent[1]["choices"][0]["finish_reason"], "stop");
        assert_eq!(sent[1]["usage"], Value::Null);
        assert_eq!(sent[2], value(USAGE));
        assert!(body.ends_with("data: [DONE]\n\n"));

        // A usage-only chunk from the upstream is the only one sent.
        let body = run(vec![sse(&[FINISH_WITH_USAGE, USAGE, "[DONE]"])], ClientUsage::Final).await;
        assert_eq!(chunks(&body).len(), 2, "{body}");

        // Totals are kept when the stream ends without `[DONE]`.
        let body = run(vec![sse(&[FINISH_WITH_USAGE])], ClientUsage::Final).await;
        assert_eq!(chunks(&body).last(), Some(&value(USAGE)));
    }

    #[tokio::test]
    async fn continuous_usage_passes_through() {
        let upstream = sse(&[FINISH_WITH_USAGE, USAGE, "[DONE]"]);
        assert_eq!(run(vec![upstream.clone()], ClientUsage::Continuous).await, upstream);
    }

    #[tokio::test]
    async fn events_split_across_reads_are_reassembled() {
        let upstream = sse(&[CONTENT, USAGE, "[DONE]"]);
        let reads: Vec<String> = upstream
            .as_bytes()
            .chunks(7)
            .map(|c| String::from_utf8(c.to_vec()).unwrap())
            .collect();

        assert_eq!(run(reads.clone(), ClientUsage::Omitted).await, sse(&[CONTENT, "[DONE]"]));
        assert_eq!(chunks(&run(reads, ClientUsage::Final).await).len(), 2);
    }

    #[tokio::test]
    async fn crlf_streams_keep_their_framing() {
        let upstream: String = [CONTENT, USAGE, "[DONE]"].iter().map(|p| format!("data: {p}\r\n\r\n")).collect();
        let reads: Vec<String> = upstream
            .as_bytes()
            .chunks(5)
            .map(|c| String::from_utf8(c.to_vec()).unwrap())
            .collect();

        let body = run(reads.clone(), ClientUsage::Omitted).await;
        assert_eq!(body, format!("data: {CONTENT}\r\n\r\ndata: [DONE]\r\n\r\n"));

        let body = run(reads, ClientUsage::Final).await;
        assert!(body.starts_with("data: {"), "{body}");
        assert!(body.contains("\"usage\":null}\r\n\r\n"), "{body}");
        assert!(body.ends_with(&format!("data: {USAGE}\r\n\r\ndata: [DONE]\r\n\r\n")), "{body}");
    }

    #[tokio::test]
    async fn error_frames_and_comments_pass_through() {
        let upstream = format!(": keep-alive\n\n{}", sse(&[CONTENT, ERROR, CHUNK_ERROR, "[DONE]"]));
        for usage in [ClientUsage::Omitted, ClientUsage::Final] {
            let body = run(vec![upstream.clone()], usage).await;
            assert!(body.starts_with(": keep-alive\n\n"), "{body}");
            assert!(body.contains(&format!("data: {ERROR}\n\n")), "{usage:?}: {body}");
            assert!(body.contains(&format!("data: {CHUNK_ERROR}\n\n")), "{usage:?}: {body}");
        }
    }

    #[tokio::test]
    async fn non_stream_responses_pass_through() {
        let json_body = Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(USAGE))
            .unwrap();
        let body = apply(json_body, ClientUsage::Omitted, onwards::sse::DEFAULT_SSE_BUFFER_LIMIT).into_body();
        assert_eq!(axum::body::to_bytes(body, usize::MAX).await.unwrap(), USAGE.as_bytes());

        let failed_stream = Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(sse(&[USAGE])))
            .unwrap();
        let body = apply(failed_stream, ClientUsage::Omitted, onwards::sse::DEFAULT_SSE_BUFFER_LIMIT).into_body();
        assert_eq!(axum::body::to_bytes(body, usize::MAX).await.unwrap(), sse(&[USAGE]).as_bytes());
    }
}
