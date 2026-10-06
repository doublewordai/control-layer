//! Chat request parameters that not every worker behind a model can honour.
//!
//! In strict mode, every chat request is checked against [`PARAMS`] before it
//! is forwarded. Each match is logged and counted; a parameter configured as
//! rejected (see
//! [`AppState::with_rejected_params`](crate::AppState::with_rejected_params))
//! gets a 400 instead of being forwarded.
//!
//! The list and its neutral values mirror the fields the Dynamo OpenRouter
//! proxy worker refuses (`UNSUPPORTED_FIELDS`, `NEUTRAL_VALUES` and
//! `UNSUPPORTED_BOOLEANS` in dynamo's `lib/spillover/proxy-core/src/chat_request.rs`),
//! so the gateway sees the same requests the proxy would refuse. Keep the two
//! lists in step. `chat_template_kwargs` is left out: strict mode refuses it
//! outright in favour of `reasoning_effort`.

use serde_json::Value;

/// The values of a parameter that ask for nothing beyond its default, so
/// count as absent. `null` always counts as absent.
enum Neutral {
    None,
    Int(u64),
    Bool(bool),
    /// An object holding only thinking controls, which every worker serves.
    ThinkingArgs,
}

const THINKING_TEMPLATE_KEYS: &[&str] = &[
    "thinking",
    "enable_thinking",
    "thinking_mode",
    "reasoning_effort",
];

/// Every parameter this module can flag, in the order they are reported.
/// Metric labels and the reject list take names only from here.
const CATALOG: &[(&str, Neutral)] = &[
    ("n", Neutral::Int(1)),
    ("logprobs", Neutral::Bool(false)),
    ("top_logprobs", Neutral::Int(0)),
    ("prompt_logprobs", Neutral::None),
    ("min_tokens", Neutral::Int(0)),
    ("ignore_eos", Neutral::Bool(false)),
    ("include_stop_str_in_output", Neutral::Bool(false)),
    ("skip_special_tokens", Neutral::Bool(true)),
    ("add_generation_prompt", Neutral::Bool(true)),
    ("continue_final_message", Neutral::Bool(false)),
    ("chat_template_args", Neutral::ThinkingArgs),
    ("guided_json", Neutral::None),
    ("guided_regex", Neutral::None),
    ("guided_grammar", Neutral::None),
    ("guided_choice", Neutral::None),
    ("guided_decoding_backend", Neutral::None),
    ("guided_whitespace_pattern", Neutral::None),
    ("functions", Neutral::None),
    ("function_call", Neutral::None),
    ("modalities", Neutral::None),
    ("audio", Neutral::None),
    ("prediction", Neutral::None),
    ("web_search_options", Neutral::None),
    ("mm_processor_kwargs", Neutral::None),
    ("media_io_kwargs", Neutral::None),
];

/// The names of every parameter this module can flag.
pub fn params() -> impl Iterator<Item = &'static str> {
    CATALOG.iter().map(|(name, _)| *name)
}

/// Look up a configured parameter name in [`params`].
pub fn known_param(name: &str) -> Option<&'static str> {
    params().find(|param| *param == name)
}

/// The flagged parameters a chat request body uses, in catalog order.
pub fn chat_request_params(body: &Value) -> Vec<&'static str> {
    CATALOG
        .iter()
        .filter(|(name, neutral)| {
            body.get(*name)
                .is_some_and(|value| !value.is_null() && !is_neutral(value, neutral))
        })
        .map(|(name, _)| *name)
        .collect()
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

fn is_neutral(value: &Value, neutral: &Neutral) -> bool {
    match neutral {
        Neutral::None => false,
        Neutral::Int(n) => value.as_u64() == Some(*n),
        Neutral::Bool(b) => value.as_bool() == Some(*b),
        Neutral::ThinkingArgs => value.as_object().is_some_and(|args| {
            args.keys()
                .all(|key| THINKING_TEMPLATE_KEYS.contains(&key.as_str()))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert!(chat_request_params(&with(json!({"temperature": 0.5, "top_k": 20}))).is_empty());
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
            "chat_template_args": {"thinking": true, "reasoning_effort": "low"}
        }));
        assert!(chat_request_params(&body).is_empty());
    }

    #[test]
    fn meaningful_values_are_flagged_in_catalog_order() {
        let body = with(json!({
            "guided_json": {"type": "object"},
            "top_logprobs": 5,
            "logprobs": true,
            "n": 2,
            "min_tokens": 4,
            "skip_special_tokens": false,
            "chat_template_args": {"enable_thinking": true, "custom_flag": 1}
        }));
        assert_eq!(
            chat_request_params(&body),
            vec![
                "n",
                "logprobs",
                "top_logprobs",
                "min_tokens",
                "skip_special_tokens",
                "chat_template_args",
                "guided_json",
            ]
        );
    }

    #[test]
    fn values_of_the_wrong_type_are_flagged() {
        let body = with(json!({
            "ignore_eos": "true",
            "n": "1",
            "chat_template_args": "raw"
        }));
        assert_eq!(
            chat_request_params(&body),
            vec!["n", "ignore_eos", "chat_template_args"]
        );
    }

    #[test]
    fn known_params_come_from_the_catalog() {
        assert_eq!(known_param("n"), Some("n"));
        assert_eq!(known_param("top_k"), None);
        assert_eq!(known_param("chat_template_kwargs"), None);
    }

    #[test]
    fn rejection_message_names_every_param() {
        assert_eq!(
            rejection_message(&["logprobs", "top_logprobs"]),
            "Unsupported parameter(s): `logprobs`, `top_logprobs`"
        );
    }
}
