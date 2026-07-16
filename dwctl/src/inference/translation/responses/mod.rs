//! OpenAI Responses (`/v1/responses`) edge translator.
//!
//! Mirrors the [`super::anthropic`] module: it converts a Responses request into
//! canonical Chat Completions, hands off to the unchanged proxy path, and
//! converts the response back into a Responses object. The stateless converters
//! are ported from onwards' `OpenResponsesAdapter`; the stateful pieces
//! (`previous_response_id` hydration, response persistence) run in the async
//! `pre_request` / `post_response` bracket, not in the pure converters.

pub mod request;
pub mod response;
pub mod streaming;
pub mod types;
pub mod util;

use axum::http::{HeaderMap, StatusCode, Uri, header, request::Parts};
use bytes::Bytes;
use serde_json::Value;

use std::sync::Arc;

use async_trait::async_trait;
use onwards::strict::schemas::chat_completions::{ChatCompletionChunk, ChatCompletionResponse};
use onwards::traits::ResponseStore;

use self::types::{Input, Item, MessageContent as ResponseMessageContent, MessageItem, ResponsesRequest, ResponsesResponse};

use super::{ProtocolTranslator, StreamReframer, TranslatedRequest, TranslationError};

/// Translator for the OpenAI Responses API.
///
/// The pure request/response transforms live in [`request`] and [`response`]. The
/// stateful pieces run in the async bracket: `pre_request` hydrates
/// `previous_response_id` (reading the prior turn from the store and inlining its
/// items ahead of the current input), and `post_response` persists the produced
/// object. Both are ported from onwards' `OpenResponsesAdapter`.
pub struct OpenResponses {
    store: Arc<dyn ResponseStore>,
}

impl OpenResponses {
    pub fn new(store: Arc<dyn ResponseStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl ProtocolTranslator for OpenResponses {
    fn name(&self) -> &'static str {
        "openai_responses"
    }

    fn detect(&self, path: &str, _headers: &HeaderMap) -> bool {
        // `/v1/responses` is owned solely by the Responses API; no header gate.
        path.ends_with("/responses")
    }

    fn translate_request(&self, parts: &Parts, body: Bytes) -> Result<TranslatedRequest, TranslationError> {
        let req: ResponsesRequest =
            serde_json::from_slice(&body).map_err(|e| TranslationError::BadRequest(format!("invalid OpenAI Responses request: {e}")))?;

        let chat = request::to_chat_request(&req);
        let new_body = serde_json::to_vec(&chat).map_err(|e| TranslationError::Internal(e.to_string()))?;

        // Normalise the path so downstream code (the non-strict upstream
        // forwarder, sanitizer, image_normalizer) reads this as chat completions.
        // The route already matched; this is not a re-route.
        let uri = normalize_path(&parts.uri)?;

        let mut headers = parts.headers.clone();
        // Body size changed; drop the stale length so it is recomputed downstream.
        headers.remove(header::CONTENT_LENGTH);

        Ok(TranslatedRequest {
            uri,
            headers,
            body: Bytes::from(new_body),
        })
    }

    fn translate_response(&self, request: &Bytes, body: Bytes) -> Result<Bytes, TranslationError> {
        // Re-parse the original inbound request: the Responses object echoes many
        // request-only fields (model, tools, instructions, previous_response_id).
        let req: ResponsesRequest =
            serde_json::from_slice(request).map_err(|e| TranslationError::Internal(format!("re-parsing Responses request: {e}")))?;
        let chat: ChatCompletionResponse = serde_json::from_slice(&body)
            .map_err(|e| TranslationError::Internal(format!("upstream response was not Chat Completions: {e}")))?;

        let out = response::to_responses_response(&chat, &req);
        serde_json::to_vec(&out)
            .map(Bytes::from)
            .map_err(|e| TranslationError::Internal(e.to_string()))
    }

    fn translate_error(&self, status: StatusCode, body: Bytes) -> (StatusCode, Bytes) {
        // The Responses API and Chat Completions share the OpenAI error envelope,
        // so a downstream error is already in the right shape - pass it through.
        (status, body)
    }

    fn error_from_message(&self, status: StatusCode, message: &str) -> (StatusCode, Bytes) {
        (status, openai_error(status, message))
    }

    fn stream_reframer(&self, request: &Bytes) -> Box<dyn StreamReframer> {
        Box::new(ResponsesStreamReframer::new(request))
    }

    async fn pre_request(&self, _parts: &Parts, body: Bytes) -> Result<Bytes, TranslationError> {
        let mut req: ResponsesRequest =
            serde_json::from_slice(&body).map_err(|e| TranslationError::BadRequest(format!("invalid OpenAI Responses request: {e}")))?;

        // No prior turn to fold in - pass the request through untouched.
        let Some(prev_id) = req.previous_response_id.clone() else {
            return Ok(body);
        };

        // Read the prior response and inline its output items ahead of the current
        // input, producing a self-contained request (a Responses-domain concat).
        // The pure translator then converts it as if there were no prior turns.
        // Ported from `OpenResponsesAdapter::to_chat_request`'s prev-response block.
        let context = self
            .store
            .get_context(&prev_id)
            .await
            .map_err(|e| TranslationError::Internal(format!("reading previous response {prev_id}: {e}")))?
            .ok_or_else(|| TranslationError::BadRequest(format!("previous response not found: {prev_id}")))?;

        let prior: ResponsesResponse =
            serde_json::from_value(context).map_err(|e| TranslationError::Internal(format!("deserialising previous response: {e}")))?;

        let current_items = match std::mem::replace(&mut req.input, Input::Items(Vec::new())) {
            Input::Text(text) => vec![Item::Message(MessageItem {
                id: None,
                role: "user".to_string(),
                content: ResponseMessageContent::Text(text),
                status: None,
            })],
            Input::Items(items) => items,
        };

        let mut items = prior.output;
        items.extend(current_items);
        req.input = Input::Items(items);

        let new_body = serde_json::to_vec(&req).map_err(|e| TranslationError::Internal(e.to_string()))?;
        Ok(Bytes::from(new_body))
    }

    async fn post_response(&self, body: Bytes) -> Result<Bytes, TranslationError> {
        // Persist the produced Responses object so `GET /v1/responses/{id}` and a
        // later turn's `previous_response_id` can resolve it. The object already
        // carries its own id. Ported from `OpenResponsesAdapter::store_response`.
        let value: Value =
            serde_json::from_slice(&body).map_err(|e| TranslationError::Internal(format!("re-parsing produced Responses object: {e}")))?;
        self.store
            .store(&value)
            .await
            .map_err(|e| TranslationError::Internal(format!("persisting response: {e}")))?;
        Ok(body)
    }
}

/// Rewrite a `.../responses` path to `.../chat/completions`, preserving any
/// query. The route already matched; this only normalises the path for the code
/// that reads it downstream (not a re-route).
fn normalize_path(uri: &Uri) -> Result<Uri, TranslationError> {
    let path = uri.path();
    let base = path
        .strip_suffix("/responses")
        .ok_or_else(|| TranslationError::Internal(format!("path does not end with /responses: {path}")))?;
    let new_path = format!("{base}/chat/completions");
    let target = match uri.query() {
        Some(q) => format!("{new_path}?{q}"),
        None => new_path,
    };
    target
        .parse::<Uri>()
        .map_err(|e| TranslationError::Internal(format!("failed to build normalised URI: {e}")))
}

/// Build an OpenAI-shaped error envelope (`{"error": {"message", "type"}}`),
/// the same shape the Responses API returns.
fn openai_error(status: StatusCode, message: &str) -> Bytes {
    let err_type = match status.as_u16() {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        s if s >= 500 => "server_error",
        _ => "invalid_request_error",
    };
    let body = serde_json::json!({ "error": { "message": message, "type": err_type } });
    Bytes::from(serde_json::to_vec(&body).unwrap_or_default())
}

/// Wraps the (dwctl-owned) [`streaming::StreamingState`] in the [`StreamReframer`]
/// interface: each upstream Chat Completions chunk is fed through `process_chunk`
/// and the resulting Responses SSE events are serialised out; `finish` flushes
/// `finalize` (which emits `response.completed`).
struct ResponsesStreamReframer {
    /// `None` only if the (already validated) request failed to re-parse; we then
    /// emit a single terminal error rather than a stream.
    state: Option<streaming::StreamingState>,
    errored: bool,
}

impl ResponsesStreamReframer {
    fn new(request: &Bytes) -> Self {
        let state = serde_json::from_slice::<ResponsesRequest>(request)
            .ok()
            .map(|req| streaming::StreamingState::new(&req));
        Self { state, errored: false }
    }

    fn emit_error(&mut self, message: &str) -> Vec<u8> {
        if self.errored {
            return Vec::new();
        }
        self.errored = true;
        error_event(message)
    }
}

impl StreamReframer for ResponsesStreamReframer {
    fn push(&mut self, chunk: &Value) -> Vec<u8> {
        if self.state.is_none() {
            return self.emit_error("could not initialise Responses stream");
        }
        // Skip a chunk we can't parse, mirroring the middleware's own tolerance
        // of unparseable SSE data lines.
        let Ok(parsed) = serde_json::from_value::<ChatCompletionChunk>(chunk.clone()) else {
            return Vec::new();
        };
        let state = self.state.as_mut().expect("state is Some");
        state.process_chunk(&parsed).iter().flat_map(|e| e.to_sse().into_bytes()).collect()
    }

    fn error(&mut self, message: &str) -> Vec<u8> {
        self.emit_error(message)
    }

    fn finish(&mut self) -> Vec<u8> {
        match self.state.as_mut() {
            Some(state) => state.finalize().iter().flat_map(|e| e.to_sse().into_bytes()).collect(),
            None => Vec::new(),
        }
    }
}

/// Build a terminal OpenAI-shaped SSE error event.
fn error_event(message: &str) -> Vec<u8> {
    let data = serde_json::json!({ "type": "error", "error": { "message": message, "type": "server_error" } });
    format!("event: error\ndata: {data}\n\n").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::translation::TranslationRegistry;
    use crate::inference::translation::middleware::translation_middleware;
    use axum::body::Body;
    use axum::response::Response;
    use axum::{Router, extract::Request, routing::post};
    use onwards::traits::NoOpResponseStore;
    use std::sync::Arc;

    #[test]
    fn detect_claims_responses_ignoring_headers() {
        let t = OpenResponses::new(Arc::new(NoOpResponseStore));
        assert!(t.detect("/v1/responses", &HeaderMap::new()));
        assert!(t.detect("/ai/v1/responses", &HeaderMap::new()));
        // Native chat completions and messages are not ours.
        assert!(!t.detect("/v1/chat/completions", &HeaderMap::new()));
        assert!(!t.detect("/v1/messages", &HeaderMap::new()));
    }

    #[test]
    fn request_translates_and_normalizes_path() {
        let req = axum::http::Request::builder().uri("/ai/v1/responses").body(()).unwrap();
        let (parts, ()) = req.into_parts();
        let body = Bytes::from(serde_json::to_vec(&serde_json::json!({ "model": "gpt-4o", "input": "hi" })).unwrap());

        let out = OpenResponses::new(Arc::new(NoOpResponseStore))
            .translate_request(&parts, body)
            .unwrap();

        assert_eq!(out.uri.path(), "/ai/v1/chat/completions");
        let chat: serde_json::Value = serde_json::from_slice(&out.body).unwrap();
        assert_eq!(chat["model"], "gpt-4o");
        assert_eq!(chat["messages"][0]["role"], "user");
        assert_eq!(chat["messages"][0]["content"], "hi");
    }

    /// Stands in for onwards' chat-completions handler, reached via the alias
    /// route onwards will register. Asserts it received a translated Chat
    /// Completions body and returns a canned completion.
    async fn fake_onwards_chat_completions(req: Request) -> Response {
        let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
        let received: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // Proof it was translated: no Responses `input`, and a chat `messages` array.
        assert!(received.get("input").is_none(), "input should be gone, got: {received}");
        assert_eq!(received["messages"][0]["content"], "hello");

        let resp = serde_json::json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 0,
            "model": received["model"],
            "choices": [ { "index": 0, "message": { "role": "assistant", "content": "hi back" }, "finish_reason": "stop" } ],
            "usage": { "prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6 }
        });
        let mut r = Response::new(Body::from(serde_json::to_vec(&resp).unwrap()));
        r.headers_mut()
            .insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/json"));
        r
    }

    /// End-to-end blocking round-trip: a `/ai/v1/responses` request is matched by
    /// the real `/responses` route (no re-routing), translated to Chat Completions
    /// before the handler, and the handler's completion is converted back into a
    /// Responses object.
    #[tokio::test]
    async fn responses_round_trips_via_alias_route() {
        let registry = TranslationRegistry::new(vec![Arc::new(OpenResponses::new(Arc::new(NoOpResponseStore)))]);
        let inner = Router::new()
            .route("/responses", post(fake_onwards_chat_completions))
            .layer(axum::middleware::from_fn_with_state(registry, translation_middleware));
        let server = axum_test::TestServer::new(Router::new().nest("/ai/v1", inner)).expect("test server");

        let response = server
            .post("/ai/v1/responses")
            .json(&serde_json::json!({
                "model": "gpt-4o",
                "input": "hello"
            }))
            .await;

        assert_eq!(response.status_code().as_u16(), 200);
        let body: serde_json::Value = response.json();
        assert_eq!(body["object"], "response");
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["status"], "completed");
        assert!(body["id"].as_str().unwrap().starts_with("resp_"));
        // The assistant text is carried as an output_text content part.
        assert_eq!(body["output"][0]["content"][0]["text"], "hi back");
        assert_eq!(body["usage"]["input_tokens"], 4);
        assert_eq!(body["usage"]["output_tokens"], 2);
    }

    /// End-to-end streaming: the handler returns a Chat Completions SSE stream and
    /// the client must receive Responses typed events instead.
    #[tokio::test]
    async fn streaming_request_is_reframed_to_responses_events() {
        async fn fake_chat_sse(_req: Request) -> Response {
            let sse = concat!(
                "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
                "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"}}]}\n\n",
                "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"m\",\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1,\"total_tokens\":4}}\n\n",
                "data: [DONE]\n\n",
            );
            let mut r = Response::new(Body::from(sse));
            r.headers_mut()
                .insert(header::CONTENT_TYPE, header::HeaderValue::from_static("text/event-stream"));
            r
        }

        let registry = TranslationRegistry::new(vec![Arc::new(OpenResponses::new(Arc::new(NoOpResponseStore)))]);
        let inner = Router::new()
            .route("/responses", post(fake_chat_sse))
            .layer(axum::middleware::from_fn_with_state(registry, translation_middleware));
        let server = axum_test::TestServer::new(Router::new().nest("/ai/v1", inner)).expect("test server");

        let response = server
            .post("/ai/v1/responses")
            .json(&serde_json::json!({ "model": "gpt-4o", "input": "hi", "stream": true }))
            .await;

        assert_eq!(response.status_code().as_u16(), 200);
        let text = response.text();
        for ev in [
            "response.created",
            "response.output_item.added",
            "response.output_text.delta",
            "response.completed",
        ] {
            assert!(text.contains(&format!("event: {ev}")), "missing {ev} in:\n{text}");
        }
        assert!(
            text.contains(r#""delta":"Hi""#) || text.contains("Hi"),
            "missing text delta in:\n{text}"
        );
    }
}
