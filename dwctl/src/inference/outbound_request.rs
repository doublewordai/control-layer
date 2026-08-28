//! Outbound request preparation, and the matching response-side reassembly.
//!
//! On the way to onwards this injects the streaming usage flags, so the upstream
//! reports token usage we can bill from:
//!
//! - for `/chat/completions` and legacy `/completions`, set
//!   `stream_options.include_usage = true` on streaming requests (so the provider
//!   emits a usage frame in the final SSE chunk), and force `stream: true` on
//!   batch traffic to a configured path.
//!
//! On the way back, the response to a request this forced is read as SSE and
//! reassembled into a single non-streaming body. Both halves of that
//! transformation therefore live here, and the daemon that dispatches the request
//! is not party to either: see [`should_force_stream`] for how its traffic is
//! recognised.
//!
//! This was dwctl's `stream_usage_transform`, previously wired through onwards'
//! `BodyTransformFn` hook. It deliberately does NOT scrub caller id fields: the
//! inference middleware already strips those in the single parse-and-shape it does
//! at the edge (`scrub_request_id_fields`, ported from onwards #240), so the body
//! that reaches this layer is already scrubbed - a second scrub here would be a
//! no-op duplicate.
//!
//! # Placement
//!
//! Innermost dwctl layer: inner to the cache layer (which must hash the original
//! body) and running last before onwards.
//!
//! That position is load-bearing for the response half rather than incidental.
//! Request logging is applied outer to this layer, so with the reassembly here it
//! sees one complete body; with the reassembly anywhere outer to request logging
//! it would see the event stream instead and retain every frame for the life of
//! the request. A streaming response carries far more bytes on the wire than the
//! body it assembles to, because each frame is a full JSON envelope around a few
//! bytes of token delta, so that difference is a large multiplier on per-request
//! memory rather than a small one.

use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde_json::Value;
use tracing::{debug, warn};

/// The three stream-shaped timeouts, mirroring the daemon's configuration.
///
/// Three rather than two: the idle check between events is what catches a stream
/// that opens and then stalls, and it has no equivalent in a single overall
/// timeout. Without it such a stream is only caught by `body`, which is sized for
/// a slow upstream and is far longer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamTimeouts {
    /// Max time to the first event.
    pub first_chunk: Duration,
    /// Max idle time between subsequent events.
    pub chunk: Duration,
    /// Max total time for the whole body.
    pub body: Duration,
}

/// What this layer needs to decide and perform the stream round trip.
#[derive(Clone)]
pub struct OutboundConfig {
    pub timeouts: StreamTimeouts,
}

/// `batch_metadata` key the daemon's dispatch sets to mark a request for
/// streaming and reassembly. The daemon forwards `batch_metadata` entries as
/// `x-fusillade-batch-<key>` headers, so it arrives here as
/// [`STREAM_MARKER_HEADER`].
pub const STREAM_MARKER_KEY: &str = "stream";

/// The header [`STREAM_MARKER_KEY`] arrives as.
pub const STREAM_MARKER_HEADER: &str = "x-fusillade-batch-stream";

impl StreamTimeouts {
    /// Read the budgets from the daemon's configuration.
    ///
    /// Its own, not a separate set: the daemon sizes these for its workload and
    /// they must not drift now that they are enforced here instead of there.
    pub fn from_daemon_config(daemon: &crate::config::DaemonConfig) -> Self {
        Self {
            first_chunk: Duration::from_millis(daemon.first_chunk_timeout_ms),
            chunk: Duration::from_millis(daemon.chunk_timeout_ms),
            body: Duration::from_millis(daemon.body_timeout_ms),
        }
    }
}

/// Whether the daemon asked for this response to be streamed and reassembled.
///
/// The daemon marks its own dispatches with this header. Nothing else at this
/// layer can tell daemon traffic from a client's own request:
/// `x-fusillade-request-id` is stamped by the edge on everything for
/// correlation, `batch_id` is absent for batchless flex as well as for realtime,
/// and the service tier never leaves the database. A client's request reaches
/// the edge directly and never passes through the daemon's dispatch processor,
/// which is what makes a mark set there the signal this needs.
///
/// Inferring it from the path instead collapses a streaming client's response
/// into a single body, because the path is identical on both planes.
///
/// Trusting a header is safe only because the sso-stack ingress strips every
/// `x-fusillade-*` header from external requests, which is the same perimeter
/// the `x-fusillade-request-id` early return in `inference::middleware` already
/// depends on - see the note on `strip_scheduling_priority`. A dwctl reachable
/// without traversing that proxy would let a caller set this. The blast radius
/// is narrow either way: a spoofed mark reassembles the caller's own response
/// and buffers their own body, under the same budgets as any other request.
fn should_force_stream(parts: &axum::http::request::Parts) -> bool {
    parts.headers.get(STREAM_MARKER_HEADER).and_then(|v| v.to_str().ok()) == Some("1")
}

pub async fn outbound_request_middleware(State(cfg): State<OutboundConfig>, request: Request, next: Next) -> Response {
    let (mut parts, body) = request.into_parts();

    // Two separate questions, and they must not share a condition. Whether the
    // body can be edited depends on its shape; whether the response is
    // reassembled depends only on the daemon's mark. Reassembly used to sit
    // behind the shape guard, which held only because Responses reaches this
    // layer already translated to the completions shape - masking the coupling
    // rather than removing it.
    let force_stream = should_force_stream(&parts);

    let path = parts.uri.path();
    // `/chat/completions` also ends with `/completions`; both take the stream flags.
    if !path.ends_with("/completions") {
        // Nothing to edit (e.g. /responses, /embeddings, /models), but a marked
        // dispatch still gets its response reassembled.
        let response = next.run(Request::from_parts(parts, body)).await;
        return if force_stream {
            reassemble_stream(response, cfg.timeouts).await
        } else {
            response
        };
    }

    // Outer layers (onwards body limit, cache) already bound the body, so buffering
    // with no extra limit here can't widen the exposure.
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "failed to read request body").into_response(),
    };

    let response = match transform(&bytes, force_stream) {
        Some(edited) => {
            // The body changed size, so the inbound Content-Length is now stale.
            // Drop it (as the Anthropic translator does) so it is recomputed
            // downstream - otherwise onwards forwards a wrong length to the upstream,
            // which can truncate or hang the read.
            parts.headers.remove(axum::http::header::CONTENT_LENGTH);
            next.run(Request::from_parts(parts, Body::from(edited))).await
        }
        // Unchanged: forwarding the original bytes, so the original Content-Length still matches.
        None => next.run(Request::from_parts(parts, Body::from(bytes))).await,
    };

    if force_stream {
        reassemble_stream(response, cfg.timeouts).await
    } else {
        response
    }
}

/// Inject the streaming usage flags into a JSON body, returning `Some(new_bytes)`
/// only when something changed. A body that is not a JSON object (or fails to
/// parse) is left untouched (`None`) - onwards still validates and rejects it.
fn transform(bytes: &Bytes, force_stream: bool) -> Option<Vec<u8>> {
    let mut value = serde_json::from_slice::<Value>(bytes).ok()?;
    let obj = value.as_object_mut()?;
    let mut changed = false;

    let request_streaming = obj.get("stream").and_then(Value::as_bool) == Some(true) || force_stream;
    if request_streaming {
        // Force stream:true when fusillade asked for it via header.
        if force_stream && obj.get("stream").and_then(Value::as_bool) != Some(true) {
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

/// Read an SSE response to completion and return the body it assembles to.
///
/// Ported from fusillade's streaming client, which used to do this after the
/// response had already passed request logging. The behaviours it preserves are
/// load-bearing and are called out individually below.
async fn reassemble_stream(response: Response, timeouts: StreamTimeouts) -> Response {
    use eventsource_stream::Eventsource;

    let (parts, body) = response.into_parts();
    let status = parts.status;

    let is_event_stream = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("text/event-stream"));

    // A streamable request can be rejected before streaming begins, and providers
    // commonly answer that with an ordinary JSON error body. Feeding one to an SSE
    // parser produces zero events and loses the diagnostic, so anything that is not
    // labelled as a stream passes through untouched.
    if !is_event_stream {
        return Response::from_parts(parts, body);
    }

    // Only successful streams are reassembled into a completion object. An error
    // status carrying an event stream keeps its raw data lines, because the
    // reassembler models a completion and would mangle anything else.
    let reassemble = status.as_u16() < 400;
    let mut stream = body.into_data_stream().eventsource();

    // Phase 1: the first event, under the time-to-first-token budget. Headers
    // alone do not count as progress here: an upstream can return them and then
    // queue, which is the case this budget exists to catch.
    let first_event = match tokio::time::timeout(timeouts.first_chunk, stream.next()).await {
        Err(_) => return timeout_response("first_chunk"),
        Ok(Some(Ok(event))) => Some(event),
        Ok(Some(Err(e))) => return sse_parse_error(&e.to_string()),
        Ok(None) => None,
    };

    // Phase 2: the remainder, under the idle budget per event and the total
    // budget across all of them.
    let outcome = tokio::time::timeout(timeouts.body, async {
        let mut sink = Sink::new(reassemble);
        if let Some(event) = first_event {
            sink.absorb(&event);
        }
        loop {
            match tokio::time::timeout(timeouts.chunk, stream.next()).await {
                Ok(Some(Ok(event))) => sink.absorb(&event),
                Ok(Some(Err(e))) => return Collected::ParseError(e.to_string()),
                Ok(None) => break,
                Err(_) => return Collected::Stalled(sink.seen),
            }
        }
        Collected::Done(sink)
    })
    .await;

    let sink = match outcome {
        Err(_) => return timeout_response("body"),
        Ok(Collected::Stalled(seen)) => {
            debug!(events_seen = seen, "fusillade stream stalled between events");
            return timeout_response("chunk");
        }
        Ok(Collected::ParseError(e)) => return sse_parse_error(&e),
        Ok(Collected::Done(sink)) => sink,
    };

    // Some providers answer 200 with an error envelope inside the stream. Surface
    // it as a real HTTP error so downstream retry classification sees it, rather
    // than as a successful but empty completion.
    if let Some(data) = &sink.embedded_error
        && let Ok(envelope) = serde_json::from_str::<EmbeddedErrorEnvelope>(data)
    {
        let code = envelope
            .error
            .code
            .as_ref()
            .and_then(|c| c.as_u64())
            .map(|c| c as u16)
            .filter(|c| (400..600).contains(c))
            .unwrap_or(500);

        warn!(
            embedded_status = code,
            "provider returned an error inside the SSE stream, reclassifying"
        );
        let status = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        return json_body_response(parts, status, data.clone());
    }

    match sink.finish() {
        Ok(body) => json_body_response(parts, status, body),
        Err(e) => {
            warn!(error = %e, "failed to reassemble SSE stream into a response body");
            sse_parse_error(&e.to_string())
        }
    }
}

/// How the collection phase ended.
enum Collected {
    Done(Sink),
    /// Idle for longer than the per-event budget, carrying the events seen so far
    /// for the diagnostic.
    Stalled(usize),
    ParseError(String),
}

/// Folds SSE events into one body as they arrive.
///
/// Incremental by design. A streaming response carries far more bytes on the wire
/// than the body it assembles to, and some upstreams add content-free keepalive
/// frames at a high rate for the life of the request, so buffering the frames and
/// processing them at the end would make memory scale with stream length rather
/// than with the result.
struct Sink {
    reassembler: openai_reassembler::Reassembler,
    /// Newline-joined raw event data, built only when not reassembling.
    raw: String,
    /// Data of the first event carrying a provider error envelope.
    embedded_error: Option<String>,
    /// Events seen, for the stall diagnostic.
    seen: usize,
    reassemble: bool,
}

impl Sink {
    fn new(reassemble: bool) -> Self {
        Self {
            reassembler: openai_reassembler::Reassembler::new(),
            raw: String::new(),
            embedded_error: None,
            seen: 0,
            reassemble,
        }
    }

    /// Fold one event. The caller may drop it as soon as this returns.
    fn absorb(&mut self, event: &eventsource_stream::Event) {
        self.seen += 1;

        // First error envelope wins, matching a scan from the start of the stream.
        if self.embedded_error.is_none() && event.data.starts_with("{\"error\"") {
            self.embedded_error = Some(event.data.clone());
        }

        if self.reassemble {
            self.reassembler.push(event);
        } else if !event.data.is_empty() && event.data != "[DONE]" {
            if !self.raw.is_empty() {
                self.raw.push('\n');
            }
            self.raw.push_str(&event.data);
        }
    }

    fn finish(self) -> anyhow::Result<String> {
        if self.reassemble { self.reassembler.finish() } else { Ok(self.raw) }
    }
}

/// A provider error envelope embedded in an otherwise successful stream.
#[derive(serde::Deserialize)]
struct EmbeddedErrorEnvelope {
    error: EmbeddedError,
}

#[derive(serde::Deserialize)]
struct EmbeddedError {
    #[serde(default)]
    code: Option<Value>,
}

/// Gateway timeout naming which budget expired.
///
/// A plain 504. The daemon already retries those, and retries all three of these
/// budgets identically, so nothing downstream needs to tell them apart and no
/// header or error type has to carry the distinction. Which one fired goes in the
/// body, which is what the failure record keeps.
fn timeout_response(which: &str) -> Response {
    (StatusCode::GATEWAY_TIMEOUT, format!("upstream stream exceeded its {which} budget")).into_response()
}

fn sse_parse_error(detail: &str) -> Response {
    warn!(error = %detail, "SSE parse error while reassembling a fusillade stream");
    (StatusCode::BAD_GATEWAY, "failed to read upstream stream").into_response()
}

/// Swap a reassembled body into the upstream response, keeping its headers.
///
/// Only the headers the swap invalidates are touched. Everything else the
/// upstream sent (rate limits, request ids, tracing headers) carries through,
/// because this sits on the batch hot path and is the last thing to see them
/// before request logging captures the response.
///
/// `Content-Length` and `Content-Encoding` go because the body is a different
/// size and is no longer encoded as the upstream encoded it; leaving either
/// would describe the old body. `Content-Type` becomes JSON because the response
/// is no longer a stream.
fn json_body_response(mut parts: axum::http::response::Parts, status: StatusCode, body: String) -> Response {
    parts.status = status;
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::CONTENT_ENCODING);
    parts
        .headers
        .insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/json"));
    Response::from_parts(parts, Body::from(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;
    use std::convert::Infallible;
    use std::time::Duration;

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
    fn timeouts(first_chunk_ms: u64, chunk_ms: u64, body_ms: u64) -> StreamTimeouts {
        StreamTimeouts {
            first_chunk: Duration::from_millis(first_chunk_ms),
            chunk: Duration::from_millis(chunk_ms),
            body: Duration::from_millis(body_ms),
        }
    }

    /// An SSE response built from ready frames, with no delay between them.
    fn sse_response(status: StatusCode, frames: &[&str]) -> Response {
        let body: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
        let mut resp = Response::new(Body::from(body));
        *resp.status_mut() = status;
        resp.headers_mut()
            .insert(header::CONTENT_TYPE, header::HeaderValue::from_static("text/event-stream"));
        resp
    }

    /// An SSE response whose frames arrive on a schedule, for the timeout tests.
    fn slow_sse_response(frames: Vec<(u64, String)>) -> Response {
        let stream = async_stream::stream! {
            for (delay_ms, frame) in frames {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                yield Ok::<Bytes, Infallible>(Bytes::from(format!("data: {frame}\n\n")));
            }
        };
        let mut resp = Response::new(Body::from_stream(stream));
        resp.headers_mut()
            .insert(header::CONTENT_TYPE, header::HeaderValue::from_static("text/event-stream"));
        resp
    }

    async fn body_string(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn chunk(content: &str) -> String {
        format!(
            r#"{{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{{"index":0,"delta":{{"content":"{content}"}},"finish_reason":null}}]}}"#
        )
    }

    /// The core of the move: SSE frames in, one completion object out.
    #[tokio::test]
    async fn reassembles_sse_into_a_single_json_body() {
        let frames = [chunk("Hello"), chunk(" world"), "[DONE]".to_string()];
        let refs: Vec<&str> = frames.iter().map(String::as_str).collect();

        let out = reassemble_stream(sse_response(StatusCode::OK, &refs), timeouts(1000, 1000, 5000)).await;

        assert_eq!(out.status(), StatusCode::OK);
        assert_eq!(
            out.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json",
            "the caller asked for a completion, not a stream"
        );
        let body: serde_json::Value = serde_json::from_str(&body_string(out).await).unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "Hello world");
    }

    /// Content-free keepalive frames are what make a stream's wire size unrelated
    /// to the body it assembles to. They must not reach the assembled body.
    #[tokio::test]
    async fn keepalive_frames_do_not_change_the_assembled_body() {
        let keepalive = r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":null}]}"#;

        let mut noisy = vec![chunk("Hello")];
        noisy.extend(std::iter::repeat_n(keepalive.to_string(), 500));
        noisy.push(chunk(" world"));
        let noisy_refs: Vec<&str> = noisy.iter().map(String::as_str).collect();
        let clean = [chunk("Hello"), chunk(" world")];
        let clean_refs: Vec<&str> = clean.iter().map(String::as_str).collect();

        let a = reassemble_stream(sse_response(StatusCode::OK, &noisy_refs), timeouts(1000, 1000, 5000)).await;
        let b = reassemble_stream(sse_response(StatusCode::OK, &clean_refs), timeouts(1000, 1000, 5000)).await;

        assert_eq!(body_string(a).await, body_string(b).await, "keepalives changed the assembled body");
    }

    /// A provider can answer 200 and then put the error in the stream. That has to
    /// surface as a real HTTP error or retry classification never sees it.
    #[tokio::test]
    async fn embedded_error_is_reclassified_to_a_real_status() {
        let frames = [r#"{"error":{"code":429,"message":"slow down"}}"#];

        let out = reassemble_stream(sse_response(StatusCode::OK, &frames), timeouts(1000, 1000, 5000)).await;

        assert_eq!(out.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(body_string(out).await.contains("slow down"));
    }

    /// The first error frame wins, matching a scan from the start of the stream.
    #[tokio::test]
    async fn the_first_embedded_error_wins() {
        let frames = [
            r#"{"error":{"code":429,"message":"first"}}"#,
            r#"{"error":{"code":500,"message":"second"}}"#,
        ];

        let out = reassemble_stream(sse_response(StatusCode::OK, &frames), timeouts(1000, 1000, 5000)).await;

        assert_eq!(out.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = body_string(out).await;
        assert!(body.contains("first"), "got: {body}");
        assert!(!body.contains("second"));
    }

    /// An error status carrying an event stream keeps its raw data lines: the
    /// reassembler models a completion and would mangle anything else.
    #[tokio::test]
    async fn error_status_keeps_raw_event_data() {
        let frames = [r#"{"detail":"bad request"}"#, "", "[DONE]"];

        let out = reassemble_stream(sse_response(StatusCode::BAD_REQUEST, &frames), timeouts(1000, 1000, 5000)).await;

        assert_eq!(out.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_string(out).await,
            r#"{"detail":"bad request"}"#,
            "empty and [DONE] frames are dropped, the rest is joined verbatim"
        );
    }

    /// A streamable request can be rejected before streaming begins, and the
    /// answer is then an ordinary JSON body. Feeding that to an SSE parser would
    /// yield zero events and lose the diagnostic.
    #[tokio::test]
    async fn non_sse_response_passes_through_untouched() {
        let mut resp = Response::new(Body::from(r#"{"error":"no such model"}"#));
        *resp.status_mut() = StatusCode::NOT_FOUND;
        resp.headers_mut()
            .insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/json"));

        let out = reassemble_stream(resp, timeouts(1000, 1000, 5000)).await;

        assert_eq!(out.status(), StatusCode::NOT_FOUND);
        assert_eq!(body_string(out).await, r#"{"error":"no such model"}"#);
    }

    /// Nothing at all within the time-to-first-token budget.
    #[tokio::test]
    async fn no_first_event_reports_a_first_chunk_timeout() {
        let response = slow_sse_response(vec![(400, chunk("late"))]);

        let out = reassemble_stream(response, timeouts(50, 5000, 5000)).await;

        assert_eq!(out.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(body_string(out).await.contains("first_chunk"));
    }

    /// The stream opens and then stalls. This is the case an overall timeout
    /// cannot express, and the reason there are three budgets rather than two.
    #[tokio::test]
    async fn a_stalled_stream_reports_a_chunk_timeout() {
        let response = slow_sse_response(vec![(0, chunk("hi")), (400, chunk(" there"))]);

        let out = reassemble_stream(response, timeouts(1000, 50, 5000)).await;

        assert_eq!(out.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(
            body_string(out).await.contains("chunk"),
            "a stream that opens and then goes idle is a chunk timeout, not a body one"
        );
    }

    /// Steady progress, but too much of it in total.
    #[tokio::test]
    async fn a_long_stream_reports_a_body_timeout() {
        let frames: Vec<(u64, String)> = (0..20).map(|_| (30, chunk("x"))).collect();

        let out = reassemble_stream(slow_sse_response(frames), timeouts(1000, 1000, 120)).await;

        assert_eq!(out.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(
            body_string(out).await.contains("body"),
            "no single gap was long enough, so this must be the total budget"
        );
    }

    /// Drive a real request through a real nest and report the content type the
    /// caller receives. The upstream always streams; whether the caller sees a
    /// stream or an assembled body is entirely this layer's decision.
    ///
    /// Hand-built `Parts` cannot answer this. They were what let a trigger that
    /// matched every request through the edge pass as correct.
    async fn content_type_through_the_edge(path: &str, headers: &[(&str, &str)]) -> String {
        use axum::{Router, routing::post};

        let upstream = || async {
            let frames = "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
            ([(header::CONTENT_TYPE, "text/event-stream")], frames)
        };

        let inner = Router::new()
            .route("/chat/completions", post(upstream))
            .route("/responses", post(upstream))
            .layer(axum::middleware::from_fn_with_state(config(), outbound_request_middleware));
        let app = Router::new().nest("/ai/v1", inner);

        let server = axum_test::TestServer::new(app).unwrap();
        let mut req = server.post(path).json(&serde_json::json!({"model": "m", "messages": []}));
        for (k, v) in headers {
            req = req.add_header(*k, *v);
        }
        let response = req.await;
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .map_or_else(String::new, |v| v.to_str().unwrap_or_default().to_string())
    }

    fn config() -> OutboundConfig {
        OutboundConfig {
            timeouts: timeouts(1000, 1000, 5000),
        }
    }

    /// The budgets have to come from the daemon's own configuration, and the
    /// three are the same type in the same order, which is exactly the shape that
    /// transposes silently. Every other test here supplies them directly, so
    /// nothing else would notice two of them swapped.
    #[test]
    fn the_budgets_are_read_from_the_daemons_own_configuration() {
        let mut daemon = crate::config::DaemonConfig::default();
        daemon.first_chunk_timeout_ms = 1;
        daemon.chunk_timeout_ms = 2;
        daemon.body_timeout_ms = 3;

        assert_eq!(
            StreamTimeouts::from_daemon_config(&daemon),
            StreamTimeouts {
                first_chunk: Duration::from_millis(1),
                chunk: Duration::from_millis(2),
                body: Duration::from_millis(3),
            }
        );
    }

    /// The daemon marks its own dispatches, and those get reassembled.
    #[tokio::test]
    async fn daemon_traffic_is_reassembled() {
        let ct = content_type_through_the_edge(
            "/ai/v1/chat/completions",
            &[
                (STREAM_MARKER_HEADER, "1"),
                ("x-fusillade-request-id", "00000000-0000-0000-0000-000000000000"),
            ],
        )
        .await;
        assert!(ct.starts_with("application/json"), "expected an assembled body, got {ct:?}");
    }

    /// A streaming client must keep receiving a stream.
    ///
    /// The correlation header is present, because the edge stamps it on every
    /// inbound request. That is exactly why it cannot be the signal: keying off
    /// it collapsed every streaming client's response into a single body.
    #[tokio::test]
    async fn client_traffic_still_streams() {
        let ct = content_type_through_the_edge(
            "/ai/v1/chat/completions",
            &[("x-fusillade-request-id", "00000000-0000-0000-0000-000000000000")],
        )
        .await;
        assert!(
            ct.starts_with("text/event-stream"),
            "a streaming client must not have its stream reassembled, got {ct:?}"
        );
    }

    /// Nothing at all from the daemon: also a client, also a stream.
    #[tokio::test]
    async fn unmarked_traffic_still_streams() {
        let ct = content_type_through_the_edge("/ai/v1/chat/completions", &[]).await;
        assert!(ct.starts_with("text/event-stream"), "got {ct:?}");
    }

    /// Responses reaches this layer already translated to the completions shape,
    /// so the mark is what carries the decision, not the path.
    #[tokio::test]
    async fn marked_responses_traffic_is_reassembled() {
        let ct = content_type_through_the_edge("/ai/v1/responses", &[(STREAM_MARKER_HEADER, "1")]).await;
        assert!(ct.starts_with("application/json"), "expected an assembled body, got {ct:?}");
    }
}
