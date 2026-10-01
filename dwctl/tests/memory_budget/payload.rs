//! Chat completion requests and responses shaped like everyday agent traffic.
//!
//! A request is an agent loop: a system prompt, tool definitions, the user's
//! task, then turns of assistant tool calls and tool results, ending on a tool
//! result. A response spends most of its output reasoning, then answers briefly
//! and calls a tool. Streamed, the reasoning and answer arrive a few tokens per
//! event, as a model server with speculative decoding sends them, and the tool
//! call arrives whole.

use axum::body::Bytes;
use serde_json::{Value, json};

/// Bytes of text per output token, cycled; about what a tokenizer produces for
/// English and code.
const TOKEN_LENGTHS: [usize; 8] = [4, 5, 4, 3, 6, 4, 5, 3];
/// Output tokens per streamed event.
const TOKENS_PER_EVENT: usize = 3;
/// Shares of output tokens, in percent, spent reasoning and answering. The
/// rest are the tool call's arguments.
const REASONING_PERCENT: usize = 70;
const ANSWER_PERCENT: usize = 10;
const TOOLS: usize = 10;
/// Tool results vary widely in size; a request cycles through these, in KiB.
const TOOL_RESULT_KIB: [usize; 8] = [1, 3, 2, 8, 1, 4, 20, 2];
const PROMPT_TOKENS: usize = 24_000;
const ID: &str = "chatcmpl-5f0c2a9e-7b1d-4e8a-9c36-2d4b8e1f6a07";
const CREATED: u64 = 1_767_225_600;

/// Text with the mix of prose, code, quotes and newlines that prompts carry.
const PASSAGE: &str = r#"The scheduler admits a request once its prompt fits in the remaining cache budget.
When it does not, the request waits in the queue and is retried on the next step.

```python
def admit(request, budget):
    if request["prompt_tokens"] > budget.free:
        return False  # wait for "free" to grow
    budget.free -= request["prompt_tokens"]
    return True
```

Run `pytest -k admit` to check the change; the failing case was a prompt of exactly
the remaining size, which the old comparison ("<" rather than "<=") rejected.
"#;

/// `bytes` bytes of text, starting `offset` bytes into the passage.
pub fn text(bytes: usize, offset: usize) -> String {
    PASSAGE
        .bytes()
        .cycle()
        .skip(offset % PASSAGE.len())
        .take(bytes)
        .map(char::from)
        .collect()
}

/// A chat request for `model` whose JSON body is at least `bytes` long, with as
/// many turns as that takes and at least one.
pub fn chat_request(model: &str, bytes: usize, stream: bool) -> Bytes {
    let mut messages = vec![
        json!({ "role": "system", "content": text(8 * 1024, 0) }),
        json!({ "role": "user", "content": text(1024, 5) }),
    ];
    loop {
        messages.extend(turn(messages.len() / 2));
        let body = request_body(model, &messages, stream);
        if body.len() >= bytes {
            return body;
        }
    }
}

fn request_body(model: &str, messages: &[Value], stream: bool) -> Bytes {
    let mut body = json!({
        "model": model,
        "messages": messages,
        "tools": tools(),
        "tool_choice": "auto",
        "max_tokens": 16384,
        "temperature": 0.6,
        "stream": stream,
    });
    if stream {
        body["stream_options"] = json!({ "include_usage": true });
    }
    Bytes::from(serde_json::to_vec(&body).unwrap())
}

/// One turn of the loop: the assistant calls a tool and the tool answers.
fn turn(turn: usize) -> [Value; 2] {
    let call_id = format!("call_{turn:08}");
    let arguments = json!({ "path": format!("src/module_{turn}.py"), "start_line": 1, "end_line": 200 });
    [
        json!({
            "role": "assistant",
            "content": text(160, turn * 11),
            "tool_calls": [{
                "id": call_id,
                "type": "function",
                "function": { "name": "tool_0", "arguments": arguments.to_string() },
            }],
        }),
        json!({
            "role": "tool",
            "tool_call_id": call_id,
            "content": text(TOOL_RESULT_KIB[turn % TOOL_RESULT_KIB.len()] * 1024, turn * 13),
        }),
    ]
}

/// Tool definitions with a paragraph of description and a few documented parameters each.
fn tools() -> Value {
    let tools: Vec<Value> = (0..TOOLS)
        .map(|tool| {
            let properties: serde_json::Map<String, Value> = (0..4)
                .map(|parameter| {
                    let kind = if parameter % 2 == 0 { "string" } else { "integer" };
                    let schema = json!({ "type": kind, "description": text(120, tool * 31 + parameter * 7) });
                    (format!("parameter_{parameter}"), schema)
                })
                .collect();
            json!({
                "type": "function",
                "function": {
                    "name": format!("tool_{tool}"),
                    "description": text(1200, tool * 17),
                    "parameters": { "type": "object", "properties": properties, "required": ["parameter_0"] },
                },
            })
        })
        .collect();
    Value::Array(tools)
}

/// The output of a response with `tokens` tokens.
struct Output {
    /// Reasoning and answer text, a token per piece.
    reasoning: Vec<String>,
    answer: Vec<String>,
    /// The tool call's arguments, as JSON text.
    arguments: String,
    tokens: usize,
}

impl Output {
    fn new(tokens: usize) -> Output {
        let mut offset = 0;
        let mut pieces = (0..).map(|token| {
            let len = TOKEN_LENGTHS[token % TOKEN_LENGTHS.len()];
            let piece = text(len, offset);
            offset += len;
            piece
        });
        let reasoning = pieces.by_ref().take(tokens * REASONING_PERCENT / 100).collect();
        let answer = pieces.by_ref().take(tokens * ANSWER_PERCENT / 100).collect();
        let argument_tokens = tokens - tokens * REASONING_PERCENT / 100 - tokens * ANSWER_PERCENT / 100;
        let content: String = pieces.take(argument_tokens).collect();
        let arguments = json!({ "path": "src/scheduler.py", "content": content }).to_string();
        Output {
            reasoning,
            answer,
            arguments,
            tokens,
        }
    }

    fn tool_call(&self) -> Value {
        json!({
            "id": "call_5d2e8f61",
            "type": "function",
            "function": { "name": "tool_1", "arguments": self.arguments },
        })
    }

    fn usage(&self) -> Value {
        json!({
            "prompt_tokens": PROMPT_TOKENS,
            "completion_tokens": self.tokens,
            "total_tokens": PROMPT_TOKENS + self.tokens,
            "prompt_tokens_details": { "cached_tokens": PROMPT_TOKENS * 7 / 8 },
            "completion_tokens_details": { "reasoning_tokens": self.reasoning.len() },
        })
    }
}

/// A complete (non-streaming) response with `tokens` tokens of output.
pub fn completion(model: &str, tokens: usize) -> Bytes {
    let output = Output::new(tokens);
    let body = json!({
        "id": ID,
        "object": "chat.completion",
        "created": CREATED,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": output.answer.concat(),
                "reasoning_content": output.reasoning.concat(),
                "tool_calls": [output.tool_call()],
            },
            "logprobs": null,
            "finish_reason": "tool_calls",
        }],
        "service_tier": null,
        "system_fingerprint": null,
        "usage": output.usage(),
    });
    Bytes::from(serde_json::to_vec(&body).unwrap())
}

/// A streamed response, as server-sent events.
pub struct Stream {
    /// Events carrying the output.
    pub content: Vec<Bytes>,
    /// The finish reason, usage and end-of-stream events.
    pub finish: Vec<Bytes>,
}

impl Stream {
    pub fn content_bytes(&self) -> usize {
        self.content.iter().map(Bytes::len).sum()
    }
}

/// A streamed response with `tokens` tokens of output.
pub fn stream(model: &str, tokens: usize) -> Stream {
    let output = Output::new(tokens);
    let chunk = |choices: Value, usage: Value| {
        sse(&json!({
            "id": ID,
            "object": "chat.completion.chunk",
            "created": CREATED,
            "model": model,
            "choices": choices,
            "service_tier": null,
            "system_fingerprint": null,
            "usage": usage,
        }))
    };
    let delta = |mut delta: Value, finish_reason: Value| {
        delta["role"] = json!("assistant");
        chunk(
            json!([{ "index": 0, "delta": delta, "logprobs": null, "finish_reason": finish_reason }]),
            Value::Null,
        )
    };
    let mut content = Vec::new();
    for tokens in output.reasoning.chunks(TOKENS_PER_EVENT) {
        content.push(delta(json!({ "reasoning_content": tokens.concat() }), Value::Null));
    }
    for tokens in output.answer.chunks(TOKENS_PER_EVENT) {
        content.push(delta(json!({ "content": tokens.concat() }), Value::Null));
    }
    let mut call = output.tool_call();
    call["index"] = json!(0);
    content.push(delta(json!({ "tool_calls": [call] }), Value::Null));
    Stream {
        content,
        finish: vec![
            delta(json!({}), json!("tool_calls")),
            chunk(json!([]), output.usage()),
            Bytes::from_static(b"data: [DONE]\n\n"),
        ],
    }
}

fn sse(value: &Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}
