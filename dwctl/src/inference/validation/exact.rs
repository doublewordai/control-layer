//! Stage-2 exact prompt token count via tokenizer-svc.
//!
//! Stage 1 ([`super::rules`]) rejects only when the prompt is *provably* over budget
//! from its UTF-8 byte length. A near-limit prompt needs a real count: this module
//! asks tokenizer-svc for one, on a hard deadline, and reports it as an
//! [`ExactOutcome`]. Every non-[`ExactOutcome::Counted`] result is a pass for the
//! caller — counting infrastructure must never turn a valid request into a rejection.
//!
//! Surfaces:
//! - **Chat Completions**: `/v1/render` over the transmitted `messages` + `tools`.
//! - **Responses** and **Messages**: run through the same pure edge translators the
//!   request path uses, then rendered.
//! - **Completions** / **Embeddings**: `/v1/tokenize` over the prompt/input strings,
//!   plus a direct count of any token-id arrays in the payload.
//!
//! Completions/Embeddings input is an *array of independent sequences*: the engine
//! processes each element on its own, so every sequence must fit the window by
//! itself. The reported count is therefore the **largest single sequence**, not the
//! sum. The accepted shapes (matching OpenAI's `prompt`/`input`) are: a bare string
//! (one sequence); an array of strings (one sequence per string); an array of token
//! ids (one sequence of that many ids); and an array of token-id arrays (one sequence
//! per inner array). Nested string arrays flatten to one sequence per string.
//! Per-string counts come back as `segment_counts` from a single `/v1/tokenize` call.
//!
//! When `/v1/render` cannot template an alias or conversation
//! ([`TokenizerError::RenderUnsupported`]) we fall back to tokenizing the message
//! text segments. Chat messages are a *single* conversation, so that fallback sums the
//! segments. It is a **lower bound** on the rendered prompt — it omits the template's
//! own scaffolding tokens — so it stays safe to reject on: a rejection is only made
//! when even this lower bound already exceeds the limit.
//!
//! A small circuit breaker short-circuits counting for [`BREAKER_COOLDOWN`] after a
//! timeout or transport failure, so a slow or down tokenizer-svc cannot impose one
//! deadline per request (or per near-limit batch line).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use metrics::{counter, histogram};
use serde_json::Value;

use crate::inference::translation::anthropic::model::MessagesRequest;
use crate::inference::translation::anthropic::request::to_chat_completions;
use crate::inference::translation::responses::request::to_chat_request;
use crate::inference::translation::responses::types::ResponsesRequest;
use crate::prompt_cache::tokenizer::{TokenizeResponse, TokenizerClient, TokenizerError};

use super::Surface;

/// Unmapped aliases are a stable fact for this long; re-probing sooner only buys a
/// failed round-trip per request for a model tokenizer-svc cannot map.
const UNMAPPED_TTL: Duration = Duration::from_secs(300);

/// `ExactOutcome::Unsupported` reason: tokenizer-svc has no tokenizer for the alias.
const UNMAPPED_MODEL: &str = "unmapped_model";
/// `ExactOutcome::Unsupported` reason: the foreign request could not be translated.
const CONVERSION_FAILED: &str = "conversion_failed";
/// `ExactOutcome::Unsupported` reason: the prompt/input shape is not countable.
const UNSUPPORTED_PROMPT: &str = "unsupported_prompt";
/// `ExactOutcome::Unavailable` reason: transport / 5xx / malformed response.
const TOKENIZER_UNAVAILABLE: &str = "tokenizer_unavailable";
/// `ExactOutcome::Unavailable` reason: the exact-count circuit breaker is open, so we
/// skip tokenizer-svc entirely until the cooldown elapses.
const CIRCUIT_OPEN: &str = "tokenizer_circuit_open";

/// How long the circuit breaker stays open after a timeout or transport failure. Kept
/// deliberately short: exact counting is an optimization, not a dependency.
const BREAKER_COOLDOWN: Duration = Duration::from_secs(30);

/// Stage-2 exact count. Deadline-bounded and cheap to clone.
///
/// Clones share the breaker and the unmapped-alias memo.
#[derive(Clone)]
pub struct ExactCounter {
    tokenizer: Arc<TokenizerClient>,
    deadline: Duration,
    /// Aliases tokenizer-svc has reported it cannot map, memoised for [`UNMAPPED_TTL`].
    unmapped: moka::future::Cache<String, ()>,
    /// Shared failure breaker; see [`BREAKER_COOLDOWN`].
    breaker: Arc<Breaker>,
}

/// Result of an exact count attempt.
///
/// Only [`ExactOutcome::Counted`] may feed a rejection; every other variant means
/// "could not decide" and MUST be treated as a pass (fail open).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExactOutcome {
    /// Exact prompt token count.
    Counted(u64),
    /// The count cannot be produced for this request (unmapped model, failed
    /// translation, unparseable prompt). `&'static str` is a low-cardinality reason.
    Unsupported(&'static str),
    /// tokenizer-svc is currently unreachable or misbehaving.
    Unavailable(&'static str),
    /// The call exceeded [`ExactCounter`]'s deadline.
    TimedOut,
}

impl ExactCounter {
    pub fn new(tokenizer: Arc<TokenizerClient>, deadline: Duration) -> Self {
        let unmapped = moka::future::Cache::builder()
            .max_capacity(10_000)
            .time_to_live(UNMAPPED_TTL)
            .build();
        Self {
            tokenizer,
            deadline,
            unmapped,
            breaker: Arc::new(Breaker::new()),
        }
    }

    /// Build a counter around a fresh client pointed at `base_url`.
    pub fn from_base_url(base_url: impl Into<String>, deadline: Duration) -> Self {
        Self::new(Arc::new(TokenizerClient::new(base_url)), deadline)
    }

    /// Count the prompt tokens for one inference request.
    ///
    /// The whole attempt runs under the configured deadline; on expiry the inner
    /// future is dropped and [`ExactOutcome::TimedOut`] is returned.
    pub async fn prompt_tokens(&self, model: &str, surface: Surface, body: &Value) -> ExactOutcome {
        let start = Instant::now();
        // Short-circuit while the breaker is open: no tokenizer call and no deadline wait.
        let outcome = if self.breaker.is_open() {
            ExactOutcome::Unavailable(CIRCUIT_OPEN)
        } else {
            let outcome = match tokio::time::timeout(self.deadline, self.count(model, surface, body)).await {
                Ok(outcome) => outcome,
                Err(_) => ExactOutcome::TimedOut,
            };
            // Infrastructure failures open the breaker; a real count or an
            // "unsupported" answer means tokenizer-svc is reachable.
            if matches!(outcome, ExactOutcome::TimedOut | ExactOutcome::Unavailable(_)) {
                self.breaker.open();
            }
            outcome
        };
        let label = match outcome {
            ExactOutcome::Counted(_) => "counted",
            ExactOutcome::Unsupported(_) => "unsupported",
            ExactOutcome::Unavailable(reason) if reason == CIRCUIT_OPEN => "circuit_open",
            ExactOutcome::Unavailable(_) => "unavailable",
            ExactOutcome::TimedOut => "timed_out",
        };
        counter!("dwctl_request_validation_exact_count_total", "outcome" => label).increment(1);
        histogram!("dwctl_request_validation_exact_count_seconds").record(start.elapsed().as_secs_f64());
        outcome
    }

    async fn count(&self, model: &str, surface: Surface, body: &Value) -> ExactOutcome {
        if self.unmapped.get(model).await.is_some() {
            return ExactOutcome::Unsupported(UNMAPPED_MODEL);
        }
        match surface {
            Surface::ChatCompletions => self.render_chat(model, body).await,
            Surface::Responses => match responses_to_chat(body) {
                Some(chat) => self.render_chat(model, &chat).await,
                None => ExactOutcome::Unsupported(CONVERSION_FAILED),
            },
            Surface::Messages => match anthropic_to_chat(body) {
                Some(chat) => self.render_chat(model, &chat).await,
                None => ExactOutcome::Unsupported(CONVERSION_FAILED),
            },
            Surface::Completions => self.tokenize_prompt(model, body, "prompt").await,
            Surface::Embeddings => self.tokenize_prompt(model, body, "input").await,
        }
    }

    /// Exact chat-templated count. `chat` is the canonical Chat Completions body
    /// (native, or translated from Responses/Messages).
    async fn render_chat(&self, model: &str, chat: &Value) -> ExactOutcome {
        let messages = chat.get("messages").cloned().unwrap_or_else(|| Value::Array(Vec::new()));
        let tools = chat.get("tools").filter(|v| !v.is_null());
        match self.tokenizer.render(model, &messages, tools, true, &[]).await {
            Ok(resp) => ExactOutcome::Counted(u64::from(resp.total)),
            Err(TokenizerError::Unmapped(_)) => {
                self.memoize_unmapped(model).await;
                ExactOutcome::Unsupported(UNMAPPED_MODEL)
            }
            Err(TokenizerError::RenderUnsupported(..)) => {
                // The template refused this view (NO_CHAT_TEMPLATE / render failed):
                // count the message text only. See the module docs — a lower bound is
                // safe because it can only under-count, and we reject only when an
                // under-count already exceeds the budget.
                let segments = message_text_segments(&messages);
                self.count_message_text(model, &segments).await
            }
            Err(TokenizerError::Http(_) | TokenizerError::Status { .. }) => ExactOutcome::Unavailable(TOKENIZER_UNAVAILABLE),
        }
    }

    /// Tokenize the prompt/input strings for Completions/Embeddings. The input is a set
    /// of independent sequences, so the result is the **largest single sequence** (text
    /// strings via `/v1/tokenize`, token-id arrays counted directly).
    async fn tokenize_prompt(&self, model: &str, body: &Value, field: &str) -> ExactOutcome {
        let Some(value) = body.get(field) else {
            return ExactOutcome::Unsupported(UNSUPPORTED_PROMPT);
        };
        let mut prompt = PromptSequences::default();
        if extract_sequences(value, &mut prompt).is_none() {
            return ExactOutcome::Unsupported(UNSUPPORTED_PROMPT);
        }
        let ids_max = prompt.token_ids.iter().copied().max().unwrap_or(0);
        if prompt.texts.is_empty() {
            // Nothing for tokenizer-svc to do; token-id sequences are already exact.
            return ExactOutcome::Counted(ids_max);
        }
        match self.tokenize_texts(model, &prompt.texts).await {
            Ok(resp) => {
                // One count per text, in request order. Any other length means we could
                // not map counts back to sequences safely, so fail open.
                if resp.segment_counts.len() != prompt.texts.len() {
                    return ExactOutcome::Unsupported(UNSUPPORTED_PROMPT);
                }
                let text_max = resp.segment_counts.iter().copied().max().map(u64::from).unwrap_or(0);
                ExactOutcome::Counted(text_max.max(ids_max))
            }
            Err(outcome) => outcome,
        }
    }

    /// Sum the text of a single conversation (the render-unsupported fallback).
    async fn count_message_text(&self, model: &str, segments: &[String]) -> ExactOutcome {
        if segments.is_empty() {
            return ExactOutcome::Counted(0);
        }
        match self.tokenize_texts(model, segments).await {
            Ok(resp) => ExactOutcome::Counted(u64::from(resp.total)),
            Err(outcome) => outcome,
        }
    }

    /// One `/v1/tokenize` call for a batch of text segments. Errors are already mapped
    /// to a fail-open [`ExactOutcome`].
    async fn tokenize_texts(&self, model: &str, segments: &[String]) -> Result<TokenizeResponse, ExactOutcome> {
        match self.tokenizer.tokenize(model, segments).await {
            Ok(resp) => Ok(resp),
            Err(TokenizerError::Unmapped(_)) => {
                self.memoize_unmapped(model).await;
                Err(ExactOutcome::Unsupported(UNMAPPED_MODEL))
            }
            Err(TokenizerError::RenderUnsupported(..) | TokenizerError::Http(_) | TokenizerError::Status { .. }) => {
                Err(ExactOutcome::Unavailable(TOKENIZER_UNAVAILABLE))
            }
        }
    }

    async fn memoize_unmapped(&self, model: &str) {
        self.unmapped.insert(model.to_string(), ()).await;
    }
}

/// Time-based circuit breaker shared by every clone of an [`ExactCounter`].
///
/// Deliberately lock-free: the hot path is one relaxed load. `open_until_ms` is a
/// monotonic deadline relative to `epoch`; `0` means closed.
struct Breaker {
    epoch: Instant,
    open_until_ms: AtomicU64,
}

impl Breaker {
    fn new() -> Self {
        Self {
            epoch: Instant::now(),
            open_until_ms: AtomicU64::new(0),
        }
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    fn is_open(&self) -> bool {
        self.now_ms() < self.open_until_ms.load(Ordering::Relaxed)
    }

    fn open(&self) {
        let cooldown_ms = u64::try_from(BREAKER_COOLDOWN.as_millis()).unwrap_or(u64::MAX);
        self.open_until_ms
            .store(self.now_ms().saturating_add(cooldown_ms), Ordering::Relaxed);
    }
}

/// Responses -> Chat Completions -> JSON, or `None` if the body is malformed.
fn responses_to_chat(body: &Value) -> Option<Value> {
    let req: ResponsesRequest = serde_json::from_value(body.clone()).ok()?;
    serde_json::to_value(to_chat_request(&req)).ok()
}

/// Anthropic Messages -> Chat Completions -> JSON, or `None` if the body is malformed.
///
/// `cache_enabled = false` strips the internal cache markers the translator would
/// otherwise emit: the cache layer removes them before the upstream call either way,
/// so the engine view (what we are counting) carries none.
fn anthropic_to_chat(body: &Value) -> Option<Value> {
    let req: MessagesRequest = serde_json::from_value(body.clone()).ok()?;
    to_chat_completions(req, false).ok()
}

/// The independently-processed sequences in an OpenAI `prompt` (`Completions`) or
/// `input` (`Embeddings`) field. Each text is one sequence; each token-id entry is the
/// length of one sequence. The engine processes every sequence separately, so the
/// eventual count is the maximum across them, not the sum.
#[derive(Default)]
struct PromptSequences {
    texts: Vec<String>,
    token_ids: Vec<u64>,
}

/// Split a `prompt`/`input` value into sequences. Returns `None` for a shape we cannot
/// count, and the caller fails open.
///
/// A non-empty array whose elements are all non-negative integers is one token-id
/// sequence, not one sequence per id. Any other array is walked recursively, so an
/// array of token-id arrays yields one id sequence per inner array and a nested array
/// of strings yields one text sequence per string.
fn extract_sequences(value: &Value, out: &mut PromptSequences) -> Option<()> {
    match value {
        Value::String(text) => out.texts.push(text.clone()),
        Value::Array(items) => {
            if !items.is_empty() && items.iter().all(|item| item.as_u64().is_some()) {
                out.token_ids.push(items.len() as u64);
            } else {
                for item in items {
                    extract_sequences(item, out)?;
                }
            }
        }
        _ => return None,
    }
    Some(())
}

/// The message text segments used for the render-unsupported fallback. Deliberately
/// text-only: tool definitions, role scaffolding and template tokens are omitted, so
/// the result is a lower bound on the rendered prompt (see the module docs).
fn message_text_segments(messages: &Value) -> Vec<String> {
    let mut segments = Vec::new();
    let Some(messages) = messages.as_array() else {
        return segments;
    };
    for message in messages {
        match message.get("content") {
            Some(Value::String(text)) if !text.is_empty() => segments.push(text.clone()),
            Some(Value::Array(parts)) => {
                for part in parts {
                    if let Some(text) = part.get("text").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        segments.push(text.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn counter(base_url: String, deadline: Duration) -> ExactCounter {
        ExactCounter::new(Arc::new(TokenizerClient::new(base_url)), deadline)
    }

    fn chat_body() -> Value {
        serde_json::json!({"messages": [{"role": "user", "content": "hello"}]})
    }

    fn render_ok(total: u32) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "virtual_model": "m",
            "tokenizer_version": "v1",
            "template_version": "t1",
            "total": total,
            "prefix_counts": []
        }))
    }

    #[tokio::test]
    async fn chat_render_counts_exact_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/render"))
            .and(body_partial_json(serde_json::json!({
                "messages": [{"role": "user", "content": "hello"}],
                "add_generation_prompt": true,
            })))
            .respond_with(render_ok(42))
            .mount(&server)
            .await;

        let counter = counter(server.uri(), Duration::from_secs(1));
        let out = counter.prompt_tokens("m", Surface::ChatCompletions, &chat_body()).await;
        assert_eq!(out, ExactOutcome::Counted(42));
    }

    #[tokio::test]
    async fn render_unsupported_falls_back_to_message_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/render"))
            .respond_with(ResponseTemplate::new(422).set_body_string(r#"{"code":"NO_CHAT_TEMPLATE"}"#))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/tokenize"))
            .and(body_partial_json(serde_json::json!({"segments": ["hello"]})))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "virtual_model": "m",
                "tokenizer_version": "v1",
                "segment_counts": [7],
                "cumulative": [7],
                "total": 7
            })))
            .mount(&server)
            .await;

        let counter = counter(server.uri(), Duration::from_secs(1));
        let out = counter.prompt_tokens("m", Surface::ChatCompletions, &chat_body()).await;
        assert_eq!(out, ExactOutcome::Counted(7));
    }

    #[tokio::test]
    async fn unmapped_model_is_memoized() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/render"))
            .respond_with(ResponseTemplate::new(422).set_body_string(r#"{"code":"UNMAPPED_MODEL"}"#))
            .mount(&server)
            .await;

        let counter = counter(server.uri(), Duration::from_secs(1));
        let body = chat_body();
        assert_eq!(
            counter.prompt_tokens("ghost", Surface::ChatCompletions, &body).await,
            ExactOutcome::Unsupported(UNMAPPED_MODEL)
        );
        // The alias is now memoised: the second call must not reach tokenizer-svc.
        assert_eq!(
            counter.prompt_tokens("ghost", Surface::ChatCompletions, &body).await,
            ExactOutcome::Unsupported(UNMAPPED_MODEL)
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "second call must not hit tokenizer-svc");
    }

    #[tokio::test]
    async fn server_error_is_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/render"))
            .respond_with(ResponseTemplate::new(503).set_body_string("overloaded"))
            .mount(&server)
            .await;

        let counter = counter(server.uri(), Duration::from_secs(1));
        let out = counter.prompt_tokens("m", Surface::ChatCompletions, &chat_body()).await;
        assert_eq!(out, ExactOutcome::Unavailable(TOKENIZER_UNAVAILABLE));
    }

    #[tokio::test]
    async fn slow_server_times_out() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/render"))
            .respond_with(render_ok(42).set_delay(Duration::from_millis(500)))
            .mount(&server)
            .await;

        let counter = counter(server.uri(), Duration::from_millis(50));
        let out = counter.prompt_tokens("m", Surface::ChatCompletions, &chat_body()).await;
        assert_eq!(out, ExactOutcome::TimedOut);
    }

    #[tokio::test]
    async fn anthropic_messages_render_the_translated_chat() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/render"))
            .and(body_partial_json(serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "add_generation_prompt": true,
            })))
            .respond_with(render_ok(11))
            .mount(&server)
            .await;

        let counter = ExactCounter::from_base_url(server.uri(), Duration::from_secs(1));
        let body = serde_json::json!({
            "model": "claude",
            "max_tokens": 10,
            "messages": [{"role": "user", "content": "hi"}],
        });
        let out = counter.prompt_tokens("m", Surface::Messages, &body).await;
        assert_eq!(out, ExactOutcome::Counted(11));
    }

    #[tokio::test]
    async fn responses_render_the_translated_chat() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/render"))
            .and(body_partial_json(serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "add_generation_prompt": true,
            })))
            .respond_with(render_ok(13))
            .mount(&server)
            .await;

        let counter = counter(server.uri(), Duration::from_secs(1));
        let body = serde_json::json!({"model": "gpt", "input": "hi"});
        let out = counter.prompt_tokens("m", Surface::Responses, &body).await;
        assert_eq!(out, ExactOutcome::Counted(13));
    }

    #[tokio::test]
    async fn malformed_foreign_body_is_unsupported() {
        let server = MockServer::start().await;
        // No mock mounted: an HTTP call here would be a bug, so any request is a 404.
        let counter = counter(server.uri(), Duration::from_secs(1));
        // Missing required `max_tokens`.
        let body = serde_json::json!({"model": "claude", "messages": []});
        assert_eq!(
            counter.prompt_tokens("m", Surface::Messages, &body).await,
            ExactOutcome::Unsupported(CONVERSION_FAILED)
        );
        let body = serde_json::json!({"model": "gpt"});
        assert_eq!(
            counter.prompt_tokens("m", Surface::Responses, &body).await,
            ExactOutcome::Unsupported(CONVERSION_FAILED)
        );
    }

    #[tokio::test]
    async fn completions_tokenize_prompt_string() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tokenize"))
            .and(body_partial_json(serde_json::json!({"segments": ["count me"]})))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "virtual_model": "m",
                "tokenizer_version": "v1",
                "segment_counts": [5],
                "cumulative": [5],
                "total": 5
            })))
            .mount(&server)
            .await;

        let counter = counter(server.uri(), Duration::from_secs(1));
        let body = serde_json::json!({"model": "m", "prompt": "count me"});
        let out = counter.prompt_tokens("m", Surface::Completions, &body).await;
        assert_eq!(out, ExactOutcome::Counted(5));
    }

    #[tokio::test]
    async fn embeddings_count_token_ids_directly() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tokenize"))
            .and(body_partial_json(serde_json::json!({"segments": ["text"]})))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "virtual_model": "m",
                "tokenizer_version": "v1",
                "segment_counts": [4],
                "cumulative": [4],
                "total": 4
            })))
            .mount(&server)
            .await;

        let counter = counter(server.uri(), Duration::from_secs(1));
        // A 4-token string and a 3-id token array are separate sequences; the larger
        // one (4) bounds the request.
        let body = serde_json::json!({"model": "m", "input": ["text", [1, 2, 3]]});
        let out = counter.prompt_tokens("m", Surface::Embeddings, &body).await;
        assert_eq!(out, ExactOutcome::Counted(4));
    }

    #[tokio::test]
    async fn embeddings_string_array_uses_largest_sequence() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tokenize"))
            .and(body_partial_json(serde_json::json!({"segments": ["short", "much longer"]})))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "virtual_model": "m",
                "tokenizer_version": "v1",
                "segment_counts": [2, 9],
                "cumulative": [2, 11],
                "total": 11
            })))
            .mount(&server)
            .await;

        let counter = counter(server.uri(), Duration::from_secs(1));
        // Two sequences that each fit but whose sum does not. Only the max counts.
        let body = serde_json::json!({"model": "m", "input": ["short", "much longer"]});
        let out = counter.prompt_tokens("m", Surface::Embeddings, &body).await;
        assert_eq!(out, ExactOutcome::Counted(9));
    }

    #[tokio::test]
    async fn embeddings_token_id_arrays_use_largest_sequence() {
        let server = MockServer::start().await;
        // No mock: token-id-only inputs must not call tokenizer-svc.
        let counter = counter(server.uri(), Duration::from_secs(1));
        // Each inner array is one sequence; the longest is 3 ids.
        let body = serde_json::json!({"model": "m", "input": [[1, 2, 3], [4, 5]]});
        let out = counter.prompt_tokens("m", Surface::Embeddings, &body).await;
        assert_eq!(out, ExactOutcome::Counted(3));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn completions_string_array_uses_largest_sequence() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tokenize"))
            .and(body_partial_json(serde_json::json!({"segments": ["a", "bbbb"]})))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "virtual_model": "m",
                "tokenizer_version": "v1",
                "segment_counts": [1, 6],
                "cumulative": [1, 7],
                "total": 7
            })))
            .mount(&server)
            .await;

        let counter = counter(server.uri(), Duration::from_secs(1));
        let body = serde_json::json!({"model": "m", "prompt": ["a", "bbbb"]});
        let out = counter.prompt_tokens("m", Surface::Completions, &body).await;
        assert_eq!(out, ExactOutcome::Counted(6));
    }

    #[tokio::test]
    async fn token_id_only_input_skips_the_tokenizer() {
        let server = MockServer::start().await;
        // No mock: a token-id-only payload must not call tokenizer-svc.
        let counter = counter(server.uri(), Duration::from_secs(1));
        let body = serde_json::json!({"model": "m", "input": [1, 2, 3, 4]});
        let out = counter.prompt_tokens("m", Surface::Embeddings, &body).await;
        assert_eq!(out, ExactOutcome::Counted(4));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn unsupported_prompt_shape_fails_open() {
        let server = MockServer::start().await;
        let counter = counter(server.uri(), Duration::from_secs(1));
        let body = serde_json::json!({"model": "m", "prompt": 12});
        assert_eq!(
            counter.prompt_tokens("m", Surface::Completions, &body).await,
            ExactOutcome::Unsupported(UNSUPPORTED_PROMPT)
        );
    }

    #[tokio::test]
    async fn breaker_opens_after_timeout_and_short_circuits() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/render"))
            .respond_with(render_ok(42).set_delay(Duration::from_millis(500)))
            .mount(&server)
            .await;

        let counter = counter(server.uri(), Duration::from_millis(50));
        assert_eq!(
            counter.prompt_tokens("m", Surface::ChatCompletions, &chat_body()).await,
            ExactOutcome::TimedOut
        );
        // The breaker is now open: the next call returns immediately without a request.
        assert_eq!(
            counter.prompt_tokens("m", Surface::ChatCompletions, &chat_body()).await,
            ExactOutcome::Unavailable(CIRCUIT_OPEN)
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn breaker_state_is_shared_by_clones() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/render"))
            .respond_with(render_ok(42).set_delay(Duration::from_millis(500)))
            .mount(&server)
            .await;

        let counter = counter(server.uri(), Duration::from_millis(50));
        let clone = counter.clone();
        assert_eq!(
            counter.prompt_tokens("m", Surface::ChatCompletions, &chat_body()).await,
            ExactOutcome::TimedOut
        );
        assert_eq!(
            clone.prompt_tokens("m", Surface::ChatCompletions, &chat_body()).await,
            ExactOutcome::Unavailable(CIRCUIT_OPEN)
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
