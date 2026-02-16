/// Body transform that injects `stream_options.include_usage = true` into streaming
/// completion requests. This ensures upstream providers report token usage in the final
/// SSE chunk, which is required for accurate billing and analytics.
pub fn stream_usage_transform(path: &str, _headers: &axum::http::HeaderMap, body_bytes: &[u8]) -> Option<axum::body::Bytes> {
    if path.ends_with("/completions")
        && let Ok(mut json_body) = serde_json::from_slice::<serde_json::Value>(body_bytes)
        && let Some(obj) = json_body.as_object_mut()
        && obj.get("stream").and_then(|v| v.as_bool()) == Some(true)
    {
        obj.entry("stream_options")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()?
            .insert("include_usage".to_string(), serde_json::json!(true));

        if let Ok(bytes) = serde_json::to_vec(&json_body) {
            return Some(axum::body::Bytes::from(bytes));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::stream_usage_transform;
    use axum::http::HeaderMap;

    fn call(path: &str, body: &serde_json::Value) -> Option<serde_json::Value> {
        let bytes = serde_json::to_vec(body).unwrap();
        stream_usage_transform(path, &HeaderMap::new(), &bytes).map(|b| serde_json::from_slice(&b).unwrap())
    }

    #[test]
    fn injects_stream_options_when_missing() {
        let body = serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        });
        let result = call("/chat/completions", &body).expect("should transform");
        assert_eq!(result["stream_options"]["include_usage"], true);
    }

    #[test]
    fn preserves_existing_stream_options_fields() {
        let body = serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
            "stream_options": {"include_usage": false}
        });
        let result = call("/chat/completions", &body).expect("should transform");
        // include_usage should be overwritten to true
        assert_eq!(result["stream_options"]["include_usage"], true);
    }

    #[test]
    fn skips_non_streaming_requests() {
        let body = serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false
        });
        assert!(call("/chat/completions", &body).is_none());
    }

    #[test]
    fn skips_when_stream_absent() {
        let body = serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}]
        });
        assert!(call("/chat/completions", &body).is_none());
    }

    #[test]
    fn skips_non_completions_paths() {
        let body = serde_json::json!({
            "model": "gpt-4",
            "input": "hello",
            "stream": true
        });
        assert!(call("/embeddings", &body).is_none());
        assert!(call("/responses", &body).is_none());
    }

    #[test]
    fn matches_legacy_completions_path() {
        let body = serde_json::json!({
            "model": "gpt-4",
            "prompt": "hello",
            "stream": true
        });
        let result = call("/completions", &body).expect("should transform");
        assert_eq!(result["stream_options"]["include_usage"], true);
    }

    #[test]
    fn matches_nested_chat_completions_path() {
        let body = serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        });
        let result = call("/v1/chat/completions", &body).expect("should transform");
        assert_eq!(result["stream_options"]["include_usage"], true);
    }

    #[test]
    fn handles_null_stream_options_gracefully() {
        let body = serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
            "stream_options": null
        });
        // stream_options is null, as_object_mut() returns None, ? returns None
        assert!(call("/chat/completions", &body).is_none());
    }
}
