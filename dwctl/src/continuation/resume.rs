//! Building and dispatching one resume leg.
//!
//! A leg is an ordinary `/v1/completions` streaming request that re-enters our
//! own stack through [`ContinuationState::resume_target`] — the router clone
//! taken at the continuation layer's insertion point. That is the entire routing
//! story: onwards resolves the global `continuation` key from its cache, the
//! `model_traffic_rules` purpose rule redirects the model to its continuation
//! composite, and the composite tries dynamo before the provider. **The
//! middleware never picks a target**, so no provider-specific code exists here
//! and none should ever be added; a provider quirk that config cannot express is
//! a reason to reject the provider.
//!
//! Because the leg enters BELOW outlet and the cache layer, it produces no second
//! analytics row, no second billing record and no second cache classify — the
//! customer's request stays exactly one logical request.

use std::pin::Pin;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::prompt_cache::sse::SseBufferedStream;

use super::layer::{ContinuationState, RequestContext};
use super::metrics;
use super::render::{RenderError, RenderPrefix, RenderedPrefix};

/// A leg's frames, already reassembled at SSE event boundaries.
pub type LegStream = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>;

/// A live resume leg plus the render that produced its prompt — the render is
/// kept because its `continuation_tokens` is the segment count the merged usage
/// arithmetic subtracts back out.
pub struct Leg {
    pub render: RenderedPrefix,
    pub stream: LegStream,
    /// The attempt's remaining budget. The tee applies it to the leg's FIRST
    /// frame — response headers alone are not a first token (dynamo can hold
    /// a 200 open with nothing behind it), and the inter-frame stall timer
    /// (sized for platform liveness, minutes) must not become the seam bound.
    pub deadline: tokio::time::Instant,
}

#[derive(Debug, thiserror::Error)]
pub enum LegError {
    #[error("render failed: {0}")]
    Render(#[from] RenderError),
    /// The client's own `max_tokens` is already spent by the partial generation,
    /// so there is nothing left to generate. Carries the render so the caller can
    /// still finish the stream with correct accounting.
    #[error("max_tokens is already exhausted by the partial generation")]
    MaxTokensReached(RenderedPrefix),
    #[error("the resume attempt exceeded its deadline")]
    Deadline,
    #[error("the resume leg returned {0}")]
    Upstream(StatusCode),
    #[error("the resume leg did not return a stream")]
    NotStreaming,
}

impl LegError {
    /// Bounded metric label.
    pub fn reason(&self) -> &'static str {
        match self {
            LegError::Render(_) => "render_failed",
            LegError::MaxTokensReached(_) => "max_tokens_reached",
            LegError::Deadline => "deadline",
            LegError::Upstream(_) => "leg_error",
            LegError::NotStreaming => "leg_not_streaming",
        }
    }
}

/// The resume leg's path: the original request's path with its
/// `chat/completions` suffix swapped for `completions`, preserving whatever
/// prefix the inner router expects (which differs between strict and non-strict
/// mode, and between nested and un-nested routers — hence a suffix swap rather
/// than a hard-coded path).
pub fn resume_path(original: &str) -> String {
    match original.strip_suffix("chat/completions") {
        Some(prefix) => format!("{prefix}completions"),
        None => original.to_string(),
    }
}

/// The completions body for a resume leg.
///
/// Token ids only — never a string prompt. The whole design rests on nobody
/// downstream re-templating or re-tokenizing: what we send is exactly the ids
/// the original prompt and the partial generation occupy.
pub fn build_leg_body(ctx: &RequestContext, token_ids: &[u32], max_tokens: Option<u32>, priority: i32) -> Value {
    let mut body = json!({
        "model": ctx.model,
        "prompt": token_ids,
        "stream": true,
        // Non-negotiable: the merged usage frame is computed from the leg's own
        // usage, so a leg that doesn't report one cannot be billed correctly.
        "stream_options": {"include_usage": true},
        // Positive priority jumps the dynamo queue ahead of new realtime work:
        // this finishes a stream we already accepted, on a strict seam budget.
        // The dynamo frontend's ONLY priority carrier is `nvext.agent_hints.
        // priority` (the same shape fusillade injects for batch deadlines) — a
        // top-level `priority` field is REJECTED by its validation
        // ("Unsupported parameter(s)"), found live in the 0731 wild window.
        // Onwards strips the whole `nvext` object for pool members whose
        // endpoint does not accept scheduling priority (third parties reject
        // unknown fields).
        "nvext": {"agent_hints": {"priority": priority}},
    });
    if let Some(max_tokens) = max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    // Sampling passthrough: the continuation must be drawn from the same
    // distribution the client asked for. The repetition penalties belong here
    // as much as temperature does — they are what stops a long generation
    // looping, and a leg that drops them can degenerate into repetition
    // precisely on the requests long enough to have needed resuming.
    for key in [
        "temperature",
        "top_p",
        "seed",
        "stop",
        "logit_bias",
        "frequency_penalty",
        "presence_penalty",
    ] {
        if let Some(value) = ctx.body.get(key).filter(|v| !v.is_null()) {
            body[key] = value.clone();
        }
    }
    body
}

fn build_leg_request(path: &str, key_secret: &str, body: &Value) -> Request<Body> {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    Request::builder()
        .method(Method::POST)
        .uri(resume_path(path))
        .header(header::AUTHORIZATION, format!("Bearer {key_secret}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "text/event-stream")
        .body(Body::from(bytes))
        .expect("a POST with static headers always builds")
}

fn is_sse(response: &axum::response::Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|ct| ct.trim().eq_ignore_ascii_case("text/event-stream"))
}

/// Run one resume attempt: render the prefix, dispatch the leg, hand back its
/// live stream. Render and time-to-response share ONE deadline — a slow render
/// eats the budget a cold provider prefill would otherwise have had, and the
/// client is waiting through all of it.
pub async fn attempt(state: &ContinuationState, ctx: &RequestContext, continuation_text: &str, attempt_no: u32) -> Result<Leg, LegError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(state.cfg.resume_deadline_secs);

    // The route's render kwargs (its serving mode) overlaid with the client's:
    // the resume prefix must reproduce how leg 1 was templated, and a route
    // serving a chat-mode model must not be rendered in thinking mode.
    let render_kwargs = ctx.render_kwargs();
    let prefix = RenderPrefix {
        virtual_model: &ctx.model,
        messages: ctx.messages(),
        tools: ctx.tools(),
        chat_template_kwargs: render_kwargs.as_ref(),
        continuation_text,
    };
    let render = match tokio::time::timeout_at(deadline, state.tokenizer.render(&prefix)).await {
        Err(_) => return Err(LegError::Deadline),
        Ok(result) => result?,
    };

    // What the client asked for minus what they already received. Resuming past
    // their cap would be a silent overrun (and an overcharge).
    let max_tokens = match ctx.max_tokens() {
        Some(requested) => {
            let generated = render.continuation_tokens.unwrap_or(0);
            let remaining = requested.saturating_sub(generated);
            if remaining == 0 {
                return Err(LegError::MaxTokensReached(render));
            }
            Some(remaining)
        }
        None => None,
    };

    // NOTE: `route.strip_leading_bos` is deliberately NOT applied here. See
    // [`super::RouteInfo::strip_leading_bos`]: BOS-prepending is a per-MEMBER
    // property, and this body is built once, before onwards picks a member.
    let body = build_leg_body(ctx, &render.token_ids, max_tokens, state.cfg.priority);
    let request = build_leg_request(&ctx.path, &state.key_secret, &body);

    // Dispatch accounting happens BEFORE the await: a leg that times out,
    // 4xxs, or comes back non-streaming still re-prefilled the prompt
    // upstream and still counts as an attempt — those are exactly the legs
    // whose cost and rate incidents need visible.
    metrics::record_resume_leg(&ctx.model, attempt_no);
    metrics::record_eaten_prompt_tokens(&ctx.model, render.token_ids.len() as u64);

    let response = match tokio::time::timeout_at(deadline, state.resume_target.clone().oneshot(request)).await {
        Err(_) => return Err(LegError::Deadline),
        // The router's error type is Infallible, so this arm cannot be taken.
        Ok(Err(_)) => return Err(LegError::NotStreaming),
        Ok(Ok(response)) => response,
    };

    if !response.status().is_success() {
        return Err(LegError::Upstream(response.status()));
    }
    if !is_sse(&response) {
        return Err(LegError::NotStreaming);
    }
    let body = BodyExt::into_data_stream(response.into_body()).map(|r| r.map_err(std::io::Error::other));
    Ok(Leg {
        render,
        stream: Box::pin(SseBufferedStream::new(body)),
        deadline,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(body: Value) -> RequestContext {
        RequestContext {
            model: "dsv4-flash".to_string(),
            body,
            path: "/chat/completions".to_string(),
            response_id: None,
            route: crate::continuation::RouteInfo::default(),
            origin: "realtime",
        }
    }

    #[test]
    fn resume_path_swaps_only_the_endpoint_suffix() {
        assert_eq!(resume_path("/chat/completions"), "/completions");
        assert_eq!(resume_path("/v1/chat/completions"), "/v1/completions");
        assert_eq!(resume_path("/ai/v1/chat/completions"), "/ai/v1/completions");
        // Not a chat-completions path (unreachable via the layer's gate): unchanged.
        assert_eq!(resume_path("/embeddings"), "/embeddings");
    }

    #[test]
    fn leg_body_is_token_ids_streaming_and_priority_bearing() {
        let body = build_leg_body(&ctx(json!({"model": "dsv4-flash", "stream": true})), &[1, 2, 3], Some(400), 100);
        assert_eq!(body["model"], "dsv4-flash");
        assert_eq!(body["prompt"], json!([1, 2, 3]), "a flat token-id array, never a string prompt");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        // The dynamo frontend's only priority carrier — a top-level `priority`
        // is rejected by its completions validation.
        assert_eq!(body["nvext"]["agent_hints"]["priority"], 100);
        assert!(body.get("priority").is_none(), "top-level priority is not a dynamo field");
        assert_eq!(body["max_tokens"], 400);
    }

    #[test]
    fn leg_body_passes_sampling_through_and_omits_absent_fields() {
        let original = json!({
            "model": "dsv4-flash", "stream": true,
            "temperature": 0.7, "top_p": 0.95, "seed": 42,
            "stop": ["\n\n"], "logit_bias": {"50256": -100},
            "frequency_penalty": 0.5, "presence_penalty": 0.25
        });
        let body = build_leg_body(&ctx(original), &[9], None, 100);
        assert_eq!(body["temperature"], 0.7);
        assert_eq!(body["top_p"], 0.95);
        assert_eq!(body["seed"], 42);
        assert_eq!(body["stop"], json!(["\n\n"]));
        assert_eq!(body["logit_bias"]["50256"], -100);
        // The repetition penalties are sampling too: a leg that dropped them
        // could degenerate into looping on exactly the long generations that
        // needed resuming in the first place.
        assert_eq!(body["frequency_penalty"], 0.5);
        assert_eq!(body["presence_penalty"], 0.25);
        assert!(body.get("max_tokens").is_none(), "an unbounded request stays unbounded");
        // The chat-shaped fields must never appear on a completions leg.
        assert!(body.get("messages").is_none());
    }

    /// A field the client sent as `null` means "unset"; forwarding it as an
    /// explicit null is not the same request.
    #[test]
    fn leg_body_omits_null_sampling_fields() {
        let body = build_leg_body(
            &ctx(json!({"model": "dsv4-flash", "temperature": null, "frequency_penalty": null, "presence_penalty": null})),
            &[9],
            None,
            100,
        );
        for key in ["temperature", "frequency_penalty", "presence_penalty"] {
            assert!(body.get(key).is_none(), "{key} was sent as null and must not be forwarded");
        }
    }

    #[test]
    fn leg_request_carries_the_global_key_and_asks_for_a_stream() {
        let body = build_leg_body(&ctx(json!({})), &[1], None, 100);
        let request = build_leg_request("/v1/chat/completions", "dw-continuation-secret", &body);
        assert_eq!(request.method(), Method::POST);
        assert_eq!(request.uri().path(), "/v1/completions");
        assert_eq!(
            request.headers().get(header::AUTHORIZATION).unwrap(),
            "Bearer dw-continuation-secret",
            "the leg authenticates as the global continuation key, not as the customer"
        );
        assert_eq!(request.headers().get(header::CONTENT_TYPE).unwrap(), "application/json");
        assert_eq!(request.headers().get(header::ACCEPT).unwrap(), "text/event-stream");
    }
}
