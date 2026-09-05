//! DeepSeek-V4 (DSML) reconstruction: split chat deltas → the raw text the model emitted.
//!
//! The DeepSeek-V4 family emits ONE token sequence. For a thinking turn (the
//! rendered default: the prompt ends `<｜Assistant｜><think>`) it looks like:
//!
//! ```text
//! reasoning … </think>content …\n\n<｜DSML｜tool_calls>
//! <｜DSML｜invoke name="NAME">
//! <｜DSML｜parameter name="K" string="true">V</｜DSML｜parameter>
//! </｜DSML｜invoke>
//! </｜DSML｜tool_calls>
//! ```
//!
//! The serving stack's chat parser splits that into `reasoning_content` deltas,
//! `content` deltas and structured tool-call fragments. This module inverts the
//! split, including the mid-flight states a death lands on: an open `<think>`,
//! a half-emitted parameter value, a partial parameter name.
//!
//! **Provenance.** This is a port of `reconstruct_dsv4.py` from the fidelity
//! harness (`test-env/continuation-fidelity/`), whose output was byte-compared
//! against ground truth read from `/v1/completions` with token-id prompts — the
//! one capture point that never runs the chat parser. Verdict and rules:
//! `working-docs/misc/continuation-fidelity-flash-dsv4.md` (24/24 resume cut
//! points, 23/23 end-to-end on real fragmented deltas). Three of its rules were
//! derived from measured failures rather than from the format, and each has a
//! test here:
//!
//! 1. **Seam newline** ([`seam_safe`]). A prefix ending exactly at
//!    `</｜DSML｜parameter>` or `</｜DSML｜invoke>` makes the model emit EOS
//!    immediately — 1 completion token, empty text, tool call left truncated.
//!    The newline that always follows those tags in well-formed DSML restores
//!    normal continuation.
//! 2. **Never speculatively close `</｜DSML｜tool_calls>`.** A completed invoke
//!    does not imply the block ended; closing it drops the sibling calls of a
//!    parallel block and breaks monotonicity (cut *k* would contain bytes that
//!    cut *k+1* removes).
//! 3. **Conditional `\n\n` injection.** The separator before the tool block is
//!    parser-dependent — some providers surface it as a content delta, others
//!    swallow it. Injecting it only when the accumulated content does not
//!    already end with it makes both shapes converge on identical bytes.
//!
//! **Known-unrecoverable input** (accepted, not guarded): a `string="false"`
//! parameter whose raw JSON used non-canonical spacing (`[1,2,3]`) round-trips
//! to the parser's canonical form (`[1, 2, 3]`), because the parser discarded the
//! raw whitespace before we ever saw it. The client received the canonical form
//! too, so the resume stays consistent with what was delivered; the cost is
//! prefix-cache alignment, not correctness. V4-Flash emits canonical spacing
//! unprompted, so this only fires when the caller asks for compact JSON.

use std::fmt;
use std::io;

use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::ser::Formatter;
use serde_json::{Number, Value};

use super::accumulate::{AccumulateError, StreamAccumulator, capture_envelope, single_choice};
use super::forward::{ForwardParser, ForwardSeed};
use super::rewrap::Envelope;

pub mod forward;

use forward::Dsv4Forward;

// The DSML tags. `｜` is U+FF5C FULLWIDTH VERTICAL LINE, not an ASCII pipe.
const TOOL_CALLS_OPEN: &str = "<｜DSML｜tool_calls>";
const TOOL_CALLS_CLOSE: &str = "</｜DSML｜tool_calls>";
const INVOKE_OPEN: &str = "<｜DSML｜invoke name=\"";
const INVOKE_CLOSE: &str = "</｜DSML｜invoke>";
const PARAMETER_OPEN: &str = "<｜DSML｜parameter name=\"";
const PARAMETER_CLOSE: &str = "</｜DSML｜parameter>";
const THINK_END: &str = "</think>";
/// What the model emits between the body and the tool block.
const TOOL_BLOCK_SEPARATOR: &str = "\n\n";

/// Tags that must never be the last bytes of a resume prefix (rule 1).
const CLOSING_TAGS: [&str; 2] = [PARAMETER_CLOSE, INVOKE_CLOSE];

// ── Python-compatible JSON ───────────────────────────────────────────────────

/// A JSON value that keeps its object keys in document order.
///
/// [`Value`] cannot be used here: its object is a `BTreeMap`, so a round trip
/// sorts the keys and `{"grid": true, "color": "red"}` comes back as
/// `{"color": "red", "grid": true}` — different bytes from what the model
/// emitted, and a prefix the model never wrote. Parameter order is part of the
/// generation, so it is preserved rather than normalised.
#[derive(Debug, PartialEq)]
enum Ordered {
    Null,
    Bool(bool),
    Number(Number),
    Str(String),
    Array(Vec<Ordered>),
    Object(Vec<(String, Ordered)>),
}

impl Serialize for Ordered {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            Ordered::Null => ser.serialize_unit(),
            Ordered::Bool(b) => ser.serialize_bool(*b),
            Ordered::Number(n) => n.serialize(ser),
            Ordered::Str(s) => ser.serialize_str(s),
            Ordered::Array(items) => ser.collect_seq(items),
            Ordered::Object(pairs) => {
                let mut map = ser.serialize_map(Some(pairs.len()))?;
                for (k, v) in pairs {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct OrderedVisitor;

        impl<'de> Visitor<'de> for OrderedVisitor {
            type Value = Ordered;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }

            fn visit_unit<E>(self) -> Result<Ordered, E> {
                Ok(Ordered::Null)
            }

            fn visit_none<E>(self) -> Result<Ordered, E> {
                Ok(Ordered::Null)
            }

            fn visit_bool<E>(self, v: bool) -> Result<Ordered, E> {
                Ok(Ordered::Bool(v))
            }

            fn visit_i64<E>(self, v: i64) -> Result<Ordered, E> {
                Ok(Ordered::Number(v.into()))
            }

            fn visit_u64<E>(self, v: u64) -> Result<Ordered, E> {
                Ok(Ordered::Number(v.into()))
            }

            fn visit_f64<E>(self, v: f64) -> Result<Ordered, E> {
                Ok(Number::from_f64(v).map_or(Ordered::Null, Ordered::Number))
            }

            fn visit_str<E>(self, v: &str) -> Result<Ordered, E> {
                Ok(Ordered::Str(v.to_string()))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Ordered, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Ordered::Array(items))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered, A::Error> {
                let mut pairs = Vec::new();
                while let Some((k, v)) = map.next_entry()? {
                    pairs.push((k, v));
                }
                Ok(Ordered::Object(pairs))
            }
        }

        de.deserialize_any(OrderedVisitor)
    }
}

/// `json.dumps(v, ensure_ascii=False, separators=(", ", ": "))`.
///
/// The model emits tool arguments with Python's separators, which is also what
/// tokenizer-svc's `pyjson` writer produces; that agreement is what makes
/// non-scalar parameter values re-serialize byte-exactly.
struct PyFormatter;

impl Formatter for PyFormatter {
    fn begin_array_value<W: ?Sized + io::Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        if first { Ok(()) } else { w.write_all(b", ") }
    }

    fn begin_object_key<W: ?Sized + io::Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        if first { Ok(()) } else { w.write_all(b", ") }
    }

    fn begin_object_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(b": ")
    }
}

fn py_json(value: &Ordered) -> String {
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, PyFormatter);
    if value.serialize(&mut ser).is_err() {
        return String::new();
    }
    String::from_utf8(buf).unwrap_or_default()
}

// ── Incremental parse of a possibly-truncated arguments object ───────────────

/// The item that was in flight when the arguments string ran out.
#[derive(Debug, PartialEq)]
enum Tail {
    /// Mid-way through a parameter NAME.
    Key(String),
    /// The name closed but its value has not started.
    KeyDone(String),
    /// Mid-way through a VALUE, with the fragment decoded as far as possible.
    Value { key: String, is_string: bool, frag: String },
}

/// Byte length of the UTF-8 character starting at `i` (never 0, so scans always
/// advance).
fn char_len(s: &str, i: usize) -> usize {
    s[i..].chars().next().map(char::len_utf8).unwrap_or(1)
}

fn skip_ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && matches!(b[*i], b' ' | b'\t' | b'\r' | b'\n') {
        *i += 1;
    }
}

/// Decode a truncated JSON string body (`abcé`), falling back to the raw
/// fragment when it cannot be decoded yet (a trailing lone backslash).
fn decode_fragment(frag: &str) -> String {
    serde_json::from_str::<String>(&format!("\"{frag}\"")).unwrap_or_else(|_| frag.to_string())
}

/// Scan the closing quote of the JSON string starting at `start`, honouring
/// escapes. Returns the index of the closing quote, or `None` if truncated.
fn scan_string(s: &str, start: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut j = start + 1;
    while j < b.len() {
        if b[j] == b'\\' && j + 1 < b.len() {
            j += 1 + char_len(s, j + 1);
            continue;
        }
        if b[j] == b'"' {
            return Some(j);
        }
        j += char_len(s, j);
    }
    None
}

/// Walk a growing prefix of a JSON object, returning what is known so far:
/// the fully-parsed pairs, plus the in-flight [`Tail`] if there is one.
///
/// This is what makes a death *inside* a tool call recoverable: the fragmenting
/// providers deliver `{`, `"city": "Paris"`, `, "unit": "c`, and the prefix has
/// to end at `…string="true">c`.
fn parse_partial_args(s: &str) -> (Vec<(String, Ordered)>, Option<Tail>) {
    let b = s.as_bytes();
    let n = b.len();
    let mut pairs: Vec<(String, Ordered)> = Vec::new();
    let mut i = 0usize;

    skip_ws(b, &mut i);
    if i < n && b[i] == b'{' {
        i += 1;
    } else {
        // Not even the opening brace yet.
        return (pairs, None);
    }

    loop {
        skip_ws(b, &mut i);
        if i >= n || b[i] == b'}' {
            return (pairs, None);
        }
        if b[i] == b',' {
            i += 1;
            skip_ws(b, &mut i);
        }
        if i >= n || b[i] != b'"' {
            return (pairs, None);
        }

        // ── key ──
        let key_start = i;
        let Some(j) = scan_string(s, key_start) else {
            return (pairs, Some(Tail::Key(decode_fragment(&s[key_start + 1..]))));
        };
        let Ok(key) = serde_json::from_str::<String>(&s[key_start..=j]) else {
            // Unparseable even though it looked closed: treat it as still in
            // flight rather than panicking on provider noise.
            return (pairs, Some(Tail::Key(s[key_start + 1..j].to_string())));
        };
        i = j + 1;

        skip_ws(b, &mut i);
        if i >= n || b[i] != b':' {
            return (pairs, Some(Tail::KeyDone(key)));
        }
        i += 1;
        skip_ws(b, &mut i);
        if i >= n {
            return (pairs, Some(Tail::KeyDone(key)));
        }

        // ── value ──
        let val_start = i;
        if b[i] == b'"' {
            let Some(j) = scan_string(s, val_start) else {
                let frag = decode_fragment(&s[val_start + 1..]);
                return (
                    pairs,
                    Some(Tail::Value {
                        key,
                        is_string: true,
                        frag,
                    }),
                );
            };
            let Ok(value) = serde_json::from_str::<Ordered>(&s[val_start..=j]) else {
                let frag = decode_fragment(&s[val_start + 1..j]);
                return (
                    pairs,
                    Some(Tail::Value {
                        key,
                        is_string: true,
                        frag,
                    }),
                );
            };
            pairs.push((key, value));
            i = j + 1;
        } else {
            // number / bool / null / array / object — scan to a balanced end.
            let (mut depth, mut j, mut in_str, mut esc) = (0i32, i, false, false);
            while j < n {
                let c = b[j];
                if in_str {
                    if esc {
                        esc = false;
                    } else if c == b'\\' {
                        esc = true;
                    } else if c == b'"' {
                        in_str = false;
                    }
                } else if c == b'"' {
                    in_str = true;
                } else if c == b'[' || c == b'{' {
                    depth += 1;
                } else if c == b']' || c == b'}' {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                } else if c == b',' && depth == 0 {
                    break;
                }
                j += char_len(s, j);
            }
            let frag = s[val_start..j].trim();
            let Ok(value) = serde_json::from_str::<Ordered>(frag) else {
                return (
                    pairs,
                    Some(Tail::Value {
                        key,
                        is_string: false,
                        frag: frag.to_string(),
                    }),
                );
            };
            if j >= n {
                // Ran out of input: the literal may still be growing (`12` → `125`).
                return (
                    pairs,
                    Some(Tail::Value {
                        key,
                        is_string: false,
                        frag: frag.to_string(),
                    }),
                );
            }
            pairs.push((key, value));
            i = j;
        }
    }
}

/// One complete parameter. `string="true"` is recoverable from the JSON type:
/// strings pass through verbatim, everything else is re-dumped.
fn param_line(key: &str, value: &Ordered) -> String {
    match value {
        Ordered::Str(s) => format!("{PARAMETER_OPEN}{key}\" string=\"true\">{s}{PARAMETER_CLOSE}"),
        other => format!("{PARAMETER_OPEN}{key}\" string=\"false\">{}{PARAMETER_CLOSE}", py_json(other)),
    }
}

/// One tool call → its DSML invoke block, tolerating truncated arguments.
///
/// With `partial = false` the block is closed regardless; with `partial = true`
/// it is closed only when the arguments are complete JSON — a mid-flight call
/// gets no `</｜DSML｜invoke>` (rule 2).
fn encode_tool_call(name: Option<&str>, arguments: &str, partial: bool) -> String {
    let Some(name) = name else {
        return String::new();
    };
    let open = format!("{INVOKE_OPEN}{name}\">");
    let (pairs, tail) = parse_partial_args(arguments);
    let mut lines: Vec<String> = pairs.iter().map(|(k, v)| param_line(k, v)).collect();

    let closed = arguments.trim_end().ends_with('}') && tail.is_none();
    if !partial || !closed {
        match tail {
            Some(Tail::Key(p)) => lines.push(format!("{PARAMETER_OPEN}{p}")),
            Some(Tail::KeyDone(k)) => lines.push(format!("{PARAMETER_OPEN}{k}\"")),
            Some(Tail::Value { key, is_string, frag }) => {
                let flag = if is_string { "true" } else { "false" };
                lines.push(format!("{PARAMETER_OPEN}{key}\" string=\"{flag}\">{frag}"));
            }
            None => {}
        }
    }

    let body = lines.join("\n");
    if closed || !partial {
        return format!("{open}\n{body}\n{INVOKE_CLOSE}");
    }
    if body.is_empty() { open } else { format!("{open}\n{body}") }
}

/// Restore the structurally-implied newline when a prefix ends on a DSML closing
/// tag (rule 1). Well-formed DSML never ends a generation on those tags without
/// a newline, so this adds no ambiguity — and because the newline is one the
/// next cut also emits, it preserves monotonicity.
fn seam_safe(text: String) -> String {
    if CLOSING_TAGS.iter().any(|t| text.ends_with(t)) {
        return text + "\n";
    }
    text
}

// ── The accumulator ──────────────────────────────────────────────────────────

/// One tool call's accumulated fragments, keyed by the delta's `index`.
struct ToolSlot {
    index: i64,
    name: Option<String>,
    arguments: String,
}

/// [`StreamAccumulator`] for the DeepSeek-V4 (DSML) family: reasoning and
/// tool-call deltas are re-serialized into the raw sequence the model sampled,
/// so a death mid-reasoning or mid-tool-call stays resumable.
///
/// Selected per model by `continuation.model_reconstructors` (value `dsv4`);
/// its thinking/chat mode comes from the route's `render_kwargs`.
pub struct Dsv4Reconstructor {
    reasoning: String,
    content: String,
    tools: Vec<ToolSlot>,
    saw_any_tool_frame: bool,
    /// Generation began inside an open `<think>` — the rendered default for this
    /// family, and what the resume render reproduces.
    thinking: bool,
    cap: usize,
    envelope: Option<Envelope>,
    finish_reason: bool,
    disarmed: Option<AccumulateError>,
}

impl Dsv4Reconstructor {
    /// `thinking` says whether the resume prompt will end inside an open
    /// `<think>` — i.e. whether the reconstruction must close it. It is derived
    /// from the route's `render_kwargs` (see [`super::RouteInfo::thinking`]),
    /// because that is what the prefix is rendered with: tokenizer-svc renders
    /// this family in thinking mode by default, but a route serving it in chat
    /// mode ends `</think>` already and must not get a second one. The mode
    /// cannot be inferred from the deltas — a thinking-mode turn that does no
    /// thinking emits `</think>` first with no `reasoning_content` at all, which
    /// is exactly the `plat-reasoning` fixture.
    pub fn new(cap: usize, thinking: bool) -> Self {
        Self {
            reasoning: String::new(),
            content: String::new(),
            tools: Vec::new(),
            saw_any_tool_frame: false,
            thinking,
            cap,
            envelope: None,
            finish_reason: false,
            disarmed: None,
        }
    }

    /// A leg that ran in chat mode (prompt already ended `</think>`), where no
    /// think tag must be closed.
    #[cfg(test)]
    fn chat_mode(cap: usize) -> Self {
        Self::new(cap, false)
    }

    fn disarm(&mut self, cause: AccumulateError) -> Result<(), AccumulateError> {
        self.reasoning = String::new();
        self.content = String::new();
        self.tools = Vec::new();
        let cause = *self.disarmed.get_or_insert(cause);
        Err(cause)
    }

    /// Reserve `extra` bytes against the cap, or report the overrun.
    fn fits(&self, extra: usize) -> bool {
        self.len_bytes() + extra <= self.cap
    }

    fn slot(&mut self, index: i64) -> &mut ToolSlot {
        if let Some(pos) = self.tools.iter().position(|t| t.index == index) {
            return &mut self.tools[pos];
        }
        self.tools.push(ToolSlot {
            index,
            name: None,
            arguments: String::new(),
        });
        self.tools.last_mut().expect("just pushed")
    }

    /// Where in the DSML sequence the resume leg's first token lands.
    ///
    /// This is the same state [`Self::reconstruct`] renders the tail of the
    /// prefix from, read out instead of written down — which is the point:
    /// the forward parser must believe exactly what the prefix says, or the
    /// resumed text is interpreted in a structure the model is not in. Each
    /// arm below is the inverse of one branch of [`encode_tool_call`]:
    ///
    /// | prefix ends … | seed |
    /// |---|---|
    /// | mid-reasoning, `<think>` still open | [`ForwardSeed::Reasoning`] |
    /// | in the body, before any tool frame | [`ForwardSeed::Content`] |
    /// | `<｜DSML｜tool_calls>` / a closed `</｜DSML｜invoke>` | [`ForwardSeed::BetweenToolCalls`] |
    /// | inside an invoke (open tag, partial name, partial value) | [`ForwardSeed::InToolCall`] |
    ///
    /// The last two are told apart by exactly the test `encode_tool_call` uses
    /// to decide whether to close the invoke — complete-JSON arguments — so a
    /// prefix that ends `</｜DSML｜invoke>` can never be seeded as if it were
    /// still inside that call.
    pub fn forward_seed(&self) -> ForwardSeed {
        if !self.saw_any_tool_frame {
            // A `</think>` is in the prefix iff the body started, so anything
            // else is still inside the think block — but only a thinking-mode
            // leg HAS one to close; a chat-mode prompt already ended with it.
            let started_body = !self.content.is_empty();
            return if self.thinking && !started_body {
                ForwardSeed::Reasoning
            } else {
                ForwardSeed::Content
            };
        }

        let Some(last) = self.tools.last() else {
            return ForwardSeed::BetweenToolCalls { next_index: 0 };
        };
        let index = u32::try_from(last.index).unwrap_or(0);
        // A slot with no name renders as nothing at all (see `encode_tool_call`),
        // so the prefix stops before this call — but its index is already spent
        // as far as the client is concerned, so the next invoke reuses it rather
        // than restarting the numbering.
        if last.name.is_none() {
            return ForwardSeed::BetweenToolCalls { next_index: index };
        }
        let (_, tail) = parse_partial_args(&last.arguments);
        let closed = last.arguments.trim_end().ends_with('}') && tail.is_none();
        if closed || self.finish_reason {
            ForwardSeed::BetweenToolCalls {
                next_index: index.saturating_add(1),
            }
        } else {
            ForwardSeed::InToolCall {
                index,
                args_so_far: last.arguments.clone(),
            }
        }
    }

    /// Rebuild the raw emitted text from the accumulated channels.
    fn reconstruct(&self) -> String {
        let mut out = String::new();
        // A tool frame counts as the body starting even with no content: the
        // separator and the block both live in the body channel.
        let started_body = !self.content.is_empty() || self.saw_any_tool_frame;

        out.push_str(&self.reasoning);
        if self.thinking && started_body {
            out.push_str(THINK_END);
        }
        out.push_str(&self.content);

        if !self.saw_any_tool_frame {
            return out;
        }

        // Rule 3: the separator is parser-dependent, so inject only what is missing.
        if !self.content.ends_with(TOOL_BLOCK_SEPARATOR) {
            out.push_str(TOOL_BLOCK_SEPARATOR);
        }
        out.push_str(TOOL_CALLS_OPEN);
        out.push('\n');

        // Rule 2: only a terminal frame may close the block. Until then the LAST
        // invoke is treated as still in flight (earlier ones are complete by
        // construction — a sibling only appears once its predecessor ended).
        let terminal = self.finish_reason;
        let last = self.tools.len().saturating_sub(1);
        let blocks: Vec<String> = self
            .tools
            .iter()
            .enumerate()
            .map(|(i, slot)| {
                let partial = i == last && !terminal;
                let mut block = encode_tool_call(slot.name.as_deref(), &slot.arguments, partial);
                // The name arrived but no arguments yet: the model has already
                // emitted the newline that follows the open tag.
                if !block.is_empty() && !block.contains('\n') && block.ends_with("\">") {
                    block.push('\n');
                }
                block
            })
            .collect();

        let joined = blocks.iter().filter(|b| !b.is_empty()).cloned().collect::<Vec<_>>().join("\n");
        out.push_str(&joined);
        if terminal && blocks.last().is_some_and(|b| b.ends_with(INVOKE_CLOSE)) {
            out.push('\n');
            out.push_str(TOOL_CALLS_CLOSE);
        }
        out
    }
}

impl StreamAccumulator for Dsv4Reconstructor {
    fn ingest(&mut self, chunk: &Value) -> Result<(), AccumulateError> {
        if let Some(cause) = self.disarmed {
            return Err(cause);
        }
        capture_envelope(&mut self.envelope, chunk);

        let choice = match single_choice(chunk) {
            Ok(Some(choice)) => choice,
            Ok(None) => return Ok(()),
            // Sticky, like PlainContent: after a multi-choice frame the
            // accumulated prefix can interleave generations — later frames
            // must not quietly re-arm it.
            Err(cause) => return self.disarm(cause),
        };
        if choice.get("finish_reason").is_some_and(|f| !f.is_null()) {
            self.finish_reason = true;
        }
        let Some(delta) = choice.get("delta").and_then(Value::as_object) else {
            return Ok(());
        };

        // `reasoning_details` is an OpenRouter envelope around the same text as
        // `reasoning_content` (`format: "unknown"`, carrying no syntax hint), so
        // it is ignored rather than accumulated — double-counting it would
        // duplicate the whole reasoning channel. Two shapes still disarm, as they
        // do for PlainContent: `function_call`, the legacy single-call encoding
        // this reconstructor has never been measured against, and a bare
        // `reasoning` with no `reasoning_content` alongside it, which is
        // reasoning text we have no measured position for in the sequence.
        let present = |k: &str| delta.get(k).is_some_and(|v| !v.is_null());
        if present("function_call") || (present("reasoning") && !present("reasoning_content")) {
            super::metrics::record_unsupported_delta(if present("function_call") { "function_call" } else { "reasoning" });
            return self.disarm(AccumulateError::UnsupportedDelta);
        }

        if let Some(r) = delta.get("reasoning_content").and_then(Value::as_str) {
            if !self.fits(r.len()) {
                return self.disarm(AccumulateError::CapExceeded);
            }
            self.reasoning.push_str(r);
        }
        if let Some(c) = delta.get("content").and_then(Value::as_str) {
            if !self.fits(c.len()) {
                return self.disarm(AccumulateError::CapExceeded);
            }
            self.content.push_str(c);
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                self.saw_any_tool_frame = true;
                let index = call.get("index").and_then(Value::as_i64).unwrap_or(0);
                let function = call.get("function");
                let name = function
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .filter(|n| !n.is_empty())
                    .map(str::to_string);
                let args = function
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                    .filter(|a| !a.is_empty())
                    .unwrap_or_default()
                    .to_string();
                if !self.fits(name.as_ref().map_or(0, String::len) + args.len()) {
                    return self.disarm(AccumulateError::CapExceeded);
                }
                // Reconstruction assumes tool calls arrive serially — a sibling
                // only appears once its predecessor ended, so only the LAST
                // slot can be partial. A fragment returning to an earlier slot
                // (interleaved parallel calls — never produced by a faithful
                // DSML parser) would encode that slot as complete with
                // truncated arguments, so disarm instead of corrupting the
                // prefix.
                if self.tools.last().is_some_and(|last| last.index != index) && self.tools.iter().any(|t| t.index == index) {
                    super::metrics::record_unsupported_delta("tool_calls");
                    return self.disarm(AccumulateError::UnsupportedDelta);
                }
                let slot = self.slot(index);
                if name.is_some() {
                    slot.name = name;
                }
                slot.arguments.push_str(&args);
            }
        }
        Ok(())
    }

    fn continuation_text(&self) -> Option<String> {
        if self.disarmed.is_some() {
            return None;
        }
        let text = seam_safe(self.reconstruct());
        // Nothing generated yet is deliberately NOT resumable: resume-from-zero
        // is a plain retry, which is not this feature's job.
        (!text.is_empty()).then_some(text)
    }

    fn len_bytes(&self) -> usize {
        self.reasoning.len()
            + self.content.len()
            + self
                .tools
                .iter()
                .map(|t| t.name.as_ref().map_or(0, String::len) + t.arguments.len())
                .sum::<usize>()
    }

    fn envelope(&self) -> Option<&Envelope> {
        self.envelope.as_ref()
    }

    fn saw_finish_reason(&self) -> bool {
        self.finish_reason
    }

    fn disarmed(&self) -> Option<AccumulateError> {
        self.disarmed
    }

    fn disarm_externally(&mut self, cause: AccumulateError) {
        let _ = self.disarm(cause);
    }

    /// Plain reframing is faithful only when the continuation can produce
    /// nothing but content: no tool syntax anywhere in the turn, and the
    /// think block (if the turn has one) already closed — for this family
    /// content only begins after `</think>`, so non-empty content is that
    /// proof. Anything else goes through the paired forward parser below.
    fn plain_resume_ok(&self) -> bool {
        !self.saw_any_tool_frame && (!self.thinking || !self.content.is_empty())
    }

    /// The DSML parser, seeded from this reconstructor's death-point state.
    /// Pairing them here is what lifts the reasoning/tool disarm for this
    /// family and nothing else.
    fn forward_parser(&self) -> Box<dyn ForwardParser> {
        Box::new(Dsv4Forward::new(self.forward_seed()))
    }

    /// Role repair rides the family reconstructor, alongside its parser: this
    /// family's captures include a provider that delivers `role` only on a
    /// late frame, so a rescue can otherwise leave the message roleless.
    fn repairs_role(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod forward_tests;
