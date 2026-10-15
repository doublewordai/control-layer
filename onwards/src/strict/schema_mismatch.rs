//! Where a strict-mode request body stopped matching its schema, described
//! without any of the request's content, so it can be recorded for every
//! caller, including those whose bodies must not be kept.
//!
//! Array positions become `[]`, and a key is kept only if the schemas declare
//! it as a field; anything else becomes `*`. Checking keys against the schemas
//! rather than their shape matters: a free-form `serde_json::Value` can fail
//! beneath keys the client chose (with serde_json's `raw_value` feature, an
//! object keyed `$serde_json::private::RawValue` must hold a string). serde's
//! message, which can quote the offending value, is reduced to a fixed reason.

use serde::de::DeserializeOwned;
use serde_path_to_error::Segment;

/// Longest path recorded; anything longer is cut at a segment boundary.
const MAX_PATH_LEN: usize = 200;
/// The field names and renames the strict request and response schemas
/// declare, sorted. A name missing here is recorded as `*`, so the list can
/// only hide too much; a test keeps it in step with the schemas.
const DECLARED_FIELDS: &[&str] = &[
    "allowed_tools",
    "annotation_type",
    "annotations",
    "arguments",
    "background",
    "best_of",
    "cached_tokens",
    "call_id",
    "call_type",
    "chat_template_kwargs",
    "choices",
    "code",
    "code_interpreter",
    "completed_at",
    "completion_tokens",
    "completion_tokens_details",
    "computer_use_preview",
    "container",
    "content",
    "content_filter",
    "created",
    "created_at",
    "custom",
    "data",
    "defer_loading",
    "delta",
    "description",
    "detail",
    "dimensions",
    "display_height",
    "display_width",
    "echo",
    "effort",
    "embedding",
    "encoding_format",
    "encrypted_content",
    "environment",
    "error",
    "error_type",
    "event_type",
    "extra",
    "fallback_id",
    "fallback_model",
    "fallback_response_id",
    "file_id",
    "file_search",
    "filename",
    "filters",
    "finish_reason",
    "format",
    "format_type",
    "frequency_penalty",
    "function",
    "headers",
    "hosted_tool",
    "id",
    "ignore_eos",
    "image_url",
    "include_stop_str_in_output",
    "include_usage",
    "incomplete_details",
    "index",
    "input",
    "input_file",
    "input_image",
    "input_text",
    "input_tokens",
    "input_tokens_details",
    "instructions",
    "json_object",
    "json_schema",
    "logit_bias",
    "logprobs",
    "max_completion_tokens",
    "max_num_results",
    "max_output_tokens",
    "max_tokens",
    "max_tool_calls",
    "mcp",
    "message",
    "messages",
    "metadata",
    "min_tokens",
    "model",
    "n",
    "name",
    "namespace",
    "object",
    "output",
    "output_text",
    "output_tokens",
    "output_tokens_details",
    "parallel_tool_calls",
    "param",
    "parameters",
    "presence_penalty",
    "previous_response_id",
    "priority",
    "prompt",
    "prompt_cache_key",
    "prompt_tokens",
    "prompt_tokens_details",
    "reason",
    "reasoning",
    "reasoning_content",
    "reasoning_details",
    "reasoning_effort",
    "reasoning_text",
    "reasoning_tokens",
    "refusal",
    "require_approval",
    "response",
    "response_format",
    "role",
    "safety_identifier",
    "schema",
    "search_context_size",
    "seed",
    "sequence_number",
    "server_label",
    "server_url",
    "service_tier",
    "skip_special_tokens",
    "status",
    "stop",
    "stop_token_ids",
    "store",
    "stream",
    "stream_options",
    "strict",
    "suffix",
    "summary",
    "summary_text",
    "system_fingerprint",
    "temperature",
    "text",
    "thinking",
    "thinking_token_budget",
    "tool_call_id",
    "tool_calls",
    "tool_choice",
    "tool_type",
    "tools",
    "top_logprobs",
    "top_p",
    "total_tokens",
    "truncation",
    "type",
    "url",
    "usage",
    "user",
    "user_location",
    "value",
    "vector_store_ids",
    "web_search_preview",
];

/// Where and how a body failed to deserialise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SchemaMismatch {
    /// For example `messages[].tool_calls[].function.arguments`. `None` when
    /// the body itself has the wrong shape.
    pub path: Option<String>,
    /// One of a fixed set, such as `invalid_type` or `missing_field`.
    pub reason: &'static str,
}

/// Finds where `body`, which failed to deserialise as `T` with a data error,
/// stopped matching. Bodies are parsed a second time only on this path, so
/// requests that pass pay nothing for it.
pub(crate) fn locate<T: DeserializeOwned>(body: &[u8]) -> Option<SchemaMismatch> {
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let error = serde_path_to_error::deserialize::<_, T>(&mut deserializer).err()?;
    let message = error.inner().to_string();
    let reason = reason(&message);
    let mut path = describe(error.path().iter());
    // serde reports a missing field at its parent and names it in the message;
    // the name comes from the schema, not the request.
    if reason == "missing_field"
        && let Some(field) = backquoted(&message).filter(|name| is_declared(name))
    {
        push_key(&mut path, field);
    }
    Some(SchemaMismatch {
        path: (!path.is_empty()).then_some(path),
        reason,
    })
}

fn describe<'a>(segments: impl IntoIterator<Item = &'a Segment>) -> String {
    let mut described = String::new();
    for segment in segments {
        let before = described.len();
        match segment {
            Segment::Seq { .. } => described.push_str("[]"),
            Segment::Map { key } => push_key(&mut described, key),
            Segment::Enum { variant } => push_key(&mut described, variant),
            Segment::Unknown => push_key(&mut described, "?"),
        }
        if described.len() > MAX_PATH_LEN {
            described.truncate(before);
            described.push_str("...");
            break;
        }
    }
    described
}

fn push_key(path: &mut String, key: &str) {
    if !path.is_empty() {
        path.push('.');
    }
    if key == "?" || is_declared(key) {
        path.push_str(key);
    } else {
        path.push('*');
    }
}

fn is_declared(key: &str) -> bool {
    DECLARED_FIELDS.binary_search(&key).is_ok()
}

/// The first `` `name` `` in a serde message.
fn backquoted(message: &str) -> Option<&str> {
    let start = message.find('`')? + 1;
    let len = message[start..].find('`')?;
    Some(&message[start..start + len])
}

fn reason(message: &str) -> &'static str {
    const REASONS: &[(&str, &str)] = &[
        ("invalid type", "invalid_type"),
        ("invalid value", "invalid_value"),
        ("invalid length", "invalid_length"),
        ("unknown variant", "unknown_variant"),
        ("unknown field", "unknown_field"),
        ("missing field", "missing_field"),
        ("duplicate field", "duplicate_field"),
        ("data did not match any variant", "no_matching_variant"),
    ];
    REASONS
        .iter()
        .find(|(prefix, _)| message.starts_with(prefix))
        .map_or("other", |(_, reason)| reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strict::schemas::chat_completions::ChatCompletionRequest;
    use serde::Deserialize;
    use serde_json::{Value, json};

    #[allow(dead_code)]
    #[derive(Debug, Deserialize)]
    struct Request {
        model: String,
        messages: Vec<Message>,
        max_tokens: Option<u32>,
        metadata: Option<Value>,
        tool_choice: Option<Choice>,
        #[serde(flatten)]
        extra: Option<Value>,
    }

    #[allow(dead_code)]
    #[derive(Debug, Deserialize)]
    struct Message {
        role: String,
        content: Option<String>,
        #[serde(flatten)]
        extra: Option<Value>,
    }

    #[allow(dead_code)]
    #[derive(Debug, Deserialize)]
    #[serde(untagged)]
    enum Choice {
        Mode(String),
        Named { name: String },
    }

    fn locate_json(body: Value) -> SchemaMismatch {
        locate::<Request>(body.to_string().as_bytes()).expect("the body should not match")
    }

    fn mismatch(path: Option<&str>, reason: &'static str) -> SchemaMismatch {
        SchemaMismatch {
            path: path.map(str::to_string),
            reason,
        }
    }

    #[test]
    fn names_the_field_and_hides_array_positions() {
        let found = locate_json(json!({
            "model": "m",
            "messages": [{"role": "user"}, {"role": "user", "content": 7}]
        }));
        assert_eq!(found, mismatch(Some("messages[].content"), "invalid_type"));
    }

    #[test]
    fn appends_a_missing_field_to_its_parent() {
        let found = locate_json(json!({"model": "m", "messages": [{"content": "x"}]}));
        assert_eq!(found, mismatch(Some("messages[].role"), "missing_field"));
    }

    #[test]
    fn a_wrongly_shaped_body_has_no_path() {
        let found = locate_json(json!(["not", "an", "object"]));
        assert_eq!(found, mismatch(None, "invalid_type"));
    }

    #[test]
    fn untagged_enums_are_named_but_not_entered() {
        let found = locate_json(json!({"model": "m", "messages": [], "tool_choice": 3}));
        assert_eq!(found, mismatch(Some("tool_choice"), "no_matching_variant"));
    }

    #[test]
    fn values_never_reach_the_description() {
        let secret = "secret-value-1234";
        let found = locate_json(json!({
            "model": "m",
            "messages": [],
            "max_tokens": secret,
            "metadata": {secret: secret}
        }));
        assert_eq!(found, mismatch(Some("max_tokens"), "invalid_type"));
        assert!(!format!("{found:?}").contains("secret"));
    }

    #[test]
    fn keys_the_schemas_do_not_declare_are_hidden() {
        let mut path = String::new();
        push_key(&mut path, "messages");
        push_key(&mut path, "client_secret");
        push_key(&mut path, "Customer Key");
        assert_eq!(path, "messages.*.*");
    }

    #[test]
    fn client_keys_inside_free_form_values_are_hidden() {
        // With serde_json's `raw_value` feature, this object must hold a
        // string, so the free-form `logit_bias` fails beneath a client key.
        let body = br#"{"model":"m","messages":[],"logit_bias":{"client_secret":{"$serde_json::private::RawValue":1}}}"#;
        let found = locate::<ChatCompletionRequest>(body).expect("the body should not match");
        assert_eq!(found.path.as_deref(), Some("logit_bias.*.*"));
    }

    #[test]
    fn long_paths_are_cut_at_a_segment() {
        let segments = vec![
            Segment::Map {
                key: "messages".to_string(),
            };
            100
        ];
        let described = describe(&segments);
        assert!(described.len() <= MAX_PATH_LEN + 3, "{described}");
        assert!(described.ends_with("messages..."), "{described}");
    }

    #[test]
    fn unrecognised_messages_have_a_fixed_reason() {
        assert_eq!(reason("something the request said"), "other");
        assert_eq!(
            reason("invalid type: string \"x\", expected u32"),
            "invalid_type"
        );
    }
    /// Every field name and serde rename declared outside the tests of a
    /// schema source file. A line scan rather than a parser: it may find a
    /// few names that aren't JSON keys, which only widens the list.
    fn declared_in(source: &str) -> Vec<String> {
        let source = source.split("#[cfg(test)]").next().unwrap_or(source);
        let mut names = Vec::new();
        for line in source.lines().map(str::trim) {
            for marker in ["rename = \"", "alias = \""] {
                let mut rest = line;
                while let Some(start) = rest.find(marker) {
                    rest = &rest[start + marker.len()..];
                    if let Some(end) = rest.find('"') {
                        names.push(rest[..end].to_string());
                    }
                }
            }
            if line.starts_with("//") || line.starts_with("let ") {
                continue;
            }
            let field = line
                .strip_prefix("pub(crate) ")
                .or_else(|| line.strip_prefix("pub "))
                .unwrap_or(line);
            let field = field.strip_prefix("r#").unwrap_or(field);
            let len = field
                .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
                .unwrap_or(field.len());
            let after = field[len..].trim_start();
            if len > 0
                && field.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
                && after.starts_with(':')
                && !after.starts_with("::")
            {
                names.push(field[..len].to_string());
            }
        }
        names
    }

    fn schema_names() -> std::collections::BTreeSet<String> {
        [
            include_str!("schemas/chat_completions.rs"),
            include_str!("schemas/completions.rs"),
            include_str!("schemas/embeddings.rs"),
            include_str!("schemas/responses.rs"),
        ]
        .into_iter()
        .flat_map(declared_in)
        .collect()
    }

    #[test]
    fn declared_fields_are_sorted_and_unique() {
        assert!(DECLARED_FIELDS.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn declared_fields_cover_the_schemas() {
        let missing: Vec<_> = schema_names()
            .into_iter()
            .filter(|name| !is_declared(name))
            .collect();
        assert!(missing.is_empty(), "add to DECLARED_FIELDS: {missing:?}");
    }
}
