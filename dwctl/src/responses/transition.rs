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
            Value::Array(items) => items.clone(),
            _ => return Err("'input' must be string or array".into()),
        }
    } else if let Some(messages) = v.get("messages").and_then(|m| m.as_array()) {
        messages.clone()
    } else {
        return Err("parent body missing 'input' or 'messages'".into());
    };

    let tools = v.get("tools").cloned();
    let stream = v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    Ok(ParsedRequest {
        model,
        initial_messages,
        tools,
        stream,
    })
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
///
/// Returns the action the loop should take. Pure function over its inputs;
/// no I/O.
pub(crate) fn decide_next_action(parsed: &ParsedRequest, chain: &[ChainStep]) -> NextAction {
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
            // Chain has steps but none reached a terminal state. The
            // loop should not have called us in this state — surface
            // as a transition failure rather than spinning.
            return NextAction::Fail(json!({
                "type": "transition_invariant_violation",
                "message": "next_action_for called with no terminal step in chain",
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
                NextAction::Complete(response)
            } else {
                NextAction::AppendSteps(tool_calls)
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
    fn empty_chain_emits_initial_model_call() {
        let parsed = ParsedRequest {
            model: "m".into(),
            initial_messages: vec![json!({"role":"user","content":"hi"})],
            tools: None,
            stream: false,
        };
        match decide_next_action(&parsed, &[]) {
            NextAction::AppendSteps(steps) => {
                assert_eq!(steps.len(), 1);
                assert!(matches!(steps[0].kind, StepKind::ModelCall));
                assert_eq!(steps[0].request_payload["model"], "m");
            }
            _ => panic!("expected AppendSteps"),
        }
    }

    #[test]
    fn model_call_with_tool_calls_emits_fan_out() {
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
        match decide_next_action(&parsed, &chain) {
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
        match decide_next_action(&parsed, &chain) {
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
        match decide_next_action(&parsed, &chain) {
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
        match decide_next_action(&parsed, &[s]) {
            NextAction::Fail(v) => assert_eq!(v, json!({"type": "upstream_500"})),
            _ => panic!("expected Fail"),
        }
    }
}
