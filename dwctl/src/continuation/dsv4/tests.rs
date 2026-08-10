//! Replay of the fidelity harness's captured streams, plus a unit test for each
//! of the three production rules the harness derived from measured failures.
//!
//! The fixtures are the harness's own captures, trimmed to the fields these tests
//! need (`source` names the file each came from). Three provider shapes are
//! represented, because the report's headline mutation is that the same model
//! alias yields structurally different delta streams:
//!
//! - `frag-*` — the OpenRouter provider that FRAGMENTS tool arguments
//!   (`{`, `"city": "Paris"`, `, "unit": "c`, `elsius"}`), repeats `role` on
//!   every frame, sends `content: ""` alongside reasoning, adds
//!   `reasoning_details`, and swallows the `\n\n` before the tool block. This is
//!   the only place the intra-tool-call partial states exist, and it is the
//!   stream behind the report's 23/23 end-to-end result;
//! - `plat-*` — the other OpenRouter provider: whole-object arguments in one
//!   frame, `\n\n` surfaced as content, no reasoning channel at all;
//! - `block-*` — direct dynamo captures, which carry `gt_block`: the DSML block
//!   as read from `/v1/completions` with a token-id prompt, i.e. raw model
//!   output that never passed through the chat parser. Those four are the
//!   byte-exactness anchor.
//!
//! `harness.cut_lens[k]` is the byte length of the text the validated Python
//! reconstructor produced after ingesting `k + 1` delta frames (`null` = nothing
//! reconstructable yet). Storing lengths rather than strings is not a shortcut:
//! it only works because every cut is a prefix of the final text, so the goldens
//! and the monotonicity property are the same statement.

use serde_json::json;

use super::*;

const CAP: usize = 1024 * 1024;

#[derive(serde::Deserialize)]
struct Fixture {
    name: String,
    /// Path of the harness capture this was trimmed from.
    #[allow(dead_code)]
    source: String,
    /// The ground-truth DSML block from a raw `/v1/completions` read, when the
    /// capture had one.
    gt_block: Option<String>,
    harness: Golden,
    frames: Vec<Value>,
}

#[derive(serde::Deserialize)]
struct Golden {
    #[serde(rename = "final")]
    final_text: String,
    cut_lens: Vec<Option<usize>>,
}

macro_rules! fixtures {
    ($($file:literal),* $(,)?) => {
        vec![$(
            serde_json::from_str::<Fixture>(include_str!(concat!("../test_fixtures/", $file)))
                .expect(concat!("fixture ", $file, " parses")),
        )*]
    };
}

fn fixtures() -> Vec<Fixture> {
    fixtures![
        "dsv4-frag-tool-parallel.json",
        "dsv4-frag-tool-single.json",
        "dsv4-plat-reasoning.json",
        "dsv4-plat-tool-single.json",
        "dsv4-plat-tool-parallel.json",
        "dsv4-plat-structured-args.json",
        "dsv4-block-scalars-and-array.json",
        "dsv4-block-nested-object.json",
        "dsv4-block-escaping.json",
        "dsv4-block-parallel.json",
    ]
}

/// Frames that carry a delta. A cut point is a count of these — usage-only
/// trailers are not a state the generation was ever in.
fn delta_frames(fixture: &Fixture) -> Vec<&Value> {
    fixture
        .frames
        .iter()
        .filter(|f| f.get("choices").and_then(Value::as_array).is_some_and(|c| !c.is_empty()))
        .collect()
}

/// The reconstruction after the first `upto` delta frames.
fn replay(fixture: &Fixture, upto: usize) -> Option<String> {
    let mut acc = Dsv4Reconstructor::new(CAP);
    for frame in delta_frames(fixture).into_iter().take(upto) {
        acc.ingest(frame).expect("fixture streams never disarm");
    }
    acc.continuation_text()
}

// ── the harness replay ───────────────────────────────────────────────────────

/// The port reproduces the validated Python reconstructor at every cut point of
/// every captured stream. This is the test that says "same reconstructor", and
/// everything below it says "for these reasons".
#[test]
fn every_cut_of_every_captured_stream_matches_the_validated_harness() {
    let mut cuts = 0;
    for fixture in fixtures() {
        let n = delta_frames(&fixture).len();
        assert_eq!(n, fixture.harness.cut_lens.len(), "{}: cut count", fixture.name);
        for (k, expected_len) in fixture.harness.cut_lens.iter().enumerate() {
            let got = replay(&fixture, k + 1);
            let want = expected_len.map(|len| fixture.harness.final_text[..len].to_string());
            assert_eq!(got, want, "{}: cut {}", fixture.name, k + 1);
            cuts += 1;
        }
        assert_eq!(
            replay(&fixture, n).as_deref(),
            Some(fixture.harness.final_text.as_str()),
            "{}: final",
            fixture.name
        );
    }
    assert_eq!(cuts, 247, "every captured cut point is exercised");
}

/// Ground truth: the DSML block we rebuild from parsed deltas is byte-identical
/// to what the model actually emitted, read back from `/v1/completions` with a
/// token-id prompt so no chat parser was involved. Covers scalars + array,
/// nested object, embedded quotes/newline/non-ASCII, and parallel calls.
#[test]
fn the_rebuilt_tool_block_is_byte_identical_to_raw_model_output() {
    let mut checked = 0;
    for fixture in fixtures() {
        let Some(gt_block) = &fixture.gt_block else {
            continue;
        };
        let text = replay(&fixture, delta_frames(&fixture).len()).expect("a tool turn reconstructs");
        let start = text.find(TOOL_CALLS_OPEN).expect("the block is present");
        assert_eq!(&text[start..], gt_block, "{}", fixture.name);
        checked += 1;
    }
    assert_eq!(checked, 4, "all four ground-truth shapes are covered");
}

// ── rule 2: monotonicity ─────────────────────────────────────────────────────

/// The property the speculative-close bug violated: a middleware that has already
/// streamed bytes to the client can never un-send them, so the reconstruction at
/// cut k must be a byte-prefix of the one at cut k+1.
#[test]
fn reconstruction_is_monotonic_across_every_fixture_stream() {
    for fixture in fixtures() {
        let n = delta_frames(&fixture).len();
        let mut previous = String::new();
        for k in 1..=n {
            let current = replay(&fixture, k).unwrap_or_default();
            assert!(
                current.starts_with(&previous),
                "{}: cut {k} is not an extension of cut {}\n  prev tail: {:?}\n  curr tail: {:?}",
                fixture.name,
                k - 1,
                previous.chars().rev().take(60).collect::<String>(),
                current.chars().rev().take(60).collect::<String>(),
            );
            previous = current;
        }
    }
}

fn tool_frame(index: i64, name: Option<&str>, arguments: &str, finish: bool) -> Value {
    let mut function = serde_json::Map::new();
    if let Some(name) = name {
        function.insert("name".into(), json!(name));
    }
    function.insert("arguments".into(), json!(arguments));
    json!({
        "id": "chatcmpl-1", "model": "dsv4", "created": 1,
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{"index": index, "function": function}]},
            "finish_reason": if finish { json!("tool_calls") } else { Value::Null },
        }]
    })
}

/// A completed invoke does NOT mean the tool block ended: the model may open a
/// sibling call. Closing `</｜DSML｜tool_calls>` speculatively drops that sibling
/// AND lands the prefix on a token that makes the model emit EOS at once.
#[test]
fn a_completed_invoke_never_closes_the_tool_block() {
    let mut acc = Dsv4Reconstructor::new(CAP);
    acc.ingest(&tool_frame(0, Some("get_weather"), r#"{"city": "Paris"}"#, false))
        .unwrap();

    let after_first = acc.continuation_text().expect("one complete call reconstructs");
    assert!(after_first.contains(INVOKE_CLOSE), "the completed invoke itself is closed");
    assert!(
        !after_first.contains(TOOL_CALLS_CLOSE),
        "the block stays open — a second call may still be coming"
    );

    // And it was: the earlier prefix survives untouched.
    acc.ingest(&tool_frame(1, Some("get_weather"), r#"{"city": "London"}"#, false))
        .unwrap();
    let after_second = acc.continuation_text().unwrap();
    assert!(after_second.starts_with(&after_first), "the second call extends, never rewrites");
    assert!(after_second.contains("London"));
    assert!(!after_second.contains(TOOL_CALLS_CLOSE));

    // Only a terminal frame closes it.
    acc.ingest(&json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}))
        .unwrap();
    let terminal = acc.continuation_text().unwrap();
    assert!(terminal.starts_with(&after_second));
    assert!(terminal.ends_with(TOOL_CALLS_CLOSE));
}

// ── rule 1: the seam newline ─────────────────────────────────────────────────

/// A prefix ending exactly at `</｜DSML｜parameter>` or `</｜DSML｜invoke>` returns
/// one completion token and empty text — the model reads those tokens as the end
/// of the generation. The newline that always follows them in well-formed DSML
/// restores normal continuation.
#[test]
fn a_prefix_ending_on_a_closing_tag_gains_its_implied_newline() {
    let mut acc = Dsv4Reconstructor::new(CAP);
    acc.ingest(&tool_frame(0, Some("get_weather"), r#"{"city": "Paris"}"#, false))
        .unwrap();
    let text = acc.continuation_text().unwrap();

    assert!(text.ends_with(&format!("{INVOKE_CLOSE}\n")));
    // The raw reconstruction is what would have been sent without the rule.
    assert!(acc.reconstruct().ends_with(INVOKE_CLOSE));
}

/// The same rule stated as an invariant over every real stream: no cut point of
/// any capture may hand the resume leg a prefix ending on a closing tag.
#[test]
fn no_cut_of_any_fixture_stream_ends_on_a_closing_tag() {
    for fixture in fixtures() {
        for k in 1..=delta_frames(&fixture).len() {
            let Some(text) = replay(&fixture, k) else {
                continue;
            };
            for tag in CLOSING_TAGS {
                assert!(!text.ends_with(tag), "{}: cut {k} ends on {tag}", fixture.name);
            }
        }
    }
}

// ── rule 3: the conditional separator ────────────────────────────────────────

/// One provider surfaces the `\n\n` before the tool block as a content delta and
/// another swallows it. Injecting unconditionally double-counts it; never
/// injecting loses it. Injecting only what is missing makes both converge.
#[test]
fn the_tool_block_separator_is_injected_only_when_the_content_lacks_it() {
    let content = |text: &str| json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"content": text}}]});

    let mut swallowed = Dsv4Reconstructor::new(CAP);
    swallowed.ingest(&content("Checking.")).unwrap();
    swallowed.ingest(&tool_frame(0, Some("f"), "{", false)).unwrap();

    let mut surfaced = Dsv4Reconstructor::new(CAP);
    surfaced.ingest(&content("Checking.")).unwrap();
    surfaced.ingest(&content("\n\n")).unwrap();
    surfaced.ingest(&tool_frame(0, Some("f"), "{", false)).unwrap();

    assert_eq!(swallowed.continuation_text(), surfaced.continuation_text());
    assert_eq!(
        swallowed.continuation_text().unwrap(),
        format!("</think>Checking.\n\n{TOOL_CALLS_OPEN}\n{INVOKE_OPEN}f\">\n")
    );
}

// ── the reasoning channel ────────────────────────────────────────────────────

#[test]
fn reasoning_is_closed_with_a_think_tag_only_once_the_body_starts() {
    let reasoning = |text: &str| {
        json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {
            // The fragmenting provider sends `content: ""` on every reasoning
            // frame; that must not read as the body having started.
            "content": "", "reasoning_content": text,
            "reasoning_details": [{"format": "unknown", "index": 0, "text": text, "type": "reasoning.text"}],
        }}]})
    };

    let mut acc = Dsv4Reconstructor::new(CAP);
    acc.ingest(&reasoning("Let me")).unwrap();
    acc.ingest(&reasoning(" think.")).unwrap();
    assert_eq!(
        acc.continuation_text().as_deref(),
        Some("Let me think."),
        "still inside the think block: closing it would fabricate an end to the reasoning"
    );

    acc.ingest(&json!({"choices": [{"index": 0, "delta": {"content": "Answer"}}]}))
        .unwrap();
    assert_eq!(acc.continuation_text().as_deref(), Some("Let me think.</think>Answer"));
}

/// A thinking-mode turn that does no thinking still emits `</think>` first, so
/// the tag cannot be conditioned on having seen `reasoning_content` — it is the
/// template mode that decides. The `plat-reasoning` fixture is exactly this
/// shape, which is why the mode is a field rather than an inference.
#[test]
fn chat_mode_omits_the_think_tag_that_thinking_mode_emits() {
    let body = json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"content": "Answer"}}]});

    let mut thinking = Dsv4Reconstructor::new(CAP);
    thinking.ingest(&body).unwrap();
    assert_eq!(thinking.continuation_text().as_deref(), Some("</think>Answer"));

    let mut chat = Dsv4Reconstructor::chat_mode(CAP);
    chat.ingest(&body).unwrap();
    assert_eq!(chat.continuation_text().as_deref(), Some("Answer"));
}

// ── partial tool-call states ─────────────────────────────────────────────────

/// The states a death can land in inside a tool call, which is the whole reason
/// the fragmenting provider matters. Each fragment is fed cumulatively, exactly
/// as the provider delivers it.
#[test]
fn every_partial_argument_state_reconstructs_to_a_resumable_prefix() {
    // (arguments delivered so far, the tail the reconstruction must end on)
    let cases = [
        // Nothing but the name: the model has already emitted the newline that
        // follows the open tag.
        ("", format!("{INVOKE_OPEN}get_weather\">\n")),
        ("{", format!("{INVOKE_OPEN}get_weather\">\n")),
        (r#"{""#, PARAMETER_OPEN.to_string()),
        (r#"{"ci"#, format!("{PARAMETER_OPEN}ci")),
        (r#"{"city""#, format!("{PARAMETER_OPEN}city\"")),
        (r#"{"city": "#, format!("{PARAMETER_OPEN}city\"")),
        (r#"{"city": "Par"#, format!("{PARAMETER_OPEN}city\" string=\"true\">Par")),
        (
            r#"{"city": "Paris", "days": 5"#,
            format!("{PARAMETER_OPEN}days\" string=\"false\">5"),
        ),
    ];
    for (arguments, tail) in cases {
        let mut acc = Dsv4Reconstructor::new(CAP);
        acc.ingest(&tool_frame(0, Some("get_weather"), arguments, false)).unwrap();
        let text = acc.continuation_text().unwrap();
        assert!(text.ends_with(&tail), "arguments {arguments:?} → {text:?}");
        assert!(!text.contains(INVOKE_CLOSE), "a call still in flight is never closed");
    }
}

/// A number literal that is still growing must not be committed early: `12` is a
/// valid JSON value but the model may be about to emit `125`.
#[test]
fn a_growing_number_literal_stays_in_flight() {
    let text = |arguments: &str| {
        let mut acc = Dsv4Reconstructor::new(CAP);
        acc.ingest(&tool_frame(0, Some("f"), arguments, false)).unwrap();
        acc.continuation_text().unwrap()
    };
    assert!(text(r#"{"n": 12"#).ends_with(">12"));
    assert!(text(r#"{"n": 125"#).ends_with(">125"));
    assert!(text(r#"{"n": 125}"#).ends_with(&format!(">125{PARAMETER_CLOSE}\n{INVOKE_CLOSE}\n")));
}

/// Escaping and non-scalar values survive the round trip: the model writes JSON
/// with Python's separators, which is what we re-dump with.
#[test]
fn escaping_and_non_scalar_values_round_trip() {
    let mut acc = Dsv4Reconstructor::new(CAP);
    acc.ingest(&tool_frame(
        0,
        Some("save"),
        r#"{"body": "She said \"café\" then\nleft.", "xs": [1, 2, 3], "opts": {"grid": true}, "n": null}"#,
        true,
    ))
    .unwrap();
    let text = acc.continuation_text().unwrap();
    assert!(text.contains(&format!(
        "{PARAMETER_OPEN}body\" string=\"true\">She said \"café\" then\nleft.{PARAMETER_CLOSE}"
    )));
    assert!(text.contains(&format!("{PARAMETER_OPEN}xs\" string=\"false\">[1, 2, 3]{PARAMETER_CLOSE}")));
    assert!(text.contains(&format!(
        "{PARAMETER_OPEN}opts\" string=\"false\">{{\"grid\": true}}{PARAMETER_CLOSE}"
    )));
    assert!(text.contains(&format!("{PARAMETER_OPEN}n\" string=\"false\">null{PARAMETER_CLOSE}")));
}

/// The one documented lossy input: the parser discarded the raw whitespace of a
/// non-canonical non-scalar argument before we saw it, so we re-dump canonically.
/// Not guarded, by design — the client received the canonical form too, so the
/// resume stays consistent with what was delivered; the cost is prefix-cache
/// alignment against the dead leg's token ids, not client-visible correctness.
#[test]
fn non_canonical_json_spacing_round_trips_to_canonical_form() {
    let mut acc = Dsv4Reconstructor::new(CAP);
    acc.ingest(&tool_frame(0, Some("plot"), r#"{"xs":[1,2,3]}"#, true)).unwrap();
    let text = acc.continuation_text().unwrap();
    assert!(text.contains(">[1, 2, 3]<"), "spacing is the parser's, not the model's: {text}");
}

/// A call with no arguments at all is a shape the harness never captured: the
/// port reproduces its encoder exactly, which puts an empty line where the
/// parameters would be. Pinned rather than "fixed" because inventing DSML the
/// model was never observed emitting is how the two bugs above happened; it
/// wants a ground-truth capture before it is changed.
#[test]
fn a_zero_parameter_call_keeps_the_encoders_blank_body_line() {
    let mut acc = Dsv4Reconstructor::new(CAP);
    acc.ingest(&tool_frame(0, Some("now"), "{}", true)).unwrap();
    assert_eq!(
        acc.continuation_text().unwrap(),
        format!("</think>\n\n{TOOL_CALLS_OPEN}\n{INVOKE_OPEN}now\">\n\n{INVOKE_CLOSE}\n{TOOL_CALLS_CLOSE}")
    );
}

// ── shared accumulator semantics ─────────────────────────────────────────────

#[test]
fn nothing_generated_is_not_resumable() {
    let acc = Dsv4Reconstructor::new(CAP);
    assert_eq!(acc.continuation_text(), None);
    assert_eq!(acc.len_bytes(), 0);
    assert!(!acc.saw_finish_reason());
}

#[test]
fn the_envelope_and_finish_reason_are_tracked_as_for_plain_content() {
    let mut acc = Dsv4Reconstructor::new(CAP);
    acc.ingest(&json!({
        "id": "chatcmpl-1", "created": 1_700_000_000, "model": "dsv4-flash",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}}]
    }))
    .unwrap();
    let env = acc.envelope().expect("captured from the first chunk");
    assert_eq!(env.id, "chatcmpl-1");
    assert_eq!(env.model, "dsv4-flash");
    assert_eq!(env.created, 1_700_000_000);

    assert!(!acc.saw_finish_reason());
    acc.ingest(&json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}))
        .unwrap();
    assert!(acc.saw_finish_reason());
}

#[test]
fn usage_only_and_choiceless_frames_are_ignored() {
    let mut acc = Dsv4Reconstructor::new(CAP);
    acc.ingest(&json!({"id": "chatcmpl-1", "choices": [{"delta": {"content": "hi"}}]}))
        .unwrap();
    acc.ingest(&json!({"choices": [], "usage": {"prompt_tokens": 5}})).unwrap();
    acc.ingest(&json!({"id": "chatcmpl-1"})).unwrap();
    assert_eq!(acc.continuation_text().as_deref(), Some("</think>hi"));
}

#[test]
fn multi_choice_disarms() {
    let mut acc = Dsv4Reconstructor::new(CAP);
    let err = acc
        .ingest(&json!({"choices": [
            {"index": 0, "delta": {"content": "a"}},
            {"index": 1, "delta": {"content": "b"}}
        ]}))
        .unwrap_err();
    assert_eq!(err, AccumulateError::MultiChoice);
    assert_eq!(acc.continuation_text(), None);
}

#[test]
fn exceeding_the_cap_disarms_and_drops_every_channel() {
    let mut acc = Dsv4Reconstructor::new(16);
    acc.ingest(&json!({"choices": [{"delta": {"reasoning_content": "12345"}}]}))
        .unwrap();
    acc.ingest(&tool_frame(0, Some("f"), "{", false)).unwrap();
    assert_eq!(acc.len_bytes(), 7, "reasoning + name + arguments all count against the cap");

    let err = acc.ingest(&tool_frame(0, None, r#""city": "Paris""#, false)).unwrap_err();
    assert_eq!(err, AccumulateError::CapExceeded);
    assert_eq!(acc.len_bytes(), 0);
    assert_eq!(acc.continuation_text(), None);
}

/// The families this reconstructor has NOT been measured against still disarm,
/// exactly as they do for `PlainContent`.
#[test]
fn unmeasured_delta_shapes_still_disarm() {
    let mut legacy = Dsv4Reconstructor::new(CAP);
    assert_eq!(
        legacy
            .ingest(&json!({"choices": [{"delta": {"function_call": {"name": "f"}}}]}))
            .unwrap_err(),
        AccumulateError::UnsupportedDelta
    );

    // `reasoning` without `reasoning_content` is reasoning text we have no
    // measured position for in the sequence.
    let mut bare = Dsv4Reconstructor::new(CAP);
    assert_eq!(
        bare.ingest(&json!({"choices": [{"delta": {"reasoning": "hmm"}}]})).unwrap_err(),
        AccumulateError::UnsupportedDelta
    );

    // Null-valued keys are sent on ordinary frames by most providers.
    let mut ok = Dsv4Reconstructor::new(CAP);
    ok.ingest(&json!({"choices": [{"delta": {
        "content": "fine", "reasoning": null, "function_call": null, "tool_calls": null
    }}]}))
    .unwrap();
    assert_eq!(ok.continuation_text().as_deref(), Some("</think>fine"));
}

#[test]
fn disarm_is_sticky_and_keeps_the_first_cause() {
    let mut acc = Dsv4Reconstructor::new(CAP);
    assert_eq!(
        acc.ingest(&json!({"choices": [{"delta": {"function_call": {}}}]})).unwrap_err(),
        AccumulateError::UnsupportedDelta
    );
    assert_eq!(
        acc.ingest(&json!({"choices": [{"delta": {"content": "more"}}]})).unwrap_err(),
        AccumulateError::UnsupportedDelta
    );
    assert_eq!(acc.disarmed(), Some(AccumulateError::UnsupportedDelta));
}
