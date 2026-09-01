//! Strict mode router with typed handlers and schema validation
//!
//! This module provides an alternative router that validates requests against
//! OpenAI API schemas before forwarding them. Unlike the default passthrough
//! router, strict mode:
//!
//! - Only accepts known OpenAI API paths
//! - Validates request bodies against typed schemas via serde
//! - Rejects unknown paths with 404
//! - Supports the Open Responses adapter for backends that only support Chat Completions
//!
//! # Usage
//!
//! ```ignore
//! use onwards::strict::build_strict_router;
//! use onwards::AppState;
//!
//! let app_state = AppState::new(targets);
//! let router = build_strict_router(app_state);
//! ```

pub mod handlers;
pub mod schemas;

use crate::AppState;
use crate::client::HttpClient;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use tracing::info;

pub use schemas::chat_completions::{ChatCompletionRequest, ChatCompletionResponse};
pub use schemas::responses::{ResponsesRequest, ResponsesResponse, ResponsesStreamingEvent};

/// Build a strict router with typed handlers and schema validation.
///
/// Unlike `build_router()`, this router:
/// - Only accepts known OpenAI API paths
/// - Validates request bodies against typed schemas
/// - Returns 404 for unknown paths (no wildcard)
///
/// # Routes
///
/// - `POST /v1/chat/completions` - Chat completions with schema validation
/// - `POST /v1/completions` - Legacy text completions (proxied to upstream /v1/completions)
/// - `POST /v1/responses` - Open Responses API (validated, optional adapter)
/// - `POST /v1/embeddings` - Embeddings API with schema validation
/// - `GET /v1/models` - List available models
/// - `GET /models` - List available models (alias)
///
/// # Example
///
/// ```ignore
/// use onwards::{AppState, target::Targets};
/// use onwards::strict::build_strict_router;
///
/// let targets = Targets::from_config_file(&"config.json".into()).await?;
/// let app_state = AppState::new(targets);
/// let router = build_strict_router(app_state);
/// ```
pub fn build_strict_router<T: HttpClient + Clone + Send + Sync + 'static>(
    state: AppState<T>,
) -> Router {
    info!("Building strict router with schema validation");

    Router::new()
        // Models endpoints
        .route("/models", get(handlers::models_handler::<T>))
        // Chat completions
        .route(
            "/chat/completions",
            post(handlers::chat_completions_handler::<T>),
        )
        // Anthropic Messages ingress alias. Foreign-protocol translation happens
        // at the dwctl edge (the request body is already Chat Completions and the
        // path is normalised to `/chat/completions` by the time it reaches here);
        // this alias only exists so strict-mode routing matches `/messages` and
        // dispatches to the chat-completions handler. No Anthropic logic lives in
        // onwards. Non-strict mode needs no alias (its catch-all already matches).
        .route("/messages", post(handlers::chat_completions_handler::<T>))
        // Legacy text completions
        .route("/completions", post(handlers::completions_handler::<T>))
        // Open Responses
        .route("/responses", post(handlers::responses_handler::<T>))
        // Embeddings
        .route("/embeddings", post(handlers::embeddings_handler::<T>))
        // Without this layer the `Json` extractors above fall back to Axum's
        // 2 MB default and reject larger payloads with a 413.
        .layer(DefaultBodyLimit::max(state.body_limit))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{Target, Targets};
    use crate::test_utils::MockHttpClient;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use dashmap::DashMap;
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn create_test_app_state() -> AppState<MockHttpClient> {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: true,
            http_pool_config: None,
        };

        let mock_response = r#"{"id":"chatcmpl-123","object":"chat.completion","choices":[{"message":{"role":"assistant","content":"Hello!"}}]}"#;
        AppState::with_client(targets, MockHttpClient::new(StatusCode::OK, mock_response))
    }

    #[tokio::test]
    async fn test_strict_router_rejects_unknown_paths() {
        let state = create_test_app_state();
        let router = build_strict_router(state);

        let request = Request::builder()
            .uri("/unknown/endpoint")
            .body(Body::empty())
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_strict_router_accepts_models_endpoint() {
        let state = create_test_app_state();
        let router = build_strict_router(state);

        let request = Request::builder()
            .uri("/models")
            .body(Body::empty())
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Build a valid chat completions body padded to at least `size` bytes.
    fn padded_chat_completions_body(size: usize) -> String {
        let padding = "x".repeat(size);
        format!(r#"{{"model": "gpt-4", "messages": [{{"role": "user", "content": "{padding}"}}]}}"#)
    }

    #[tokio::test]
    async fn test_strict_router_accepts_body_over_axum_2mb_default() {
        // Regression test: without an explicit DefaultBodyLimit layer the Json
        // extractors fall back to Axum's 2 MB default and 413 anything larger.
        let state = create_test_app_state();
        let router = build_strict_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(padded_chat_completions_body(3 * 1024 * 1024)))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_strict_router_messages_alias_routes_to_chat_completions() {
        // The Anthropic ingress alias: a (already edge-translated) Chat
        // Completions body posted to `/messages` must route to the
        // chat-completions handler, exactly like `/chat/completions`. Foreign
        // translation and path normalisation happen upstream at the dwctl edge;
        // onwards just needs the route to match.
        let state = create_test_app_state();
        let router = build_strict_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/messages")
            .header("content-type", "application/json")
            .body(Body::from(padded_chat_completions_body(16)))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// The dwctl edge translates a Responses request into Chat Completions and
    /// normalises the path to `/chat/completions`, but that rewrite cannot
    /// re-trigger route matching through a nest, so the request still arrives on
    /// the `/responses` route carrying a Chat Completions body. `responses_handler`
    /// must recognise the normalised path and hand it to the chat handler rather
    /// than rejecting it against the Responses schema.
    ///
    /// Driven through the handler directly because routing in a test follows the
    /// URI, so a router-level test cannot reproduce that path/route mismatch.
    #[tokio::test]
    async fn test_responses_handler_dispatches_edge_translated_body_to_chat() {
        let state = create_test_app_state();

        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(padded_chat_completions_body(16)))
            .unwrap();

        let response = handlers::responses_handler(
            axum::extract::State(state),
            axum::http::HeaderMap::new(),
            request,
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "an edge-translated chat body on /responses must be served by the chat handler"
        );
    }

    #[tokio::test]
    async fn test_strict_router_accepts_base64_image_over_axum_2mb_default() {
        // The prod failure mode for COR-440: a vision request whose base64
        // data URL pushes the body past Axum's old 2 MB extractor default.
        // Also exercises the image_url content-part schema with a large URL.
        let state = create_test_app_state();
        let router = build_strict_router(state);

        // ~3 MB of valid base64 (must be a multiple of 4 chars).
        let base64_data = "QUJD".repeat(3 * 1024 * 1024 / 4);
        let body = serde_json::json!({
            "model": "gpt-4",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "What is in this image?"},
                    {"type": "image_url", "image_url": {"url": format!("data:image/jpeg;base64,{base64_data}")}}
                ]
            }]
        });
        assert!(body.to_string().len() > 2 * 1024 * 1024);

        let request = Request::builder()
            .method("POST")
            .uri("/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_strict_router_rejects_body_over_configured_limit() {
        let state = create_test_app_state().with_body_limit(1024);
        let router = build_strict_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(padded_chat_completions_body(2048)))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// Build strict-mode state whose upstream returns `body` verbatim.
    fn create_reasoning_test_app_state(body: &str) -> AppState<MockHttpClient> {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: true,
            http_pool_config: None,
        };

        AppState::with_client(targets, MockHttpClient::new(StatusCode::OK, body))
    }

    /// Guards the handler wiring, not just the schema helper: a refactor that
    /// stops calling `canonicalise_reasoning` would still pass the unit tests
    /// in `schemas::chat_completions` but fail here.
    #[tokio::test]
    async fn strict_chat_completions_rewrites_openrouter_reasoning_field() {
        let upstream = r#"{
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "created": 1234567890,
            "model": "gpt-4",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "42",
                    "reasoning": "let me think about this"
                },
                "finish_reason": "stop"
            }]
        }"#;

        let router = build_strict_router(create_reasoning_test_app_state(upstream));

        let request = Request::builder()
            .method("POST")
            .uri("/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}"#,
            ))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        let message = &json["choices"][0]["message"];

        assert_eq!(
            message["reasoning_content"], "let me think about this",
            "upstream `reasoning` should be rewritten onto `reasoning_content`"
        );
        assert!(
            message.get("reasoning").is_none(),
            "the OpenRouter spelling should not reach the caller"
        );
    }

    /// The vLLM spelling is already canonical, so it must survive untouched.
    #[tokio::test]
    async fn strict_chat_completions_leaves_vllm_reasoning_content_alone() {
        let upstream = r#"{
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "created": 1234567890,
            "model": "gpt-4",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "42",
                    "reasoning_content": "vllm style reasoning"
                },
                "finish_reason": "stop"
            }]
        }"#;

        let router = build_strict_router(create_reasoning_test_app_state(upstream));

        let request = Request::builder()
            .method("POST")
            .uri("/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}"#,
            ))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        let message = &json["choices"][0]["message"];

        assert_eq!(message["reasoning_content"], "vllm style reasoning");
        assert!(message.get("reasoning").is_none());
    }

    /// Same guarantee on the streaming path, which sanitises chunk-by-chunk in
    /// a separate code path from the non-streaming handler.
    #[tokio::test]
    async fn strict_streaming_chat_completions_rewrites_reasoning_field() {
        let chunks = concat!(
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,",
            "\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"first \"},",
            "\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,",
            "\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"second\"},",
            "\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n",
        );

        let router = build_strict_router(create_reasoning_test_app_state(chunks));

        let request = Request::builder()
            .method("POST")
            .uri("/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"gpt-4","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
            ))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body_text = std::str::from_utf8(&body_bytes).unwrap();

        assert!(
            body_text.contains("reasoning_content"),
            "streamed deltas should carry the canonical spelling; got: {body_text}"
        );
        assert!(
            !body_text.contains("\"reasoning\""),
            "the OpenRouter spelling should not survive in any chunk; got: {body_text}"
        );
    }

    #[tokio::test]
    async fn test_responses_forwarded_upstream_as_passthrough() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4o".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: true,
            http_pool_config: None,
        };

        // Mock response in Responses format (as if upstream supports it)
        let mock_response = r#"{
            "id": "resp_abc123",
            "object": "response",
            "created_at": 1234567890,
            "completed_at": 1234567900,
            "status": "completed",
            "incomplete_details": null,
            "model": "gpt-4o",
            "previous_response_id": null,
            "instructions": null,
            "output": [],
            "error": null,
            "tools": [],
            "tool_choice": "auto",
            "truncation": "disabled",
            "parallel_tool_calls": true,
            "text": {
                "format": {
                    "type": "text"
                }
            },
            "top_p": 1.0,
            "presence_penalty": 0.0,
            "frequency_penalty": 0.0,
            "top_logprobs": 0,
            "temperature": 1.0,
            "reasoning": null,
            "usage": null,
            "max_output_tokens": null,
            "max_tool_calls": null,
            "store": false,
            "background": false,
            "service_tier": "default",
            "metadata": null,
            "safety_identifier": null,
            "prompt_cache_key": null
        }"#;
        let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        let state = AppState::with_client(targets, mock_client.clone());
        let router = build_strict_router(state);

        // Send a Responses API request
        let request_body = r#"{
            "model": "gpt-4o",
            "input": "Hello"
        }"#;

        let request = Request::builder()
            .method("POST")
            .uri("/responses")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Verify the mock client received the request
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        // In passthrough mode, request should go to /v1/responses (not chat/completions)
        assert!(requests[0].uri.contains("/responses"));
    }

    fn create_completions_test_app_state() -> (AppState<MockHttpClient>, MockHttpClient) {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-3.5-turbo-instruct".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .build()
                .into_pool(),
        );

        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: true,
            http_pool_config: None,
        };

        let mock_response = r#"{
            "id": "cmpl-abc123",
            "object": "text_completion",
            "created": 1677652288,
            "model": "gpt-3.5-turbo-instruct",
            "choices": [{"text": "Hello!", "index": 0, "logprobs": null, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 7, "total_tokens": 12}
        }"#;
        let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        (
            AppState::with_client(targets, mock_client.clone()),
            mock_client,
        )
    }

    /// The strict router accepts POST /completions with a valid request body
    #[tokio::test]
    async fn test_strict_router_accepts_completions_endpoint() {
        let (state, _) = create_completions_test_app_state();
        let router = build_strict_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"gpt-3.5-turbo-instruct","prompt":"Say hello"}"#,
            ))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // Response is in legacy completions format
        assert_eq!(body_json["object"], "text_completion");
        assert!(body_json["choices"].is_array());
        assert!(body_json["choices"][0]["text"].is_string());
    }

    #[tokio::test]
    async fn test_strict_router_rejects_reasoning_on_completions_endpoint() {
        let (state, mock_client) = create_completions_test_app_state();
        let router = build_strict_router(state);
        let request = Request::builder()
            .method("POST")
            .uri("/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"gpt-3.5-turbo-instruct","prompt":"Say hello","reasoning_effort":"low"}"#,
            ))
            .unwrap();

        let response = router.clone().oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["param"], "reasoning_effort");
        assert_eq!(body["error"]["code"], "unsupported_parameter");

        let request = Request::builder()
            .method("POST")
            .uri("/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"gpt-3.5-turbo-instruct","prompt":"Say hello","thinking":false}"#,
            ))
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["param"], "thinking");
        assert_eq!(body["error"]["code"], "unsupported_parameter");

        let request = Request::builder()
            .method("POST")
            .uri("/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"gpt-3.5-turbo-instruct","prompt":"Say hello","thinking_token_budget":1024}"#,
            ))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["param"], "thinking_token_budget");
        assert_eq!(body["error"]["code"], "unsupported_parameter");
        assert!(mock_client.get_requests().is_empty());
    }

    /// The strict router forwards to the upstream /completions endpoint (not /chat/completions)
    #[tokio::test]
    async fn test_completions_proxied_to_upstream_completions() {
        let (state, mock_client) = create_completions_test_app_state();
        let router = build_strict_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"gpt-3.5-turbo-instruct","prompt":"Hello"}"#,
            ))
            .unwrap();

        router.oneshot(request).await.unwrap();

        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].uri.contains("completions"),
            "Should proxy to /completions, got: {}",
            requests[0].uri
        );
        assert!(
            !requests[0].uri.contains("chat"),
            "Must NOT proxy to /chat/completions"
        );
    }

    /// The strict router accepts POST /completions without a prompt — prompt is optional per the
    /// OpenAI spec (defaults to `<|endoftext|>` server-side)
    #[tokio::test]
    async fn test_completions_accepts_missing_prompt() {
        let (state, _) = create_completions_test_app_state();
        let router = build_strict_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/completions")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model":"gpt-3.5-turbo-instruct"}"#))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Streaming completions: SSE chunks contain text_completion objects
    #[tokio::test]
    async fn test_completions_streaming_response_format() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-3.5-turbo-instruct".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .build()
                .into_pool(),
        );
        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: true,
            http_pool_config: None,
        };

        let chunks = vec![
            "data: {\"id\":\"cmpl-abc\",\"object\":\"text_completion\",\"created\":1677652288,\"model\":\"gpt-3.5-turbo-instruct\",\"choices\":[{\"text\":\"Hello\",\"index\":0,\"logprobs\":null,\"finish_reason\":null}]}\n\n".to_string(),
            "data: {\"id\":\"cmpl-abc\",\"object\":\"text_completion\",\"created\":1677652288,\"model\":\"gpt-3.5-turbo-instruct\",\"choices\":[{\"text\":\" world\",\"index\":0,\"logprobs\":null,\"finish_reason\":null}]}\n\n".to_string(),
            "data: {\"id\":\"cmpl-abc\",\"object\":\"text_completion\",\"created\":1677652288,\"model\":\"gpt-3.5-turbo-instruct\",\"choices\":[{\"text\":\"\",\"index\":0,\"logprobs\":null,\"finish_reason\":\"stop\"}]}\n\n".to_string(),
            "data: [DONE]\n\n".to_string(),
        ];
        let mock_client = MockHttpClient::new_streaming(StatusCode::OK, chunks);
        let state = AppState::with_client(targets, mock_client);
        let router = build_strict_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"gpt-3.5-turbo-instruct","prompt":"Hello","stream":true}"#,
            ))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(content_type.contains("text/event-stream"));

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = std::str::from_utf8(&body).unwrap();

        // Each SSE chunk is a text_completion object
        assert!(body_str.contains("\"object\":\"text_completion\""));
        assert!(body_str.contains("\"text\":\"Hello\""));
        assert!(body_str.contains("\"text\":\" world\""));
        assert!(body_str.contains("[DONE]"));
    }

    // ── scheduling priority policy ───────────────────────────────────────────
    //
    // `priority` steers the dynamo scheduler's queue, so it is tri-level on the
    // authenticating key's purpose: `batch` and `continuation` pass through,
    // everyone else is stripped from BOTH the typed field and (for chat) the
    // flattened extras.

    /// Build a strict app whose single key carries `purpose`, exactly as dwctl's
    /// onwards sync stamps it.
    fn app_with_key_purpose(purpose: Option<&str>) -> (AppState<MockHttpClient>, MockHttpClient) {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "dsv4-flash".to_string(),
            Target::builder()
                .url("https://api.example.com/v1/".parse().unwrap())
                .build()
                .into_pool(),
        );
        let key_labels = Arc::new(DashMap::new());
        if let Some(purpose) = purpose {
            key_labels.insert(
                "sk-test".to_string(),
                std::collections::HashMap::from([("purpose".to_string(), purpose.to_string())]),
            );
        }

        let targets = Targets {
            targets,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels,
            strict_mode: true,
            http_pool_config: None,
        };
        let mock_client = MockHttpClient::new(
            StatusCode::OK,
            r#"{"id":"cmpl-1","object":"text_completion","created":0,"model":"dsv4-flash","choices":[]}"#,
        );
        (
            AppState::with_client(targets, mock_client.clone()),
            mock_client,
        )
    }

    async fn forwarded_body(
        state: AppState<MockHttpClient>,
        mock_client: &MockHttpClient,
        uri: &str,
        body: &str,
    ) -> serde_json::Value {
        let request = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .header("authorization", "Bearer sk-test")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = build_strict_router(state).oneshot(request).await.unwrap();
        assert!(
            response.status().is_success(),
            "{uri}: {}",
            response.status()
        );
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1, "{uri}");
        serde_json::from_slice(&requests[0].body).unwrap()
    }

    /// Onwards VALIDATES and then forwards the ORIGINAL bytes (COR-522) — it is
    /// not the enforcement point for privileged fields. `priority` is stripped
    /// from external traffic by dwctl's inference middleware (which fusillade
    /// daemon requests bypass, and continuation resume legs enter below); this
    /// test pins that onwards itself never eats the field, for any key, so
    /// enforcement lives in exactly one place.
    #[tokio::test]
    async fn scheduling_fields_forward_verbatim_for_every_key() {
        for purpose in [Some("realtime"), Some("continuation"), None] {
            let (state, mock_client) = app_with_key_purpose(purpose);
            let body = forwarded_body(
                state,
                &mock_client,
                "/completions",
                r#"{"model":"dsv4-flash","prompt":[1,2,3],"priority":100,"stream_options":{"include_usage":true}}"#,
            )
            .await;
            assert_eq!(
                body["priority"], 100,
                "purpose={purpose:?}: onwards must not eat the field"
            );
            assert_eq!(body["stream_options"]["include_usage"], true);
        }
    }

    /// REGRESSION: fusillade derives a NEGATIVE priority from each batch
    /// request's deadline, so batch work sorts behind realtime traffic and,
    /// within itself, by urgency. Stripping it would silently flatten batch
    /// scheduling to one tier — a live behaviour change on the busiest path
    /// through the platform, invisible from the response.
    ///
    /// Asserted on chat, which is what fusillade actually sends.
    #[tokio::test]
    async fn a_batch_keys_deadline_priority_survives_re_serialization() {
        for (uri, body) in [
            (
                "/chat/completions",
                r#"{"model":"dsv4-flash","messages":[{"role":"user","content":"hi"}],"priority":-1754812800}"#,
            ),
            (
                "/completions",
                r#"{"model":"dsv4-flash","prompt":[1,2,3],"priority":-1754812800}"#,
            ),
        ] {
            let (state, mock_client) = app_with_key_purpose(Some("batch"));
            let forwarded = forwarded_body(state, &mock_client, uri, body).await;
            assert_eq!(
                forwarded["priority"], -1754812800,
                "{uri}: a batch key's deadline-derived priority must reach the scheduler"
            );
        }
    }

    /// Verbatim forwarding applies to unmodelled fields too, on BOTH endpoints:
    /// the schema is a validation surface, not a filter (COR-522 moved body
    /// manipulation to the dwctl edge). This pins the contract so nobody
    /// reintroduces per-field dropping here and silently breaks engine knobs
    /// that ride through today.
    #[tokio::test]
    async fn unmodelled_fields_forward_verbatim_after_validation() {
        for (uri, body) in [
            (
                "/completions",
                r#"{"model":"dsv4-flash","prompt":[1,2],"ignore_eos":true,"repetition_penalty":1.1,"custom_knob":true}"#,
            ),
            (
                "/chat/completions",
                r#"{"model":"dsv4-flash","messages":[{"role":"user","content":"hi"}],"custom_knob":true}"#,
            ),
        ] {
            let (state, mock_client) = app_with_key_purpose(Some("realtime"));
            let forwarded = forwarded_body(state, &mock_client, uri, body).await;
            assert_eq!(
                forwarded["custom_knob"], true,
                "{uri}: original bytes forward untouched"
            );
        }
    }
}
