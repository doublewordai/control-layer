//! Outbound request-body preparation middleware.
//!
//! Injects the streaming usage flags on the request's way to onwards, so the
//! upstream reports token usage we can bill from:
//!
//! - for `/chat/completions` and legacy `/completions`, set
//!   `stream_options.include_usage = true` on streaming requests (so the provider
//!   emits a usage frame in the final SSE chunk), and honour the
//!   `x-fusillade-stream` header by forcing `stream: true`.
//!
//! This was dwctl's `stream_usage_transform`, previously wired through onwards'
//! `BodyTransformFn` hook. It deliberately does NOT scrub caller id fields: the
//! inference middleware already strips those in the single parse-and-shape it does
//! at the edge (`scrub_request_id_fields`, ported from onwards #240), so the body
//! that reaches this layer is already scrubbed - a second scrub here would be a
//! no-op duplicate.
//!
//! Placement: innermost dwctl layer, inner to the cache layer (which must hash the
//! original body) and running last before onwards.

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

pub async fn outbound_request_middleware(request: Request, next: Next) -> Response {
    let (parts, body) = request.into_parts();

    let path = parts.uri.path();
    // `/chat/completions` also ends with `/completions`; both take the stream flags.
    if !path.ends_with("/completions") {
        // Nothing to edit (e.g. /responses, /embeddings, /models): forward untouched.
        return next.run(Request::from_parts(parts, body)).await;
    }

    let fusillade_stream = parts.headers.get("x-fusillade-stream").and_then(|v| v.to_str().ok()) == Some("true");

    // Outer layers (onwards body limit, cache) already bound the body, so buffering
    // with no extra limit here can't widen the exposure.
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "failed to read request body").into_response(),
    };

    match transform(&bytes, fusillade_stream) {
        Some(edited) => next.run(Request::from_parts(parts, Body::from(edited))).await,
        None => next.run(Request::from_parts(parts, Body::from(bytes))).await,
    }
}

/// Inject the streaming usage flags into a JSON body, returning `Some(new_bytes)`
/// only when something changed. A body that is not a JSON object (or fails to
/// parse) is left untouched (`None`) - onwards still validates and rejects it.
fn transform(bytes: &Bytes, fusillade_stream: bool) -> Option<Vec<u8>> {
    let mut value = serde_json::from_slice::<Value>(bytes).ok()?;
    let obj = value.as_object_mut()?;
    let mut changed = false;

    let request_streaming = obj.get("stream").and_then(Value::as_bool) == Some(true) || fusillade_stream;
    if request_streaming {
        // Force stream:true when fusillade asked for it via header.
        if fusillade_stream && obj.get("stream").and_then(Value::as_bool) != Some(true) {
            obj.insert("stream".to_string(), Value::Bool(true));
            changed = true;
        }
        // Ensure stream_options.include_usage = true. Only when stream_options is
        // absent or already an object; a `null` (or otherwise non-object) value is
        // left alone, matching the old transform's graceful skip.
        let stream_options = obj.entry("stream_options").or_insert_with(|| Value::Object(Default::default()));
        if let Some(so) = stream_options.as_object_mut()
            && so.get("include_usage").and_then(Value::as_bool) != Some(true)
        {
            so.insert("include_usage".to_string(), Value::Bool(true));
            changed = true;
        }
    }

    if changed { serde_json::to_vec(&value).ok() } else { None }
}

#[cfg(test)]
mod tests {
    use super::transform;
    use bytes::Bytes;

    fn run(body: &serde_json::Value, fusillade: bool) -> Option<serde_json::Value> {
        let bytes = Bytes::from(serde_json::to_vec(body).unwrap());
        transform(&bytes, fusillade).map(|b| serde_json::from_slice(&b).unwrap())
    }

    #[test]
    fn injects_stream_options_when_streaming() {
        let body = serde_json::json!({"model": "gpt-4", "messages": [], "stream": true});
        let out = run(&body, false).expect("should transform");
        assert_eq!(out["stream_options"]["include_usage"], true);
    }

    #[test]
    fn skips_non_streaming() {
        let body = serde_json::json!({"model": "gpt-4", "messages": [], "stream": false});
        assert!(run(&body, false).is_none());
    }

    #[test]
    fn fusillade_header_forces_stream_and_usage() {
        let body = serde_json::json!({"model": "gpt-4", "messages": []});
        let out = run(&body, true).expect("should transform");
        assert_eq!(out["stream"], true);
        assert_eq!(out["stream_options"]["include_usage"], true);
    }

    #[test]
    fn null_stream_options_left_alone() {
        let body = serde_json::json!({"model": "gpt-4", "messages": [], "stream": true, "stream_options": null});
        assert!(run(&body, false).is_none());
    }
}
