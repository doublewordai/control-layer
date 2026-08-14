//! The forward direction: a resume leg's RAW text → the chat deltas the client
//! expects.
//!
//! [`super::accumulate`] runs the client→model direction: it takes the split
//! deltas the client received and rebuilds the raw sequence the model sampled,
//! so a resume prefix can be rendered. This module is its inverse. A resume leg
//! is a `/v1/completions` request, so what comes back is the model's RAW
//! sequence — `</think>`, DSML tool syntax, everything the serving stack's chat
//! parser would normally have split apart. Without a parser here, that text
//! reaches the client verbatim inside `delta.content` (a real `</think>` in a
//! customer-shaped stream is how this was found), which is why reasoning and
//! tool streams DISARM by default.
//!
//! **The pairing is structural.** A [`ForwardParser`] is obtained from the
//! accumulator that produced the prefix ([`super::accumulate::StreamAccumulator::forward_parser`]),
//! never chosen separately: the only accumulator that keeps reasoning/tool deltas
//! instead of disarming is a family reconstructor, and a family reconstructor is
//! the only thing that can hand out its family's parser. So the disarm is lifted
//! exactly where a parser exists to undo it, and a model with no
//! `continuation.model_reconstructors` entry gets [`PlainContent`] +
//! [`PlainForward`] — today's byte-for-byte passthrough.
//!
//! [`PlainContent`]: super::accumulate::PlainContent

use serde_json::{Map, Value, json};

/// One client-ready delta payload: the contents of a single
/// `choices[0].delta`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardDelta {
    /// → `delta.reasoning_content`
    Reasoning(String),
    /// → `delta.content`
    Content(String),
    /// → `delta.tool_calls[0]`. `id`/`name` are `Some` only on the frame that
    /// OPENS a call; every later fragment of the same call carries `index` and
    /// an `arguments` fragment alone, which is the shape the fidelity captures
    /// show from the fragmenting provider.
    ToolCall {
        index: u32,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
}

impl ForwardDelta {
    /// The `choices[0].delta` object for this payload.
    pub fn into_delta(self) -> Value {
        match self {
            ForwardDelta::Reasoning(text) => json!({ "reasoning_content": text }),
            ForwardDelta::Content(text) => json!({ "content": text }),
            ForwardDelta::ToolCall {
                index,
                id,
                name,
                arguments,
            } => {
                let mut function = Map::new();
                if let Some(name) = name {
                    function.insert("name".to_string(), Value::String(name));
                }
                function.insert("arguments".to_string(), Value::String(arguments));

                let mut call = Map::new();
                call.insert("index".to_string(), json!(index));
                if let Some(id) = id {
                    // `type` accompanies the id on the opening frame only, as
                    // both providers in the captures do.
                    call.insert("id".to_string(), Value::String(id));
                    call.insert("type".to_string(), Value::String("function".to_string()));
                }
                call.insert("function".to_string(), Value::Object(function));
                json!({ "tool_calls": [Value::Object(call)] })
            }
        }
    }
}

/// Parses a resume leg's raw text into the chat deltas the client expects.
///
/// Stateful for two independent reasons: text arrives at arbitrary chunk
/// boundaries (a tag can be split across two chunks), and the stream begins
/// MID-STRUCTURE — at the death point, which may be inside reasoning, inside an
/// invoke, or half-way through a parameter value.
pub trait ForwardParser: Send {
    /// Feed raw text; emit zero or more client-ready delta payloads.
    ///
    /// Never reorders and never drops bytes: anything withheld because it might
    /// be the start of a tag is emitted as soon as that is disproven, or by
    /// [`ForwardParser::finish`].
    fn feed(&mut self, raw: &str) -> Vec<ForwardDelta>;

    /// The leg ended (a `finish_reason` arrived, or the chain closed): flush
    /// whatever is still held back.
    fn finish(&mut self) -> Vec<ForwardDelta>;
}

/// Where in the generation the resumed text picks up, taken from the
/// accumulator at the moment of death.
///
/// The resumed leg continues whatever structure was open when leg 1 died, and
/// nothing in the text itself says which: a leg that resumes inside reasoning
/// starts with reasoning tokens that look exactly like body text, and a leg that
/// resumes inside a tool call starts with a parameter value. Only the
/// accumulator knows, because it is the thing that built the prefix the model is
/// continuing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardSeed {
    /// Inside an unclosed `<think>`: text is reasoning until the close tag.
    Reasoning,
    /// The body has started: text is content until a tool block opens.
    Content,
    /// Inside the tool block, between invokes — the next invoke the model opens
    /// is call `next_index`. Carrying the index is what stops the client seeing
    /// a tool-call index restart across the seam.
    BetweenToolCalls { next_index: u32 },
    /// Inside call `index`'s invoke, with `args_so_far` being the arguments text
    /// the client has ALREADY received for it. The finer sub-state (mid
    /// parameter name, name closed, mid value) is derived from that text by the
    /// same partial-arguments parse the reconstructor used to render the prefix,
    /// so the two can never disagree about where the raw stopped.
    InToolCall { index: u32, args_so_far: String },
}

/// Today's behaviour, and the default for every model: raw text IS content.
///
/// Byte-for-byte identical to the pre-parser path — an empty text yields no
/// delta at all, so a leg's keep-alive chunk stays invisible.
pub struct PlainForward;

impl ForwardParser for PlainForward {
    fn feed(&mut self, raw: &str) -> Vec<ForwardDelta> {
        if raw.is_empty() {
            return Vec::new();
        }
        vec![ForwardDelta::Content(raw.to_string())]
    }

    fn finish(&mut self) -> Vec<ForwardDelta> {
        // Nothing is ever held back, so there is never anything to flush.
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_forward_is_a_passthrough_that_holds_nothing_back() {
        let mut p = PlainForward;
        assert_eq!(p.feed("Hello"), vec![ForwardDelta::Content("Hello".to_string())]);
        assert_eq!(p.feed(""), vec![], "an empty text is not a delta, exactly as before");
        assert_eq!(p.finish(), vec![]);
        // Structure is text to a plain model: it must not be interpreted.
        assert_eq!(
            p.feed("</think>"),
            vec![ForwardDelta::Content("</think>".to_string())],
            "an unmapped model's stream is never parsed"
        );
    }

    #[test]
    fn deltas_render_the_openai_streaming_shapes() {
        assert_eq!(
            ForwardDelta::Reasoning("hmm".to_string()).into_delta(),
            json!({"reasoning_content": "hmm"})
        );
        assert_eq!(ForwardDelta::Content("hi".to_string()).into_delta(), json!({"content": "hi"}));

        let opening = ForwardDelta::ToolCall {
            index: 1,
            id: Some("call_abc".to_string()),
            name: Some("get_weather".to_string()),
            arguments: "{".to_string(),
        }
        .into_delta();
        assert_eq!(
            opening,
            json!({"tool_calls": [{
                "index": 1, "id": "call_abc", "type": "function",
                "function": {"name": "get_weather", "arguments": "{"}
            }]})
        );

        // A continuation fragment carries neither id nor name nor type.
        let fragment = ForwardDelta::ToolCall {
            index: 1,
            id: None,
            name: None,
            arguments: "\"city\"".to_string(),
        }
        .into_delta();
        assert_eq!(
            fragment,
            json!({"tool_calls": [{"index": 1, "function": {"arguments": "\"city\""}}]})
        );
    }
}
