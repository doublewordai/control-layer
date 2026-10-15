//! Where a strict-mode request body stopped matching its schema, described
//! without any of the request's content, so it can be recorded for every
//! caller, including those whose bodies must not be kept.
//!
//! The path names only fields the schemas declare. Array positions become
//! `[]`, and every free-form object in the schemas is a `serde_json::Value`,
//! which accepts anything, so deserialisation never fails beneath a key the
//! client chose. Keep it that way: a typed map would put client keys in the
//! path. A key that doesn't look like a field name is replaced by `*` all the
//! same. serde's message, which can quote the offending value, is reduced to
//! a fixed reason.

use serde::de::DeserializeOwned;
use serde_path_to_error::Segment;

/// Longest path recorded; anything longer is cut at a segment boundary.
const MAX_PATH_LEN: usize = 200;
/// Longest key kept as a field name.
const MAX_FIELD_LEN: usize = 64;

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
        && let Some(field) = backquoted(&message).filter(|name| is_field_name(name))
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
    if key == "?" || is_field_name(key) {
        path.push_str(key);
    } else {
        path.push('*');
    }
}

fn is_field_name(key: &str) -> bool {
    let mut chars = key.chars();
    key.len() <= MAX_FIELD_LEN
        && chars
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
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
    fn keys_that_are_not_field_names_are_hidden() {
        let mut path = String::new();
        push_key(&mut path, "messages");
        push_key(&mut path, "Customer Key");
        push_key(&mut path, &"a".repeat(MAX_FIELD_LEN + 1));
        assert_eq!(path, "messages.*.*");
    }

    #[test]
    fn long_paths_are_cut_at_a_segment() {
        let segments = vec![
            Segment::Map {
                key: "nested".to_string(),
            };
            100
        ];
        let described = describe(&segments);
        assert!(described.len() <= MAX_PATH_LEN + 3, "{described}");
        assert!(described.ends_with("nested..."), "{described}");
    }

    #[test]
    fn unrecognised_messages_have_a_fixed_reason() {
        assert_eq!(reason("something the request said"), "other");
        assert_eq!(
            reason("invalid type: string \"x\", expected u32"),
            "invalid_type"
        );
    }
}
