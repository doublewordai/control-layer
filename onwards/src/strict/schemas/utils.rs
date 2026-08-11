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
    remove_keys_from_extra(
        extra,
        &[
            "id",
            "completion_id",
            "completionId",
            "response_id",
            "responseId",
        ],
    )
}

/// Remove a caller-supplied scheduling `priority` captured by a
/// `#[serde(flatten)]` extras bag.
///
/// `priority` steers OUR scheduling rather than the model's output: the dynamo
/// scheduler orders its queue by it, so left in place any caller could put
/// themselves ahead of every realtime request on the platform. The chat schema
/// forwards unmodelled fields verbatim, so its bag is a way past a schema that
/// never modelled the field — hence this scrub. (The completions schema is
/// strict and has no bag; it models `priority` and clears the typed field.)
pub(crate) fn strip_priority_from_extra(extra: &mut Option<Value>) {
    remove_keys_from_extra(extra, &["priority"])
}

fn remove_keys_from_extra(extra: &mut Option<Value>, keys: &[&str]) {
    let Some(Value::Object(object)) = extra.as_mut() else {
        return;
    };

    for key in keys {
        object.remove(*key);
    }

    if object.is_empty() {
        *extra = None;
    }
}
