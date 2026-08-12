//! Request and response serialization for AI proxy analytics.
//!
//! This module provides the serialization layer between the [outlet] request logging
//! middleware and the analytics database. It parses incoming AI requests, extracts
//! usage metrics from responses, records analytics data, and handles credit deduction.
//!
//! # Request Path
//!
//! When [outlet] intercepts an incoming request, it calls [`parse_ai_request`] to parse
//! the JSON body into an [`AiRequest`] variant (ChatCompletions, Completions, Embeddings,
//! or Other). This happens synchronously before the request is forwarded upstream.
//!
//! # Response Path
//!
//! After the upstream response completes, [outlet] calls the response serializer.
//! This is split into two phases:
//!
//! **Inline** (in the serializer closure):
//! 1. Parse response body via [`parse_ai_response`] (handles JSON, SSE streams, compression)
//! 2. Extract [`UsageMetrics`] (tokens, model, duration)
//! 3. Extract auth info from headers
//! 4. Return parsed [`AiResponse`] to outlet
//!
//! **Fire-and-forget** (spawned via `tokio::spawn`):
//! 1. Lookup API key → user_id, email
//! 2. Lookup model tariffs → price per token
//! 3. Write [`HttpAnalyticsRow`] to `http_analytics` table
//! 4. Deduct credits (if 2xx status and pricing configured)
//! 5. Record Prometheus metrics
//!
//! The spawned task runs independently - outlet doesn't wait for it.
//!
//! # Credit Deduction
//!
//! Credits are deducted based on token usage and model-specific pricing. The serializer
//! looks up the model's tariffs (input/output price per token) and creates a credit
//! transaction for each successful request. Failed requests (non-2xx status codes) do
//! not incur charges.
//!
//! [outlet]: https://github.com/doublewordai/outlet

use crate::config::Config;
use crate::request_logging::models::{AiRequest, AiResponse, ChatCompletionChunk, CompletionChunk, ParsedAIRequest, ResponsesRequest};
use outlet::{RequestData, ResponseData};
use outlet_postgres::SerializationError;
use serde_json::Value;
use std::fmt;
use std::str;
use tracing::{error, instrument};
use uuid::Uuid;

use super::utils;

/// Authentication information extracted from request headers
#[derive(Clone)]
pub enum Auth {
    /// API key access (Authorization: Bearer <key>)
    ApiKey { bearer_token: String },
    /// No authentication found
    None,
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Auth::ApiKey { .. } => f.debug_struct("ApiKey").field("bearer_token", &"<redacted>").finish(),
            Auth::None => write!(f, "None"),
        }
    }
}

/// Complete row structure for http_analytics table.
///
/// This struct mirrors the `http_analytics` database schema. Some fields are used by
/// `MetricsRecorder::record_from_analytics()` for Prometheus metrics, while others
/// exist to maintain parity with the database schema (populated but not read in Rust).
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields maintain schema parity; only subset read by MetricsRecorder
pub struct HttpAnalyticsRow {
    pub instance_id: Uuid,
    pub correlation_id: i64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub method: String,
    pub uri: String,
    pub request_model: Option<String>,
    pub response_model: Option<String>,
    pub status_code: i32,
    pub duration_ms: i64,
    pub duration_to_first_byte_ms: Option<i64>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub reasoning_tokens: i64,
    pub total_tokens: i64,
    pub response_type: String,
    pub user_id: Option<Uuid>,
    pub access_source: String,
    pub input_price_per_token: Option<rust_decimal::Decimal>,
    pub output_price_per_token: Option<rust_decimal::Decimal>,
    pub server_address: String,
    pub server_port: u16,
    pub provider_name: Option<String>,
    pub fusillade_batch_id: Option<Uuid>,
    pub fusillade_request_id: Option<Uuid>,
    pub custom_id: Option<String>,
    /// Request origin: "api", "frontend", or "fusillade"
    pub request_origin: String,
    /// Batch completion window (priority): "1h", "24h", etc.
    ///
    /// This is recorded as an empty string (`""`) for non-batch requests rather than
    /// using `None`/`NULL`. The empty-string sentinel is intentional so that
    /// Prometheus metrics can be filtered with a simple `batch_sla=""` label
    /// selector, at the cost of a small increase in label cardinality.
    pub batch_sla: String,
    /// The request_source from batch metadata (e.g., "api", "frontend").
    /// Empty string for non-batch requests or when not provided.
    pub batch_request_source: String,
    /// URL of the upstream that served the request (onwards `ServedBy`
    /// extension), for per-component attribution of composite models.
    pub served_by: Option<String>,
}

/// Usage metrics extracted from AI responses (subset of HttpAnalyticsRow)
#[derive(Debug, Clone)]
pub struct UsageMetrics {
    pub instance_id: Uuid,
    pub correlation_id: i64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub method: String,
    pub uri: String,
    pub request_model: Option<String>,
    pub response_model: Option<String>,
    pub status_code: i32,
    pub duration_ms: i64,
    pub duration_to_first_byte_ms: Option<i64>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub reasoning_tokens: i64,
    pub total_tokens: i64,
    // Cached-input split, read from the response `usage` the dwctl cache layer
    // injected. Zero on non-cache requests / models. `prompt_tokens` stays the full input
    // count; it typically equals cache_read + cache_creation + uncached, but the split need
    // not reconcile exactly (our tokenizer and the provider's can disagree — billing floors
    // uncached at 0 to absorb the drift).
    pub cache_read_input_tokens: i64,
    pub cache_creation_5m_input_tokens: i64,
    pub cache_creation_1h_input_tokens: i64,
    pub cache_creation_24h_input_tokens: i64,
    pub response_type: String,
    /// Why the model stopped — `stop`, `length`, `tool_calls`, ... `None` when the
    /// response shape has no such concept (embeddings, the Responses API) or couldn't be
    /// read. See `extract_finish_reason`.
    pub finish_reason: Option<String>,
    pub server_address: String,
    pub server_port: u16,
    /// URL of the upstream that actually served the request, read from the
    /// onwards `ServedBy` response extension. For composite models this is the
    /// selected component's endpoint (after any fallback), which is the only
    /// place per-request routing attribution is knowable. `None` when the
    /// request never reached an upstream (or predates the extension).
    pub served_by: Option<String>,
}

/// Parses HTTP request body data into structured AI request types.
///
/// # Arguments
/// * `request_data` - The HTTP request data containing body and metadata
///
/// # Returns
/// * `Ok(ParsedAIRequest)` - Successfully parsed request as chat completion, completion,
///   embeddings, responses, or other
/// * `Err(SerializationError)` - Parse error with base64-encoded fallback data for storage
///
/// # Behavior
/// - Returns `AiRequest::Other(Value::Null)` for missing or empty bodies
/// - On parse failure, returns error with base64-encoded body for safe PostgreSQL storage
/// - For `/v1/responses` paths, uses path-based detection to avoid serde disambiguation
///   issues with the embeddings variant (both use an `input` field).
#[instrument(skip_all, name = "dwctl.parse_ai_request")]
pub fn parse_ai_request(request_data: &RequestData) -> Result<ParsedAIRequest, SerializationError> {
    let headers = request_data
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.iter().map(|b| String::from_utf8_lossy(b).to_string()).collect()))
        .collect();

    let bytes = match &request_data.body {
        Some(body) => body.as_ref(),
        None => {
            return Ok(ParsedAIRequest {
                headers,
                request: AiRequest::Other(Value::Null),
                responses_request: None,
            });
        }
    };

    let body_str = String::from_utf8_lossy(bytes);

    if body_str.trim().is_empty() {
        return Ok(ParsedAIRequest {
            headers,
            request: AiRequest::Other(Value::Null),
            responses_request: None,
        });
    }

    // Use path-based detection for /v1/responses to avoid serde disambiguation issues
    // (both embeddings and responses requests have an `input` field).
    let is_responses_path = request_data.uri.path().ends_with("/responses");
    if is_responses_path {
        return match serde_json::from_str::<Value>(&body_str) {
            Ok(value) => {
                let model = value.get("model").and_then(|v| v.as_str()).map(|s| s.to_string());
                let stream = value.get("stream").and_then(|v| v.as_bool());
                Ok(ParsedAIRequest {
                    headers,
                    request: AiRequest::Other(value),
                    responses_request: Some(ResponsesRequest { model, stream }),
                })
            }
            Err(e) => {
                let base64_encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
                Err(SerializationError {
                    fallback_data: format!("base64:{base64_encoded}"),
                    error: Box::new(e),
                })
            }
        };
    }

    match serde_json::from_str(&body_str) {
        Ok(request) => Ok(ParsedAIRequest {
            headers,
            request,
            responses_request: None,
        }),
        Err(e) => {
            // Always base64 encode unparseable content to avoid PostgreSQL issues
            let base64_encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
            Err(SerializationError {
                fallback_data: format!("base64:{base64_encoded}"),
                error: Box::new(e),
            })
        }
    }
}

/// Parses HTTP response body data into structured AI response types.
///
/// # Arguments
/// * `request_data` - The original HTTP request data (used to determine response parsing strategy)
/// * `response_data` - The HTTP response data containing body, headers, and metadata
///
/// # Returns
/// * `Ok(AiResponse)` - Successfully parsed response as chat completion, completion, embeddings, or other
/// * `Err(SerializationError)` - Parse error with base64-encoded fallback data for storage
///
/// # Behavior
/// - Returns `AiResponse::Other(Value::Null)` for missing or empty response bodies
/// - Handles gzip/brotli decompression based on Content-Encoding headers
/// - Parses streaming responses (SSE format) vs non-streaming based on request stream parameter
/// - On parse failure, returns error with base64-encoded decompressed body
#[instrument(skip_all, name = "dwctl.parse_ai_response")]
pub fn parse_ai_response(request_data: &RequestData, response_data: &ResponseData) -> Result<AiResponse, SerializationError> {
    let bytes = match &response_data.body {
        Some(body) => body.as_ref(),
        None => return Ok(AiResponse::Other(Value::Null)),
    };

    if bytes.is_empty() {
        return Ok(AiResponse::Other(Value::Null));
    }

    // Decompress if needed
    let final_bytes = utils::decompress_response_if_needed(bytes, &response_data.headers)?;
    let body_str = String::from_utf8_lossy(&final_bytes);
    if body_str.trim().is_empty() {
        return Ok(AiResponse::Other(Value::Null));
    }

    // Onwards injects stream:true into the forwarded body when it sees this header,
    // but outlet captures the original request body (without stream:true). Check the
    // header so we know to use the streaming parser for the response.
    let fusillade_stream = request_data
        .headers
        .get("x-fusillade-stream")
        .and_then(|values| values.first())
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        == Some("true");

    // /v1/messages (Anthropic) has its own SSE event lifecycle and a distinct
    // blocking shape, both produced by dwctl's edge translator. Detect it by path
    // (like /responses) - stream is signalled by the request body's `stream` flag
    // or the fusillade header.
    let result = if request_data.uri.path().ends_with("/messages") {
        let anthropic_stream = request_data
            .body
            .as_ref()
            .and_then(|b| serde_json::from_slice::<Value>(b).ok())
            .and_then(|v| v.get("stream").and_then(Value::as_bool))
            .unwrap_or(false)
            || fusillade_stream;
        if anthropic_stream {
            utils::parse_anthropic_streaming_response(&body_str)
        } else {
            // Typed MessagesResponse first; fall back to the generic untagged parser so
            // error bodies (4xx/5xx JSON) are captured as AiResponse::Other rather than
            // a base64 SerializationError.
            utils::parse_anthropic_non_streaming_response(&body_str).or_else(|_| utils::parse_non_streaming_response(&body_str))
        }
    } else {
        // Parse response based on request type
        match parse_ai_request(request_data) {
            Ok(parsed_request) => {
                // /v1/responses has its own SSE event format distinct from chat completions.
                if let Some(responses_req) = &parsed_request.responses_request {
                    if responses_req.stream.unwrap_or(false) || fusillade_stream {
                        utils::parse_responses_streaming_response(&body_str)
                    } else {
                        // Try the typed Response parser first. Fall back to the generic untagged
                        // parser so that error bodies (4xx/5xx JSON) are captured as
                        // AiResponse::Other rather than becoming a base64 SerializationError.
                        utils::parse_responses_non_streaming_response(&body_str).or_else(|_| utils::parse_non_streaming_response(&body_str))
                    }
                } else {
                    match parsed_request.request {
                        AiRequest::ChatCompletions(chat_req) if chat_req.stream.unwrap_or(false) || fusillade_stream => {
                            utils::parse_streaming_response(&body_str)
                        }
                        AiRequest::Completions(completion_req) if completion_req.stream.unwrap_or(false) || fusillade_stream => {
                            utils::parse_completions_streaming_response(&body_str)
                        }
                        _ => utils::parse_non_streaming_response(&body_str),
                    }
                }
            }
            _ => utils::parse_non_streaming_response(&body_str),
        }
    };

    result.map_err(|_| SerializationError {
        fallback_data: format!(
            "base64:{}",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &final_bytes)
        ),
        error: "Failed to parse response as JSON or SSE".into(),
    })
}

impl UsageMetrics {
    /// Extracts usage metrics from request and response data.
    ///
    /// # Arguments
    /// * `instance_id` - Unique identifier for the service instance
    /// * `request_data` - HTTP request data containing method, URI, timestamp, and correlation ID
    /// * `response_data` - HTTP response data containing status code and duration
    /// * `parsed_response` - The parsed AI response for token usage extraction
    /// * `config` - Configuration containing server address and port
    ///
    /// # Returns
    /// A `UsageMetrics` struct with extracted model, tokens, and timing data
    #[instrument(skip_all, name = "dwctl.extract_usage_metrics")]
    pub fn extract(
        instance_id: Uuid,
        request_data: &RequestData,
        response_data: &ResponseData,
        parsed_response: &AiResponse,
        config: &Config,
    ) -> Self {
        // Extract model from request.
        // First try typed deserialization (ChatCompletions, Completions, Embeddings).
        // If that fails (e.g. request uses content types async_openai doesn't know about,
        // like Responses API's input_text/input_image), fall back to extracting the
        // "model" field from raw JSON.
        let request_model = match parse_ai_request(request_data) {
            Ok(parsed_request) => {
                if let Some(responses_req) = parsed_request.responses_request {
                    responses_req.model
                } else {
                    match parsed_request.request {
                        AiRequest::ChatCompletions(req) => Some(req.model),
                        AiRequest::Completions(req) => Some(req.model),
                        AiRequest::Embeddings(req) => Some(req.model),
                        AiRequest::Other(ref value) => {
                            let model = value.get("model").and_then(|v| v.as_str()).map(String::from);
                            if model.is_some() {
                                error!(
                                    uri = %request_data.uri,
                                    "Request body has a model field but failed typed deserialization — \
                                     likely uses unsupported content types"
                                );
                            }
                            model
                        }
                    }
                }
            }
            _ => None,
        };

        // Token metrics come from the single parse of the response into `AiResponse`
        // (the same value request logging stores), normalised to one currency here.
        let metrics = TokenMetrics::from(parsed_response);

        // The cache split lives in extension fields the typed parse drops, so read it from
        // the raw `usage` object. It only exists on a successful response that carried a
        // usage frame, so an errored/partial stream naturally extracts zero (no cache bill).
        let cache_tokens = extract_cache_tokens(response_data);

        // Streams that started with HTTP 200 but ended with an embedded provider error frame
        // get reclassified to 500 so success-rate / availability metrics, the credits-eligibility
        // check, and dashboards keyed on `status_code BETWEEN 200 AND 299` exclude them.
        // 500 matches what fusillade reclassifies these to in its HTTP layer, so the two views
        // (analytics row, fusillade request state) agree on a number for the same logical event.
        let upstream_status = response_data.status.as_u16() as i32;
        let status_code = if upstream_status < 400 && ai_response_stream_errored(parsed_response) {
            500
        } else {
            upstream_status
        };

        Self {
            instance_id,
            correlation_id: request_data.correlation_id as i64,
            timestamp: chrono::DateTime::<chrono::Utc>::from(request_data.timestamp),
            method: request_data.method.to_string(),
            uri: request_data.uri.to_string(),
            request_model,
            response_model: metrics.response_model,
            status_code,
            duration_ms: response_data.duration.as_millis() as i64,
            duration_to_first_byte_ms: Some(response_data.duration_to_first_byte.as_millis() as i64),
            prompt_tokens: metrics.prompt_tokens,
            completion_tokens: metrics.completion_tokens,
            reasoning_tokens: metrics.reasoning_tokens,
            total_tokens: metrics.total_tokens,
            cache_read_input_tokens: cache_tokens.read,
            cache_creation_5m_input_tokens: cache_tokens.creation_5m,
            cache_creation_1h_input_tokens: cache_tokens.creation_1h,
            cache_creation_24h_input_tokens: cache_tokens.creation_24h,
            response_type: metrics.response_type,
            finish_reason: extract_finish_reason(parsed_response),
            server_address: config.host.clone(),
            server_port: config.port,
            served_by: response_data.extensions.get::<onwards::ServedBy>().map(|s| s.url.clone()),
        }
    }
}

/// Whether a streamed response opened 2xx but ended with an embedded error frame.
/// [`UsageMetrics::extract`] reclassifies these to 500, matching how fusillade's
/// HTTP layer scores the same event. Blocking responses are never stream-errored.
fn ai_response_stream_errored(response: &AiResponse) -> bool {
    match response {
        AiResponse::ChatCompletionsStream(chunks) => chunks.iter().any(|c| matches!(c, ChatCompletionChunk::Error(_))),
        AiResponse::CompletionsStream(chunks) => chunks.iter().any(|c| matches!(c, CompletionChunk::Error(_))),
        AiResponse::AnthropicStream(events) => events.iter().any(|e| e.get("type").and_then(|v| v.as_str()) == Some("error")),
        AiResponse::ResponsesStream(events) => events.iter().any(|e| matches!(e.event_type.as_str(), "response.failed" | "error")),
        _ => false,
    }
}

/// The cache token split read from a response `usage` object.
#[derive(Debug, Clone, Copy, Default)]
struct CacheTokens {
    read: i64,
    creation_5m: i64,
    creation_1h: i64,
    creation_24h: i64,
}

/// Pull the cache split out of a single `usage` JSON object. Reads come **only** from
/// `cache_read_input_tokens` — the field the dwctl cache layer injects whenever it applies
/// caching (it sets `prompt_tokens_details.cached_tokens` too, but that's for client
/// visibility). We deliberately do *not* fall back to `prompt_tokens_details.cached_tokens`:
/// providers (e.g. OpenAI) report their *own* server-side cached_tokens there, and reading
/// it would attribute a provider's caching to dwctl's billing for a model dwctl isn't
/// caching — breaking the "non-cache models produce a zero cache split" invariant. Creation
/// is read per tier from the `cache_creation` object.
///
/// NOTE: these are also Anthropic's native usage field names, so a model dwctl isn't caching
/// that's routed to an Anthropic-native provider could carry the provider's *own* cache
/// tokens here. That does NOT affect billing: the batcher gates the cache discount on dwctl
/// enablement (a tariff valid at inference time), so provider-side tokens on a non-enabled
/// model are billed at list price (see `charged_cost`). What's read here only populates the
/// analytics columns — so the residual is cosmetic (provider cache tokens shown for a model
/// dwctl isn't caching), not a billing leak.
fn cache_tokens_from_usage(usage: &Value) -> CacheTokens {
    // Floor at 0: token counts can't be negative, but a malformed response could carry one —
    // never let it reach the analytics columns or the cost math (the batcher floors too).
    let read = usage.get("cache_read_input_tokens").and_then(Value::as_i64).unwrap_or(0).max(0);
    let tier = |k: &str| {
        usage
            .get("cache_creation")
            .and_then(|c| c.get(k))
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .max(0)
    };
    CacheTokens {
        read,
        creation_5m: tier("ephemeral_5m_input_tokens"),
        creation_1h: tier("ephemeral_1h_input_tokens"),
        creation_24h: tier("ephemeral_24h_input_tokens"),
    }
}

/// Extract the cache split from a response body, handling both shapes: a non-streaming
/// JSON body carries `usage` at the top level; a streaming SSE body carries it in the
/// terminal `data:` frame (take the last one seen). Returns all-zero when there is no
/// usage object (non-cache request, error body, or a stream that died before its usage
/// frame) — which is exactly the no-cache-billing case.
fn extract_cache_tokens(response_data: &ResponseData) -> CacheTokens {
    let Some(body) = &response_data.body else {
        return CacheTokens::default();
    };
    // On a decompress failure (e.g. a mis-set Content-Encoding on an actually-plain body),
    // fall back to the raw bytes rather than silently returning zero cache tokens — zeroing
    // would drop the read discount and overcharge a cache-enabled request. Log so the
    // mis-encoding is diagnosable.
    let bytes = match utils::decompress_response_if_needed(body.as_ref(), &response_data.headers) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "cache token extraction: response decompress failed, falling back to raw body");
            body.as_ref().to_vec()
        }
    };
    let body_str = String::from_utf8_lossy(&bytes);

    // Non-streaming: the whole body is one JSON object with a top-level `usage`.
    if let Ok(value) = serde_json::from_str::<Value>(body_str.trim())
        && let Some(usage) = value.get("usage").filter(|u| u.is_object())
    {
        return cache_tokens_from_usage(usage);
    }

    // Streaming: scan SSE frames, keeping the last one that carries a usage object.
    // SSE allows `data:<value>` and `data: <value>` — strip the colon then an optional space.
    let mut last = CacheTokens::default();
    for line in body_str.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.strip_prefix(' ').unwrap_or(data);
            let trimmed = data.trim();
            if trimmed != "[DONE]"
                && let Ok(value) = serde_json::from_str::<Value>(trimmed)
                && let Some(usage) = value.get("usage").filter(|u| u.is_object())
            {
                last = cache_tokens_from_usage(usage);
            }
        }
    }
    last
}

impl Auth {
    /// Extract authentication from request headers
    #[instrument(skip_all, name = "dwctl.extract_auth")]
    pub fn from_request(request_data: &RequestData, _config: &Config) -> Self {
        // Check for API key in Authorization header
        if let Some(auth_header) = Self::get_header_value(request_data, "authorization")
            && let Some(bearer_token) = auth_header.strip_prefix("Bearer ")
        {
            return Auth::ApiKey {
                bearer_token: bearer_token.to_string(),
            };
        }

        Auth::None
    }

    /// Extract header value as string
    fn get_header_value(request_data: &RequestData, header_name: &str) -> Option<String> {
        request_data
            .headers
            .get(header_name)
            .and_then(|values| values.first())
            .and_then(|bytes| str::from_utf8(bytes).ok())
            .map(|s| s.to_string())
    }
}

/// Why the model stopped: `stop`, `length`, `tool_calls`, `content_filter`, ...
///
/// `tool_calls` is the only signal we have that the caller is running a CLIENT-side tool
/// loop. The client executes the tool itself and sends a fresh request, so the follow-up
/// arrives with its own `fusillade_request_id` and is otherwise indistinguishable from an
/// ordinary multi-turn message. Without this the whole class of usage is invisible.
/// (Server-side tool loops are a different thing entirely, counted by `tool_iterations`
/// and detailed in `tool_call_analytics`.)
///
/// Deliberately a free function over the already-deserialised `AiResponse` rather than a
/// field on `TokenMetrics`: nothing new is parsed, and none of the nine `TokenMetrics`
/// arms have to change.
///
/// **Scanned independently of usage, on purpose.** It is tempting to read this off the
/// same chunk `TokenMetrics` already found (the last one carrying `usage`), and on the
/// local stack that happens to work — the terminal chunk carries `usage` AND a populated
/// `choices[0].finish_reason`. It is not safe in general: OpenAI's own convention is a
/// final usage chunk with `choices: []`, and self-hosted GLM-5.2 is on record returning
/// `usage: null` on precisely the `tool_calls` finishes we care about. Either shape would
/// silently yield NULL for the one case this column exists to catch, so the scan is its
/// own reverse pass for the last non-null `finish_reason`.
fn extract_finish_reason(response: &AiResponse) -> Option<String> {
    /// The enums derive `Serialize` with the wire spelling, so this stays in step with
    /// the API rather than hard-coding a match that a new variant would silently skip.
    fn as_wire<T: serde::Serialize>(v: &T) -> Option<String> {
        match serde_json::to_value(v).ok()? {
            serde_json::Value::String(s) => Some(s),
            _ => None,
        }
    }
    match response {
        AiResponse::ChatCompletions(r) => r.choices.first()?.finish_reason.as_ref().and_then(as_wire),
        AiResponse::ChatCompletionsStream(chunks) => chunks.iter().rev().find_map(|c| match c {
            ChatCompletionChunk::Normal(n) => n.choices.first()?.finish_reason.as_ref().and_then(as_wire),
            _ => None,
        }),
        AiResponse::Completions(r) => r.choices.first()?.finish_reason.as_ref().and_then(as_wire),
        AiResponse::CompletionsStream(chunks) => chunks.iter().rev().find_map(|c| match c {
            CompletionChunk::Normal(n) => n.choices.first()?.finish_reason.as_ref().and_then(as_wire),
            _ => None,
        }),
        // Neither the Responses API nor Anthropic Messages has an OpenAI-style finish_reason.
        // Their equivalents (Responses' terminal status + `output[]` shape; Anthropic's
        // `stop_reason`) are a different mapping job — left for a follow-up rather than
        // guessed at here, so a NULL means "not extracted yet" rather than "no tool call".
        AiResponse::Responses(_) | AiResponse::ResponsesStream(_) => None,
        AiResponse::Anthropic(_) | AiResponse::AnthropicStream(_) => None,
        AiResponse::Embeddings(_) | AiResponse::Base64Embeddings(_) | AiResponse::Other(_) => None,
    }
}

/// Helper struct for extracting token metrics from responses
#[derive(Debug, Clone)]
struct TokenMetrics {
    prompt_tokens: i64,
    completion_tokens: i64,
    reasoning_tokens: i64,
    total_tokens: i64,
    response_type: String,
    response_model: Option<String>,
}

fn extract_completion_reasoning_tokens(usage: &async_openai::types::chat::CompletionUsage) -> i64 {
    usage
        .completion_tokens_details
        .as_ref()
        .and_then(|d| d.reasoning_tokens)
        .map(|t| t as i64)
        .unwrap_or(0)
}

/// Build [`TokenMetrics`] from a /v1/responses `usage` object (dwctl's own
/// [`ResponseUsage`]), shared by the blocking and streaming arms. Falls back to
/// `input + output` when the provider reported a zero/absent `total_tokens`.
fn token_metrics_from_responses_usage(
    usage: Option<&crate::inference::translation::responses::types::ResponseUsage>,
    response_model: Option<String>,
    response_type: &str,
) -> TokenMetrics {
    match usage {
        Some(usage) => {
            let prompt_tokens = usage.input_tokens as i64;
            let completion_tokens = usage.output_tokens as i64;
            TokenMetrics {
                prompt_tokens,
                completion_tokens,
                reasoning_tokens: usage.output_tokens_details.reasoning_tokens as i64,
                total_tokens: if usage.total_tokens == 0 {
                    prompt_tokens + completion_tokens
                } else {
                    usage.total_tokens as i64
                },
                response_type: response_type.to_string(),
                response_model,
            }
        }
        None => TokenMetrics {
            prompt_tokens: 0,
            completion_tokens: 0,
            reasoning_tokens: 0,
            total_tokens: 0,
            response_type: response_type.to_string(),
            response_model,
        },
    }
}

impl From<&AiResponse> for TokenMetrics {
    fn from(response: &AiResponse) -> Self {
        match response {
            AiResponse::ChatCompletions(response) => {
                if let Some(usage) = &response.usage {
                    Self {
                        prompt_tokens: usage.prompt_tokens as i64,
                        completion_tokens: usage.completion_tokens as i64,
                        reasoning_tokens: extract_completion_reasoning_tokens(usage),
                        total_tokens: usage.total_tokens as i64,
                        response_type: "chat_completion".to_string(),
                        response_model: Some(response.model.clone()),
                    }
                } else {
                    Self {
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        reasoning_tokens: 0,
                        total_tokens: 0,
                        response_type: "chat_completion".to_string(),
                        response_model: Some(response.model.clone()),
                    }
                }
            }
            AiResponse::ChatCompletionsStream(chunks) => {
                // For streaming responses, token usage and model are in the last Normal chunk (not Done marker)
                // Find the last Normal chunk, prioritizing those with usage data
                let last_normal_with_usage = chunks.iter().rev().find_map(|chunk| match chunk {
                    ChatCompletionChunk::Normal(normal_chunk) if normal_chunk.usage.is_some() => Some(normal_chunk),
                    _ => None,
                });

                let model = chunks.iter().find_map(|chunk| match chunk {
                    ChatCompletionChunk::Normal(c) => Some(c.model.clone()),
                    _ => None,
                });

                if let Some(chunk) = last_normal_with_usage {
                    if let Some(usage) = &chunk.usage {
                        Self {
                            prompt_tokens: usage.prompt_tokens as i64,
                            completion_tokens: usage.completion_tokens as i64,
                            reasoning_tokens: extract_completion_reasoning_tokens(usage),
                            total_tokens: usage.total_tokens as i64,
                            response_type: "chat_completion_stream".to_string(),
                            response_model: model,
                        }
                    } else {
                        // This shouldn't happen since we filtered for usage.is_some()
                        Self {
                            prompt_tokens: 0,
                            completion_tokens: 0,
                            reasoning_tokens: 0,
                            total_tokens: 0,
                            response_type: "chat_completion_stream".to_string(),
                            response_model: model,
                        }
                    }
                } else {
                    Self {
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        reasoning_tokens: 0,
                        total_tokens: 0,
                        response_type: "chat_completion_stream".to_string(),
                        response_model: model,
                    }
                }
            }
            AiResponse::CompletionsStream(chunks) => {
                let last_normal_with_usage = chunks.iter().rev().find_map(|chunk| match chunk {
                    CompletionChunk::Normal(normal_chunk) if normal_chunk.usage.is_some() => Some(normal_chunk),
                    _ => None,
                });

                let model = chunks.iter().find_map(|chunk| match chunk {
                    CompletionChunk::Normal(c) => Some(c.model.clone()),
                    _ => None,
                });

                if let Some(chunk) = last_normal_with_usage {
                    if let Some(usage) = &chunk.usage {
                        Self {
                            prompt_tokens: usage.prompt_tokens as i64,
                            completion_tokens: usage.completion_tokens as i64,
                            reasoning_tokens: extract_completion_reasoning_tokens(usage),
                            total_tokens: usage.total_tokens as i64,
                            response_type: "completion_stream".to_string(),
                            response_model: model,
                        }
                    } else {
                        Self {
                            prompt_tokens: 0,
                            completion_tokens: 0,
                            reasoning_tokens: 0,
                            total_tokens: 0,
                            response_type: "completion_stream".to_string(),
                            response_model: model,
                        }
                    }
                } else {
                    Self {
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        reasoning_tokens: 0,
                        total_tokens: 0,
                        response_type: "completion_stream".to_string(),
                        response_model: model,
                    }
                }
            }
            AiResponse::Completions(response) => {
                if let Some(usage) = &response.usage {
                    Self {
                        prompt_tokens: usage.prompt_tokens as i64,
                        completion_tokens: usage.completion_tokens as i64,
                        reasoning_tokens: extract_completion_reasoning_tokens(usage),
                        total_tokens: usage.total_tokens as i64,
                        response_type: "completion".to_string(),
                        response_model: Some(response.model.clone()),
                    }
                } else {
                    Self {
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        reasoning_tokens: 0,
                        total_tokens: 0,
                        response_type: "completion".to_string(),
                        response_model: Some(response.model.clone()),
                    }
                }
            }
            AiResponse::Embeddings(response) => {
                let usage = &response.usage;
                Self {
                    prompt_tokens: usage.prompt_tokens as i64,
                    completion_tokens: 0, // Embeddings don't have completion tokens
                    reasoning_tokens: 0,
                    total_tokens: usage.total_tokens as i64,
                    response_type: "embeddings".to_string(),
                    response_model: Some(response.model.clone()),
                }
            }
            AiResponse::Base64Embeddings(response) => {
                let usage = &response.usage;
                Self {
                    prompt_tokens: usage.prompt_tokens as i64,
                    completion_tokens: 0, // Embeddings don't have completion tokens
                    reasoning_tokens: 0,
                    total_tokens: usage.total_tokens as i64,
                    response_type: "base64_embeddings".to_string(),
                    response_model: Some(response.model.clone()),
                }
            }
            AiResponse::Responses(response) => {
                token_metrics_from_responses_usage(response.usage.as_ref(), Some(response.model.clone()), "response")
            }
            AiResponse::ResponsesStream(events) => {
                // Usage is reported in the terminal response snapshot (response.completed).
                let snapshot = events.iter().rev().find_map(|e| e.response.as_ref().filter(|r| r.usage.is_some()));
                let model = events.iter().rev().find_map(|e| e.response.as_ref().map(|r| r.model.clone()));
                token_metrics_from_responses_usage(snapshot.and_then(|r| r.usage.as_ref()), model, "response_stream")
            }
            AiResponse::Anthropic(response) => {
                // Anthropic reports no `total_tokens` (sum the two) and folds any thinking
                // tokens into `output_tokens`, so there is no separate reasoning count.
                //
                // `input_tokens` EXCLUDES cached tokens in Anthropic's shape, but
                // `prompt_tokens` means TOTAL input everywhere else in dwctl — billing
                // derives uncached input as `prompt - read - creations`, so recording the
                // reduced value subtracts the cached tokens twice and bills them at
                // nothing. Add the cache buckets back to recover the full prompt.
                let usage = &response.usage;
                let prompt_tokens = (usage.input_tokens + usage.cache_read_input_tokens + usage.cache_creation_input_tokens) as i64;
                let completion_tokens = usage.output_tokens as i64;
                Self {
                    prompt_tokens,
                    completion_tokens,
                    reasoning_tokens: 0,
                    total_tokens: prompt_tokens + completion_tokens,
                    response_type: "anthropic_message".to_string(),
                    response_model: Some(response.model.clone()),
                }
            }
            AiResponse::AnthropicStream(events) => {
                // Anthropic splits usage across the stream: `message_start` carries the
                // model and input tokens; the reframer also puts both input and output on
                // `message_delta` (non-standard, see the streaming module), so prefer it.
                // As in the blocking branch, `input_tokens` excludes the cache buckets;
                // they are added back so `prompt_tokens` is the TOTAL input dwctl means
                // everywhere else. Cache counts ride the same frames as the input count.
                let mut input = 0i64;
                let mut cached = 0i64;
                let mut output = 0i64;
                let mut model = None;
                let cache_total = |usage: &Value| -> Option<i64> {
                    let read = usage.get("cache_read_input_tokens").and_then(Value::as_i64);
                    let creation = usage.get("cache_creation_input_tokens").and_then(Value::as_i64);
                    (read.is_some() || creation.is_some()).then(|| read.unwrap_or(0).max(0) + creation.unwrap_or(0).max(0))
                };
                for event in events {
                    match event.get("type").and_then(|v| v.as_str()).unwrap_or_default() {
                        "message_start" => {
                            if let Some(m) = event.pointer("/message/model").and_then(|v| v.as_str()) {
                                model = Some(m.to_string());
                            }
                            if let Some(i) = event
                                .pointer("/message/usage/input_tokens")
                                .and_then(Value::as_i64)
                                .filter(|i| *i > 0)
                            {
                                input = i;
                            }
                            if let Some(c) = event.pointer("/message/usage").and_then(cache_total) {
                                cached = c;
                            }
                        }
                        "message_delta" => {
                            if let Some(usage) = event.get("usage").filter(|u| !u.is_null()) {
                                if let Some(i) = usage.get("input_tokens").and_then(Value::as_i64).filter(|i| *i > 0) {
                                    input = i;
                                }
                                if let Some(c) = cache_total(usage) {
                                    cached = c;
                                }
                                output = usage.get("output_tokens").and_then(Value::as_i64).unwrap_or(output);
                            }
                        }
                        _ => {}
                    }
                }
                let input = input + cached;
                Self {
                    prompt_tokens: input,
                    completion_tokens: output,
                    reasoning_tokens: 0,
                    total_tokens: input + output,
                    response_type: "anthropic_message_stream".to_string(),
                    response_model: model,
                }
            }
            AiResponse::Other(_) => Self {
                prompt_tokens: 0,
                completion_tokens: 0,
                reasoning_tokens: 0,
                total_tokens: 0,
                response_type: "other".to_string(),
                response_model: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{UsageMetrics, extract_cache_tokens, extract_finish_reason, parse_ai_request, parse_ai_response};
    use crate::request_logging::models::{AiRequest, AiResponse};
    use async_openai::types::chat::{CreateChatCompletionResponse, CreateChatCompletionStreamResponse};
    use async_openai::types::completions::CreateCompletionResponse;
    use async_openai::types::embeddings::{CreateBase64EmbeddingResponse, CreateEmbeddingResponse, EmbeddingUsage};
    use axum::http::{Method, StatusCode, Uri};
    use bytes::Bytes;
    use outlet::{RequestData, ResponseData};
    use std::{
        collections::HashMap,
        time::{Duration, SystemTime},
    };
    use uuid::Uuid;

    #[test]
    fn test_parse_ai_request_no_body() {
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
            trace_id: None,
            span_id: None,
        };

        let result = parse_ai_request(&request_data).unwrap();

        match result.request {
            AiRequest::Other(value) => assert!(value.is_null()),
            _ => panic!("Expected AiRequest::Other(null)"),
        }
    }

    #[test]
    fn test_parse_ai_request_empty_bytes() {
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: Some(Bytes::new()), // Empty bytes
            trace_id: None,
            span_id: None,
        };

        let result = parse_ai_request(&request_data).unwrap();

        match result.request {
            AiRequest::Other(value) => assert!(value.is_null()),
            _ => panic!("Expected AiRequest::Other(null)"),
        }
    }

    #[test]
    fn test_parse_ai_request_invalid_json() {
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: Some(Bytes::from("invalid json")),
            trace_id: None,
            span_id: None,
        };

        let result = parse_ai_request(&request_data);

        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.fallback_data.starts_with("base64:"));
    }

    #[test]
    fn test_parse_ai_request_valid_json() {
        let json_body = r#"{"model": "gpt-4", "messages": [{"role": "user", "content": "hello"}]}"#;
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: Some(Bytes::from(json_body)),
            trace_id: None,
            span_id: None,
        };

        let result = parse_ai_request(&request_data).unwrap();

        match result.request {
            AiRequest::ChatCompletions(req) => {
                assert_eq!(req.model, "gpt-4");
                assert_eq!(req.messages.len(), 1);
            }
            _ => panic!("Expected AiRequest::ChatCompletions"),
        }
    }

    #[test]
    fn test_parse_ai_request_completions() {
        let json_body = r#"{"model": "gpt-3.5-turbo-instruct", "prompt": "Say hello"}"#;
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: Some(Bytes::from(json_body)),
            trace_id: None,
            span_id: None,
        };

        let result = parse_ai_request(&request_data).unwrap();

        match result.request {
            AiRequest::Completions(req) => {
                assert_eq!(req.model, "gpt-3.5-turbo-instruct");
            }
            _ => panic!("Expected AiRequest::Completions"),
        }
    }

    #[test]
    fn test_parse_ai_request_embeddings() {
        let json_body = r#"{"model": "text-embedding-ada-002", "input": "hello world"}"#;
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: Some(Bytes::from(json_body)),
            trace_id: None,
            span_id: None,
        };

        let result = parse_ai_request(&request_data).unwrap();

        match result.request {
            AiRequest::Embeddings(req) => {
                assert_eq!(req.model, "text-embedding-ada-002");
            }
            _ => panic!("Expected AiRequest::Embeddings"),
        }
    }

    #[test]
    fn test_parse_ai_response_no_body() {
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
            trace_id: None,
            span_id: None,
        };

        let response_data = ResponseData {
            extensions: Default::default(),
            correlation_id: 123,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: None,
            duration: Duration::from_millis(100),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let result = parse_ai_response(&request_data, &response_data).unwrap();

        match result {
            AiResponse::Other(value) => assert!(value.is_null()),
            _ => panic!("Expected AiResponse::Other(null)"),
        }
    }

    #[test]
    fn test_parse_ai_response_empty_body() {
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
            trace_id: None,
            span_id: None,
        };

        let response_data = ResponseData {
            extensions: Default::default(),
            correlation_id: 123,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: Some(Bytes::new()), // Empty bytes
            duration: Duration::from_millis(100),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let result = parse_ai_response(&request_data, &response_data).unwrap();

        match result {
            AiResponse::Other(value) => assert!(value.is_null()),
            _ => panic!("Expected AiResponse::Other(null)"),
        }
    }

    #[test]
    fn test_parse_ai_response_valid_json() {
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
            trace_id: None,
            span_id: None,
        };

        let json_response = r#"{
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "created": 1677652288,
            "model": "gpt-4",
            "choices": [],
            "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
        }"#;

        let response_data = ResponseData {
            extensions: Default::default(),
            correlation_id: 123,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: Some(Bytes::from(json_response)),
            duration: Duration::from_millis(100),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let result = parse_ai_response(&request_data, &response_data).unwrap();

        match result {
            AiResponse::ChatCompletions(response) => {
                assert_eq!(response.model, "gpt-4");
                assert_eq!(response.id, "chatcmpl-123");
            }
            _ => panic!("Expected AiResponse::ChatCompletions"),
        }
    }

    #[test]
    fn test_parse_ai_response_streaming() {
        // Request with stream: true
        let request_json = r#"{"model": "gpt-4", "messages": [{"role": "user", "content": "hello"}], "stream": true}"#;
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: Some(Bytes::from(request_json)),
            trace_id: None,
            span_id: None,
        };

        // SSE streaming response
        let sse_response = "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1677652288,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"}}]}\n\ndata: [DONE]\n\n";

        let response_data = ResponseData {
            extensions: Default::default(),
            correlation_id: 123,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: Some(Bytes::from(sse_response)),
            duration: Duration::from_millis(100),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let result = parse_ai_response(&request_data, &response_data).unwrap();

        match result {
            AiResponse::ChatCompletionsStream(chunks) => {
                assert!(!chunks.is_empty());
            }
            _ => panic!("Expected AiResponse::ChatCompletionsStream"),
        }
    }

    #[test]
    fn test_parse_ai_response_fusillade_stream_header() {
        // Request body has stream: false, but x-fusillade-stream header is set.
        // Outlet captures the original body before onwards injects stream:true,
        // so the header is the only signal that the response is SSE.
        let request_json = r#"{"model": "gpt-4", "messages": [{"role": "user", "content": "hello"}], "stream": false}"#;
        let mut headers = HashMap::new();
        headers.insert("x-fusillade-stream".to_string(), vec![Bytes::from("true")]);
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers,
            body: Some(Bytes::from(request_json)),
            trace_id: None,
            span_id: None,
        };

        let sse_response = "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1677652288,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"}}],\"usage\":null}\n\ndata: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1677652288,\"model\":\"gpt-4\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\ndata: [DONE]\n\n";

        let response_data = ResponseData {
            extensions: Default::default(),
            correlation_id: 123,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: Some(Bytes::from(sse_response)),
            duration: Duration::from_millis(100),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let result = parse_ai_response(&request_data, &response_data).unwrap();

        match &result {
            AiResponse::ChatCompletionsStream(chunks) => {
                assert!(!chunks.is_empty(), "Expected parsed SSE chunks");
                // Verify usage is extractable (this is what billing uses)
                let metrics = UsageMetrics::extract(
                    uuid::Uuid::nil(),
                    &request_data,
                    &response_data,
                    &result,
                    &crate::config::Config::default(),
                );
                assert_eq!(metrics.prompt_tokens, 10);
                assert_eq!(metrics.completion_tokens, 5);
                assert_eq!(metrics.total_tokens, 15);
            }
            other => panic!("Expected ChatCompletionsStream, got {:?}", std::mem::discriminant(other)),
        }
    }

    #[test]
    fn test_fusillade_stream_with_embedded_error_frame_reclassifies_to_500() {
        // Reproduces trace 91ea8848dc08735f183449277b8b8846: Dynamo started a 200 OK
        // SSE stream, generated some delta chunks, then crashed mid-generation and
        // emitted an error frame in place of the terminal usage chunk + [DONE].
        let request_json = r#"{"model": "moonshotai/Kimi-K2.6", "messages": [{"role": "user", "content": "hi"}], "stream": false}"#;
        let mut headers = HashMap::new();
        headers.insert("x-fusillade-stream".to_string(), vec![Bytes::from("true")]);
        let request_data = RequestData {
            correlation_id: 999,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/v1/chat/completions".parse::<Uri>().unwrap(),
            headers,
            body: Some(Bytes::from(request_json)),
            trace_id: None,
            span_id: None,
        };

        let sse_response = concat!(
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"moonshotai/Kimi-K2.6\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n",
            "data: {\"error\":{\"message\":\"Engine was shut down during token generation\",\"type\":\"internal_server_error\",\"code\":500}}\n\n",
        );

        let response_data = ResponseData {
            extensions: Default::default(),
            correlation_id: 999,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: Some(Bytes::from(sse_response)),
            duration: Duration::from_millis(335_000),
            duration_to_first_byte: Duration::from_millis(2_300),
        };

        let parsed = parse_ai_response(&request_data, &response_data).unwrap();
        let metrics = UsageMetrics::extract(
            uuid::Uuid::nil(),
            &request_data,
            &response_data,
            &parsed,
            &crate::config::Config::default(),
        );

        assert_eq!(
            metrics.status_code, 500,
            "200 OK with embedded SSE error frame must be reclassified to 500 so success-rate \
             metrics and credit-eligibility checks exclude this row, and so the analytics row \
             agrees with fusillade's HTTP-layer reclassification"
        );
        assert_eq!(metrics.total_tokens, 0);
        assert_eq!(metrics.response_type, "chat_completion_stream");
    }

    #[test]
    fn test_served_by_extension_flows_into_usage_metrics() {
        // The onwards ServedBy response extension (set at final load-balancer
        // selection) must reach the analytics record, so composite-model
        // traffic can be attributed to the component that actually served it.
        let request_json = r#"{"model": "zai-org/GLM-5.2-FP8", "messages": [{"role": "user", "content": "hi"}]}"#;
        let request_data = RequestData {
            correlation_id: 42,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/v1/chat/completions".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: Some(Bytes::from(request_json)),
            trace_id: None,
            span_id: None,
        };

        let mut extensions = axum::http::Extensions::new();
        extensions.insert(onwards::ServedBy {
            url: "https://router.requesty.ai/v1".to_string(),
            onwards_model: Some("policy/glm-5.2".to_string()),
        });
        let response_data = ResponseData {
            extensions,
            correlation_id: 42,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: None,
            duration: Duration::from_millis(100),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let parsed = parse_ai_response(&request_data, &response_data).unwrap();
        let metrics = UsageMetrics::extract(
            uuid::Uuid::nil(),
            &request_data,
            &response_data,
            &parsed,
            &crate::config::Config::default(),
        );
        assert_eq!(metrics.served_by.as_deref(), Some("https://router.requesty.ai/v1"));

        // Absent extension → None (request never reached an upstream).
        let response_data_no_ext = ResponseData {
            extensions: Default::default(),
            correlation_id: 42,
            timestamp: SystemTime::now(),
            status: StatusCode::BAD_GATEWAY,
            headers: HashMap::new(),
            body: None,
            duration: Duration::from_millis(100),
            duration_to_first_byte: Duration::from_millis(50),
        };
        let parsed = parse_ai_response(&request_data, &response_data_no_ext).unwrap();
        let metrics = UsageMetrics::extract(
            uuid::Uuid::nil(),
            &request_data,
            &response_data_no_ext,
            &parsed,
            &crate::config::Config::default(),
        );
        assert_eq!(metrics.served_by, None);
    }

    #[test]
    fn test_fusillade_stream_with_real_error_status_is_preserved() {
        // If upstream returns a real non-2xx status (no SSE body to scan), we must NOT
        // override it to 500. The real status code is more informative.
        let request_json = r#"{"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}], "stream": false}"#;
        let mut headers = HashMap::new();
        headers.insert("x-fusillade-stream".to_string(), vec![Bytes::from("true")]);
        let request_data = RequestData {
            correlation_id: 7,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/v1/chat/completions".parse::<Uri>().unwrap(),
            headers,
            body: Some(Bytes::from(request_json)),
            trace_id: None,
            span_id: None,
        };
        let response_data = ResponseData {
            extensions: Default::default(),
            correlation_id: 7,
            timestamp: SystemTime::now(),
            status: StatusCode::TOO_MANY_REQUESTS,
            headers: HashMap::new(),
            body: Some(Bytes::from(r#"{"error":{"message":"rate limit","type":"rate_limit"}}"#)),
            duration: Duration::from_millis(50),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let parsed = parse_ai_response(&request_data, &response_data).unwrap();
        let metrics = UsageMetrics::extract(
            uuid::Uuid::nil(),
            &request_data,
            &response_data,
            &parsed,
            &crate::config::Config::default(),
        );

        assert_eq!(metrics.status_code, 429);
    }

    #[test]
    fn test_parse_ai_response_fusillade_completions_stream() {
        let request_json = r#"{"model": "gpt-3.5-turbo-instruct", "prompt": "Hello", "stream": false}"#;
        let mut headers = HashMap::new();
        headers.insert("x-fusillade-stream".to_string(), vec![Bytes::from("true")]);
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers,
            body: Some(Bytes::from(request_json)),
            trace_id: None,
            span_id: None,
        };

        let sse_response = "data: {\"id\":\"cmpl-123\",\"object\":\"text_completion\",\"created\":1677652288,\"model\":\"gpt-3.5-turbo-instruct\",\"choices\":[{\"text\":\" world\",\"index\":0}]}\n\ndata: {\"id\":\"cmpl-123\",\"object\":\"text_completion\",\"created\":1677652288,\"model\":\"gpt-3.5-turbo-instruct\",\"choices\":[],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":12,\"total_tokens\":20}}\n\ndata: [DONE]\n\n";

        let response_data = ResponseData {
            extensions: Default::default(),
            correlation_id: 123,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: Some(Bytes::from(sse_response)),
            duration: Duration::from_millis(100),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let result = parse_ai_response(&request_data, &response_data).unwrap();

        match &result {
            AiResponse::CompletionsStream(chunks) => {
                assert!(!chunks.is_empty(), "Expected parsed SSE chunks");
                let metrics = UsageMetrics::extract(
                    uuid::Uuid::nil(),
                    &request_data,
                    &response_data,
                    &result,
                    &crate::config::Config::default(),
                );
                assert_eq!(metrics.prompt_tokens, 8);
                assert_eq!(metrics.completion_tokens, 12);
                assert_eq!(metrics.total_tokens, 20);
            }
            other => panic!("Expected CompletionsStream, got {:?}", std::mem::discriminant(other)),
        }
    }

    #[test]
    fn test_parse_ai_response_embeddings() {
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
            trace_id: None,
            span_id: None,
        };

        let embeddings_response = r#"{
            "object": "list",
            "data": [{"object": "embedding", "embedding": [0.1, 0.2], "index": 0}],
            "model": "text-embedding-ada-002",
            "usage": {"prompt_tokens": 5, "total_tokens": 5}
        }"#;

        let response_data = ResponseData {
            extensions: Default::default(),
            correlation_id: 123,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: Some(Bytes::from(embeddings_response)),
            duration: Duration::from_millis(100),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let result = parse_ai_response(&request_data, &response_data).unwrap();

        match result {
            AiResponse::Embeddings(response) => {
                assert_eq!(response.model, "text-embedding-ada-002");
                assert_eq!(response.object, "list");
            }
            _ => panic!("Expected AiResponse::Embeddings"),
        }
    }

    #[test]
    fn test_parse_ai_response_invalid_json() {
        let request_data = RequestData {
            correlation_id: 123,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/test".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
            trace_id: None,
            span_id: None,
        };

        let response_data = ResponseData {
            extensions: Default::default(),
            correlation_id: 123,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: Some(Bytes::from("invalid json response")),
            duration: Duration::from_millis(100),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let result = parse_ai_response(&request_data, &response_data);

        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.fallback_data.starts_with("base64:"));
    }

    #[test]
    fn test_analytics_metrics_extract_basic() {
        let instance_id = Uuid::new_v4();

        let request_data = RequestData {
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/v1/chat/completions".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
            trace_id: None,
            span_id: None,
        };

        let response_data = ResponseData {
            extensions: Default::default(),
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: None,
            duration: Duration::from_millis(250),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let parsed_response = AiResponse::Other(serde_json::Value::Null);

        let metrics = UsageMetrics::extract(
            instance_id,
            &request_data,
            &response_data,
            &parsed_response,
            &crate::test::utils::create_test_config(),
        );

        assert_eq!(metrics.instance_id, instance_id);
        assert_eq!(metrics.correlation_id, 12345);
        assert_eq!(metrics.method, "POST");
        assert_eq!(metrics.uri, "/v1/chat/completions");
        assert_eq!(metrics.request_model, None);
        assert_eq!(metrics.response_model, None);
        assert_eq!(metrics.status_code, 200);
        assert_eq!(metrics.duration_ms, 250);
        assert_eq!(metrics.duration_to_first_byte_ms, Some(50));
        assert_eq!(metrics.prompt_tokens, 0);
        assert_eq!(metrics.completion_tokens, 0);
        assert_eq!(metrics.total_tokens, 0);
        assert_eq!(metrics.response_type, "other");
    }

    #[test]
    fn test_analytics_metrics_extract_with_tokens() {
        let instance_id = Uuid::new_v4();

        // Request with model info
        let request_json = r#"{"model": "gpt-4", "messages": [{"role": "user", "content": "hello"}]}"#;
        let request_data = RequestData {
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/v1/chat/completions".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: Some(Bytes::from(request_json)),
            trace_id: None,
            span_id: None,
        };

        let response_data = ResponseData {
            extensions: Default::default(),
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: None,
            duration: Duration::from_millis(500),
            duration_to_first_byte: Duration::from_millis(50),
        };

        // Response with usage data
        #[allow(deprecated)]
        let chat_response = CreateChatCompletionResponse {
            id: "chatcmpl-123".to_string(),
            object: "chat.completion".to_string(),
            created: 1677652288,
            model: "gpt-5".to_string(),
            choices: vec![],
            usage: Some(async_openai::types::chat::CompletionUsage {
                prompt_tokens: 15,
                completion_tokens: 25,
                total_tokens: 40,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
            system_fingerprint: None,
            service_tier: None,
        };

        let parsed_response = AiResponse::ChatCompletions(chat_response);

        let metrics = UsageMetrics::extract(
            instance_id,
            &request_data,
            &response_data,
            &parsed_response,
            &crate::test::utils::create_test_config(),
        );

        assert_eq!(metrics.instance_id, instance_id);
        assert_eq!(metrics.correlation_id, 12345);
        assert_eq!(metrics.method, "POST");
        assert_eq!(metrics.uri, "/v1/chat/completions");
        assert_eq!(metrics.request_model, Some("gpt-4".to_string()));
        assert_eq!(metrics.response_model, Some("gpt-5".to_string()));
        assert_eq!(metrics.status_code, 200);
        assert_eq!(metrics.duration_ms, 500);
        assert_eq!(metrics.prompt_tokens, 15);
        assert_eq!(metrics.completion_tokens, 25);
        assert_eq!(metrics.reasoning_tokens, 0);
        assert_eq!(metrics.total_tokens, 40);
        assert_eq!(metrics.response_type, "chat_completion");
    }

    #[test]
    fn test_analytics_metrics_extract_streaming_tokens() {
        let instance_id = Uuid::new_v4();

        let request_data = RequestData {
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/v1/chat/completions".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
            trace_id: None,
            span_id: None,
        };

        let response_data = ResponseData {
            extensions: Default::default(),
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: None,
            duration: Duration::from_millis(300),
            duration_to_first_byte: Duration::from_millis(50),
        };

        // Streaming response with usage in the last chunk
        #[allow(deprecated)]
        let stream_chunk = CreateChatCompletionStreamResponse {
            id: "chatcmpl-123".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1677652288,
            model: "gpt-4".to_string(),
            choices: vec![],
            usage: Some(async_openai::types::chat::CompletionUsage {
                prompt_tokens: 8,
                completion_tokens: 12,
                total_tokens: 20,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
            system_fingerprint: None,
            service_tier: None,
        };

        let parsed_response =
            AiResponse::ChatCompletionsStream(vec![crate::request_logging::models::ChatCompletionChunk::Normal(stream_chunk)]);

        let metrics = UsageMetrics::extract(
            instance_id,
            &request_data,
            &response_data,
            &parsed_response,
            &crate::test::utils::create_test_config(),
        );

        assert_eq!(metrics.prompt_tokens, 8);
        assert_eq!(metrics.completion_tokens, 12);
        assert_eq!(metrics.reasoning_tokens, 0);
        assert_eq!(metrics.total_tokens, 20);
        assert_eq!(metrics.response_type, "chat_completion_stream");
    }

    #[test]
    fn test_analytics_metrics_extract_chat_reasoning_tokens() {
        let instance_id = Uuid::new_v4();
        let request_json = r#"{"model": "gpt-5", "messages": [{"role": "user", "content": "hello"}]}"#;
        let request_data = RequestData {
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/v1/chat/completions".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: Some(Bytes::from(request_json)),
            trace_id: None,
            span_id: None,
        };

        let response_data = ResponseData {
            extensions: Default::default(),
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: None,
            duration: Duration::from_millis(500),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let chat_response: CreateChatCompletionResponse = serde_json::from_value(serde_json::json!({
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "created": 1677652288,
            "model": "gpt-5",
            "choices": [],
            "usage": {
                "prompt_tokens": 15,
                "completion_tokens": 25,
                "total_tokens": 40,
                "completion_tokens_details": {
                    "reasoning_tokens": 11
                }
            }
        }))
        .unwrap();

        let parsed_response = AiResponse::ChatCompletions(chat_response);
        let metrics = UsageMetrics::extract(
            instance_id,
            &request_data,
            &response_data,
            &parsed_response,
            &crate::test::utils::create_test_config(),
        );

        assert_eq!(metrics.reasoning_tokens, 11);
        assert_eq!(metrics.completion_tokens, 25);
        assert_eq!(metrics.total_tokens, 40);
    }

    #[test]
    fn test_analytics_metrics_extract_embeddings_tokens() {
        let instance_id = Uuid::new_v4();

        let request_data = RequestData {
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/v1/embeddings".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
            trace_id: None,
            span_id: None,
        };

        let response_data = ResponseData {
            extensions: Default::default(),
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: None,
            duration: Duration::from_millis(150),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let embeddings_response = CreateEmbeddingResponse {
            object: "list".to_string(),
            data: vec![],
            model: "text-embedding-ada-002".to_string(),
            usage: EmbeddingUsage {
                prompt_tokens: 6,
                total_tokens: 6,
            },
        };

        let parsed_response = AiResponse::Embeddings(embeddings_response);

        let metrics = UsageMetrics::extract(
            instance_id,
            &request_data,
            &response_data,
            &parsed_response,
            &crate::test::utils::create_test_config(),
        );

        assert_eq!(metrics.prompt_tokens, 6);
        assert_eq!(metrics.completion_tokens, 0); // Embeddings don't have completion tokens
        assert_eq!(metrics.total_tokens, 6);
        assert_eq!(metrics.response_type, "embeddings");
    }

    #[test]
    fn test_analytics_metrics_extract_completions_tokens() {
        let instance_id = Uuid::new_v4();

        let request_data = RequestData {
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/v1/completions".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
            trace_id: None,
            span_id: None,
        };

        let response_data = ResponseData {
            extensions: Default::default(),
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: None,
            duration: Duration::from_millis(400),
            duration_to_first_byte: Duration::from_millis(50),
        };

        #[allow(deprecated)]
        let completions_response = CreateCompletionResponse {
            id: "cmpl-123".to_string(),
            object: "text_completion".to_string(),
            created: 1677652288,
            model: "gpt-3.5-turbo-instruct".to_string(),
            choices: vec![],
            usage: Some(async_openai::types::chat::CompletionUsage {
                prompt_tokens: 10,
                completion_tokens: 15,
                total_tokens: 25,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
            system_fingerprint: None,
        };

        let parsed_response = AiResponse::Completions(completions_response);

        let metrics = UsageMetrics::extract(
            instance_id,
            &request_data,
            &response_data,
            &parsed_response,
            &crate::test::utils::create_test_config(),
        );

        assert_eq!(metrics.prompt_tokens, 10);
        assert_eq!(metrics.completion_tokens, 15);
        assert_eq!(metrics.total_tokens, 25);
        assert_eq!(metrics.response_type, "completion");
    }

    #[test]
    fn test_analytics_metrics_extract_base64_embeddings_tokens() {
        let instance_id = Uuid::new_v4();

        let request_data = RequestData {
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/v1/embeddings".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: None,
            trace_id: None,
            span_id: None,
        };

        let response_data = ResponseData {
            extensions: Default::default(),
            correlation_id: 12345,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: None,
            duration: Duration::from_millis(200),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let base64_embeddings_response = CreateBase64EmbeddingResponse {
            object: "list".to_string(),
            data: vec![],
            model: "text-embedding-3-large".to_string(),
            usage: EmbeddingUsage {
                prompt_tokens: 4,
                total_tokens: 4,
            },
        };

        let parsed_response = AiResponse::Base64Embeddings(base64_embeddings_response);

        let metrics = UsageMetrics::extract(
            instance_id,
            &request_data,
            &response_data,
            &parsed_response,
            &crate::test::utils::create_test_config(),
        );

        assert_eq!(metrics.prompt_tokens, 4);
        assert_eq!(metrics.completion_tokens, 0); // Base64 embeddings don't have completion tokens
        assert_eq!(metrics.total_tokens, 4);
        assert_eq!(metrics.response_type, "base64_embeddings");
    }

    // A full dwctl `ResponsesResponse` body. The response is produced by dwctl's
    // edge translator (which backfills every field), so the logging/billing parse
    // uses the strict dwctl schema - the test body must carry all required fields.
    fn responses_api_body(usage: bool) -> String {
        let usage_json = if usage {
            r#","usage":{"input_tokens":15,"input_tokens_details":{"cached_tokens":0},"output_tokens":25,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":40}"#
        } else {
            ""
        };
        format!(
            r#"{{"id":"resp_123","object":"response","created_at":1234567890,"status":"completed","model":"gpt-4o","output":[],"tools":[],"tool_choice":"auto","truncation":"disabled","parallel_tool_calls":true,"text":{{"format":{{"type":"text"}}}},"top_p":1.0,"presence_penalty":0.0,"frequency_penalty":0.0,"top_logprobs":0,"temperature":1.0,"reasoning":null,"store":false,"background":false,"service_tier":"default"{usage_json}}}"#
        )
    }

    fn responses_request_data(stream: Option<bool>) -> RequestData {
        let stream_field = match stream {
            Some(true) => r#","stream":true"#,
            Some(false) => r#","stream":false"#,
            None => "",
        };
        let body = format!(r#"{{"model":"gpt-4o","input":"tell me a joke"{stream_field}}}"#);
        RequestData {
            correlation_id: 1,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/v1/responses".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: Some(Bytes::from(body)),
            trace_id: None,
            span_id: None,
        }
    }

    fn responses_response_data(body: String) -> ResponseData {
        ResponseData {
            extensions: Default::default(),
            correlation_id: 1,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: Some(Bytes::from(body)),
            duration: Duration::from_millis(100),
            duration_to_first_byte: Duration::from_millis(50),
        }
    }

    #[test]
    fn test_parse_ai_request_responses_path_not_classified_as_embeddings() {
        // Both embeddings and responses requests have an `input` field.
        // Path-based detection must prevent /v1/responses bodies being classified as Embeddings.
        let result = parse_ai_request(&responses_request_data(None)).unwrap();

        let rr = result.responses_request.expect("responses_request should be set");
        assert_eq!(rr.model, Some("gpt-4o".to_string()));
        assert_eq!(rr.stream, None);

        match result.request {
            AiRequest::Other(_) => {}
            _ => panic!("expected AiRequest::Other for /v1/responses, not Embeddings or ChatCompletions"),
        }
    }

    #[test]
    fn test_parse_ai_request_responses_path_stream_flag() {
        let result = parse_ai_request(&responses_request_data(Some(true))).unwrap();
        let rr = result.responses_request.unwrap();
        assert_eq!(rr.stream, Some(true));
    }

    #[test]
    fn test_parse_ai_response_responses_non_streaming() {
        let request_data = responses_request_data(None);
        let response_data = responses_response_data(responses_api_body(true));

        let result = parse_ai_response(&request_data, &response_data).unwrap();

        match result {
            AiResponse::Responses(resp) => {
                assert_eq!(resp.model, "gpt-4o");
                let usage = resp.usage.expect("usage should be present");
                assert_eq!(usage.input_tokens, 15);
                assert_eq!(usage.output_tokens, 25);
                assert_eq!(usage.output_tokens_details.reasoning_tokens, 0);
                assert_eq!(usage.total_tokens, 40);
            }
            _ => panic!("expected AiResponse::Responses"),
        }
    }

    #[test]
    fn test_analytics_metrics_extract_responses_reasoning_tokens() {
        let instance_id = Uuid::new_v4();
        let request_data = responses_request_data(None);
        let response_data = responses_response_data(
            r#"{"id":"resp_123","object":"response","created_at":1234567890,"status":"completed","model":"gpt-4o","output":[],"tools":[],"tool_choice":"auto","truncation":"disabled","parallel_tool_calls":true,"text":{"format":{"type":"text"}},"top_p":1.0,"presence_penalty":0.0,"frequency_penalty":0.0,"top_logprobs":0,"temperature":1.0,"reasoning":null,"store":false,"background":false,"service_tier":"default","usage":{"input_tokens":15,"input_tokens_details":{"cached_tokens":0},"output_tokens":25,"output_tokens_details":{"reasoning_tokens":9},"total_tokens":40}}"#
                .to_string(),
        );

        let parsed_response = parse_ai_response(&request_data, &response_data).unwrap();
        let metrics = UsageMetrics::extract(
            instance_id,
            &request_data,
            &response_data,
            &parsed_response,
            &crate::test::utils::create_test_config(),
        );

        assert_eq!(metrics.reasoning_tokens, 9);
        assert_eq!(metrics.completion_tokens, 25);
        assert_eq!(metrics.total_tokens, 40);
        assert_eq!(metrics.response_type, "response");
    }

    #[test]
    fn test_parse_ai_response_responses_error_body_falls_back_to_other() {
        // 4xx/5xx error responses from the provider don't match the Response schema;
        // they should be stored as AiResponse::Other rather than returning a SerializationError.
        let request_data = responses_request_data(None);
        let error_json = r#"{"error":{"message":"invalid request","type":"invalid_request_error","code":"model_not_found"}}"#;
        let response_data = ResponseData {
            extensions: Default::default(),
            correlation_id: 1,
            timestamp: SystemTime::now(),
            status: StatusCode::BAD_REQUEST,
            headers: HashMap::new(),
            body: Some(Bytes::from(error_json)),
            duration: Duration::from_millis(100),
            duration_to_first_byte: Duration::from_millis(50),
        };

        let result = parse_ai_response(&request_data, &response_data).unwrap();
        match result {
            AiResponse::Other(_) => {}
            _ => panic!("expected AiResponse::Other for error body, got something else"),
        }
    }

    #[test]
    fn test_parse_ai_response_responses_streaming() {
        let request_data = responses_request_data(Some(true));

        // SSE body with a response.completed event carrying usage
        let completed_data = responses_api_body(true);
        let sse_body = format!(
            "data: {{\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"item_id\":\"item_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"Hello\"}}\n\ndata: {{\"type\":\"response.completed\",\"sequence_number\":5,\"response\":{completed_data}}}\n\n"
        );
        let response_data = responses_response_data(sse_body);

        let result = parse_ai_response(&request_data, &response_data).unwrap();

        match result {
            AiResponse::ResponsesStream(events) => {
                assert!(!events.is_empty(), "should have parsed at least the completed event");
                let has_completed = events.iter().any(|e| e.event_type == "response.completed");
                assert!(has_completed, "should contain a response.completed event");
            }
            _ => panic!("expected AiResponse::ResponsesStream"),
        }
    }

    #[test]
    fn test_analytics_metrics_extract_responses_tokens() {
        let instance_id = Uuid::new_v4();
        let request_data = responses_request_data(None);
        let response_data = responses_response_data(responses_api_body(true));

        let parsed_response = parse_ai_response(&request_data, &response_data).unwrap();

        let metrics = UsageMetrics::extract(
            instance_id,
            &request_data,
            &response_data,
            &parsed_response,
            &crate::test::utils::create_test_config(),
        );

        assert_eq!(metrics.request_model, Some("gpt-4o".to_string()));
        assert_eq!(metrics.response_model, Some("gpt-4o".to_string()));
        assert_eq!(metrics.prompt_tokens, 15);
        assert_eq!(metrics.completion_tokens, 25);
        assert_eq!(metrics.total_tokens, 40);
        assert_eq!(metrics.response_type, "response");
    }

    #[test]
    fn test_analytics_metrics_extract_responses_streaming_tokens() {
        let instance_id = Uuid::new_v4();
        let request_data = responses_request_data(Some(true));

        let completed_data = responses_api_body(true);
        let sse_body = format!("data: {{\"type\":\"response.completed\",\"sequence_number\":5,\"response\":{completed_data}}}\n\n");
        let response_data = responses_response_data(sse_body);

        let parsed_response = parse_ai_response(&request_data, &response_data).unwrap();

        let metrics = UsageMetrics::extract(
            instance_id,
            &request_data,
            &response_data,
            &parsed_response,
            &crate::test::utils::create_test_config(),
        );

        assert_eq!(metrics.request_model, Some("gpt-4o".to_string()));
        assert_eq!(metrics.response_model, Some("gpt-4o".to_string()));
        assert_eq!(metrics.prompt_tokens, 15);
        assert_eq!(metrics.completion_tokens, 25);
        assert_eq!(metrics.total_tokens, 40);
        assert_eq!(metrics.response_type, "response_stream");
    }

    // ----- Anthropic /v1/messages billing (the original goal: Anthropic charged) -----

    fn messages_request_data(stream: bool) -> RequestData {
        let body =
            format!(r#"{{"model":"claude-3-5-sonnet","max_tokens":100,"messages":[{{"role":"user","content":"hi"}}],"stream":{stream}}}"#);
        RequestData {
            correlation_id: 1,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/v1/messages".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: Some(Bytes::from(body)),
            trace_id: None,
            span_id: None,
        }
    }

    fn ok_response(body: impl Into<Bytes>) -> ResponseData {
        ResponseData {
            extensions: Default::default(),
            correlation_id: 1,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: Some(body.into()),
            duration: Duration::from_millis(100),
            duration_to_first_byte: Duration::from_millis(50),
        }
    }

    #[test]
    fn test_parse_ai_response_anthropic_blocking() {
        let request_data = messages_request_data(false);
        let body = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-3-5-sonnet","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":12,"output_tokens":8}}"#;
        let response_data = ok_response(body);

        let result = parse_ai_response(&request_data, &response_data).unwrap();
        match result {
            AiResponse::Anthropic(resp) => {
                assert_eq!(resp.model, "claude-3-5-sonnet");
                assert_eq!(resp.usage.input_tokens, 12);
                assert_eq!(resp.usage.output_tokens, 8);
            }
            _ => panic!("expected AiResponse::Anthropic"),
        }
    }

    #[test]
    fn test_analytics_metrics_extract_anthropic_blocking_tokens() {
        let request_data = messages_request_data(false);
        let body = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-3-5-sonnet","content":[],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":12,"output_tokens":8}}"#;
        let response_data = ok_response(body);

        let parsed = parse_ai_response(&request_data, &response_data).unwrap();
        let metrics = UsageMetrics::extract(
            Uuid::new_v4(),
            &request_data,
            &response_data,
            &parsed,
            &crate::test::utils::create_test_config(),
        );

        assert_eq!(metrics.response_model, Some("claude-3-5-sonnet".to_string()));
        assert_eq!(metrics.prompt_tokens, 12);
        assert_eq!(metrics.completion_tokens, 8);
        // Anthropic has no total_tokens; billed as input + output.
        assert_eq!(metrics.total_tokens, 20);
        assert_eq!(metrics.reasoning_tokens, 0);
        assert_eq!(metrics.response_type, "anthropic_message");
        assert_eq!(metrics.status_code, 200);
    }

    /// The bug this guards: Anthropic's `input_tokens` EXCLUDES cached tokens, so
    /// recording it verbatim made `prompt_tokens` mean "uncached input" on this ingress
    /// while meaning "total input" everywhere else. Billing derives uncached input as
    /// `prompt - read - creations`, so the cached tokens were subtracted twice and billed
    /// at nothing (observed: 3.28M prompt tokens recorded against 19.34M processed).
    /// The assertion that matters is PARITY: the SAME conversation, billed through either
    /// ingress, must report the same `prompt_tokens` — that is the invariant billing
    /// relies on, and the one nothing checked before.
    #[test]
    fn anthropic_prompt_tokens_include_cached_and_match_chat_completions() {
        // One conversation, 20k total input: 12k read from cache, 3k written, 5k uncached.
        // Anthropic splits it as input_tokens=5000 + the two cache buckets; chat
        // completions reports prompt_tokens=20000 with the cached share as a detail.
        let anthropic_request = messages_request_data(false);
        let anthropic_response = ok_response(
            r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-3-5-sonnet","content":[],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":5000,"output_tokens":8,"cache_read_input_tokens":12000,"cache_creation_input_tokens":3000,"cache_creation":{"ephemeral_1h_input_tokens":3000}}}"#,
        );
        let anthropic_parsed = parse_ai_response(&anthropic_request, &anthropic_response).unwrap();
        let anthropic = UsageMetrics::extract(
            Uuid::new_v4(),
            &anthropic_request,
            &anthropic_response,
            &anthropic_parsed,
            &crate::test::utils::create_test_config(),
        );

        let chat_request = RequestData {
            correlation_id: 2,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/v1/chat/completions".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: Some(Bytes::from(
                r#"{"model":"claude-3-5-sonnet","messages":[{"role":"user","content":"hi"}]}"#,
            )),
            trace_id: None,
            span_id: None,
        };
        let chat_response = ok_response(
            r#"{"id":"chatcmpl-1","object":"chat.completion","created":1,"model":"claude-3-5-sonnet","choices":[{"index":0,"message":{"role":"assistant","content":""},"finish_reason":"stop"}],"usage":{"prompt_tokens":20000,"completion_tokens":8,"total_tokens":20008,"prompt_tokens_details":{"cached_tokens":12000},"cache_read_input_tokens":12000,"cache_creation_input_tokens":3000,"cache_creation":{"ephemeral_1h_input_tokens":3000}}}"#,
        );
        let chat_parsed = parse_ai_response(&chat_request, &chat_response).unwrap();
        let chat = UsageMetrics::extract(
            Uuid::new_v4(),
            &chat_request,
            &chat_response,
            &chat_parsed,
            &crate::test::utils::create_test_config(),
        );

        assert_eq!(
            anthropic.prompt_tokens, chat.prompt_tokens,
            "the same conversation must report the same prompt_tokens on both ingresses"
        );
        assert_eq!(
            anthropic.prompt_tokens, 20000,
            "prompt_tokens must be TOTAL input (5000 uncached + 12000 read + 3000 written)"
        );
        assert_eq!(anthropic.completion_tokens, chat.completion_tokens);
        assert_eq!(anthropic.total_tokens, 20008);
        // The CACHE SPLIT must also survive both ingresses identically — the
        // batcher prices reads at the read multiplier and each creation tier at
        // its own write premium, so a bucket lost in translation bills wrong.
        assert_eq!(anthropic.cache_read_input_tokens, 12000);
        assert_eq!(anthropic.cache_read_input_tokens, chat.cache_read_input_tokens);
        assert_eq!(anthropic.cache_creation_1h_input_tokens, 3000);
        assert_eq!(anthropic.cache_creation_1h_input_tokens, chat.cache_creation_1h_input_tokens);
    }

    /// Same split, streaming. Anthropic splits usage across `message_start` and
    /// `message_delta`; the cache buckets must be folded in from whichever frame
    /// carries them.
    #[test]
    fn anthropic_stream_prompt_tokens_include_cached() {
        let request_data = messages_request_data(true);
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-3-5-sonnet\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":5000,\"output_tokens\":0,\"cache_read_input_tokens\":12000,\"cache_creation_input_tokens\":3000}}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":5000,\"output_tokens\":8,\"cache_read_input_tokens\":12000,\"cache_creation_input_tokens\":3000}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let response_data = ok_response(sse);
        let parsed = parse_ai_response(&request_data, &response_data).unwrap();
        let metrics = UsageMetrics::extract(
            Uuid::new_v4(),
            &request_data,
            &response_data,
            &parsed,
            &crate::test::utils::create_test_config(),
        );

        assert_eq!(metrics.prompt_tokens, 20000, "streaming must fold the cache buckets in too");
        assert_eq!(metrics.completion_tokens, 8);
        assert_eq!(metrics.total_tokens, 20008);
    }

    /// A response with no cache activity must be unchanged by the fix.
    #[test]
    fn anthropic_prompt_tokens_unchanged_without_caching() {
        let request_data = messages_request_data(false);
        let body = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-3-5-sonnet","content":[],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":12,"output_tokens":8}}"#;
        let response_data = ok_response(body);
        let parsed = parse_ai_response(&request_data, &response_data).unwrap();
        let metrics = UsageMetrics::extract(
            Uuid::new_v4(),
            &request_data,
            &response_data,
            &parsed,
            &crate::test::utils::create_test_config(),
        );
        assert_eq!(metrics.prompt_tokens, 12);
        assert_eq!(metrics.total_tokens, 20);
    }

    #[test]
    fn test_analytics_metrics_extract_anthropic_streaming_tokens() {
        // Anthropic splits usage: message_start carries input, message_delta carries the
        // final counts (the reframer puts both input and output there). Billing must sum them.
        let request_data = messages_request_data(true);
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-3-5-sonnet\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":12,\"output_tokens\":0}}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":12,\"output_tokens\":8}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let response_data = ok_response(sse);

        let parsed = parse_ai_response(&request_data, &response_data).unwrap();
        assert!(
            matches!(parsed, AiResponse::AnthropicStream(_)),
            "streaming /messages should parse as AnthropicStream"
        );

        let metrics = UsageMetrics::extract(
            Uuid::new_v4(),
            &request_data,
            &response_data,
            &parsed,
            &crate::test::utils::create_test_config(),
        );

        assert_eq!(metrics.response_model, Some("claude-3-5-sonnet".to_string()));
        assert_eq!(metrics.prompt_tokens, 12);
        assert_eq!(metrics.completion_tokens, 8);
        assert_eq!(metrics.total_tokens, 20);
        assert_eq!(metrics.response_type, "anthropic_message_stream");
        assert_eq!(metrics.status_code, 200);
    }

    #[test]
    fn test_anthropic_stream_error_event_reclassifies_to_500() {
        // A stream that opened 200 then emitted an `error` event is reclassified to 500,
        // matching native OpenAI streams, so it is excluded from success/credit accounting.
        let request_data = messages_request_data(true);
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-3-5-sonnet\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded\"}}\n\n",
        );
        let response_data = ok_response(sse);

        let parsed = parse_ai_response(&request_data, &response_data).unwrap();
        let metrics = UsageMetrics::extract(
            Uuid::new_v4(),
            &request_data,
            &response_data,
            &parsed,
            &crate::test::utils::create_test_config(),
        );

        assert_eq!(metrics.status_code, 500);
    }

    fn response_with_body(body: impl Into<Bytes>) -> ResponseData {
        ResponseData {
            extensions: Default::default(),
            correlation_id: 1,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: Some(body.into()),
            duration: Duration::from_millis(1),
            duration_to_first_byte: Duration::from_millis(1),
        }
    }

    #[test]
    fn extract_cache_tokens_non_streaming() {
        let body = serde_json::json!({
            "usage": {
                "prompt_tokens": 2000, "completion_tokens": 5,
                "cache_read_input_tokens": 1000,
                "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 500, "ephemeral_24h_input_tokens": 0},
                "prompt_tokens_details": {"cached_tokens": 1000}
            }
        })
        .to_string();
        let c = extract_cache_tokens(&response_with_body(body));
        assert_eq!(c.read, 1000);
        assert_eq!(c.creation_1h, 500);
        assert_eq!(c.creation_5m, 0);
        assert_eq!(c.creation_24h, 0);
    }

    #[test]
    fn extract_cache_tokens_streaming_terminal_frame() {
        // Only the terminal usage frame carries the split; deltas + [DONE] are ignored.
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                   data: {\"choices\":[],\"usage\":{\"prompt_tokens\":2000,\"cache_read_input_tokens\":1500,\"cache_creation\":{\"ephemeral_24h_input_tokens\":1500}}}\n\n\
                   data: [DONE]\n\n";
        let c = extract_cache_tokens(&response_with_body(sse));
        assert_eq!(c.read, 1500);
        assert_eq!(c.creation_24h, 1500);
        assert_eq!(c.creation_1h, 0);
    }

    #[test]
    fn extract_cache_tokens_ignores_provider_native_cached_tokens() {
        // Only the provider's own `prompt_tokens_details.cached_tokens` is present (e.g. an
        // OpenAI-backed model dwctl is NOT caching). It must NOT be attributed to dwctl's
        // cache billing — reads come solely from the `cache_read_input_tokens` our layer sets.
        let body = serde_json::json!({
            "usage": {"prompt_tokens": 100, "completion_tokens": 2, "prompt_tokens_details": {"cached_tokens": 64}}
        })
        .to_string();
        let c = extract_cache_tokens(&response_with_body(body));
        assert_eq!(c.read, 0, "provider-native cached_tokens is not a dwctl cache read");
    }

    #[test]
    fn extract_cache_tokens_absent_is_zero() {
        // A plain response (no cache fields) and an error body both extract zero.
        let plain = serde_json::json!({"usage": {"prompt_tokens": 10, "completion_tokens": 2}}).to_string();
        let c = extract_cache_tokens(&response_with_body(plain));
        assert_eq!(c.read, 0);
        assert_eq!(c.creation_1h, 0);

        let err = serde_json::json!({"error": {"message": "bad"}}).to_string();
        let c = extract_cache_tokens(&response_with_body(err));
        assert_eq!(c.read, 0);
    }

    // ---- finish_reason extraction ----------------------------------------------------
    //
    // `finish_reason = 'tool_calls'` is the only signal that a caller is running a
    // client-side tool loop, so a silent None here means that whole class of usage goes
    // unmeasured. These cover the shapes that would produce one.

    /// Build a POST /v1/chat/completions request + response pair from a raw body.
    ///
    /// `stream` has to be declared on the REQUEST: parse_ai_response picks the SSE parser
    /// from the request's `stream: true` (or the x-fusillade-stream header), not by sniffing
    /// the response. Getting this wrong makes an SSE body fail to parse rather than
    /// producing a wrong finish_reason, which is how the first draft of these tests failed.
    fn chat_pair(body: &'static str, stream: bool) -> (RequestData, ResponseData) {
        let request_json = if stream {
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"stream":true}"#
        } else {
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#
        };
        let request_data = RequestData {
            correlation_id: 1,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/v1/chat/completions".parse::<Uri>().unwrap(),
            headers: HashMap::new(),
            body: Some(Bytes::from(request_json)),
            trace_id: None,
            span_id: None,
        };
        let response_data = ResponseData {
            extensions: Default::default(),
            correlation_id: 1,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers: HashMap::new(),
            body: Some(Bytes::from(body)),
            duration: Duration::from_millis(10),
            duration_to_first_byte: Duration::from_millis(5),
        };
        (request_data, response_data)
    }

    /// Mirrors what analytics_handler does with an unparseable body: fall back to
    /// `AiResponse::Other` rather than propagating the error, so extraction is still
    /// exercised on the shape production would actually hand it.
    fn finish_reason_of(body: &'static str, stream: bool) -> Option<String> {
        let (req, res) = chat_pair(body, stream);
        let parsed = parse_ai_response(&req, &res).unwrap_or(AiResponse::Other(serde_json::Value::Null));
        extract_finish_reason(&parsed)
    }

    #[test]
    fn finish_reason_from_non_streamed_chat_completion() {
        let body = r#"{"id":"c1","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"t1","type":"function","function":{"name":"f","arguments":"{}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
        assert_eq!(finish_reason_of(body, false).as_deref(), Some("tool_calls"));
    }

    /// THE case this function is scanned independently of usage for.
    ///
    /// OpenAI's convention is a terminal usage chunk carrying `choices: []`, so the chunk
    /// `TokenMetrics` selects (the last one WITH usage) has no finish_reason at all — it is
    /// on the penultimate chunk. Reading finish_reason off the usage chunk, which is the
    /// obvious implementation, returns None here and silently loses every tool_calls signal
    /// from any provider following that convention.
    #[test]
    fn finish_reason_survives_a_terminal_usage_chunk_with_no_choices() {
        let body = concat!(
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"tool_calls\"}],\"usage\":null}\n\n",
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        assert_eq!(finish_reason_of(body, true).as_deref(), Some("tool_calls"));
    }

    /// The other half of the same problem: a provider that omits usage entirely on a
    /// tool_calls finish (self-hosted GLM-5.2 does exactly this). There is no chunk with
    /// usage to read from, so a usage-anchored implementation has nothing at all.
    #[test]
    fn finish_reason_present_when_no_chunk_carries_usage() {
        let body = concat!(
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        assert_eq!(finish_reason_of(body, true).as_deref(), Some("tool_calls"));
    }

    /// Scanning in reverse must not stop at the [DONE] marker or a trailing error frame —
    /// neither is a Normal chunk, and both appear after the one carrying the value.
    #[test]
    fn finish_reason_skips_done_and_error_frames() {
        let body = concat!(
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"length\"}]}\n\n",
            "data: {\"error\":{\"message\":\"boom\",\"type\":\"internal_server_error\",\"code\":500}}\n\n",
            "data: [DONE]\n\n",
        );
        assert_eq!(finish_reason_of(body, true).as_deref(), Some("length"));
    }

    /// A stream that never finished (client hung up, upstream died) has no finish_reason.
    /// None must mean "not stated", never a fabricated 'stop'.
    #[test]
    fn finish_reason_none_when_stream_never_terminated() {
        let body = concat!(
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
        );
        assert_eq!(finish_reason_of(body, true), None);
    }

    #[test]
    fn finish_reason_none_for_shapes_without_the_concept() {
        // Embeddings have no finish_reason, and neither does an unparseable body. Both must
        // come back None rather than picking something up out of the JSON by accident.
        let embeddings = r#"{"object":"list","model":"m","data":[{"object":"embedding","index":0,"embedding":[0.1]}],"usage":{"prompt_tokens":1,"total_tokens":1}}"#;
        assert_eq!(finish_reason_of(embeddings, false), None);
        assert_eq!(finish_reason_of("not json at all", false), None);
    }

    #[test]
    fn finish_reason_round_trips_through_usage_metrics() {
        // The column is only useful if it survives the hop onto UsageMetrics, which is what
        // analytics_handler copies onto RawAnalyticsRecord.
        let body = r#"{"id":"c1","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"x"},"finish_reason":"length"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
        let (req, res) = chat_pair(body, false);
        let parsed = parse_ai_response(&req, &res).unwrap();
        let metrics = UsageMetrics::extract(Uuid::nil(), &req, &res, &parsed, &crate::config::Config::default());
        assert_eq!(metrics.finish_reason.as_deref(), Some("length"));
    }
}
