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

    // Snapshot the original inbound foreign request for the response side: the
    // foreign response can echo request fields (e.g. Responses echoes model /
    // tools / previous_response_id). We capture BEFORE pre_request so echoed
    // fields reflect what the client actually sent (the original
    // previous_response_id, not the hydrated request). A `Bytes` clone is a cheap
    // refcount bump, so the no-op (Anthropic) path pays nothing meaningful.
    let original_request = body_bytes.clone();

    // Opt-in async pre-request stage (default no-op). A store-backed translator
    // does its stateful pre-work here (e.g. Responses hydration, in the foreign
    // domain, before translation); pure translators return the body unchanged.
    let body_bytes = match translator.pre_request(&parts, body_bytes).await {
        Ok(b) => b,
        Err(TranslationError::BadRequest(msg)) => {
            return error_response(translator.as_ref(), StatusCode::BAD_REQUEST, &msg);
        }
        Err(TranslationError::Internal(msg)) => {
            warn!(error = %msg, translator = translator.name(), "edge translation: pre-request stage failed");
            return error_response(translator.as_ref(), StatusCode::INTERNAL_SERVER_ERROR, &msg);
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

    translate_response_back(translator.as_ref(), &original_request, response).await
}

/// Translate the downstream response back into the foreign protocol. `request`
/// is the original inbound foreign request body, forwarded to the translator's
/// response side for protocols whose response echoes request fields.
async fn translate_response_back(translator: &dyn ProtocolTranslator, request: &Bytes, response: Response) -> Response {
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
        return reframe_sse(translator, request, status, body);
    }

    let body_bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "edge translation: failed to read upstream response body");
            return error_response(translator, StatusCode::BAD_GATEWAY, "failed to read upstream response");
        }
    };

    if status.is_success() {
        match translator.translate_response(request, body_bytes) {
            Ok(new_body) => {
                // Opt-in async post-response stage (default no-op). A store-backed
                // translator persists the produced object here (e.g. Responses);
                // pure translators return the body unchanged.
                match translator.post_response(new_body).await {
                    Ok(final_body) => json_response(status, final_body),
                    Err(e) => {
                        warn!(error = %e, translator = translator.name(), "edge translation: post-response stage failed");
                        error_response(translator, StatusCode::BAD_GATEWAY, "response persistence failed")
                    }
                }
            }
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
fn reframe_sse(translator: &dyn ProtocolTranslator, request: &Bytes, status: StatusCode, body: Body) -> Response {
    let mut reframer = translator.stream_reframer(request);
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
    use crate::inference::translation::{StreamReframer, TranslatedRequest, TranslationRegistry, anthropic::AnthropicMessages};
    use async_trait::async_trait;
    use axum::http::{HeaderMap, request::Parts};
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
        let registry = TranslationRegistry::new(vec![Arc::new(AnthropicMessages::new(true))]);
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
        let registry = TranslationRegistry::new(vec![Arc::new(AnthropicMessages::new(true))]).with_max_body_size(16);
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

    /// A translator that overrides both opt-in stateful hooks, to prove the
    /// middleware runs them around the pure sync translation: `pre_request`
    /// rewrites the request body (foreign domain) before `translate_request`,
    /// and `post_response` tags the response (foreign domain) after
    /// `translate_response`.
    struct StatefulProbe;

    #[async_trait]
    impl ProtocolTranslator for StatefulProbe {
        fn name(&self) -> &'static str {
            "stateful_probe"
        }
        fn detect(&self, path: &str, _headers: &HeaderMap) -> bool {
            path.ends_with("/probe")
        }
        fn translate_request(&self, parts: &Parts, body: Bytes) -> Result<TranslatedRequest, TranslationError> {
            // Pass the (already pre_request-rewritten) body through, normalising
            // the path so it reaches the `/probe` route standing in for onwards.
            let path = parts.uri.path();
            let base = path.strip_suffix("/probe").unwrap_or(path);
            let uri = format!("{base}/chat/completions")
                .parse()
                .map_err(|e| TranslationError::Internal(format!("{e}")))?;
            Ok(TranslatedRequest {
                uri,
                headers: parts.headers.clone(),
                body,
            })
        }
        fn translate_response(&self, _request: &Bytes, body: Bytes) -> Result<Bytes, TranslationError> {
            Ok(body)
        }
        fn translate_error(&self, status: StatusCode, body: Bytes) -> (StatusCode, Bytes) {
            (status, body)
        }
        fn error_from_message(&self, status: StatusCode, message: &str) -> (StatusCode, Bytes) {
            (status, Bytes::from(format!(r#"{{"error":"{message}"}}"#)))
        }
        fn stream_reframer(&self, _request: &Bytes) -> Box<dyn StreamReframer> {
            unreachable!("this probe is blocking-only")
        }

        async fn pre_request(&self, _parts: &Parts, _body: Bytes) -> Result<Bytes, TranslationError> {
            // Replace the body so the downstream handler can prove the pre stage
            // ran before translate_request forwarded it.
            Ok(Bytes::from_static(br#"{"model":"m","pre":"ran"}"#))
        }
        async fn post_response(&self, body: Bytes) -> Result<Bytes, TranslationError> {
            // Tag the response so the client can prove the post stage ran after
            // translate_response.
            let mut v: serde_json::Value = serde_json::from_slice(&body).map_err(|e| TranslationError::Internal(format!("{e}")))?;
            v["post"] = serde_json::Value::String("ran".into());
            Ok(Bytes::from(
                serde_json::to_vec(&v).map_err(|e| TranslationError::Internal(format!("{e}")))?,
            ))
        }
    }

    /// The two opt-in stateful hooks bracket the pure translation: `pre_request`
    /// rewrites the outbound body before it reaches the handler, and
    /// `post_response` rewrites the returned one before it reaches the client.
    #[tokio::test]
    async fn stateful_hooks_bracket_the_pure_translation() {
        async fn echo_ok(req: Request) -> Response {
            let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
            let received: serde_json::Value = serde_json::from_slice(&body).unwrap();
            // pre_request rewrote the body before it reached the handler.
            assert_eq!(received["pre"], "ran", "pre_request should run before translate_request");
            json_response(
                StatusCode::OK,
                Bytes::from(serde_json::to_vec(&serde_json::json!({ "ok": true })).unwrap()),
            )
        }

        let registry = TranslationRegistry::new(vec![Arc::new(StatefulProbe)]);
        let inner = Router::new()
            .route("/probe", post(echo_ok))
            .layer(axum::middleware::from_fn_with_state(registry, translation_middleware));
        let server = axum_test::TestServer::new(Router::new().nest("/ai/v1", inner)).expect("test server");

        let response = server.post("/ai/v1/probe").json(&serde_json::json!({ "orig": true })).await;

        assert_eq!(response.status_code().as_u16(), 200);
        let body: serde_json::Value = response.json();
        assert_eq!(body["ok"], true);
        // post_response tagged the response after translate_response.
        assert_eq!(body["post"], "ran", "post_response should run after translate_response");
    }
}
