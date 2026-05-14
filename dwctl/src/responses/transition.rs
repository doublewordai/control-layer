//! Open Responses transition function: chain → next step.
//!
//! Given the current chain of completed steps and the parent fusillade
//! request's body, decide whether the loop should fire another model
//! call, dispatch tool calls, complete, or fail.
//!
//! ## Protocol shape
//!
//! Onwards' multi-step storage is JSON-payload-typed. We pin a concrete
//! protocol shape here so the implementation has something to parse and
//! produce; this is what the integration test speaks.
//!
//! - **Parent request body** (the fusillade row's `body` column, set
//!   when the `/v1/responses` POST handler created the row):
//!   ```json
//!   { "model": "gpt-4o", "input": [{"role":"user","content":"hi"}] }
//!   ```
//!
//! - **Upstream model_call request_payload** (what we POST upstream):
//!   ```json
//!   {
//!     "model": "gpt-4o",
//!     "messages": [...running conversation...],
//!     "tools": [...resolved tool schemas (optional)...]
//!   }
//!   ```
//!   This is OpenAI Chat Completions shape — what most upstream models
//!   accept. The real Responses API can be wired later by adapting
//!   the `prepare_model_call` helper.
//!
//! - **Upstream model response_payload** (what we receive back):
//!   ```json
//!   {
//!     "choices": [{
//!       "message": {
//!         "role": "assistant",
//!         "content": "...",
//!         "tool_calls": [{"id":"call_1","type":"function","function":{"name":"x","arguments":"{}"}}]
//!       },
//!       "finish_reason": "tool_calls" | "stop"
//!     }]
//!   }
//!   ```
//!
//! - **tool_call step request_payload** (what `DwctlStepExecutor`/the
//!   loop reads):
//!   ```json
//!   { "name": "get_weather", "args": {"city":"Paris"}, "call_id":"call_1" }
//!   ```
//!
//! ## Why not import onwards' Responses schemas
//!
//! Onwards' `strict::schemas::responses` module has 1.3kloc of strictly
//! typed Open Responses request/response structs. We deliberately work
//! with `serde_json::Value` here so the transition function (and its
//! tests) stay decoupled from the strict-mode adapter — wiring those in
//! is a future cleanup once the multi-step path is live.

use std::collections::HashSet;

use onwards::{ChainStep, NextAction, StepDescriptor, StepKind, StepState};
use serde_json::{Value, json};

/// Decode the parent fusillade request body into the Responses-API shape
/// we care about. Returns the user's input messages + the model name.
pub(crate) struct ParsedRequest {
    pub model: String,
    pub initial_messages: Vec<Value>,
    pub tools: Option<Value>,
    /// User's `stream` flag from the parent body. Propagated verbatim
    /// onto every model_call request_payload we construct so the
    /// upstream HTTP fire honors the user's choice (and the loop
    /// forwards token deltas to the sink only when this is true).
    pub stream: bool,
}

pub(crate) fn parse_parent_request(body: &str) -> Result<ParsedRequest, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("parent body parse: {e}"))?;
    let model = v
        .get("model")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "parent body missing 'model'".to_string())?
        .to_string();

    let initial_messages = if let Some(input) = v.get("input") {
        // Open Responses 'input' may be a string or an array of items
        match input {
            Value::String(s) => vec![json!({"role": "user", "content": s})],
            Value::Array(items) => translate_input_items(items)?,
            _ => return Err("'input' must be string or array".into()),
        }
    } else if let Some(messages) = v.get("messages").and_then(|m| m.as_array()) {
        // Direct chat-completions shape — pass through verbatim.
        messages.clone()
    } else {
        return Err("parent body missing 'input' or 'messages'".into());
    };

    let tools = v.get("tools").map(normalize_tools);
    let stream = v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    Ok(ParsedRequest {
        model,
        initial_messages,
        tools,
        stream,
    })
}

/// Translate Open Responses `input` items into chat-completions `messages`
/// shape. The upstream model speaks chat-completions, so every message
/// must carry a `role`, and tool_call items must live as `tool_calls`
/// arrays on assistant messages — not as free-floating items.
///
/// Per-item rules:
/// - `{type:"message", role, content}` → `{role, content}` (drop `type`)
/// - `{type:"function_call", call_id, name, arguments}` → folded onto the
///   preceding assistant message's `tool_calls` array if there is one;
///   otherwise a new `{role:"assistant", content:null, tool_calls:[…]}`
///   message. Consecutive function_calls collapse into a single assistant
///   message with multiple `tool_calls`. An assistant text message
///   immediately followed by function_calls absorbs them so the upstream
///   sees one combined `{role:"assistant", content:"…", tool_calls:[…]}`.
/// - `{type:"function_call_output", call_id, output}` →
///   `{role:"tool", tool_call_id:call_id, content:output}`
/// - `{type:"reasoning", …}` → dropped. Reasoning items are for client
///   display; chat-completions has no equivalent shape and most upstream
///   models reject the type. Dropping is the safe default.
///
/// Without this translation, `input.clone()` was passed verbatim to the
/// model, and any non-`message` item produced a chat-completions message
/// with no `role` field — the upstream then 422'd with
/// `messages[N]: missing field 'role'`, breaking every multi-turn
/// tool-calling conversation.
fn translate_input_items(items: &[Value]) -> Result<Vec<Value>, String> {
    let mut out: Vec<Value> = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        // `type` defaults to "message" so historical clients that send
        // bare `{role, content}` items still translate correctly.
        let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("message");
        match item_type {
            "message" => {
                // A non-object item (string/number/array) can't carry
                // `role`, so produce an explicit error here rather
                // than letting the missing-`role` check below report
                // a misleading message.
                let obj = item
                    .as_object()
                    .ok_or_else(|| format!("input[{idx}]: 'message' item must be a JSON object"))?;
                // Validate `role` is present; the upstream model will
                // 422 with `messages[N]: missing field 'role'`
                // otherwise.
                if obj.get("role").and_then(|r| r.as_str()).is_none() {
                    return Err(format!("input[{idx}]: 'message' item missing 'role'"));
                }
                // Preserve every field except the Open Responses `type`
                // discriminator. Clients sometimes send chat-completions-
                // shaped messages directly (`{role, content, tool_calls,
                // tool_call_id, name, …}`) and the previous `items.clone()`
                // forwarded those verbatim — dropping anything outside
                // `role`/`content` would be a regression.
                let mut translated = serde_json::Map::new();
                for (k, v) in obj {
                    if k != "type" {
                        translated.insert(k.clone(), v.clone());
                    }
                }
                out.push(Value::Object(translated));
            }
            "function_call" => {
                let obj = item
                    .as_object()
                    .ok_or_else(|| format!("input[{idx}]: 'function_call' item must be a JSON object"))?;
                let call_id = obj
                    .get("call_id")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| format!("input[{idx}]: 'function_call' missing 'call_id'"))?
                    .to_string();
                let name = obj
                    .get("name")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| format!("input[{idx}]: 'function_call' missing 'name'"))?
                    .to_string();
                // `arguments` is a JSON-encoded string per the Responses
                // spec, but tolerate raw objects from looser callers
                // (we'll serialize them back into the JSON string form
                // chat-completions expects). Explicit-null is treated
                // like missing — a client serializing an arguments-less
                // call sometimes emits `"arguments": null`, and the
                // literal string "null" would then be parsed as JSON
                // arguments and rejected by the upstream model.
                //
                // Empty-string `arguments` are forwarded as-is. The
                // upstream model will reject malformed JSON; faithfully
                // round-tripping the client's input is the right
                // contract for a translator at this layer.
                //
                // `serde_json::to_string` (rather than `.to_string()`)
                // for the fallback makes the JSON-serialization intent
                // explicit; both produce the same bytes for `Value`,
                // but the explicit call is what a reader expects to
                // see when the goal is "a JSON-encoded string."
                let arguments_str = match obj.get("arguments") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Null) | None => "{}".to_string(),
                    Some(other) => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_string()),
                };
                let new_tool_call = json!({
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments_str},
                });

                if let Some(last) = out.last_mut()
                    && last.get("role").and_then(|r| r.as_str()) == Some("assistant")
                    && let Some(obj) = last.as_object_mut()
                {
                    let tool_calls = obj.entry("tool_calls").or_insert_with(|| json!([]));
                    if let Value::Array(arr) = tool_calls {
                        arr.push(new_tool_call);
                        continue;
                    }
                }
                out.push(json!({
                    "role": "assistant",
                    "content": Value::Null,
                    "tool_calls": [new_tool_call],
                }));
            }
            "function_call_output" => {
                let obj = item
                    .as_object()
                    .ok_or_else(|| format!("input[{idx}]: 'function_call_output' item must be a JSON object"))?;
                let call_id = obj
                    .get("call_id")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| format!("input[{idx}]: 'function_call_output' missing 'call_id'"))?;
                // Tool output is conventionally a JSON-encoded string.
                // Stringify non-string values via `serde_json::to_string`
                // so chat-completions sees a string `content` (rather
                // than the literal `Display` form). Treat explicit-null
                // like missing — `Value::Null` would otherwise become
                // the string "null", surfaced back to the user; the
                // spec intent is "no output".
                let content_str = match obj.get("output") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Null) | None => String::new(),
                    Some(other) => serde_json::to_string(other).unwrap_or_default(),
                };
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": content_str,
                }));
            }
            "reasoning" => {
                // Reasoning items are not valid chat-completions
                // messages. Drop with a debug trace so unexpected
                // disappearances are diagnosable.
                tracing::debug!(idx, "dropping 'reasoning' input item during /v1/responses translation");
            }
            other => {
                // Unknown types: warn rather than fail so a future spec
                // addition doesn't take the whole request down. The
                // model may still error if it needed the item, but at
                // least the failure mode is observable.
                tracing::warn!(idx, item_type = %other, "unknown Open Responses input item type; dropping");
            }
        }
    }
    Ok(out)
}

/// Normalize the `tools` array into chat-completions wrapped shape:
/// `[{type:"function", function:{name, description, parameters, …}}]`.
///
/// Accepts:
/// - already-wrapped (`{type:"function", function:{…}}`) → pass through
/// - Open Responses spec-flat (`{type:"function", name, description, parameters, …}`)
///   → wrap into `{type:"function", function:{…}}`
///
/// Without this, spec-flat tools were forwarded as-is and rejected by
/// the upstream model with a deserialization error similar to the
/// per-message role bug. See task 5 in the bug report.
fn normalize_tools(tools: &Value) -> Value {
    let Value::Array(items) = tools else {
        return tools.clone();
    };
    let normalized: Vec<Value> = items
        .iter()
        .map(|item| {
            // Already wrapped — leave it.
            if item.get("function").is_some() {
                return item.clone();
            }
            // Only wrap function tools. Hosted Open Responses tool
            // types like `web_search`, `file_search`,
            // `computer_use_preview`, etc. have their own schemas
            // (no `function` sub-object) and forwarding them verbatim
            // is the only correct call — wrapping their fields under
            // a `function` key would produce an invalid tool object.
            let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("function");
            if item_type != "function" {
                return item.clone();
            }
            // Spec-flat function tool: lift everything except the
            // top-level `type` discriminator into a `function` object.
            // Copying the whole object (rather than a fixed whitelist)
            // avoids silently dropping spec additions or vendor
            // extensions — anything the client cared to send gets
            // forwarded to the upstream tool schema.
            let mut function_obj = serde_json::Map::new();
            if let Some(obj) = item.as_object() {
                for (k, v) in obj {
                    if k != "type" {
                        function_obj.insert(k.clone(), v.clone());
                    }
                }
            }
            json!({"type": "function", "function": function_obj})
        })
        .collect();
    Value::Array(normalized)
}

/// Build the messages list for the next model call by accumulating:
/// 1. the parent request's initial messages,
/// 2. for each completed (model_call → tool_call*) iteration in the chain:
///    - the assistant message from the model_call's response, and
///    - one `tool` message per completed tool_call (carrying the tool's
///      output as `content`).
pub(crate) fn build_messages_from_chain(initial: &[Value], chain: &[ChainStep]) -> Vec<Value> {
    let mut messages: Vec<Value> = initial.to_vec();

    let mut i = 0;
    while i < chain.len() {
        let step = &chain[i];
        if !matches!(step.state, StepState::Completed) {
            i += 1;
            continue;
        }
        match step.kind {
            StepKind::ModelCall => {
                if let Some(payload) = &step.response_payload
                    && let Some(message) = extract_assistant_message(payload)
                {
                    messages.push(message);
                }
                i += 1;
            }
            StepKind::ToolCall => {
                let call_id = step
                    .response_payload
                    .as_ref()
                    .map(|_p| "unknown".to_string())
                    .unwrap_or_else(|| format!("step_{}", step.sequence));
                let content = step
                    .response_payload
                    .as_ref()
                    .map(|p| serde_json::to_string(p).unwrap_or_default())
                    .unwrap_or_default();
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": content,
                }));
                i += 1;
            }
        }
    }

    messages
}

fn extract_assistant_message(model_response: &Value) -> Option<Value> {
    model_response
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|choice| choice.get("message"))
        .cloned()
}

/// Extract any `tool_calls` array from a model response, in the OpenAI
/// Chat Completions shape. Returns descriptors ready to be appended as
/// fan-out tool_call steps.
pub(crate) fn extract_tool_calls(model_response: &Value) -> Vec<StepDescriptor> {
    let Some(message) = extract_assistant_message(model_response) else {
        return vec![];
    };
    let Some(tool_calls) = message.get("tool_calls").and_then(|x| x.as_array()) else {
        return vec![];
    };

    tool_calls
        .iter()
        .filter_map(|call| {
            let function = call.get("function")?;
            let name = function.get("name")?.as_str()?.to_string();
            let raw_args = function.get("arguments");
            let args: Value = match raw_args {
                Some(Value::String(s)) => {
                    // OpenAI sends arguments as a JSON-encoded string.
                    serde_json::from_str(s).unwrap_or(json!({}))
                }
                Some(other) => other.clone(),
                None => json!({}),
            };
            let call_id = call.get("id").and_then(|x| x.as_str()).unwrap_or("call_unknown").to_string();
            Some(StepDescriptor {
                kind: StepKind::ToolCall,
                request_payload: json!({
                    "name": name,
                    "args": args,
                    "call_id": call_id,
                }),
            })
        })
        .collect()
}

/// Build the `request_payload` for the first model_call (initial
/// invocation) given the parent request.
pub(crate) fn prepare_initial_model_call(parsed: &ParsedRequest) -> StepDescriptor {
    let mut payload = json!({
        "model": parsed.model,
        "messages": parsed.initial_messages,
        "stream": parsed.stream,
    });
    if let Some(tools) = &parsed.tools {
        payload["tools"] = tools.clone();
    }
    StepDescriptor {
        kind: StepKind::ModelCall,
        request_payload: payload,
    }
}

/// Build the `request_payload` for a follow-up model_call (after one or
/// more tool_calls completed) by reconstructing the running messages
/// list from the chain.
pub(crate) fn prepare_followup_model_call(parsed: &ParsedRequest, chain: &[ChainStep]) -> StepDescriptor {
    let messages = build_messages_from_chain(&parsed.initial_messages, chain);
    let mut payload = json!({
        "model": parsed.model,
        "messages": messages,
        "stream": parsed.stream,
    });
    if let Some(tools) = &parsed.tools {
        payload["tools"] = tools.clone();
    }
    StepDescriptor {
        kind: StepKind::ModelCall,
        request_payload: payload,
    }
}

/// Decide the next action given:
/// - `parsed`: the parent fusillade request body
/// - `chain`: completed/failed steps in the current scope, in sequence order
/// - `resolved_tool_names`: names of server-side tools registered for this
///   request. Tool_calls whose name is in this set get auto-dispatched as
///   server-side `ToolCall` steps; tool_calls outside the set are
///   passed through to the client as `function_call` output items by
///   completing the response with the model's payload (the assembly step
///   surfaces them per the OpenAI Responses contract).
///
/// Returns the action the loop should take. Pure function over its inputs;
/// no I/O.
pub(crate) fn decide_next_action(parsed: &ParsedRequest, chain: &[ChainStep], resolved_tool_names: &HashSet<String>) -> NextAction {
    if chain.is_empty() {
        return NextAction::AppendSteps(vec![prepare_initial_model_call(parsed)]);
    }

    // Walk to the most recent terminal step.
    let last = match chain
        .iter()
        .rev()
        .find(|s| matches!(s.state, StepState::Completed | StepState::Failed))
    {
        Some(s) => s,
        None => {
            // Chain has steps but none reached a terminal state. In
            // steady-state operation a single worker owns the loop and
            // every iteration either appends new steps or terminates,
            // so this branch should only fire on a genuine resume —
            // another worker has re-claimed the row after the original
            // worker died mid-step. Re-firing the abandoned step risks
            // duplicate model/tool side effects and we don't carry
            // enough metadata to know whether the upstream call
            // completed before the worker died, so we fail the chain.
            return NextAction::Fail(json!({
                "type": "step_abandoned",
                "message": "a step was in flight when this worker took over; the previous worker exited before completing it",
            }));
        }
    };

    if matches!(last.state, StepState::Failed) {
        return NextAction::Fail(last.error.clone().unwrap_or_else(|| json!({"type": "step_failed"})));
    }

    match last.kind {
        StepKind::ModelCall => {
            let response = last.response_payload.as_ref().cloned().unwrap_or_else(|| json!({}));
            let tool_calls = extract_tool_calls(&response);
            if tool_calls.is_empty() {
                // No tool calls — the model returned final output.
                return NextAction::Complete(response);
            }

            // Server-side dispatch is only safe when every tool_call
            // names a server-registered tool (i.e., one with a row in
            // `tool_sources` for this request's user/deployment). If
            // any name is unregistered, it's a client-side function
            // tool — the model must have seen it because the user put
            // it in the request body — and we cannot dispatch it. The
            // OpenAI Responses contract for that case is to surface
            // every tool_call as a `function_call` output item and let
            // the client run them and submit results in a follow-up.
            //
            // We bail out for the *whole* fan-out (rather than partial
            // dispatch) because the model expects results for every
            // call it emitted before producing its next message; a
            // mixed dispatch would leave the conversation in a state
            // the upstream model can't reason about.
            let all_registered = tool_calls
                .iter()
                .all(|step| tool_call_name(&step.request_payload).is_some_and(|name| resolved_tool_names.contains(name)));
            if all_registered {
                NextAction::AppendSteps(tool_calls)
            } else {
                NextAction::Complete(response)
            }
        }
        StepKind::ToolCall => {
            // After a tool_call (and any sibling tool_calls — they all
            // chain via prev_step_id, so by the time we see one the
            // others have also reached terminal state), the next step
            // is a synthesizing model_call.
            NextAction::AppendSteps(vec![prepare_followup_model_call(parsed, chain)])
        }
    }
}

fn tool_call_name(payload: &Value) -> Option<&str> {
    payload.get("name").and_then(|n| n.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &str, seq: i64, kind: StepKind, state: StepState, response: Option<Value>) -> ChainStep {
        ChainStep {
            id: id.into(),
            kind,
            state,
            sequence: seq,
            prev_step_id: None,
            parent_step_id: None,
            response_payload: response,
            error: None,
        }
    }

    #[test]
    fn parses_string_input() {
        let body = r#"{"model":"gpt-4o","input":"hi"}"#;
        let p = parse_parent_request(body).unwrap();
        assert_eq!(p.model, "gpt-4o");
        assert_eq!(p.initial_messages, vec![json!({"role":"user","content":"hi"})]);
    }

    #[test]
    fn parses_messages_form() {
        let body = r#"{"model":"x","messages":[{"role":"user","content":"hello"}]}"#;
        let p = parse_parent_request(body).unwrap();
        assert_eq!(p.initial_messages.len(), 1);
    }

    #[test]
    fn translates_message_input_items() {
        // {type:"message", role, content} → {role, content}; the `type`
        // field is dropped since chat-completions doesn't expect it.
        let body = r#"{
            "model": "m",
            "input": [
                {"type":"message","role":"system","content":"Be terse."},
                {"type":"message","role":"user","content":"Find one fact."}
            ]
        }"#;
        let p = parse_parent_request(body).unwrap();
        assert_eq!(p.initial_messages.len(), 2);
        assert_eq!(p.initial_messages[0], json!({"role":"system","content":"Be terse."}));
        assert_eq!(p.initial_messages[1], json!({"role":"user","content":"Find one fact."}));
        // No leftover `type` fields.
        for msg in &p.initial_messages {
            assert!(msg.get("type").is_none(), "translated message must drop 'type' field");
        }
    }

    #[test]
    fn translates_function_call_to_assistant_tool_calls_message() {
        // The exact reproduction from the bug report: a `function_call`
        // input item must produce a chat-completions assistant message
        // with a non-empty `tool_calls` array — not a role-less item.
        let body = r#"{
            "model": "m",
            "input": [
                {"type":"message","role":"user","content":"go"},
                {"type":"function_call","call_id":"call_a","name":"search","arguments":"{\"query\":\"x\"}"}
            ]
        }"#;
        let p = parse_parent_request(body).unwrap();
        assert_eq!(p.initial_messages.len(), 2);
        let assistant = &p.initial_messages[1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"], Value::Null);
        let tool_calls = assistant["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_a");
        assert_eq!(tool_calls[0]["type"], "function");
        assert_eq!(tool_calls[0]["function"]["name"], "search");
        // `arguments` stays a JSON-encoded string per chat-completions spec.
        assert_eq!(tool_calls[0]["function"]["arguments"], "{\"query\":\"x\"}");
    }

    #[test]
    fn translates_function_call_output_to_tool_message() {
        let body = r#"{
            "model": "m",
            "input": [
                {"type":"function_call","call_id":"call_a","name":"f","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_a","output":"{\"results\":[]}"}
            ]
        }"#;
        let p = parse_parent_request(body).unwrap();
        // assistant tool_call message + tool result message
        assert_eq!(p.initial_messages.len(), 2);
        let tool_msg = &p.initial_messages[1];
        assert_eq!(tool_msg["role"], "tool");
        assert_eq!(tool_msg["tool_call_id"], "call_a");
        assert_eq!(tool_msg["content"], "{\"results\":[]}");
    }

    #[test]
    fn collapses_consecutive_function_calls_into_one_assistant_message() {
        // Two function_calls back-to-back belong to a single assistant
        // turn and must collapse into one message with multiple
        // tool_calls — chat-completions doesn't accept two assistant
        // tool-call messages with no intervening tool result.
        let body = r#"{
            "model": "m",
            "input": [
                {"type":"function_call","call_id":"call_a","name":"a","arguments":"{}"},
                {"type":"function_call","call_id":"call_b","name":"b","arguments":"{}"}
            ]
        }"#;
        let p = parse_parent_request(body).unwrap();
        assert_eq!(p.initial_messages.len(), 1);
        let tool_calls = p.initial_messages[0]["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0]["id"], "call_a");
        assert_eq!(tool_calls[1]["id"], "call_b");
    }

    #[test]
    fn folds_function_calls_into_preceding_assistant_message() {
        // An assistant text turn that also issued a tool call arrives
        // as two adjacent items: the message + the function_call. They
        // must merge into one chat-completions message with both
        // `content` and `tool_calls`.
        let body = r#"{
            "model": "m",
            "input": [
                {"type":"message","role":"user","content":"hi"},
                {"type":"message","role":"assistant","content":"thinking..."},
                {"type":"function_call","call_id":"call_a","name":"f","arguments":"{}"}
            ]
        }"#;
        let p = parse_parent_request(body).unwrap();
        assert_eq!(p.initial_messages.len(), 2);
        let assistant = &p.initial_messages[1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"], "thinking...");
        let tool_calls = assistant["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_a");
    }

    #[test]
    fn full_multi_turn_tool_conversation_translates_correctly() {
        // The exact bug-report payload. After the fix, every translated
        // message must have a `role` field — that's what the upstream
        // 422 was complaining about. This test would have caught the
        // original bug.
        let body = r#"{
            "model": "Qwen/Qwen3-VL-30B-A3B-Instruct-FP8",
            "service_tier": "flex",
            "max_output_tokens": 64,
            "input": [
                {"type":"message","role":"system","content":"Be terse."},
                {"type":"message","role":"user","content":"Find one fact."},
                {"type":"message","role":"system","content":"ctx"},
                {"type":"function_call","call_id":"call_a","name":"search","arguments":"{\"query\":\"x\"}"},
                {"type":"function_call_output","call_id":"call_a","output":"{\"results\":[]}"}
            ],
            "tools":[{"type":"function","function":{"name":"search","description":"s","parameters":{"type":"object"}}}]
        }"#;
        let p = parse_parent_request(body).unwrap();
        // 3 message items + 1 collapsed assistant tool-call + 1 tool result = 5
        assert_eq!(p.initial_messages.len(), 5);
        for (idx, msg) in p.initial_messages.iter().enumerate() {
            let role = msg.get("role").and_then(|r| r.as_str());
            assert!(role.is_some(), "messages[{idx}] must have a role; got {msg}");
        }
        assert_eq!(p.initial_messages[0]["role"], "system");
        assert_eq!(p.initial_messages[1]["role"], "user");
        assert_eq!(p.initial_messages[2]["role"], "system");
        assert_eq!(p.initial_messages[3]["role"], "assistant");
        assert_eq!(p.initial_messages[3]["tool_calls"][0]["function"]["name"], "search");
        assert_eq!(p.initial_messages[4]["role"], "tool");
        assert_eq!(p.initial_messages[4]["tool_call_id"], "call_a");
    }

    #[test]
    fn drops_reasoning_input_items() {
        // Reasoning items have no chat-completions equivalent — chat
        // completions has no `reasoning` role. Dropping them keeps the
        // request valid; the reasoning is lost but most upstream models
        // either reject it or ignore it.
        let body = r#"{
            "model": "m",
            "input": [
                {"type":"message","role":"user","content":"hi"},
                {"type":"reasoning","summary":["thought"]},
                {"type":"message","role":"assistant","content":"hello"}
            ]
        }"#;
        let p = parse_parent_request(body).unwrap();
        assert_eq!(p.initial_messages.len(), 2);
        assert_eq!(p.initial_messages[0]["role"], "user");
        assert_eq!(p.initial_messages[1]["role"], "assistant");
    }

    #[test]
    fn non_object_input_items_return_clear_errors() {
        // Bare strings/numbers/arrays in the input list aren't valid
        // translatable items. Each translator branch surfaces a
        // precise error rather than letting a missing-field check
        // below produce a misleading message.
        //
        // A bare primitive defaults to `type: "message"` (the
        // `unwrap_or("message")` fallback) and triggers the message
        // branch's object validation.
        let err = match parse_parent_request(r#"{"model":"m","input":["bare string"]}"#) {
            Ok(_) => panic!("expected Err for bare string in input"),
            Err(e) => e,
        };
        assert!(err.contains("must be a JSON object"), "got: {err}");
        assert!(err.contains("'message'"), "got: {err}");
    }

    #[test]
    fn missing_role_on_message_item_returns_error() {
        // A message item without a role is malformed input — surface
        // an error rather than silently producing an invalid upstream
        // request that 422s deep in the loop.
        let body = r#"{
            "model": "m",
            "input": [{"type":"message","content":"hi"}]
        }"#;
        let err = match parse_parent_request(body) {
            Ok(_) => panic!("expected Err, got Ok"),
            Err(e) => e,
        };
        assert!(err.contains("missing 'role'"), "got: {err}");
    }

    #[test]
    fn normalizes_spec_flat_tools_into_wrapped_form() {
        // Spec-flat: {type:"function", name, description, parameters}.
        // Chat-completions wants wrapped: {type:"function", function:{…}}.
        let body = r#"{
            "model": "m",
            "input": "hi",
            "tools": [
                {"type":"function","name":"search","description":"s","parameters":{"type":"object"}}
            ]
        }"#;
        let p = parse_parent_request(body).unwrap();
        let tools = p.tools.unwrap();
        let arr = tools.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "function");
        let function = &arr[0]["function"];
        assert_eq!(function["name"], "search");
        assert_eq!(function["description"], "s");
        assert_eq!(function["parameters"], json!({"type": "object"}));
        // The flat fields must not survive at the top level.
        assert!(arr[0].get("name").is_none(), "wrapped tool must not have top-level 'name'");
    }

    #[test]
    fn passes_already_wrapped_tools_through_unchanged() {
        let body = r#"{
            "model": "m",
            "input": "hi",
            "tools": [
                {"type":"function","function":{"name":"search","description":"s","parameters":{"type":"object"}}}
            ]
        }"#;
        let p = parse_parent_request(body).unwrap();
        let tools = p.tools.unwrap();
        let arr = tools.as_array().unwrap();
        assert_eq!(arr[0]["function"]["name"], "search");
        assert!(arr[0].get("name").is_none());
    }

    #[test]
    fn null_arguments_on_function_call_become_empty_object() {
        // Some clients serialize an arguments-less call as
        // `"arguments": null`. Without explicit handling, that becomes
        // the literal string "null" — which the upstream model would
        // try to parse as the JSON arguments string and reject.
        let body = r#"{
            "model": "m",
            "input": [
                {"type":"function_call","call_id":"c","name":"f","arguments":null}
            ]
        }"#;
        let p = parse_parent_request(body).unwrap();
        let tool_calls = p.initial_messages[0]["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls[0]["function"]["arguments"], "{}");
    }

    #[test]
    fn null_output_on_function_call_output_becomes_empty_string() {
        let body = r#"{
            "model": "m",
            "input": [
                {"type":"function_call","call_id":"c","name":"f","arguments":"{}"},
                {"type":"function_call_output","call_id":"c","output":null}
            ]
        }"#;
        let p = parse_parent_request(body).unwrap();
        let tool_msg = &p.initial_messages[1];
        assert_eq!(tool_msg["role"], "tool");
        assert_eq!(tool_msg["content"], "");
    }

    #[test]
    fn normalize_tools_preserves_unknown_fields() {
        // Spec additions and vendor extensions should be forwarded to
        // the wrapped `function` object rather than silently dropped.
        // A whitelist would lock the translator to a frozen view of
        // the spec; copying everything except the top-level `type`
        // discriminator stays compatible with future fields.
        let body = r#"{
            "model": "m",
            "input": "hi",
            "tools": [
                {"type":"function","name":"f","description":"d","parameters":{"type":"object"},"strict":true,"x_vendor":{"hint":"v"}}
            ]
        }"#;
        let p = parse_parent_request(body).unwrap();
        let arr = p.tools.unwrap();
        let function = &arr[0]["function"];
        assert_eq!(function["name"], "f");
        assert_eq!(function["description"], "d");
        assert_eq!(function["strict"], true);
        assert_eq!(function["x_vendor"]["hint"], "v");
        // Top-level `type` discriminator stays at the wrapper level,
        // not lifted into `function`.
        assert!(function.get("type").is_none());
        assert_eq!(arr[0]["type"], "function");
    }

    #[test]
    fn empty_input_array_translates_to_empty_messages() {
        // Edge case: a client sending an empty input array shouldn't
        // crash the translator. The downstream model will reject the
        // empty messages list, but that's a model-level concern, not
        // a translator-level one.
        let body = r#"{"model":"m","input":[]}"#;
        let p = parse_parent_request(body).unwrap();
        assert!(p.initial_messages.is_empty());
    }

    #[test]
    fn raw_object_arguments_are_json_serialized() {
        // Looser clients sometimes send `arguments` as a raw JSON
        // object instead of the spec-mandated JSON-encoded string.
        // The translator should serialize it back into a string so
        // the upstream model sees a valid `arguments` value.
        let body = r#"{
            "model": "m",
            "input": [
                {"type":"function_call","call_id":"c","name":"f","arguments":{"query":"x"}}
            ]
        }"#;
        let p = parse_parent_request(body).unwrap();
        let tool_calls = p.initial_messages[0]["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls[0]["function"]["arguments"], "{\"query\":\"x\"}");
    }

    #[test]
    fn message_translation_preserves_chat_completions_fields() {
        // Clients sometimes send chat-completions-shaped message
        // objects directly through `input` (the legacy `name` field,
        // assistant `tool_calls`, tool-message `tool_call_id`, etc.).
        // The previous `items.clone()` forwarded those verbatim —
        // dropping anything outside `role`/`content` would be a
        // regression. Only the Open Responses `type` discriminator
        // is stripped.
        let body = r#"{
            "model": "m",
            "input": [
                {"type":"message","role":"user","content":"hi","name":"alice"},
                {"role":"tool","tool_call_id":"call_a","content":"{\"ok\":1}"},
                {"role":"assistant","content":null,"tool_calls":[{"id":"call_a","type":"function","function":{"name":"f","arguments":"{}"}}]}
            ]
        }"#;
        let p = parse_parent_request(body).unwrap();
        assert_eq!(p.initial_messages.len(), 3);
        // `name` survives on the user message.
        assert_eq!(p.initial_messages[0]["role"], "user");
        assert_eq!(p.initial_messages[0]["name"], "alice");
        assert!(p.initial_messages[0].get("type").is_none());
        // `tool_call_id` survives on the tool message.
        assert_eq!(p.initial_messages[1]["role"], "tool");
        assert_eq!(p.initial_messages[1]["tool_call_id"], "call_a");
        // `tool_calls` survives on the assistant message.
        assert_eq!(p.initial_messages[2]["role"], "assistant");
        let tool_calls = p.initial_messages[2]["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_a");
    }

    #[test]
    fn normalize_tools_passes_through_non_function_tool_types() {
        // Hosted tool types (web_search, file_search, …) have their
        // own schema and don't carry a `function` sub-object. Wrapping
        // them with the function-tool transformation would produce an
        // invalid tool object that upstream would reject. They must
        // pass through as-is.
        let body = r#"{
            "model": "m",
            "input": "hi",
            "tools": [
                {"type":"web_search"},
                {"type":"file_search","vector_store_ids":["vs_1"]},
                {"type":"function","name":"f","parameters":{"type":"object"}}
            ]
        }"#;
        let p = parse_parent_request(body).unwrap();
        let arr = p.tools.unwrap();
        let arr = arr.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        // Non-function tools pass through verbatim.
        assert_eq!(arr[0], json!({"type": "web_search"}));
        assert_eq!(arr[1], json!({"type": "file_search", "vector_store_ids": ["vs_1"]}));
        // Function tool gets wrapped as before.
        assert_eq!(arr[2]["type"], "function");
        assert_eq!(arr[2]["function"]["name"], "f");
        assert!(arr[2].get("name").is_none());
    }

    #[test]
    fn normalizes_mixed_wrapped_and_spec_flat_tools_in_one_array() {
        // Clients that hand-write requests sometimes mix shapes in
        // the same `tools` array. Each entry should be normalized
        // independently — wrapped items pass through, spec-flat
        // items get wrapped.
        let body = r#"{
            "model": "m",
            "input": "hi",
            "tools": [
                {"type":"function","function":{"name":"wrapped","description":"w"}},
                {"type":"function","name":"flat","description":"f","parameters":{"type":"object"}}
            ]
        }"#;
        let p = parse_parent_request(body).unwrap();
        let arr = p.tools.unwrap();
        let arr = arr.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        // Both end up wrapped after normalization.
        assert_eq!(arr[0]["function"]["name"], "wrapped");
        assert_eq!(arr[1]["function"]["name"], "flat");
        // The second item's flat fields must not survive at the top
        // level after wrapping.
        assert!(arr[1].get("name").is_none());
        assert!(arr[1].get("parameters").is_none());
    }

    fn names(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_chain_emits_initial_model_call() {
        let parsed = ParsedRequest {
            model: "m".into(),
            initial_messages: vec![json!({"role":"user","content":"hi"})],
            tools: None,
            stream: false,
        };
        match decide_next_action(&parsed, &[], &HashSet::new()) {
            NextAction::AppendSteps(steps) => {
                assert_eq!(steps.len(), 1);
                assert!(matches!(steps[0].kind, StepKind::ModelCall));
                assert_eq!(steps[0].request_payload["model"], "m");
            }
            _ => panic!("expected AppendSteps"),
        }
    }

    #[test]
    fn model_call_with_registered_tool_calls_emits_fan_out() {
        let parsed = ParsedRequest {
            model: "m".into(),
            initial_messages: vec![],
            tools: None,
            stream: false,
        };
        let response = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [
                        {"id": "call_1", "type": "function", "function": {"name": "a", "arguments": "{\"x\":1}"}},
                        {"id": "call_2", "type": "function", "function": {"name": "b", "arguments": "{}"}},
                    ]
                }
            }]
        });
        let chain = vec![step("s1", 1, StepKind::ModelCall, StepState::Completed, Some(response))];
        match decide_next_action(&parsed, &chain, &names(&["a", "b"])) {
            NextAction::AppendSteps(steps) => {
                assert_eq!(steps.len(), 2);
                assert_eq!(steps[0].request_payload["name"], "a");
                assert_eq!(steps[0].request_payload["args"]["x"], 1);
                assert_eq!(steps[0].request_payload["call_id"], "call_1");
                assert_eq!(steps[1].request_payload["name"], "b");
            }
            _ => panic!("expected AppendSteps"),
        }
    }

    #[test]
    fn model_call_with_unregistered_tool_completes_for_client_dispatch() {
        // The user supplied a client-side function tool in the request
        // body; the model emits a tool_call for it. With no row in
        // `tool_sources` for this name, the loop must NOT try to
        // dispatch it (HttpToolExecutor would fail with NotFound) —
        // instead it completes with the model's response so assembly
        // can surface a `function_call` output item to the client.
        let parsed = ParsedRequest {
            model: "m".into(),
            initial_messages: vec![],
            tools: None,
            stream: false,
        };
        let response = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [
                        {"id": "call_1", "type": "function", "function": {"name": "read_pages", "arguments": "{\"id\":1}"}},
                    ]
                }
            }]
        });
        let chain = vec![step("s1", 1, StepKind::ModelCall, StepState::Completed, Some(response.clone()))];
        match decide_next_action(&parsed, &chain, &HashSet::new()) {
            NextAction::Complete(v) => assert_eq!(v, response),
            other => panic!("expected Complete for unregistered tool, got {other:?}"),
        }
    }

    #[test]
    fn model_call_with_mixed_registered_and_unregistered_completes() {
        // If even one tool_call in a fan-out is unregistered, the whole
        // batch passes through to the client. Partial dispatch would
        // leave the model expecting results for tool_calls the loop
        // never ran.
        let parsed = ParsedRequest {
            model: "m".into(),
            initial_messages: vec![],
            tools: None,
            stream: false,
        };
        let response = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [
                        {"id": "call_1", "type": "function", "function": {"name": "weather", "arguments": "{}"}},
                        {"id": "call_2", "type": "function", "function": {"name": "client_only", "arguments": "{}"}},
                    ]
                }
            }]
        });
        let chain = vec![step("s1", 1, StepKind::ModelCall, StepState::Completed, Some(response.clone()))];
        match decide_next_action(&parsed, &chain, &names(&["weather"])) {
            NextAction::Complete(v) => assert_eq!(v, response),
            other => panic!("expected Complete for mixed tool_calls, got {other:?}"),
        }
    }

    #[test]
    fn model_call_without_tool_calls_completes() {
        let parsed = ParsedRequest {
            model: "m".into(),
            initial_messages: vec![],
            tools: None,
            stream: false,
        };
        let response = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "the answer is 42"}
            }]
        });
        let chain = vec![step("s1", 1, StepKind::ModelCall, StepState::Completed, Some(response.clone()))];
        match decide_next_action(&parsed, &chain, &HashSet::new()) {
            NextAction::Complete(v) => assert_eq!(v, response),
            _ => panic!("expected Complete"),
        }
    }

    #[test]
    fn after_tool_call_emits_followup_model_call() {
        let parsed = ParsedRequest {
            model: "m".into(),
            initial_messages: vec![json!({"role":"user","content":"hi"})],
            tools: None,
            stream: false,
        };
        let model_response = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "a", "arguments": "{}"}}]
                }
            }]
        });
        let chain = vec![
            step("s1", 1, StepKind::ModelCall, StepState::Completed, Some(model_response)),
            step("s2", 2, StepKind::ToolCall, StepState::Completed, Some(json!({"result": 1}))),
        ];
        match decide_next_action(&parsed, &chain, &names(&["a"])) {
            NextAction::AppendSteps(steps) => {
                assert_eq!(steps.len(), 1);
                assert!(matches!(steps[0].kind, StepKind::ModelCall));
                let messages = steps[0].request_payload["messages"].as_array().unwrap();
                // initial_messages + assistant_message + tool_message = 3
                assert_eq!(messages.len(), 3);
                assert_eq!(messages[0]["role"], "user");
                assert_eq!(messages[1]["role"], "assistant");
                assert_eq!(messages[2]["role"], "tool");
            }
            _ => panic!("expected AppendSteps"),
        }
    }

    #[test]
    fn failed_step_propagates_as_fail() {
        let parsed = ParsedRequest {
            model: "m".into(),
            initial_messages: vec![],
            tools: None,
            stream: false,
        };
        let mut s = step("s1", 1, StepKind::ModelCall, StepState::Failed, None);
        s.error = Some(json!({"type": "upstream_500"}));
        match decide_next_action(&parsed, &[s], &HashSet::new()) {
            NextAction::Fail(v) => assert_eq!(v, json!({"type": "upstream_500"})),
            _ => panic!("expected Fail"),
        }
    }
}
