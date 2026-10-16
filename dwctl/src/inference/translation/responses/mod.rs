//! OpenAI Responses (`/v1/responses`) edge translator.
//!
//! Mirrors the [`super::anthropic`] module: it converts a Responses request into
//! canonical Chat Completions, hands off to the unchanged proxy path, and
//! converts the response back into a Responses object. It is a PURE, stateless
//! converter - the ported version of onwards' `OpenResponsesAdapter` transforms.
//! The Responses control plane lives outside it: `previous_response_id` hydration
//! and id minting run in the inference middleware (outer), and persistence in the
//! outlet. The translator only stamps the tracking id (passed in by the
//! middleware) onto the response so `GET /v1/responses/{id}` resolves.

pub mod hydrate;
pub mod request;
pub mod response;
pub mod streaming;
pub mod types;
pub mod util;

use axum::http::{HeaderMap, StatusCode, Uri, header, request::Parts};
use bytes::Bytes;
use serde_json::Value;

use onwards::strict::schemas::chat_completions::{
    ChatCompletionChunk, ChatCompletionResponse, normalize_chat_completion_chunk_value, normalize_chat_completion_response_value,
};

use self::types::ResponsesRequest;

use super::{ProtocolTranslator, StreamReframer, TranslatedRequest, TranslationError, parse_request};

/// Translator for the OpenAI Responses API. Pure and stateless; see module docs.
pub struct OpenResponses;

impl OpenResponses {
    pub fn new() -> Self {
        Self
    }
}

impl Default for OpenResponses {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtocolTranslator for OpenResponses {
    fn name(&self) -> &'static str {
        "openai_responses"
    }

    fn detect(&self, path: &str, _headers: &HeaderMap) -> bool {
        // `/v1/responses` is owned solely by the Responses API; no header gate.
        path.ends_with("/responses")
    }

    fn translate_request(&self, parts: &Parts, body: Bytes) -> Result<TranslatedRequest, TranslationError> {
        let req: ResponsesRequest = parse_request(&body, DECLARED_FIELDS, "OpenAI Responses")?;

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

    fn translate_response(&self, request: &Bytes, response_id: Option<&str>, body: Bytes) -> Result<Bytes, TranslationError> {
        // Re-parse the original inbound request: the Responses object echoes many
        // request-only fields (model, tools, instructions, previous_response_id).
        let req: ResponsesRequest =
            serde_json::from_slice(request).map_err(|e| TranslationError::Internal(format!("re-parsing Responses request: {e}")))?;
        // Backends differ on which optional-per-spec fields they omit (Dynamo
        // omits `role` on streamed deltas, which the reassembled batch/flex body
        // inherits), so relax the payload with onwards' sanitizer before the
        // strict parse — the same order onwards' own strict handlers use.
        let mut raw: Value = serde_json::from_slice(&body)
            .map_err(|e| TranslationError::Internal(format!("upstream response was not Chat Completions: {e}")))?;
        normalize_chat_completion_response_value(&mut raw, &req.model);
        let cache_write_tokens = util::cache_write_tokens(raw.get("usage"));
        let chat: ChatCompletionResponse = serde_json::from_value(raw)
            .map_err(|e| TranslationError::Internal(format!("upstream response was not Chat Completions: {e}")))?;

        // Stamp the platform tracking id so `GET /v1/responses/{id}` resolves; fall
        // back to the upstream completion id when there's no tracking row.
        let mut out = response::to_responses_response(&chat, &req, response_id);
        if let Some(usage) = out.usage.as_mut() {
            usage.input_tokens_details.cache_write_tokens = cache_write_tokens;
        }
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
        self.error_for_param(status, message, None)
    }

    fn error_for_param(&self, status: StatusCode, message: &str, param: Option<&str>) -> (StatusCode, Bytes) {
        (status, openai_error(status, message, param))
    }

    fn stream_reframer(&self, request: &Bytes, response_id: Option<&str>) -> Box<dyn StreamReframer> {
        Box::new(ResponsesStreamReframer::new(request, response_id))
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
/// The field names and renames declared in [`types`], sorted, for locating
/// where a request stopped matching [`ResponsesRequest`] without recording a
/// key the client chose. A name missing here is recorded as `*`, so the list
/// can only hide too much; a test keeps it in step with `types.rs`.
const DECLARED_FIELDS: &[&str] = &[
    "allowed_tools",
    "annotation_type",
    "annotations",
    "arguments",
    "background",
    "cache_write_tokens",
    "cached_tokens",
    "call_id",
    "code",
    "code_interpreter",
    "completed_at",
    "computer_use_preview",
    "container",
    "content",
    "content_filter",
    "created_at",
    "custom",
    "data",
    "defer_loading",
    "description",
    "detail",
    "display_height",
    "display_width",
    "effort",
    "encrypted_content",
    "environment",
    "error",
    "error_type",
    "event_type",
    "extra",
    "fallback_model",
    "fallback_response_id",
    "file_id",
    "file_search",
    "filename",
    "filters",
    "format",
    "frequency_penalty",
    "function",
    "headers",
    "hosted_tool",
    "id",
    "image_url",
    "include",
    "incomplete_details",
    "input",
    "input_file",
    "input_image",
    "input_text",
    "input_tokens",
    "input_tokens_details",
    "instructions",
    "json_object",
    "json_schema",
    "logprobs",
    "max_num_results",
    "max_output_tokens",
    "max_tool_calls",
    "mcp",
    "message",
    "message.output_text.logprobs",
    "metadata",
    "model",
    "name",
    "namespace",
    "object",
    "output",
    "output_text",
    "output_tokens",
    "output_tokens_details",
    "parallel_tool_calls",
    "param",
    "parameters",
    "presence_penalty",
    "previous_response_id",
    "prompt_cache_key",
    "reason",
    "reasoning",
    "reasoning.encrypted_content",
    "reasoning_text",
    "reasoning_tokens",
    "refusal",
    "require_approval",
    "response",
    "role",
    "safety_identifier",
    "schema",
    "search_context_size",
    "sequence_number",
    "server_label",
    "server_url",
    "service_tier",
    "status",
    "stop",
    "store",
    "stream",
    "strict",
    "summary",
    "summary_text",
    "temperature",
    "text",
    "tool_choice",
    "tool_type",
    "tools",
    "top_logprobs",
    "top_p",
    "total_tokens",
    "truncation",
    "type",
    "usage",
    "user",
    "user_location",
    "vector_store_ids",
    "web_search_preview",
];

fn openai_error(status: StatusCode, message: &str, param: Option<&str>) -> Bytes {
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
    let body = serde_json::json!({ "error": { "message": message, "type": err_type, "param": param } });
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
    /// Requested model, used to backfill chunks from backends that omit it.
    model: String,
    /// Chunk id to backfill when the backend omits one.
    fallback_id: String,
    errored: bool,
}

impl ResponsesStreamReframer {
    fn new(request: &Bytes, response_id: Option<&str>) -> Self {
        let req = serde_json::from_slice::<ResponsesRequest>(request).ok();
        let model = req.as_ref().map(|r| r.model.clone()).unwrap_or_default();
        let state = req.map(|req| streaming::StreamingState::new(&req, response_id));
        let fallback_id = response_id
            .map(str::to_string)
            .unwrap_or_else(|| format!("chatcmpl-{}", uuid::Uuid::new_v4()));
        Self {
            state,
            model,
            fallback_id,
            errored: false,
        }
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
        if self.errored {
            return Vec::new();
        }
        if self.state.is_none() {
            return self.emit_error("could not initialise Responses stream");
        }
        // An upstream error inside the stream (onwards forwards it as a bare
        // `{"error":{...}}` chunk) is terminal: surface it, and never follow it
        // with `response.completed`.
        if let Some(err) = chunk.get("error").filter(|e| e.is_object()) {
            let message = err.get("message").and_then(Value::as_str).unwrap_or("upstream error");
            return self.emit_error(message);
        }
        // Backfill fields backends legally omit on chunks before the strict
        // parse (same relaxation as the blocking path); then skip a chunk we
        // still can't parse, mirroring the middleware's own tolerance of
        // unparseable SSE data lines.
        let mut raw = chunk.clone();
        normalize_chat_completion_chunk_value(&mut raw, &self.model, &self.fallback_id);
        let Ok(parsed) = serde_json::from_value::<ChatCompletionChunk>(raw) else {
            return Vec::new();
        };
        let state = self.state.as_mut().expect("state is Some");
        let events = state.process_chunk(&parsed);
        if parsed.usage.is_some() {
            state.preserve_cache_write_tokens(chunk.get("usage"));
        }
        events.iter().flat_map(|e| e.to_sse().into_bytes()).collect()
    }

    fn error(&mut self, message: &str) -> Vec<u8> {
        self.emit_error(message)
    }

    fn finish(&mut self) -> Vec<u8> {
        if self.errored {
            return Vec::new();
        }
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
    use onwards::strict::schema_mismatch::SchemaMismatch;
    use std::sync::Arc;

    /// How the translator refuses `body`, which must not translate.
    fn refusal(body: &[u8]) -> TranslationError {
        let (parts, ()) = axum::http::Request::builder().uri("/v1/responses").body(()).unwrap().into_parts();
        match OpenResponses::new().translate_request(&parts, Bytes::copy_from_slice(body)) {
            Ok(_) => panic!("the body should not translate"),
            Err(e) => e,
        }
    }

    fn located(body: &[u8]) -> Option<SchemaMismatch> {
        match refusal(body) {
            TranslationError::SchemaMismatch { mismatch, .. } => mismatch,
            other => panic!("expected a schema mismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_schema_mismatch_names_the_field_but_not_its_value() {
        let found = located(br#"{"model":"m","input":"hi","max_output_tokens":"planted-secret","metadata":{"planted-secret":1}}"#)
            .expect("located");
        assert_eq!(found.path.as_deref(), Some("max_output_tokens"));
        assert_eq!(found.reason, "invalid_type");
        assert!(!format!("{found:?}").contains("planted"));
    }

    #[test]
    fn a_schema_mismatch_hides_client_keys_in_free_form_values() {
        // With serde_json's `raw_value` feature this object must hold a string,
        // so the free-form `metadata` fails beneath keys the client chose.
        let found =
            located(br#"{"model":"m","input":"hi","metadata":{"client_secret":{"$serde_json::private::RawValue":1}}}"#).expect("located");
        assert_eq!(found.path.as_deref(), Some("metadata.*.*"));
    }

    #[test]
    fn a_missing_field_is_named() {
        let found = located(br#"{"input":"hi"}"#).expect("located");
        assert_eq!(found.path.as_deref(), Some("model"));
        assert_eq!(found.reason, "missing_field");
    }

    #[test]
    fn a_body_that_is_not_json_is_not_a_schema_mismatch() {
        assert!(matches!(refusal(b"{not json"), TranslationError::InvalidJson(_)));
    }

    #[test]
    fn errors_name_the_param() {
        let (_, body) = OpenResponses::new().error_for_param(StatusCode::BAD_REQUEST, "bad", Some("input"));
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["param"], "input");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        let (_, body) = OpenResponses::new().error_from_message(StatusCode::BAD_REQUEST, "bad");
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["param"], Value::Null);
    }

    #[test]
    fn declared_fields_are_sorted_and_unique() {
        assert!(DECLARED_FIELDS.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn declared_fields_cover_the_request_types() {
        let missing: Vec<_> = onwards::test_utils::declared_field_names(include_str!("types.rs"))
            .into_iter()
            .filter(|name| DECLARED_FIELDS.binary_search(&name.as_str()).is_err())
            .collect();
        assert!(missing.is_empty(), "add to DECLARED_FIELDS: {missing:?}");
    }

    #[test]
    fn detect_claims_responses_ignoring_headers() {
        let t = OpenResponses::new();
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

        let out = OpenResponses::new().translate_request(&parts, body).unwrap();

        assert_eq!(out.uri.path(), "/ai/v1/chat/completions");
        let chat: serde_json::Value = serde_json::from_slice(&out.body).unwrap();
        assert_eq!(chat["model"], "gpt-4o");
        assert_eq!(chat["messages"][0]["role"], "user");
        assert_eq!(chat["messages"][0]["content"], "hi");
    }

    #[test]
    fn blocking_response_emits_standard_cache_usage_only() {
        let request = Bytes::from_static(br#"{"model":"m","input":"hi"}"#);
        let usage = serde_json::json!({
            "prompt_tokens": 2000, "completion_tokens": 2, "total_tokens": 2002,
            "prompt_tokens_details": {"cached_tokens": 1000},
            "cache_read_input_tokens": 1000,
            "cache_creation_input_tokens": 600,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 100,
                "ephemeral_1h_input_tokens": 200,
                "ephemeral_24h_input_tokens": 300
            },
            "unrelated_provider_field": "do not forward"
        });
        let body = serde_json::json!({
            "id": "c1", "object": "chat.completion", "created": 0, "model": "m",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": usage
        });
        let translated = OpenResponses::new()
            .translate_response(&request, None, Bytes::from(body.to_string()))
            .unwrap();
        let response: Value = serde_json::from_slice(&translated).unwrap();
        assert_eq!(response["usage"]["input_tokens"], 2000);
        assert_eq!(response["usage"]["input_tokens_details"]["cached_tokens"], 1000);
        assert_eq!(response["usage"]["input_tokens_details"]["cache_write_tokens"], 600);
        for field in ["cache_read_input_tokens", "cache_creation_input_tokens", "cache_creation"] {
            assert!(response["usage"].get(field).is_none(), "internal field {field} must not be public");
        }
        assert!(response["usage"].get("unrelated_provider_field").is_none());
    }

    #[test]
    fn displayed_cache_tokens_do_not_create_billing_fields() {
        let request = Bytes::from_static(br#"{"model":"m","input":"hi"}"#);
        let body = serde_json::json!({
            "id": "c1", "object": "chat.completion", "created": 0, "model": "m",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 2000, "completion_tokens": 2, "total_tokens": 2002,
                "prompt_tokens_details": {"cached_tokens": 1000}}
        });
        let translated = OpenResponses::new()
            .translate_response(&request, None, Bytes::from(body.to_string()))
            .unwrap();
        let response: Value = serde_json::from_slice(&translated).unwrap();
        assert_eq!(response["usage"]["input_tokens_details"]["cached_tokens"], 1000);
        assert_eq!(response["usage"]["input_tokens_details"]["cache_write_tokens"], 0);
        for field in ["cache_read_input_tokens", "cache_creation_input_tokens", "cache_creation"] {
            assert!(
                response["usage"].get(field).is_none(),
                "display-only counts must not synthesize {field}"
            );
        }
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
        let registry = TranslationRegistry::new(vec![Arc::new(OpenResponses::new())]);
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

        let registry = TranslationRegistry::new(vec![Arc::new(OpenResponses::new())]);
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

    /// An upstream error after partial output ends the Responses stream with an
    /// `error` event, never with `response.completed`, even when the upstream
    /// follows it with `[DONE]`.
    #[test]
    fn embedded_upstream_error_ends_the_stream_with_an_error_event() {
        let request = Bytes::from(r#"{"model":"m","input":"hi","stream":true}"#);
        let mut reframer = OpenResponses::new().stream_reframer(&request, None);
        let content = serde_json::json!({
            "id": "c1", "object": "chat.completion.chunk", "created": 0, "model": "m",
            "choices": [{"index": 0, "delta": {"content": "Hel"}}]
        });
        let error = serde_json::json!({
            "error": {"message": "The response stream was interrupted before it finished. Retry the request.", "type": "api_error", "param": null, "code": 502}
        });

        let mut out = reframer.push(&content);
        out.extend(reframer.push(&error));
        out.extend(reframer.push(&content));
        out.extend(reframer.finish());
        let text = String::from_utf8(out).unwrap();

        assert!(text.contains("event: response.output_text.delta"), "{text}");
        assert!(text.contains("event: error"), "{text}");
        assert!(text.contains("interrupted before it finished"), "{text}");
        assert!(!text.contains("response.completed"), "{text}");
        assert_eq!(text.matches("event: error").count(), 1, "{text}");
        assert!(
            text.rfind("event: error") > text.rfind("output_text.delta"),
            "nothing follows the error: {text}"
        );
    }

    /// A blocking reply whose message omits `role` (and other optional-per-spec
    /// top-level fields) must still translate. This is the shape a reassembled
    /// single-chunk stream takes when the backend never sends `role` on its
    /// deltas; `finish_reason: "length"` mimics a `max_output_tokens: 1` reply,
    /// which is single-chunk and therefore hits it deterministically.
    #[test]
    fn blocking_reply_missing_optional_fields_is_tolerated() {
        let request = Bytes::from(r#"{"model":"m","input":"hi","max_output_tokens":1}"#);
        let body = Bytes::from(r#"{"choices":[{"message":{"content":"Hello"},"finish_reason":"length"}]}"#);

        let out = OpenResponses::new()
            .translate_response(&request, None, body)
            .expect("must translate");

        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["object"], "response");
        assert_eq!(v["status"], "incomplete", "length-capped replies surface as incomplete");
        assert_eq!(v["model"], "m", "model backfilled from the request");
        assert_eq!(v["output"][0]["role"], "assistant", "missing role defaults to assistant");
        assert_eq!(v["output"][0]["content"][0]["text"], "Hello");
    }

    /// Streamed chunks whose deltas omit `role` (and whose envelopes omit
    /// `created`/`model`) must still reframe into Responses events rather than
    /// being silently dropped.
    #[tokio::test]
    async fn streamed_chunks_missing_optional_fields_are_reframed() {
        async fn fake_chat_sse(_req: Request) -> Response {
            let sse = concat!(
                "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"}}]}\n\n",
                "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
                "data: [DONE]\n\n",
            );
            let mut r = Response::new(Body::from(sse));
            r.headers_mut()
                .insert(header::CONTENT_TYPE, header::HeaderValue::from_static("text/event-stream"));
            r
        }

        let registry = TranslationRegistry::new(vec![Arc::new(OpenResponses::new())]);
        let inner = Router::new()
            .route("/responses", post(fake_chat_sse))
            .layer(axum::middleware::from_fn_with_state(registry, translation_middleware));
        let server = axum_test::TestServer::new(Router::new().nest("/ai/v1", inner)).expect("test server");

        let response = server
            .post("/ai/v1/responses")
            .json(&serde_json::json!({ "model": "gpt-4o", "input": "hi", "stream": true, "max_output_tokens": 1 }))
            .await;

        assert_eq!(response.status_code().as_u16(), 200);
        let text = response.text();
        assert!(text.contains("event: response.output_text.delta"), "chunk was dropped:\n{text}");
        assert!(text.contains("Hi"), "missing text delta in:\n{text}");
    }

    /// A 2xx body the translator genuinely cannot translate must surface as a
    /// status the batch daemon will NOT retry (it retries >=500, 429, 408 and
    /// 404): the upstream succeeded, so every retry fails identically.
    #[tokio::test]
    async fn untranslatable_success_body_is_a_non_retriable_status() {
        async fn bad_handler(_req: Request) -> Response {
            let mut r = Response::new(Body::from("not json at all"));
            r.headers_mut()
                .insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/json"));
            r
        }

        let registry = TranslationRegistry::new(vec![Arc::new(OpenResponses::new())]);
        let inner = Router::new()
            .route("/responses", post(bad_handler))
            .layer(axum::middleware::from_fn_with_state(registry, translation_middleware));
        let server = axum_test::TestServer::new(Router::new().nest("/ai/v1", inner)).expect("test server");

        let response = server
            .post("/ai/v1/responses")
            .json(&serde_json::json!({ "model": "gpt-4o", "input": "hi" }))
            .await;

        assert_eq!(response.status_code().as_u16(), 422);
        let body: serde_json::Value = response.json();
        assert_eq!(body["error"]["message"], "response translation failed");
    }
}
