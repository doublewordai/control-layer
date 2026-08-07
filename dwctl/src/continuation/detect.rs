//! Death-signature classification: the mid-stream death taxonomy as code.
//!
//! Everything the tee loop observes about a stream — a frame, an EOF, a
//! transport error, a stall, the client hanging up — is fed to [`classify`] as a
//! [`DeathEvent`], which returns the [`Verdict`] the loop acts on. Keeping the
//! table in one pure function is deliberate: the death-signature taxonomy
//! workstream (mining a month of prod deaths) issues rows in exactly this shape,
//! and each new row is a match arm plus a test, never a change to the tee loop.
//!
//! The two rules worth stating twice, because getting them backwards is a
//! customer-visible bug:
//!
//! - **499 is two populations.** `type: "request_cancelled"` is a dynamo
//!   worker-leg cancellation carried over NATS while the client connection is
//!   perfectly healthy → resume. `type: "client_disconnected"` is our own
//!   fabrication for a client that hung up → nobody is listening, never resume.
//!   Never classify by status code alone.
//! - **A 4xx error envelope inside a 200 stream is not resumable.** The input
//!   was rejected; re-sending a longer version of the same prompt cannot help,
//!   and the client should see the error exactly as it does today.

use serde_json::Value;

/// Something the tee loop observed on the current leg.
#[derive(Debug)]
pub enum DeathEvent<'a> {
    /// The inner body stream yielded `Err` — hyper reset, incomplete body, a
    /// truncated chunked encoding.
    TransportError,
    /// The inner body stream ended (`None`). `saw_finish_reason` is whether every
    /// choice seen so far carried a `finish_reason`; `saw_done` whether a
    /// `[DONE]` sentinel arrived.
    Eof { saw_finish_reason: bool, saw_done: bool },
    /// A complete, parsed SSE data frame. Ordinary content frames classify as
    /// [`Verdict::Alive`]; error envelopes carry the interesting rows.
    Frame(&'a Value),
    /// No frames for the resume deadline after the first byte.
    Stall,
    /// The client went away (our forward-write failed, or the body was dropped).
    ClientDisconnect,
}

/// What the tee loop should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to act on — forward the frame and keep going.
    Alive,
    /// The stream completed normally; disarm and pass through.
    Complete,
    /// The generation finished (a `finish_reason` was seen for every choice) but
    /// the trailer never arrived. Nothing to resume — synthesize the missing
    /// usage frame + `[DONE]` and finish. Death families `no_done` / `no_usage`.
    LostTrailer,
    /// Resume: run the chain loop. The label is the death family, for
    /// `dwctl_continuation_outcome_total{reason}`.
    Resume(&'static str),
    /// A death we deliberately do not resume; surface it exactly as today. The
    /// label is the reason.
    NoResume(&'static str),
}

/// Classify one observation. Pure — this is the taxonomy table.
pub fn classify(event: &DeathEvent) -> Verdict {
    match event {
        // A mid-stream transport failure says nothing about the request's
        // validity: the generation was in flight and the bytes stopped.
        DeathEvent::TransportError => Verdict::Resume("transport_error"),

        // A clean EOF is only clean if the model actually said it was done.
        DeathEvent::Eof { saw_done: true, .. } => Verdict::Complete,
        DeathEvent::Eof {
            saw_finish_reason: true,
            saw_done: false,
        } => Verdict::LostTrailer,
        DeathEvent::Eof { .. } => Verdict::Resume("truncated"),

        DeathEvent::Frame(frame) => classify_frame(frame),

        // Treated as a death at the last received frame boundary: the partial
        // generation is intact, so the resume prefix is exact.
        DeathEvent::Stall => Verdict::Resume("stall"),

        // Nobody is listening. Disarm, drop the accumulator, never dispatch a
        // resume leg that would generate tokens into a closed socket.
        DeathEvent::ClientDisconnect => Verdict::NoResume("client_disconnect"),
    }
}

/// Classify a parsed SSE data frame. Non-error frames are [`Verdict::Alive`].
fn classify_frame(frame: &Value) -> Verdict {
    let Some(error) = error_object(frame) else {
        return Verdict::Alive;
    };

    let error_type = error.get("type").and_then(Value::as_str).unwrap_or("");
    // Type first, code second — 499 is two populations and only the body tells
    // them apart.
    match error_type {
        "request_cancelled" => return Verdict::Resume("cancelled_499"),
        "client_disconnected" => return Verdict::NoResume("client_disconnect"),
        _ => {}
    }

    match error_code(error) {
        // A rejected input stays rejected; a longer prompt cannot fix it.
        Some(code) if (400..500).contains(&code) => Verdict::NoResume("client_error"),
        // 5xx envelopes and shapeless errors (no numeric code) are upstream
        // faults inside a 200 stream — the OpenRouter family. Resume.
        _ => Verdict::Resume("error_envelope"),
    }
}

/// The error object of an error-envelope frame, if this frame is one. Handles
/// both the nested `{"error": {...}}` shape (OpenAI, OpenRouter, dynamo) and a
/// bare `{"object": "error", "message": ...}` shape.
fn error_object(frame: &Value) -> Option<&serde_json::Map<String, Value>> {
    if let Some(err) = frame.get("error").and_then(Value::as_object) {
        return Some(err);
    }
    if frame.get("object").and_then(Value::as_str) == Some("error") {
        return frame.as_object();
    }
    None
}

/// `error.code` as a number. Providers send it as an integer or as a numeric
/// string; a non-numeric code (`"invalid_request_error"`) yields `None`.
fn error_code(error: &serde_json::Map<String, Value>) -> Option<i64> {
    match error.get("code") {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// One case per row of the §4 taxonomy table.
    #[test]
    fn death_table() {
        let cancelled = json!({"error": {"code": 499, "message": "CancelledError: ", "type": "request_cancelled"}});
        let disconnected = json!({"error": {"code": 499, "message": "client disconnected", "type": "client_disconnected"}});
        let bad_request = json!({"error": {"code": 400, "message": "input too long", "type": "invalid_request_error"}});
        let upstream = json!({"error": {"code": 502, "message": "Provider returned error", "metadata": {"provider_name": "x"}}});
        let content = json!({"choices": [{"delta": {"content": "hi"}}]});

        let cases: Vec<(&str, DeathEvent, Verdict)> = vec![
            (
                "transport error mid-stream",
                DeathEvent::TransportError,
                Verdict::Resume("transport_error"),
            ),
            (
                "stream ends with no finish_reason and no [DONE]",
                DeathEvent::Eof {
                    saw_finish_reason: false,
                    saw_done: false,
                },
                Verdict::Resume("truncated"),
            ),
            (
                "error envelope inside a 200 stream",
                DeathEvent::Frame(&upstream),
                Verdict::Resume("error_envelope"),
            ),
            (
                "dynamo 499 request_cancelled",
                DeathEvent::Frame(&cancelled),
                Verdict::Resume("cancelled_499"),
            ),
            (
                "4xx-shaped envelope",
                DeathEvent::Frame(&bad_request),
                Verdict::NoResume("client_error"),
            ),
            (
                "499 client_disconnected",
                DeathEvent::Frame(&disconnected),
                Verdict::NoResume("client_disconnect"),
            ),
            (
                "client disconnected",
                DeathEvent::ClientDisconnect,
                Verdict::NoResume("client_disconnect"),
            ),
            ("stall past the deadline", DeathEvent::Stall, Verdict::Resume("stall")),
            ("ordinary content frame", DeathEvent::Frame(&content), Verdict::Alive),
            (
                "finished, then [DONE]",
                DeathEvent::Eof {
                    saw_finish_reason: true,
                    saw_done: true,
                },
                Verdict::Complete,
            ),
            (
                "finished but no [DONE] (lost trailer)",
                DeathEvent::Eof {
                    saw_finish_reason: true,
                    saw_done: false,
                },
                Verdict::LostTrailer,
            ),
        ];

        for (name, event, expected) in cases {
            assert_eq!(classify(&event), expected, "case: {name}");
        }
    }

    #[test]
    fn a_499_is_never_classified_by_its_code_alone() {
        // Same status, opposite verdicts — the body's `type` is the whole signal.
        let worker = json!({"error": {"code": 499, "type": "request_cancelled"}});
        let client = json!({"error": {"code": 499, "type": "client_disconnected"}});
        assert_eq!(classify(&DeathEvent::Frame(&worker)), Verdict::Resume("cancelled_499"));
        assert_eq!(classify(&DeathEvent::Frame(&client)), Verdict::NoResume("client_disconnect"));
        // A 499 with neither marker falls into the 4xx band: not resumable.
        let bare = json!({"error": {"code": 499}});
        assert_eq!(classify(&DeathEvent::Frame(&bare)), Verdict::NoResume("client_error"));
    }

    #[test]
    fn numeric_string_codes_are_read_as_numbers() {
        let as_string = json!({"error": {"code": "400", "message": "bad"}});
        assert_eq!(classify(&DeathEvent::Frame(&as_string)), Verdict::NoResume("client_error"));
    }

    #[test]
    fn shapeless_and_bare_error_envelopes_resume() {
        // No code at all (a plain OpenRouter mid-stream envelope).
        let shapeless = json!({"error": {"message": "upstream exploded"}});
        assert_eq!(classify(&DeathEvent::Frame(&shapeless)), Verdict::Resume("error_envelope"));
        // Non-numeric code — treated as an upstream fault, not a 4xx.
        let typed = json!({"error": {"code": "server_error", "message": "boom"}});
        assert_eq!(classify(&DeathEvent::Frame(&typed)), Verdict::Resume("error_envelope"));
        // The bare `object: "error"` shape.
        let bare = json!({"object": "error", "message": "boom", "code": 503});
        assert_eq!(classify(&DeathEvent::Frame(&bare)), Verdict::Resume("error_envelope"));
    }

    #[test]
    fn a_content_frame_that_merely_mentions_error_is_not_an_envelope() {
        // `error` must be an OBJECT; a model writing the word "error" in its
        // output, or a null field, must not kill the stream.
        let text = json!({"choices": [{"delta": {"content": "error: not really"}}], "error": null});
        assert_eq!(classify(&DeathEvent::Frame(&text)), Verdict::Alive);
    }
}
