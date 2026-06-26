//! The single generic edge-translation Axum middleware.
//!
//! Layered as the outermost Tower layer on the onwards router. Dispatches to a
//! [`TranslationRegistry`]; a non-matching request is a pure pass-through.

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode, header},
    middleware::Next,
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;
use tracing::{debug, warn};

use super::{ProtocolTranslator, TranslationError, TranslationRegistry};

/// Generic edge translation middleware. See module docs.
pub async fn translation_middleware(State(registry): State<TranslationRegistry>, request: Request<Body>, next: Next) -> Response {
    let path = request.uri().path().to_string();

    let Some(translator) = registry.detect(&path, request.headers()) else {
        return next.run(request).await;
    };
    debug!(translator = translator.name(), path = %path, "edge translation: request matched");

    let (mut parts, body) = request.into_parts();

    // Bound the buffered request body by the configured limit so a translated
    // route cannot be used as an unbounded-memory DoS vector.
    let body_bytes = match axum::body::to_bytes(body, registry.max_body_size()).await {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "edge translation: request body too large or unreadable");
            return error_response(translator.as_ref(), StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
        }
    };

    let translated = match translator.translate_request(&parts, body_bytes) {
        Ok(t) => t,
        Err(TranslationError::BadRequest(msg)) => {
            return error_response(translator.as_ref(), StatusCode::BAD_REQUEST, &msg);
        }
        Err(TranslationError::Internal(msg)) => {
            warn!(error = %msg, translator = translator.name(), "edge translation: request translate failed");
            return error_response(translator.as_ref(), StatusCode::INTERNAL_SERVER_ERROR, &msg);
        }
    };

    // The route already matched; we swap the body/headers and normalise the path
    // so downstream code reads it as chat completions (not a re-route).
    parts.uri = translated.uri;
    parts.headers = translated.headers;
    let downstream_req = Request::from_parts(parts, Body::from(translated.body));

    let response = next.run(downstream_req).await;

    translate_response_back(translator.as_ref(), response).await
}

/// Translate the downstream response back into the foreign protocol.
async fn translate_response_back(translator: &dyn ProtocolTranslator, response: Response) -> Response {
    let (parts, body) = response.into_parts();
    let status = parts.status;

    // The response is self-describing, so we decide blocking-vs-stream from it,
    // not from the request: onwards labels every stream with `text/event-stream`
    // (and detects streaming the same way). A streaming request that fails before
    // streaming starts comes back as a JSON error and is correctly handled as a
    // blocking error below, rather than being force-fed to the SSE reframer.
    let is_sse = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.starts_with("text/event-stream"))
        .unwrap_or(false);

    if is_sse {
        return reframe_sse(translator, status, body);
    }

    let body_bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "edge translation: failed to read upstream response body");
            return error_response(translator, StatusCode::BAD_GATEWAY, "failed to read upstream response");
        }
    };

    if status.is_success() {
        match translator.translate_response(body_bytes) {
            Ok(new_body) => json_response(status, new_body),
            Err(e) => {
                warn!(error = %e, translator = translator.name(), "edge translation: response translate failed");
                error_response(translator, StatusCode::BAD_GATEWAY, "response translation failed")
            }
        }
    } else {
        let (new_status, new_body) = translator.translate_error(status, body_bytes);
        json_response(new_status, new_body)
    }
}

/// Build a JSON response with the given status and body.
fn json_response(status: StatusCode, body: Bytes) -> Response {
    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/json"));
    resp
}

/// Build a foreign-shaped error response from a plain message, via the
/// translator, so a pre-forward failure looks the same as a downstream one.
fn error_response(translator: &dyn ProtocolTranslator, status: StatusCode, message: &str) -> Response {
    let (status, bytes) = translator.error_from_message(status, message);
    json_response(status, bytes)
}

/// Wrap the upstream Chat Completions SSE body in the translator's reframer and
/// stream the foreign-protocol events out. Stays streaming (no buffering): each
/// complete `\n\n`-delimited SSE event is parsed and fed to the reframer as it
/// arrives.
fn reframe_sse(translator: &dyn ProtocolTranslator, status: StatusCode, body: Body) -> Response {
    let mut reframer = translator.stream_reframer();
    let mut data = body.into_data_stream();

    let out = async_stream::stream! {
        let mut buf: Vec<u8> = Vec::new();
        while let Some(item) = data.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(e) => {
                    // Upstream stream failed mid-flight: emit a terminal foreign
                    // error event rather than a clean close, and stop.
                    warn!(error = %e, "edge translation: upstream SSE transport error");
                    let ev = reframer.error("upstream stream error");
                    if !ev.is_empty() {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(ev));
                    }
                    return;
                }
            };
            buf.extend_from_slice(&chunk);
            // Drain every complete SSE event (terminated by a blank line).
            while let Some(pos) = find_subsequence(&buf, b"\n\n") {
                let block: Vec<u8> = buf.drain(..pos + 2).collect();
                // A complete event block is valid UTF-8 (no split multibyte char).
                for line in String::from_utf8_lossy(&block).lines() {
                    let Some(data_part) = line.strip_prefix("data:") else { continue };
                    let data_part = data_part.trim();
                    if data_part.is_empty() || data_part == "[DONE]" {
                        continue;
                    }
                    match serde_json::from_str::<serde_json::Value>(data_part) {
                        Ok(val) => {
                            let emitted = reframer.push(&val);
                            if !emitted.is_empty() {
                                yield Ok::<Bytes, std::io::Error>(Bytes::from(emitted));
                            }
                        }
                        Err(e) => {
                            debug!(error = %e, "edge translation: dropping unparseable SSE data line");
                        }
                    }
                }
            }
        }
        let closing = reframer.finish();
        if !closing.is_empty() {
            yield Ok(Bytes::from(closing));
        }
    };

    let mut resp = Response::new(Body::from_stream(out));
    // Preserve the downstream status (e.g. a non-200 event-stream) rather than
    // defaulting to 200.
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, header::HeaderValue::from_static("text/event-stream"));
    resp
}

/// First index of `needle` in `haystack`, if present.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::translation::{TranslationRegistry, anthropic::AnthropicMessages};
    use axum::{Router, extract::Request, routing::post};
    use std::sync::Arc;

    /// Stand-in for onwards' chat-completions handler (reached here via the
    /// `/messages` alias onwards will register). Asserts it received a translated
    /// Chat Completions body (no top-level `system`, with `max_tokens`) and
    /// returns a canned completion.
    async fn fake_onwards_chat_completions(req: Request) -> Response {
        let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
        let received: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // Proof the request was translated before reaching "onwards": Anthropic's
        // top-level `system` is gone (folded into a system message) and the
        // Chat Completions `max_tokens` is present.
        assert!(
            received.get("system").is_none(),
            "system should be folded into messages, got: {received}"
        );
        assert_eq!(received["max_tokens"], 50);
        assert_eq!(received["messages"][0]["role"], "system");
        assert_eq!(received["messages"][1]["role"], "user");

        let resp = serde_json::json!({
            "id": "chatcmpl-1",
            "model": received["model"],
            "choices": [ { "message": { "role": "assistant", "content": "hi back" }, "finish_reason": "stop" } ],
            "usage": { "prompt_tokens": 4, "completion_tokens": 2 }
        });
        json_response(StatusCode::OK, Bytes::from(serde_json::to_vec(&resp).unwrap()))
    }

    /// Build a test app mirroring production: an inner (onwards-like) router
    /// carrying the given routes, with the translation layer applied to it, all
    /// nested under `/ai/v1`. The `/messages` route stands in for the alias
    /// onwards will register to its chat-completions handler.
    fn test_app(inner: Router) -> axum_test::TestServer {
        let registry = TranslationRegistry::new(vec![Arc::new(AnthropicMessages)]);
        let inner = inner.layer(axum::middleware::from_fn_with_state(registry, translation_middleware));
        let app = Router::new().nest("/ai/v1", inner);
        axum_test::TestServer::new(app).expect("test server")
    }

    /// End-to-end: a request to `/ai/v1/messages` is matched by the real
    /// `/messages` route (no re-routing), translated to Chat Completions before
    /// the handler, and the handler's completion is reframed back to Anthropic.
    #[tokio::test]
    async fn messages_round_trips_via_alias_route() {
        let inner = Router::new().route("/messages", post(fake_onwards_chat_completions));
        let server = test_app(inner);

        let response = server
            .post("/ai/v1/messages")
            .add_header("x-api-key", "sk-test")
            .json(&serde_json::json!({
                "model": "claude-x",
                "max_tokens": 50,
                "system": "be brief",
                "messages": [ { "role": "user", "content": "hello" } ]
            }))
            .await;

        assert_eq!(response.status_code().as_u16(), 200);
        let body: serde_json::Value = response.json();
        assert_eq!(body["type"], "message");
        assert_eq!(body["role"], "assistant");
        assert_eq!(body["content"][0]["type"], "text");
        assert_eq!(body["content"][0]["text"], "hi back");
        assert_eq!(body["stop_reason"], "end_turn");
        assert_eq!(body["usage"]["input_tokens"], 4);
        assert_eq!(body["usage"]["output_tokens"], 2);
    }

    /// An over-limit request body is rejected as an Anthropic error, not buffered.
    #[tokio::test]
    async fn oversized_request_body_is_rejected_as_anthropic_error() {
        let inner = Router::new().route("/messages", post(fake_onwards_chat_completions));
        let registry = TranslationRegistry::new(vec![Arc::new(AnthropicMessages)]).with_max_body_size(16);
        let inner = inner.layer(axum::middleware::from_fn_with_state(registry, translation_middleware));
        let server = axum_test::TestServer::new(Router::new().nest("/ai/v1", inner)).expect("test server");

        let response = server
            .post("/ai/v1/messages")
            .add_header("x-api-key", "sk-test")
            .json(&serde_json::json!({
                "model": "claude-x", "max_tokens": 50,
                "messages": [ { "role": "user", "content": "this body is well over sixteen bytes" } ]
            }))
            .await;

        assert_eq!(response.status_code().as_u16(), 413);
        let body: serde_json::Value = response.json();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "request_too_large");
    }

    /// A 2xx downstream body that cannot be translated becomes an Anthropic error
    /// envelope, not a plain-text 502.
    #[tokio::test]
    async fn untranslatable_success_body_becomes_anthropic_error() {
        async fn bad_handler(_req: Request) -> Response {
            json_response(StatusCode::OK, Bytes::from_static(b"not a chat completion"))
        }
        let server = test_app(Router::new().route("/messages", post(bad_handler)));

        let response = server
            .post("/ai/v1/messages")
            .add_header("x-api-key", "sk-test")
            .json(&serde_json::json!({
                "model": "claude-x", "max_tokens": 50,
                "messages": [ { "role": "user", "content": "hi" } ]
            }))
            .await;

        assert_eq!(response.status_code().as_u16(), 502);
        let body: serde_json::Value = response.json();
        assert_eq!(body["type"], "error");
    }

    /// Non-strict onwards derives the upstream path from the inbound path. This
    /// proves the layer's path normalisation propagates to a catch-all handler:
    /// the handler (standing in for `target_message_handler`) must read
    /// `/chat/completions`, not `/messages`.
    #[tokio::test]
    async fn non_strict_catch_all_sees_normalized_path() {
        // Echo the path the handler reads back as the assistant content, so it
        // survives the response-side reframe into `content[0].text`.
        async fn echo_path(req: Request) -> Response {
            let path = req.uri().path().to_string();
            let resp = serde_json::json!({
                "id": "chatcmpl-1",
                "model": "m",
                "choices": [ { "message": { "role": "assistant", "content": path }, "finish_reason": "stop" } ],
                "usage": { "prompt_tokens": 0, "completion_tokens": 0 }
            });
            json_response(StatusCode::OK, Bytes::from(serde_json::to_vec(&resp).unwrap()))
        }

        // A catch-all route, like onwards' non-strict `/{*path}`.
        let inner = Router::new().route("/{*rest}", post(echo_path));
        let server = test_app(inner);

        let response = server
            .post("/ai/v1/messages")
            .add_header("x-api-key", "sk-test")
            .json(&serde_json::json!({ "model": "claude-x", "max_tokens": 10, "messages": [ { "role": "user", "content": "hi" } ] }))
            .await;

        assert_eq!(response.status_code().as_u16(), 200);
        let body: serde_json::Value = response.json();
        let seen = body["content"][0]["text"].as_str().unwrap();
        // The handler must read a chat-completions path, not a messages path, so
        // onwards' upstream-path join lands on the chat-completions endpoint.
        assert!(seen.ends_with("/chat/completions"), "downstream saw: {seen}");
        assert!(!seen.contains("/messages"), "downstream saw: {seen}");
    }

    /// A streaming `/messages` request: the handler returns an OpenAI SSE
    /// stream, and the client must receive Anthropic typed events instead.
    #[tokio::test]
    async fn streaming_request_is_reframed_to_anthropic_events() {
        async fn fake_sse(_req: Request) -> Response {
            let sse = concat!(
                "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\n",
                "data: [DONE]\n\n",
            );
            let mut resp = Response::new(Body::from(sse));
            resp.headers_mut()
                .insert(header::CONTENT_TYPE, header::HeaderValue::from_static("text/event-stream"));
            resp
        }

        let inner = Router::new().route("/messages", post(fake_sse));
        let server = test_app(inner);

        let response = server
            .post("/ai/v1/messages")
            .add_header("x-api-key", "sk-test")
            .json(&serde_json::json!({ "model": "claude-x", "max_tokens": 10, "stream": true, "messages": [ { "role": "user", "content": "hi" } ] }))
            .await;

        assert_eq!(response.status_code().as_u16(), 200);
        let text = response.text();
        for ev in [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ] {
            assert!(text.contains(&format!("event: {ev}")), "missing {ev} in:\n{text}");
        }
        assert!(text.contains(r#""text":"Hi""#));
        assert!(text.find("message_start").unwrap() < text.find("message_stop").unwrap());
    }

    /// Upstream SSE events split across network chunk boundaries (including a
    /// `\n\n` split) must still be reassembled and reframed correctly.
    #[tokio::test]
    async fn sse_event_split_across_chunks_is_reassembled() {
        async fn fake_split_sse(_req: Request) -> Response {
            let pieces: Vec<Result<Bytes, std::io::Error>> = vec![
                Ok(Bytes::from_static(b"data: {\"id\":\"c1\",\"model\":\"m\",\"cho")),
                Ok(Bytes::from_static(b"ices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n")),
                Ok(Bytes::from_static(
                    b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
                )),
                Ok(Bytes::from_static(b"\ndata: [DONE]\n\n")),
            ];
            let mut resp = Response::new(Body::from_stream(futures::stream::iter(pieces)));
            resp.headers_mut()
                .insert(header::CONTENT_TYPE, header::HeaderValue::from_static("text/event-stream"));
            resp
        }

        let inner = Router::new().route("/messages", post(fake_split_sse));
        let server = test_app(inner);

        let response = server
            .post("/ai/v1/messages")
            .add_header("x-api-key", "sk-test")
            .json(&serde_json::json!({ "model": "claude-x", "max_tokens": 10, "stream": true, "messages": [ { "role": "user", "content": "hi" } ] }))
            .await;

        assert_eq!(response.status_code().as_u16(), 200);
        let text = response.text();
        assert!(text.contains("event: message_start"), "{text}");
        assert!(text.contains(r#""text":"Hi""#), "{text}");
        assert!(text.contains(r#""stop_reason":"end_turn""#), "{text}");
        assert!(text.contains("event: message_stop"), "{text}");
    }

    /// A multibyte UTF-8 character whose bytes are split across two network
    /// chunks must survive reassembly intact (the byte-buffer must not decode a
    /// partial char).
    #[tokio::test]
    async fn multibyte_char_split_across_chunks_is_intact() {
        async fn fake(_req: Request) -> Response {
            // "cafe" with an e-acute (U+00E9, bytes C3 A9), split between its bytes.
            let full = "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"caf\u{00e9}\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            let bytes = full.as_bytes();
            let split = bytes.iter().position(|&b| b == 0xC3).unwrap() + 1; // between the two bytes of e-acute
            let pieces: Vec<Result<Bytes, std::io::Error>> = vec![
                Ok(Bytes::copy_from_slice(&bytes[..split])),
                Ok(Bytes::copy_from_slice(&bytes[split..])),
            ];
            let mut resp = Response::new(Body::from_stream(futures::stream::iter(pieces)));
            resp.headers_mut()
                .insert(header::CONTENT_TYPE, header::HeaderValue::from_static("text/event-stream"));
            resp
        }

        let inner = Router::new().route("/messages", post(fake));
        let server = test_app(inner);

        let response = server
            .post("/ai/v1/messages")
            .add_header("x-api-key", "sk-test")
            .json(&serde_json::json!({ "model": "m", "max_tokens": 1, "stream": true, "messages": [] }))
            .await;

        let text = response.text();
        assert!(text.contains("caf\u{00e9}"), "multibyte corrupted: {text}");
    }

    /// Non-2xx upstream responses are reshaped into the Anthropic error envelope
    /// with the right `type` per status, preserving the message.
    #[tokio::test]
    async fn error_responses_become_anthropic_error_envelope() {
        async fn status_echo(req: Request) -> Response {
            let code = req
                .headers()
                .get("x-test-status")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(200);
            let mut resp = Response::new(Body::from(r#"{"error":{"message":"boom"}}"#));
            *resp.status_mut() = StatusCode::from_u16(code).unwrap();
            resp.headers_mut()
                .insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/json"));
            resp
        }

        let inner = Router::new().route("/messages", post(status_echo));
        let server = test_app(inner);

        for (code, ty) in [(400u16, "invalid_request_error"), (429, "rate_limit_error"), (500, "api_error")] {
            let cs = code.to_string();
            let response = server
                .post("/ai/v1/messages")
                .add_header("x-api-key", "sk-test")
                .add_header("x-test-status", cs.as_str())
                .json(&serde_json::json!({ "model": "m", "max_tokens": 1, "messages": [ { "role": "user", "content": "hi" } ] }))
                .await;

            assert_eq!(response.status_code().as_u16(), code);
            let body: serde_json::Value = response.json();
            assert_eq!(body["type"], "error");
            assert_eq!(body["error"]["type"], ty, "status {code}");
            assert_eq!(body["error"]["message"], "boom");
        }
    }

    /// A native `/chat/completions` request matches no translator and passes
    /// through untouched (body and response unchanged).
    #[tokio::test]
    async fn native_chat_completions_passes_through() {
        async fn echo(req: Request) -> Response {
            let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
            json_response(StatusCode::OK, body)
        }

        let inner = Router::new().route("/chat/completions", post(echo));
        let server = test_app(inner);

        let response = server
            .post("/ai/v1/chat/completions")
            .json(&serde_json::json!({ "model": "gpt-x", "messages": [] }))
            .await;

        assert_eq!(response.status_code().as_u16(), 200);
        let body: serde_json::Value = response.json();
        // Echoed back verbatim — proof the middleware did not touch it.
        assert_eq!(body["model"], "gpt-x");
    }
}
