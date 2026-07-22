//! # Onwards - A flexible LLM proxy library
//!
//! Onwards provides core functionality for building LLM proxy services that can route requests
//! to multiple AI model endpoints with authentication, rate limiting, and request transformation.
//!
//! ## Quick Start
//!
//! ```no_run
//! use onwards::{AppState, build_router, config::Config, target::Targets};
//! use axum::serve;
//! use tokio::net::TcpListener;
//! use clap::Parser;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Parse CLI configuration
//!     let config = Config::parse();
//!
//!     // Load targets from configuration file
//!     let targets = Targets::from_config_file(&config.targets).await?;
//!
//!     // Create application state with connection pool settings from config
//!     let app_state = AppState::new(targets);
//!
//!     // Build router with proxy routes
//!     let app = build_router(app_state);
//!
//!     // Start server
//!     let listener = TcpListener::bind("0.0.0.0:3000").await?;
//!     serve(listener, app).await?;
//!     Ok(())
//! }
//! ```
//!

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::HeaderMap;
use axum::routing::{any, get};
use axum_prometheus::{
    GenericMetricLayer, Handle, PrometheusMetricLayerBuilder,
    metrics_exporter_prometheus::PrometheusHandle,
};
use std::borrow::Cow;
use std::sync::Arc;
use tracing::{info, instrument};

pub mod auth;
pub mod client;
pub mod config;
pub mod errors;
pub mod handlers;
pub mod load_balancer;
pub mod models;
pub mod reasoning;
pub mod response_id;
#[cfg(feature = "multi-step")]
pub mod response_loop;
pub mod response_sanitizer;
pub mod sse;
pub mod stream_continuation;
#[cfg(feature = "multi-step")]
pub mod streaming;
pub mod strict;
pub mod target;
pub mod telemetry;
pub mod traits;

use client::{HttpClient, HyperClient};
pub use handlers::ServedBy;
use handlers::{models as models_handler, target_message_handler_with_continuation};
use models::ExtractedModel;
#[cfg(feature = "multi-step")]
pub use response_loop::{LoopConfig, LoopError, UpstreamTarget, run_response_loop};
#[cfg(feature = "multi-step")]
pub use streaming::{EventSink, EventSinkError, LoopEvent, LoopEventKind};
#[cfg(feature = "multi-step")]
pub use traits::{
    ChainStep, ExecutorError, MultiStepStore, NextAction, RecordedStep, StepDescriptor, StepKind,
    StepState,
};
pub use traits::{
    NoOpResponseStore, NoOpToolExecutor, RequestContext, ResponseStore, StoreError, ToolError,
    ToolExecutor, ToolKind, ToolSchema,
};

/// Type alias for body transformation function
///
/// Takes (path, headers, body_bytes) and returns transformed body_bytes or None if no transformation.
/// This allows you to modify request bodies before they are forwarded to upstream services.
///
/// # Arguments
///
/// * `&str` - The request path (e.g., "/v1/chat/completions")
/// * `&HeaderMap` - HTTP headers from the incoming request
/// * `&[u8]` - The request body as raw bytes
///
/// # Returns
///
/// * `Some(Bytes)` - Transformed request body to forward
/// * `None` - Use original request body unchanged
///
/// # Examples
///
/// ```
/// use onwards::BodyTransformFn;
/// use axum::http::HeaderMap;
/// use std::sync::Arc;
/// use serde_json::{json, Value};
///
/// // Transform function that adds stream_options to OpenAI streaming requests
/// let transform: BodyTransformFn = Arc::new(|path, _headers, body_bytes| {
///     if path == "/v1/chat/completions" {
///         if let Ok(mut json_body) = serde_json::from_slice::<Value>(body_bytes) {
///             if json_body.get("stream") == Some(&json!(true)) {
///                 json_body["stream_options"] = json!({"include_usage": true});
///                 if let Ok(transformed) = serde_json::to_vec(&json_body) {
///                     return Some(axum::body::Bytes::from(transformed));
///                 }
///             }
///         }
///     }
///     None // No transformation
/// });
/// ```
pub type BodyTransformFn =
    Arc<dyn Fn(&str, &HeaderMap, &[u8]) -> Option<axum::body::Bytes> + Send + Sync>;

/// Type alias for response transformation function
///
/// Takes (path, headers, body_bytes, original_model) and returns transformed body_bytes or None if no transformation.
/// This allows you to sanitize and modify response bodies before they are returned to clients.
///
/// # Arguments
///
/// * `&str` - The request path (e.g., "/v1/chat/completions")
/// * `&HeaderMap` - HTTP response headers from the upstream provider
/// * `&[u8]` - The response body as raw bytes
/// * `Option<&str>` - The original model requested by the client (for rewriting)
///
/// # Returns
///
/// * `Ok(Some(Bytes))` - Transformed response body to return
/// * `Ok(None)` - Use original response body unchanged
/// * `Err(String)` - Transformation failed with error message
///
/// # Examples
///
/// ```
/// use onwards::ResponseTransformFn;
/// use axum::http::HeaderMap;
/// use std::sync::Arc;
///
/// // Transform function that sanitizes OpenAI responses
/// let transform: ResponseTransformFn = Arc::new(|path, _headers, body_bytes, _model| {
///     if path.contains("/v1/chat/completions") {
///         // Sanitization logic here
///         Ok(Some(axum::body::Bytes::from(body_bytes.to_vec())))
///     } else {
///         Ok(None)
///     }
/// });
/// ```
pub type ResponseTransformFn = Arc<
    dyn Fn(&str, &HeaderMap, &[u8], Option<&str>) -> Result<Option<axum::body::Bytes>, String>
        + Send
        + Sync,
>;

/// The main application state containing the HTTP client and targets configuration
///
/// This struct holds all the state needed to run the proxy server. It contains:
/// - An HTTP client for making upstream requests
/// - The collection of configured targets (destinations)
/// - An optional body transformation function
/// - An optional response transformation function
///
/// # Examples
///
/// Basic setup:
/// ```no_run
/// use onwards::{AppState, config::Config, target::Targets};
/// use clap::Parser;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let config = Config::parse();
/// let targets = Targets::from_config_file(&config.targets).await?;
/// let app_state = AppState::new(targets);
/// # Ok(())
/// # }
/// ```
///
/// With request transformation:
/// ```no_run
/// use onwards::{AppState, BodyTransformFn, config::Config, target::Targets};
/// use std::sync::Arc;
/// use clap::Parser;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let config = Config::parse();
/// let targets = Targets::from_config_file(&config.targets).await?;
///
/// let transform: BodyTransformFn = Arc::new(|path, _headers, body| {
///     // Custom transformation logic
///     None
/// });
///
/// let app_state = AppState::with_transform(targets, transform);
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct AppState<T: HttpClient> {
    pub http_client: T,
    pub targets: target::Targets,
    pub body_transform_fn: Option<BodyTransformFn>,
    pub response_transform_fn: Option<ResponseTransformFn>,
    /// Header name that signals the request should be treated as streaming.
    /// When set, the responses adapter checks this header to decide whether to
    /// use the streaming path before forwarding (since the body transform runs
    /// too late for that decision). Defaults to `None` (header check disabled).
    pub streaming_header: Option<String>,
    /// Header name whose value overrides the generated `id` in Responses API
    /// responses. When set, the handler reads this header from the incoming
    /// request and uses its value (prefixed with `resp_` if not already) as the
    /// response object's `id` field. This lets callers (e.g. dwctl, fusillade)
    /// correlate responses with pre-created tracking records without needing to
    /// patch the response body after the fact.
    pub response_id_header: Option<String>,
    pub tool_executor: Arc<dyn ToolExecutor>,
    pub response_store: Arc<dyn ResponseStore>,
    /// Maximum request body size in bytes, enforced by both routers. Without
    /// this, the strict router's `Json` extractors fall back to Axum's 2 MB
    /// `DefaultBodyLimit`, which rejects large (e.g. long-context or base64
    /// image) payloads with a 413. Defaults to [`DEFAULT_BODY_LIMIT`].
    pub body_limit: usize,
}

/// Default maximum request body size (32 MB).
///
/// Deliberately larger than Axum's 2 MB default so that long-context and
/// vision payloads are not rejected out of the box. Override with
/// [`AppState::with_body_limit`].
pub const DEFAULT_BODY_LIMIT: usize = 32 * 1024 * 1024;

impl<T: HttpClient> std::fmt::Debug for AppState<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("http_client", &self.http_client)
            .field("targets", &self.targets)
            .field(
                "body_transform_fn",
                &self.body_transform_fn.as_ref().map(|_| "<function>"),
            )
            .field(
                "response_transform_fn",
                &self.response_transform_fn.as_ref().map(|_| "<function>"),
            )
            .field("streaming_header", &self.streaming_header)
            .field("response_id_header", &self.response_id_header)
            .field("tool_executor", &"<dyn ToolExecutor>")
            .field("response_store", &"<dyn ResponseStore>")
            .field("body_limit", &self.body_limit)
            .finish()
    }
}

impl AppState<HyperClient> {
    /// Create a new AppState with the default Hyper client
    pub fn new(targets: target::Targets) -> Self {
        let (max_idle, timeout) = targets
            .http_pool_config
            .as_ref()
            .map(|p| (p.max_idle_per_host, p.idle_timeout_secs))
            .unwrap_or((100, 90));

        let http_client = client::create_hyper_client(max_idle, timeout);
        Self {
            http_client,
            targets,
            body_transform_fn: None,
            response_transform_fn: None,
            streaming_header: None,
            response_id_header: None,
            tool_executor: Arc::new(NoOpToolExecutor),
            response_store: Arc::new(NoOpResponseStore),
            body_limit: DEFAULT_BODY_LIMIT,
        }
    }

    /// Create a new AppState with the default Hyper client and a body transformation function
    pub fn with_transform(targets: target::Targets, body_transform_fn: BodyTransformFn) -> Self {
        let (max_idle, timeout) = targets
            .http_pool_config
            .as_ref()
            .map(|p| (p.max_idle_per_host, p.idle_timeout_secs))
            .unwrap_or((100, 90));

        let http_client = client::create_hyper_client(max_idle, timeout);
        Self {
            http_client,
            targets,
            body_transform_fn: Some(body_transform_fn),
            response_transform_fn: None,
            streaming_header: None,
            response_id_header: None,
            tool_executor: Arc::new(NoOpToolExecutor),
            response_store: Arc::new(NoOpResponseStore),
            body_limit: DEFAULT_BODY_LIMIT,
        }
    }
}

impl<T: HttpClient> AppState<T> {
    /// Create a new AppState with a custom HTTP client (useful for testing)
    pub fn with_client(targets: target::Targets, http_client: T) -> Self {
        Self {
            http_client,
            targets,
            body_transform_fn: None,
            response_transform_fn: None,
            streaming_header: None,
            response_id_header: None,
            tool_executor: Arc::new(NoOpToolExecutor),
            response_store: Arc::new(NoOpResponseStore),
            body_limit: DEFAULT_BODY_LIMIT,
        }
    }

    /// Create a new AppState with a custom HTTP client and body transformation function
    pub fn with_client_and_transform(
        targets: target::Targets,
        http_client: T,
        body_transform_fn: BodyTransformFn,
    ) -> Self {
        Self {
            http_client,
            targets,
            body_transform_fn: Some(body_transform_fn),
            response_transform_fn: None,
            streaming_header: None,
            response_id_header: None,
            tool_executor: Arc::new(NoOpToolExecutor),
            response_store: Arc::new(NoOpResponseStore),
            body_limit: DEFAULT_BODY_LIMIT,
        }
    }

    /// Set the header name that signals a request should use the streaming path.
    /// Used by the responses adapter to decide streaming vs non-streaming before forwarding.
    pub fn with_streaming_header(mut self, header: impl Into<String>) -> Self {
        self.streaming_header = Some(header.into());
        self
    }

    /// Set the header name whose value overrides the Responses API `id` field.
    pub fn with_response_id_header(mut self, header: impl Into<String>) -> Self {
        self.response_id_header = Some(header.into());
        self
    }

    /// Set the response transformation function (builder pattern)
    pub fn with_response_transform(mut self, transform_fn: ResponseTransformFn) -> Self {
        self.response_transform_fn = Some(transform_fn);
        self
    }

    /// Set the tool executor for server-side tool handling (builder pattern)
    pub fn with_tool_executor(mut self, executor: Arc<dyn ToolExecutor>) -> Self {
        self.tool_executor = executor;
        self
    }

    /// Set the response store for stateful conversations (builder pattern)
    pub fn with_response_store(mut self, store: Arc<dyn ResponseStore>) -> Self {
        self.response_store = store;
        self
    }

    /// Set the maximum request body size in bytes (builder pattern).
    /// Applies to both the strict and non-strict routers.
    pub fn with_body_limit(mut self, limit: usize) -> Self {
        self.body_limit = limit;
        self
    }
}

/// Extract the model name from a request
///
/// This function checks for a model override header first, then extracts the model from the JSON body.
/// This is the same logic used by the proxy handler, extracted for reuse.
///
/// The extraction follows this precedence order:
/// 1. `model-override` header value
/// 2. `model` field in JSON request body
///
/// # Arguments
/// * `headers` - The request headers to check for model override
/// * `body_bytes` - The request body as bytes to parse for model field
///
/// # Returns
/// * `Ok(String)` - The extracted model name
/// * `Err(())` - If no model could be extracted or parsing failed
///
/// # Examples
///
/// Extract from header:
/// ```
/// use onwards::extract_model_from_request;
/// use axum::http::{HeaderMap, HeaderValue};
///
/// let mut headers = HeaderMap::new();
/// headers.insert("model-override", HeaderValue::from_static("gpt-4"));
/// let body = br#"{"model": "gpt-3.5", "messages": []}"#;
///
/// let model = extract_model_from_request(&headers, body).unwrap();
/// assert_eq!(model, "gpt-4"); // Header takes precedence
/// ```
///
/// Extract from JSON body:
/// ```
/// use onwards::extract_model_from_request;
/// use axum::http::HeaderMap;
///
/// let headers = HeaderMap::new();
/// let body = br#"{"model": "claude-3", "messages": []}"#;
///
/// let model = extract_model_from_request(&headers, body).unwrap();
/// assert_eq!(model, "claude-3");
/// ```
pub fn extract_model_from_request(headers: &HeaderMap, body_bytes: &[u8]) -> Option<String> {
    const MODEL_OVERRIDE_HEADER: &str = "model-override";

    // Order of precedence for the model:
    // 1. supplied as a header (model-override)
    // 2. Available in the request body as JSON
    match headers.get(MODEL_OVERRIDE_HEADER) {
        Some(header_value) => {
            let model_str = header_value.to_str().ok()?;
            Some(model_str.to_string())
        }
        None => {
            let extracted: ExtractedModel = serde_json::from_slice(body_bytes).ok()?;
            Some(extracted.model.to_string())
        }
    }
}

/// Creates the default OpenAI response sanitization function
///
/// This function creates a response transformer that:
/// - Enforces strict OpenAI API schema compliance
/// - Removes provider-specific fields
/// - Rewrites the model field to match the client's original request
///
/// The sanitizer only applies to `/v1/chat/completions` endpoint and supports
/// both streaming (SSE) and non-streaming responses.
///
/// # Returns
/// A [`ResponseTransformFn`] that can be used with [`AppState::with_response_transform`]
///
/// # Examples
///
/// ```no_run
/// use onwards::{AppState, config::Config, create_openai_sanitizer, target::Targets};
/// use clap::Parser;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let config = Config::parse();
/// let targets = Targets::from_config_file(&config.targets).await?;
/// let app_state = AppState::new(targets)
///     .with_response_transform(create_openai_sanitizer());
/// # Ok(())
/// # }
/// ```
pub fn create_openai_sanitizer() -> ResponseTransformFn {
    Arc::new(|path, headers, body, original_model| {
        let sanitizer = response_sanitizer::ResponseSanitizer {
            original_model: original_model.map(String::from),
        };
        sanitizer.sanitize(path, headers, body)
    })
}

/// Build the main router for the proxy
///
/// This creates the main Axum router with all proxy endpoints configured.
/// The router handles model listing and request forwarding to configured targets.
///
/// # Routes Created
/// - `/models` - Returns available models (OpenAI-compatible)
/// - `/v1/models` - Returns available models (OpenAI-compatible)
/// - `/{*path}` - Forwards all other requests to the appropriate target
///
/// # Arguments
/// * `state` - The application state containing targets and configuration
///
/// # Returns
/// A configured Axum [`Router`] ready to be served
///
/// # Examples
///
/// Basic usage:
/// ```no_run
/// use onwards::{AppState, build_router, target::Targets};
/// use axum::serve;
/// use tokio::net::TcpListener;
/// use clap::Parser;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let config = onwards::config::Config::parse();
/// let targets = Targets::from_config_file(&config.targets).await?;
/// let app_state = AppState::new(targets);
/// let router = build_router(app_state);
///
/// let listener = TcpListener::bind("0.0.0.0:3000").await?;
/// serve(listener, router).await?;
/// # Ok(())
/// # }
/// ```
#[instrument(skip(state))]
pub fn build_router<T: HttpClient + Clone + Send + Sync + 'static>(state: AppState<T>) -> Router {
    info!("Building router");
    Router::new()
        .route("/models", get(models_handler))
        .route("/v1/models", get(models_handler))
        .route("/{*path}", any(target_message_handler_with_continuation))
        // The wildcard handler buffers the body itself (bounded by
        // `state.body_limit`), but raise the extractor-level default too so
        // any extractor-based route added later shares the same limit.
        .layer(DefaultBodyLimit::max(state.body_limit))
        .with_state(state)
}

/// Builds a router for the metrics endpoint
///
/// Creates a separate router specifically for serving Prometheus metrics.
/// This is typically run on a different port from the main proxy for security.
///
/// # Arguments
/// * `handle` - Prometheus handle for rendering metrics
///
/// # Returns
/// A configured Axum [`Router`] serving metrics at `/metrics`
///
/// # Examples
///
/// ```no_run
/// use onwards::{build_metrics_router, build_metrics_layer_and_handle};
/// use axum::serve;
/// use tokio::net::TcpListener;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let (_metrics_layer, metrics_handle) = build_metrics_layer_and_handle("myapp");
/// let metrics_router = build_metrics_router(metrics_handle);
///
/// let listener = TcpListener::bind("0.0.0.0:9090").await?;
/// serve(listener, metrics_router).await?;
/// # Ok(())
/// # }
/// ```
#[instrument(skip(handle))]
pub fn build_metrics_router(handle: PrometheusHandle) -> Router {
    info!("Building metrics router");
    Router::new().route(
        "/metrics",
        axum::routing::get(move || async move { handle.render() }),
    )
}

type MetricsLayerAndHandle = (
    GenericMetricLayer<'static, PrometheusHandle, Handle>,
    PrometheusHandle,
);

/// Builds a layer and handle for prometheus metrics collection
///
/// Creates both the metrics collection layer (to add to your main router) and a handle
/// for serving the metrics on a separate endpoint. The layer automatically tracks
/// HTTP requests, response times, and other useful metrics.
///
/// # Arguments
/// * `prefix` - A string prefix for all metric names (e.g., "myapp" creates "myapp_http_requests_total")
///
/// # Returns
/// A tuple containing:
/// 1. Metrics layer to add to your main router
/// 2. Prometheus handle for serving metrics
///
/// # Examples
///
/// ```no_run
/// use onwards::{AppState, build_router, build_metrics_router, build_metrics_layer_and_handle, target::Targets};
/// use axum::serve;
/// use tokio::net::TcpListener;
/// use clap::Parser;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let config = onwards::config::Config::parse();
/// let targets = Targets::from_config_file(&config.targets).await?;
/// let app_state = AppState::new(targets);
///
/// // Build metrics layer and handle
/// let (metrics_layer, metrics_handle) = build_metrics_layer_and_handle("myapp");
///
/// // Main router with metrics collection
/// let app = build_router(app_state)
///     .layer(metrics_layer);
///
/// // Separate metrics router
/// let metrics_app = build_metrics_router(metrics_handle);
///
/// // Serve both (typically on different ports)
/// let main_listener = TcpListener::bind("0.0.0.0:3000").await?;
/// let metrics_listener = TcpListener::bind("0.0.0.0:9090").await?;
///
/// tokio::select! {
///     _ = serve(main_listener, app) => {},
///     _ = serve(metrics_listener, metrics_app) => {},
/// }
/// # Ok(())
/// # }
/// ```
pub fn build_metrics_layer_and_handle(
    prefix: impl Into<Cow<'static, str>>,
) -> MetricsLayerAndHandle {
    info!("Building metrics layer");
    // IMPORTANT: do not enable metric idle-timeout / eviction on this recorder.
    // `onwards_model_inflight{model}` is consumed by the autoscaler to decide when a
    // model has zero active streams and is safe to scale to zero. Series must persist
    // for the process lifetime: with idle eviction, a long-lived single stream's gauge
    // (value 1, untouched for the whole stream) could be evicted and render as ABSENT,
    // which the consumer reads as "0 inflight" and would tear the worker down mid-stream.
    // With eviction off (the default), an absent series unambiguously means "this model
    // has had no requests in this process lifetime" == genuinely 0 inflight.
    PrometheusMetricLayerBuilder::new()
        .with_prefix(prefix)
        .enable_response_body_size(true)
        .with_endpoint_label_type(axum_prometheus::EndpointLabel::Exact)
        .with_default_metrics()
        .build_pair()
}

pub mod test_utils {
    use super::*;
    use async_trait::async_trait;
    use axum::http::StatusCode;
    use std::sync::{Arc, Mutex};

    pub struct MockHttpClient {
        pub requests: Arc<Mutex<Vec<MockRequest>>>,
        response_builder: Arc<dyn Fn() -> axum::response::Response + Send + Sync>,
        custom_headers: Arc<Mutex<Vec<(String, String)>>>,
    }

    #[derive(Clone)]
    pub enum MockStreamEvent {
        Data(String),
        Bytes(Vec<u8>),
        Error(String),
        Pending,
    }

    #[derive(Clone)]
    pub struct MockStreamingResponse {
        pub status: StatusCode,
        pub content_type: Option<String>,
        pub headers: Vec<(String, String)>,
        pub events: Vec<MockStreamEvent>,
    }

    impl MockStreamingResponse {
        pub fn sse(status: StatusCode, events: Vec<MockStreamEvent>) -> Self {
            Self {
                status,
                content_type: Some("text/event-stream".to_string()),
                headers: Vec::new(),
                events,
            }
        }
    }

    #[derive(Debug, Clone)]
    pub struct MockRequest {
        pub method: String,
        pub uri: String,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
    }

    impl MockHttpClient {
        pub fn new(status: StatusCode, body: &str) -> Self {
            let body = body.to_string();
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                response_builder: Arc::new(move || {
                    axum::response::Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(body.clone()))
                        .unwrap()
                }),
                custom_headers: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub fn new_streaming(status: StatusCode, chunks: Vec<String>) -> Self {
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                response_builder: Arc::new(move || {
                    use axum::body::Body;
                    use futures_util::stream;

                    let stream = stream::iter(
                        chunks
                            .clone()
                            .into_iter()
                            .map(|chunk| Ok::<_, std::io::Error>(chunk.into_bytes())),
                    );

                    axum::response::Response::builder()
                        .status(status)
                        .header("content-type", "text/event-stream")
                        .header("cache-control", "no-cache")
                        .header("connection", "keep-alive")
                        .body(Body::from_stream(stream))
                        .unwrap()
                }),
                custom_headers: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// Create a mock that returns a different streaming response for each
        /// successive call. Useful for testing tool loops where the first call
        /// returns `tool_calls` and the second returns `stop`.
        pub fn new_streaming_sequence(status: StatusCode, responses: Vec<Vec<String>>) -> Self {
            let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                custom_headers: Arc::new(Mutex::new(Vec::new())),
                response_builder: Arc::new(move || {
                    use axum::body::Body;
                    use futures_util::stream;

                    let idx = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let chunks = responses.get(idx).cloned().unwrap_or_default();

                    let stream = stream::iter(
                        chunks
                            .into_iter()
                            .map(|chunk| Ok::<_, std::io::Error>(chunk.into_bytes())),
                    );

                    axum::response::Response::builder()
                        .status(status)
                        .header("content-type", "text/event-stream")
                        .header("cache-control", "no-cache")
                        .header("connection", "keep-alive")
                        .body(Body::from_stream(stream))
                        .unwrap()
                }),
            }
        }

        pub fn new_streaming_response_sequence(responses: Vec<MockStreamingResponse>) -> Self {
            let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                custom_headers: Arc::new(Mutex::new(Vec::new())),
                response_builder: Arc::new(move || {
                    use axum::body::Body;
                    use std::collections::VecDeque;
                    use std::task::Poll;

                    let idx = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let response = responses
                        .get(idx)
                        .cloned()
                        .unwrap_or(MockStreamingResponse {
                            status: StatusCode::OK,
                            content_type: Some("text/event-stream".to_string()),
                            headers: Vec::new(),
                            events: Vec::new(),
                        });
                    let mut events = VecDeque::from(response.events);
                    let stream = futures_util::stream::poll_fn(move |_cx| match events.front() {
                        Some(MockStreamEvent::Pending) => Poll::Pending,
                        Some(_) => match events.pop_front().expect("front checked above") {
                            MockStreamEvent::Data(data) => {
                                Poll::Ready(Some(Ok::<_, std::io::Error>(data.into_bytes())))
                            }
                            MockStreamEvent::Bytes(data) => {
                                Poll::Ready(Some(Ok::<_, std::io::Error>(data)))
                            }
                            MockStreamEvent::Error(message) => {
                                Poll::Ready(Some(Err(std::io::Error::other(message))))
                            }
                            MockStreamEvent::Pending => unreachable!("handled above"),
                        },
                        None => Poll::Ready(None),
                    });

                    let mut builder = axum::response::Response::builder()
                        .status(response.status)
                        .header("cache-control", "no-cache")
                        .header("connection", "keep-alive");
                    if let Some(content_type) = response.content_type {
                        builder = builder.header("content-type", content_type);
                    }
                    for (name, value) in response.headers {
                        builder = builder.header(name, value);
                    }
                    builder.body(Body::from_stream(stream)).unwrap()
                }),
            }
        }

        pub fn get_requests(&self) -> Vec<MockRequest> {
            self.requests.lock().unwrap().clone()
        }

        pub fn set_header(&mut self, name: &str, value: String) {
            self.custom_headers
                .lock()
                .unwrap()
                .push((name.to_string(), value));
        }
    }

    impl std::fmt::Debug for MockHttpClient {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MockHttpClient")
                .field("requests", &self.requests)
                .field("response_builder", &"<closure>")
                .finish()
        }
    }

    impl Clone for MockHttpClient {
        fn clone(&self) -> Self {
            Self {
                requests: Arc::clone(&self.requests),
                response_builder: Arc::clone(&self.response_builder),
                custom_headers: Arc::clone(&self.custom_headers),
            }
        }
    }

    #[async_trait]
    impl HttpClient for MockHttpClient {
        async fn request(
            &self,
            req: axum::extract::Request,
        ) -> Result<axum::response::Response, Box<dyn std::error::Error + Send + Sync>> {
            // Extract request details
            let method = req.method().to_string();
            let uri = req.uri().to_string();
            let headers = req
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();

            // Read body
            let body = axum::body::to_bytes(req.into_body(), usize::MAX)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
                .to_vec();

            // Store the request
            let mock_request = MockRequest {
                method,
                uri,
                headers,
                body,
            };
            self.requests.lock().unwrap().push(mock_request);

            // Build the response and apply custom headers
            let mut response = (self.response_builder)();
            let custom_headers = self.custom_headers.lock().unwrap();
            for (name, value) in custom_headers.iter() {
                response.headers_mut().insert(
                    axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    axum::http::HeaderValue::from_str(value).unwrap(),
                );
            }

            // Return the configured response
            Ok(response)
        }
    }

    /// A mock HTTP client that can be controlled with triggers
    /// Useful for testing concurrency limits with precise control over when requests complete
    pub struct TriggeredMockHttpClient {
        pub requests: Arc<Mutex<Vec<MockRequest>>>,
        response_builder: Arc<dyn Fn() -> axum::response::Response + Send + Sync>,
        triggers: Arc<Mutex<Vec<tokio::sync::oneshot::Sender<()>>>>,
    }

    impl TriggeredMockHttpClient {
        pub fn new(status: StatusCode, body: &str) -> Self {
            let body = body.to_string();
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                response_builder: Arc::new(move || {
                    axum::response::Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(body.clone()))
                        .unwrap()
                }),
                triggers: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub fn get_requests(&self) -> Vec<MockRequest> {
            self.requests.lock().unwrap().clone()
        }

        /// Complete the nth request (0-indexed)
        /// Returns true if the trigger was sent, false if no such request exists
        pub fn complete_request(&self, index: usize) -> bool {
            let mut triggers = self.triggers.lock().unwrap();
            if index < triggers.len() {
                // Remove and send the trigger
                let trigger = triggers.remove(index);
                let _ = trigger.send(());
                true
            } else {
                false
            }
        }

        /// Complete all pending requests
        pub fn complete_all(&self) {
            let mut triggers = self.triggers.lock().unwrap();
            while let Some(trigger) = triggers.pop() {
                let _ = trigger.send(());
            }
        }
    }

    impl std::fmt::Debug for TriggeredMockHttpClient {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("TriggeredMockHttpClient")
                .field("requests", &self.requests)
                .field("pending_triggers", &self.triggers.lock().unwrap().len())
                .field("response_builder", &"<closure>")
                .finish()
        }
    }

    impl Clone for TriggeredMockHttpClient {
        fn clone(&self) -> Self {
            Self {
                requests: Arc::clone(&self.requests),
                response_builder: Arc::clone(&self.response_builder),
                triggers: Arc::clone(&self.triggers),
            }
        }
    }

    #[async_trait]
    impl HttpClient for TriggeredMockHttpClient {
        async fn request(
            &self,
            req: axum::extract::Request,
        ) -> Result<axum::response::Response, Box<dyn std::error::Error + Send + Sync>> {
            // Extract request details
            let method = req.method().to_string();
            let uri = req.uri().to_string();
            let headers = req
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();

            // Read body
            let body = axum::body::to_bytes(req.into_body(), usize::MAX)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
                .to_vec();

            // Store the request
            let mock_request = MockRequest {
                method,
                uri,
                headers,
                body,
            };
            self.requests.lock().unwrap().push(mock_request);

            // Create a trigger for this request
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.triggers.lock().unwrap().push(tx);

            // Wait for the trigger to be fired
            let _ = rx.await;

            // Return the configured response
            Ok((self.response_builder)())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load_balancer::{Provider, ProviderPool};
    use crate::target::{
        ConcurrencyLimiter, FallbackConfig, LoadBalanceStrategy, OpenResponsesConfig,
        StreamContinuationConfig, Target, Targets,
    };
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum_test::TestServer;
    use dashmap::DashMap;
    use futures_util::StreamExt;
    use http_body_util::BodyExt;
    use serde_json::json;
    use std::sync::Arc;
    use test_utils::{MockHttpClient, MockStreamEvent, MockStreamingResponse};
    use tower::ServiceExt;

    /// Helper to create a single-provider pool from a target
    fn pool(target: Target) -> ProviderPool {
        target.into_pool()
    }

    #[tokio::test]
    async fn test_empty_targets_returns_404() {
        // Create empty targets
        let targets = target::Targets {
            targets: Arc::new(DashMap::new()),
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, "{}");
        let app_state = AppState::with_client(targets, mock_client);
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "gpt-4",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 404);
    }

    #[tokio::test]
    async fn test_non_strict_router_rejects_body_over_configured_limit() {
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "gpt-4".to_string(),
            pool(
                target::Target::builder()
                    .url("https://api.openai.com".parse().unwrap())
                    .build(),
            ),
        );

        let targets = target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, "{}");
        let app_state = AppState::with_client(targets, mock_client).with_body_limit(1024);
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "gpt-4",
                "messages": [{"role": "user", "content": "x".repeat(2048)}]
            }))
            .await;

        assert_eq!(response.status_code(), StatusCode::PAYLOAD_TOO_LARGE);
        let body: serde_json::Value = response.json();
        assert_eq!(body["error"]["code"], "payload_too_large");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("1024 bytes")
        );
    }

    #[tokio::test]
    async fn test_multiple_targets_routing() {
        // Create targets with multiple models
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "gpt-4".to_string(),
            pool(
                target::Target::builder()
                    .url("https://api.openai.com".parse().unwrap())
                    .onwards_key("sk-test-key".to_string())
                    .build(),
            ),
        );
        targets_map.insert(
            "claude-3".to_string(),
            pool(
                target::Target::builder()
                    .url("https://api.anthropic.com".parse().unwrap())
                    .onwards_key("sk-ant-test-key".to_string())
                    .build(),
            ),
        );

        let targets = target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        let mock_client = MockHttpClient::new(
            StatusCode::OK,
            r#"{"choices": [{"message": {"content": "Hello!"}}]}"#,
        );
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Test that gpt-4 model is recognized and returns 200
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "gpt-4",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 200);

        // Test that claude-3 model is recognized and returns 200
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "claude-3",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 200);

        // Test that non-existent model returns 404
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "non-existent-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 404);

        // Verify that 2 requests were made to the mock client (for the valid models)
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 2);

        // Check that requests were made to the correct URLs
        assert!(requests[0].uri.contains("api.openai.com"));
        assert!(requests[1].uri.contains("api.anthropic.com"));
    }

    /// The success path must attach a `ServedBy` response extension naming the
    /// upstream that produced the response, so integrators (request logging,
    /// billing attribution) can read it in-process. Driven through the router
    /// as a tower service — extensions don't survive a real HTTP hop, which is
    /// exactly why the extension (not a header) carries this.
    #[tokio::test]
    async fn test_served_by_extension_set_on_success() {
        use tower::ServiceExt;

        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "gpt-4".to_string(),
            pool(
                target::Target::builder()
                    .url("https://api.openai.com".parse().unwrap())
                    .onwards_model("gpt-4-upstream".to_string())
                    .build(),
            ),
        );
        let targets = target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };
        let mock_client = MockHttpClient::new(
            StatusCode::OK,
            r#"{"choices": [{"message": {"content": "Hello!"}}]}"#,
        );
        let app_state = AppState::with_client(targets, mock_client);
        let router = build_router(app_state);

        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({
                    "model": "gpt-4",
                    "messages": [{"role": "user", "content": "Hello"}]
                })
                .to_string(),
            ))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let served_by = response
            .extensions()
            .get::<crate::ServedBy>()
            .expect("success response must carry ServedBy");
        assert_eq!(served_by.url, "https://api.openai.com/");
        assert_eq!(served_by.onwards_model.as_deref(), Some("gpt-4-upstream"));
    }

    /// Strict-mode `Targets` with one alias backed by a fallback pool of `n`
    /// identical providers (all hit the shared mock client), configured to retry
    /// on the given upstream `on_status` codes.
    fn fallback_targets(alias: &str, n: usize, on_status: Vec<u16>) -> target::Targets {
        use crate::load_balancer::{Provider, ProviderPool};
        use crate::target::{FallbackConfig, LoadBalanceStrategy, Target};

        let providers = (0..n)
            .map(|i| {
                let t = Target::builder()
                    .url(format!("https://p{i}.example.com/").parse().unwrap())
                    .request_timeout_secs(5)
                    .build();
                Provider::new(t, 1)
            })
            .collect();
        let fallback = Some(FallbackConfig {
            enabled: true,
            on_status,
            ..Default::default()
        });
        let pool = ProviderPool::with_config(
            providers,
            None,
            None,
            None,
            fallback,
            LoadBalanceStrategy::Priority,
            false,
            Vec::new(),
        );
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(alias.to_string(), pool);
        target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: true,
            http_pool_config: None,
        }
    }

    fn stream_continuation_targets(
        alias: &str,
        provider_models: &[Option<&str>],
        fallback: FallbackConfig,
    ) -> target::Targets {
        stream_continuation_targets_with_options(alias, provider_models, fallback, false, false)
    }

    fn stream_continuation_targets_with_options(
        alias: &str,
        provider_models: &[Option<&str>],
        fallback: FallbackConfig,
        strict_mode: bool,
        sanitize_response: bool,
    ) -> target::Targets {
        let providers = provider_models
            .iter()
            .enumerate()
            .map(|(i, model)| {
                let builder = Target::builder()
                    .url(format!("https://stream-{i}.example.com/").parse().unwrap())
                    .sanitize_response(sanitize_response);
                let target = match model {
                    Some(model) => builder.onwards_model((*model).to_string()).build(),
                    None => builder.build(),
                };
                Provider::new(target, 1)
            })
            .collect();
        let pool = ProviderPool::with_config(
            providers,
            None,
            None,
            None,
            Some(fallback),
            LoadBalanceStrategy::Priority,
            false,
            Vec::new(),
        );
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(alias.to_string(), pool);
        target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode,
            http_pool_config: None,
        }
    }

    fn stream_continuation_targets_from_targets(
        alias: &str,
        targets: Vec<Target>,
        fallback: FallbackConfig,
        strict_mode: bool,
        pool_trusted: bool,
    ) -> target::Targets {
        let providers = targets
            .into_iter()
            .map(|target| Provider::new(target, 1))
            .collect();
        let pool = ProviderPool::with_config(
            providers,
            None,
            None,
            None,
            Some(fallback),
            LoadBalanceStrategy::Priority,
            pool_trusted,
            Vec::new(),
        );
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(alias.to_string(), pool);
        target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode,
            http_pool_config: None,
        }
    }

    fn strict_stream_continuation_router(
        targets: target::Targets,
        mock: MockHttpClient,
    ) -> axum::Router {
        axum::Router::new().nest(
            "/v1",
            crate::strict::build_strict_router(AppState::with_client(targets, mock)),
        )
    }

    fn stream_continuation_fallback(
        enabled: bool,
        pre_response_attempts: usize,
        continuation_attempts: usize,
        idle_timeout_ms: Option<u64>,
        endpoints: Vec<&str>,
    ) -> FallbackConfig {
        FallbackConfig {
            enabled,
            on_status: vec![502],
            max_attempts: Some(pre_response_attempts),
            stream_continuation: Some(StreamContinuationConfig {
                enabled: true,
                endpoints: endpoints.into_iter().map(str::to_string).collect(),
                max_attempts: continuation_attempts,
                max_buffered_bytes: 1024,
                idle_timeout_ms,
            }),
            ..Default::default()
        }
    }

    fn completion_event(id: &str, model: &str, text: &str, finish_reason: &str) -> String {
        format!(
            "data: {{\"id\":\"{id}\",\"object\":\"text_completion\",\"created\":1,\"model\":\"{model}\",\"choices\":[{{\"index\":0,\"text\":{text:?},\"finish_reason\":{finish_reason}}}]}}\n\n"
        )
    }

    fn completion_sse_values(body: &str) -> Vec<serde_json::Value> {
        body.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(data).unwrap())
            .collect()
    }

    fn gzip_bytes(data: &[u8]) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn brotli_bytes(data: &[u8]) -> Vec<u8> {
        use std::io::Write;

        let mut compressed = Vec::new();
        {
            let mut writer = brotli::CompressorWriter::new(&mut compressed, 4096, 4, 22);
            writer.write_all(data).unwrap();
        }
        compressed
    }

    #[tokio::test]
    async fn stream_continuation_interrupted_completion_continues_from_exact_emitted_prefix() {
        let first = completion_event("cmpl-first", "first", "Hello", "null");
        let second = completion_event("cmpl-second", "second", " world", "null");
        let stop = completion_event("cmpl-second", "second", "", "\"stop\"");
        let usage = "data: {\"id\":\"cmpl-second\",\"object\":\"text_completion\",\"created\":2,\"model\":\"second\",\"choices\":[],\"usage\":{\"completion_tokens\":1}}\n\n".to_string();
        let mock = MockHttpClient::new_streaming_sequence(
            StatusCode::OK,
            vec![
                vec![first],
                vec![second, stop, usage, "data: [DONE]\n\n".to_string()],
            ],
        );
        let targets = stream_continuation_targets(
            "requested-model",
            &[None, None],
            stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
        );
        let server =
            TestServer::new(build_router(AppState::with_client(targets, mock.clone()))).unwrap();

        let response = server
            .post("/v1/completions")
            .json(&json!({
                "model": "requested-model",
                "prompt": "Say hello: ",
                "stream": true
            }))
            .await;

        assert_eq!(response.status_code(), 200);
        let body = response.text();
        assert!(body.contains("Hello"));
        assert!(body.contains(" world"));
        let requests = mock.get_requests();
        assert_eq!(requests.len(), 2);
        let second_body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(second_body["prompt"], "Say hello: Hello");
        assert!(body.contains("\"id\":\"cmpl-first\""));
        assert!(!body.contains("cmpl-second"));
    }

    #[tokio::test]
    async fn stream_continuation_terminal_finish_forwards_usage_and_does_not_retry() {
        let text = completion_event("cmpl-first", "first", "Hello", "null");
        let stop = completion_event("cmpl-first", "first", "", "\"stop\"");
        let usage = "data: {\"id\":\"cmpl-first\",\"object\":\"text_completion\",\"created\":1,\"model\":\"first\",\"choices\":[],\"usage\":{\"completion_tokens\":1}}\n\n".to_string();
        let mock = MockHttpClient::new_streaming_sequence(
            StatusCode::OK,
            vec![vec![text, stop, usage, "data: [DONE]\n\n".to_string()]],
        );
        let targets = stream_continuation_targets(
            "requested-model",
            &[None, None],
            stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
        );
        let server =
            TestServer::new(build_router(AppState::with_client(targets, mock.clone()))).unwrap();

        let response = server
            .post("/v1/completions")
            .json(&json!({
                "model": "requested-model",
                "prompt": "Say hello: ",
                "stream": true
            }))
            .await;

        let body = response.text();
        assert!(body.contains("completion_tokens"));
        assert!(body.contains("[DONE]"));
        assert_eq!(mock.get_requests().len(), 1);
    }

    #[tokio::test]
    async fn stream_continuation_crlf_terminal_stream_is_not_spuriously_continued() {
        let event =
            completion_event("cmpl-first", "first", "Hello", "\"stop\"").replace('\n', "\r\n");
        let mock = MockHttpClient::new_streaming_sequence(
            StatusCode::OK,
            vec![vec![event, "data:[DONE]\r\n\r\n".to_string()]],
        );
        let targets = stream_continuation_targets(
            "requested-model",
            &[None, None],
            stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
        );
        let server =
            TestServer::new(build_router(AppState::with_client(targets, mock.clone()))).unwrap();

        let response = server
            .post("/v1/completions")
            .json(&json!({"model":"requested-model","prompt":"P: ","stream":true}))
            .await;
        let body = response.text();

        assert!(body.contains("Hello"));
        assert!(body.contains("data:[DONE]"));
        assert_eq!(mock.get_requests().len(), 1);
    }

    #[tokio::test]
    async fn stream_continuation_strict_chat_stream_resumes_as_one_assistant_message() {
        let first_role = "data: {\"id\":\"chat-first\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"first\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n".to_string();
        let first_text = "data: {\"id\":\"chat-first\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"first\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n".to_string();
        let second_role = "data: {\"id\":\"chat-second\",\"object\":\"chat.completion.chunk\",\"created\":2,\"model\":\"second\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n".to_string();
        let second_text = "data: {\"id\":\"chat-second\",\"object\":\"chat.completion.chunk\",\"created\":2,\"model\":\"second\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n".to_string();
        let stop = "data: {\"id\":\"chat-second\",\"object\":\"chat.completion.chunk\",\"created\":2,\"model\":\"second\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_string();
        let mock = MockHttpClient::new_streaming_sequence(
            StatusCode::OK,
            vec![
                vec![first_role, first_text],
                vec![
                    second_role,
                    second_text,
                    stop,
                    "data: [DONE]\n\n".to_string(),
                ],
            ],
        );
        let targets = stream_continuation_targets_with_options(
            "requested-model",
            &[None, None],
            stream_continuation_fallback(
                true,
                2,
                1,
                None,
                vec!["/v1/completions", "/v1/chat/completions"],
            ),
            true,
            false,
        );
        let server =
            TestServer::new(strict_stream_continuation_router(targets, mock.clone())).unwrap();

        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "requested-model",
                "messages": [{"role": "user", "content": "Hello"}],
                "stream": true
            }))
            .await;

        assert_eq!(response.status_code(), 200);
        let body = response.text();
        assert!(body.contains("Hello"));
        assert!(body.contains(" world"));
        assert!(body.contains("chat-first"));
        assert!(!body.contains("chat-second"));

        let requests = mock.get_requests();
        assert_eq!(requests.len(), 2);
        let continuation: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        let messages = continuation["messages"].as_array().unwrap();
        assert_eq!(messages.last().unwrap()["role"], "assistant");
        assert_eq!(messages.last().unwrap()["content"], "Hello");
    }

    #[tokio::test]
    async fn stream_continuation_strict_responses_stream_stitches_text_events() {
        let first_created = "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_first\",\"model\":\"first\",\"output\":[]}}\n\n".to_string();
        let first_item = "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_first\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n".to_string();
        let first_part = "event: response.content_part.added\ndata: {\"type\":\"response.content_part.added\",\"sequence_number\":2,\"item_id\":\"msg_first\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"\"}}\n\n".to_string();
        let first_delta = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"item_id\":\"msg_first\",\"output_index\":0,\"content_index\":0,\"delta\":\"Hello\"}\n\n".to_string();
        let second_created = first_created.replace("resp_first", "resp_second");
        let second_item = first_item.replace("msg_first", "msg_second");
        let second_part = first_part.replace("msg_first", "msg_second");
        let second_delta = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"item_id\":\"msg_second\",\"output_index\":0,\"content_index\":0,\"delta\":\" world\"}\n\n".to_string();
        let completed = "event: response.completed\ndata: {\"type\":\"response.completed\",\"sequence_number\":4,\"response\":{\"id\":\"resp_second\",\"model\":\"second\",\"output\":[{\"id\":\"msg_second\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\" world\"}]}]}}\n\n".to_string();
        let mock = MockHttpClient::new_streaming_sequence(
            StatusCode::OK,
            vec![
                vec![first_created, first_item, first_part, first_delta],
                vec![
                    second_created,
                    second_item,
                    second_part,
                    second_delta,
                    completed,
                ],
            ],
        );
        let targets = stream_continuation_targets_with_options(
            "requested-model",
            &[None, None],
            stream_continuation_fallback(true, 2, 1, None, vec!["/v1/responses"]),
            true,
            false,
        );
        let server =
            TestServer::new(strict_stream_continuation_router(targets, mock.clone())).unwrap();

        let response = server
            .post("/v1/responses")
            .json(&json!({
                "model": "requested-model",
                "input": "Hello",
                "stream": true
            }))
            .await;

        assert_eq!(response.status_code(), 200);
        let body = response.text();
        assert!(body.contains("Hello"));
        assert!(body.contains(" world"));
        assert!(body.contains("resp_first"));
        assert!(!body.contains("resp_second"));

        let requests = mock.get_requests();
        assert_eq!(requests.len(), 2);
        let continuation: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        let input = continuation["input"].as_array().unwrap();
        assert_eq!(input.last().unwrap()["role"], "assistant");
        assert_eq!(input.last().unwrap()["content"][0]["text"], "Hello");
    }

    #[tokio::test]
    async fn stream_continuation_adapter_responses_uses_external_endpoint_and_chat_protocol() {
        let first_role = "data: {\"id\":\"chat-first\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"first\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n".to_string();
        let first_text = "data: {\"id\":\"chat-first\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"first\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n".to_string();
        let second_role = "data: {\"id\":\"chat-second\",\"object\":\"chat.completion.chunk\",\"created\":2,\"model\":\"second\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n".to_string();
        let second_text = "data: {\"id\":\"chat-second\",\"object\":\"chat.completion.chunk\",\"created\":2,\"model\":\"second\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n".to_string();
        let stop = "data: {\"id\":\"chat-second\",\"object\":\"chat.completion.chunk\",\"created\":2,\"model\":\"second\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_string();
        let mock = MockHttpClient::new_streaming_sequence(
            StatusCode::OK,
            vec![
                vec![first_role, first_text],
                vec![
                    second_role,
                    second_text,
                    stop,
                    "data: [DONE]\n\n".to_string(),
                ],
            ],
        );
        let adapter_targets = (0..2)
            .map(|index| {
                Target::builder()
                    .url(
                        format!("https://adapter-{index}.example.com/")
                            .parse()
                            .unwrap(),
                    )
                    .open_responses(OpenResponsesConfig { adapter: true })
                    .build()
            })
            .collect();
        let targets = stream_continuation_targets_from_targets(
            "requested-model",
            adapter_targets,
            stream_continuation_fallback(true, 2, 1, None, vec!["/v1/responses"]),
            true,
            false,
        );
        let server =
            TestServer::new(strict_stream_continuation_router(targets, mock.clone())).unwrap();

        let response = server
            .post("/v1/responses")
            .json(&json!({
                "model": "requested-model",
                "input": "Hello",
                "stream": true
            }))
            .await;

        assert_eq!(response.status_code(), 200);
        let body = response.text();
        assert!(body.contains("Hello"));
        assert!(body.contains(" world"));
        let requests = mock.get_requests();
        assert_eq!(requests.len(), 2);
        let continuation: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        let messages = continuation["messages"].as_array().unwrap();
        assert_eq!(messages.last().unwrap()["role"], "assistant");
        assert_eq!(messages.last().unwrap()["content"], "Hello");
    }

    #[tokio::test]
    async fn stream_continuation_edge_translated_responses_preserves_external_endpoint() {
        let first = "data: {\"id\":\"chat-first\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"first\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n".to_string();
        let second = "data: {\"id\":\"chat-second\",\"object\":\"chat.completion.chunk\",\"created\":2,\"model\":\"second\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":\"stop\"}]}\n\n".to_string();
        let mock = MockHttpClient::new_streaming_sequence(
            StatusCode::OK,
            vec![vec![first], vec![second, "data: [DONE]\n\n".to_string()]],
        );
        let targets = stream_continuation_targets_with_options(
            "requested-model",
            &[None, None],
            stream_continuation_fallback(true, 2, 1, None, vec!["/v1/responses"]),
            true,
            false,
        );
        let state = AppState::with_client(targets, mock.clone());
        let request = axum::extract::Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "model": "requested-model",
                    "messages": [{"role": "user", "content": "Hello"}],
                    "stream": true
                })
                .to_string(),
            ))
            .unwrap();

        let response = crate::strict::handlers::responses_handler(
            axum::extract::State(state),
            HeaderMap::new(),
            request,
        )
        .await;
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert!(body.contains("Hello"));
        assert!(body.contains(" world"));
        let requests = mock.get_requests();
        assert_eq!(requests.len(), 2);
        let continuation: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        let messages = continuation["messages"].as_array().unwrap();
        assert_eq!(messages.last().unwrap()["role"], "assistant");
        assert_eq!(messages.last().unwrap()["content"], "Hello");
    }

    #[tokio::test]
    async fn stream_continuation_accepts_large_responses_terminal_snapshot_beyond_prefix_cap() {
        let chunk = "\"".repeat(1024);
        let full_text = chunk.repeat(70);
        let mut events = vec![
            "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_first\",\"model\":\"first\",\"output\":[]}}\n\n".to_string(),
            "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_first\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n".to_string(),
            "event: response.content_part.added\ndata: {\"type\":\"response.content_part.added\",\"sequence_number\":2,\"item_id\":\"msg_first\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"\"}}\n\n".to_string(),
        ];
        for sequence in 3..73 {
            events.push(format!(
                "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"sequence_number\":{sequence},\"item_id\":\"msg_first\",\"output_index\":0,\"content_index\":0,\"delta\":{chunk:?}}}\n\n"
            ));
        }
        events.push(format!(
            "event: response.completed\ndata: {{\"type\":\"response.completed\",\"sequence_number\":73,\"response\":{{\"id\":\"resp_first\",\"model\":\"first\",\"output\":[{{\"id\":\"msg_first\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":{full_text:?}}}]}}]}}}}\n\n"
        ));
        let mock = MockHttpClient::new_streaming_sequence(StatusCode::OK, vec![events]);
        let mut fallback = stream_continuation_fallback(true, 1, 0, None, vec!["/v1/responses"]);
        fallback
            .stream_continuation
            .as_mut()
            .unwrap()
            .max_buffered_bytes = 1024;
        let targets = stream_continuation_targets_with_options(
            "requested-model",
            &[None],
            fallback,
            true,
            false,
        );
        let request = axum::extract::Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"model":"requested-model","input":"Hello","stream":true}).to_string(),
            ))
            .unwrap();

        let response = strict_stream_continuation_router(targets, mock)
            .oneshot(request)
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();
        let completed: serde_json::Value = body
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|data| serde_json::from_str(data.trim()).ok())
            .find(|event: &serde_json::Value| event["type"] == "response.completed")
            .unwrap();
        assert_eq!(
            completed["response"]["output"][0]["content"][0]["text"],
            full_text
        );
    }

    #[tokio::test]
    async fn stream_continuation_fallback_master_switch_disables_continuation() {
        let first = completion_event("cmpl-first", "first", "Hello", "null");
        let mock = MockHttpClient::new_streaming_sequence(StatusCode::OK, vec![vec![first]]);
        let targets = stream_continuation_targets(
            "requested-model",
            &[None, None],
            stream_continuation_fallback(false, 2, 1, None, vec!["/v1/completions"]),
        );
        let server =
            TestServer::new(build_router(AppState::with_client(targets, mock.clone()))).unwrap();

        server
            .post("/v1/completions")
            .json(&json!({
                "model": "requested-model",
                "prompt": "Say hello: ",
                "stream": true
            }))
            .await;

        assert_eq!(mock.get_requests().len(), 1);
    }

    #[tokio::test]
    async fn stream_continuation_body_error_resumes_and_finishes() {
        let first = completion_event("cmpl-first", "first", "Hello", "null");
        let second = completion_event("cmpl-second", "second", " world", "\"stop\"");
        let mock = MockHttpClient::new_streaming_response_sequence(vec![
            MockStreamingResponse::sse(
                StatusCode::OK,
                vec![
                    MockStreamEvent::Data(first),
                    MockStreamEvent::Error("upstream reset".to_string()),
                ],
            ),
            MockStreamingResponse::sse(
                StatusCode::OK,
                vec![
                    MockStreamEvent::Data(second),
                    MockStreamEvent::Data("data: [DONE]\n\n".to_string()),
                ],
            ),
        ]);
        let targets = stream_continuation_targets(
            "requested-model",
            &[None, None],
            stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
        );
        let server =
            TestServer::new(build_router(AppState::with_client(targets, mock.clone()))).unwrap();

        let response = server
            .post("/v1/completions")
            .json(&json!({
                "model": "requested-model",
                "prompt": "Say hello: ",
                "stream": true
            }))
            .await;

        let body = response.text();
        assert!(body.contains("Hello"));
        assert!(body.contains(" world"));
        assert_eq!(mock.get_requests().len(), 2);
    }

    #[tokio::test]
    async fn stream_continuation_exhausted_plain_eof_has_no_synthetic_done() {
        let first = completion_event("cmpl-first", "first", "Hello", "null");
        let second = completion_event("cmpl-second", "second", " world", "null");
        let mock =
            MockHttpClient::new_streaming_sequence(StatusCode::OK, vec![vec![first], vec![second]]);
        let targets = stream_continuation_targets(
            "requested-model",
            &[None, None],
            stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
        );
        let server =
            TestServer::new(build_router(AppState::with_client(targets, mock.clone()))).unwrap();

        let response = server
            .post("/v1/completions")
            .json(&json!({
                "model": "requested-model",
                "prompt": "Say hello: ",
                "stream": true
            }))
            .await;

        let body = response.text();
        assert!(body.contains("Hello"));
        assert!(body.contains(" world"));
        assert!(!body.contains("[DONE]"));
        assert_eq!(mock.get_requests().len(), 2);
    }

    #[tokio::test]
    async fn stream_continuation_exhausted_body_error_remains_downstream_error() {
        let first = completion_event("cmpl-first", "first", "Hello", "null");
        let second = completion_event("cmpl-second", "second", " world", "null");
        let mock = MockHttpClient::new_streaming_response_sequence(vec![
            MockStreamingResponse::sse(
                StatusCode::OK,
                vec![
                    MockStreamEvent::Data(first),
                    MockStreamEvent::Error("first reset".to_string()),
                ],
            ),
            MockStreamingResponse::sse(
                StatusCode::OK,
                vec![
                    MockStreamEvent::Data(second),
                    MockStreamEvent::Error("final reset".to_string()),
                ],
            ),
        ]);
        let targets = stream_continuation_targets(
            "requested-model",
            &[None, None],
            stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
        );
        let request = axum::extract::Request::builder()
            .method("POST")
            .uri("/v1/completions")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({
                    "model": "requested-model",
                    "prompt": "Say hello: ",
                    "stream": true
                })
                .to_string(),
            ))
            .unwrap();

        let response = build_router(AppState::with_client(targets, mock.clone()))
            .oneshot(request)
            .await
            .unwrap();
        let result = response.into_body().collect().await;

        assert!(result.is_err());
        assert_eq!(mock.get_requests().len(), 2);
    }

    #[tokio::test]
    async fn stream_continuation_idle_timeout_resumes_pending_body() {
        let first = completion_event("cmpl-first", "first", "Hello", "null");
        let second = completion_event("cmpl-second", "second", " world", "\"stop\"");
        let mock = MockHttpClient::new_streaming_response_sequence(vec![
            MockStreamingResponse::sse(
                StatusCode::OK,
                vec![MockStreamEvent::Data(first), MockStreamEvent::Pending],
            ),
            MockStreamingResponse {
                status: StatusCode::OK,
                content_type: Some("text/event-stream".to_string()),
                headers: vec![("x-upstream".to_string(), "continuation".to_string())],
                events: vec![
                    MockStreamEvent::Data(second),
                    MockStreamEvent::Data("data: [DONE]\n\n".to_string()),
                ],
            },
        ]);
        let targets = stream_continuation_targets(
            "requested-model",
            &[None, None],
            stream_continuation_fallback(true, 2, 1, Some(10), vec!["/v1/completions"]),
        );
        let server =
            TestServer::new(build_router(AppState::with_client(targets, mock.clone()))).unwrap();

        let response = server
            .post("/v1/completions")
            .json(&json!({
                "model": "requested-model",
                "prompt": "Say hello: ",
                "stream": true
            }))
            .await;

        assert!(response.text().contains(" world"));
        assert_eq!(mock.get_requests().len(), 2);
    }

    #[tokio::test]
    async fn stream_continuation_rewrites_model_for_selected_provider() {
        let first = completion_event("cmpl-first", "provider-a", "Hello", "null");
        let second = completion_event("cmpl-second", "provider-b", " world", "\"stop\"");
        let mock = MockHttpClient::new_streaming_sequence(
            StatusCode::OK,
            vec![vec![first], vec![second, "data: [DONE]\n\n".to_string()]],
        );
        let targets = stream_continuation_targets(
            "requested-model",
            &[Some("provider-a"), Some("provider-b")],
            stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
        );
        let server =
            TestServer::new(build_router(AppState::with_client(targets, mock.clone()))).unwrap();

        server
            .post("/v1/completions")
            .json(&json!({
                "model": "requested-model",
                "prompt": "Say hello: ",
                "stream": true
            }))
            .await;

        let requests = mock.get_requests();
        assert_eq!(requests.len(), 2);
        let first_body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let second_body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(first_body["model"], "provider-a");
        assert_eq!(second_body["model"], "provider-a");
        assert!(requests[1].uri.contains("stream-0.example.com"));
    }

    #[tokio::test]
    async fn stream_continuation_releases_provider_guard_before_reselection() {
        let first = completion_event("cmpl-first", "provider-a", "Hello", "null");
        let second = completion_event("cmpl-second", "provider-a", " world", "\"stop\"");
        let mock = MockHttpClient::new_streaming_sequence(
            StatusCode::OK,
            vec![vec![first], vec![second, "data: [DONE]\n\n".to_string()]],
        );
        let target = Target::builder()
            .url("https://stream-0.example.com/".parse().unwrap())
            .onwards_model("provider-a".to_string())
            .build();
        let pool = ProviderPool::with_config(
            vec![Provider::with_concurrency_limit(target, 1, 1)],
            None,
            None,
            None,
            Some(stream_continuation_fallback(
                true,
                1,
                1,
                None,
                vec!["/v1/completions"],
            )),
            LoadBalanceStrategy::Priority,
            false,
            Vec::new(),
        );
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert("requested-model".to_string(), pool);
        let targets = target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };
        let server =
            TestServer::new(build_router(AppState::with_client(targets, mock.clone()))).unwrap();

        let response = server
            .post("/v1/completions")
            .json(&json!({
                "model": "requested-model",
                "prompt": "Say hello: ",
                "stream": true
            }))
            .await;

        assert!(response.text().contains(" world"));
        assert_eq!(mock.get_requests().len(), 2);
    }

    #[tokio::test]
    async fn stream_continuation_uses_fresh_post_response_attempt_budget() {
        let first = completion_event("cmpl-first", "provider-b", "Hello", "null");
        let second = completion_event("cmpl-second", "provider-a", " world", "\"stop\"");
        let mock = MockHttpClient::new_streaming_response_sequence(vec![
            MockStreamingResponse::sse(StatusCode::BAD_GATEWAY, Vec::new()),
            MockStreamingResponse::sse(StatusCode::OK, vec![MockStreamEvent::Data(first)]),
            MockStreamingResponse::sse(
                StatusCode::OK,
                vec![
                    MockStreamEvent::Data(second),
                    MockStreamEvent::Data("data: [DONE]\n\n".to_string()),
                ],
            ),
        ]);
        let targets = stream_continuation_targets(
            "requested-model",
            &[Some("provider-a"), Some("provider-b")],
            stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
        );
        let server =
            TestServer::new(build_router(AppState::with_client(targets, mock.clone()))).unwrap();

        let response = server
            .post("/v1/completions")
            .json(&json!({
                "model": "requested-model",
                "prompt": "Say hello: ",
                "stream": true
            }))
            .await;

        assert!(response.text().contains(" world"));
        assert_eq!(mock.get_requests().len(), 3);
    }

    #[tokio::test]
    async fn stream_continuation_requests_identity_and_clears_composite_framing() {
        let first = completion_event("cmpl-first", "first", "Hello", "null");
        let second = completion_event("cmpl-second", "second", " world", "\"stop\"");
        let mock = MockHttpClient::new_streaming_response_sequence(vec![
            MockStreamingResponse {
                status: StatusCode::OK,
                content_type: Some("Text/Event-Stream; charset=utf-8".to_string()),
                headers: vec![
                    ("content-length".to_string(), first.len().to_string()),
                    ("content-encoding".to_string(), "IDENTITY".to_string()),
                    ("transfer-encoding".to_string(), "chunked".to_string()),
                    ("trailer".to_string(), "x-checksum".to_string()),
                    ("etag".to_string(), "\"initial-etag\"".to_string()),
                    ("digest".to_string(), "sha-256=YWJj".to_string()),
                    ("content-digest".to_string(), "sha-256=:YWJj:".to_string()),
                    ("repr-digest".to_string(), "sha-256=:YWJj:".to_string()),
                    (
                        "representation-digest".to_string(),
                        "sha-256=:YWJj:".to_string(),
                    ),
                    ("accept-ranges".to_string(), "bytes".to_string()),
                    ("content-range".to_string(), "bytes 0-1/2".to_string()),
                    (
                        "last-modified".to_string(),
                        "Mon, 01 Jan 2024 00:00:00 GMT".to_string(),
                    ),
                    ("x-upstream".to_string(), "initial".to_string()),
                ],
                events: vec![MockStreamEvent::Data(first)],
            },
            MockStreamingResponse {
                status: StatusCode::OK,
                content_type: Some("text/event-stream".to_string()),
                headers: vec![("x-upstream".to_string(), "continuation".to_string())],
                events: vec![
                    MockStreamEvent::Data(second),
                    MockStreamEvent::Data("data: [DONE]\n\n".to_string()),
                ],
            },
        ]);
        let targets = stream_continuation_targets(
            "requested-model",
            &[None, None],
            stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
        );
        let request = axum::extract::Request::builder()
            .method("POST")
            .uri("/v1/completions")
            .header("content-type", "application/json")
            .header("accept-encoding", "gzip, br")
            .body(axum::body::Body::from(
                json!({
                    "model": "requested-model",
                    "prompt": "Say hello: ",
                    "stream": true
                })
                .to_string(),
            ))
            .unwrap();

        let response = build_router(AppState::with_client(targets, mock.clone()))
            .oneshot(request)
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get("x-upstream").unwrap(), "initial");
        assert_eq!(response.headers().get("cache-control").unwrap(), "no-cache");
        for removed in [
            "content-length",
            "content-encoding",
            "transfer-encoding",
            "trailer",
            "etag",
            "digest",
            "content-digest",
            "repr-digest",
            "representation-digest",
            "accept-ranges",
            "content-range",
            "last-modified",
            "connection",
        ] {
            assert!(
                response.headers().get(removed).is_none(),
                "composite response retained {removed}"
            );
        }
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("Hello"));
        assert!(body.contains(" world"));

        let requests = mock.get_requests();
        assert_eq!(requests.len(), 2);
        for request in requests {
            assert_eq!(
                request
                    .headers
                    .iter()
                    .find(|(name, _)| name == "accept-encoding")
                    .map(|(_, value)| value.as_str()),
                Some("identity")
            );
        }
    }

    #[tokio::test]
    async fn stream_continuation_preserves_encoded_initial_response_unwrapped() {
        let first = completion_event("cmpl-first", "first", "Hello", "null");
        let compressed = gzip_bytes(first.as_bytes());
        let mock = MockHttpClient::new_streaming_response_sequence(vec![MockStreamingResponse {
            status: StatusCode::OK,
            content_type: Some("text/event-stream".to_string()),
            headers: vec![
                ("content-encoding".to_string(), "gzip".to_string()),
                ("content-length".to_string(), compressed.len().to_string()),
            ],
            events: vec![MockStreamEvent::Bytes(compressed.clone())],
        }]);
        let targets = stream_continuation_targets(
            "requested-model",
            &[None, None],
            stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
        );
        let request = axum::extract::Request::builder()
            .method("POST")
            .uri("/v1/completions")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({"model":"requested-model","prompt":"P: ","stream":true}).to_string(),
            ))
            .unwrap();

        let response = build_router(AppState::with_client(targets, mock.clone()))
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.headers().get("content-encoding").unwrap(), "gzip");
        assert_eq!(
            response
                .headers()
                .get("content-length")
                .unwrap()
                .to_str()
                .unwrap(),
            compressed.len().to_string().as_str()
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.as_ref(), compressed.as_slice());
        assert_ne!(body.as_ref(), first.as_bytes());
        assert_eq!(mock.get_requests().len(), 1);
    }

    #[tokio::test]
    async fn stream_continuation_encoded_chat_still_uses_baseline_strict_processing() {
        let error = "data: {\"error\":{\"code\":429,\"message\":\"rate limited\"}}\n\n";
        let compressed = gzip_bytes(error.as_bytes());
        let responses = (0..2)
            .map(|_| MockStreamingResponse {
                status: StatusCode::OK,
                content_type: Some("text/event-stream".to_string()),
                headers: vec![("content-encoding".to_string(), "gzip".to_string())],
                events: vec![MockStreamEvent::Bytes(compressed.clone())],
            })
            .collect();
        let mock = MockHttpClient::new_streaming_response_sequence(responses);
        let targets = fallback_targets("gpt-4", 2, vec![429]);
        let request = axum::extract::Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({
                    "model": "gpt-4",
                    "messages": [{"role":"user","content":"Hello"}],
                    "stream": true
                })
                .to_string(),
            ))
            .unwrap();
        let response = strict_stream_continuation_router(targets, mock.clone())
            .oneshot(request)
            .await;

        let response = response.unwrap();
        assert!(response.into_body().collect().await.is_err());
        assert_eq!(mock.get_requests().len(), 1);
    }

    #[tokio::test]
    async fn stream_continuation_encoded_ineligible_strict_completion_is_preserved_safely() {
        let chunk = "data: {\"id\":\"cmpl-one\",\"object\":\"text_completion\",\"created\":1,\"model\":\"provider-model\",\"choices\":[{\"index\":0,\"text\":\"Hello\",\"finish_reason\":\"stop\"}],\"provider_field\":\"remove\"}\n\ndata: [DONE]\n\n";
        let compressed = gzip_bytes(chunk.as_bytes());
        let mock = MockHttpClient::new_streaming_response_sequence(vec![MockStreamingResponse {
            status: StatusCode::OK,
            content_type: Some("text/event-stream".to_string()),
            headers: vec![
                ("content-encoding".to_string(), "gzip".to_string()),
                ("content-length".to_string(), compressed.len().to_string()),
            ],
            events: vec![MockStreamEvent::Bytes(compressed.clone())],
        }]);
        let targets = stream_continuation_targets_with_options(
            "requested-model",
            &[None],
            stream_continuation_fallback(false, 1, 1, None, vec!["/v1/completions"]),
            true,
            false,
        );
        let request = axum::extract::Request::builder()
            .method("POST")
            .uri("/v1/completions")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({"model":"requested-model","prompt":"P: ","stream":true}).to_string(),
            ))
            .unwrap();
        let response = strict_stream_continuation_router(targets, mock.clone())
            .oneshot(request)
            .await
            .unwrap();

        assert_eq!(response.headers().get("content-encoding").unwrap(), "gzip");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.as_ref(), compressed.as_slice());
        assert_eq!(mock.get_requests().len(), 1);
        assert_eq!(
            mock.get_requests()[0]
                .headers
                .iter()
                .find(|(name, _)| name == "accept-encoding")
                .map(|(_, value)| value.as_str()),
            Some("identity")
        );
    }

    #[tokio::test]
    async fn stream_continuation_encoded_strict_chat_and_responses_are_preserved_unwrapped() {
        for (path, request_body) in [
            (
                "/v1/chat/completions",
                json!({
                    "model": "requested-model",
                    "messages": [{"role": "user", "content": "Hello"}],
                    "stream": true
                }),
            ),
            (
                "/v1/responses",
                json!({
                    "model": "requested-model",
                    "input": "Hello",
                    "stream": true
                }),
            ),
        ] {
            let compressed = gzip_bytes(b"encoded-stream");
            let mock =
                MockHttpClient::new_streaming_response_sequence(vec![MockStreamingResponse {
                    status: StatusCode::OK,
                    content_type: Some("text/event-stream".to_string()),
                    headers: vec![
                        ("content-encoding".to_string(), "gzip".to_string()),
                        ("content-length".to_string(), compressed.len().to_string()),
                    ],
                    events: vec![MockStreamEvent::Bytes(compressed.clone())],
                }]);
            let targets = stream_continuation_targets_with_options(
                "requested-model",
                &[None],
                stream_continuation_fallback(true, 1, 1, None, vec![path]),
                true,
                false,
            );
            let request = axum::extract::Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(request_body.to_string()))
                .unwrap();

            let response = strict_stream_continuation_router(targets, mock.clone())
                .oneshot(request)
                .await
                .unwrap();

            assert_eq!(response.headers().get("content-encoding").unwrap(), "gzip");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(body.as_ref(), compressed.as_slice());
            assert_eq!(mock.get_requests().len(), 1);
            assert_eq!(
                mock.get_requests()[0]
                    .headers
                    .iter()
                    .find(|(name, _)| name == "accept-encoding")
                    .map(|(_, value)| value.as_str()),
                Some("identity")
            );
        }
    }

    #[tokio::test]
    async fn stream_continuation_encoded_unrelated_response_still_uses_non_strict_transform() {
        let upstream = gzip_bytes(br#"{"object":"list","data":[]}"#);
        let transformed = br#"{"transformed":true}"#.to_vec();
        let mock = MockHttpClient::new_streaming_response_sequence(vec![MockStreamingResponse {
            status: StatusCode::OK,
            content_type: Some("application/json".to_string()),
            headers: vec![("content-encoding".to_string(), "gzip".to_string())],
            events: vec![MockStreamEvent::Bytes(upstream)],
        }]);
        let targets = stream_continuation_targets_with_options(
            "requested-model",
            &[None],
            stream_continuation_fallback(true, 1, 1, None, vec!["/v1/completions"]),
            false,
            true,
        );
        let transformed_for_closure = transformed.clone();
        let transform: ResponseTransformFn = Arc::new(move |path, _, _, _| {
            (path == "/v1/embeddings")
                .then(|| axum::body::Bytes::from(transformed_for_closure.clone()))
                .map(Some)
                .ok_or_else(|| "unexpected path".to_string())
        });
        let state = AppState::with_client(targets, mock.clone()).with_response_transform(transform);
        let request = axum::extract::Request::builder()
            .method("POST")
            .uri("/v1/embeddings")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({"model":"requested-model","input":"Hello"}).to_string(),
            ))
            .unwrap();

        let response = build_router(state).oneshot(request).await.unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();

        assert_eq!(body.as_ref(), transformed.as_slice());
        assert_eq!(mock.get_requests().len(), 1);
    }

    #[tokio::test]
    async fn stream_continuation_rejects_encoded_and_lookalike_continuations() {
        let first = completion_event("cmpl-first", "first", "Hello", "null");
        let encoded = completion_event("cmpl-encoded", "encoded", " encoded", "null");
        let encoded_bytes = brotli_bytes(encoded.as_bytes());
        let lookalike = completion_event("cmpl-lookalike", "lookalike", " lookalike", "null");
        let malformed = completion_event("cmpl-malformed", "malformed", " malformed", "null");
        let final_event = completion_event("cmpl-final", "final", " world", "\"stop\"");
        let mock = MockHttpClient::new_streaming_response_sequence(vec![
            MockStreamingResponse::sse(StatusCode::OK, vec![MockStreamEvent::Data(first)]),
            MockStreamingResponse {
                status: StatusCode::OK,
                content_type: Some("text/event-stream".to_string()),
                headers: vec![("content-encoding".to_string(), "br".to_string())],
                events: vec![MockStreamEvent::Bytes(encoded_bytes)],
            },
            MockStreamingResponse {
                status: StatusCode::OK,
                content_type: Some("application/x-text/event-streamish".to_string()),
                headers: Vec::new(),
                events: vec![MockStreamEvent::Data(lookalike)],
            },
            MockStreamingResponse {
                status: StatusCode::OK,
                content_type: Some("text/event-stream; charset".to_string()),
                headers: Vec::new(),
                events: vec![MockStreamEvent::Data(malformed)],
            },
            MockStreamingResponse {
                status: StatusCode::OK,
                content_type: Some("TEXT/EVENT-STREAM; CHARSET=UTF-8".to_string()),
                headers: Vec::new(),
                events: vec![
                    MockStreamEvent::Data(final_event),
                    MockStreamEvent::Data("data: [DONE]\n\n".to_string()),
                ],
            },
        ]);
        let targets = stream_continuation_targets(
            "requested-model",
            &[None, None, None, None, None],
            stream_continuation_fallback(true, 2, 4, None, vec!["/v1/completions"]),
        );
        let server =
            TestServer::new(build_router(AppState::with_client(targets, mock.clone()))).unwrap();

        let response = server
            .post("/v1/completions")
            .json(&json!({"model":"requested-model","prompt":"P: ","stream":true}))
            .await;
        let body = response.text();

        assert!(body.contains("Hello"));
        assert!(body.contains(" world"));
        assert!(!body.contains(" encoded"));
        assert!(!body.contains(" lookalike"));
        assert!(!body.contains(" malformed"));
        assert_eq!(mock.get_requests().len(), 5);
    }

    #[tokio::test]
    async fn stream_continuation_initial_lookalike_media_type_is_not_wrapped() {
        let first = completion_event("cmpl-first", "first", "Hello", "null");
        let mock = MockHttpClient::new_streaming_response_sequence(vec![MockStreamingResponse {
            status: StatusCode::OK,
            content_type: Some("application/x-text/event-streamish".to_string()),
            headers: Vec::new(),
            events: vec![MockStreamEvent::Data(first.clone())],
        }]);
        let targets = stream_continuation_targets(
            "requested-model",
            &[None, None],
            stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
        );
        let server =
            TestServer::new(build_router(AppState::with_client(targets, mock.clone()))).unwrap();

        let response = server
            .post("/v1/completions")
            .json(&json!({"model":"requested-model","prompt":"P: ","stream":true}))
            .await;

        assert_eq!(response.text(), first);
        assert_eq!(mock.get_requests().len(), 1);
    }

    #[tokio::test]
    async fn stream_continuation_strict_mode_uses_external_completion_endpoint() {
        let first = completion_event("cmpl-first", "first", "Hello", "null");
        let second = completion_event("cmpl-second", "second", " world", "\"stop\"");
        let mock = MockHttpClient::new_streaming_response_sequence(vec![
            MockStreamingResponse::sse(StatusCode::OK, vec![MockStreamEvent::Data(first)]),
            MockStreamingResponse::sse(
                StatusCode::OK,
                vec![
                    MockStreamEvent::Data(second),
                    MockStreamEvent::Data("data: [DONE]\n\n".to_string()),
                ],
            ),
            MockStreamingResponse {
                status: StatusCode::OK,
                content_type: Some("application/json".to_string()),
                headers: Vec::new(),
                events: vec![MockStreamEvent::Data(
                    "{\"object\":\"list\",\"data\":[],\"model\":\"requested-model\",\"usage\":{\"prompt_tokens\":1,\"total_tokens\":1}}".to_string(),
                )],
            },
        ]);
        let targets = stream_continuation_targets_with_options(
            "requested-model",
            &[None, None],
            stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
            true,
            false,
        );
        let server =
            TestServer::new(strict_stream_continuation_router(targets, mock.clone())).unwrap();

        let response = server
            .post("/v1/completions")
            .json(&json!({"model":"requested-model","prompt":"P: ","stream":true}))
            .await;
        let body = response.text();

        assert!(body.contains("Hello"));
        assert!(body.contains(" world"));
        let continuation_uri: axum::http::Uri = mock.get_requests()[1].uri.parse().unwrap();
        assert_eq!(continuation_uri.path(), "/completions");
        assert_eq!(
            continuation_uri.path_and_query().unwrap().as_str(),
            "/completions"
        );

        let unrelated = server
            .post("/v1/embeddings")
            .json(&json!({"model":"requested-model","input":"Hello"}))
            .await;
        assert_eq!(unrelated.status_code(), StatusCode::OK);

        let requests = mock.get_requests();
        assert_eq!(requests.len(), 3);
        let unrelated_uri: axum::http::Uri = requests[2].uri.parse().unwrap();
        assert_eq!(unrelated_uri.path(), "/embeddings");
        assert_eq!(
            unrelated_uri.path_and_query().unwrap().as_str(),
            "/embeddings"
        );
    }

    #[tokio::test]
    async fn stream_continuation_non_strict_sanitizer_preserves_completion_text() {
        let first = completion_event("cmpl-first", "first", "Hello", "null");
        let second = completion_event("cmpl-second", "second", " world", "\"stop\"");
        let mock = MockHttpClient::new_streaming_sequence(
            StatusCode::OK,
            vec![vec![first], vec![second, "data: [DONE]\n\n".to_string()]],
        );
        let targets = stream_continuation_targets_with_options(
            "requested-model",
            &[None, None],
            stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
            false,
            true,
        );
        let state = AppState::with_client(targets, mock.clone())
            .with_response_transform(create_openai_sanitizer());
        let server = TestServer::new(build_router(state)).unwrap();

        let response = server
            .post("/v1/completions")
            .json(&json!({"model":"requested-model","prompt":"P: ","stream":true}))
            .await;
        let body = response.text();
        let values = completion_sse_values(&body);
        let text: String = values
            .iter()
            .filter_map(|value| value["choices"][0]["text"].as_str())
            .collect();

        assert_eq!(text, "Hello world");
        assert!(body.contains("data: [DONE]"));
        assert_eq!(mock.get_requests().len(), 2);
    }

    #[tokio::test]
    async fn stream_continuation_strict_missing_identity_stays_stable() {
        let first = "data: {\"object\":\"text_completion\",\"choices\":[{\"index\":0,\"text\":\"Hello\",\"finish_reason\":null}]}\n\n".to_string();
        let second = completion_event("cmpl-provider", "provider-model", " world", "\"stop\"");
        let mock = MockHttpClient::new_streaming_sequence(
            StatusCode::OK,
            vec![vec![first], vec![second, "data: [DONE]\n\n".to_string()]],
        );
        let targets = stream_continuation_targets_with_options(
            "requested-model",
            &[None, None],
            stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
            true,
            false,
        );
        let server =
            TestServer::new(strict_stream_continuation_router(targets, mock.clone())).unwrap();

        let response = server
            .post("/v1/completions")
            .add_header("model-override", "requested-model")
            .json(&json!({"model":"body-model","prompt":"P: ","stream":true}))
            .await;
        let body = response.text();
        let values = completion_sse_values(&body);
        let ids: std::collections::HashSet<_> = values.iter().map(|value| &value["id"]).collect();
        let models: std::collections::HashSet<_> =
            values.iter().map(|value| &value["model"]).collect();
        let created: std::collections::HashSet<_> =
            values.iter().map(|value| &value["created"]).collect();

        assert_eq!(ids.len(), 1);
        assert_eq!(models.len(), 1);
        assert_eq!(created.len(), 1);
        assert_eq!(values[0]["model"], "requested-model");
        assert_ne!(values[0]["id"], "cmpl-provider");
        assert_eq!(mock.get_requests().len(), 2);
    }

    #[tokio::test]
    async fn stream_continuation_accumulates_bytes_across_two_body_failures() {
        let first = completion_event("cmpl-first", "first", "Hello", "null");
        let second = completion_event("cmpl-second", "second", " brave", "null");
        let third = completion_event("cmpl-third", "third", " world", "\"stop\"");
        let mock = MockHttpClient::new_streaming_response_sequence(vec![
            MockStreamingResponse::sse(
                StatusCode::OK,
                vec![
                    MockStreamEvent::Data(first),
                    MockStreamEvent::Error("first reset".to_string()),
                ],
            ),
            MockStreamingResponse::sse(
                StatusCode::OK,
                vec![
                    MockStreamEvent::Data(second),
                    MockStreamEvent::Error("second reset".to_string()),
                ],
            ),
            MockStreamingResponse::sse(
                StatusCode::OK,
                vec![
                    MockStreamEvent::Data(third),
                    MockStreamEvent::Data("data: [DONE]\n\n".to_string()),
                ],
            ),
        ]);
        let targets = stream_continuation_targets(
            "requested-model",
            &[None, None, None],
            stream_continuation_fallback(true, 2, 2, None, vec!["/v1/completions"]),
        );
        let server =
            TestServer::new(build_router(AppState::with_client(targets, mock.clone()))).unwrap();

        let response = server
            .post("/v1/completions")
            .json(&json!({"model":"requested-model","prompt":"P: ","stream":true}))
            .await;
        let body = response.text();
        let requests = mock.get_requests();

        assert!(body.contains("Hello"));
        assert!(body.contains(" brave"));
        assert!(body.contains(" world"));
        assert_eq!(requests.len(), 3);
        let third_body: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
        assert_eq!(third_body["prompt"], "P: Hello brave");
    }

    #[tokio::test]
    async fn stream_continuation_finish_reason_makes_later_error_clean() {
        let finished = completion_event("cmpl-first", "first", "Hello", "\"stop\"");
        let mock =
            MockHttpClient::new_streaming_response_sequence(vec![MockStreamingResponse::sse(
                StatusCode::OK,
                vec![
                    MockStreamEvent::Data(finished),
                    MockStreamEvent::Error("late reset".to_string()),
                ],
            )]);
        let targets = stream_continuation_targets(
            "requested-model",
            &[None, None],
            stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
        );
        let request = axum::extract::Request::builder()
            .method("POST")
            .uri("/v1/completions")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({"model":"requested-model","prompt":"P: ","stream":true}).to_string(),
            ))
            .unwrap();

        let response = build_router(AppState::with_client(targets, mock.clone()))
            .oneshot(request)
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();

        assert!(String::from_utf8_lossy(&body).contains("Hello"));
        assert_eq!(mock.get_requests().len(), 1);
    }

    #[tokio::test]
    async fn stream_continuation_finish_reason_makes_later_timeout_clean() {
        let finished = completion_event("cmpl-first", "first", "Hello", "\"stop\"");
        let mock =
            MockHttpClient::new_streaming_response_sequence(vec![MockStreamingResponse::sse(
                StatusCode::OK,
                vec![MockStreamEvent::Data(finished), MockStreamEvent::Pending],
            )]);
        let targets = stream_continuation_targets(
            "requested-model",
            &[None, None],
            stream_continuation_fallback(true, 2, 1, Some(10), vec!["/v1/completions"]),
        );
        let request = axum::extract::Request::builder()
            .method("POST")
            .uri("/v1/completions")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({"model":"requested-model","prompt":"P: ","stream":true}).to_string(),
            ))
            .unwrap();

        let response = build_router(AppState::with_client(targets, mock.clone()))
            .oneshot(request)
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();

        assert!(String::from_utf8_lossy(&body).contains("Hello"));
        assert_eq!(mock.get_requests().len(), 1);
    }

    #[tokio::test]
    async fn stream_continuation_finish_reason_makes_later_framing_errors_clean() {
        let finished = completion_event("cmpl-first", "first", "Hello", "\"stop\"");

        for tail in [
            MockStreamEvent::Data("data: incomplete".to_string()),
            MockStreamEvent::Bytes(vec![b'x'; 64 * 1024 + 1]),
        ] {
            let mock =
                MockHttpClient::new_streaming_response_sequence(vec![MockStreamingResponse::sse(
                    StatusCode::OK,
                    vec![MockStreamEvent::Data(finished.clone()), tail],
                )]);
            let targets = stream_continuation_targets(
                "requested-model",
                &[None, None],
                stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
            );
            let request = axum::extract::Request::builder()
                .method("POST")
                .uri("/v1/completions")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"model":"requested-model","prompt":"P: ","stream":true}).to_string(),
                ))
                .unwrap();

            let response = build_router(AppState::with_client(targets, mock.clone()))
                .oneshot(request)
                .await
                .unwrap();
            let body = response.into_body().collect().await.unwrap().to_bytes();

            assert!(String::from_utf8_lossy(&body).contains("Hello"));
            assert_eq!(mock.get_requests().len(), 1);
        }
    }

    #[tokio::test]
    async fn stream_continuation_strict_late_framing_errors_never_retry() {
        let first = completion_event("cmpl-first", "first", "Hello", "null");
        let fallback = completion_event("cmpl-second", "second", " leaked", "\"stop\"");

        for tail in [
            MockStreamEvent::Data("data: incomplete".to_string()),
            MockStreamEvent::Bytes(vec![b'x'; 64 * 1024 + 1]),
        ] {
            let mock = MockHttpClient::new_streaming_response_sequence(vec![
                MockStreamingResponse::sse(
                    StatusCode::OK,
                    vec![MockStreamEvent::Data(first.clone()), tail],
                ),
                MockStreamingResponse::sse(
                    StatusCode::OK,
                    vec![MockStreamEvent::Data(fallback.clone())],
                ),
            ]);
            let targets = stream_continuation_targets_with_options(
                "requested-model",
                &[None, None],
                stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
                true,
                false,
            );
            let request = axum::extract::Request::builder()
                .method("POST")
                .uri("/v1/completions")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"model":"requested-model","prompt":"P: ","stream":true}).to_string(),
                ))
                .unwrap();

            let response = strict_stream_continuation_router(targets, mock.clone())
                .oneshot(request)
                .await
                .unwrap();

            assert!(response.into_body().collect().await.is_err());
            assert_eq!(mock.get_requests().len(), 1);
        }
    }

    #[tokio::test]
    async fn stream_continuation_strict_first_frame_framing_errors_never_retry() {
        let fallback = completion_event("cmpl-second", "second", " leaked", "\"stop\"");

        for first_event in [
            MockStreamEvent::Data("data: incomplete".to_string()),
            MockStreamEvent::Bytes(vec![b'x'; 64 * 1024 + 1]),
        ] {
            let mock = MockHttpClient::new_streaming_response_sequence(vec![
                MockStreamingResponse::sse(StatusCode::OK, vec![first_event]),
                MockStreamingResponse::sse(
                    StatusCode::OK,
                    vec![MockStreamEvent::Data(fallback.clone())],
                ),
            ]);
            let targets = stream_continuation_targets_with_options(
                "requested-model",
                &[None, None],
                stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
                true,
                false,
            );
            let request = axum::extract::Request::builder()
                .method("POST")
                .uri("/v1/completions")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"model":"requested-model","prompt":"P: ","stream":true}).to_string(),
                ))
                .unwrap();

            let response = strict_stream_continuation_router(targets, mock.clone())
                .oneshot(request)
                .await
                .unwrap();

            assert!(response.into_body().collect().await.is_err());
            assert_eq!(mock.get_requests().len(), 1);
        }
    }

    #[tokio::test]
    async fn stream_continuation_framing_errors_fail_closed_without_retry() {
        let incomplete = completion_event("cmpl-first", "first", "Hello", "null")
            .trim_end_matches('\n')
            .to_string();
        let oversized = vec![b'x'; 64 * 1024 + 1];

        for event in [
            MockStreamEvent::Data(incomplete),
            MockStreamEvent::Bytes(oversized),
        ] {
            let mock =
                MockHttpClient::new_streaming_response_sequence(vec![MockStreamingResponse::sse(
                    StatusCode::OK,
                    vec![event],
                )]);
            let targets = stream_continuation_targets(
                "requested-model",
                &[None, None],
                stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
            );
            let request = axum::extract::Request::builder()
                .method("POST")
                .uri("/v1/completions")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"model":"requested-model","prompt":"P: ","stream":true}).to_string(),
                ))
                .unwrap();

            let response = build_router(AppState::with_client(targets, mock.clone()))
                .oneshot(request)
                .await
                .unwrap();

            assert!(response.into_body().collect().await.is_err());
            assert_eq!(mock.get_requests().len(), 1);
        }
    }

    #[tokio::test]
    async fn stream_continuation_unsafe_initial_events_never_retry_incomplete_prefixes() {
        for unsafe_event in [
            "data: {not-json}\n\n",
            "data: {\"error\":{\"message\":\"secret\"}}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"message\":{\"content\":\"x\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"text\":\"x\",\"finish_reason\":null,\"tool_calls\":[]}]}\n\n",
            "data: {\"choices\":[{\"text\":\"x\",\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":1,\"text\":\"x\",\"finish_reason\":null}]}\n\n",
            "data: {\"type\":\"notification\",\"payload\":{}}\n\n",
            "event: completion\ndata: {\"choices\":[{\"index\":0,\"text\":\"x\",\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"text\":\"x\",\"finish_reason\":null}],\"payload\":{}}\n\n",
            "data: {\"choices\":[{\"index\":0,\"text\":\"x\",\"finish_reason\":null}],\"tool_calls\":[]}\n\n",
        ] {
            let mock = MockHttpClient::new_streaming_sequence(
                StatusCode::OK,
                vec![vec![unsafe_event.to_string()]],
            );
            let targets = stream_continuation_targets(
                "requested-model",
                &[None, None],
                stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
            );
            let server =
                TestServer::new(build_router(AppState::with_client(targets, mock.clone())))
                    .unwrap();

            let response = server
                .post("/v1/completions")
                .json(&json!({"model":"requested-model","prompt":"P: ","stream":true}))
                .await;

            assert!(response.text().contains(unsafe_event.trim()));
            assert_eq!(mock.get_requests().len(), 1, "unsafe event: {unsafe_event}");
        }
    }

    #[tokio::test]
    async fn stream_continuation_unsafe_continuation_events_are_not_spliced() {
        for unsafe_event in [
            "data: {not-json}\n\n",
            "data: {\"error\":{\"message\":\"continuation-secret\"}}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"continuation-secret\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"message\":{\"content\":\"continuation-secret\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":1,\"text\":\"continuation-secret\",\"finish_reason\":null}]}\n\n",
            "data: {\"type\":\"notification\",\"payload\":{\"value\":\"continuation-secret\"}}\n\n",
            "event: completion\ndata: {\"choices\":[{\"index\":0,\"text\":\"continuation-secret\",\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"text\":\"continuation-secret\",\"finish_reason\":null}],\"payload\":{}}\n\n",
            "data: {\"choices\":[{\"index\":0,\"text\":\"continuation-secret\",\"finish_reason\":null}],\"tool_calls\":[]}\n\n",
        ] {
            let first = completion_event("cmpl-first", "first", "Hello", "null");
            let mock = MockHttpClient::new_streaming_sequence(
                StatusCode::OK,
                vec![vec![first], vec![unsafe_event.to_string()]],
            );
            let targets = stream_continuation_targets(
                "requested-model",
                &[None, None],
                stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
            );
            let request = axum::extract::Request::builder()
                .method("POST")
                .uri("/v1/completions")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"model":"requested-model","prompt":"P: ","stream":true}).to_string(),
                ))
                .unwrap();
            let response = build_router(AppState::with_client(targets, mock.clone()))
                .oneshot(request)
                .await
                .unwrap();
            let mut stream = response.into_body().into_data_stream();
            let mut visible = Vec::new();
            let mut saw_error = false;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(bytes) => visible.extend_from_slice(&bytes),
                    Err(_) => {
                        saw_error = true;
                        break;
                    }
                }
            }

            let visible = String::from_utf8(visible).unwrap();
            assert!(visible.contains("Hello"));
            assert!(!visible.contains("continuation-secret"));
            assert!(saw_error, "unsafe event: {unsafe_event}");
            assert_eq!(mock.get_requests().len(), 2);
        }
    }

    #[tokio::test]
    async fn stream_continuation_mixed_trust_uses_least_privilege_strict_policy() {
        let first = completion_event("cmpl-first", "first", "Hello", "null");
        let error = "data: {\"error\":{\"code\":500,\"message\":\"provider-secret\"}}\n\n";
        let mock = MockHttpClient::new_streaming_sequence(
            StatusCode::OK,
            vec![vec![first, error.to_string()]],
        );
        let trusted = Target::builder()
            .url("https://trusted.example.com/".parse().unwrap())
            .trusted(true)
            .build();
        let untrusted = Target::builder()
            .url("https://untrusted.example.com/".parse().unwrap())
            .trusted(false)
            .build();
        let targets = stream_continuation_targets_from_targets(
            "requested-model",
            vec![trusted, untrusted],
            stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
            true,
            true,
        );
        let server =
            TestServer::new(strict_stream_continuation_router(targets, mock.clone())).unwrap();

        let response = server
            .post("/v1/completions")
            .json(&json!({"model":"requested-model","prompt":"P: ","stream":true}))
            .await;
        let body = response.text();

        assert!(body.contains("Hello"));
        assert!(!body.contains("provider-secret"));
        assert!(body.contains("Internal server error"));
        assert_eq!(mock.get_requests().len(), 1);
    }

    #[tokio::test]
    async fn stream_continuation_mixed_sanitize_policy_removes_provider_fields() {
        let chunk = "data: {\"id\":\"cmpl-first\",\"object\":\"text_completion\",\"created\":1,\"model\":\"provider\",\"choices\":[{\"index\":0,\"text\":\"Hello\",\"finish_reason\":\"stop\"}],\"provider_secret\":\"remove-me\"}\n\ndata:[DONE]\n\n";
        let mock =
            MockHttpClient::new_streaming_sequence(StatusCode::OK, vec![vec![chunk.to_string()]]);
        let permissive = Target::builder()
            .url("https://permissive.example.com/".parse().unwrap())
            .sanitize_response(false)
            .build();
        let sanitizing = Target::builder()
            .url("https://sanitizing.example.com/".parse().unwrap())
            .sanitize_response(true)
            .build();
        let targets = stream_continuation_targets_from_targets(
            "requested-model",
            vec![permissive, sanitizing],
            stream_continuation_fallback(true, 2, 1, None, vec!["/v1/completions"]),
            false,
            false,
        );
        let state = AppState::with_client(targets, mock.clone())
            .with_response_transform(create_openai_sanitizer());
        let server = TestServer::new(build_router(state)).unwrap();

        let response = server
            .post("/v1/completions")
            .json(&json!({"model":"requested-model","prompt":"P: ","stream":true}))
            .await;
        let body = response.text();

        assert!(body.contains("Hello"));
        assert!(body.contains("data:[DONE]"));
        assert!(!body.contains("provider_secret"));
        assert!(!body.contains("remove-me"));
        assert_eq!(mock.get_requests().len(), 1);
    }

    #[tokio::test]
    async fn stream_continuation_custom_headers_cannot_restore_composite_metadata() {
        let forbidden = [
            "content-length",
            "content-encoding",
            "transfer-encoding",
            "trailer",
            "connection",
            "etag",
            "digest",
            "content-digest",
            "repr-digest",
            "representation-digest",
            "accept-ranges",
            "content-range",
            "last-modified",
        ];
        let custom_headers = json!({
            "content-length": "1",
            "content-encoding": "gzip",
            "transfer-encoding": "chunked",
            "trailer": "x-checksum",
            "connection": "keep-alive",
            "etag": "\"custom\"",
            "digest": "sha-256=YWJj",
            "content-digest": "sha-256=:YWJj:",
            "repr-digest": "sha-256=:YWJj:",
            "representation-digest": "sha-256=:YWJj:",
            "accept-ranges": "bytes",
            "content-range": "bytes 0-1/2",
            "last-modified": "Mon, 01 Jan 2024 00:00:00 GMT",
            "content-type": "application/json",
            "x-safe-custom": "retained"
        });

        for scope in ["provider", "pool"] {
            let mut provider = json!({"url": "https://provider.example.com/"});
            let mut pool = json!({
                "providers": [provider.clone()],
                "fallback": {
                    "enabled": true,
                    "max_attempts": 1,
                    "stream_continuation": {
                        "enabled": true,
                        "endpoints": ["/v1/completions"],
                        "max_attempts": 0,
                        "max_buffered_bytes": 1024
                    }
                }
            });
            if scope == "provider" {
                provider["response_headers"] = custom_headers.clone();
                pool["providers"] = json!([provider]);
            } else {
                pool["response_headers"] = custom_headers.clone();
            }
            let config: target::ConfigFile = serde_json::from_value(json!({
                "targets": {"requested-model": pool},
                "auth": null,
                "strict_mode": false
            }))
            .unwrap();
            let targets = target::Targets::from_config(config).unwrap();
            let terminal = completion_event("cmpl-first", "first", "Hello", "\"stop\"");
            let mock = MockHttpClient::new_streaming_sequence(
                StatusCode::OK,
                vec![vec![terminal, "data:[DONE]\n\n".to_string()]],
            );
            let request = axum::extract::Request::builder()
                .method("POST")
                .uri("/v1/completions")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"model":"requested-model","prompt":"P: ","stream":true}).to_string(),
                ))
                .unwrap();

            let response = build_router(AppState::with_client(targets, mock))
                .oneshot(request)
                .await
                .unwrap();

            assert_eq!(
                response.headers().get("content-type").unwrap(),
                "text/event-stream",
                "scope: {scope}"
            );
            assert_eq!(
                response.headers().get("x-safe-custom").unwrap(),
                "retained",
                "scope: {scope}"
            );
            assert_eq!(response.headers().get("cache-control").unwrap(), "no-cache");
            for name in forbidden {
                assert!(
                    response.headers().get(name).is_none(),
                    "scope {scope} restored {name}"
                );
            }
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert!(String::from_utf8_lossy(&body).contains("Hello"));
        }
    }

    #[tokio::test]
    async fn stream_continuation_retains_only_response_headers_common_to_all_providers() {
        let config: target::ConfigFile = serde_json::from_value(json!({
            "targets": {
                "requested-model": {
                    "response_headers": {"x-common": "retained"},
                    "providers": [
                        {
                            "url": "https://first.example.com/",
                            "response_headers": {"x-provider-price": "1"}
                        },
                        {
                            "url": "https://second.example.com/",
                            "response_headers": {"x-provider-price": "2"}
                        }
                    ],
                    "fallback": {
                        "enabled": true,
                        "max_attempts": 2,
                        "stream_continuation": {
                            "enabled": true,
                            "endpoints": ["/v1/completions"],
                            "max_attempts": 1,
                            "max_buffered_bytes": 1024
                        }
                    }
                }
            },
            "auth": null,
            "strict_mode": false
        }))
        .unwrap();
        let targets = target::Targets::from_config(config).unwrap();
        let terminal = completion_event("cmpl-first", "first", "Hello", "\"stop\"");
        let mock = MockHttpClient::new_streaming_sequence(
            StatusCode::OK,
            vec![vec![terminal, "data:[DONE]\n\n".to_string()]],
        );
        let request = axum::extract::Request::builder()
            .method("POST")
            .uri("/v1/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"model":"requested-model","prompt":"P: ","stream":true}).to_string(),
            ))
            .unwrap();

        let response = build_router(AppState::with_client(targets, mock))
            .oneshot(request)
            .await
            .unwrap();

        assert_eq!(response.headers().get("x-common").unwrap(), "retained");
        assert!(response.headers().get("x-provider-price").is_none());
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("Hello"));
    }

    #[tokio::test]
    async fn stream_continuation_body_cancellation_releases_provider_guard() {
        let mock =
            MockHttpClient::new_streaming_response_sequence(vec![MockStreamingResponse::sse(
                StatusCode::OK,
                vec![MockStreamEvent::Pending],
            )]);
        let target = Target::builder()
            .url("https://stream.example.com/".parse().unwrap())
            .build();
        let pool = ProviderPool::with_config(
            vec![Provider::with_concurrency_limit(target, 1, 1)],
            None,
            None,
            None,
            Some(stream_continuation_fallback(
                true,
                1,
                1,
                None,
                vec!["/v1/completions"],
            )),
            LoadBalanceStrategy::Priority,
            false,
            Vec::new(),
        );
        let observed_pool = pool.clone();
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert("requested-model".to_string(), pool);
        let targets = target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };
        let request = axum::extract::Request::builder()
            .method("POST")
            .uri("/v1/completions")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({"model":"requested-model","prompt":"P: ","stream":true}).to_string(),
            ))
            .unwrap();

        let response = build_router(AppState::with_client(targets, mock))
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(observed_pool.providers()[0].active_connections(), 1);

        drop(response);

        assert_eq!(observed_pool.providers()[0].active_connections(), 0);
    }

    /// Retry on an upstream 429. Used by the embedded-error tests below.
    fn embedded_error_targets(alias: &str, n: usize) -> target::Targets {
        fallback_targets(alias, n, vec![429])
    }

    // Some upstreams return HTTP 200 and put the real error in the body. These
    // tests exercise the embedded-error detection + retry in target_message_handler.

    #[tokio::test]
    async fn test_streaming_embedded_error_retries_then_exhausts_to_503() {
        // 200 stream whose first frame is a `429` error envelope. onwards must
        // retry across providers and, when exhausted, return a sanitized 503 —
        // never the upstream 429.
        let error_frame =
            "data: {\"error\":{\"code\":429,\"message\":\"Provider returned error\"}}\n\n"
                .to_string();
        let mock = MockHttpClient::new_streaming(StatusCode::OK, vec![error_frame]);
        let app_state = AppState::with_client(embedded_error_targets("gpt-4", 2), mock.clone());
        let server = TestServer::new(build_router(app_state)).unwrap();

        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "gpt-4", "stream": true,
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(
            response.status_code(),
            503,
            "exhausted retries must surface a sanitized 503, not the upstream 429"
        );
        assert_eq!(
            mock.get_requests().len(),
            2,
            "both providers should be tried"
        );
    }

    #[tokio::test]
    async fn test_streaming_embedded_error_retries_then_succeeds() {
        // First provider returns the 200+error frame; the retry succeeds and the
        // user is served the successful stream — the throttle stays invisible.
        let error_frame =
            "data: {\"error\":{\"code\":429,\"message\":\"Provider returned error\"}}\n\n"
                .to_string();
        let ok_chunk = "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n".to_string();
        let done = "data: [DONE]\n\n".to_string();
        let mock = MockHttpClient::new_streaming_sequence(
            StatusCode::OK,
            vec![vec![error_frame], vec![ok_chunk, done]],
        );
        let app_state = AppState::with_client(embedded_error_targets("gpt-4", 2), mock.clone());
        let server = TestServer::new(build_router(app_state)).unwrap();

        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "gpt-4", "stream": true,
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(
            response.status_code(),
            200,
            "a successful retry must be served transparently"
        );
        assert_eq!(
            mock.get_requests().len(),
            2,
            "first attempt errored, second succeeded"
        );
    }

    #[tokio::test]
    async fn test_unary_embedded_error_collapses_to_503() {
        // The same envelope on a non-streaming 200 body collapses to a 503.
        let body = r#"{"error":{"code":429,"message":"Provider returned error"}}"#;
        let mock = MockHttpClient::new(StatusCode::OK, body);
        let app_state = AppState::with_client(embedded_error_targets("gpt-4", 2), mock.clone());
        let server = TestServer::new(build_router(app_state)).unwrap();

        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "gpt-4",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 503);
        assert_eq!(
            mock.get_requests().len(),
            2,
            "both providers should be tried"
        );
    }

    #[tokio::test]
    async fn test_streaming_keepalive_before_error_is_still_detected() {
        // A keep-alive comment precedes the error frame; the peek must skip it
        // and still detect the 429, retry, and exhaust to 503.
        let keepalive = ": keep-alive\n\n".to_string();
        let error_frame =
            "data: {\"error\":{\"code\":429,\"message\":\"Provider returned error\"}}\n\n"
                .to_string();
        let mock = MockHttpClient::new_streaming(StatusCode::OK, vec![keepalive, error_frame]);
        let app_state = AppState::with_client(embedded_error_targets("gpt-4", 2), mock.clone());
        let server = TestServer::new(build_router(app_state)).unwrap();

        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "gpt-4", "stream": true,
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(
            response.status_code(),
            503,
            "an error after a keep-alive frame must still be detected"
        );
        assert_eq!(mock.get_requests().len(), 2);
    }

    // Some upstreams also fail by returning a `200 OK` with an *empty* body (no
    // tokens, no error envelope — just EOF). These tests exercise the
    // empty-body ("Mode C") detection + retry, and crucially that a valid stream
    // which simply hasn't produced a token yet is NOT mistaken for empty.

    // A normal streaming content frame (classified as data, never an error).
    const OK_CONTENT_FRAME: &str = "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n";

    #[tokio::test]
    async fn test_streaming_empty_body_retries_then_succeeds() {
        // First provider opens a 200 stream that ends with zero frames (terminal
        // empty); the retry to a healthy provider serves the real stream. The
        // empty response must never reach the client.
        let mock = MockHttpClient::new_streaming_sequence(
            StatusCode::OK,
            vec![
                vec![],                             // provider 0: empty stream
                vec![OK_CONTENT_FRAME.to_string()], // provider 1: real content
            ],
        );
        let app_state =
            AppState::with_client(fallback_targets("gpt-4", 2, vec![502]), mock.clone());
        let server = TestServer::new(build_router(app_state)).unwrap();

        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "gpt-4", "stream": true,
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 200, "the retry's success is served");
        assert!(
            response.text().contains("hi"),
            "client receives the healthy provider's content"
        );
        assert_eq!(
            mock.get_requests().len(),
            2,
            "empty stream must trigger a retry"
        );
    }

    #[tokio::test]
    async fn test_streaming_empty_body_exhausts_to_503() {
        // Every provider returns an empty 200 stream — exhausted retries surface a
        // sanitized 503, never a bodyless 200.
        let mock = MockHttpClient::new_streaming(StatusCode::OK, vec![]);
        let app_state =
            AppState::with_client(fallback_targets("gpt-4", 2, vec![502]), mock.clone());
        let server = TestServer::new(build_router(app_state)).unwrap();

        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "gpt-4", "stream": true,
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 503);
        assert_eq!(
            mock.get_requests().len(),
            2,
            "both providers should be tried"
        );
    }

    #[tokio::test]
    async fn test_unary_empty_body_retries_and_exhausts_to_503() {
        // The unary form: a 200 with an empty body (the deserialize-EOF case) is
        // retried across providers and collapses to a 503 when exhausted.
        let mock = MockHttpClient::new(StatusCode::OK, "");
        let app_state =
            AppState::with_client(fallback_targets("gpt-4", 2, vec![502]), mock.clone());
        let server = TestServer::new(build_router(app_state)).unwrap();

        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "gpt-4",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 503);
        assert_eq!(
            mock.get_requests().len(),
            2,
            "empty unary body must trigger a retry"
        );
    }

    #[tokio::test]
    async fn test_streaming_keepalive_then_token_is_forwarded_not_retried() {
        // A valid stream that leads with a keep-alive comment and only *then*
        // emits its first token must be forwarded as-is — the keep-alive is not
        // "empty". This is the false-positive guard for slow-first-token streams.
        let keepalive = ": keep-alive\n\n".to_string();
        let mock = MockHttpClient::new_streaming(
            StatusCode::OK,
            vec![keepalive, OK_CONTENT_FRAME.to_string()],
        );
        let app_state =
            AppState::with_client(fallback_targets("gpt-4", 2, vec![502]), mock.clone());
        let server = TestServer::new(build_router(app_state)).unwrap();

        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "gpt-4", "stream": true,
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 200);
        assert!(response.text().contains("hi"));
        assert_eq!(
            mock.get_requests().len(),
            1,
            "a valid stream must not be retried just because the token followed a keep-alive"
        );
    }

    #[tokio::test]
    async fn test_streaming_only_keepalives_so_far_is_forwarded_not_retried() {
        // The stream has emitted only keep-alive comments by the time the peek
        // budget/event-count is reached, and has NOT terminated. That is a
        // slow-but-alive stream, not a terminal empty — it must be forwarded, not
        // retried (and definitely not collapsed to 503).
        let keepalive = ": keep-alive\n\n".to_string();
        let mock = MockHttpClient::new_streaming(
            StatusCode::OK,
            vec![
                keepalive.clone(),
                keepalive.clone(),
                keepalive.clone(),
                keepalive.clone(),
                keepalive,
            ],
        );
        let app_state =
            AppState::with_client(fallback_targets("gpt-4", 2, vec![502]), mock.clone());
        let server = TestServer::new(build_router(app_state)).unwrap();

        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "gpt-4", "stream": true,
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 200);
        assert_eq!(
            mock.get_requests().len(),
            1,
            "keep-alives without a terminal end must not be treated as empty"
        );
    }

    #[tokio::test]
    async fn test_request_and_response_details() {
        // Create a target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "test-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .onwards_key("test-api-key".to_string())
                    .build(),
            ),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        let mock_response_body = r#"{"id": "test-response", "object": "chat.completion", "choices": [{"message": {"content": "Hello from mock!"}}]}"#;
        let mock_client = MockHttpClient::new(StatusCode::OK, mock_response_body);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Make a request
        let request_body = json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "Hello!"}],
            "temperature": 0.7
        });

        let response = server
            .post("/v1/chat/completions")
            .json(&request_body)
            .await;

        // Assert on the response
        assert_eq!(response.status_code(), 200);
        let response_body: serde_json::Value = response.json();
        assert_eq!(response_body["id"], "test-response");
        assert_eq!(
            response_body["choices"][0]["message"]["content"],
            "Hello from mock!"
        );

        // Assert on the request that was sent
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let request = &requests[0];

        // Check HTTP method
        assert_eq!(request.method, "POST");

        // Check URL was correctly constructed
        assert_eq!(request.uri, "https://api.example.com/v1/chat/completions");

        // Check headers
        let auth_header = request
            .headers
            .iter()
            .find(|(key, _)| key == "authorization")
            .map(|(_, value)| value);
        assert_eq!(auth_header, Some(&"Bearer test-api-key".to_string()));

        let host_header = request
            .headers
            .iter()
            .find(|(key, _)| key == "host")
            .map(|(_, value)| value);
        assert_eq!(host_header, Some(&"api.example.com".to_string()));

        let content_type_header = request
            .headers
            .iter()
            .find(|(key, _)| key == "content-type")
            .map(|(_, value)| value);
        assert_eq!(content_type_header, Some(&"application/json".to_string()));

        // Check request body
        let forwarded_body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(forwarded_body["model"], "test-model");
        assert_eq!(forwarded_body["messages"][0]["content"], "Hello!");
        assert_eq!(forwarded_body["temperature"], 0.7);
    }

    #[tokio::test]
    async fn test_model_override_header_takes_precedence() {
        // Create two targets
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "header-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.header.com".parse().unwrap())
                    .onwards_key("header-key".to_string())
                    .build(),
            ),
        );
        targets_map.insert(
            "body-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.body.com".parse().unwrap())
                    .onwards_key("body-key".to_string())
                    .build(),
            ),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Make request with model in JSON body AND model-override header
        let response = server
            .post("/v1/chat/completions")
            .add_header("model-override", "header-model") // Should use this target
            .json(&json!({
                "model": "body-model",  // Should ignore this one
                "messages": [{"role": "user", "content": "Test"}]
            }))
            .await;

        assert_eq!(response.status_code(), 200);

        // Verify request was sent to the header-model target, not body-model
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let request = &requests[0];
        assert!(request.uri.contains("api.header.com"));
        assert!(!request.uri.contains("api.body.com"));

        // Verify authorization header uses the header-model target's key
        let auth_header = request
            .headers
            .iter()
            .find(|(key, _)| key == "authorization")
            .map(|(_, value)| value);
        assert_eq!(auth_header, Some(&"Bearer header-key".to_string()));
    }

    #[tokio::test]
    async fn test_models_endpoint_returns_proper_model_list() {
        // Create multiple targets
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "gpt-4".to_string(),
            pool(
                Target::builder()
                    .url("https://api.openai.com".parse().unwrap())
                    .onwards_key("sk-openai-key".to_string())
                    .build(),
            ),
        );
        targets_map.insert(
            "claude-3".to_string(),
            pool(
                Target::builder()
                    .url("https://api.anthropic.com".parse().unwrap())
                    .onwards_key("sk-ant-key".to_string())
                    .build(),
            ),
        );
        targets_map.insert(
            "gemini-pro".to_string(),
            pool(
                Target::builder()
                    .url("https://api.google.com".parse().unwrap())
                    .onwards_model("gemini-1.5-pro".to_string())
                    .build(),
            ),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"unused": "response"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Request the /v1/models endpoint
        let response = server.get("/v1/models").await;

        assert_eq!(response.status_code(), 200);

        let response_body: serde_json::Value = response.json();

        // Verify the structure of the response
        assert_eq!(response_body["object"], "list");
        assert!(response_body["data"].is_array());

        let models = response_body["data"].as_array().unwrap();
        assert_eq!(models.len(), 3);

        // Check that all our models are present
        let model_ids: Vec<&str> = models
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();

        assert!(model_ids.contains(&"gpt-4"));
        assert!(model_ids.contains(&"claude-3"));
        assert!(model_ids.contains(&"gemini-pro"));

        // Verify model structure
        for model in models {
            assert_eq!(model["object"], "model");
            assert_eq!(model["owned_by"], "None");
            assert!(model["id"].is_string());
            // created field is optional and None in our implementation
        }

        // Verify that NO requests were made to the mock client (models are handled locally)
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 0);
    }

    #[tokio::test]
    async fn test_models_endpoint_filters_by_bearer_token() {
        use crate::auth::ConstantTimeString;
        use std::collections::HashSet;

        // Create keys for different models
        let mut gpt4_keys = HashSet::new();
        gpt4_keys.insert(ConstantTimeString::from("gpt4-token".to_string()));

        let mut claude_keys = HashSet::new();
        claude_keys.insert(ConstantTimeString::from("claude-token".to_string()));

        // Create targets with different access keys
        let targets_map = Arc::new(DashMap::new());

        // gpt-4: requires gpt4-token
        targets_map.insert(
            "gpt-4".to_string(),
            pool(
                Target::builder()
                    .url("https://api.openai.com".parse().unwrap())
                    .keys(gpt4_keys)
                    .build(),
            ),
        );

        // claude-3: requires claude-token
        targets_map.insert(
            "claude-3".to_string(),
            pool(
                Target::builder()
                    .url("https://api.anthropic.com".parse().unwrap())
                    .keys(claude_keys)
                    .build(),
            ),
        );

        // gemini-pro: no keys required (public)
        targets_map.insert(
            "gemini-pro".to_string(),
            pool(
                Target::builder()
                    .url("https://api.google.com".parse().unwrap())
                    .build(),
            ),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"unused": "response"}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Test 1: No bearer token - should only see public models
        let response = server.get("/v1/models").await;
        assert_eq!(response.status_code(), 200);

        let response_body: serde_json::Value = response.json();
        let models = response_body["data"].as_array().unwrap();
        assert_eq!(models.len(), 1); // Only gemini-pro

        let model_ids: Vec<&str> = models
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        assert!(model_ids.contains(&"gemini-pro"));

        // Test 2: Valid bearer token for gpt-4 - should see gpt-4 + public models
        let response = server
            .get("/v1/models")
            .add_header("authorization", "Bearer gpt4-token")
            .await;
        assert_eq!(response.status_code(), 200);

        let response_body: serde_json::Value = response.json();
        let models = response_body["data"].as_array().unwrap();
        assert_eq!(models.len(), 2); // gpt-4 + gemini-pro

        let model_ids: Vec<&str> = models
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        assert!(model_ids.contains(&"gpt-4"));
        assert!(model_ids.contains(&"gemini-pro"));

        // Test 3: Valid bearer token for claude - should see claude + public models
        let response = server
            .get("/v1/models")
            .add_header("authorization", "Bearer claude-token")
            .await;
        assert_eq!(response.status_code(), 200);

        let response_body: serde_json::Value = response.json();
        let models = response_body["data"].as_array().unwrap();
        assert_eq!(models.len(), 2); // claude-3 + gemini-pro

        let model_ids: Vec<&str> = models
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        assert!(model_ids.contains(&"claude-3"));
        assert!(model_ids.contains(&"gemini-pro"));

        // Test 4: Invalid bearer token - should only see public models
        let response = server
            .get("/v1/models")
            .add_header("authorization", "Bearer invalid-token")
            .await;
        assert_eq!(response.status_code(), 200);

        let response_body: serde_json::Value = response.json();
        let models = response_body["data"].as_array().unwrap();
        assert_eq!(models.len(), 1); // Only gemini-pro

        let model_ids: Vec<&str> = models
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        assert!(model_ids.contains(&"gemini-pro"));
    }

    #[tokio::test]
    async fn test_rate_limiting_blocks_requests() {
        use crate::target::{RateLimitExceeded, RateLimiter, Target, Targets};
        use std::sync::Arc;

        // Create a mock rate limiter that blocks requests
        #[derive(Debug)]
        struct BlockingRateLimiter;

        impl RateLimiter for BlockingRateLimiter {
            fn check(&self) -> Result<(), RateLimitExceeded> {
                Err(RateLimitExceeded) // Always block
            }
        }

        // Create a target with a blocking rate limiter
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "rate-limited-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .limiter(Arc::new(BlockingRateLimiter) as Arc<dyn RateLimiter>)
                    .build(),
            ),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Make a request to the rate-limited model
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "rate-limited-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        // Should get 429 Too Many Requests
        assert_eq!(response.status_code(), 429);

        // Should return proper error structure wrapped in OpenAI error envelope
        let response_body: serde_json::Value = response.json();
        assert_eq!(response_body["error"]["type"], "rate_limit_error");
        assert_eq!(response_body["error"]["code"], "rate_limit");

        // Verify no request was made to the upstream (since it was rate limited)
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 0);
    }

    #[tokio::test]
    async fn test_rate_limiting_allows_requests() {
        use crate::target::{RateLimitExceeded, RateLimiter, Target, Targets};
        use std::sync::Arc;

        // Create a mock rate limiter that allows requests
        #[derive(Debug)]
        struct AllowingRateLimiter;

        impl RateLimiter for AllowingRateLimiter {
            fn check(&self) -> Result<(), RateLimitExceeded> {
                Ok(()) // Always allow
            }
        }

        // Create a target with an allowing rate limiter
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "rate-limited-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .limiter(Arc::new(AllowingRateLimiter) as Arc<dyn RateLimiter>)
                    .build(),
            ),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Make a request to the rate-limited model
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "rate-limited-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        // Should get 200 OK since rate limiter allows it
        assert_eq!(response.status_code(), 200);

        // Verify request was made to the upstream
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].uri.contains("api.example.com"));
    }

    #[tokio::test]
    async fn test_rate_limiting_with_mixed_targets() {
        use crate::target::{RateLimitExceeded, RateLimiter, Target, Targets};
        use std::sync::Arc;

        // Create different rate limiters
        #[derive(Debug)]
        struct BlockingRateLimiter;
        impl RateLimiter for BlockingRateLimiter {
            fn check(&self) -> Result<(), RateLimitExceeded> {
                Err(RateLimitExceeded)
            }
        }

        #[derive(Debug)]
        struct AllowingRateLimiter;
        impl RateLimiter for AllowingRateLimiter {
            fn check(&self) -> Result<(), RateLimitExceeded> {
                Ok(())
            }
        }

        // Create targets with different rate limiting behavior
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "blocked-model".to_string(),
            pool(
                Target::builder()
                    .url("https://blocked.example.com".parse().unwrap())
                    .limiter(Arc::new(BlockingRateLimiter) as Arc<dyn RateLimiter>)
                    .build(),
            ),
        );
        targets_map.insert(
            "allowed-model".to_string(),
            pool(
                Target::builder()
                    .url("https://allowed.example.com".parse().unwrap())
                    .limiter(Arc::new(AllowingRateLimiter) as Arc<dyn RateLimiter>)
                    .build(),
            ),
        );
        targets_map.insert(
            "unlimited-model".to_string(),
            pool(
                Target::builder()
                    .url("https://unlimited.example.com".parse().unwrap())
                    .build(),
            ), // No rate limiter
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Test blocked model - should get 429
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "blocked-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;
        assert_eq!(response.status_code(), 429);

        // Test allowed model - should get 200
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "allowed-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;
        assert_eq!(response.status_code(), 200);

        // Test unlimited model - should get 200
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "unlimited-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;
        assert_eq!(response.status_code(), 200);

        // Verify only allowed and unlimited models made upstream requests
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 2);

        let urls: Vec<&str> = requests.iter().map(|r| r.uri.as_str()).collect();
        assert!(urls.contains(&"https://allowed.example.com/v1/chat/completions"));
        assert!(urls.contains(&"https://unlimited.example.com/v1/chat/completions"));
        assert!(!urls.iter().any(|&url| url.contains("blocked.example.com")));
    }

    #[tokio::test]
    async fn test_concurrency_limiting_below_limits() {
        // Create a target with concurrency limit
        let targets_map = Arc::new(DashMap::new());
        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();
        targets_map.insert(
            "limited-model".to_string(),
            ProviderPool::new(vec![Provider::with_concurrency_limit(target, 1, 5)]),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Request should succeed (within concurrency limit)
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "limited-model",
                "messages": [{"role": "user", "content": "Test"}]
            }))
            .await;

        assert_eq!(response.status_code(), 200);

        // Verify request made it through
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);
    }

    #[tokio::test]
    async fn test_concurrency_limiting_at_limits() {
        use std::rc::Rc;

        // Create a target with pool-level concurrency limit of 1
        let targets_map = Arc::new(DashMap::new());
        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();
        targets_map.insert(
            "limited-model".to_string(),
            ProviderPool::with_config(
                vec![Provider::new(target, 1)],
                None,
                None,
                Some(ConcurrencyLimiter::with_limit(1)),
                None,
                target::LoadBalanceStrategy::default(),
                false,
                Vec::new(),
            ),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        let mock_client =
            test_utils::TriggeredMockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = Rc::new(TestServer::new(router).unwrap());

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                // Start first request in background (it will block waiting for trigger)
                let server_clone = Rc::clone(&server);
                let handle1 = tokio::task::spawn_local(async move {
                    server_clone
                        .post("/v1/chat/completions")
                        .json(&json!({"model": "limited-model", "messages": []}))
                        .await
                });

                // Give it a moment to start and acquire the permit
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;

                // Now send second request - should be rejected immediately since limit is 1
                let response2 = server
                    .post("/v1/chat/completions")
                    .json(&json!({"model": "limited-model", "messages": []}))
                    .await;

                // Second request should be rejected (concurrency limit exceeded)
                assert_eq!(response2.status_code(), 429);
                let body: serde_json::Value = response2.json();
                assert_eq!(body["error"]["code"], "concurrency_limit_exceeded");

                // Complete the first request
                mock_client.complete_request(0);
                let response1 = handle1.await.unwrap();
                assert_eq!(response1.status_code(), 200);

                // Only 1 request made it to the mock client
                assert_eq!(mock_client.get_requests().len(), 1);
            })
            .await;
    }

    #[tokio::test]
    async fn test_per_key_concurrency_limiting() {
        use std::rc::Rc;

        // Create a target without concurrency limit
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "test-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .build(),
            ),
        );

        // Set up per-key concurrency limiter
        let key_concurrency_limiters = Arc::new(DashMap::new());
        key_concurrency_limiters.insert(
            "sk-limited-key".to_string(),
            ConcurrencyLimiter::with_limit(1),
        );

        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters,
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        let mock_client =
            test_utils::TriggeredMockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state = AppState::with_client(targets, mock_client.clone());
        let router = build_router(app_state);
        let server = Rc::new(TestServer::new(router).unwrap());

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                // Start first request with the limited key (it will block waiting for trigger)
                let server_clone = Rc::clone(&server);
                let handle1 = tokio::task::spawn_local(async move {
                    server_clone
                        .post("/v1/chat/completions")
                        .add_header(
                            axum::http::HeaderName::from_static("authorization"),
                            axum::http::HeaderValue::from_static("Bearer sk-limited-key"),
                        )
                        .json(&json!({"model": "test-model", "messages": []}))
                        .await
                });

                // Give it a moment to start and acquire the permit
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;

                // Second request with same key should be rejected
                let response2 = server
                    .post("/v1/chat/completions")
                    .add_header(
                        axum::http::HeaderName::from_static("authorization"),
                        axum::http::HeaderValue::from_static("Bearer sk-limited-key"),
                    )
                    .json(&json!({"model": "test-model", "messages": []}))
                    .await;

                // Second request should be rejected (per-key concurrency limit exceeded)
                assert_eq!(response2.status_code(), 429);
                let body: serde_json::Value = response2.json();
                assert_eq!(body["error"]["code"], "concurrency_limit_exceeded");

                // Complete the first request
                mock_client.complete_request(0);
                let response1 = handle1.await.unwrap();
                assert_eq!(response1.status_code(), 200);

                // Only 1 request made it to the mock client
                assert_eq!(mock_client.get_requests().len(), 1);
            })
            .await;
    }

    mod metrics {
        use super::*;
        use axum_test::TestServer;
        use dashmap::DashMap;
        use rstest::*;
        use serde_json::json;
        use std::sync::Arc;

        /// Fixture to create a shared metrics server and main server.
        /// The axum-prometheus library using a global Prometheus registry that maintains state across test executions within the same process. Even
        /// with unique prefixes and serial execution, the library prevents creating multiple metric registries with overlapping metric names. So we
        /// use a shared metrics server for all metrics tests.
        #[fixture]
        #[once]
        fn get_shared_metrics_servers(
            #[default(Arc::new(DashMap::new()))] targets: Arc<DashMap<String, ProviderPool>>,
        ) -> (TestServer, TestServer) {
            let targets = Targets {
                targets,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
                key_labels: Arc::new(DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            };

            let (prometheus_layer, handle) = build_metrics_layer_and_handle("onwards");

            let metrics_router = build_metrics_router(handle);
            let metrics_server = TestServer::new(metrics_router).unwrap();

            let app_state = AppState::new(targets);
            let router = build_router(app_state).layer(prometheus_layer);
            let server = TestServer::new(router).unwrap();

            (server, metrics_server)
        }

        #[rstest]
        #[tokio::test]
        async fn test_metrics_server_for_v1_models(
            get_shared_metrics_servers: &(TestServer, TestServer),
        ) {
            let (server, metrics_server) = get_shared_metrics_servers;

            // Get current metrics count before making requests
            let initial_response = metrics_server.get("/metrics").await;
            let initial_metrics = initial_response.text();

            // Count existing v1/models requests (if any)
            let initial_count = initial_metrics
                .lines()
                .find(|line| line.contains("onwards_http_requests_total{method=\"GET\",status=\"200\",endpoint=\"/v1/models\"}"))
                .and_then(|line| line.split_whitespace().last())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);

            // Make a request
            let response = server.get("/v1/models").await;
            assert_eq!(response.status_code(), 200);

            // Check metrics increased by 1
            let response = metrics_server.get("/metrics").await;
            assert_eq!(response.status_code(), 200);
            let metrics_text = response.text();

            let new_count = metrics_text
                .lines()
                .find(|line| line.contains("onwards_http_requests_total{method=\"GET\",status=\"200\",endpoint=\"/v1/models\"}"))
                .and_then(|line| line.split_whitespace().last())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);

            assert_eq!(
                new_count,
                initial_count + 1,
                "Metrics should increment by 1"
            );

            // Make 10 more requests
            for _ in 0..10 {
                let response = server.get("/v1/models").await;
                assert_eq!(response.status_code(), 200);
            }

            // Check metrics increased by 11 total
            let response = metrics_server.get("/metrics").await;
            assert_eq!(response.status_code(), 200);
            let metrics_text = response.text();

            let final_count = metrics_text
                .lines()
                .find(|line| line.contains("onwards_http_requests_total{method=\"GET\",status=\"200\",endpoint=\"/v1/models\"}"))
                .and_then(|line| line.split_whitespace().last())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);

            assert_eq!(
                final_count,
                initial_count + 11,
                "Metrics should increment by 11 total"
            );
        }

        #[rstest]
        #[tokio::test]
        async fn test_metrics_server_for_missing_targets(
            get_shared_metrics_servers: &(TestServer, TestServer),
        ) {
            let (server, metrics_server) = get_shared_metrics_servers;

            // Get current metrics count before making requests
            let initial_response = metrics_server.get("/metrics").await;
            let initial_metrics = initial_response.text();

            // Count existing chat/completions 404 requests (if any)
            let initial_count = initial_metrics
                .lines()
                .find(|line| line.contains("onwards_http_requests_total{method=\"POST\",status=\"404\",endpoint=\"/v1/chat/completions\"}"))
                .and_then(|line| line.split_whitespace().last())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "claude-3",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;
            assert_eq!(response.status_code(), 404);

            let response = metrics_server.get("/metrics").await;

            assert_eq!(response.status_code(), 200);
            let metrics_text = response.text();

            let new_count = metrics_text
                .lines()
                .find(|line| line.contains("onwards_http_requests_total{method=\"POST\",status=\"404\",endpoint=\"/v1/chat/completions\"}"))
                .and_then(|line| line.split_whitespace().last())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);

            assert_eq!(
                new_count,
                initial_count + 1,
                "Metrics should increment by 1"
            );

            for _ in 0..10 {
                let response = server
                    .post("/v1/chat/completions")
                    .json(&json!({
                        "model": "claude-3",
                        "messages": [{"role": "user", "content": "Hello"}]
                    }))
                    .await;
                assert_eq!(response.status_code(), 404);
            }

            let response = metrics_server.get("/metrics").await;
            assert_eq!(response.status_code(), 200);
            let metrics_text = response.text();

            let final_count = metrics_text
                .lines()
                .find(|line| line.contains("onwards_http_requests_total{method=\"POST\",status=\"404\",endpoint=\"/v1/chat/completions\"}"))
                .and_then(|line| line.split_whitespace().last())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);

            assert_eq!(
                final_count,
                initial_count + 11,
                "Metrics should increment by 11 total"
            );
        }

        /// The per-model `onwards_model_inflight` gauge is created ONLY for a
        /// configured model. An unknown model from a request body must never
        /// register a series — that would be an unbounded-cardinality / DoS vector,
        /// made worse by our deliberate no-eviction policy. Here no target is
        /// configured, so the request 404s at the target lookup *before* `set_model`
        /// runs, and no `onwards_model_inflight{model=…}` series appears.
        #[rstest]
        #[tokio::test]
        async fn test_onwards_model_inflight_not_created_for_unknown_model(
            get_shared_metrics_servers: &(TestServer, TestServer),
        ) {
            let (server, metrics_server) = get_shared_metrics_servers;
            let model = "unconfigured-cardinality-probe";

            // Distinct endpoint path (still the wildcard proxy handler) so we don't
            // perturb the shared `/v1/chat/completions` 404 counter other tests read.
            let response = server
                .post("/v1/embeddings")
                .json(&json!({
                    "model": model,
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;
            // Unknown model -> 404 at the target lookup, before set_model.
            assert_eq!(response.status_code(), 404);

            let metrics_text = metrics_server.get("/metrics").await.text();
            assert!(
                !metrics_text.contains(&format!("onwards_model_inflight{{model=\"{model}\"}}")),
                "an unknown model must NOT create a per-model series:\n{metrics_text}"
            );
        }
    }

    #[tokio::test]
    async fn test_body_transformation_applied() {
        use serde_json::json;

        // Create a simple target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "test-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .build(),
            ),
        );

        let targets = target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        // Create a body transformation function that adds a "transformed": true field
        let transform_fn: BodyTransformFn = Arc::new(|_path, _headers, body_bytes| {
            if let Ok(mut json_body) = serde_json::from_slice::<serde_json::Value>(body_bytes)
                && let Some(obj) = json_body.as_object_mut()
            {
                obj.insert("transformed".to_string(), json!(true));
                if let Ok(transformed_bytes) = serde_json::to_vec(&json_body) {
                    return Some(axum::body::Bytes::from(transformed_bytes));
                }
            }
            None
        });

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state =
            AppState::with_client_and_transform(targets, mock_client.clone(), transform_fn);
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Make a request
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "test-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code(), 200);

        // Check that the request was transformed before forwarding
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let forwarded_body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(forwarded_body["transformed"], true);
        assert_eq!(forwarded_body["model"], "test-model");
        assert_eq!(forwarded_body["messages"][0]["content"], "Hello");
    }

    #[tokio::test]
    async fn test_body_transformation_not_applied_when_none() {
        use serde_json::json;

        // Create a simple target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "test-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .build(),
            ),
        );

        let targets = target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state = AppState::with_client(targets, mock_client.clone()); // No transform function
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Make a request
        let original_body = json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "Hello"}]
        });

        let response = server
            .post("/v1/chat/completions")
            .json(&original_body)
            .await;

        assert_eq!(response.status_code(), 200);

        // Check that the request was NOT transformed
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let forwarded_body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(forwarded_body.get("transformed").is_none());
        assert_eq!(forwarded_body["model"], "test-model");
        assert_eq!(forwarded_body["messages"][0]["content"], "Hello");
    }

    #[tokio::test]
    async fn test_body_transformation_returns_none() {
        use serde_json::json;

        // Create a simple target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "test-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .build(),
            ),
        );

        let targets = target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        // Create a transformation function that always returns None (no transformation)
        let transform_fn: BodyTransformFn = Arc::new(|_path, _headers, _body_bytes| None);

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state =
            AppState::with_client_and_transform(targets, mock_client.clone(), transform_fn);
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Make a request
        let original_body = json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "Hello"}]
        });

        let response = server
            .post("/v1/chat/completions")
            .json(&original_body)
            .await;

        assert_eq!(response.status_code(), 200);

        // Check that the request was NOT transformed since function returned None
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let forwarded_body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(forwarded_body, original_body);
    }

    #[tokio::test]
    async fn test_openai_streaming_include_usage_transformation() {
        use serde_json::json;

        // Create a target for OpenAI
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "gpt-4".to_string(),
            pool(
                Target::builder()
                    .url("https://api.openai.com".parse().unwrap())
                    .build(),
            ),
        );

        let targets = target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        // Create a transformation function that forces include_usage for streaming requests
        let transform_fn: BodyTransformFn = Arc::new(|path, _headers, body_bytes| {
            // Only transform requests to OpenAI chat completions endpoint
            if path == "/v1/chat/completions"
                && let Ok(mut json_body) = serde_json::from_slice::<serde_json::Value>(body_bytes)
                && let Some(obj) = json_body.as_object_mut()
            {
                // Check if this is a streaming request
                if let Some(stream) = obj.get("stream")
                    && stream.as_bool() == Some(true)
                {
                    // Force include_usage to true for streaming requests
                    obj.insert(
                        "stream_options".to_string(),
                        json!({
                            "include_usage": true
                        }),
                    );

                    if let Ok(transformed_bytes) = serde_json::to_vec(&json_body) {
                        return Some(axum::body::Bytes::from(transformed_bytes));
                    }
                }
            }
            None
        });

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state =
            AppState::with_client_and_transform(targets, mock_client.clone(), transform_fn);
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Test streaming request - should add include_usage
        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "gpt-4",
                "messages": [{"role": "user", "content": "Hello"}],
                "stream": true
            }))
            .await;

        assert_eq!(response.status_code(), 200);

        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let forwarded_body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(forwarded_body["model"], "gpt-4");
        assert_eq!(forwarded_body["stream"], true);
        assert_eq!(forwarded_body["stream_options"]["include_usage"], true);
    }

    #[tokio::test]
    async fn test_openai_non_streaming_not_transformed() {
        use serde_json::json;

        // Create a target for OpenAI
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "gpt-4".to_string(),
            pool(
                Target::builder()
                    .url("https://api.openai.com".parse().unwrap())
                    .build(),
            ),
        );

        let targets = target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        // Create the same transformation function
        let transform_fn: BodyTransformFn = Arc::new(|path, _headers, body_bytes| {
            if path == "/v1/chat/completions"
                && let Ok(mut json_body) = serde_json::from_slice::<serde_json::Value>(body_bytes)
                && let Some(obj) = json_body.as_object_mut()
                && let Some(stream) = obj.get("stream")
                && stream.as_bool() == Some(true)
            {
                obj.insert(
                    "stream_options".to_string(),
                    json!({
                        "include_usage": true
                    }),
                );

                if let Ok(transformed_bytes) = serde_json::to_vec(&json_body) {
                    return Some(axum::body::Bytes::from(transformed_bytes));
                }
            }
            None
        });

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state =
            AppState::with_client_and_transform(targets, mock_client.clone(), transform_fn);
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Test non-streaming request - should NOT be transformed
        let original_body = json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": false
        });

        let response = server
            .post("/v1/chat/completions")
            .json(&original_body)
            .await;

        assert_eq!(response.status_code(), 200);

        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);

        let forwarded_body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(forwarded_body, original_body);
        assert!(forwarded_body.get("stream_options").is_none());
    }

    #[tokio::test]
    async fn test_transformation_path_filtering() {
        use serde_json::json;

        // Create a target
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "test-model".to_string(),
            pool(
                Target::builder()
                    .url("https://api.example.com".parse().unwrap())
                    .build(),
            ),
        );

        let targets = target::Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };

        // Create a transformation function that only transforms specific paths
        let transform_fn: BodyTransformFn = Arc::new(|path, _headers, body_bytes| {
            if path == "/v1/chat/completions"
                && let Ok(mut json_body) = serde_json::from_slice::<serde_json::Value>(body_bytes)
                && let Some(obj) = json_body.as_object_mut()
            {
                obj.insert("path_transformed".to_string(), json!(path));
                if let Ok(transformed_bytes) = serde_json::to_vec(&json_body) {
                    return Some(axum::body::Bytes::from(transformed_bytes));
                }
            }
            None
        });

        let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
        let app_state =
            AppState::with_client_and_transform(targets, mock_client.clone(), transform_fn);
        let router = build_router(app_state);
        let server = TestServer::new(router).unwrap();

        // Test matching path - should be transformed
        let response1 = server
            .post("/v1/chat/completions")
            .json(&json!({"model": "test-model", "test": "data"}))
            .await;
        assert_eq!(response1.status_code(), 200);

        // Test non-matching path - should NOT be transformed
        let response2 = server
            .post("/v1/embeddings")
            .json(&json!({"model": "test-model", "test": "data"}))
            .await;
        assert_eq!(response2.status_code(), 200);

        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 2);

        // First request should be transformed
        let forwarded_body1: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(forwarded_body1["path_transformed"], "/v1/chat/completions");

        // Second request should NOT be transformed
        let forwarded_body2: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert!(forwarded_body2.get("path_transformed").is_none());
    }

    mod response_headers_pricing {
        use super::*;
        use std::collections::HashMap;
        use target::{Target, Targets};

        #[tokio::test]
        async fn test_pricing_added_to_response_headers_when_configured() {
            let targets_map = Arc::new(DashMap::new());
            let mut response_headers = HashMap::new();
            response_headers.insert("Input-Price-Per-Token".to_string(), "0.00003".to_string());
            response_headers.insert("Output-Price-Per-Token".to_string(), "0.00006".to_string());

            targets_map.insert(
                "gpt-4".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.openai.com".parse().unwrap())
                        .response_headers(response_headers)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
                key_labels: Arc::new(DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            };

            let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
            let app_state = AppState::with_client(targets, mock_client);
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "gpt-4",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);
            assert_eq!(response.header("Input-Price-Per-Token"), "0.00003");
            assert_eq!(response.header("Output-Price-Per-Token"), "0.00006");
        }

        #[tokio::test]
        async fn test_no_pricing_headers_when_not_configured() {
            let targets_map = Arc::new(DashMap::new());
            targets_map.insert(
                "free-model".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.example.com".parse().unwrap())
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
                key_labels: Arc::new(DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            };

            let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
            let app_state = AppState::with_client(targets, mock_client);
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "free-model",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);
            assert!(response.maybe_header("Input-Price-Per-Token").is_none());
            assert!(response.maybe_header("Output-Price-Per-Token").is_none());
        }

        #[tokio::test]
        async fn test_pricing_preserved_in_error_response_headers() {
            let targets_map = Arc::new(DashMap::new());
            let mut response_headers = HashMap::new();
            response_headers.insert("Input-Price-Per-Token".to_string(), "0.00001".to_string());
            response_headers.insert("Output-Price-Per-Token".to_string(), "0.00002".to_string());

            targets_map.insert(
                "error-model".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.example.com".parse().unwrap())
                        .response_headers(response_headers)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
                key_labels: Arc::new(DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            };

            let mock_client = MockHttpClient::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"error": "Server error"}"#,
            );
            let app_state = AppState::with_client(targets, mock_client);
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "error-model",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 500);
            assert_eq!(response.header("Input-Price-Per-Token"), "0.00001");
            assert_eq!(response.header("Output-Price-Per-Token"), "0.00002");
        }

        #[tokio::test]
        async fn test_pricing_headers_with_different_models() {
            let targets_map = Arc::new(DashMap::new());

            let mut expensive_headers = HashMap::new();
            expensive_headers.insert("Input-Price-Per-Token".to_string(), "0.0001".to_string());
            expensive_headers.insert("Output-Price-Per-Token".to_string(), "0.0002".to_string());

            targets_map.insert(
                "expensive-model".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.expensive.com".parse().unwrap())
                        .response_headers(expensive_headers)
                        .build(),
                ),
            );

            let mut cheap_headers = HashMap::new();
            cheap_headers.insert("Input-Price-Per-Token".to_string(), "0.000001".to_string());
            cheap_headers.insert("Output-Price-Per-Token".to_string(), "0.000002".to_string());

            targets_map.insert(
                "cheap-model".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.cheap.com".parse().unwrap())
                        .response_headers(cheap_headers)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
                key_labels: Arc::new(DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            };

            let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
            let app_state = AppState::with_client(targets, mock_client);
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            // Test expensive model
            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "expensive-model",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);
            assert_eq!(response.header("Input-Price-Per-Token"), "0.0001");
            assert_eq!(response.header("Output-Price-Per-Token"), "0.0002");

            // Test cheap model
            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "cheap-model",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);
            assert_eq!(response.header("Input-Price-Per-Token"), "0.000001");
            assert_eq!(response.header("Output-Price-Per-Token"), "0.000002");
        }

        #[tokio::test]
        async fn test_pricing_header_with_only_input_price() {
            let targets_map = Arc::new(DashMap::new());
            let mut response_headers = HashMap::new();
            response_headers.insert("Input-Price-Per-Token".to_string(), "0.00005".to_string());

            targets_map.insert(
                "input-only-model".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.example.com".parse().unwrap())
                        .response_headers(response_headers)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
                key_labels: Arc::new(DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            };

            let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
            let app_state = AppState::with_client(targets, mock_client);
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "input-only-model",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);
            assert_eq!(response.header("Input-Price-Per-Token"), "0.00005");
            assert!(response.maybe_header("Output-Price-Per-Token").is_none());
        }

        #[tokio::test]
        async fn test_pricing_header_with_only_output_price() {
            let targets_map = Arc::new(DashMap::new());
            let mut response_headers = HashMap::new();
            response_headers.insert("Output-Price-Per-Token".to_string(), "0.00008".to_string());

            targets_map.insert(
                "output-only-model".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.example.com".parse().unwrap())
                        .response_headers(response_headers)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
                key_labels: Arc::new(DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            };

            let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
            let app_state = AppState::with_client(targets, mock_client);
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "output-only-model",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);
            assert!(response.maybe_header("Input-Price-Per-Token").is_none());
            assert_eq!(response.header("Output-Price-Per-Token"), "0.00008");
        }
    }

    mod load_balancing {
        use super::*;
        use crate::load_balancer::{Provider, ProviderPool};

        #[tokio::test]
        async fn test_load_balancing_with_multiple_providers() {
            // Create a pool with two providers
            let providers = vec![
                Provider::new(
                    Target::builder()
                        .url("https://api.provider1.com".parse().unwrap())
                        .onwards_key("key1".to_string())
                        .build(),
                    1,
                ),
                Provider::new(
                    Target::builder()
                        .url("https://api.provider2.com".parse().unwrap())
                        .onwards_key("key2".to_string())
                        .build(),
                    1,
                ),
            ];
            let pool = ProviderPool::new(providers);

            let targets_map = Arc::new(DashMap::new());
            targets_map.insert("test-model".to_string(), pool);

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
                key_labels: Arc::new(DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            };

            let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
            let app_state = AppState::with_client(targets, mock_client.clone());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            // Make multiple requests - they should be routed to one of the providers
            for _ in 0..5 {
                let response = server
                    .post("/v1/chat/completions")
                    .json(&json!({
                        "model": "test-model",
                        "messages": [{"role": "user", "content": "Hello"}]
                    }))
                    .await;

                assert_eq!(response.status_code(), 200);
            }

            // Verify requests were made (at least one should go to each provider over multiple runs)
            let requests = mock_client.get_requests();
            assert_eq!(requests.len(), 5);
        }

        #[tokio::test]
        async fn test_load_balancing_with_weighted_providers() {
            // Create a pool with providers having different weights
            let providers = vec![
                Provider::new(
                    Target::builder()
                        .url("https://api.high-weight.com".parse().unwrap())
                        .onwards_key("key-high".to_string())
                        .build(),
                    3,
                ), // Higher weight = more traffic
                Provider::new(
                    Target::builder()
                        .url("https://api.low-weight.com".parse().unwrap())
                        .onwards_key("key-low".to_string())
                        .build(),
                    1,
                ),
            ];
            let pool = ProviderPool::new(providers);

            let targets_map = Arc::new(DashMap::new());
            targets_map.insert("weighted-model".to_string(), pool);

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
                key_labels: Arc::new(DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            };

            let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
            let app_state = AppState::with_client(targets, mock_client.clone());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            // Make enough requests that a 3:1 weighted random pool should
            // reliably favor the high-weight provider without making the test
            // expensive.
            for _ in 0..100 {
                let response = server
                    .post("/v1/chat/completions")
                    .json(&json!({
                        "model": "weighted-model",
                        "messages": [{"role": "user", "content": "Hello"}]
                    }))
                    .await;

                assert_eq!(response.status_code(), 200);
            }

            let requests = mock_client.get_requests();
            assert_eq!(requests.len(), 100);

            // Count requests to each provider (URI is the full path, check if it contains the host)
            let high_weight_count = requests
                .iter()
                .filter(|r| r.uri.contains("api.high-weight.com"))
                .count();
            let low_weight_count = requests
                .iter()
                .filter(|r| r.uri.contains("api.low-weight.com"))
                .count();

            // With weights 3:1, high-weight should get roughly 3x more traffic
            // Allow for statistical variance - high weight should have at least more requests
            assert!(
                high_weight_count > low_weight_count,
                "Expected high-weight provider ({}) to receive more requests than low-weight ({})",
                high_weight_count,
                low_weight_count
            );
        }

        #[tokio::test]
        async fn test_single_provider_pool_behaves_like_single_target() {
            // A pool with a single provider should work identically to the old behavior
            let pool = ProviderPool::single(
                Target::builder()
                    .url("https://api.single.com".parse().unwrap())
                    .onwards_key("single-key".to_string())
                    .build(),
                1,
            );

            let targets_map = Arc::new(DashMap::new());
            targets_map.insert("single-model".to_string(), pool);

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
                key_labels: Arc::new(DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            };

            let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
            let app_state = AppState::with_client(targets, mock_client.clone());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "single-model",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);

            let requests = mock_client.get_requests();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].uri.contains("api.single.com"));
        }
    }

    #[tokio::test]
    async fn test_provider_reasoning_translation_applies_to_upstream_body() {
        let reasoning_translation = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["minimal", "low", "medium", "xhigh", "max"],
                "writes": [{
                    "target_path": "/chat_template_kwargs/thinking",
                    "values": {"none": false, "high": true}
                }]
            }
        }))
        .unwrap();
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "kimi-k2.5".to_string(),
            pool(
                Target::builder()
                    .url("https://sglang.example.com".parse().unwrap())
                    .reasoning_translation(reasoning_translation)
                    .build(),
            ),
        );
        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };
        let mock_client = MockHttpClient::new(StatusCode::OK, "{}");
        let app_state = AppState::with_client(targets, mock_client.clone());
        let server = TestServer::new(build_router(app_state)).unwrap();

        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "kimi-k2.5",
                "messages": [{"role": "user", "content": "Hello"}],
                "reasoning_effort": "none"
            }))
            .await;

        assert_eq!(response.status_code(), StatusCode::OK);
        let requests = mock_client.get_requests();
        let upstream_body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(
            upstream_body.pointer("/chat_template_kwargs/thinking"),
            Some(&json!(false))
        );
        assert!(upstream_body.get("reasoning_effort").is_none());
    }

    #[tokio::test]
    async fn test_missing_output_limit_returns_422_before_upstream_request() {
        let reasoning_translation = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["none", "minimal", "low", "medium", "xhigh", "max"],
                "writes": [
                    {
                        "target_path": "/reasoning_effort",
                        "values": {"high": "high"}
                    },
                    {
                        "target_path": "/thinking_token_budget",
                        "values": {"high": 8192}
                    }
                ]
            }
        }))
        .unwrap();
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "reasoning-model".to_string(),
            pool(
                Target::builder()
                    .url("https://vllm.example.com".parse().unwrap())
                    .reasoning_translation(reasoning_translation)
                    .build(),
            ),
        );
        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };
        let mock_client = MockHttpClient::new(StatusCode::OK, "{}");
        let app_state = AppState::with_client(targets, mock_client.clone());
        let server = TestServer::new(build_router(app_state)).unwrap();

        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "reasoning-model",
                "messages": [],
                "reasoning_effort": "high"
            }))
            .await;

        assert_eq!(response.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
        let body: serde_json::Value = response.json();
        assert_eq!(body["error"]["param"], "max_completion_tokens");
        assert_eq!(
            body["error"]["code"],
            "reasoning_budget_requires_max_tokens"
        );
        assert!(mock_client.get_requests().is_empty());
    }

    #[tokio::test]
    async fn test_reasoning_budget_is_validated_for_every_pool_provider() {
        let budget_config = |budget| {
            serde_json::from_value(json!({
                "chat_completions": {
                    "unsupported_efforts": ["none", "minimal", "low", "medium", "xhigh", "max"],
                    "writes": [
                        {
                            "target_path": "/reasoning_effort",
                            "values": {"high": "high"}
                        },
                        {
                            "target_path": "/thinking_token_budget",
                            "values": {"high": budget}
                        }
                    ]
                }
            }))
            .unwrap()
        };
        let providers = vec![
            Provider::new(
                Target::builder()
                    .url("https://first.example.com".parse().unwrap())
                    .reasoning_translation(budget_config(4096))
                    .build(),
                1,
            ),
            Provider::new(
                Target::builder()
                    .url("https://fallback.example.com".parse().unwrap())
                    .reasoning_translation(budget_config(8192))
                    .build(),
                1,
            ),
        ];
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "reasoning-model".to_string(),
            ProviderPool::with_config(
                providers,
                None,
                None,
                None,
                None,
                target::LoadBalanceStrategy::Priority,
                false,
                Vec::new(),
            ),
        );
        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };
        let mock_client = MockHttpClient::new(StatusCode::OK, "{}");
        let app_state = AppState::with_client(targets, mock_client.clone());
        let server = TestServer::new(build_router(app_state)).unwrap();

        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "reasoning-model",
                "messages": [],
                "reasoning_effort": "high",
                "max_completion_tokens": 6000
            }))
            .await;

        assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = response.json();
        assert_eq!(body["error"]["code"], "reasoning_budget_exceeds_max_tokens");
        assert!(mock_client.get_requests().is_empty());
    }

    #[tokio::test]
    async fn test_non_reasoning_routes_allow_non_json_payloads() {
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "whisper-1".to_string(),
            pool(
                Target::builder()
                    .url("https://audio.example.com".parse().unwrap())
                    .build(),
            ),
        );
        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };
        let mock_client = MockHttpClient::new(StatusCode::OK, "{}");
        let app_state = AppState::with_client(targets, mock_client.clone());
        let server = TestServer::new(build_router(app_state)).unwrap();
        let payload = "--boundary\r\ncontent-disposition: form-data\r\n\r\naudio\r\n--boundary--";

        let response = server
            .post("/v1/audio/transcriptions")
            .add_header("model-override", "whisper-1")
            .add_header("content-type", "multipart/form-data; boundary=boundary")
            .text(payload)
            .await;

        assert_eq!(response.status_code(), StatusCode::OK);
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].body, payload.as_bytes());
    }

    #[tokio::test]
    async fn test_invalid_reasoning_effort_returns_parameter_specific_400() {
        let reasoning_translation = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["minimal", "low", "medium", "xhigh", "max"],
                "writes": [{
                    "target_path": "/chat_template_kwargs/thinking",
                    "values": {"none": false, "high": true}
                }]
            }
        }))
        .unwrap();
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "kimi-k2.5".to_string(),
            pool(
                Target::builder()
                    .url("https://sglang.example.com".parse().unwrap())
                    .reasoning_translation(reasoning_translation)
                    .build(),
            ),
        );
        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };
        let mock_client = MockHttpClient::new(StatusCode::OK, "{}");
        let app_state = AppState::with_client(targets, mock_client.clone());
        let server = TestServer::new(build_router(app_state)).unwrap();

        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "kimi-k2.5",
                "messages": [],
                "reasoning_effort": "ultra"
            }))
            .await;

        assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = response.json();
        assert_eq!(body["error"]["param"], "reasoning_effort");
        assert_eq!(body["error"]["code"], "invalid_value");
        assert!(mock_client.get_requests().is_empty());
    }

    #[tokio::test]
    async fn test_reasoning_effort_must_be_supported_by_every_pool_provider() {
        let supports_high = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["minimal", "low", "medium", "xhigh", "max"],
                "writes": [{
                    "target_path": "/chat_template_kwargs/thinking",
                    "values": {"none": false, "high": true}
                }]
            }
        }))
        .unwrap();
        let binary_disabled_only = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["minimal", "low", "medium", "high", "xhigh", "max"],
                "writes": [{
                    "target_path": "/thinking",
                    "values": {"none": {"type": "disabled"}}
                }]
            }
        }))
        .unwrap();
        let providers = vec![
            Provider::new(
                Target::builder()
                    .url("https://first.example.com".parse().unwrap())
                    .reasoning_translation(supports_high)
                    .build(),
                1,
            ),
            Provider::new(
                Target::builder()
                    .url("https://fallback.example.com".parse().unwrap())
                    .reasoning_translation(binary_disabled_only)
                    .build(),
                1,
            ),
        ];
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "reasoning-model".to_string(),
            ProviderPool::with_config(
                providers,
                None,
                None,
                None,
                None,
                target::LoadBalanceStrategy::Priority,
                false,
                Vec::new(),
            ),
        );
        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: false,
            http_pool_config: None,
        };
        let mock_client = MockHttpClient::new(StatusCode::OK, "{}");
        let app_state = AppState::with_client(targets, mock_client.clone());
        let server = TestServer::new(build_router(app_state)).unwrap();

        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "reasoning-model",
                "messages": [],
                "reasoning_effort": "high"
            }))
            .await;

        assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = response.json();
        assert_eq!(body["error"]["code"], "unsupported_value");
        assert!(mock_client.get_requests().is_empty());
    }

    #[tokio::test]
    async fn test_reasoning_translation_is_rebuilt_for_each_fallback_attempt() {
        let sglang = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["minimal", "low", "medium", "high", "xhigh", "max"],
                "writes": [{
                    "target_path": "/chat_template_kwargs/thinking",
                    "values": {"none": false}
                }]
            }
        }))
        .unwrap();
        let object_shape = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["minimal", "low", "medium", "high", "xhigh", "max"],
                "writes": [{
                    "target_path": "/thinking",
                    "values": {"none": {"type": "disabled"}}
                }]
            }
        }))
        .unwrap();
        let providers = vec![
            Provider::new(
                Target::builder()
                    .url("https://first.example.com".parse().unwrap())
                    .reasoning_translation(sglang)
                    .build(),
                1,
            ),
            Provider::new(
                Target::builder()
                    .url("https://fallback.example.com".parse().unwrap())
                    .reasoning_translation(object_shape)
                    .build(),
                1,
            ),
        ];
        let fallback = Some(target::FallbackConfig {
            enabled: true,
            on_status: vec![429],
            ..Default::default()
        });
        let targets_map = Arc::new(DashMap::new());
        targets_map.insert(
            "reasoning-model".to_string(),
            ProviderPool::with_config(
                providers,
                None,
                None,
                None,
                fallback,
                target::LoadBalanceStrategy::Priority,
                false,
                Vec::new(),
            ),
        );
        let targets = Targets {
            targets: targets_map,
            key_rate_limiters: Arc::new(DashMap::new()),
            key_concurrency_limiters: Arc::new(DashMap::new()),
            key_labels: Arc::new(DashMap::new()),
            strict_mode: true,
            http_pool_config: None,
        };
        let error_frame =
            "data: {\"error\":{\"code\":429,\"message\":\"Provider returned error\"}}\n\n"
                .to_string();
        let ok_chunk = "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"reasoning-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n".to_string();
        let done = "data: [DONE]\n\n".to_string();
        let mock_client = MockHttpClient::new_streaming_sequence(
            StatusCode::OK,
            vec![vec![error_frame], vec![ok_chunk, done]],
        );
        let app_state = AppState::with_client(targets, mock_client.clone());
        let server = TestServer::new(build_router(app_state)).unwrap();

        let response = server
            .post("/v1/chat/completions")
            .json(&json!({
                "model": "reasoning-model",
                "stream": true,
                "messages": [],
                "reasoning_effort": "none"
            }))
            .await;

        assert_eq!(response.status_code(), StatusCode::OK);
        assert!(response.text().contains("hi"));
        let requests = mock_client.get_requests();
        assert_eq!(requests.len(), 2);
        let first: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(
            first.pointer("/chat_template_kwargs/thinking"),
            Some(&json!(false))
        );
        assert_eq!(second.pointer("/thinking/type"), Some(&json!("disabled")));
        assert!(second.get("chat_template_kwargs").is_none());
        assert!(first.get("reasoning_effort").is_none());
        assert!(second.get("reasoning_effort").is_none());
    }

    mod response_sanitization {
        use super::*;

        #[tokio::test]
        async fn test_sanitize_non_streaming_removes_unknown_fields() {
            let targets_map = Arc::new(DashMap::new());
            targets_map.insert(
                "gpt-4".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.openai.com".parse().unwrap())
                        .onwards_key("sk-test".to_string())
                        .sanitize_response(true)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
                key_labels: Arc::new(DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            };

            // Response with provider-specific fields
            let mock_response = r#"{
                "id": "chatcmpl-123",
                "object": "chat.completion",
                "created": 1677652288,
                "model": "gpt-4",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Hello!"
                    },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 9,
                    "completion_tokens": 2,
                    "total_tokens": 11
                },
                "custom_provider_field": "should be removed",
                "another_unknown_field": 12345
            }"#;

            let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
            let app_state = AppState::with_client(targets, mock_client)
                .with_response_transform(create_openai_sanitizer());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "gpt-4",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);

            let body: serde_json::Value = response.json();
            // Verify standard fields are present
            assert!(body.get("id").is_some());
            assert!(body.get("choices").is_some());
            assert!(body.get("usage").is_some());

            // Verify unknown fields are removed
            assert!(body.get("custom_provider_field").is_none());
            assert!(body.get("another_unknown_field").is_none());
        }

        #[tokio::test]
        async fn test_sanitize_rewrites_model_field() {
            let targets_map = Arc::new(DashMap::new());
            targets_map.insert(
                "gpt-4".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.openai.com".parse().unwrap())
                        .onwards_key("sk-test".to_string())
                        .onwards_model("gpt-4-turbo-2024-04-09".to_string())
                        .sanitize_response(true)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
                key_labels: Arc::new(DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            };

            // Response from upstream has the turbo model
            let mock_response = r#"{
                "id": "chatcmpl-123",
                "object": "chat.completion",
                "created": 1677652288,
                "model": "gpt-4-turbo-2024-04-09",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Hello!"
                    },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 9,
                    "completion_tokens": 2,
                    "total_tokens": 11
                }
            }"#;

            let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
            let app_state = AppState::with_client(targets, mock_client)
                .with_response_transform(create_openai_sanitizer());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "gpt-4",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);

            let body: serde_json::Value = response.json();
            // Verify model field was rewritten to match client request
            assert_eq!(body["model"], "gpt-4");
        }

        #[tokio::test]
        async fn test_sanitize_streaming_removes_unknown_fields() {
            let targets_map = Arc::new(DashMap::new());
            targets_map.insert(
                "gpt-4".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.openai.com".parse().unwrap())
                        .onwards_key("sk-test".to_string())
                        .sanitize_response(true)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
                key_labels: Arc::new(DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            };

            let streaming_chunks = vec![
                r#"data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1677652288,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}],"custom_field":"remove_me"}

"#
                    .to_string(),
                "data: [DONE]\n\n".to_string(),
            ];

            let mock_client = MockHttpClient::new_streaming(StatusCode::OK, streaming_chunks);
            let app_state = AppState::with_client(targets, mock_client)
                .with_response_transform(create_openai_sanitizer());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "gpt-4",
                    "messages": [{"role": "user", "content": "Hello"}],
                    "stream": true
                }))
                .await;

            assert_eq!(response.status_code(), 200);

            let body = response.text();
            // Verify [DONE] marker is preserved
            assert!(body.contains("data: [DONE]"));
            // Verify custom field is removed
            assert!(!body.contains("custom_field"));
            assert!(!body.contains("remove_me"));
        }

        #[tokio::test]
        async fn test_sanitize_streaming_rewrites_model() {
            let targets_map = Arc::new(DashMap::new());
            targets_map.insert(
                "gpt-4".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.openai.com".parse().unwrap())
                        .onwards_key("sk-test".to_string())
                        .onwards_model("gpt-4-turbo".to_string())
                        .sanitize_response(true)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
                key_labels: Arc::new(DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            };

            let streaming_chunks = vec![
                r#"data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1677652288,"model":"gpt-4-turbo","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}

"#
                    .to_string(),
                "data: [DONE]\n\n".to_string(),
            ];

            let mock_client = MockHttpClient::new_streaming(StatusCode::OK, streaming_chunks);
            let app_state = AppState::with_client(targets, mock_client)
                .with_response_transform(create_openai_sanitizer());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "gpt-4",
                    "messages": [{"role": "user", "content": "Hello"}],
                    "stream": true
                }))
                .await;

            assert_eq!(response.status_code(), 200);

            let body = response.text();
            // Verify model was rewritten
            assert!(body.contains(r#""model":"gpt-4""#));
            assert!(!body.contains(r#""model":"gpt-4-turbo""#));
        }

        #[tokio::test]
        async fn test_sanitization_disabled_passes_through() {
            let targets_map = Arc::new(DashMap::new());
            targets_map.insert(
                "gpt-4".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.openai.com".parse().unwrap())
                        .onwards_key("sk-test".to_string())
                        .sanitize_response(false) // Explicitly disabled
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
                key_labels: Arc::new(DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            };

            // Response with provider-specific fields
            let mock_response = r#"{
                "id": "chatcmpl-123",
                "object": "chat.completion",
                "created": 1677652288,
                "model": "gpt-4",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Hello!"
                    },
                    "finish_reason": "stop"
                }],
                "custom_provider_field": "should be preserved"
            }"#;

            let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
            let app_state = AppState::with_client(targets, mock_client)
                .with_response_transform(create_openai_sanitizer());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "gpt-4",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);

            let body: serde_json::Value = response.json();
            // Verify custom field is preserved when sanitization is disabled
            assert!(body.get("custom_provider_field").is_some());
            assert_eq!(body["custom_provider_field"], "should be preserved");
        }

        #[tokio::test]
        async fn test_sanitization_only_applies_to_chat_completions() {
            let targets_map = Arc::new(DashMap::new());
            targets_map.insert(
                "gpt-4".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.openai.com".parse().unwrap())
                        .onwards_key("sk-test".to_string())
                        .sanitize_response(true)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
                key_labels: Arc::new(DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            };

            // Response from a different endpoint (e.g., embeddings)
            let mock_response = r#"{
                "object": "list",
                "data": [{"embedding": [0.1, 0.2]}],
                "model": "text-embedding-ada-002",
                "usage": {"prompt_tokens": 8, "total_tokens": 8},
                "custom_field": "preserved"
            }"#;

            let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
            let app_state = AppState::with_client(targets, mock_client)
                .with_response_transform(create_openai_sanitizer());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/embeddings")
                .json(&json!({
                    "model": "gpt-4",
                    "input": "Hello"
                }))
                .await;

            assert_eq!(response.status_code(), 200);

            let body: serde_json::Value = response.json();
            // Verify response is not sanitized for non-chat-completion endpoints
            assert!(body.get("custom_field").is_some());
        }

        #[tokio::test]
        async fn test_pool_level_sanitization_applies_to_all_providers() {
            use crate::load_balancer::ProviderPool;

            // Create a pool with sanitization enabled at pool level
            let provider1 = Target::builder()
                .url("https://api1.com".parse().unwrap())
                .onwards_key("key1".to_string())
                .build();

            let provider2 = Target::builder()
                .url("https://api2.com".parse().unwrap())
                .onwards_key("key2".to_string())
                .sanitize_response(false) // Provider overrides pool setting
                .build();

            let pool = ProviderPool::new(vec![
                crate::load_balancer::Provider::new(provider1, 1),
                crate::load_balancer::Provider::new(provider2, 1),
            ]);

            let targets_map = Arc::new(DashMap::new());
            targets_map.insert("test-model".to_string(), pool);

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
                key_labels: Arc::new(DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            };

            let mock_response = r#"{
                "id": "chatcmpl-123",
                "object": "chat.completion",
                "created": 1677652288,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "Hello!"},
                    "finish_reason": "stop"
                }],
                "custom_field": "value"
            }"#;

            let mock_client = MockHttpClient::new(StatusCode::OK, mock_response);
            let app_state = AppState::with_client(targets, mock_client)
                .with_response_transform(create_openai_sanitizer());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "test-model",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 200);
        }

        #[tokio::test]
        async fn test_error_sanitization_replaces_4xx_body() {
            let targets_map = Arc::new(DashMap::new());
            targets_map.insert(
                "gpt-4".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.openai.com".parse().unwrap())
                        .onwards_key("sk-test".to_string())
                        .sanitize_response(true)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
                key_labels: Arc::new(DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            };

            // Upstream returns 422 with provider-specific error details
            let upstream_error = r#"{
                "error": {
                    "message": "Internal provider Novita returned error: model not loaded on GPU cluster eu-west-3",
                    "type": "provider_error",
                    "code": "novita_internal_error",
                    "metadata": {"provider": "novita", "region": "eu-west-3"}
                }
            }"#;

            let mock_client = MockHttpClient::new(StatusCode::UNPROCESSABLE_ENTITY, upstream_error);
            let app_state = AppState::with_client(targets, mock_client)
                .with_response_transform(create_openai_sanitizer());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "gpt-4",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            // Status code should be preserved
            assert_eq!(response.status_code(), 422);

            let body: serde_json::Value = response.json();
            // Should have OpenAI error envelope with generic message
            assert_eq!(
                body["error"]["message"],
                "The upstream provider rejected the request."
            );
            assert_eq!(body["error"]["type"], "invalid_request_error");
            assert_eq!(body["error"]["code"], "upstream_error");
            // Should NOT contain any upstream provider details
            let body_str = serde_json::to_string(&body).unwrap();
            assert!(!body_str.contains("Novita"));
            assert!(!body_str.contains("novita"));
            assert!(!body_str.contains("eu-west-3"));
        }

        #[tokio::test]
        async fn test_error_sanitization_preserves_5xx_status() {
            let targets_map = Arc::new(DashMap::new());
            targets_map.insert(
                "gpt-4".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.openai.com".parse().unwrap())
                        .onwards_key("sk-test".to_string())
                        .sanitize_response(true)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
                key_labels: Arc::new(DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            };

            // Upstream returns 503 with internal details
            let upstream_error = r#"{"error": "GPU cluster overloaded", "retry_after": 30}"#;

            let mock_client = MockHttpClient::new(StatusCode::SERVICE_UNAVAILABLE, upstream_error);
            let app_state = AppState::with_client(targets, mock_client)
                .with_response_transform(create_openai_sanitizer());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "gpt-4",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            // Original 503 status should be preserved
            assert_eq!(response.status_code(), 503);

            let body: serde_json::Value = response.json();
            // Should have generic error body, not upstream details
            assert_eq!(
                body["error"]["message"],
                "An internal error occurred. Please try again later."
            );
            assert_eq!(body["error"]["type"], "internal_error");
            assert_eq!(body["error"]["code"], "internal_error");
            // Should NOT contain upstream details
            let body_str = serde_json::to_string(&body).unwrap();
            assert!(!body_str.contains("GPU cluster"));
            assert!(!body_str.contains("retry_after"));
        }

        #[tokio::test]
        async fn test_error_sanitization_disabled_passes_through() {
            let targets_map = Arc::new(DashMap::new());
            targets_map.insert(
                "gpt-4".to_string(),
                pool(
                    Target::builder()
                        .url("https://api.openai.com".parse().unwrap())
                        .onwards_key("sk-test".to_string())
                        .sanitize_response(false)
                        .build(),
                ),
            );

            let targets = Targets {
                targets: targets_map,
                key_rate_limiters: Arc::new(DashMap::new()),
                key_concurrency_limiters: Arc::new(DashMap::new()),
                key_labels: Arc::new(DashMap::new()),
                strict_mode: false,
                http_pool_config: None,
            };

            // Upstream returns 422 with provider-specific details
            let upstream_error = r#"{"error": {"message": "provider-specific detail"}}"#;

            let mock_client = MockHttpClient::new(StatusCode::UNPROCESSABLE_ENTITY, upstream_error);
            let app_state = AppState::with_client(targets, mock_client)
                .with_response_transform(create_openai_sanitizer());
            let router = build_router(app_state);
            let server = TestServer::new(router).unwrap();

            let response = server
                .post("/v1/chat/completions")
                .json(&json!({
                    "model": "gpt-4",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
                .await;

            assert_eq!(response.status_code(), 422);

            // When sanitization is disabled, upstream error body should pass through
            let body: serde_json::Value = response.json();
            assert_eq!(body["error"]["message"], "provider-specific detail");
        }
    }
}
