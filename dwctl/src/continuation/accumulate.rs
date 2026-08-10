//! Reconstructing the partial generation from the deltas the client received.
//!
//! A resume leg conditions the model on `messages`, the generation-prompt stub,
//! and **the exact text the model had already emitted**. Getting that text wrong
//! is the biggest correctness risk in this feature, so reconstruction lives behind a
//! trait: [`StreamAccumulator`], chosen per model by [`for_model`].
//!
//! The default is [`PlainContent`], which handles the case we can prove — plain
//! `delta.content` — and DISARMS (never guesses) on everything else. Why disarm
//! rather than approximate: onwards splits a model's single token sequence into
//! separate delta fields (`content` vs `reasoning_content`), and tool calls
//! arrive as structured fragments, not as the syntax the model actually sampled.
//! Re-serializing those needs a per-model serializer whose output is
//! byte-compared against real emissions — that's the fidelity-harness workstream.
//! Until it issues a verdict for a model, a stream carrying those deltas is not
//! reconstructable and the tee disarms with a labelled reason.
//!
//! A model that HAS a verdict gets its family's reconstructor instead:
//! [`super::dsv4::Dsv4Reconstructor`] for the DeepSeek-V4 (DSML) family, which
//! makes mid-reasoning and mid-tool-call deaths resumable. Adding the next family
//! is a new `impl StreamAccumulator` plus one arm in [`for_model`].

use serde_json::Value;

use crate::config::ContinuationConfig;

use super::dsv4::Dsv4Reconstructor;
use super::rewrap::Envelope;

/// Why a stream stopped being reconstructable. Each maps to a bounded
/// `dwctl_continuation_outcome_total{outcome="disarmed", reason}` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AccumulateError {
    /// A delta field this implementation cannot faithfully re-emit
    /// (`reasoning_content`, `tool_calls`). Awaiting a per-model reconstructor.
    #[error("stream carries a delta this accumulator cannot reconstruct")]
    UnsupportedDelta,
    /// `n > 1`: several independent generations share one stream, and a
    /// completions resume produces one. Out of scope.
    #[error("multi-choice (n>1) streams are not resumable")]
    MultiChoice,
    /// The retained generation passed `continuation.max_buffer_bytes`. The
    /// buffer is dropped immediately — memory safety wins over resumability.
    #[error("accumulated generation exceeded the configured cap")]
    CapExceeded,
}

impl AccumulateError {
    /// Bounded metric label.
    pub fn reason(self) -> &'static str {
        match self {
            AccumulateError::UnsupportedDelta => "unsupported_delta",
            AccumulateError::MultiChoice => "multi_choice",
            AccumulateError::CapExceeded => "cap_exceeded",
        }
    }
}

/// Retains enough of a live stream to rebuild its prompt suffix for a resume leg.
pub trait StreamAccumulator: Send {
    /// Feed one parsed chat chunk; extract and retain generation state.
    fn ingest(&mut self, chunk: &Value) -> Result<(), AccumulateError>;
    /// The partial generation as the raw text the model emitted, for
    /// `/v1/render`'s `continuation_text`. `None` => this stream is not
    /// reconstructable (or nothing has been generated yet) — disarm.
    fn continuation_text(&self) -> Option<String>;
    /// Retained generation length in bytes (cap accounting).
    fn len_bytes(&self) -> usize;
    /// The original stream's identity, once a chunk has carried one, so resumed
    /// frames can be reframed onto the client's stream.
    fn envelope(&self) -> Option<&Envelope>;
    /// Whether a `finish_reason` has been seen — the signal that separates "died
    /// mid-generation" from "finished, trailer lost".
    fn saw_finish_reason(&self) -> bool;
    /// The disarm cause, if this stream is no longer reconstructable.
    fn disarmed(&self) -> Option<AccumulateError>;
}

/// Pick the accumulator for `model`.
///
/// Selection is a capability lookup, not a guess: a model gets a family
/// reconstructor only once the fidelity harness has issued a byte-exactness
/// verdict for it, and until then it gets [`PlainContent`] — the same behaviour
/// as before any reconstructor existed. An unrecognised value falls back the same
/// way, so a typo degrades resumability instead of corrupting a prefix.
///
/// `continuation.model_reconstructors` is the v1 home for that lookup because it
/// needs no schema change to canary one model. It moves onto the per-route DB row
/// (alongside the `continuation` traffic rule that already gates which models are
/// resumable at all) once more than one family is live, at which point the two
/// halves of "is this model resumable" stop being configured in two places.
pub fn for_model(model: &str, cfg: &ContinuationConfig) -> Box<dyn StreamAccumulator> {
    match cfg.model_reconstructors.get(model).map(String::as_str) {
        Some("dsv4") => Box::new(Dsv4Reconstructor::new(cfg.max_buffer_bytes)),
        _ => Box::new(PlainContent::new(cfg.max_buffer_bytes)),
    }
}

/// Capture id/model/created the first time a chunk carries them.
pub(super) fn capture_envelope(slot: &mut Option<Envelope>, chunk: &Value) {
    if slot.is_some() {
        return;
    }
    let Some(id) = chunk.get("id").and_then(Value::as_str) else {
        return;
    };
    *slot = Some(Envelope {
        id: id.to_string(),
        model: chunk.get("model").and_then(Value::as_str).unwrap_or_default().to_string(),
        created: chunk.get("created").and_then(Value::as_u64).unwrap_or(0),
    });
}

/// The chunk's single choice. `Ok(None)` means there is nothing to process —
/// usage-only trailers and keep-alives carry no choices.
pub(super) fn single_choice(chunk: &Value) -> Result<Option<&Value>, AccumulateError> {
    let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
        return Ok(None);
    };
    if choices.len() > 1 {
        // `n > 1`: several independent generations share one stream, and a
        // completions resume produces one.
        return Err(AccumulateError::MultiChoice);
    }
    Ok(choices.first())
}

/// The v1 accumulator: plain `choices[0].delta.content` concatenation.
///
/// Also carries the stream's [`Envelope`] (captured from the first chunk that
/// has one) so resumed frames can be reframed onto the client's original stream
/// identity, and tracks whether a `finish_reason` has been seen — the signal
/// that separates "died mid-generation" from "finished, trailer lost".
pub struct PlainContent {
    text: String,
    cap: usize,
    envelope: Option<Envelope>,
    finish_reason: bool,
    disarmed: Option<AccumulateError>,
}

impl PlainContent {
    pub fn new(cap: usize) -> Self {
        Self {
            text: String::new(),
            cap,
            envelope: None,
            finish_reason: false,
            disarmed: None,
        }
    }

    /// Disarm and drop the buffer. Sticky: the first cause is kept.
    fn disarm(&mut self, cause: AccumulateError) -> Result<(), AccumulateError> {
        self.text.clear();
        self.text.shrink_to_fit();
        let cause = *self.disarmed.get_or_insert(cause);
        Err(cause)
    }
}

impl StreamAccumulator for PlainContent {
    fn ingest(&mut self, chunk: &Value) -> Result<(), AccumulateError> {
        if let Some(cause) = self.disarmed {
            return Err(cause);
        }
        capture_envelope(&mut self.envelope, chunk);

        let choice = match single_choice(chunk) {
            Ok(Some(choice)) => choice,
            Ok(None) => return Ok(()),
            Err(cause) => return self.disarm(cause),
        };

        if choice.get("finish_reason").is_some_and(|f| !f.is_null()) {
            self.finish_reason = true;
        }

        let Some(delta) = choice.get("delta").and_then(Value::as_object) else {
            return Ok(());
        };
        // A delta we cannot re-emit byte-exactly poisons the whole prefix, so it
        // disarms even if it also carries `content`: what the model sampled
        // interleaved the two, and we only have one of them.
        let unsupported = ["reasoning_content", "reasoning", "tool_calls", "function_call"]
            .iter()
            .any(|k| delta.get(*k).is_some_and(|v| !v.is_null()));
        if unsupported {
            return self.disarm(AccumulateError::UnsupportedDelta);
        }

        if let Some(content) = delta.get("content").and_then(Value::as_str) {
            if self.text.len() + content.len() > self.cap {
                return self.disarm(AccumulateError::CapExceeded);
            }
            self.text.push_str(content);
        }
        Ok(())
    }

    fn continuation_text(&self) -> Option<String> {
        if self.disarmed.is_some() || self.text.is_empty() {
            // Nothing generated yet is deliberately NOT resumable: resume-from-zero
            // is a plain retry, which is not this feature's job.
            return None;
        }
        Some(self.text.clone())
    }

    fn len_bytes(&self) -> usize {
        self.text.len()
    }

    fn envelope(&self) -> Option<&Envelope> {
        self.envelope.as_ref()
    }

    fn saw_finish_reason(&self) -> bool {
        self.finish_reason
    }

    fn disarmed(&self) -> Option<AccumulateError> {
        self.disarmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CAP: usize = 1024;

    fn content_chunk(text: &str) -> Value {
        json!({
            "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1_700_000_000,
            "model": "dsv4-flash",
            "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
        })
    }

    #[test]
    fn concatenates_content_deltas_and_captures_the_envelope() {
        let mut acc = PlainContent::new(CAP);
        acc.ingest(&json!({
            "id": "chatcmpl-1", "created": 1_700_000_000, "model": "dsv4-flash",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}}]
        }))
        .unwrap();
        acc.ingest(&content_chunk("Hello")).unwrap();
        acc.ingest(&content_chunk(", world")).unwrap();

        assert_eq!(acc.continuation_text().as_deref(), Some("Hello, world"));
        assert_eq!(acc.len_bytes(), 12);
        let env = acc.envelope().expect("envelope captured from the first chunk");
        assert_eq!(env.id, "chatcmpl-1");
        assert_eq!(env.model, "dsv4-flash");
        assert_eq!(env.created, 1_700_000_000);
        assert!(!acc.saw_finish_reason());
    }

    #[test]
    fn nothing_generated_is_not_resumable() {
        let acc = PlainContent::new(CAP);
        assert_eq!(acc.continuation_text(), None, "resume-from-zero is a retry, not a resume");
    }

    #[test]
    fn finish_reason_is_tracked() {
        let mut acc = PlainContent::new(CAP);
        acc.ingest(&content_chunk("done")).unwrap();
        assert!(!acc.saw_finish_reason());
        acc.ingest(&json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}))
            .unwrap();
        assert!(acc.saw_finish_reason());
    }

    #[test]
    fn usage_only_and_choiceless_frames_are_ignored() {
        let mut acc = PlainContent::new(CAP);
        acc.ingest(&content_chunk("hi")).unwrap();
        acc.ingest(&json!({"choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 1}}))
            .unwrap();
        acc.ingest(&json!({"id": "chatcmpl-1"})).unwrap();
        assert_eq!(acc.continuation_text().as_deref(), Some("hi"));
    }

    // ── disarm cases ─────────────────────────────────────────────────────────

    #[test]
    fn reasoning_delta_disarms() {
        let mut acc = PlainContent::new(CAP);
        acc.ingest(&content_chunk("visible")).unwrap();
        let err = acc
            .ingest(&json!({"choices": [{"delta": {"reasoning_content": "hmm"}}]}))
            .unwrap_err();
        assert_eq!(err, AccumulateError::UnsupportedDelta);
        assert_eq!(err.reason(), "unsupported_delta");
        assert_eq!(acc.continuation_text(), None, "the buffer is poisoned, not merely paused");
        assert_eq!(acc.len_bytes(), 0, "the buffer is dropped on disarm");
    }

    #[test]
    fn tool_call_delta_disarms() {
        let mut acc = PlainContent::new(CAP);
        let err = acc
            .ingest(&json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"a\":"}}]}}]}))
            .unwrap_err();
        assert_eq!(err, AccumulateError::UnsupportedDelta);
    }

    #[test]
    fn a_content_delta_alongside_an_unsupported_one_still_disarms() {
        // The model sampled one interleaved sequence; holding only the content
        // half would silently reorder the generation.
        let mut acc = PlainContent::new(CAP);
        let err = acc
            .ingest(&json!({"choices": [{"delta": {"content": "x", "reasoning_content": "y"}}]}))
            .unwrap_err();
        assert_eq!(err, AccumulateError::UnsupportedDelta);
    }

    #[test]
    fn null_valued_unsupported_fields_do_not_disarm() {
        // Providers routinely send `"reasoning_content": null` / `"tool_calls": null`
        // on every ordinary content chunk.
        let mut acc = PlainContent::new(CAP);
        acc.ingest(&json!({"choices": [{"delta": {"content": "ok", "reasoning_content": null, "tool_calls": null}}]}))
            .unwrap();
        assert_eq!(acc.continuation_text().as_deref(), Some("ok"));
    }

    #[test]
    fn multi_choice_disarms() {
        let mut acc = PlainContent::new(CAP);
        let err = acc
            .ingest(&json!({"choices": [
                {"index": 0, "delta": {"content": "a"}},
                {"index": 1, "delta": {"content": "b"}}
            ]}))
            .unwrap_err();
        assert_eq!(err, AccumulateError::MultiChoice);
        assert_eq!(err.reason(), "multi_choice");
    }

    #[test]
    fn exceeding_the_cap_disarms_and_drops_the_buffer() {
        let mut acc = PlainContent::new(8);
        acc.ingest(&content_chunk("12345")).unwrap();
        assert_eq!(acc.len_bytes(), 5);
        let err = acc.ingest(&content_chunk("6789")).unwrap_err();
        assert_eq!(err, AccumulateError::CapExceeded);
        assert_eq!(err.reason(), "cap_exceeded");
        assert_eq!(acc.len_bytes(), 0);
        assert_eq!(acc.continuation_text(), None);
    }

    // ── per-model selection ──────────────────────────────────────────────────

    fn cfg_with(entries: &[(&str, &str)]) -> ContinuationConfig {
        ContinuationConfig {
            max_buffer_bytes: CAP,
            model_reconstructors: entries.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        }
    }

    /// A reasoning delta is the cheapest way to tell the two apart from behind
    /// the trait: `PlainContent` disarms on it, the DSV4 reconstructor keeps it.
    fn survives_reasoning(acc: &mut dyn StreamAccumulator) -> bool {
        acc.ingest(&json!({"id": "chatcmpl-1", "choices": [{"delta": {"reasoning_content": "hmm"}}]}))
            .is_ok()
    }

    #[test]
    fn a_mapped_model_gets_its_family_reconstructor() {
        let cfg = cfg_with(&[("deepseek-ai/DeepSeek-V4-Flash", "dsv4")]);
        let mut acc = for_model("deepseek-ai/DeepSeek-V4-Flash", &cfg);
        assert!(survives_reasoning(acc.as_mut()));
        assert_eq!(acc.continuation_text().as_deref(), Some("hmm"));
    }

    #[test]
    fn every_other_model_keeps_the_plain_content_behaviour() {
        let cfg = cfg_with(&[("deepseek-ai/DeepSeek-V4-Flash", "dsv4")]);
        for model in ["gpt-4o", "deepseek-ai/DeepSeek-V4-Flash-0731", ""] {
            let mut acc = for_model(model, &cfg);
            assert!(!survives_reasoning(acc.as_mut()), "{model} must not be reconstructed as dsv4");
        }
        // Including when nothing is configured at all.
        let mut acc = for_model("deepseek-ai/DeepSeek-V4-Flash", &ContinuationConfig::default());
        assert!(!survives_reasoning(acc.as_mut()));
    }

    #[test]
    fn an_unrecognised_family_falls_back_instead_of_guessing() {
        let cfg = cfg_with(&[("m", "glm5"), ("n", "DSV4")]);
        for model in ["m", "n"] {
            let mut acc = for_model(model, &cfg);
            assert!(
                !survives_reasoning(acc.as_mut()),
                "{model}: a typo degrades resumability, never fidelity"
            );
        }
    }

    #[test]
    fn the_configured_cap_reaches_both_accumulators() {
        let cfg = ContinuationConfig {
            max_buffer_bytes: 4,
            ..cfg_with(&[("dsv4-model", "dsv4")])
        };
        for model in ["dsv4-model", "plain-model"] {
            let mut acc = for_model(model, &cfg);
            let err = acc.ingest(&content_chunk("12345")).unwrap_err();
            assert_eq!(err, AccumulateError::CapExceeded, "{model}");
        }
    }

    #[test]
    fn disarm_is_sticky_and_keeps_the_first_cause() {
        let mut acc = PlainContent::new(CAP);
        assert_eq!(
            acc.ingest(&json!({"choices": [{"delta": {"tool_calls": []}}]})).unwrap_err(),
            AccumulateError::UnsupportedDelta
        );
        // A later, different violation must not relabel the outcome.
        assert_eq!(acc.ingest(&content_chunk("more")).unwrap_err(), AccumulateError::UnsupportedDelta);
        assert_eq!(acc.disarmed(), Some(AccumulateError::UnsupportedDelta));
    }
}
