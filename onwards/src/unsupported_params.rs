//! Request parameters that not every worker behind a model can honour.
//!
//! A chat request is checked against this list before it is forwarded. Each
//! match is logged and counted; a parameter configured as rejected (see
//! [`AppState::with_rejected_params`](crate::AppState::with_rejected_params))
//! gets a 400 instead of being forwarded.
//!
//! The list and its neutral values mirror the fields the Dynamo OpenRouter
//! proxy worker refuses (`UNSUPPORTED_FIELDS`, `NEUTRAL_VALUES` and
//! `UNSUPPORTED_BOOLEANS` in dynamo's `lib/spillover/proxy-core/src/chat_request.rs`),
//! so the gateway sees the same requests the proxy would refuse. Keep the two
//! lists in step.

use serde_json::{Map, Value};

use crate::strict::schemas::chat_completions::ChatCompletionRequest;

/// Fields flagged whenever they carry a non-null, non-neutral value.
const VALUE_FIELDS: &[&str] = &[
    "guided_json",
    "guided_regex",
    "guided_grammar",
    "guided_choice",
    "guided_decoding_backend",
    "guided_whitespace_pattern",
    "min_tokens",
    "prompt_logprobs",
    "functions",
    "function_call",
    "modalities",
    "audio",
    "prediction",
    "web_search_options",
    "mm_processor_kwargs",
    "media_io_kwargs",
];

/// Values that ask for nothing beyond the field's default, so they count as absent.
const NEUTRAL_VALUES: &[(&str, u64)] = &[("top_logprobs", 0), ("min_tokens", 0)];

/// Boolean fields, mapped to the value that changes generation. The other
/// value is the engine default and counts as absent.
const BOOLEAN_FIELDS: &[(&str, bool)] = &[
    ("logprobs", true),
    ("ignore_eos", true),
    ("include_stop_str_in_output", true),
    ("skip_special_tokens", false),
    ("add_generation_prompt", false),
    ("continue_final_message", true),
];

/// Chat-template argument fields. Only the thinking controls in
/// [`THINKING_TEMPLATE_KEYS`] can be served everywhere; any other key is flagged.
const TEMPLATE_ARG_FIELDS: &[&str] = &["chat_template_args", "chat_template_kwargs"];

const THINKING_TEMPLATE_KEYS: &[&str] = &[
    "thinking",
    "enable_thinking",
    "thinking_mode",
    "reasoning_effort",
];

/// Every parameter this module can flag. Metric labels and the reject list
/// take values only from here.
pub const PARAMS: &[&str] = &[
    "n",
    "logprobs",
    "top_logprobs",
    "prompt_logprobs",
    "min_tokens",
    "ignore_eos",
    "include_stop_str_in_output",
    "skip_special_tokens",
    "add_generation_prompt",
    "continue_final_message",
    "chat_template_args",
    "chat_template_kwargs",
    "guided_json",
    "guided_regex",
    "guided_grammar",
    "guided_choice",
    "guided_decoding_backend",
    "guided_whitespace_pattern",
    "functions",
    "function_call",
    "modalities",
    "audio",
    "prediction",
    "web_search_options",
    "mm_processor_kwargs",
    "media_io_kwargs",
];

/// The parameters a request uses from [`PARAMS`], carried as a request
/// extension from the strict handler to the shared forwarding handler.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlaggedParams(pub Vec<&'static str>);

/// Look up a configured parameter name in [`PARAMS`].
pub fn known_param(name: &str) -> Option<&'static str> {
    PARAMS.iter().copied().find(|param| *param == name)
}

/// The parameters of [`PARAMS`] a chat request uses, in [`PARAMS`] order.
pub fn chat_request_params(request: &ChatCompletionRequest) -> FlaggedParams {
    let empty = Map::new();
    let extra = request
        .extra
        .as_ref()
        .and_then(Value::as_object)
        .unwrap_or(&empty);

    let mut found = Vec::new();
    if request.n.is_some_and(|n| n != 1) {
        found.push("n");
    }
    if request.logprobs == Some(true) {
        found.push("logprobs");
    }
    if request
        .top_logprobs
        .is_some_and(|value| !is_neutral("top_logprobs", u64::from(value)))
    {
        found.push("top_logprobs");
    }
    for (field, meaningful) in BOOLEAN_FIELDS {
        // `logprobs` is a typed field, handled above.
        if extra.get(*field).and_then(Value::as_bool) == Some(*meaningful) {
            found.push(*field);
        }
    }
    for field in TEMPLATE_ARG_FIELDS {
        if extra
            .get(*field)
            .is_some_and(|args| !args.is_null() && !only_thinking_keys(args))
        {
            found.push(*field);
        }
    }
    for field in VALUE_FIELDS {
        if extra.get(*field).is_some_and(|value| {
            !value.is_null() && !value.as_u64().is_some_and(|n| is_neutral(field, n))
        }) {
            found.push(*field);
        }
    }

    found.sort_by_key(|param| PARAMS.iter().position(|known| known == param));
    FlaggedParams(found)
}

/// The client-facing message for rejected parameters, in the same style as
/// Dynamo's unknown-field error.
pub fn rejection_message(params: &[&str]) -> String {
    let quoted = params
        .iter()
        .map(|param| format!("`{param}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("Unsupported parameter(s): {quoted}")
}

fn is_neutral(field: &str, value: u64) -> bool {
    NEUTRAL_VALUES
        .iter()
        .any(|(neutral_field, neutral)| *neutral_field == field && value == *neutral)
}

fn only_thinking_keys(args: &Value) -> bool {
    match args {
        Value::Object(map) => map
            .keys()
            .all(|key| THINKING_TEMPLATE_KEYS.contains(&key.as_str())),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params(body: Value) -> Vec<&'static str> {
        let request: ChatCompletionRequest = serde_json::from_value(body).unwrap();
        chat_request_params(&request).0
    }

    fn with(fields: Value) -> Value {
        let mut body = json!({
            "model": "example-model",
            "messages": [{"role": "user", "content": "Hello"}]
        });
        body.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        body
    }

    #[test]
    fn plain_request_flags_nothing() {
        assert!(params(with(json!({"temperature": 0.5, "top_k": 20}))).is_empty());
    }

    #[test]
    fn neutral_values_count_as_absent() {
        let body = with(json!({
            "n": 1,
            "logprobs": false,
            "top_logprobs": 0,
            "min_tokens": 0,
            "ignore_eos": false,
            "skip_special_tokens": true,
            "add_generation_prompt": true,
            "prompt_logprobs": null,
            "chat_template_kwargs": {"enable_thinking": false},
            "chat_template_args": {"thinking": true, "reasoning_effort": "low"}
        }));
        assert!(params(body).is_empty());
    }

    #[test]
    fn meaningful_values_are_flagged_in_list_order() {
        let body = with(json!({
            "guided_json": {"type": "object"},
            "top_logprobs": 5,
            "logprobs": true,
            "n": 2,
            "min_tokens": 4,
            "skip_special_tokens": false,
            "chat_template_kwargs": {"enable_thinking": true, "custom_flag": 1}
        }));
        assert_eq!(
            params(body),
            vec![
                "n",
                "logprobs",
                "top_logprobs",
                "min_tokens",
                "skip_special_tokens",
                "chat_template_kwargs",
                "guided_json",
            ]
        );
    }

    #[test]
    fn non_object_template_args_are_flagged() {
        assert_eq!(
            params(with(json!({"chat_template_args": "raw"}))),
            vec!["chat_template_args"]
        );
    }

    #[test]
    fn every_listed_field_is_a_known_param() {
        let listed = VALUE_FIELDS
            .iter()
            .chain(TEMPLATE_ARG_FIELDS)
            .chain(BOOLEAN_FIELDS.iter().map(|(field, _)| field))
            .chain(NEUTRAL_VALUES.iter().map(|(field, _)| field));
        for field in listed {
            assert_eq!(
                known_param(field),
                Some(*field),
                "{field} missing from PARAMS"
            );
        }
        assert_eq!(known_param("n"), Some("n"));
        assert_eq!(known_param("top_k"), None);
    }

    #[test]
    fn rejection_message_names_every_param() {
        assert_eq!(
            rejection_message(&["logprobs", "top_logprobs"]),
            "Unsupported parameter(s): `logprobs`, `top_logprobs`"
        );
    }
}
