//! Forward-parser tests, driven by the same fidelity captures the
//! reconstructor is pinned against.
//!
//! Each fixture is a raw+parsed PAIR of one real stream: `harness.final` is the
//! raw text the model emitted (validated byte-exact against a `/v1/completions`
//! read with token-id prompts, which never runs the chat parser), and `frames`
//! is what the serving stack's parser split that same text into. The
//! reconstructor turns `frames` back into `final`; this parser turns `final`
//! back into `frames`. Two consequences are used throughout:
//!
//! - **Goldens** feed `final` and compare the CUMULATIVE emitted channels
//!   against the capture's. Cumulative because fragment granularity legitimately
//!   differs — the two providers in the captures do not even agree with each
//!   other — while channel CONTENT must not.
//! - **Round trips** compose the two directions. `harness.cut_lens[k]` is the
//!   byte length of the reconstruction after `k + 1` frames, and every cut is a
//!   prefix of `final`, so cut *k* gives both a resume prefix and the exact raw
//!   text a resume leg would have to produce.
//!
//! Where the spec asks for a case the captures do not contain (a tag split
//! immediately inside a multi-byte character, an argument value that is still
//! growing), the case is constructed from the DSML grammar and its test name
//! says `synthetic`.

use serde_json::{Value, json};

use super::*;
use crate::continuation::forward::{ForwardDelta, ForwardParser, ForwardSeed};

const CAP: usize = 1024 * 1024;

/// Every tag that is STRUCTURE. None of these may ever reach a client channel —
/// a real `</think>` in a customer's content stream is the bug this parser
/// exists to fix.
const STRUCTURE: [&str; 7] = [
    THINK_END,
    TOOL_CALLS_OPEN,
    TOOL_CALLS_CLOSE,
    INVOKE_OPEN,
    INVOKE_CLOSE,
    PARAMETER_OPEN,
    PARAMETER_CLOSE,
];

// ── fixtures ─────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct Fixture {
    name: String,
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

fn delta_frames(fixture: &Fixture) -> Vec<&Value> {
    fixture
        .frames
        .iter()
        .filter(|f| f.get("choices").and_then(Value::as_array).is_some_and(|c| !c.is_empty()))
        .collect()
}

// ── channels ─────────────────────────────────────────────────────────────────

#[derive(Debug, Default, PartialEq, Eq)]
struct Channels {
    reasoning: String,
    content: String,
    calls: Vec<Call>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Call {
    index: u32,
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl Channels {
    fn slot(&mut self, index: u32) -> &mut Call {
        if let Some(pos) = self.calls.iter().position(|c| c.index == index) {
            return &mut self.calls[pos];
        }
        self.calls.push(Call { index, ..Call::default() });
        self.calls.last_mut().expect("just pushed")
    }

    fn add(&mut self, delta: &ForwardDelta) {
        match delta {
            ForwardDelta::Reasoning(text) => self.reasoning.push_str(text),
            ForwardDelta::Content(text) => self.content.push_str(text),
            ForwardDelta::ToolCall {
                index,
                id,
                name,
                arguments,
            } => {
                let slot = self.slot(*index);
                if id.is_some() {
                    slot.id = id.clone();
                }
                if name.is_some() {
                    slot.name = name.clone();
                }
                slot.arguments.push_str(arguments);
            }
        }
    }

    fn of(deltas: &[ForwardDelta]) -> Self {
        let mut channels = Self::default();
        for delta in deltas {
            channels.add(delta);
        }
        channels
    }

    /// The same accumulation performed over a capture's own frames.
    fn of_capture(fixture: &Fixture) -> Self {
        let mut channels = Self::default();
        for frame in delta_frames(fixture) {
            let Some(delta) = frame["choices"][0].get("delta").and_then(Value::as_object) else {
                continue;
            };
            if let Some(text) = delta.get("reasoning_content").and_then(Value::as_str) {
                channels.reasoning.push_str(text);
            }
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                channels.content.push_str(text);
            }
            for call in delta.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                let index = u32::try_from(call["index"].as_u64().unwrap_or(0)).unwrap_or(0);
                let slot = channels.slot(index);
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    slot.id = Some(id.to_string());
                }
                let function = call.get("function");
                if let Some(name) = function.and_then(|f| f.get("name")).and_then(Value::as_str) {
                    slot.name = Some(name.to_string());
                }
                if let Some(args) = function.and_then(|f| f.get("arguments")).and_then(Value::as_str) {
                    slot.arguments.push_str(args);
                }
            }
        }
        channels
    }
}

// ── driving the parser ───────────────────────────────────────────────────────

/// Feed `raw` split at every offset in `splits`, then finish.
fn parse_split(parser: &mut dyn ForwardParser, raw: &str, splits: &[usize]) -> Vec<ForwardDelta> {
    let mut out = Vec::new();
    let mut previous = 0;
    for &split in splits {
        out.extend(parser.feed(&raw[previous..split]));
        previous = split;
    }
    out.extend(parser.feed(&raw[previous..]));
    out.extend(parser.finish());
    out
}

/// Parse a whole raw generation from the start of a thinking-mode turn.
fn parse_whole(raw: &str) -> Vec<ForwardDelta> {
    let mut parser = Dsv4Forward::new(ForwardSeed::Reasoning);
    parse_split(&mut parser, raw, &[])
}

/// One chat chunk carrying a parsed delta, as the middleware builds it.
fn chunk(delta: &ForwardDelta) -> Value {
    json!({
        "id": "chatcmpl-resumed", "model": "dsv4-flash", "created": 1,
        "choices": [{"index": 0, "delta": delta.clone().into_delta(), "finish_reason": Value::Null}],
    })
}

fn terminal_frame() -> Value {
    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]})
}

/// Push parsed deltas back through the reconstructor — the layer's chain-resume
/// path — and read out the raw text it would send to a further resume leg.
fn reserialize(acc: &mut Dsv4Reconstructor, deltas: &[ForwardDelta]) -> String {
    for delta in deltas {
        acc.ingest(&chunk(delta)).expect("a parsed delta never disarms its own family");
    }
    acc.ingest(&terminal_frame()).expect("a terminal frame never disarms");
    acc.continuation_text().unwrap_or_default()
}

/// Char boundaries of `raw`, excluding 0 — every place a chunk could end.
fn boundaries(raw: &str) -> Vec<usize> {
    (1..raw.len()).filter(|i| raw.is_char_boundary(*i)).collect()
}

fn assert_no_structure_leaked(channels: &Channels, what: &str) {
    for tag in STRUCTURE {
        assert!(!channels.reasoning.contains(tag), "{what}: {tag} leaked into reasoning");
        assert!(!channels.content.contains(tag), "{what}: {tag} leaked into content");
        for call in &channels.calls {
            assert!(!call.arguments.contains(tag), "{what}: {tag} leaked into arguments");
        }
    }
}

// ── goldens: the captured streams, parsed back into their channels ───────────

/// The headline golden. Feed each capture's RAW text and compare the cumulative
/// channels against what the serving stack's own parser produced from it.
///
/// Two documented differences, both fixture reality rather than parser error:
///
/// - the fragmenting provider SWALLOWS the `\n\n` that separates the body from
///   the tool block, so its captured content channel is short by that run of
///   newlines. We emit it (the other provider does too), which is also what
///   makes the round trip byte-exact — the reconstructor only re-injects a
///   separator the content lacks;
/// - argument BYTES differ where the provider chose compact JSON
///   (`{"city":"Paris"}`) for a call the model wrote out in DSML, so arguments
///   are compared as parsed JSON. The spec expects byte equality here; the
///   fixtures show the provider's spacing is its own, so the fixtures win.
#[test]
fn every_captured_stream_parses_back_into_the_channels_it_was_split_from() {
    let mut checked = 0;
    for fixture in fixtures() {
        let parsed = Channels::of(&parse_whole(&fixture.harness.final_text));
        let captured = Channels::of_capture(&fixture);
        let name = &fixture.name;

        assert_eq!(parsed.reasoning, captured.reasoning, "{name}: reasoning channel");
        assert!(
            parsed.content.starts_with(&captured.content),
            "{name}: content channel\n  ours: {:?}\n  capture: {:?}",
            parsed.content,
            captured.content
        );
        assert!(
            parsed.content[captured.content.len()..].chars().all(|c| c == '\n'),
            "{name}: the only content a provider may swallow is the tool-block separator, got {:?}",
            &parsed.content[captured.content.len()..]
        );

        assert_eq!(parsed.calls.len(), captured.calls.len(), "{name}: tool-call count");
        for (ours, theirs) in parsed.calls.iter().zip(&captured.calls) {
            assert_eq!(ours.index, theirs.index, "{name}: tool-call index");
            assert_eq!(ours.name, theirs.name, "{name}: tool-call name");
            let ours_args = serde_json::from_str::<Value>(&ours.arguments).ok();
            let theirs_args = serde_json::from_str::<Value>(&theirs.arguments).ok();
            assert_eq!(
                ours_args, theirs_args,
                "{name}: call {} arguments\n  ours: {}\n  capture: {}",
                ours.index, ours.arguments, theirs.arguments
            );
        }
        assert_no_structure_leaked(&parsed, name);
        checked += 1;
    }
    assert_eq!(checked, 10, "every captured stream is exercised");
}

/// Ids are minted in the scheme the captures use, so a client cannot tell a
/// resumed call from an original one by the shape of its id.
#[test]
fn generated_call_ids_match_the_captured_id_scheme() {
    for fixture in fixtures() {
        for call in Channels::of(&parse_whole(&fixture.harness.final_text)).calls {
            let Some(id) = call.id else {
                panic!("{}: a call opened by the parser always carries an id", fixture.name);
            };
            let hex = id.strip_prefix("call_").expect("the captured prefix");
            assert_eq!(hex.len(), 24, "{}: {id}", fixture.name);
            assert!(hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()), "{id}");
        }
    }
}

// ── the cumulative-identity property ─────────────────────────────────────────

/// **Deliverable 2.** Whatever the chunk boundaries, the emitted channels
/// re-serialized by the reconstructor equal the input bytes. Nothing is
/// reordered, nothing is dropped, and no tag ever reaches a channel — including
/// when a chunk ends in the middle of one.
///
/// Every char boundary of every capture is used as a split point, which covers
/// mid-tag (every DSML tag is 19-23 bytes of mostly multi-byte `｜`), mid-value,
/// mid-name and mid-reasoning splits without having to enumerate them.
#[test]
fn cumulative_identity_holds_at_every_split_point_of_every_capture() {
    let mut splits = 0;
    for fixture in fixtures() {
        let raw = &fixture.harness.final_text;
        for split in boundaries(raw) {
            let mut parser = Dsv4Forward::new(ForwardSeed::Reasoning);
            let deltas = parse_split(&mut parser, raw, &[split]);
            let mut acc = Dsv4Reconstructor::new(CAP, true);
            assert_eq!(
                reserialize(&mut acc, &deltas),
                *raw,
                "{}: split at {split} is not byte-identical",
                fixture.name
            );
            assert_no_structure_leaked(&Channels::of(&deltas), &fixture.name);
            splits += 1;
        }
    }
    assert!(splits > 3_000, "the property is exercised broadly, got {splits} splits");
}

/// The pathological boundary set: a chunk per character, so EVERY tag arrives
/// split and the hold-back is exercised at every position within it.
#[test]
fn cumulative_identity_holds_when_every_character_is_its_own_chunk() {
    for fixture in fixtures() {
        let raw = &fixture.harness.final_text;
        let mut parser = Dsv4Forward::new(ForwardSeed::Reasoning);
        let deltas = parse_split(&mut parser, raw, &boundaries(raw));
        let mut acc = Dsv4Reconstructor::new(CAP, true);
        assert_eq!(reserialize(&mut acc, &deltas), *raw, "{}", fixture.name);
        assert_no_structure_leaked(&Channels::of(&deltas), &fixture.name);
    }
}

/// Three-way splits, strided so the pairs stay affordable. A two-way split can
/// only ever hold back once; this puts a second boundary inside the text a
/// previous hold-back released.
#[test]
fn cumulative_identity_holds_across_three_way_splits() {
    for fixture in fixtures() {
        let raw = &fixture.harness.final_text;
        let points = boundaries(raw);
        for (n, &first) in points.iter().enumerate().filter(|(n, _)| n % 11 == 0) {
            for &second in points.iter().skip(n).step_by(13) {
                let mut parser = Dsv4Forward::new(ForwardSeed::Reasoning);
                let deltas = parse_split(&mut parser, raw, &[first, second]);
                let mut acc = Dsv4Reconstructor::new(CAP, true);
                assert_eq!(
                    reserialize(&mut acc, &deltas),
                    *raw,
                    "{}: splits at {first} and {second}",
                    fixture.name
                );
            }
        }
    }
}

// ── hold-back, in isolation ──────────────────────────────────────────────────

/// A chunk that ends mid-tag must not emit the fragment as text, and must
/// resolve it as the tag once the rest arrives.
#[test]
fn a_tag_split_across_chunks_is_never_emitted_as_text() {
    let mut parser = Dsv4Forward::new(ForwardSeed::Reasoning);
    let first = parser.feed("thinking</thi");
    assert_eq!(
        first,
        vec![ForwardDelta::Reasoning("thinking".to_string())],
        "the tag fragment is withheld, not sent as reasoning"
    );
    let second = parser.feed("nk>body");
    assert_eq!(second, vec![ForwardDelta::Content("body".to_string())]);
    assert!(parser.finish().is_empty());
}

/// The other half of the rule: a withheld fragment that turns out NOT to be a
/// tag is released as ordinary text, in order.
#[test]
fn a_disproven_tag_prefix_is_released_as_text() {
    let mut parser = Dsv4Forward::new(ForwardSeed::Content);
    // `<` opens no tag here, and `</thing>` is not `</think>` — both are ordinary
    // body text that a naive scan would swallow.
    let mut deltas = parser.feed("a <");
    deltas.extend(parser.feed("3 and 4</th"));
    deltas.extend(parser.feed("ing>"));
    deltas.extend(parser.finish());

    let channels = Channels::of(&deltas);
    assert_eq!(channels.content, "a <3 and 4</thing>", "every byte arrives once, in order");
    assert!(channels.calls.is_empty() && channels.reasoning.is_empty());
}

/// The layer maps/synthesizes `finish_reason: "tool_calls"` from this signal,
/// so it must stay false while a call's structure is still open — a leg that
/// ends mid-invoke has handed the client incomplete arguments JSON, and
/// announcing `tool_calls` there would tell it to execute a half-built call.
#[test]
fn tool_calls_are_only_signalled_once_the_block_closes() {
    let mut parser = Dsv4Forward::new(ForwardSeed::Content);
    parser.feed("Now.\n\n");
    assert!(!parser.ends_in_tool_calls(), "no call announced yet");
    parser.feed(TOOL_CALLS_OPEN);
    parser.feed("\n");
    parser.feed(INVOKE_OPEN);
    parser.feed("get_weather\">\n");
    assert!(!parser.ends_in_tool_calls(), "announced, but its arguments are still open");
    parser.feed(&format!("{PARAMETER_OPEN}city\" string=\"true\">Par"));
    assert!(!parser.ends_in_tool_calls(), "mid-value is the dangerous case: partial JSON");
    parser.feed(&format!("is{PARAMETER_CLOSE}\n"));
    parser.feed(INVOKE_CLOSE);
    assert!(!parser.ends_in_tool_calls(), "the block itself has not closed");
    parser.feed("\n");
    parser.feed(TOOL_CALLS_CLOSE);
    assert!(parser.ends_in_tool_calls(), "a closed block is a complete, executable call");
}

/// `finish` flushes a hold that never resolved — the stream ended on what could
/// still have become a tag.
#[test]
fn finish_flushes_a_hold_that_never_resolved() {
    let mut parser = Dsv4Forward::new(ForwardSeed::Reasoning);
    assert_eq!(parser.feed("done</thin"), vec![ForwardDelta::Reasoning("done".to_string())]);
    assert_eq!(
        parser.finish(),
        vec![ForwardDelta::Reasoning("</thin".to_string())],
        "a truncated tag is text after all — dropping it would lose generated bytes"
    );
    assert!(parser.finish().is_empty(), "the hold is cleared, not re-emitted");
}

/// SYNTHETIC: no capture splits a chunk inside a multi-byte character, because
/// `feed` takes `&str` — an SSE frame is JSON-decoded before it reaches us, so a
/// torn UTF-8 sequence cannot be represented at this boundary. What CAN happen
/// is a split immediately either side of one, and every DSML tag is built from
/// three-byte `｜` (U+FF5C), so this pins that case against the grammar.
#[test]
fn synthetic_a_tag_split_around_its_multi_byte_characters_holds_back() {
    let tag = TOOL_CALLS_OPEN;
    for split in boundaries(tag) {
        let mut parser = Dsv4Forward::new(ForwardSeed::Content);
        let mut deltas = parser.feed("body");
        deltas.extend(parser.feed(&tag[..split]));
        deltas.extend(parser.feed(&tag[split..]));
        deltas.extend(parser.feed(INVOKE_OPEN));
        deltas.extend(parser.feed("f\">\n</｜DSML｜invoke>\n</｜DSML｜tool_calls>"));
        deltas.extend(parser.finish());

        let channels = Channels::of(&deltas);
        assert_eq!(channels.content, "body", "split at {split} of {tag:?}");
        assert_no_structure_leaked(&channels, "synthetic multi-byte split");
        assert_eq!(channels.calls.len(), 1);
        assert_eq!(channels.calls[0].arguments, "{}");
    }

    // And the same for a multi-byte VALUE, which is text rather than structure
    // but shares the hold-back path — the `｜` inside it is one character the
    // parser must not confuse with the start of a closing tag.
    let raw = format!("{PARAMETER_OPEN}note\" string=\"true\">caf\u{e9} \u{ff5c}{PARAMETER_CLOSE}");
    for split in boundaries(&raw) {
        let mut parser = Dsv4Forward::new(ForwardSeed::InToolCall {
            index: 0,
            args_so_far: "{".to_string(),
        });
        let deltas = parse_split(&mut parser, &raw, &[split]);
        assert_eq!(
            Channels::of(&deltas).calls[0].arguments,
            "\"note\": \"caf\u{e9} \u{ff5c}\"",
            "split at {split}"
        );
    }
}

// ── the seed ─────────────────────────────────────────────────────────────────

/// **Deliverable 1.** Every cut of every capture yields the seed the prefix
/// actually left open, and all four states occur in the captures.
#[test]
fn the_seed_describes_the_structure_the_prefix_left_open() {
    let mut seen = (0, 0, 0, 0);
    for fixture in fixtures() {
        let frames = delta_frames(&fixture);
        for k in 1..=frames.len() {
            let mut acc = Dsv4Reconstructor::new(CAP, true);
            for frame in frames.iter().take(k) {
                acc.ingest(frame).expect("fixture streams never disarm");
            }
            let Some(prefix) = acc.continuation_text() else {
                continue;
            };
            let seed = acc.forward_seed();
            let label = format!("{}: cut {k}", fixture.name);
            match &seed {
                ForwardSeed::Reasoning => {
                    seen.0 += 1;
                    assert!(!prefix.contains(THINK_END), "{label}: still inside the think block");
                }
                ForwardSeed::Content => {
                    seen.1 += 1;
                    assert!(!prefix.contains(TOOL_CALLS_OPEN), "{label}: the tool block has not opened");
                }
                ForwardSeed::BetweenToolCalls { .. } => {
                    seen.2 += 1;
                    assert!(
                        prefix.ends_with(&format!("{INVOKE_CLOSE}\n"))
                            || prefix.ends_with(&format!("{TOOL_CALLS_OPEN}\n"))
                            || prefix.ends_with(TOOL_CALLS_CLOSE),
                        "{label}: between calls, prefix tail {:?}",
                        prefix.chars().rev().take(30).collect::<String>()
                    );
                }
                ForwardSeed::InToolCall { .. } => {
                    seen.3 += 1;
                    assert!(
                        !prefix.ends_with(&format!("{INVOKE_CLOSE}\n")),
                        "{label}: a closed invoke is never still in flight"
                    );
                }
            }
        }
    }
    assert!(
        seen.0 > 0 && seen.1 > 0 && seen.2 > 0 && seen.3 > 0,
        "all four seeds occur: {seen:?}"
    );
}

/// The sub-state inside a call is derived from the arguments the CLIENT already
/// holds, so the parser owes exactly the bytes the prefix has not yet spelled.
#[test]
fn an_in_call_seed_owes_only_what_the_client_has_not_received() {
    // (arguments already delivered, the raw that follows, the arguments text the
    // resumed leg must add)
    let cases = [
        ("", "<｜DSML｜parameter name=\"city\" string=\"true\">Paris", r#"{"city": "Paris"#),
        // The opening brace is already delivered, so it is not owed again.
        ("{", "<｜DSML｜parameter name=\"city\" string=\"true\">Paris", r#""city": "Paris"#),
        (r#"{"ci"#, "ty\" string=\"true\">Paris", r#"ty": "Paris"#),
        (r#"{"city""#, " string=\"true\">Paris", r#": "Paris"#),
        (r#"{"city": "Par"#, "is", "is"),
        // A closed value quote in the delivered args means leg 1's parser saw
        // the parameter close tag, so the reconstructed prefix ends AFTER
        // `</｜DSML｜parameter>` + the rule-1 newline — a real leg resumes at
        // the next parameter, never by re-emitting the close tag (a repeated
        // close tag is out of grammar and poisons).
        (
            r#"{"city": "Paris""#,
            "<｜DSML｜parameter name=\"unit\" string=\"true\">c",
            r#", "unit": "c"#,
        ),
        (r#"{"n": 12"#, "5", "5"),
    ];
    for (delivered, raw, owed) in cases {
        let mut parser = Dsv4Forward::new(ForwardSeed::InToolCall {
            index: 3,
            args_so_far: delivered.to_string(),
        });
        let mut deltas = parser.feed(raw);
        deltas.extend(parser.finish());
        let channels = Channels::of(&deltas);
        assert_eq!(channels.calls.len(), 1, "delivered {delivered:?}");
        assert_eq!(channels.calls[0].arguments, owed, "delivered {delivered:?}");
        assert_eq!(channels.calls[0].index, 3, "the open call keeps its index");
        assert_eq!(channels.calls[0].id, None, "leg 1 already gave this call its id");
        assert_eq!(channels.calls[0].name, None, "and its name");
    }
}

/// A chat-mode leg has no think tag to close, so its resumed text is content
/// from the first byte — the seed says so, and nothing infers it from the text.
#[test]
fn the_seed_follows_the_legs_serving_mode() {
    let body = json!({"id": "c", "choices": [{"index": 0, "delta": {"reasoning_content": "hmm"}}]});

    let mut thinking = Dsv4Reconstructor::new(CAP, true);
    thinking.ingest(&body).unwrap();
    assert_eq!(thinking.forward_seed(), ForwardSeed::Reasoning);

    let mut chat = Dsv4Reconstructor::new(CAP, false);
    chat.ingest(&body).unwrap();
    assert_eq!(
        chat.forward_seed(),
        ForwardSeed::Content,
        "a chat-mode prompt already ended with </think>; there is none coming"
    );

    // Once the body has started, both are content.
    thinking
        .ingest(&json!({"choices": [{"index": 0, "delta": {"content": "Answer"}}]}))
        .unwrap();
    assert_eq!(thinking.forward_seed(), ForwardSeed::Content);
}

// ── index and id continuity ──────────────────────────────────────────────────

/// **Deliverable 3.** A resumed leg continues leg 1's tool-call numbering. The
/// client must never see an index restart, because that is how an SDK decides
/// two fragments belong to different calls.
#[test]
fn tool_call_indexes_continue_leg_ones_numbering() {
    // Leg 1 delivered a complete call at index 0 and died before the next.
    let mut acc = Dsv4Reconstructor::new(CAP, true);
    acc.ingest(&json!({"id": "c", "choices": [{"index": 0, "delta": {"tool_calls": [{
        "index": 0, "id": "call_from_leg_one", "type": "function",
        "function": {"name": "get_weather", "arguments": r#"{"city": "Paris"}"#}
    }]}}]}))
    .unwrap();
    assert_eq!(acc.forward_seed(), ForwardSeed::BetweenToolCalls { next_index: 1 });

    let mut parser = acc.forward_parser();
    let raw = format!("{INVOKE_OPEN}get_weather\">\n{PARAMETER_OPEN}city\" string=\"true\">London{PARAMETER_CLOSE}\n{INVOKE_CLOSE}");
    let mut deltas = parser.feed(&raw);
    deltas.extend(parser.finish());

    let channels = Channels::of(&deltas);
    assert_eq!(channels.calls.len(), 1);
    assert_eq!(channels.calls[0].index, 1, "the sibling call is index 1, not a restart at 0");
    assert_eq!(channels.calls[0].arguments, r#"{"city": "London"}"#);
    let id = channels.calls[0].id.as_deref().expect("a call opened after the seam gets an id");
    assert_ne!(id, "call_from_leg_one", "a new call is a new id");
    assert!(id.starts_with("call_"));

    // And the accumulator sees one two-call block, still correctly numbered.
    for delta in &deltas {
        acc.ingest(&chunk(delta)).unwrap();
    }
    acc.ingest(&terminal_frame()).unwrap();
    let text = acc.continuation_text().unwrap();
    assert_eq!(text.matches(INVOKE_OPEN).count(), 2, "both calls are in the prefix: {text}");
    assert!(text.contains("Paris") && text.contains("London"));
}

/// A call that was still OPEN at the death point keeps the id and name leg 1
/// already sent — re-announcing either would make a client open a second call.
#[test]
fn a_call_open_at_the_seam_is_never_re_announced() {
    let mut acc = Dsv4Reconstructor::new(CAP, true);
    acc.ingest(&json!({"id": "c", "choices": [{"index": 0, "delta": {"tool_calls": [{
        "index": 0, "id": "call_from_leg_one", "type": "function",
        "function": {"name": "get_weather", "arguments": r#"{"city": "Par"#}
    }]}}]}))
    .unwrap();

    let mut parser = acc.forward_parser();
    let mut deltas = parser.feed(&format!("is{PARAMETER_CLOSE}\n{INVOKE_CLOSE}"));
    deltas.extend(parser.finish());

    for delta in &deltas {
        match delta {
            ForwardDelta::ToolCall { index, id, name, .. } => {
                assert_eq!(*index, 0);
                assert_eq!(*id, None, "the id is leg 1's");
                assert_eq!(*name, None, "the name is leg 1's");
            }
            other => panic!("only tool-call fragments continue an open call, got {other:?}"),
        }
    }
    assert_eq!(Channels::of(&deltas).calls[0].arguments, r#"is"}"#);
}

// ── the chain round trip ─────────────────────────────────────────────────────

/// **Deliverable 4.** One full round trip is the identity:
/// `reconstruct(accumulate(leg1 ⊕ parse(resumed))) == raw prefix ⊕ resumed raw`.
///
/// At every cut of every capture: take the reconstruction the resume leg would
/// be prompted with, treat the rest of the raw text as what the model then
/// generates, parse it with a parser seeded from that same accumulator, feed the
/// PARSED deltas back in (never the raw text — the reconstructor ingests
/// channels, which is its whole job), and require the accumulator to arrive back
/// at the original generation byte for byte.
#[test]
fn chain_resume_is_a_byte_exact_round_trip_at_every_cut() {
    let mut cuts = 0;
    for fixture in fixtures() {
        let frames = delta_frames(&fixture);
        let raw = &fixture.harness.final_text;
        assert_eq!(frames.len(), fixture.harness.cut_lens.len(), "{}: cut count", fixture.name);

        for k in 1..=frames.len() {
            let mut acc = Dsv4Reconstructor::new(CAP, true);
            for frame in frames.iter().take(k) {
                acc.ingest(frame).expect("fixture streams never disarm");
            }
            let Some(prefix) = acc.continuation_text() else {
                continue;
            };
            assert!(
                raw.starts_with(&prefix),
                "{}: cut {k} is not a prefix of the raw generation",
                fixture.name
            );

            // The parser is seeded from the accumulator at exactly this point —
            // the same handoff the middleware makes when a leg starts.
            let mut parser = acc.forward_parser();
            let resumed = &raw[prefix.len()..];
            let deltas = parse_split(parser.as_mut(), resumed, &boundaries(resumed));

            assert_no_structure_leaked(&Channels::of(&deltas), &format!("{}: cut {k}", fixture.name));
            assert_eq!(
                reserialize(&mut acc, &deltas),
                *raw,
                "{}: cut {k} does not round-trip\n  prefix ended: {:?}",
                fixture.name,
                prefix.chars().rev().take(40).collect::<String>()
            );
            cuts += 1;
        }
    }
    assert_eq!(cuts, 247, "every captured cut point is resumed and round-tripped");
}

// ── shapes the captures do not contain ───────────────────────────────────────

/// SYNTHETIC: a `string="false"` value is already JSON and passes through
/// verbatim, including while it is still growing — the client sees `12` before
/// `125`, and the reconstructor puts the raw literal back.
#[test]
fn synthetic_a_non_scalar_value_passes_through_as_json() {
    // The leading separator is part of what the model emits; without it the
    // reconstructor would inject one, which is an injection with no inverse.
    let raw = format!(
        "\n\n{TOOL_CALLS_OPEN}\n{INVOKE_OPEN}make_plot\">\n\
         {PARAMETER_OPEN}xs\" string=\"false\">[1, 2, 3]{PARAMETER_CLOSE}\n\
         {PARAMETER_OPEN}opts\" string=\"false\">{{\"grid\": true}}{PARAMETER_CLOSE}\n\
         {PARAMETER_OPEN}n\" string=\"false\">null{PARAMETER_CLOSE}\n{INVOKE_CLOSE}\n{TOOL_CALLS_CLOSE}"
    );
    let mut parser = Dsv4Forward::new(ForwardSeed::Content);
    let deltas = parse_split(&mut parser, &raw, &boundaries(&raw));
    let channels = Channels::of(&deltas);
    assert_eq!(
        channels.calls[0].arguments,
        r#"{"xs": [1, 2, 3], "opts": {"grid": true}, "n": null}"#
    );

    let mut acc = Dsv4Reconstructor::new(CAP, false);
    assert_eq!(reserialize(&mut acc, &deltas), raw, "and it re-serializes to the same DSML");
}

/// SYNTHETIC: a value truncated mid-flight (the model died, or hit its cap)
/// still reaches the client, and leaves the arguments in exactly the partial
/// state the reconstructor knows how to resume from.
#[test]
fn synthetic_a_truncated_value_is_flushed_rather_than_dropped() {
    let raw = format!("\n\n{TOOL_CALLS_OPEN}\n{INVOKE_OPEN}save\">\n{PARAMETER_OPEN}body\" string=\"true\">half a sent");
    let mut parser = Dsv4Forward::new(ForwardSeed::Content);
    let mut deltas = parser.feed(&raw);
    let flushed = parser.finish();
    assert!(flushed.is_empty(), "nothing was held back: no tag was in progress");
    deltas.extend(flushed);

    let channels = Channels::of(&deltas);
    assert_eq!(channels.calls[0].arguments, r#"{"body": "half a sent"#);
    assert_eq!(channels.calls[0].name.as_deref(), Some("save"));

    let mut acc = Dsv4Reconstructor::new(CAP, false);
    for delta in &deltas {
        acc.ingest(&chunk(delta)).unwrap();
    }
    assert_eq!(
        acc.continuation_text().as_deref(),
        Some(raw.as_str()),
        "a truncated call resumes from where it stopped, unclosed"
    );
}

/// SYNTHETIC: a half-written invoke open tag is structure the client was never
/// shown and the accumulator never recorded. Flushing it as text would leak
/// DSML into the content channel — the exact bug this parser exists to prevent.
#[test]
fn synthetic_a_partial_invoke_open_tag_dies_with_the_leg() {
    let mut parser = Dsv4Forward::new(ForwardSeed::BetweenToolCalls { next_index: 2 });
    let mut deltas = parser.feed(&format!("{INVOKE_OPEN}get_wea"));
    deltas.extend(parser.finish());
    assert!(deltas.is_empty(), "no half-announced call, and no leaked tag: {deltas:?}");
}

/// SYNTHETIC: a zero-parameter call. The reconstructor pins the encoder's blank
/// body line for this shape, so the parser has to produce `{}` from it.
#[test]
fn synthetic_a_zero_parameter_call_round_trips() {
    let raw = format!("\n\n{TOOL_CALLS_OPEN}\n{INVOKE_OPEN}now\">\n\n{INVOKE_CLOSE}\n{TOOL_CALLS_CLOSE}");
    let mut parser = Dsv4Forward::new(ForwardSeed::Content);
    let deltas = parse_split(&mut parser, &raw, &boundaries(&raw));
    assert_eq!(Channels::of(&deltas).calls[0].arguments, "{}");

    let mut acc = Dsv4Reconstructor::new(CAP, false);
    assert_eq!(reserialize(&mut acc, &deltas), raw);
}

// ── bounds and finish-time drops (Copilot review round, 2026-09-03) ──────────

/// An unterminated invoke name (a pathological leg with no max_tokens) must
/// not grow memory without bound: past the structural cap the parser poisons —
/// consumes everything, emits nothing, leaks nothing.
#[test]
fn an_unbounded_invoke_name_poisons_instead_of_growing() {
    let mut p = Dsv4Forward::new(ForwardSeed::Content);
    let mut out = p.feed("body text\n\n<｜DSML｜tool_calls>\n<｜DSML｜invoke name=\"");
    for _ in 0..40 {
        out.extend(p.feed(&"q".repeat(1024)));
    }
    out.extend(p.finish());
    let text: String = out
        .iter()
        .filter_map(|d| match d {
            ForwardDelta::Content(t) | ForwardDelta::Reasoning(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert!(!text.contains('q'), "poisoned input leaked as text: {text:?}");
    assert!(
        !out.iter().any(|d| matches!(d, ForwardDelta::ToolCall { .. })),
        "no call from a poisoned name"
    );
}

/// A parameter attribute that never closes accumulates in `hold`; the generic
/// feed-time bound must poison rather than re-scan a growing buffer forever.
#[test]
fn an_unbounded_param_attr_poisons_the_hold() {
    let mut p = Dsv4Forward::new(ForwardSeed::Content);
    p.feed("b\n\n<｜DSML｜tool_calls>\n<｜DSML｜invoke name=\"f\">\n<｜DSML｜parameter name=\"k\"");
    let mut out = Vec::new();
    for _ in 0..40 {
        out.extend(p.feed(&"y".repeat(1024)));
    }
    out.extend(p.finish());
    let text: String = out
        .iter()
        .filter_map(|d| match d {
            ForwardDelta::Content(t) | ForwardDelta::Reasoning(t) => Some(t.as_str()),
            ForwardDelta::ToolCall { arguments, .. } => Some(arguments.as_str()),
        })
        .collect();
    assert!(!text.contains('y'), "attr overflow leaked: {text:?}");
}

/// A leg that dies midway through a tag between invokes must not leak the
/// partial tag bytes as content at end-of-stream.
#[test]
fn a_partial_tag_at_finish_is_dropped_not_leaked() {
    let mut p = Dsv4Forward::new(ForwardSeed::Content);
    let mut out = p.feed("done.\n\n<｜DSML｜tool_calls>\n<｜DSML｜invoke name=\"f\">\n</｜DSML｜invoke>\n</｜DSML");
    out.extend(p.finish());
    let text: String = out
        .iter()
        .filter_map(|d| match d {
            ForwardDelta::Content(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert!(!text.contains("DSML"), "partial closing tag leaked: {text:?}");
}

/// Out-of-grammar text inside a tool block must poison, not surface as
/// content: the reconstructor serializes content BEFORE the tool block, so a
/// chained continuation would reorder those bytes.
#[test]
fn garbage_inside_a_tool_block_poisons() {
    let mut p = Dsv4Forward::new(ForwardSeed::BetweenToolCalls { next_index: 1 });
    let out = p.feed("\nwhat is this doing here\n<｜DSML｜invoke name=\"f\">");
    assert!(p.poisoned());
    assert!(
        !out.iter().any(|d| matches!(d, ForwardDelta::Content(t) if t.contains("what is"))),
        "out-of-grammar block text must not become content: {out:?}"
    );
}

/// Trailing non-whitespace after the block close is out of grammar for the
/// same reason — poison rather than emit content that a second resume would
/// re-order ahead of the tool block.
#[test]
fn trailing_text_after_the_block_poisons() {
    let mut p = Dsv4Forward::new(ForwardSeed::BetweenToolCalls { next_index: 1 });
    let mut out = p.feed("\n</｜DSML｜tool_calls>");
    out.extend(p.feed("\n\nBy the way, here is more prose."));
    out.extend(p.finish());
    assert!(p.poisoned());
    assert!(
        !out.iter().any(|d| matches!(d, ForwardDelta::Content(t) if t.contains("prose"))),
        "post-block text must not become content: {out:?}"
    );
}
