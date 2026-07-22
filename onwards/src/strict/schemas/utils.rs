use serde_json::{Map, Value};

/// Insert a schema-valid placeholder only when the provider omitted a field.
///
/// This helper is used by strict-mode response sanitizers, not by the serde
/// schema types themselves. Keeping these defaults out of the struct
/// definitions avoids silently relaxing every deserialize path, including
/// internal codepaths that should stay strict.
pub(crate) fn ensure_field(
    object: &mut Map<String, Value>,
    key: &str,
    default: impl FnOnce() -> Value,
) {
    if !object.contains_key(key) {
        object.insert(key.to_string(), default());
    }
}

/// Remove caller-supplied completion/response identifiers captured by
/// `#[serde(flatten)]` request extras before forwarding to an upstream LLM.
pub(crate) fn scrub_request_id_fields_from_extra(extra: &mut Option<Value>) {
    let Some(Value::Object(object)) = extra.as_mut() else {
        return;
    };

    for key in [
        "id",
        "completion_id",
        "completionId",
        "response_id",
        "responseId",
    ] {
        object.remove(key);
    }

    if object.is_empty() {
        *extra = None;
    }
}
