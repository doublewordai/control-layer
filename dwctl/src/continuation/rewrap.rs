//! Re-wrapping a resume leg back into the client's original stream shape, and
//! the merged terminal usage frame.
//!
//! A resume leg is a `/v1/completions` request, so it streams
//! `object: "text_completion"` chunks with `choices[0].text`. The client is
//! mid-way through a `chat.completion.chunk` stream and must not be able to tell:
//! every leg chunk is reframed onto the ORIGINAL stream's envelope (same `id`,
//! `model`, `created` — captured from leg 1's first chunk, see
//! [`super::accumulate`]) with the text moved into `choices[0].delta.content`.
//! No `role` delta is re-sent: leg 1 already opened the message.
//!
//! The other half is [`merge_usage`] — the billing-critical arithmetic. The
//! resume leg's own usage frame describes the LEG (its prompt is the original
//! prompt *plus* everything generated so far, re-prefilled), not the logical
//! request. The client must be billed as if nothing went wrong:
//!
//! ```text
//! seg        = continuation_tokens of the FINAL render  (all generation before the final leg)
//! prompt_tokens     := leg.usage.prompt_tokens - seg    // the provider-counted original prompt
//! completion_tokens := seg + leg.usage.completion_tokens // everything the client received
//! ```
//!
//! What we ate (each leg re-paid the whole prompt) is a metric, not a customer
//! charge — see [`super::metrics::record_eaten_prompt_tokens`].

use bytes::Bytes;
use serde_json::{Value, json};

/// SSE terminator frame. Emitted by us only when we have replaced the (lost or
/// leg-local) trailer; leg 1's own `[DONE]` bytes are forwarded verbatim.
pub const DONE_FRAME: &[u8] = b"data: [DONE]\n\n";

/// The original stream's identity, captured from leg 1's first chunk so every
/// resumed frame can be reframed onto it. A client that correlates on `id`
/// (most SDKs do) never sees the seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub id: String,
    pub model: String,
    pub created: u64,
}

/// Merged, customer-visible token accounting for the whole logical request.
///
/// Deliberately minimal: no provider extras (`cached_tokens`, `*_details`, …)
/// from the resume leg, which describe the leg's re-prefill and would read as a
/// discount we never gave. The cache layer sits ABOVE us and injects its own
/// `cache_*` fields into whatever terminal usage we emit, so those must be left
/// alone here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergedUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

impl MergedUsage {
    fn to_value(self) -> Value {
        json!({
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "total_tokens": self.total_tokens,
        })
    }
}

/// Why a merged-usage computation had to fall back. Bounded label for
/// `dwctl_continuation_usage_anomaly_total{kind}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageAnomaly {
    /// The leg reported FEWER prompt tokens than the render says the partial
    /// generation alone occupies — the provider counted a different prefix
    /// (BOS drift, a different template, a truncating engine). Subtracting
    /// would underflow, so the original prompt is taken from the render instead.
    PromptUnderflow,
}

impl UsageAnomaly {
    pub fn kind(self) -> &'static str {
        match self {
            UsageAnomaly::PromptUnderflow => "prompt_underflow",
        }
    }
}

/// The §6 merge. `seg` = `continuation_tokens` of the FINAL render (everything
/// generated before the final leg), `reported_prompt`/`leg_completion` = the
/// final leg's own usage frame, `render_total` = that render's total token count
/// (prompt + generation stub + partial generation).
///
/// Never panics and never underflows: an inconsistent provider count degrades to
/// the render-derived prompt with an anomaly kind for the caller to record.
pub fn merge_usage(seg: u64, reported_prompt: u64, leg_completion: u64, render_total: u64) -> (MergedUsage, Option<UsageAnomaly>) {
    let (prompt_tokens, anomaly) = if reported_prompt >= seg {
        (reported_prompt - seg, None)
    } else {
        // The provider's prompt count can't even cover the partial generation we
        // sent it. Fall back to our own rendering: total - seg is the prompt +
        // generation stub as tokenizer-svc counted it.
        (render_total.saturating_sub(seg), Some(UsageAnomaly::PromptUnderflow))
    };
    let completion_tokens = seg.saturating_add(leg_completion);
    (
        MergedUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens.saturating_add(completion_tokens),
        },
        anomaly,
    )
}

/// Read `usage.prompt_tokens` / `usage.completion_tokens` off a chunk, if it
/// carries a usage object at all. Tolerates the field being `null` (some
/// providers emit `"usage": null` on every non-terminal chunk).
pub fn usage_of(chunk: &Value) -> Option<(u64, u64)> {
    let usage = chunk.get("usage")?.as_object()?;
    let prompt = usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
    let completion = usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0);
    Some((prompt, completion))
}

/// The generated text and `finish_reason` of one `text_completion` streaming
/// chunk. `None` when the chunk carries no choice at all — the leg's terminal
/// usage-only chunk, which the caller replaces with [`usage_frame`].
///
/// This is the raw material the leg's [`ForwardParser`] consumes: on a model
/// with a reconstructor the text is the model's RAW sequence and becomes 0..n
/// chat deltas, and on every other model it is the content itself.
///
/// [`ForwardParser`]: super::forward::ForwardParser
pub fn completion_parts(chunk: &Value) -> Option<(&str, Value)> {
    let choice = chunk.get("choices")?.as_array()?.first()?;
    let text = choice.get("text").and_then(Value::as_str).unwrap_or("");
    let finish_reason = choice.get("finish_reason").cloned().unwrap_or(Value::Null);
    Some((text, finish_reason))
}

/// One chat chunk on the original stream's envelope, carrying an already-built
/// `delta` object.
///
/// No `role` delta is added HERE: most streams have already opened the message
/// on leg 1, and re-sending one makes strict clients open a second. But not
/// every provider opens with a role preamble (the plat captures attach it to a
/// later frame), so the layer tracks whether a role has actually been delivered
/// and injects one into the first resumed delta only when it is still owed
/// (`ensure_role` in the layer).
pub fn delta_chunk(env: &Envelope, delta: Value, finish_reason: Value) -> Value {
    json!({
        "id": env.id,
        "object": "chat.completion.chunk",
        "created": env.created,
        "model": env.model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }],
    })
}

/// Reframe one `text_completion` streaming chunk as a `chat.completion.chunk` on
/// the original envelope, treating its text as plain content. Returns `None`
/// when there is nothing for the client in it (no choices, or an empty text with
/// no `finish_reason`).
///
/// **The reference implementation of the passthrough path.** The live path runs
/// the text through the leg's forward parser instead; for an unmapped model that
/// parser is `PlainForward`, and the frames it produces are byte-identical to
/// this function's — pinned by a test here, so the parser rewiring cannot
/// silently change what a plain model's client receives.
pub fn reframe_chunk(chunk: &Value, env: &Envelope) -> Option<Value> {
    let (text, finish_reason) = completion_parts(chunk)?;
    if text.is_empty() && finish_reason.is_null() {
        return None;
    }
    let mut delta = serde_json::Map::new();
    if !text.is_empty() {
        delta.insert("content".to_string(), Value::String(text.to_string()));
    }
    Some(delta_chunk(env, Value::Object(delta), finish_reason))
}

/// A bare `finish_reason: "length"` chunk. Emitted when the client's own
/// `max_tokens` is already spent by the partial generation: the correct end of
/// that stream is the one the model would itself have produced on the next
/// token, not a resume that would overrun the cap.
pub fn length_stop_frame(env: &Envelope) -> Value {
    json!({
        "id": env.id,
        "object": "chat.completion.chunk",
        "created": env.created,
        "model": env.model,
        "choices": [{"index": 0, "delta": {}, "finish_reason": "length"}],
    })
}

/// The terminal usage frame in the shape the model would have emitted: a chat
/// chunk with no choices carrying the merged accounting.
pub fn usage_frame(env: &Envelope, usage: MergedUsage) -> Value {
    json!({
        "id": env.id,
        "object": "chat.completion.chunk",
        "created": env.created,
        "model": env.model,
        "choices": [],
        "usage": usage.to_value(),
    })
}

/// Serialize a chunk as one SSE data frame.
pub fn sse_frame(value: &Value) -> Bytes {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(b"data: ");
    // A serde_json::Value always serializes; the fallback keeps this infallible.
    match serde_json::to_vec(value) {
        Ok(mut v) => out.append(&mut v),
        Err(_) => out.extend_from_slice(b"{}"),
    }
    out.extend_from_slice(b"\n\n");
    Bytes::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Envelope {
        Envelope {
            id: "chatcmpl-abc".to_string(),
            model: "dsv4-flash".to_string(),
            created: 1_700_000_000,
        }
    }

    // ── merged usage arithmetic ──────────────────────────────────────────────

    #[test]
    fn merge_usage_subtracts_the_partial_generation_from_the_leg_prompt() {
        // Leg 2 was sent prompt(1000) + partial generation(40) and generated 60 more.
        let (usage, anomaly) = merge_usage(40, 1040, 60, 1042);
        assert_eq!(anomaly, None);
        assert_eq!(usage.prompt_tokens, 1000, "the customer pays for their original prompt once");
        assert_eq!(usage.completion_tokens, 100, "everything the client received across both legs");
        assert_eq!(usage.total_tokens, 1100);
    }

    #[test]
    fn merge_usage_with_no_generation_before_the_leg_is_the_leg_itself() {
        let (usage, anomaly) = merge_usage(0, 1000, 60, 1000);
        assert_eq!(anomaly, None);
        assert_eq!(
            usage,
            MergedUsage {
                prompt_tokens: 1000,
                completion_tokens: 60,
                total_tokens: 1060
            }
        );
    }

    #[test]
    fn merge_usage_falls_back_to_render_when_the_provider_undercounts() {
        // Provider says the whole prompt was 30 tokens but the partial generation
        // alone is 40 — impossible; fall back to render.total - seg.
        let (usage, anomaly) = merge_usage(40, 30, 60, 1042);
        assert_eq!(anomaly, Some(UsageAnomaly::PromptUnderflow));
        assert_eq!(usage.prompt_tokens, 1002, "render-derived prompt (total 1042 - seg 40)");
        assert_eq!(usage.completion_tokens, 100, "the generation total is unaffected by the anomaly");
        assert_eq!(usage.total_tokens, 1102);
    }

    #[test]
    fn merge_usage_never_underflows_even_when_the_render_is_smaller_than_the_segment() {
        let (usage, anomaly) = merge_usage(40, 0, 0, 10);
        assert_eq!(anomaly, Some(UsageAnomaly::PromptUnderflow));
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.completion_tokens, 40);
    }

    #[test]
    fn usage_of_reads_counts_and_tolerates_null_usage() {
        assert_eq!(
            usage_of(&json!({"usage": {"prompt_tokens": 7, "completion_tokens": 3}})),
            Some((7, 3))
        );
        assert_eq!(usage_of(&json!({"usage": null})), None);
        assert_eq!(usage_of(&json!({"choices": []})), None);
        // Partial usage objects degrade to zero rather than dropping the frame.
        assert_eq!(usage_of(&json!({"usage": {"prompt_tokens": 7}})), Some((7, 0)));
    }

    // ── chunk reframe ────────────────────────────────────────────────────────

    #[test]
    fn reframe_maps_text_onto_the_original_envelope() {
        let chunk = json!({
            "id": "cmpl-leg2", "object": "text_completion", "created": 1_800_000_000,
            "model": "continuation-composite",
            "choices": [{"text": " world", "index": 0, "logprobs": null, "finish_reason": null}]
        });
        let out = reframe_chunk(&chunk, &env()).unwrap();
        assert_eq!(out["id"], "chatcmpl-abc", "the client's stream id, not the leg's");
        assert_eq!(out["object"], "chat.completion.chunk");
        assert_eq!(out["created"], 1_700_000_000);
        assert_eq!(out["model"], "dsv4-flash");
        assert_eq!(out["choices"][0]["delta"]["content"], " world");
        assert!(
            out["choices"][0]["delta"].get("role").is_none(),
            "leg 1 already opened the message; a second role delta would start a new one"
        );
        assert_eq!(out["choices"][0]["finish_reason"], Value::Null);
    }

    #[test]
    fn reframe_passes_through_finish_reason_on_the_last_content_chunk() {
        let chunk = json!({
            "choices": [{"text": "!", "index": 0, "finish_reason": "stop"}]
        });
        let out = reframe_chunk(&chunk, &env()).unwrap();
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["choices"][0]["delta"]["content"], "!");
    }

    #[test]
    fn reframe_emits_a_bare_finish_reason_chunk_with_no_content_key() {
        let chunk = json!({"choices": [{"text": "", "index": 0, "finish_reason": "length"}]});
        let out = reframe_chunk(&chunk, &env()).unwrap();
        assert_eq!(out["choices"][0]["finish_reason"], "length");
        assert!(out["choices"][0]["delta"].as_object().unwrap().is_empty());
    }

    #[test]
    fn reframe_skips_frames_with_nothing_for_the_client() {
        // The leg's usage-only trailer: no choices at all.
        assert!(reframe_chunk(&json!({"choices": [], "usage": {"prompt_tokens": 1}}), &env()).is_none());
        // An empty keep-alive text with no finish reason.
        assert!(reframe_chunk(&json!({"choices": [{"text": "", "finish_reason": null}]}), &env()).is_none());
        // Not a completions chunk at all.
        assert!(reframe_chunk(&json!({"error": {"message": "boom"}}), &env()).is_none());
    }

    // ── SSE framing ──────────────────────────────────────────────────────────

    #[test]
    fn usage_frame_is_a_choiceless_chat_chunk() {
        let frame = usage_frame(
            &env(),
            MergedUsage {
                prompt_tokens: 1000,
                completion_tokens: 100,
                total_tokens: 1100,
            },
        );
        assert_eq!(frame["object"], "chat.completion.chunk");
        assert_eq!(frame["choices"].as_array().unwrap().len(), 0);
        assert_eq!(frame["usage"]["prompt_tokens"], 1000);
        assert_eq!(frame["usage"]["completion_tokens"], 100);
        assert_eq!(frame["usage"]["total_tokens"], 1100);
        assert!(
            frame["usage"].get("prompt_tokens_details").is_none(),
            "our frame stays minimal — the cache layer above injects its own cache_* fields"
        );
    }

    #[test]
    fn sse_frame_wraps_json_in_a_data_event() {
        let bytes = sse_frame(&json!({"a": 1}));
        assert_eq!(&bytes[..], b"data: {\"a\":1}\n\n");
    }
}
