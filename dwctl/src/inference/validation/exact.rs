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
//! When `/v1/render` cannot template an alias or conversation
//! ([`TokenizerError::RenderUnsupported`]) we fall back to tokenizing the message
//! text segments. That is a **lower bound** on the rendered prompt — it omits the
//! template's own scaffolding tokens — so it stays safe to reject on: a rejection is
//! only made when even this lower bound already exceeds the limit.

use std::sync::Arc;
use std::time::{Duration, Instant};

use metrics::{counter, histogram};
use serde_json::Value;

use crate::inference::translation::anthropic::model::MessagesRequest;
use crate::inference::translation::anthropic::request::to_chat_completions;
use crate::inference::translation::responses::request::to_chat_request;
use crate::inference::translation::responses::types::ResponsesRequest;
use crate::prompt_cache::tokenizer::{TokenizerClient, TokenizerError};

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

/// Stage-2 exact count. Deadline-bounded and cheap to clone.
#[derive(Clone)]
pub struct ExactCounter {
    tokenizer: Arc<TokenizerClient>,
    deadline: Duration,
    /// Aliases tokenizer-svc has reported it cannot map, memoised for [`UNMAPPED_TTL`].
    unmapped: moka::future::Cache<String, ()>,
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
        let outcome = match tokio::time::timeout(self.deadline, self.count(model, surface, body)).await {
            Ok(outcome) => outcome,
            Err(_) => ExactOutcome::TimedOut,
        };
        let label = match outcome {
            ExactOutcome::Counted(_) => "counted",
            ExactOutcome::Unsupported(_) => "unsupported",
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
                self.tokenize_segments(model, &segments, 0).await
            }
            Err(TokenizerError::Http(_) | TokenizerError::Status { .. }) => ExactOutcome::Unavailable(TOKENIZER_UNAVAILABLE),
        }
    }

    /// Tokenize the prompt/input strings for Completions/Embeddings, adding a direct
    /// count for any token-id arrays (which need no round-trip).
    async fn tokenize_prompt(&self, model: &str, body: &Value, field: &str) -> ExactOutcome {
        let Some(value) = body.get(field) else {
            return ExactOutcome::Unsupported(UNSUPPORTED_PROMPT);
        };
        let Some(prompt) = extract_prompt(value) else {
            return ExactOutcome::Unsupported(UNSUPPORTED_PROMPT);
        };
        self.tokenize_segments(model, &prompt.segments, prompt.direct_ids).await
    }

    async fn tokenize_segments(&self, model: &str, segments: &[String], direct_ids: u64) -> ExactOutcome {
        if segments.is_empty() {
            // Nothing for tokenizer-svc to do; token ids (or nothing) are already exact.
            return ExactOutcome::Counted(direct_ids);
        }
        match self.tokenizer.tokenize(model, segments).await {
            Ok(resp) => ExactOutcome::Counted(direct_ids + u64::from(resp.total)),
            Err(TokenizerError::Unmapped(_)) => {
                self.memoize_unmapped(model).await;
                ExactOutcome::Unsupported(UNMAPPED_MODEL)
            }
            Err(TokenizerError::RenderUnsupported(..) | TokenizerError::Http(_) | TokenizerError::Status { .. }) => {
                ExactOutcome::Unavailable(TOKENIZER_UNAVAILABLE)
            }
        }
    }

    async fn memoize_unmapped(&self, model: &str) {
        self.unmapped.insert(model.to_string(), ()).await;
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

/// Text segments plus a direct token-id count extracted from an OpenAI `prompt`
/// (`Completions`) or `input` (`Embeddings`) field. `None` for a shape we cannot
/// count (the caller fails open).
#[derive(Default)]
struct PromptText {
    segments: Vec<String>,
    direct_ids: u64,
}

fn extract_prompt(value: &Value) -> Option<PromptText> {
    let mut out = PromptText::default();
    match value {
        Value::String(s) => out.segments.push(s.clone()),
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::String(s) => out.segments.push(s.clone()),
                    Value::Number(n) if n.as_u64().is_some() => out.direct_ids += 1,
                    Value::Array(inner) => {
                        for entry in inner {
                            match entry {
                                Value::String(s) => out.segments.push(s.clone()),
                                Value::Number(n) if n.as_u64().is_some() => out.direct_ids += 1,
                                _ => return None,
                            }
                        }
                    }
                    _ => return None,
                }
            }
        }
        _ => return None,
    }
    Some(out)
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
        // One string (4 tokens) plus a 3-id token array, counted without a round-trip.
        let body = serde_json::json!({"model": "m", "input": ["text", [1, 2, 3]]});
        let out = counter.prompt_tokens("m", Surface::Embeddings, &body).await;
        assert_eq!(out, ExactOutcome::Counted(7));
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
}
