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

pub mod adapter;
pub mod handlers;
pub mod schemas;
pub mod streaming;

/// Merge reasoning text from the various provider-specific fields, deduplicating
/// identical content. Order: `reasoning_content` → `reasoning` → `reasoning_details`.
pub(crate) fn merge_reasoning_text(
    reasoning: Option<&String>,
    reasoning_content: Option<&String>,
    reasoning_details: Option<&Vec<serde_json::Value>>,
) -> String {
    let mut parts: Vec<&str> = Vec::new();

    if let Some(rc) = reasoning_content
        && !rc.is_empty()
    {
        parts.push(rc);
    }
    if let Some(r) = reasoning
        && !r.is_empty()
        && !parts.contains(&r.as_str())
    {
        parts.push(r);
    }
    if let Some(details) = reasoning_details {
        for detail in details {
            if let Some(text) = detail.get("text").and_then(|v| v.as_str())
                && !text.is_empty()
                && !parts.contains(&text)
            {
                parts.push(text);
            }
        }
    }

    parts.join("\n")
}

/// Convert chat completions `Usage` to Responses API `ResponseUsage`,
/// extracting `reasoning_tokens` and `cached_tokens` from the raw JSON detail fields.
/// Values are clamped to `u32::MAX` to guard against wraparound on untrusted input.
pub(crate) fn chat_usage_to_response_usage(
    u: &schemas::chat_completions::Usage,
) -> schemas::responses::ResponseUsage {
    fn extract(details: Option<&serde_json::Value>, key: &str) -> u32 {
        details
            .and_then(|v| v.get(key))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32
    }

    let cached_tokens = extract(u.prompt_tokens_details.as_ref(), "cached_tokens");
    let reasoning_tokens = extract(u.completion_tokens_details.as_ref(), "reasoning_tokens");

    schemas::responses::ResponseUsage {
        input_tokens: u.prompt_tokens,
        output_tokens: u.completion_tokens,
        total_tokens: u.total_tokens,
        input_tokens_details: schemas::responses::InputTokensDetails { cached_tokens },
        output_tokens_details: schemas::responses::OutputTokensDetails { reasoning_tokens },
    }
}

/// Merge two optional usage detail JSON objects, summing matching numeric fields.
///
/// Used to accumulate `prompt_tokens_details` / `completion_tokens_details` across
/// tool-loop iterations so that per-field counts (`cached_tokens`, `reasoning_tokens`,
/// etc.) stay consistent with the summed top-level totals.
///
/// Behavior:
/// - If only one side has a value, it is returned as-is.
/// - If both are JSON objects, the result is an object containing the union of keys,
///   with matching numeric (`u64`) fields summed. Non-numeric fields prefer the
///   newer value.
/// - If either side is not an object, the newer value wins (can't meaningfully merge).
pub(crate) fn merge_usage_details(
    prev: Option<serde_json::Value>,
    next: Option<serde_json::Value>,
) -> Option<serde_json::Value> {
    match (prev, next) {
        (None, None) => None,
        (Some(v), None) | (None, Some(v)) => Some(v),
        (Some(prev), Some(next)) => {
            let (Some(mut merged), Some(next_obj)) = (prev.as_object().cloned(), next.as_object())
            else {
                return Some(next);
            };
            for (key, next_val) in next_obj {
                match merged.get(key) {
                    Some(existing) if existing.is_u64() && next_val.is_u64() => {
                        let sum = existing.as_u64().unwrap_or(0) + next_val.as_u64().unwrap_or(0);
                        merged.insert(key.clone(), sum.into());
                    }
                    _ => {
                        merged.insert(key.clone(), next_val.clone());
                    }
                }
            }
            Some(serde_json::Value::Object(merged))
        }
    }
}

use crate::AppState;
use crate::client::HttpClient;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use tracing::info;

pub use adapter::OpenResponsesAdapter;
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
    use crate::target::{OpenResponsesConfig, Target, Targets};
    use crate::test_utils::MockHttpClient;
    use crate::traits::{RequestContext, ToolError, ToolExecutor, ToolSchema};
    use async_trait::async_trait;
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

    /// Create test app state with adapter mode enabled
    fn create_adapter_test_app_state() -> (AppState<MockHttpClient>, MockHttpClient) {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4o".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .open_responses(OpenResponsesConfig { adapter: true })
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

        // Mock response that simulates a Chat Completions response
        let mock_response = r#"{
            "id": "chatcmpl-abc123",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello! How can I help you today?"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 8,
                "total_tokens": 18
            }
        }"#;
        let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        (
            AppState::with_client(targets, mock_client.clone()),
            mock_client,
        )
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

    #[tokio::test]
    async fn test_adapter_converts_responses_to_chat_completions() {
        let (state, mock_client) = create_adapter_test_app_state();
        let router = build_strict_router(state);

        // Send a Responses API request
        let request_body = r#"{
            "model": "gpt-4o",
            "input": "Hello, how are you?"
        }"#;

        let request = Request::builder()
            .method("POST")
            .uri("/responses")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();

        // Check that we got a successful response
        assert_eq!(response.status(), StatusCode::OK);

        // Parse the response body
        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let response_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

        // Verify the response is in Open Responses format
        assert_eq!(response_json["object"], "response");
        assert!(response_json["id"].as_str().unwrap().starts_with("resp_"));
        assert_eq!(response_json["status"], "completed");

        // Verify output items exist
        let output = response_json["output"].as_array().unwrap();
        assert!(!output.is_empty());

        // Verify the mock client received a Chat Completions request
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        // Check the request was sent to chat completions endpoint
        assert!(requests[0].uri.contains("chat/completions"));

        // Parse the request body to verify conversion
        let request_json: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(request_json["model"], "gpt-4o");

        // Verify messages array was created from input
        let messages = request_json["messages"].as_array().unwrap();
        assert!(!messages.is_empty());
        assert_eq!(messages[0]["role"], "user");
    }

    #[tokio::test]
    async fn test_adapter_with_instructions() {
        let (state, mock_client) = create_adapter_test_app_state();
        let router = build_strict_router(state);

        // Send a Responses API request with instructions
        let request_body = r#"{
            "model": "gpt-4o",
            "input": "What's 2 + 2?",
            "instructions": "You are a helpful math tutor."
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

        // Parse the request body to verify system message was added
        let request_json: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let messages = request_json["messages"].as_array().unwrap();

        // Should have system message first, then user message
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are a helpful math tutor.");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "What's 2 + 2?");
    }

    #[tokio::test]
    async fn test_passthrough_mode_without_adapter() {
        // Create a target WITHOUT adapter enabled
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4o".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                // No open_responses config - defaults to passthrough
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

    #[tokio::test]
    async fn test_streaming_adapter_mode() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4o".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .open_responses(OpenResponsesConfig { adapter: true })
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

        // Mock streaming SSE response
        let chunks = vec![
            "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n".to_string(),
            "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n".to_string(),
            "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" there!\"},\"finish_reason\":null}]}\n\n".to_string(),
            "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_string(),
            "data: [DONE]\n\n".to_string(),
        ];
        let mock_client = MockHttpClient::new_streaming(StatusCode::OK, chunks);
        let state = AppState::with_client(targets, mock_client.clone());
        let router = build_strict_router(state);

        // Send a streaming Responses API request
        let request_body = r#"{
            "model": "gpt-4o",
            "input": "Hello",
            "stream": true
        }"#;

        let request = Request::builder()
            .method("POST")
            .uri("/responses")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();

        // Check status
        assert_eq!(response.status(), StatusCode::OK);

        // Check content type is SSE
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(content_type.contains("text/event-stream"));

        // Read the streaming response
        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body_text = std::str::from_utf8(&body_bytes).unwrap();

        // Verify we got Open Responses semantic events
        assert!(body_text.contains("response.created"));
        assert!(body_text.contains("response.output_item.added"));
        assert!(body_text.contains("response.output_text.delta"));
        assert!(body_text.contains("response.completed"));

        // Verify the mock client received the request
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        // Check the request was sent to chat completions with stream: true
        assert!(requests[0].uri.contains("chat/completions"));
        let request_json: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(request_json["stream"], true);
    }

    #[tokio::test]
    async fn test_streaming_adapter_preserves_utf8_split_across_transport_chunks() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4o".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .open_responses(OpenResponsesConfig { adapter: true })
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

        let event = "data: {\"id\":\"chatcmpl-utf8\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"café\"},\"finish_reason\":\"stop\"}]}\n\n";
        let event = event.as_bytes();
        let split = event
            .windows(2)
            .position(|window| window == "é".as_bytes())
            .expect("fixture must contain a multibyte character")
            + 1;
        let chunks = vec![
            event[..split].to_vec(),
            event[split..].to_vec(),
            b"data: [DONE]\n\n".to_vec(),
        ];

        let mock_client = MockHttpClient::new_streaming_bytes(StatusCode::OK, chunks);
        let state = AppState::with_client(targets, mock_client);
        let router = build_strict_router(state);
        let request = Request::builder()
            .method("POST")
            .uri("/responses")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"gpt-4o","input":"Hello","stream":true}"#,
            ))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(
            body.contains("café"),
            "split UTF-8 model output was lost: {body}"
        );
    }

    /// Test streaming with tool_calls finish reason.
    ///
    /// With the default NoOpToolExecutor, all tools are unhandled, so the
    /// handler stops after one iteration (correct behavior — unhandled tools
    /// are passed through to the client). Multi-iteration tool loop mechanics
    /// are exercised by the streaming::tests unit tests.
    #[tokio::test]
    async fn test_streaming_adapter_tool_calls_unhandled() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4o".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .open_responses(OpenResponsesConfig { adapter: true })
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

        // Streaming response that finishes with tool_calls
        let chunks = vec![
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n".to_string(),
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_xyz\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n".to_string(),
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"location\\\":\\\"Paris\\\"}\"}}]},\"finish_reason\":null}]}\n\n".to_string(),
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n".to_string(),
            "data: [DONE]\n\n".to_string(),
        ];

        let mock_client = MockHttpClient::new_streaming(StatusCode::OK, chunks);
        let state = AppState::with_client(targets, mock_client.clone());
        let router = build_strict_router(state);

        let request_body = r#"{
            "model": "gpt-4o",
            "input": "What's the weather in Paris?",
            "stream": true,
            "tools": [{
                "type": "function",
                "name": "get_weather",
                "description": "Get the current weather for a location",
                "parameters": {"type": "object", "properties": {"location": {"type": "string"}}}
            }]
        }"#;

        let request = Request::builder()
            .method("POST")
            .uri("/responses")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(content_type.contains("text/event-stream"));

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body_text = std::str::from_utf8(&body_bytes).unwrap();

        // With NoOpToolExecutor, tools are unhandled → single iteration, then complete
        assert!(body_text.contains("response.created"));
        assert!(body_text.contains("response.output_item.added"));
        assert!(body_text.contains("response.completed"));

        // Only one upstream request — no second iteration because tools are unhandled
        let requests = mock_client.get_requests();
        assert_eq!(
            requests.len(),
            1,
            "Should make only 1 request (tools are unhandled by NoOpToolExecutor)"
        );
        assert!(requests[0].uri.contains("chat/completions"));

        // Verify stream_options.include_usage was set
        let request_json: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(request_json["stream"], true);
        assert_eq!(
            request_json["stream_options"]["include_usage"], true,
            "Should set include_usage for streaming"
        );
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

    /// A tool executor that advertises a single server-side tool.
    struct CollisionTestExecutor;

    #[async_trait]
    impl ToolExecutor for CollisionTestExecutor {
        async fn tools(&self, _ctx: &RequestContext) -> Vec<ToolSchema> {
            vec![ToolSchema {
                name: "get_weather".to_string(),
                description: "Get weather for a location".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "location": { "type": "string" }
                    }
                }),
                strict: false,
                kind: Default::default(),
            }]
        }

        async fn execute(
            &self,
            _tool_name: &str,
            _tool_call_id: &str,
            _arguments: &serde_json::Value,
            _ctx: &RequestContext,
        ) -> Result<serde_json::Value, ToolError> {
            Ok(serde_json::json!({"temp": 20}))
        }
    }

    #[tokio::test]
    async fn test_duplicate_tool_name_returns_400() {
        let targets = Arc::new(DashMap::new());
        targets.insert(
            "gpt-4o".to_string(),
            Target::builder()
                .url("https://api.openai.com/v1/".parse().unwrap())
                .open_responses(OpenResponsesConfig { adapter: true })
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

        let mock_response = r#"{"id":"chatcmpl-1","object":"chat.completion","choices":[{"message":{"role":"assistant","content":"Hi"}}]}"#;
        let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
        let state = AppState::with_client(targets, mock_client)
            .with_tool_executor(Arc::new(CollisionTestExecutor));
        let router = build_strict_router(state);

        // Client sends both a Function tool and a HostedTool with the same name
        let request_body = r#"{
            "model": "gpt-4o",
            "input": "What's the weather?",
            "tools": [
                {
                    "type": "function",
                    "name": "get_weather",
                    "description": "Client-side get_weather",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
                },
                {
                    "type": "hosted_tool",
                    "name": "get_weather"
                }
            ]
        }"#;

        let request = Request::builder()
            .method("POST")
            .uri("/responses")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            body_json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("get_weather"),
            "Error should mention the colliding tool name"
        );
    }

    #[test]
    fn test_chat_usage_to_response_usage_extracts_details() {
        let usage = schemas::chat_completions::Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            prompt_tokens_details: Some(serde_json::json!({
                "cached_tokens": 42,
                "audio_tokens": 0,
            })),
            completion_tokens_details: Some(serde_json::json!({
                "reasoning_tokens": 30,
                "accepted_prediction_tokens": 0,
            })),
        };

        let converted = chat_usage_to_response_usage(&usage);
        assert_eq!(converted.input_tokens, 100);
        assert_eq!(converted.output_tokens, 50);
        assert_eq!(converted.total_tokens, 150);
        assert_eq!(converted.input_tokens_details.cached_tokens, 42);
        assert_eq!(converted.output_tokens_details.reasoning_tokens, 30);
    }

    #[test]
    fn test_chat_usage_to_response_usage_missing_details() {
        let usage = schemas::chat_completions::Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            prompt_tokens_details: None,
            completion_tokens_details: None,
        };

        let converted = chat_usage_to_response_usage(&usage);
        assert_eq!(converted.input_tokens_details.cached_tokens, 0);
        assert_eq!(converted.output_tokens_details.reasoning_tokens, 0);
    }

    #[test]
    fn test_chat_usage_to_response_usage_clamps_oversized_values() {
        let usage = schemas::chat_completions::Usage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            prompt_tokens_details: Some(serde_json::json!({
                "cached_tokens": u64::MAX,
            })),
            completion_tokens_details: Some(serde_json::json!({
                "reasoning_tokens": (u32::MAX as u64) + 1,
            })),
        };

        let converted = chat_usage_to_response_usage(&usage);
        // Clamped to u32::MAX instead of silently wrapping.
        assert_eq!(converted.input_tokens_details.cached_tokens, u32::MAX);
        assert_eq!(converted.output_tokens_details.reasoning_tokens, u32::MAX);
    }

    #[test]
    fn test_merge_usage_details_sums_numeric_fields() {
        let prev = Some(serde_json::json!({
            "cached_tokens": 10,
            "audio_tokens": 2,
        }));
        let next = Some(serde_json::json!({
            "cached_tokens": 15,
            "audio_tokens": 3,
        }));

        let merged = merge_usage_details(prev, next).unwrap();
        assert_eq!(merged["cached_tokens"], 25);
        assert_eq!(merged["audio_tokens"], 5);
    }

    #[test]
    fn test_merge_usage_details_union_of_keys() {
        let prev = Some(serde_json::json!({ "cached_tokens": 10 }));
        let next = Some(serde_json::json!({ "audio_tokens": 3 }));

        let merged = merge_usage_details(prev, next).unwrap();
        assert_eq!(merged["cached_tokens"], 10);
        assert_eq!(merged["audio_tokens"], 3);
    }

    #[test]
    fn test_merge_usage_details_handles_none() {
        assert!(merge_usage_details(None, None).is_none());

        let v = serde_json::json!({ "cached_tokens": 10 });
        assert_eq!(
            merge_usage_details(None, Some(v.clone())).unwrap(),
            v.clone()
        );
        assert_eq!(merge_usage_details(Some(v.clone()), None).unwrap(), v);
    }
}
