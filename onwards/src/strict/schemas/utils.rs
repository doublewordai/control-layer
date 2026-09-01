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
