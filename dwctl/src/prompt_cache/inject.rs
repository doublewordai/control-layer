//! OpenAI-shaped request sanitisation and response usage injection,
//! relocated into dwctl for the dwctl-owned cache layer.
//!
//! Two jobs, both run by the cache tower layer (only when a cacheable request is
//! classified):
//!
//! 1. **Outbound request sanitisation** ([`strip_cache_control`]): remove `cache_control`
//!    markers from the exact locations the parser reads them (message content blocks and
//!    tool objects) — NOT recursively — and ensure
//!    `stream_options.include_usage = true` so a streaming response carries a terminal
//!    usage frame to edit. Markers are a billing signal consumed here, not forwarded.
//! 2. **Response usage injection**: splice the neutral [`CacheStats`] into the OpenAI `usage`
//!    object — `prompt_tokens_details.cached_tokens` plus the doubleword extension fields.
//!    Non-streaming ([`inject_into_response_nonstreaming`]) buffers + edits the JSON body;
//!    streaming ([`scan_inject_sse`]) edits *only* the terminal usage frame before `[DONE]`,
//!    never buffering the whole stream (the cache layer drives it so the classify-await is
//!    deferred to that frame and never holds the first token).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::Value;
use tracing::error;

use super::parse::TelemetryPolicy;
use super::stats::CacheStats;

/// Remove `cache_control` markers from the request body at exactly the locations the parser
/// reads them — each `messages[i].content[j]` block and each `tools[i]` object — and NOWHERE
/// else. Returns `(rewrote, had_marker)`: `rewrote` = a marker was removed (so the body
/// changed and must be re-serialised before forwarding — a marker would otherwise leak
/// upstream); `had_marker` = a removed value was NON-NULL, the adoption signal. An explicit
/// `cache_control: null` is "no marker" (matching parse/validation) but is still stripped.
///
/// Deliberately **not recursive**: a `cache_control` nested inside a tool's JSON Schema (e.g.
/// a function parameter literally named `cache_control`) is the caller's own data — deleting
/// it would corrupt the tool the model sees, and it isn't a marker. Stripping only the marker
/// locations also keeps the forwarded bytes identical to what [`super::parse`] hashes/tokenizes.
fn remove_cache_control(body: &mut Value, telemetry: &TelemetryPolicy) -> (bool, bool) {
    let mut rewrote = false;
    let mut had_marker = false;
    let Some(obj) = body.as_object_mut() else {
        return (false, false);
    };

    // Message content blocks (array-form content only; string content carries no marker).
    if let Some(messages) = obj.get_mut("messages").and_then(Value::as_array_mut) {
        for msg in messages.iter_mut() {
            // Read the role before the mutable content borrow — telemetry handling is scoped to the
            // system role (matching `super::parse`), so a non-system block that coincidentally starts
            // with a configured prefix is never mutated out of the forwarded prompt.
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("").to_string();
            if let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) {
                // In strip mode, drop unmarked provider-telemetry blocks (e.g. the Claude Code
                // SDK's `x-anthropic-billing-header` line) from the FORWARDED prompt — done BEFORE
                // stripping cache_control so `excludes_block` still sees the original marker state
                // and never drops a caller-marked block. This keeps the forwarded bytes aligned
                // with what `super::parse` excludes from the cache hash, and lets the upstream
                // KV/prefix cache see a stable prompt.
                if telemetry.strip_from_prompt {
                    let before = content.len();
                    content.retain(|block| !telemetry.excludes_block(&role, block));
                    rewrote |= content.len() != before;
                }
                for block in content.iter_mut() {
                    strip_block_marker(block, &mut rewrote, &mut had_marker);
                }
            }
        }
    }

    // Tool definitions: the top-level of each tool object only — never into its schema.
    if let Some(tools) = obj.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools.iter_mut() {
            strip_block_marker(tool, &mut rewrote, &mut had_marker);
        }
    }

    (rewrote, had_marker)
}

/// Remove a single top-level `cache_control` marker from one block/tool object, recording
/// whether the body changed and whether the marker was non-null.
fn strip_block_marker(value: &mut Value, rewrote: &mut bool, had_marker: &mut bool) {
    if let Some(obj) = value.as_object_mut()
        && let Some(removed) = obj.remove("cache_control")
    {
        *rewrote = true;
        *had_marker |= !removed.is_null();
    }
}

/// Sanitise an outbound request body: strip every `cache_control` marker and, for
/// streaming requests, ensure `stream_options.include_usage = true`. Returns the
/// rewritten bytes when anything changed, or `None` to leave the original untouched. Also
/// returns whether the client actually sent `cache_control` markers (`had_markers`) — the
/// adoption signal, kept distinct from the body changing, since a stream gets
/// `include_usage` injected even when no markers were present.
pub fn strip_cache_control(body: &[u8], telemetry: &TelemetryPolicy) -> (Option<Bytes>, bool) {
    let Ok(mut json) = serde_json::from_slice::<Value>(body) else {
        return (None, false);
    };
    let (rewrote, had_markers) = remove_cache_control(&mut json, telemetry);

    let mut usage_set = false;
    if let Some(obj) = json.as_object_mut() {
        let is_streaming = obj.get("stream").and_then(Value::as_bool) == Some(true);
        if is_streaming {
            let opts = obj.entry("stream_options").or_insert_with(|| serde_json::json!({}));
            if let Some(opts_obj) = opts.as_object_mut() {
                let already = opts_obj.get("include_usage").and_then(Value::as_bool) == Some(true);
                if !already {
                    opts_obj.insert("include_usage".to_string(), serde_json::json!(true));
                    usage_set = true;
                }
            }
        }
    }

    let body = if rewrote || usage_set {
        serde_json::to_vec(&json).ok().map(Bytes::from)
    } else {
        None
    };
    (body, had_markers)
}

/// Splice the OpenAI-shaped cache fields into a `usage` object in place.
/// `prompt_tokens` is left as the full input count; only the cache breakdown is added.
fn splice_cache_fields(usage: &mut serde_json::Map<String, Value>, stats: &CacheStats) {
    let details = usage.entry("prompt_tokens_details").or_insert_with(|| serde_json::json!({}));
    if let Some(details_obj) = details.as_object_mut() {
        details_obj.insert("cached_tokens".to_string(), serde_json::json!(stats.read));
    }
    usage.insert("cache_read_input_tokens".to_string(), serde_json::json!(stats.read));
    usage.insert("cache_creation_input_tokens".to_string(), serde_json::json!(stats.creation_total()));
    usage.insert(
        "cache_creation".to_string(),
        serde_json::json!({
            "ephemeral_5m_input_tokens": stats.creation_5m,
            "ephemeral_1h_input_tokens": stats.creation_1h,
            "ephemeral_24h_input_tokens": stats.creation_24h,
        }),
    );
}

/// Inject the cache stats into a non-streaming chat-completion JSON body. Returns the
/// rewritten body, or `None` if it can't be parsed or has no `usage` object.
pub fn inject_into_usage_json(body: &[u8], stats: &CacheStats) -> Option<Bytes> {
    let mut json: Value = serde_json::from_slice(body).ok()?;
    let obj = json.as_object_mut()?;
    let usage = obj.get_mut("usage")?.as_object_mut()?;
    splice_cache_fields(usage, stats);
    serde_json::to_vec(&json).ok().map(Bytes::from)
}

/// The outcome of scanning one SSE body chunk: the (optionally) rewritten bytes plus the
/// two billing-success signals observed in it. Accumulated across chunks by the streaming
/// path so the cache-commit gate matches what billing sees.
pub(crate) struct SseScan {
    /// `Some` only if a usage frame was found *and* injected this call.
    pub rewritten: Option<Bytes>,
    /// A `data:` frame carrying an `error` payload (mid-stream provider failure).
    pub saw_error: bool,
    /// A `data:` frame carrying a `usage` object (the terminal usage frame).
    pub saw_usage: bool,
}

/// Scan an SSE body for error/usage frames and, unless `already_edited`, inject the cache
/// fields into the first usage frame found. Editing touches only that one frame; every
/// other line (deltas, `[DONE]`) is preserved byte-for-byte. Assumes uncompressed UTF-8
/// `text/event-stream`; non-UTF-8 bodies are a graceful no-op (no scan, no edit).
///
/// Each `data:` line is parsed as a complete JSON object. The SSE spec permits one object
/// to span several `data:` lines (joined by `\n`), but every OpenAI-compatible
/// chat-completions provider emits one compact line per frame, so we don't reassemble.
/// This deliberately matches the billing-path scanner `extract_cache_tokens`
/// (request_logging::serializers) line-for-line: the commit gate's "saw a usage frame" and
/// billing's "found usage" must make the *same* call, or the cache could commit a write for
/// a frame billing reads as zero. If a multi-line provider ever appears, both must learn to
/// reassemble together — not this one alone.
pub(crate) fn scan_inject_sse(body: &[u8], stats: &CacheStats, already_edited: bool) -> SseScan {
    let Ok(body_str) = std::str::from_utf8(body) else {
        return SseScan {
            rewritten: None,
            saw_error: false,
            saw_usage: false,
        };
    };

    // Fast path: the streaming layer probes every frame with `already_edited=true` purely to
    // collect the commit-gate signals — it can never rewrite, so skip the output-buffer rebuild
    // (an allocation + full-body copy per SSE frame otherwise).
    if already_edited {
        let mut saw_error = false;
        let mut saw_usage = false;
        for line in body_str.split('\n') {
            if let Some(chunk) = sse_data_json(line) {
                saw_error |= chunk.get("error").is_some();
                saw_usage |= chunk.get("usage").is_some_and(Value::is_object);
            }
        }
        return SseScan {
            rewritten: None,
            saw_error,
            saw_usage,
        };
    }

    let mut out = String::with_capacity(body_str.len() + 256);
    let mut edited = false;
    let mut saw_error = false;
    let mut saw_usage = false;

    let mut first = true;
    for line in body_str.split('\n') {
        if !first {
            out.push('\n');
        }
        first = false;

        if let Some(mut chunk) = sse_data_json(line) {
            let chunk_obj = chunk.as_object_mut().expect("sse_data_json returns only objects");
            // Observe billing signals on every frame, even after we've injected.
            if chunk_obj.contains_key("error") {
                saw_error = true;
            }
            if let Some(usage) = chunk_obj.get_mut("usage")
                && let Some(usage_obj) = usage.as_object_mut()
            {
                saw_usage = true;
                if !edited {
                    // Preserve the line's terminator style: on a CRLF stream this `line`
                    // (split on '\n') ends with '\r', which the reserialized JSON drops —
                    // re-append it so we don't emit a lone '\n' amid '\r\n' framing.
                    let has_cr = line.ends_with('\r');
                    splice_cache_fields(usage_obj, stats);
                    if let Ok(reserialized) = serde_json::to_string(&chunk) {
                        out.push_str("data: ");
                        out.push_str(&reserialized);
                        if has_cr {
                            out.push('\r');
                        }
                        edited = true;
                        continue;
                    }
                }
            }
        }
        out.push_str(line);
    }

    SseScan {
        rewritten: if edited { Some(Bytes::from(out)) } else { None },
        saw_error,
        saw_usage,
    }
}

/// Parse one SSE line's `data:` payload into its JSON object, or `None` for non-`data` lines,
/// `[DONE]`, unparseable JSON, or non-object payloads. Shared by both the scan-only fast path and
/// the editing path so they make the *identical* "is this a usage/error frame" call — the same
/// invariant the module doc requires against the billing scanner.
fn sse_data_json(line: &str) -> Option<Value> {
    // SSE allows `data:<value>` and `data: <value>` — strip the colon, then an optional single
    // space (matches onwards' own SSE parser).
    let data = line.strip_prefix("data:")?;
    let trimmed = data.strip_prefix(' ').unwrap_or(data).trim();
    if trimmed == "[DONE]" {
        return None;
    }
    serde_json::from_str::<Value>(trimmed).ok().filter(Value::is_object)
}

/// Inject the cache stats into the terminal usage frame of an SSE body. `None` if no usage
/// frame is found. (Thin wrapper over [`scan_inject_sse`]; the streaming path uses the
/// scan directly to also collect the commit-gate signals.)
pub fn inject_into_sse_body(body: &[u8], stats: &CacheStats) -> Option<Bytes> {
    scan_inject_sse(body, stats, false).rewritten
}

/// Inject the cache stats into a **non-streaming** chat-completion JSON response. Buffers the
/// body, splices the cache fields into `usage`, and returns whether the request succeeded for
/// billing — a 2xx *with* a usage object — so the caller gates the index write on it. A body that
/// can't be buffered becomes a structured 5xx with a `false` gate. Streaming responses are handled
/// separately by the cache layer, which defers the classify-await into the SSE stream so it never
/// holds the first token.
pub async fn inject_into_response_nonstreaming(response: Response, stats: &CacheStats) -> (Response, bool) {
    let status_ok = response.status().is_success();

    // Only JSON can carry a chat-completion `usage`; don't buffer explicitly non-JSON bodies
    // (preserve pass-through). Media types are case-insensitive and may carry parameters, so match
    // the trimmed base type case-insensitively. Missing/unknown content-type → try JSON.
    let is_json = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .map(str::trim)
                .is_some_and(|ct| ct.eq_ignore_ascii_case("application/json"))
        })
        .unwrap_or(true);
    if !is_json {
        return (response, false);
    }
    let (mut parts, body) = response.into_parts();
    let body_bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            // Buffering the upstream response failed (e.g. the upstream connection broke
            // mid-read). Forwarding an empty body would hand the client a misleading 200 with no
            // content; instead return a structured 5xx and veto the commit.
            error!("Failed to buffer response body for cache injection: {}", e);
            let err_body = serde_json::json!({
                "error": {
                    "message": format!("failed to read upstream response body: {e}"),
                    "type": "internal_error",
                    "code": "response_body_read_failed",
                }
            });
            return ((StatusCode::INTERNAL_SERVER_ERROR, axum::Json(err_body)).into_response(), false);
        }
    };

    // A present `usage` object is billing's success signal for a non-streamed call (it's where
    // token counts come from); combined with a 2xx status, it gates the write.
    match inject_into_usage_json(&body_bytes, stats) {
        Some(rewritten) => {
            let len = rewritten.len();
            parts.headers.remove(axum::http::header::TRANSFER_ENCODING);
            // We emit plain JSON (parse succeeded), so drop any stale Content-Encoding.
            parts.headers.remove(axum::http::header::CONTENT_ENCODING);
            parts
                .headers
                .insert(axum::http::header::CONTENT_LENGTH, axum::http::HeaderValue::from(len as u64));
            (Response::from_parts(parts, axum::body::Body::from(rewritten)), status_ok)
        }
        None => {
            // No usage object (error body, or non-completion JSON) → never commit.
            let len = body_bytes.len();
            parts.headers.remove(axum::http::header::TRANSFER_ENCODING);
            parts
                .headers
                .insert(axum::http::header::CONTENT_LENGTH, axum::http::HeaderValue::from(len as u64));
            (Response::from_parts(parts, axum::body::Body::from(body_bytes)), false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats() -> CacheStats {
        CacheStats {
            read: 1024,
            creation_5m: 10,
            creation_1h: 20,
            creation_24h: 30,
        }
    }

    #[test]
    fn strip_removes_nested_cache_control_and_sets_include_usage() {
        let body = serde_json::json!({
            "stream": true,
            "messages": [{"role":"system","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral"}}]}]
        })
        .to_string();
        let (out, had_markers) = strip_cache_control(body.as_bytes(), &TelemetryPolicy::default());
        assert!(had_markers, "body had cache_control");
        let out = out.expect("changed");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(!out.windows(13).any(|w| w == b"cache_control"));
        assert_eq!(v["stream_options"]["include_usage"], true);
    }

    #[test]
    fn strip_none_when_nothing_to_do() {
        let body = serde_json::json!({"messages":[{"role":"user","content":"hi"}]}).to_string();
        let (out, had_markers) = strip_cache_control(body.as_bytes(), &TelemetryPolicy::default());
        assert!(out.is_none());
        assert!(!had_markers);
    }

    #[test]
    fn strip_stream_without_markers_changes_body_but_not_marked() {
        // The overcounting case: a stream with no cache_control still gets include_usage
        // injected (body changes), but had_markers must stay false.
        let body = serde_json::json!({"stream": true, "messages":[{"role":"user","content":"hi"}]}).to_string();
        let (out, had_markers) = strip_cache_control(body.as_bytes(), &TelemetryPolicy::default());
        assert!(out.is_some(), "include_usage injected");
        assert!(!had_markers, "no markers present");
    }

    #[test]
    fn strip_null_cache_control_is_removed_but_not_marked() {
        // An explicit `cache_control: null` is "no marker" for the adoption metric (matching
        // parse/validation), but is still stripped so it can't leak to the upstream.
        let body = serde_json::json!({
            "messages": [{"role":"system","content":[{"type":"text","text":"x","cache_control":null}]}]
        })
        .to_string();
        let (out, had_markers) = strip_cache_control(body.as_bytes(), &TelemetryPolicy::default());
        assert!(!had_markers, "null cache_control is not a marker");
        let out = out.expect("body rewritten to drop the null cache_control key");
        assert!(!out.windows(13).any(|w| w == b"cache_control"), "cache_control key removed");
    }

    #[test]
    fn strip_removes_tool_marker_but_preserves_schema_field_named_cache_control() {
        // The top-level tool marker is stripped, but a `cache_control` that is a legitimate
        // JSON-Schema property inside the tool is the caller's data and must be forwarded
        // untouched (the recursive strip used to delete it, corrupting the tool).
        let body = serde_json::json!({
            "tools": [{
                "type": "function",
                "function": {
                    "name": "set_config",
                    "parameters": {"type": "object", "properties": {
                        "cache_control": {"type": "string", "description": "a real argument"}
                    }}
                },
                "cache_control": {"type": "ephemeral", "ttl": "1h"}
            }],
            "messages": [{"role": "user", "content": "hi"}]
        })
        .to_string();
        let (out, had_markers) = strip_cache_control(body.as_bytes(), &TelemetryPolicy::default());
        assert!(had_markers, "the top-level tool marker is a marker");
        let v: Value = serde_json::from_slice(&out.expect("body rewritten")).unwrap();
        assert!(v["tools"][0].get("cache_control").is_none(), "tool marker stripped");
        assert!(
            v["tools"][0]["function"]["parameters"]["properties"]["cache_control"].is_object(),
            "legitimate schema field preserved"
        );
    }

    #[test]
    fn strip_schema_field_named_cache_control_alone_is_not_a_marker() {
        // A tool with NO top-level marker but a schema property named cache_control: nothing
        // to strip, no adoption signal, body left untouched.
        let body = serde_json::json!({
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {
                "type": "object", "properties": {"cache_control": {"type": "string"}}}}}],
            "messages": [{"role": "user", "content": "hi"}]
        })
        .to_string();
        let (out, had_markers) = strip_cache_control(body.as_bytes(), &TelemetryPolicy::default());
        assert!(!had_markers, "a schema field is not a marker");
        assert!(out.is_none(), "nothing stripped, body unchanged");
    }

    #[test]
    fn strip_mode_removes_telemetry_block_from_forwarded_body() {
        // strip_from_prompt=true: the unmarked telemetry block is dropped from the forwarded body,
        // and cache_control is still stripped from the surviving (marked) block.
        let body = serde_json::json!({
            "messages": [{"role": "system", "content": [
                {"type": "text", "text": "x-anthropic-billing-header: cch=abc;"},
                {"type": "text", "text": "real system", "cache_control": {"type": "ephemeral"}}
            ]}]
        })
        .to_string();
        let tele = TelemetryPolicy::from_config(true, &["x-anthropic-billing-header:".to_string()]);
        let (out, _) = strip_cache_control(body.as_bytes(), &tele);
        let v: Value = serde_json::from_slice(&out.expect("body rewritten")).unwrap();
        let content = v["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1, "telemetry block removed from the forwarded prompt");
        assert_eq!(content[0]["text"], "real system");
        assert!(content[0].get("cache_control").is_none(), "marker stripped from survivor");
    }

    #[test]
    fn strip_mode_leaves_non_system_blocks_untouched() {
        // strip_from_prompt=true, but the prefix appears in a USER block: telemetry handling is
        // scoped to the system role, so the forwarded prompt must be left untouched.
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "x-anthropic-billing-header: cch=abc;"},
                {"type": "text", "text": "actual question"}
            ]}]
        })
        .to_string();
        let tele = TelemetryPolicy::from_config(true, &["x-anthropic-billing-header:".to_string()]);
        let (out, _) = strip_cache_control(body.as_bytes(), &tele);
        assert!(out.is_none(), "non-system block is not stripped from the forwarded prompt");
    }

    #[test]
    fn ignore_mode_keeps_telemetry_block_in_forwarded_body() {
        // strip_from_prompt=false: the telemetry block stays in the forwarded body (only the cache
        // hash excludes it). No marker + no strip → body untouched.
        let body = serde_json::json!({
            "messages": [{"role": "system", "content": [
                {"type": "text", "text": "x-anthropic-billing-header: cch=abc;"},
                {"type": "text", "text": "real system"}
            ]}]
        })
        .to_string();
        let tele = TelemetryPolicy::from_config(false, &["x-anthropic-billing-header:".to_string()]);
        let (out, _) = strip_cache_control(body.as_bytes(), &tele);
        assert!(out.is_none(), "ignore mode leaves the forwarded body untouched");
    }

    #[test]
    fn inject_non_streaming_adds_cache_fields() {
        let body = serde_json::json!({"usage":{"prompt_tokens":2000,"completion_tokens":5}}).to_string();
        let out = inject_into_usage_json(body.as_bytes(), &stats()).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["usage"]["prompt_tokens"], 2000, "total preserved");
        assert_eq!(v["usage"]["prompt_tokens_details"]["cached_tokens"], 1024);
        assert_eq!(v["usage"]["cache_read_input_tokens"], 1024);
        assert_eq!(v["usage"]["cache_creation_input_tokens"], 60);
        assert_eq!(v["usage"]["cache_creation"]["ephemeral_1h_input_tokens"], 20);
    }

    #[test]
    fn inject_non_streaming_none_when_no_usage() {
        let body = serde_json::json!({"choices":[]}).to_string();
        assert!(inject_into_usage_json(body.as_bytes(), &stats()).is_none());
    }

    #[test]
    fn inject_sse_preserves_crlf_on_edited_frame() {
        // CRLF-framed stream: the rewritten usage frame must keep its trailing '\r' so the
        // '\r\n\r\n' event boundary stays intact (no lone '\n' amid CRLF).
        let sse = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":2000}}\r\n\r\ndata: [DONE]\r\n\r\n";
        let out = inject_into_sse_body(sse.as_bytes(), &stats()).unwrap();
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("\"cache_read_input_tokens\":1024"), "got: {s}");
        // The injected frame is still terminated by CRLF, not a bare LF.
        assert!(s.contains("}\r\n\r\n"), "edited frame must keep CRLF framing, got: {s}");
        assert!(!s.contains("}\n\r"), "must not produce a malformed \\n\\r, got: {s}");
    }

    #[test]
    fn inject_sse_edits_only_terminal_usage_frame() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":2000}}\n\ndata: [DONE]\n\n";
        let out = inject_into_sse_body(sse.as_bytes(), &stats()).unwrap();
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("\"cached_tokens\":1024"));
        assert!(s.contains("\"cache_read_input_tokens\":1024"));
        // exactly one injected frame; the delta + [DONE] are untouched.
        assert_eq!(s.matches("cached_tokens").count(), 1);
        assert!(s.contains("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}"));
        assert!(s.contains("data: [DONE]"));
    }

    #[test]
    fn inject_sse_none_when_no_usage_frame() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        assert!(inject_into_sse_body(sse.as_bytes(), &stats()).is_none());
    }

    #[test]
    fn inject_sse_handles_data_prefix_without_space() {
        // `data:{…}` (no space after the colon) is valid SSE and must still be injected.
        let sse = "data:{\"choices\":[],\"usage\":{\"prompt_tokens\":2000}}\n\ndata:[DONE]\n\n";
        let out = inject_into_sse_body(sse.as_bytes(), &stats()).expect("no-space data: frame is injected");
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("\"cache_read_input_tokens\":1024"), "got: {s}");
    }

    #[test]
    fn inject_into_sse_body_edits_the_usage_frame() {
        // The injection primitive: splice cache fields into the terminal usage frame, leaving the
        // deltas and `[DONE]` untouched. (The streaming orchestration — deferred classify resolve
        // + the commit gate — is exercised end-to-end in the layer tests.)
        let body = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":2000}}\n\ndata: [DONE]\n\n";
        let out = inject_into_sse_body(body, &stats()).expect("usage frame present → edited");
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("\"cached_tokens\":1024"), "got: {s}");
        assert!(s.contains("data: [DONE]"), "DONE preserved");
        assert!(s.contains("\"content\":\"hi\""), "delta preserved");
    }

    #[test]
    fn inject_into_sse_body_none_without_usage() {
        let body = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        assert!(inject_into_sse_body(body, &stats()).is_none(), "no usage frame → nothing to edit");
    }

    #[tokio::test]
    async fn inject_nonstreaming_error_body_vetoes_commit() {
        use axum::body::Body;
        // A 400 JSON error body has no usage object → no injection, no commit.
        let resp = Response::builder()
            .status(400)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::json!({"error":{"message":"bad request"}}).to_string()))
            .unwrap();
        let (_out, billing_ok) = inject_into_response_nonstreaming(resp, &stats()).await;
        assert!(!billing_ok, "error body → no commit");
    }

    #[tokio::test]
    async fn inject_nonstreaming_success_injects_and_allows_commit() {
        use axum::body::Body;
        let resp = Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::json!({"usage":{"prompt_tokens":2000}}).to_string()))
            .unwrap();
        let (out, billing_ok) = inject_into_response_nonstreaming(resp, &stats()).await;
        assert!(billing_ok, "2xx with usage → commit allowed");
        let collected = axum::body::to_bytes(out.into_body(), usize::MAX).await.unwrap();
        let s = std::str::from_utf8(&collected).unwrap();
        assert!(s.contains("\"cached_tokens\":1024"), "got: {s}");
    }
}
