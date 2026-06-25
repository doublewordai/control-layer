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

use chrono::Utc;
use onwards::{ChainStep, StepKind, StepState};
use serde_json::{Value, json};

/// Build the final Response JSON for `request_id` from the top-level
/// chain (steps with `parent_step_id IS NULL`).
pub(crate) fn assemble_from_chain(request_id: &str, chain: &[ChainStep]) -> Value {
    let mut output: Vec<Value> = Vec::new();
    let mut pending_tool_calls: Vec<(String, String, String)> = Vec::new();
    let mut total_usage: Option<Value> = None;
    let created_at = first_created_at(chain).unwrap_or_else(|| Utc::now().timestamp());
    let model = first_model(chain).unwrap_or_else(|| "unknown".to_string());

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
                        "id": format!("msg_{}", step.sequence),
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": content,
                            "annotations": [],
                            "logprobs": [],
                        }],
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
                            "id": format!("fc_{}_{}", step.sequence, output.len()),
                            "type": "function_call",
                            "call_id": call_id,
                            "name": name,
                            "arguments": arguments,
                            "status": "completed",
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
                    "id": format!("fco_{}", step.sequence),
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output_text,
                    "status": "completed",
                }));
            }
        }
    }

    json!({
        "id": format!("resp_{request_id}"),
        "object": "response",
        "created_at": created_at,
        "completed_at": created_at,
        "status": "completed",
        "incomplete_details": null,
        "model": model,
        "previous_response_id": null,
        "instructions": null,
        "output": output,
        "error": null,
        "tools": [],
        "tool_choice": "auto",
        "truncation": "disabled",
        "parallel_tool_calls": true,
        "text": {"format": {"type": "text"}},
        "top_p": 1.0,
        "presence_penalty": 0.0,
        "frequency_penalty": 0.0,
        "top_logprobs": 0,
        "temperature": 1.0,
        "reasoning": null,
        "usage": total_usage.map(chat_usage_to_response_usage).unwrap_or(Value::Null),
        "max_output_tokens": null,
        "max_tool_calls": null,
        "store": false,
        "background": false,
        "service_tier": "default",
        "metadata": null,
        "safety_identifier": null,
        "prompt_cache_key": null,
    })
}

fn first_created_at(chain: &[ChainStep]) -> Option<i64> {
    chain
        .iter()
        .filter(|step| matches!(step.kind, StepKind::ModelCall) && matches!(step.state, StepState::Completed))
        .find_map(|step| step.response_payload.as_ref()?.get("created")?.as_i64())
}

fn first_model(chain: &[ChainStep]) -> Option<String> {
    chain
        .iter()
        .filter(|step| matches!(step.kind, StepKind::ModelCall) && matches!(step.state, StepState::Completed))
        .find_map(|step| step.response_payload.as_ref()?.get("model")?.as_str())
        .map(ToOwned::to_owned)
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

fn chat_usage_to_response_usage(usage: Value) -> Value {
    let input_tokens = token_count(&usage, "input_tokens").unwrap_or_else(|| token_count(&usage, "prompt_tokens").unwrap_or(0));
    let output_tokens = token_count(&usage, "output_tokens").unwrap_or_else(|| token_count(&usage, "completion_tokens").unwrap_or(0));
    let total_tokens = token_count(&usage, "total_tokens").unwrap_or(input_tokens + output_tokens);

    json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": total_tokens,
        "input_tokens_details": {
            "cached_tokens": nested_token_count(&usage, "input_tokens_details", "cached_tokens")
                .or_else(|| nested_token_count(&usage, "prompt_tokens_details", "cached_tokens"))
                .unwrap_or(0),
        },
        "output_tokens_details": {
            "reasoning_tokens": nested_token_count(&usage, "output_tokens_details", "reasoning_tokens")
                .or_else(|| nested_token_count(&usage, "completion_tokens_details", "reasoning_tokens"))
                .unwrap_or(0),
        },
    })
}

fn token_count(usage: &Value, key: &str) -> Option<i64> {
    usage.get(key).and_then(Value::as_i64)
}

fn nested_token_count(usage: &Value, details_key: &str, token_key: &str) -> Option<i64> {
    usage
        .get(details_key)
        .and_then(|details| details.get(token_key))
        .and_then(Value::as_i64)
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
                "created": 123,
                "model": "test-model",
                "choices": [{"message": {"role":"assistant","content":"hello world"}}]
            }),
        )];
        let r = assemble_from_chain("abc", &chain);
        assert_eq!(r["object"], "response");
        assert_eq!(r["status"], "completed");
        assert_eq!(r["created_at"], 123);
        assert_eq!(r["completed_at"], 123);
        assert_eq!(r["model"], "test-model");
        assert_eq!(r["tools"].as_array().unwrap().len(), 0);
        let output = r["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["id"], "msg_1");
        assert_eq!(output[0]["status"], "completed");
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
        assert_eq!(output[0]["id"], "fc_1_0");
        assert_eq!(output[0]["status"], "completed");
        assert_eq!(output[0]["call_id"], "call_1");
        assert_eq!(output[0]["name"], "weather");
        assert_eq!(output[1]["type"], "function_call_output");
        assert_eq!(output[1]["id"], "fco_2");
        assert_eq!(output[1]["status"], "completed");
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
                    "usage": {
                        "prompt_tokens": 10,
                        "completion_tokens": 20,
                        "prompt_tokens_details": {"cached_tokens": 3},
                        "completion_tokens_details": {"reasoning_tokens": 4}
                    }
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
        assert_eq!(r["usage"]["input_tokens"], 15);
        assert_eq!(r["usage"]["output_tokens"], 35);
        assert_eq!(r["usage"]["total_tokens"], 50);
        assert_eq!(r["usage"]["input_tokens_details"]["cached_tokens"], 3);
        assert_eq!(r["usage"]["output_tokens_details"]["reasoning_tokens"], 4);
    }
}
