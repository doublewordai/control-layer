//! Assemble the final OpenAI Response JSON from the chain of steps.
//!
//! Walks top-level steps (parent_step_id IS NULL) and produces an
//! `output: [...]` array per the OpenAI Responses API contract. Sub-agent
//! steps (those with a non-null `parent_step_id`) are NOT exposed here
//! by default — they surface only via the dashboard's
//! `GET /v1/responses/{id}/steps` extension.
//!
//! ## Output items shape
//!
//! - `model_call` steps with assistant text → `{"type":"message","role":"assistant","content":[{"type":"output_text","text":"..."}]}`
//! - `tool_call` steps → `{"type":"function_call","call_id":"...","name":"...","arguments":"..."}` followed by `{"type":"function_call_output","call_id":"...","output":"..."}`
//!
//! The assembled object lives at the top level of the Response and
//! includes `id`, `object: "response"`, `status`, `output`, and optional
//! `usage` if present in any model_call response.

use onwards::{ChainStep, StepKind, StepState};
use serde_json::{Value, json};

/// Build the final Response JSON for `request_id` from the top-level
/// chain (steps with `parent_step_id IS NULL`).
pub(crate) fn assemble_from_chain(request_id: &str, chain: &[ChainStep]) -> Value {
    let mut output: Vec<Value> = Vec::new();
    let mut pending_tool_calls: Vec<(String, String, String)> = Vec::new();
    let mut total_usage: Option<Value> = None;

    for step in chain {
        if !matches!(step.state, StepState::Completed) {
            continue;
        }
        match step.kind {
            StepKind::ModelCall => {
                let payload = step.response_payload.as_ref().cloned().unwrap_or_else(|| json!({}));

                // Extract assistant text content if present.
                let assistant = payload
                    .get("choices")
                    .and_then(|c| c.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|c| c.get("message"));
                if let Some(message) = assistant
                    && let Some(content) = message.get("content").and_then(|c| c.as_str())
                    && !content.is_empty()
                {
                    output.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": content}],
                    }));
                }

                // Stash tool_calls so subsequent tool_call output items
                // can pair with them by call_id.
                if let Some(tool_calls) = assistant.and_then(|m| m.get("tool_calls")).and_then(|v| v.as_array()) {
                    for call in tool_calls {
                        let call_id = call.get("id").and_then(|x| x.as_str()).unwrap_or("call_unknown").to_string();
                        let name = call
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();
                        let arguments = call
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .map(|v| match v {
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            })
                            .unwrap_or_default();
                        // Surface the function_call item now; the matching
                        // function_call_output is appended when we process
                        // the corresponding tool_call step below.
                        output.push(json!({
                            "type": "function_call",
                            "call_id": call_id,
                            "name": name,
                            "arguments": arguments,
                        }));
                        pending_tool_calls.push((call_id, name, arguments));
                    }
                }

                // Accumulate usage across model calls.
                if let Some(usage) = payload.get("usage").cloned() {
                    total_usage = Some(merge_usage(total_usage, usage));
                }
            }
            StepKind::ToolCall => {
                let call_id = step
                    .response_payload
                    .as_ref()
                    .and_then(|_| {
                        // request_payload isn't in ChainStep, but we
                        // recorded the call_id at fan-out time. Walk
                        // pending_tool_calls in order — they should
                        // pair up with tool_call steps in the chain.
                        pending_tool_calls.first().map(|(id, _, _)| id.clone())
                    })
                    .unwrap_or_else(|| format!("call_step_{}", step.sequence));
                if !pending_tool_calls.is_empty() {
                    pending_tool_calls.remove(0);
                }
                let output_text = step
                    .response_payload
                    .as_ref()
                    .map(|p| serde_json::to_string(p).unwrap_or_default())
                    .unwrap_or_default();
                output.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output_text,
                }));
            }
        }
    }

    let mut response = json!({
        "id": format!("resp_{request_id}"),
        "object": "response",
        "status": "completed",
        "output": output,
    });
    if let Some(usage) = total_usage {
        response["usage"] = usage;
    }
    response
}

fn merge_usage(prev: Option<Value>, next: Value) -> Value {
    let Some(prev) = prev else { return next };
    let mut out = prev;
    if let (Some(prev_obj), Some(next_obj)) = (out.as_object_mut(), next.as_object()) {
        for (k, v) in next_obj {
            if let (Some(prev_n), Some(next_n)) = (prev_obj.get(k).and_then(|x| x.as_i64()), v.as_i64()) {
                prev_obj.insert(k.clone(), json!(prev_n + next_n));
            } else {
                prev_obj.insert(k.clone(), v.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(seq: i64, kind: StepKind, response: Value) -> ChainStep {
        ChainStep {
            id: format!("s{seq}"),
            kind,
            state: StepState::Completed,
            sequence: seq,
            prev_step_id: None,
            parent_step_id: None,
            response_payload: Some(response),
            error: None,
        }
    }

    #[test]
    fn assembles_text_only_response() {
        let chain = vec![step(
            1,
            StepKind::ModelCall,
            json!({
                "choices": [{"message": {"role":"assistant","content":"hello world"}}]
            }),
        )];
        let r = assemble_from_chain("abc", &chain);
        assert_eq!(r["object"], "response");
        assert_eq!(r["status"], "completed");
        let output = r["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["content"][0]["text"], "hello world");
    }

    #[test]
    fn assembles_model_tool_model_chain() {
        let chain = vec![
            step(
                1,
                StepKind::ModelCall,
                json!({
                    "choices": [{
                        "message": {
                            "role":"assistant",
                            "tool_calls":[{"id":"call_1","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Paris\"}"}}]
                        }
                    }]
                }),
            ),
            step(2, StepKind::ToolCall, json!({"temp": 72})),
            step(
                3,
                StepKind::ModelCall,
                json!({
                    "choices": [{"message":{"role":"assistant","content":"It's 72 in Paris"}}]
                }),
            ),
        ];
        let r = assemble_from_chain("abc", &chain);
        let output = r["output"].as_array().unwrap();
        // function_call (from step 1) + function_call_output (from step 2) +
        // assistant message (from step 3)
        assert_eq!(output.len(), 3);
        assert_eq!(output[0]["type"], "function_call");
        assert_eq!(output[0]["call_id"], "call_1");
        assert_eq!(output[0]["name"], "weather");
        assert_eq!(output[1]["type"], "function_call_output");
        assert_eq!(output[1]["call_id"], "call_1");
        assert_eq!(output[2]["type"], "message");
        assert_eq!(output[2]["content"][0]["text"], "It's 72 in Paris");
    }

    #[test]
    fn merges_usage_across_model_calls() {
        let chain = vec![
            step(
                1,
                StepKind::ModelCall,
                json!({
                    "choices": [{"message": {"role":"assistant","content":"a"}}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 20}
                }),
            ),
            step(
                2,
                StepKind::ModelCall,
                json!({
                    "choices": [{"message": {"role":"assistant","content":"b"}}],
                    "usage": {"prompt_tokens": 5, "completion_tokens": 15}
                }),
            ),
        ];
        let r = assemble_from_chain("x", &chain);
        assert_eq!(r["usage"]["prompt_tokens"], 15);
        assert_eq!(r["usage"]["completion_tokens"], 35);
    }
}
