//! Outbound request-body preparation middleware.
//!
//! Small dwctl-owned edits applied to the request body on its way to onwards, so
//! that onwards can forward the body untouched (it validates in strict mode and
//! forwards the original bytes). Consolidates two manipulations that previously
//! lived in onwards:
//!
//! - **id-scrub**: strip caller-supplied response/completion identifiers
//!   (`id`, `completion_id`, `response_id`, ...) from the top level of a
//!   `/chat/completions` body, so a client can't smuggle an id upstream. This was
//!   onwards' `scrub_request_id_fields`. Applied on the chat path only: translated
//!   `/responses` and `/messages` requests are already `/chat/completions` by the
//!   time they reach this layer, so the chat path covers every case onwards did.
//! - **streaming usage flags**: for `/chat/completions` and legacy `/completions`,
//!   inject `stream_options.include_usage = true` on streaming requests (so the
//!   provider emits a usage frame we can bill from), and honour the
//!   `x-fusillade-stream` header by forcing `stream: true`. This was dwctl's
//!   `stream_usage_transform`, previously wired through onwards' `BodyTransformFn`
//!   hook.
//!
//! Placement: innermost dwctl layer, inner to the cache layer (which must hash the
//! original body) and running last before onwards - mirroring where these edits
//! used to happen at the forward boundary. The `background` strip the old
//! transform did for `/responses` is intentionally dropped: by this layer the
//! translation middleware has already flattened `/responses` to a chat body and
//! `background` was consumed by the (outer) inference middleware, so it never
//! arrives here.

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

/// Caller-supplied identifier fields stripped from a chat request body before it
/// is forwarded upstream (a verbatim move of onwards' `scrub_request_id_fields`).
const SCRUBBED_ID_FIELDS: [&str; 5] = ["id", "completion_id", "completionId", "response_id", "responseId"];

pub async fn outbound_request_middleware(request: Request, next: Next) -> Response {
    let (parts, body) = request.into_parts();

    let path = parts.uri.path();
    // `/chat/completions` also ends with `/completions`; both are stream-flag routes,
    // but only the chat path is id-scrubbed (matching onwards).
    let is_completions = path.ends_with("/completions");
    let is_chat = path.ends_with("/chat/completions");
    if !is_completions {
        // Nothing to edit (e.g. /embeddings, /models): forward untouched, no buffering.
        return next.run(Request::from_parts(parts, body)).await;
    }

    let fusillade_stream = parts.headers.get("x-fusillade-stream").and_then(|v| v.to_str().ok()) == Some("true");

    // Outer layers (onwards body limit, cache) already bound the body, so buffering
    // with no extra limit here can't widen the exposure.
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "failed to read request body").into_response(),
    };

    match transform(&bytes, is_chat, fusillade_stream) {
        Some(edited) => next.run(Request::from_parts(parts, Body::from(edited))).await,
        None => next.run(Request::from_parts(parts, Body::from(bytes))).await,
    }
}

/// Apply the edits to a JSON body, returning `Some(new_bytes)` only when something
/// changed. A body that is not a JSON object (or fails to parse) is left untouched
/// (`None`) - onwards still validates and rejects malformed bodies.
fn transform(bytes: &Bytes, is_chat: bool, fusillade_stream: bool) -> Option<Vec<u8>> {
    let mut value = serde_json::from_slice::<Value>(bytes).ok()?;
    let obj = value.as_object_mut()?;
    let mut changed = false;

    // id-scrub (chat path only)
    if is_chat {
        for key in SCRUBBED_ID_FIELDS {
            if obj.remove(key).is_some() {
                changed = true;
            }
        }
    }

    // streaming usage flags (chat + legacy completions)
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

    fn run(body: &serde_json::Value, is_chat: bool, fusillade: bool) -> Option<serde_json::Value> {
        let bytes = Bytes::from(serde_json::to_vec(body).unwrap());
        transform(&bytes, is_chat, fusillade).map(|b| serde_json::from_slice(&b).unwrap())
    }

    #[test]
    fn scrubs_caller_id_fields_on_chat() {
        let body = serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}],
            "id": "smuggled",
            "response_id": "smuggled",
            "completion_id": "smuggled"
        });
        let out = run(&body, true, false).expect("should transform");
        assert!(out.get("id").is_none());
        assert!(out.get("response_id").is_none());
        assert!(out.get("completion_id").is_none());
        assert_eq!(out["model"], "gpt-4");
    }

    #[test]
    fn does_not_scrub_on_legacy_completions() {
        // onwards only scrubbed chat + responses; legacy /completions was untouched.
        let body = serde_json::json!({"model": "gpt-4", "prompt": "hi", "id": "keep"});
        assert!(run(&body, false, false).is_none());
    }

    #[test]
    fn injects_stream_options_when_streaming() {
        let body = serde_json::json!({"model": "gpt-4", "messages": [], "stream": true});
        let out = run(&body, true, false).expect("should transform");
        assert_eq!(out["stream_options"]["include_usage"], true);
    }

    #[test]
    fn skips_non_streaming() {
        let body = serde_json::json!({"model": "gpt-4", "messages": [], "stream": false});
        assert!(run(&body, true, false).is_none());
    }

    #[test]
    fn fusillade_header_forces_stream_and_usage() {
        let body = serde_json::json!({"model": "gpt-4", "messages": []});
        let out = run(&body, true, true).expect("should transform");
        assert_eq!(out["stream"], true);
        assert_eq!(out["stream_options"]["include_usage"], true);
    }

    #[test]
    fn null_stream_options_left_alone() {
        let body = serde_json::json!({"model": "gpt-4", "messages": [], "stream": true, "stream_options": null});
        // stream_options is null (not an object): no include_usage injected, no change.
        assert!(run(&body, true, false).is_none());
    }

    #[test]
    fn combines_scrub_and_stream_flags() {
        let body = serde_json::json!({"model": "gpt-4", "messages": [], "stream": true, "id": "x"});
        let out = run(&body, true, false).expect("should transform");
        assert!(out.get("id").is_none());
        assert_eq!(out["stream_options"]["include_usage"], true);
    }
}
