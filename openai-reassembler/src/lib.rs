//! Reassemble OpenAI-compatible SSE streaming responses into non-streaming format.
//!
//! When an OpenAI-compatible API streams a response as Server-Sent Events (SSE),
//! each event contains a partial "chunk" of the final response. This crate merges
//! those chunks into the equivalent non-streaming JSON response.
//!
//! [`Reassembler`] folds events one at a time, so a caller reading a live stream
//! can drop each event as it arrives and retain only the assembled response. That
//! is deliberately the only entry point: a chunked response carries a full JSON
//! envelope per frame to deliver a few bytes of token delta, and some upstreams
//! emit content-free keepalive frames on top of that, so an API that took the
//! whole stream at once would invite callers to hold orders of magnitude more
//! memory than the answer they are assembling.
//!
//! # Supported formats
//!
//! - **Chat completions** (`/v1/chat/completions`): merges `choices[].delta` fields
//!   into `choices[].message`, concatenating string values (e.g. `content`, `refusal`)
//!   and assembling `tool_calls` by index. Other non-string delta fields use last-value-wins.
//! - **Legacy completions** (`/v1/completions`): concatenates `choices[].text`.
//! - **Responses API** (`/v1/responses`): extracts the full response from the
//!   `response.completed` event.
//! - **Multiple choices**: tracked independently by `index`.
//! - **Usage**: taken from the final chunk.
//!
//! Format detection is automatic: if any event's `event` field (from
//! `eventsource_stream::Event`) starts with `"response."`, the Responses API
//! path is used; otherwise the completions path.

use serde_json::{Map, Value};

/// Protocol-independent evidence used to decide whether a chat completion
/// stopped after producing reasoning but before producing a usable answer.
///
/// Callers feed this from whatever representation they already have. The
/// reassembler derives it from its already-merged response values; downstream
/// consumers can update it from typed values without parsing bodies again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompletionEvidence {
    stopped: bool,
    has_reasoning: bool,
    has_answer: bool,
    has_tool_call: bool,
}

impl CompletionEvidence {
    /// Fold one observation into the completion's accumulated evidence.
    pub fn observe(
        &mut self,
        finish_reason: Option<&str>,
        has_reasoning: bool,
        has_answer: bool,
        has_tool_call: bool,
    ) {
        if let Some(reason) = finish_reason {
            self.stopped = reason == "stop";
        }
        self.has_reasoning |= has_reasoning;
        self.has_answer |= has_answer;
        self.has_tool_call |= has_tool_call;
    }

    /// Whether this is the premature end-of-sequence shape that should not be
    /// treated as a successful, billable completion.
    pub fn is_reasoning_without_answer(&self) -> bool {
        self.stopped && self.has_reasoning && !self.has_answer && !self.has_tool_call
    }
}

/// Incremental accumulator for an OpenAI-compatible SSE stream.
///
/// Folds each event into the assembled response as it arrives, so the caller
/// never keeps the raw stream. Retained memory is proportional to the assembled
/// response, not to the bytes that crossed the wire - a chunked response carries
/// a full JSON envelope per frame to deliver a few bytes of token delta, and
/// some upstreams emit content-free keepalive frames on top of that.
///
/// Auto-detects the stream format:
/// - **Responses API**: if any event's `event` field starts with `"response."`,
///   the full response is taken from the `response.completed` event.
/// - **Completions**: otherwise, merges `choices[].delta` / `choices[].text`.
///   Top-level fields (`id`, `created`, `model`, etc.) come from the first chunk,
///   and `object` has the `.chunk` suffix stripped (`chat.completion.chunk` to
///   `chat.completion`). Responses API objects are left unchanged.
///
/// Events with empty data or `[DONE]` are skipped. Errors are deferred to
/// [`Reassembler::finish`], since the format is not known until a
/// `response.`-prefixed event arrives and a chunk that is malformed as a
/// completions chunk is irrelevant on the Responses API path.
///
/// ```no_run
/// # use openai_reassembler::Reassembler;
/// # fn f(events: impl Iterator<Item = eventsource_stream::Event>) -> anyhow::Result<String> {
/// let mut acc = Reassembler::new();
/// for event in events {
///     acc.push(&event);
///     // `event` is dropped here; nothing retains it.
/// }
/// acc.finish()
/// # }
/// ```
pub struct Reassembler {
    /// Top-level fields of the assembled response, from the first usable chunk.
    base: Option<Value>,
    /// Per-choice accumulated fields, keyed by `index`.
    choices: std::collections::BTreeMap<u64, Map<String, Value>>,
    /// Usage from the most recent chunk that carried it.
    usage: Value,
    /// Data of the most recent `response.completed` event, if any. Only this
    /// one event is needed on the Responses API path, so earlier ones are
    /// discarded as they arrive.
    responses_completed: Option<String>,
    /// Whether any `response.`-prefixed event has been seen.
    is_responses_api: bool,
    /// First chunk-parse failure. Surfaced by `finish` on the completions path
    /// and ignored on the Responses API path.
    chunk_error: Option<anyhow::Error>,
    /// Every event pushed, including skipped ones, for the diagnostic in `finish`.
    total_events: usize,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Reassembler {
    /// Create an empty accumulator.
    pub fn new() -> Self {
        Self {
            base: None,
            choices: Default::default(),
            usage: Value::Null,
            responses_completed: None,
            is_responses_api: false,
            chunk_error: None,
            total_events: 0,
        }
    }

    /// Fold one event into the accumulator.
    ///
    /// The event is not retained; the caller may drop it as soon as this
    /// returns. Parse failures are recorded rather than returned - see the type
    /// docs for why - and surface from [`Reassembler::finish`].
    pub fn push(&mut self, event: &eventsource_stream::Event) {
        self.total_events += 1;

        if event.event.starts_with("response.") {
            self.is_responses_api = true;
            if event.event == "response.completed" {
                self.responses_completed = Some(event.data.clone());
            }
            return;
        }

        if event.data.is_empty() || event.data == "[DONE]" {
            return;
        }

        // The eager path returns on the first bad chunk, so stop folding once
        // one has been seen rather than merging chunks it never would have.
        if self.chunk_error.is_some() {
            return;
        }

        let chunk: Value = match serde_json::from_str(&event.data) {
            Ok(chunk) => chunk,
            Err(e) => {
                self.chunk_error = Some(anyhow::anyhow!("Invalid chunk JSON: {}", e));
                return;
            }
        };

        if self.base.is_none() {
            let mut b = chunk.clone();
            if let Some(obj) = b["object"].as_str() {
                b["object"] = Value::String(obj.strip_suffix(".chunk").unwrap_or(obj).to_string());
            }
            if let Some(m) = b.as_object_mut() {
                m.remove("choices");
                m.remove("usage");
            }
            self.base = Some(b);
        }

        if !chunk["usage"].is_null() {
            self.usage = chunk["usage"].clone();
        }

        let Some(chunk_choices) = chunk["choices"].as_array() else {
            return;
        };

        for choice in chunk_choices {
            let index = choice["index"].as_u64().unwrap_or(0);
            let merged = self.choices.entry(index).or_default();

            if !choice["finish_reason"].is_null() {
                merged.insert("finish_reason".to_string(), choice["finish_reason"].clone());
            }

            // Legacy completions: concatenate "text"
            if let Some(text) = choice["text"].as_str() {
                let existing = merged
                    .entry("text".to_string())
                    .or_insert(Value::String(String::new()));
                if let Value::String(s) = existing {
                    s.push_str(text);
                }
            }

            // Chat completions: merge "delta" into "message"
            if let Some(delta) = choice["delta"].as_object() {
                let message = merged
                    .entry("message".to_string())
                    .or_insert(Value::Object(Map::new()));
                if let Value::Object(msg) = message {
                    for (key, value) in delta {
                        if value.is_null() {
                            continue;
                        }
                        match key.as_str() {
                            "tool_calls" => merge_tool_calls(msg, value),
                            _ => merge_delta_field(msg, key, value),
                        }
                    }
                }
            }
        }
    }

    /// Whether any reassembled chat choice ended after reasoning without an
    /// answer or tool call. This reads state collected during `push`; it does
    /// not parse the assembled response.
    pub fn is_reasoning_without_answer(&self) -> bool {
        !self.is_responses_api
            && self.choices.values().any(|choice| {
                let Some(message) = choice.get("message").and_then(Value::as_object) else {
                    return false;
                };
                let has_reasoning = ["reasoning_content", "reasoning"].iter().any(|key| {
                    message
                        .get(*key)
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty())
                });
                let has_answer = message.get("content").is_some_and(|content| match content {
                    Value::Null => false,
                    Value::String(text) => !text.is_empty(),
                    _ => true,
                });
                let has_tool_call = message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .is_some_and(|calls| !calls.is_empty())
                    || message
                        .get("function_call")
                        .is_some_and(|call| !call.is_null());

                let mut evidence = CompletionEvidence::default();
                evidence.observe(
                    choice.get("finish_reason").and_then(Value::as_str),
                    has_reasoning,
                    has_answer,
                    has_tool_call,
                );
                evidence.is_reasoning_without_answer()
            })
    }

    /// Assemble the accumulated state into a non-streaming response body.
    pub fn finish(self) -> anyhow::Result<String> {
        // The Responses API emits typed events (`response.created`,
        // `response.output_text.delta`, etc.). The final `response.completed`
        // event contains the full response object under the `"response"` key,
        // and everything else on this path is discardable.
        if self.is_responses_api {
            let Some(data) = self.responses_completed else {
                anyhow::bail!("No response.completed event found in Responses API SSE stream")
            };
            let parsed: Value = serde_json::from_str(&data)
                .map_err(|e| anyhow::anyhow!("Invalid response.completed JSON: {}", e))?;
            let Some(response) = parsed.get("response") else {
                anyhow::bail!(
                    "response.completed event JSON does not contain top-level \"response\" field"
                )
            };
            return serde_json::to_string(response).map_err(Into::into);
        }

        if let Some(e) = self.chunk_error {
            return Err(e);
        }

        let Some(mut response) = self.base else {
            anyhow::bail!(
                "SSE stream contained no usable content events ({} total events, all empty or [DONE])",
                self.total_events
            );
        };
        let assembled_choices: Vec<Value> = self
            .choices
            .into_iter()
            .map(|(index, mut fields)| {
                fields.insert("index".to_string(), Value::Number(index.into()));
                if !fields.contains_key("finish_reason") {
                    fields.insert("finish_reason".to_string(), Value::Null);
                }
                Value::Object(fields)
            })
            .collect();
        response["choices"] = Value::Array(assembled_choices);
        response["usage"] = self.usage;

        Ok(response.to_string())
    }
}

/// Merge streamed tool_calls deltas into the accumulated message.
///
/// Tool calls arrive as an array of deltas, each with an `index` field indicating
/// which tool call slot they belong to. `id` and `type` are set once; `function.name`
/// and `function.arguments` are concatenated across chunks.
fn merge_tool_calls(msg: &mut Map<String, Value>, value: &Value) {
    let Some(arr) = value.as_array() else { return };
    let tc_list = msg
        .entry("tool_calls".to_string())
        .or_insert(Value::Array(vec![]));
    let Value::Array(existing) = tc_list else {
        return;
    };

    for tc_delta in arr {
        let idx = tc_delta["index"].as_u64().unwrap_or(0) as usize;
        while existing.len() <= idx {
            existing.push(Value::Object(Map::new()));
        }
        let slot = existing[idx].as_object_mut().unwrap();

        // Set id and type (arrive once, on the first delta for this tool call)
        for field in ["id", "type"] {
            if let Some(v) = tc_delta.get(field)
                && !v.is_null()
            {
                slot.insert(field.to_string(), v.clone());
            }
        }

        // Concatenate function name and arguments
        if let Some(func) = tc_delta["function"].as_object() {
            let f = slot
                .entry("function".to_string())
                .or_insert(Value::Object(Map::new()))
                .as_object_mut()
                .unwrap();
            for field in ["name", "arguments"] {
                if let Some(s) = func.get(field).and_then(|v| v.as_str()) {
                    let existing = f
                        .entry(field.to_string())
                        .or_insert(Value::String(String::new()));
                    if let Value::String(es) = existing {
                        es.push_str(s);
                    }
                }
            }
        }
    }
}

/// Merge a single delta field into the accumulated message.
///
/// String fields (content, refusal, etc.) are concatenated.
/// The `role` field uses last-value-wins (providers may send it on every chunk).
/// Non-string fields use last-value-wins.
fn merge_delta_field(msg: &mut Map<String, Value>, key: &str, value: &Value) {
    if key == "role" {
        msg.insert(key.to_string(), value.clone());
    } else if let Some(s) = value.as_str() {
        let existing = msg
            .entry(key.to_string())
            .or_insert(Value::String(String::new()));
        if let Value::String(existing_str) = existing {
            existing_str.push_str(s);
        }
    } else {
        msg.insert(key.to_string(), value.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Once;

    static GENERATE: Once = Once::new();

    /// If BASE_URL, MODEL, and FIXTURE_NAME are set, generate fixtures for that
    /// provider once before tests run. Fixtures are written to
    /// `fixtures/{FIXTURE_NAME}/`.
    fn ensure_fixtures() {
        GENERATE.call_once(|| {
            let (Ok(base_url), Ok(model), Ok(fixture_name)) = (
                std::env::var("BASE_URL"),
                std::env::var("MODEL"),
                std::env::var("FIXTURE_NAME"),
            ) else {
                return;
            };
            let api_key = std::env::var("API_KEY").unwrap_or_else(|_| "none".to_string());
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let fixtures_dir = root.join("fixtures").join(&fixture_name);
            std::fs::create_dir_all(&fixtures_dir).unwrap();

            let cases: Value = serde_json::from_str(
                &std::fs::read_to_string(root.join("test_cases.json")).unwrap(),
            )
            .unwrap();

            let rt = tokio::runtime::Runtime::new().unwrap();
            let client = reqwest::Client::new();

            for (name, case) in cases.as_object().unwrap() {
                let endpoint = case["endpoint"].as_str().unwrap();
                if endpoint.ends_with("/responses") {
                    rt.block_on(record_responses_fixture(
                        &client,
                        &base_url,
                        &api_key,
                        &model,
                        name,
                        case,
                        &fixtures_dir,
                    ));
                } else {
                    rt.block_on(record_fixture(
                        &client,
                        &base_url,
                        &api_key,
                        &model,
                        name,
                        case,
                        &fixtures_dir,
                    ));
                }
            }
        });
    }

    /// Discover all fixture provider directories under `fixtures/`.
    fn fixture_providers() -> Vec<String> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let fixtures_dir = root.join("fixtures");
        let mut providers: Vec<String> = std::fs::read_dir(&fixtures_dir)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                if entry.file_type().ok()?.is_dir() {
                    Some(entry.file_name().to_string_lossy().to_string())
                } else {
                    None
                }
            })
            .collect();
        providers.sort();
        providers
    }

    async fn record_fixture(
        client: &reqwest::Client,
        base_url: &str,
        api_key: &str,
        model: &str,
        name: &str,
        case: &Value,
        fixtures_dir: &Path,
    ) {
        let endpoint = case["endpoint"].as_str().unwrap();
        let url = format!("{base_url}{endpoint}");
        let mut body = case["body"].as_object().unwrap().clone();
        body.insert("model".to_string(), Value::String(model.to_string()));
        body.insert("temperature".to_string(), Value::Number(0.into()));
        body.insert("seed".to_string(), Value::Number(42.into()));

        // Non-streaming
        let mut non_stream_body = body.clone();
        non_stream_body.insert("stream".to_string(), Value::Bool(false));
        eprintln!("[{name}] POST {url} (non-streaming)");
        let expected: Value = client
            .post(&url)
            .bearer_auth(api_key)
            .json(&non_stream_body)
            .send()
            .await
            .unwrap_or_else(|e| panic!("{name}: non-streaming request failed: {e}"))
            .json()
            .await
            .unwrap_or_else(|e| panic!("{name}: non-streaming parse failed: {e}"));
        eprintln!("[{name}] non-streaming response received");

        // Streaming
        let mut stream_body = body.clone();
        stream_body.insert("stream".to_string(), Value::Bool(true));
        let mut stream_opts = serde_json::Map::new();
        stream_opts.insert("include_usage".to_string(), Value::Bool(true));
        stream_body.insert("stream_options".to_string(), Value::Object(stream_opts));

        eprintln!("[{name}] POST {url} (streaming)");
        let response_text = client
            .post(&url)
            .bearer_auth(api_key)
            .json(&stream_body)
            .send()
            .await
            .unwrap_or_else(|e| panic!("{name}: streaming request failed: {e}"))
            .text()
            .await
            .unwrap_or_else(|e| panic!("{name}: streaming read failed: {e}"));

        let mut chunks: Vec<Value> = vec![];
        for line in response_text.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                if data == "[DONE]" {
                    chunks.push(Value::String("[DONE]".to_string()));
                } else if let Ok(parsed) = serde_json::from_str::<Value>(data) {
                    chunks.push(parsed);
                }
            }
        }

        eprintln!("[{name}] streaming response: {} chunks", chunks.len());

        let fixture = serde_json::json!({ "chunks": chunks, "expected": expected });
        let path = fixtures_dir.join(format!("{name}.json"));
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&fixture).unwrap() + "\n",
        )
        .unwrap_or_else(|e| panic!("{name}: failed to write fixture: {e}"));
        eprintln!("[{name}] fixture written to {}", path.display());
    }

    /// Record a fixture for the Responses API.
    ///
    /// Unlike completions, responses SSE events are typed (e.g. `event: response.created`)
    /// and the non-streaming request omits `stream` entirely rather than setting it to false.
    /// Usage is always included in `response.completed` without needing `stream_options`.
    async fn record_responses_fixture(
        client: &reqwest::Client,
        base_url: &str,
        api_key: &str,
        model: &str,
        name: &str,
        case: &Value,
        fixtures_dir: &Path,
    ) {
        let endpoint = case["endpoint"].as_str().unwrap();
        let url = format!("{base_url}{endpoint}");
        let mut body = case["body"].as_object().unwrap().clone();
        body.insert("model".to_string(), Value::String(model.to_string()));
        body.insert("temperature".to_string(), Value::Number(0.into()));
        body.insert("seed".to_string(), Value::Number(42.into()));

        // Non-streaming (no stream field at all for responses API)
        eprintln!("[{name}] POST {url} (non-streaming)");
        let expected: Value = client
            .post(&url)
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
            .unwrap_or_else(|e| panic!("{name}: non-streaming request failed: {e}"))
            .json()
            .await
            .unwrap_or_else(|e| panic!("{name}: non-streaming parse failed: {e}"));
        eprintln!("[{name}] non-streaming response received");

        // Streaming
        body.insert("stream".to_string(), Value::Bool(true));

        eprintln!("[{name}] POST {url} (streaming)");
        let response_text = client
            .post(&url)
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
            .unwrap_or_else(|e| panic!("{name}: streaming request failed: {e}"))
            .text()
            .await
            .unwrap_or_else(|e| panic!("{name}: streaming read failed: {e}"));

        // Parse SSE events preserving event types (spec-compliant: accumulate
        // data lines until a blank line delimits the event).
        let mut events: Vec<Value> = vec![];
        let mut current_event_type: Option<String> = None;
        let mut current_data_lines: Vec<String> = Vec::new();

        for raw_line in response_text.lines() {
            let line = raw_line.trim_end_matches('\r');
            if line.is_empty() {
                if !current_data_lines.is_empty() {
                    let data_str = current_data_lines.join("\n");
                    if data_str != "[DONE]"
                        && let Ok(parsed) = serde_json::from_str::<Value>(&data_str)
                    {
                        let event_type = current_event_type.clone().unwrap_or_default();
                        events
                            .push(serde_json::json!({ "event_type": event_type, "data": parsed }));
                    }
                }
                current_event_type = None;
                current_data_lines.clear();
            } else if let Some(event_type) = line
                .strip_prefix("event: ")
                .or_else(|| line.strip_prefix("event:"))
            {
                current_event_type = Some(event_type.to_string());
            } else if let Some(data) = line
                .strip_prefix("data: ")
                .or_else(|| line.strip_prefix("data:"))
            {
                current_data_lines.push(data.to_string());
            }
        }

        // Finalize any event not terminated by a trailing blank line
        if !current_data_lines.is_empty() {
            let data_str = current_data_lines.join("\n");
            if data_str != "[DONE]"
                && let Ok(parsed) = serde_json::from_str::<Value>(&data_str)
            {
                let event_type = current_event_type.clone().unwrap_or_default();
                events.push(serde_json::json!({ "event_type": event_type, "data": parsed }));
            }
        }

        eprintln!("[{name}] streaming response: {} events", events.len());

        let fixture = serde_json::json!({ "events": events, "expected": expected });
        let path = fixtures_dir.join(format!("{name}.json"));
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&fixture).unwrap() + "\n",
        )
        .unwrap_or_else(|e| panic!("{name}: failed to write fixture: {e}"));
        eprintln!("[{name}] fixture written to {}", path.display());
    }

    /// Recursively compare two JSON values, collecting mismatches.
    /// Fields in `skip` are skipped at any nesting depth.
    fn diff(
        actual: &Value,
        expected: &Value,
        path: &str,
        skip: &[String],
        errors: &mut Vec<String>,
    ) {
        match (actual, expected) {
            (Value::Object(a), Value::Object(e)) => {
                for (key, ev) in e {
                    if skip.iter().any(|s| s == key) {
                        continue;
                    }
                    let p = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    match a.get(key) {
                        Some(av) => diff(av, ev, &p, skip, errors),
                        None if ev.is_null() => {} // missing field == explicit null
                        None => errors.push(format!("{p}: missing from reassembled output")),
                    }
                }
                for key in a.keys() {
                    if skip.iter().any(|s| s == key) {
                        continue;
                    }
                    if !e.contains_key(key) {
                        let p = if path.is_empty() {
                            key.clone()
                        } else {
                            format!("{path}.{key}")
                        };
                        errors.push(format!("{p}: unexpected field in reassembled output"));
                    }
                }
            }
            (Value::Array(a), Value::Array(e)) => {
                if a.len() != e.len() {
                    errors.push(format!(
                        "{path}: array length {}, expected {}",
                        a.len(),
                        e.len()
                    ));
                    return;
                }
                for (i, (av, ev)) in a.iter().zip(e).enumerate() {
                    diff(av, ev, &format!("{path}[{i}]"), skip, errors);
                }
            }
            _ => {
                if actual != expected {
                    // Tool call arguments: compare as parsed JSON (whitespace may differ)
                    if path.ends_with(".arguments")
                        && let (Some(a), Some(e)) = (actual.as_str(), expected.as_str())
                    {
                        let ap: Result<Value, _> = serde_json::from_str(a);
                        let ep: Result<Value, _> = serde_json::from_str(e);
                        if let (Ok(ap), Ok(ep)) = (ap, ep)
                            && ap == ep
                        {
                            return;
                        }
                    }
                    errors.push(format!("{path}: got {actual}, expected {expected}"));
                }
            }
        }
    }

    fn assert_fixture(provider: &str, name: &str) {
        ensure_fixtures();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

        // Load allowed_mismatches from test_cases.json
        let cases: Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("test_cases.json")).unwrap())
                .unwrap();
        let skip: Vec<String> = cases[name]["allowed_mismatches"]
            .as_array()
            .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
            .unwrap_or_default();

        let path = root
            .join("fixtures")
            .join(provider)
            .join(format!("{name}.json"));
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("missing fixture {}: {e}", path.display()));
        let fixture: Value = serde_json::from_str(&content).unwrap();

        let events: Vec<eventsource_stream::Event> = fixture["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|chunk| eventsource_stream::Event {
                data: if chunk.is_string() {
                    chunk.as_str().unwrap().to_string()
                } else {
                    chunk.to_string()
                },
                ..Default::default()
            })
            .collect();

        let actual: Value = serde_json::from_str(&reassemble(&events).unwrap()).unwrap();

        let mut errors = vec![];
        diff(&actual, &fixture["expected"], "", &skip, &mut errors);
        if !errors.is_empty() {
            panic!("fixture {provider}/{name}:\n{}", errors.join("\n"));
        }
    }

    /// Load a Responses API fixture and verify reassembly matches the expected response.
    ///
    /// Responses fixtures store events as `{ "event_type": ..., "data": ... }` objects
    /// under the `"events"` key (not `"chunks"`).
    fn assert_responses_fixture(provider: &str, name: &str) {
        ensure_fixtures();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

        let cases: Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("test_cases.json")).unwrap())
                .unwrap();
        let skip: Vec<String> = cases[name]["allowed_mismatches"]
            .as_array()
            .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
            .unwrap_or_default();

        let path = root
            .join("fixtures")
            .join(provider)
            .join(format!("{name}.json"));
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("missing fixture {}: {e}", path.display()));
        let fixture: Value = serde_json::from_str(&content).unwrap();

        let events: Vec<eventsource_stream::Event> = fixture["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|ev| eventsource_stream::Event {
                event: ev["event_type"].as_str().unwrap_or_default().to_string(),
                data: ev["data"].to_string(),
                ..Default::default()
            })
            .collect();

        let actual: Value = serde_json::from_str(&reassemble(&events).unwrap()).unwrap();

        let mut errors = vec![];
        diff(&actual, &fixture["expected"], "", &skip, &mut errors);
        if !errors.is_empty() {
            panic!("fixture {provider}/{name}:\n{}", errors.join("\n"));
        }
    }

    /// Dynamically test all fixtures across all providers.
    ///
    /// Iterates over each subdirectory in `fixtures/` (each is a provider like
    /// "vllm" or "dynamo"), and for each fixture file found, runs the appropriate
    /// assertion based on whether it's a responses or completions fixture.
    #[test]
    fn all_fixtures() {
        ensure_fixtures();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let cases: Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("test_cases.json")).unwrap())
                .unwrap();

        let providers = fixture_providers();
        assert!(
            !providers.is_empty(),
            "No fixture provider directories found under fixtures/"
        );

        for provider in &providers {
            let provider_dir = root.join("fixtures").join(provider);
            let mut ran = 0;
            for (name, case) in cases.as_object().unwrap() {
                let fixture_path = provider_dir.join(format!("{name}.json"));
                if !fixture_path.exists() {
                    eprintln!("[skip] {provider}/{name}: fixture file not present");
                    continue;
                }

                let endpoint = case["endpoint"].as_str().unwrap();
                eprintln!("[test] {provider}/{name}");
                if endpoint.ends_with("/responses") {
                    assert_responses_fixture(provider, name);
                } else {
                    assert_fixture(provider, name);
                }
                ran += 1;
            }
            assert!(ran > 0, "Provider {provider} has no fixture files");
        }
    }

    /// Verify that `role` sent on every chunk is not concatenated (Dynamo-style streams).
    #[test]
    fn role_not_concatenated() {
        let events: Vec<eventsource_stream::Event> = vec![
            eventsource_stream::Event {
                data: r#"{"id":"1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"}}]}"#.to_string(),
                ..Default::default()
            },
            eventsource_stream::Event {
                data: r#"{"id":"1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":" world"}}]}"#.to_string(),
                ..Default::default()
            },
            eventsource_stream::Event {
                data: r#"{"id":"1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"!"},"finish_reason":"stop"}]}"#.to_string(),
                ..Default::default()
            },
        ];

        let result: Value = serde_json::from_str(&reassemble(&events).unwrap()).unwrap();
        let message = &result["choices"][0]["message"];
        assert_eq!(message["role"], "assistant");
        assert_eq!(message["content"], "Hello world!");
    }

    #[test]
    fn completion_evidence_requires_reasoning_without_answer_or_tool() {
        let mut failed = CompletionEvidence::default();
        failed.observe(None, true, false, false);
        failed.observe(Some("stop"), false, false, false);
        assert!(failed.is_reasoning_without_answer());

        let mut answered = failed.clone();
        answered.observe(None, false, true, false);
        assert!(!answered.is_reasoning_without_answer());

        let mut tool_call = failed;
        tool_call.observe(None, false, false, true);
        assert!(!tool_call.is_reasoning_without_answer());

        let mut capped = CompletionEvidence::default();
        capped.observe(Some("length"), true, false, false);
        assert!(!capped.is_reasoning_without_answer());
    }

    #[test]
    fn reassembler_classifies_from_its_existing_chunk_parse() {
        let events = [
            ev(
                "",
                r#"{"id":"1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"reasoning_content":"thinking"}}]}"#,
            ),
            ev(
                "",
                r#"{"id":"1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ),
        ];
        let mut reassembler = Reassembler::new();
        for event in &events {
            reassembler.push(event);
        }

        assert!(reassembler.is_reasoning_without_answer());
    }

    /// Build an event with the given type and data.
    fn ev(event: &str, data: &str) -> eventsource_stream::Event {
        eventsource_stream::Event {
            event: event.to_string(),
            data: data.to_string(),
            ..Default::default()
        }
    }

    /// Fold a whole slice through the accumulator. Real callers read a live
    /// stream and push as they go; tests already have the events in hand.
    fn reassemble(events: &[eventsource_stream::Event]) -> anyhow::Result<String> {
        let mut acc = Reassembler::new();
        for event in events {
            acc.push(event);
        }
        acc.finish()
    }

    /// Some upstreams emit keepalive frames at a high rate for the life of a
    /// request: well-formed chunks whose delta fields are all null. They merge
    /// to nothing, so they must not reach the output, and folding them must not
    /// be quadratic. A caller that buffers them instead pays for every one of
    /// them until the stream closes, which for a long request dwarfs the
    /// response being assembled.
    #[test]
    fn content_free_keepalive_frames_contribute_nothing() {
        const KEEPALIVE: &str = r#"{"id":"dyn-1","model":"m","usage":null,"object":"chat.completion.chunk","choices":[{"delta":{"role":null,"content":null,"refusal":null,"tool_calls":null,"function_call":null},"index":0}],"created":1,"service_tier":null,"system_fingerprint":null}"#;
        let first = ev(
            "",
            r#"{"id":"dyn-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"Hi"}}]}"#,
        );
        let last = ev(
            "",
            r#"{"id":"dyn-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"!"},"finish_reason":"stop"}]}"#,
        );

        let mut noisy = vec![first.clone()];
        noisy.extend(std::iter::repeat_with(|| ev("", KEEPALIVE)).take(20_000));
        noisy.push(last.clone());

        let clean = vec![first, last];

        assert_eq!(
            reassemble(&noisy).unwrap(),
            reassemble(&clean).unwrap(),
            "20,000 all-null-delta frames changed the assembled response"
        );

        let assembled: Value = serde_json::from_str(&reassemble(&noisy).unwrap()).unwrap();
        assert_eq!(assembled["choices"][0]["message"]["content"], "Hi!");
        assert_eq!(assembled["choices"][0]["finish_reason"], "stop");
    }

    /// Only the final `response.completed` matters, so earlier ones are dropped
    /// as they arrive rather than kept for a reverse scan at the end.
    #[test]
    fn responses_api_uses_the_last_completed_event() {
        let events = vec![
            ev("response.created", r#"{"response":{"id":"created"}}"#),
            ev("response.completed", r#"{"response":{"id":"first"}}"#),
            ev("response.completed", r#"{"response":{"id":"last"}}"#),
        ];

        let out: Value = serde_json::from_str(&reassemble(&events).unwrap()).unwrap();
        assert_eq!(out["id"], "last");
    }

    /// The format is not known until a `response.`-prefixed event arrives, so a
    /// chunk that is malformed *as a completions chunk* must stay deferred. The
    /// eager path decides the format before parsing anything and never looks at
    /// this event; the incremental path has to reach the same answer.
    #[test]
    fn malformed_chunk_is_ignored_on_the_responses_path() {
        let events = vec![
            ev("", "not json"),
            ev("response.completed", r#"{"response":{"id":"ok"}}"#),
        ];

        let out: Value = serde_json::from_str(&reassemble(&events).unwrap()).unwrap();
        assert_eq!(out["id"], "ok");
    }

    /// ...but on a completions stream it is still an error, and still the first one.
    #[test]
    fn malformed_chunk_errors_on_the_completions_path() {
        let events = vec![ev("", "not json"), ev("", "also not json")];

        let err = reassemble(&events).unwrap_err().to_string();
        assert!(err.contains("Invalid chunk JSON"), "got: {err}");
    }

    /// A stream of nothing but keepalives assembles to no usable content rather
    /// than to an empty success, and the count in the message is every event
    /// pushed, not just the parsed ones.
    #[test]
    fn a_stream_with_no_usable_events_reports_the_total() {
        let events = vec![ev("", ""), ev("", "[DONE]"), ev("", "")];

        let err = reassemble(&events).unwrap_err().to_string();
        assert!(err.contains("3 total events"), "got: {err}");
    }

    /// `.chunk` is stripped as a *suffix*, not replaced wherever it occurs. This
    /// crate reassembles whatever an OpenAI-compatible provider emits rather
    /// than a fixed set of object names, so a global substring replace would
    /// silently corrupt any name that contained `.chunk` elsewhere.
    #[test]
    fn chunk_is_stripped_as_a_suffix_not_replaced_globally() {
        let stream = |object: &str| {
            vec![ev(
                "",
                &format!(
                    r#"{{"object":"{object}","choices":[{{"index":0,"delta":{{"content":"x"}}}}]}}"#
                ),
            )]
        };

        let out: Value =
            serde_json::from_str(&reassemble(&stream("chat.completion.chunk")).unwrap()).unwrap();
        assert_eq!(out["object"], "chat.completion");

        let out: Value = serde_json::from_str(&reassemble(&stream("a.chunk.b")).unwrap()).unwrap();
        assert_eq!(
            out["object"], "a.chunk.b",
            "interior .chunk must be left alone"
        );

        // Legacy completions reuse one object name for streaming and not, so
        // the strip must be a no-op there.
        let out: Value =
            serde_json::from_str(&reassemble(&stream("text_completion")).unwrap()).unwrap();
        assert_eq!(out["object"], "text_completion");
    }
}
