//! Reframe an OpenAI Chat Completions SSE stream into Anthropic's typed event
//! sequence.
//!
//! OpenAI streams flat delta chunks (`choices[].delta`); Anthropic streams a
//! lifecycle of typed events:
//!
//! ```text
//! message_start
//!   content_block_start (thinking)  content_block_delta (thinking_delta)*  content_block_stop
//!   content_block_start (text)   content_block_delta (text_delta)*   content_block_stop
//!   content_block_start (tool_use)  content_block_delta (input_json_delta)*  content_block_stop
//! message_delta (stop_reason + usage)
//! message_stop
//! ```
//!
//! This is a stateful, streaming transform (no buffering): it opens a content
//! block on the first delta of a given kind, accumulates tool-call argument
//! fragments by index, and closes blocks when the kind switches or the stream
//! ends. Modelled on onwards' `StreamingState` (the Chat -> Responses analogue).
//!
//! Known limitations:
//!
//! - OpenAI reports `usage` only in the final chunk (with
//!   `stream_options.include_usage`), so `message_start` carries
//!   `input_tokens: 0`; the real input/output counts are sent on `message_delta`.
//! - Tool calls are assumed to arrive sequentially (call 0 fully, then call 1).
//!   We close the open block when a new tool call starts, so genuinely
//!   *interleaved* parallel tool-call deltas would be mis-grouped. This is
//!   acceptable because Anthropic's format cannot represent interleaved
//!   `tool_use` blocks anyway (content blocks are strictly sequential) and
//!   OpenAI-compatible backends stream tool calls one at a time.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::response::anthropic_usage;
use crate::inference::translation::StreamReframer;

/// The content block currently open (Anthropic requires blocks be opened and
/// closed explicitly, with sequential indices).
enum OpenBlock {
    Thinking(usize),
    Text(usize),
    Tool(usize),
}

/// Reframer for the Anthropic Messages streaming format.
#[derive(Default)]
pub struct AnthropicStreamReframer {
    started: bool,
    finished: bool,
    next_index: usize,
    open: Option<OpenBlock>,
    /// OpenAI `tool_calls[].index` -> Anthropic content-block index.
    tool_block: HashMap<u64, usize>,
    stop_reason: Option<&'static str>,
    /// Matched stop sequence from `choices[].stop_reason` (vLLM/sglang).
    matched_stop: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
}

impl AnthropicStreamReframer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Close the currently open content block, if any.
    fn close_open(&mut self, out: &mut Vec<u8>) {
        if let Some(open) = self.open.take() {
            let index = match open {
                OpenBlock::Thinking(i) | OpenBlock::Text(i) | OpenBlock::Tool(i) => i,
            };
            push_event(out, "content_block_stop", &json!({ "type": "content_block_stop", "index": index }));
        }
    }
}

impl StreamReframer for AnthropicStreamReframer {
    fn push(&mut self, chunk: &Value) -> Vec<u8> {
        let mut out = Vec::new();

        // A mid-stream upstream error (a bare `{"error":{...}}` chunk, or the
        // OpenRouter shape with `error` alongside empty choices) is terminal:
        // surface it as an Anthropic `error` event and stop emitting.
        if let Some(err) = chunk.get("error").filter(|e| !e.is_null()) {
            let message = err.get("message").and_then(Value::as_str).unwrap_or("upstream error");
            push_event(
                &mut out,
                "error",
                &json!({ "type": "error", "error": { "type": "api_error", "message": message } }),
            );
            self.finished = true;
            return out;
        }

        if !self.started {
            self.started = true;
            let id = chunk.get("id").and_then(Value::as_str).unwrap_or("msg_stream");
            let model = chunk.get("model").and_then(Value::as_str).unwrap_or_default();
            push_event(
                &mut out,
                "message_start",
                &json!({
                    "type": "message_start",
                    "message": {
                        "id": id,
                        "type": "message",
                        "role": "assistant",
                        "model": model,
                        "content": [],
                        "stop_reason": Value::Null,
                        "stop_sequence": Value::Null,
                        "usage": { "input_tokens": 0, "output_tokens": 0 },
                    }
                }),
            );
        }

        // Usage usually arrives as a final, choices-empty chunk.
        if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
            let (input, output, cache_read, cache_creation) = anthropic_usage(usage);
            self.input_tokens = input;
            self.output_tokens = output;
            self.cache_read = cache_read;
            self.cache_creation = cache_creation;
        }

        if let Some(choice) = chunk.get("choices").and_then(Value::as_array).and_then(|a| a.first()) {
            let delta = choice.get("delta");

            // Reasoning deltas -> a leading `thinking` block (before any text/tool).
            if let Some(reasoning) = delta
                .and_then(|d| d.get("reasoning_content").or_else(|| d.get("reasoning")))
                .and_then(Value::as_str)
                && !reasoning.is_empty()
            {
                if !matches!(self.open, Some(OpenBlock::Thinking(_))) {
                    self.close_open(&mut out);
                    let index = self.next_index;
                    self.next_index += 1;
                    self.open = Some(OpenBlock::Thinking(index));
                    push_event(
                        &mut out,
                        "content_block_start",
                        &json!({ "type": "content_block_start", "index": index, "content_block": { "type": "thinking", "thinking": "" } }),
                    );
                }
                if let Some(OpenBlock::Thinking(index)) = self.open {
                    push_event(
                        &mut out,
                        "content_block_delta",
                        &json!({ "type": "content_block_delta", "index": index, "delta": { "type": "thinking_delta", "thinking": reasoning } }),
                    );
                }
            }

            // Text content -> open/continue a text block.
            if let Some(text) = delta.and_then(|d| d.get("content")).and_then(Value::as_str)
                && !text.is_empty()
            {
                if !matches!(self.open, Some(OpenBlock::Text(_))) {
                    self.close_open(&mut out);
                    let index = self.next_index;
                    self.next_index += 1;
                    self.open = Some(OpenBlock::Text(index));
                    push_event(
                        &mut out,
                        "content_block_start",
                        &json!({ "type": "content_block_start", "index": index, "content_block": { "type": "text", "text": "" } }),
                    );
                }
                if let Some(OpenBlock::Text(index)) = self.open {
                    push_event(
                        &mut out,
                        "content_block_delta",
                        &json!({ "type": "content_block_delta", "index": index, "delta": { "type": "text_delta", "text": text } }),
                    );
                }
            }

            // Tool calls -> open a tool_use block per call, stream argument fragments.
            if let Some(tool_calls) = delta.and_then(|d| d.get("tool_calls")).and_then(Value::as_array) {
                for tc in tool_calls {
                    let tc_index = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let func = tc.get("function");

                    // A new tool call carries an id + name on its first delta.
                    if let (Some(id), Some(name)) = (
                        tc.get("id").and_then(Value::as_str),
                        func.and_then(|f| f.get("name")).and_then(Value::as_str),
                    ) {
                        self.close_open(&mut out);
                        let index = self.next_index;
                        self.next_index += 1;
                        self.tool_block.insert(tc_index, index);
                        self.open = Some(OpenBlock::Tool(index));
                        push_event(
                            &mut out,
                            "content_block_start",
                            &json!({ "type": "content_block_start", "index": index, "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} } }),
                        );
                    }

                    // Argument fragments accumulate as input_json_delta.
                    if let Some(args) = func.and_then(|f| f.get("arguments")).and_then(Value::as_str)
                        && !args.is_empty()
                        && let Some(&index) = self.tool_block.get(&tc_index)
                    {
                        push_event(
                            &mut out,
                            "content_block_delta",
                            &json!({ "type": "content_block_delta", "index": index, "delta": { "type": "input_json_delta", "partial_json": args } }),
                        );
                    }
                }
            }

            if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
                self.stop_reason = Some(map_stop_reason(fr));
            }
            // vLLM/sglang report the matched stop sequence here (takes precedence).
            if let Some(s) = choice.get("stop_reason").and_then(Value::as_str)
                && !s.is_empty()
            {
                self.matched_stop = Some(s.to_string());
            }
        }

        out
    }

    fn finish(&mut self) -> Vec<u8> {
        if !self.started || self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut out = Vec::new();
        self.close_open(&mut out);
        // A matched stop sequence takes precedence over the finish_reason mapping.
        let (stop_reason, stop_sequence) = match &self.matched_stop {
            Some(s) => ("stop_sequence", json!(s)),
            None => (self.stop_reason.unwrap_or("end_turn"), Value::Null),
        };
        // input_tokens is non-standard on message_delta but included so the count
        // is not lost (message_start could only carry 0). See module docs. Cache
        // counts are added only when present.
        let mut usage = json!({ "input_tokens": self.input_tokens, "output_tokens": self.output_tokens });
        if let Some(cache_read) = self.cache_read {
            usage["cache_read_input_tokens"] = json!(cache_read);
        }
        if let Some(cache_creation) = self.cache_creation {
            usage["cache_creation_input_tokens"] = json!(cache_creation);
        }
        push_event(
            &mut out,
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": { "stop_reason": stop_reason, "stop_sequence": stop_sequence },
                "usage": usage,
            }),
        );
        push_event(&mut out, "message_stop", &json!({ "type": "message_stop" }));
        out
    }
}

/// OpenAI `finish_reason` -> Anthropic `stop_reason`.
fn map_stop_reason(finish: &str) -> &'static str {
    match finish {
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        _ => "end_turn",
    }
}

/// Append one SSE event (`event: <t>\ndata: <json>\n\n`).
fn push_event(out: &mut Vec<u8>, event: &str, data: &Value) {
    out.extend_from_slice(format!("event: {event}\ndata: {data}\n\n").as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect the `event:` names emitted across a sequence of chunks + finish.
    fn run(chunks: &[Value]) -> String {
        let mut r = AnthropicStreamReframer::new();
        let mut out = Vec::new();
        for c in chunks {
            out.extend_from_slice(&r.push(c));
        }
        out.extend_from_slice(&r.finish());
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn text_stream_lifecycle() {
        let sse = run(&[
            json!({ "id": "c1", "model": "m", "choices": [ { "delta": { "role": "assistant" } } ] }),
            json!({ "choices": [ { "delta": { "content": "Hel" } } ] }),
            json!({ "choices": [ { "delta": { "content": "lo" } } ] }),
            json!({ "choices": [ { "delta": {}, "finish_reason": "stop" } ] }),
            json!({ "choices": [], "usage": { "prompt_tokens": 5, "completion_tokens": 2 } }),
        ]);
        // Ordered lifecycle.
        let order: Vec<&str> = sse.lines().filter_map(|l| l.strip_prefix("event: ")).collect();
        assert_eq!(
            order,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert!(sse.contains(r#""text":"Hel""#));
        assert!(sse.contains(r#""stop_reason":"end_turn""#));
        assert!(sse.contains(r#""output_tokens":2"#));
    }

    #[test]
    fn tool_call_stream_accumulates_by_index() {
        let sse = run(&[
            json!({ "id": "c1", "model": "m", "choices": [ { "delta": { "role": "assistant" } } ] }),
            json!({ "choices": [ { "delta": { "tool_calls": [ { "index": 0, "id": "tu_1", "function": { "name": "get_weather", "arguments": "" } } ] } } ] }),
            json!({ "choices": [ { "delta": { "tool_calls": [ { "index": 0, "function": { "arguments": "{\"city\":" } } ] } } ] }),
            json!({ "choices": [ { "delta": { "tool_calls": [ { "index": 0, "function": { "arguments": "\"SF\"}" } } ] } } ] }),
            json!({ "choices": [ { "delta": {}, "finish_reason": "tool_calls" } ] }),
        ]);
        let order: Vec<&str> = sse.lines().filter_map(|l| l.strip_prefix("event: ")).collect();
        assert_eq!(
            order,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert!(sse.contains(r#""type":"tool_use""#));
        assert!(sse.contains(r#""name":"get_weather""#));
        assert!(sse.contains(r#""type":"input_json_delta""#));
        assert!(sse.contains(r#""partial_json":"{\"city\":""#));
        assert!(sse.contains(r#""stop_reason":"tool_use""#));
    }

    /// Collect `(event, index)` pairs for content-block events to assert indices.
    fn indices(sse: &str) -> Vec<(String, i64)> {
        let mut out = Vec::new();
        let mut last_event = String::new();
        for line in sse.lines() {
            if let Some(ev) = line.strip_prefix("event: ") {
                last_event = ev.to_string();
            } else if let Some(data) = line.strip_prefix("data: ")
                && last_event.starts_with("content_block")
                && let Ok(v) = serde_json::from_str::<Value>(data)
                && let Some(i) = v.get("index").and_then(Value::as_i64)
            {
                out.push((last_event.clone(), i));
            }
        }
        out
    }

    #[test]
    fn text_then_tool_call_get_distinct_blocks() {
        // A text block must be opened/closed at index 0, then a tool_use block at
        // index 1 - distinct, sequentially-indexed blocks (the tool-indexing gotcha).
        let sse = run(&[
            json!({ "id": "c1", "model": "m", "choices": [ { "delta": { "content": "ok " } } ] }),
            json!({ "choices": [ { "delta": { "tool_calls": [ { "index": 0, "id": "t1", "function": { "name": "f", "arguments": "{}" } } ] } } ] }),
            json!({ "choices": [ { "delta": {}, "finish_reason": "tool_calls" } ] }),
        ]);
        let idx = indices(&sse);
        assert_eq!(idx[0], ("content_block_start".into(), 0)); // text
        assert!(idx.contains(&("content_block_stop".into(), 0)));
        assert!(idx.contains(&("content_block_start".into(), 1))); // tool_use
        assert!(sse.contains(r#""type":"text""#));
        assert!(sse.contains(r#""type":"tool_use""#));
    }

    #[test]
    fn sequential_parallel_tool_calls_get_separate_blocks() {
        let sse = run(&[
            json!({ "id": "c1", "model": "m", "choices": [ { "delta": { "tool_calls": [ { "index": 0, "id": "t1", "function": { "name": "a", "arguments": "{}" } } ] } } ] }),
            json!({ "choices": [ { "delta": { "tool_calls": [ { "index": 1, "id": "t2", "function": { "name": "b", "arguments": "{}" } } ] } } ] }),
            json!({ "choices": [ { "delta": {}, "finish_reason": "tool_calls" } ] }),
        ]);
        let starts: Vec<i64> = indices(&sse)
            .into_iter()
            .filter(|(e, _)| e == "content_block_start")
            .map(|(_, i)| i)
            .collect();
        assert_eq!(starts, vec![0, 1]); // two distinct tool_use blocks
        assert_eq!(sse.matches(r#""type":"tool_use""#).count(), 2);
    }

    #[test]
    fn tool_call_with_no_arguments_emits_empty_input_block() {
        let sse = run(&[
            json!({ "id": "c1", "model": "m", "choices": [ { "delta": { "tool_calls": [ { "index": 0, "id": "t1", "function": { "name": "noargs", "arguments": "" } } ] } } ] }),
            json!({ "choices": [ { "delta": {}, "finish_reason": "tool_calls" } ] }),
        ]);
        assert!(sse.contains(r#""type":"tool_use""#));
        assert!(sse.contains(r#""name":"noargs""#));
        // no arguments -> no input_json_delta events
        assert!(!sse.contains("input_json_delta"));
    }

    #[test]
    fn mid_stream_error_becomes_error_event_and_stops() {
        let sse = run(&[
            json!({ "id": "c1", "model": "m", "choices": [ { "delta": { "content": "partial" } } ] }),
            json!({ "error": { "type": "overloaded_error", "message": "backend on fire" } }),
        ]);
        assert!(sse.contains("event: message_start"));
        assert!(sse.contains("event: error"));
        assert!(sse.contains(r#""message":"backend on fire""#));
        // terminal: no normal close-out after an error
        assert!(!sse.contains("event: message_stop"));
        assert!(!sse.contains("event: message_delta"));
    }

    #[test]
    fn reasoning_becomes_thinking_block_before_text() {
        let sse = run(&[
            json!({ "id": "c1", "model": "m", "choices": [ { "delta": { "reasoning_content": "think " } } ] }),
            json!({ "choices": [ { "delta": { "reasoning_content": "more" } } ] }),
            json!({ "choices": [ { "delta": { "content": "answer" } } ] }),
            json!({ "choices": [ { "delta": {}, "finish_reason": "stop" } ] }),
        ]);
        assert!(sse.contains(r#""type":"thinking""#));
        assert!(sse.contains(r#""type":"thinking_delta""#));
        assert!(sse.contains(r#""thinking":"think ""#));
        // thinking block opens at index 0, text block at index 1.
        let starts: Vec<i64> = indices(&sse).into_iter().filter(|(e, _)| e == "content_block_start").map(|(_, i)| i).collect();
        assert_eq!(starts, vec![0, 1]);
        // thinking is emitted before the answer text.
        assert!(sse.find("thinking_delta").unwrap() < sse.find("text_delta").unwrap());
    }

    #[test]
    fn matched_stop_sequence_in_message_delta() {
        let sse = run(&[
            json!({ "id": "c1", "model": "m", "choices": [ { "delta": { "content": "one two" } } ] }),
            json!({ "choices": [ { "delta": {}, "finish_reason": "stop", "stop_reason": "three" } ] }),
        ]);
        assert!(sse.contains(r#""stop_reason":"stop_sequence""#));
        assert!(sse.contains(r#""stop_sequence":"three""#));
    }

    #[test]
    fn streaming_usage_excludes_cached_tokens() {
        let sse = run(&[
            json!({ "id": "c1", "model": "m", "choices": [ { "delta": { "content": "hi" } } ] }),
            json!({ "choices": [ { "delta": {}, "finish_reason": "stop" } ],
                "usage": { "prompt_tokens": 50, "completion_tokens": 4, "prompt_tokens_details": { "cached_tokens": 20 } } }),
        ]);
        assert!(sse.contains(r#""input_tokens":30"#)); // 50 - 20 cached
        assert!(sse.contains(r#""cache_read_input_tokens":20"#));
    }
}
