//! DeepSeek-V4 (DSML) forward parsing: the resume leg's raw text → chat deltas.
//!
//! The exact inverse of [`super::Dsv4Reconstructor`], and deliberately written
//! against the same grammar constants, so the two cannot drift: everything the
//! reconstructor emits as structure (`</think>`, `<｜DSML｜tool_calls>`, the
//! invoke/parameter tags, the newlines between them) this parser consumes as
//! structure and never shows the client, and everything the reconstructor takes
//! from a channel this parser puts back into that channel.
//!
//! ```text
//! reasoning … </think>content …\n\n<｜DSML｜tool_calls>
//! <｜DSML｜invoke name="NAME">
//! <｜DSML｜parameter name="K" string="true">V</｜DSML｜parameter>
//! </｜DSML｜invoke>
//! </｜DSML｜tool_calls>
//! ```
//!
//! Three things make this more than a `split()`:
//!
//! 1. **It starts mid-structure.** A resume leg begins wherever leg 1 died, so
//!    the initial state is a [`ForwardSeed`] taken from the accumulator, not
//!    inferred from the text. Everything downstream of the seed — which tool
//!    index the next invoke gets, whether a `{` or a `, ` is still owed on the
//!    arguments object, whether the value being read is a JSON string — is
//!    derived from the same partial-arguments parse that rendered the prefix.
//! 2. **Tags split across chunks.** A chunk may end at `</thi`. Any suffix of
//!    the buffer that is a proper prefix of a tag the current state is looking
//!    for is held back, and released as ordinary text the moment the next chunk
//!    disproves it (or by [`ForwardParser::finish`]). Nothing is ever emitted
//!    out of order and nothing is dropped: the cumulative emitted channels,
//!    re-serialized by the reconstructor, equal the cumulative input bytes.
//! 3. **The arguments object is rebuilt as JSON.** DSML carries a tool call as
//!    per-parameter tags with a `string="true|false"` attribute; the client
//!    expects one growing JSON object. Parameter names and `string="true"`
//!    values are JSON-escaped on the way in; `string="false"` values are already
//!    JSON and pass through verbatim. Because the reconstructor re-dumps with
//!    Python's separators, an argument fragment we emit and one it re-reads land
//!    on the same bytes.
//!
//! **What is deliberately dropped.** Structural whitespace between tags (the
//! reconstructor regenerates it exactly), and a partial `<｜DSML｜invoke name="`
//! whose name never completed — that call was never announced to the client and
//! never entered the accumulator, so dropping it keeps both sides agreeing that
//! it does not exist. Everything else that cannot be placed is emitted as
//! content rather than lost.

use uuid::Uuid;

use crate::continuation::forward::{ForwardDelta, ForwardParser, ForwardSeed};

use super::{
    INVOKE_CLOSE, INVOKE_OPEN, PARAMETER_CLOSE, PARAMETER_OPEN, THINK_END, TOOL_CALLS_CLOSE, TOOL_CALLS_OPEN, Tail,
    parse_partial_args,
};

/// Closes an invoke's open tag (`…name="NAME">`) and a parameter's
/// `string="…"` attribute alike.
const TAG_END: &str = "\">";
/// Closes a parameter NAME, mid-tag: `<｜DSML｜parameter name="K` ends here.
const NAME_END: &str = "\"";

/// Which part of the grammar the next byte belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Before `</think>`: reasoning channel.
    Reasoning,
    /// The body, before the tool block opens: content channel.
    Content,
    /// Inside the tool block, between invokes.
    Block,
    /// Reading an invoke's name, up to the `">` that closes its open tag.
    InvokeName,
    /// Inside an invoke, between parameters.
    Invoke,
    /// Reading a parameter name, up to its closing quote.
    ParamName,
    /// Reading the ` string="…">` attribute that follows a parameter name.
    ParamAttr,
    /// Reading a parameter value, up to `</｜DSML｜parameter>`.
    ParamValue,
    /// After `</｜DSML｜tool_calls>`. Well-formed DSML ends here.
    AfterBlock,
}

/// The DSML/think state machine.
pub struct Dsv4Forward {
    state: State,
    /// Bytes withheld because they may be the start of a tag this state is
    /// looking for. Bounded by the longest such tag, except in the two
    /// structural scans (an invoke name, a parameter attribute) where it is
    /// bounded by the leg's own generation.
    hold: String,
    /// The invoke name being read. Buffered rather than streamed because the
    /// reconstructor's slot REPLACES a name instead of appending to it, so a
    /// fragmented name would not survive the round trip.
    name: String,
    /// Arguments text for the current call that has not been emitted yet,
    /// coalesced so one `feed` yields at most one tool-call delta per call.
    pending: String,
    /// The call currently being read.
    index: u32,
    /// The index the next invoke opens at — seeded, never restarted at zero.
    next_index: u32,
    /// `{` has been emitted for the current call's arguments.
    opened: bool,
    /// At least one parameter is complete, so the next one owes a `, `.
    params: bool,
    /// The value being read is a `string="true"` one, so it is JSON-escaped and
    /// wrapped in quotes.
    is_string: bool,
}

impl Dsv4Forward {
    /// Build a parser positioned at the death point the accumulator reports.
    pub fn new(seed: ForwardSeed) -> Self {
        let mut parser = Self {
            state: State::Content,
            hold: String::new(),
            name: String::new(),
            pending: String::new(),
            index: 0,
            next_index: 0,
            opened: false,
            params: false,
            is_string: false,
        };
        match seed {
            ForwardSeed::Reasoning => parser.state = State::Reasoning,
            ForwardSeed::Content => parser.state = State::Content,
            ForwardSeed::BetweenToolCalls { next_index } => {
                parser.state = State::Block;
                parser.next_index = next_index;
            }
            ForwardSeed::InToolCall { index, args_so_far } => {
                parser.index = index;
                parser.next_index = index + 1;
                // What the client already holds decides what is still owed:
                // an opening brace, a separator before the next parameter, and
                // which half of a parameter is in flight.
                parser.opened = args_so_far.trim_start().starts_with('{');
                let (pairs, tail) = parse_partial_args(&args_so_far);
                parser.params = !pairs.is_empty();
                parser.state = match tail {
                    None => State::Invoke,
                    Some(Tail::Key(_)) => State::ParamName,
                    Some(Tail::KeyDone(_)) => State::ParamAttr,
                    Some(Tail::Value { is_string, .. }) => {
                        parser.is_string = is_string;
                        State::ParamValue
                    }
                };
            }
        }
        parser
    }

    /// A tool-call id in the scheme the serving parser uses, taken from the
    /// fidelity captures: `call_` plus 24 lowercase hex characters. Only ever
    /// minted for a call that OPENS in the resumed leg — a call leg 1 already
    /// announced keeps the id the client was given.
    fn new_id() -> String {
        let hex = Uuid::new_v4().simple().to_string();
        format!("call_{}", &hex[..24])
    }

    /// Queue arguments text for the current call.
    fn push_args(&mut self, text: &str) {
        self.pending.push_str(text);
    }

    /// Emit whatever arguments text is queued. Called before any other delta and
    /// at the end of every `feed`/`finish`, so ordering across channels is
    /// preserved.
    fn flush_args(&mut self, out: &mut Vec<ForwardDelta>) {
        if self.pending.is_empty() {
            return;
        }
        out.push(ForwardDelta::ToolCall {
            index: self.index,
            id: None,
            name: None,
            arguments: std::mem::take(&mut self.pending),
        });
    }

    fn emit(&mut self, out: &mut Vec<ForwardDelta>, delta: ForwardDelta) {
        self.flush_args(out);
        out.push(delta);
    }

    /// Text in a position where only whitespace and tags are legal. Whitespace
    /// is structure (the reconstructor regenerates it); anything else is a shape
    /// this parser has never been measured against, and is surfaced as content
    /// rather than swallowed.
    fn structural(&mut self, out: &mut Vec<ForwardDelta>, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        self.emit(out, ForwardDelta::Content(text.to_string()));
    }

    /// One step of the machine over the front of `rest`, returning how many
    /// bytes it consumed. Zero means "blocked until more input arrives".
    fn advance(&mut self, rest: &str, out: &mut Vec<ForwardDelta>) -> usize {
        match self.state {
            State::Reasoning => self.scan_text(rest, out, THINK_END, State::Content, ForwardDelta::Reasoning),
            State::Content => self.scan_text(rest, out, TOOL_CALLS_OPEN, State::Block, ForwardDelta::Content),
            State::AfterBlock => {
                // Nothing structural is expected after the block closes; hold
                // nothing back and hand it to the client as content.
                self.emit(out, ForwardDelta::Content(rest.to_string()));
                rest.len()
            }
            State::Block => self.scan_block(rest, out),
            State::InvokeName => self.scan_invoke_name(rest, out),
            State::Invoke => self.scan_invoke(rest, out),
            State::ParamName => self.scan_param_name(rest),
            State::ParamAttr => self.scan_param_attr(rest),
            State::ParamValue => self.scan_param_value(rest),
        }
    }

    /// A channel that runs until one closing tag: emit everything before it,
    /// swallow the tag, switch state.
    fn scan_text(
        &mut self,
        rest: &str,
        out: &mut Vec<ForwardDelta>,
        tag: &'static str,
        next: State,
        channel: fn(String) -> ForwardDelta,
    ) -> usize {
        if let Some(i) = rest.find(tag) {
            if i > 0 {
                self.emit(out, channel(rest[..i].to_string()));
            }
            self.state = next;
            return i + tag.len();
        }
        let emittable = rest.len() - hold_len(rest, &[tag]);
        if emittable > 0 {
            self.emit(out, channel(rest[..emittable].to_string()));
        }
        emittable
    }

    fn scan_block(&mut self, rest: &str, out: &mut Vec<ForwardDelta>) -> usize {
        const TAGS: [&str; 2] = [INVOKE_OPEN, TOOL_CALLS_CLOSE];
        if let Some((i, tag)) = find_first(rest, &TAGS) {
            self.structural(out, &rest[..i]);
            // The previous call's closing `}` can still be queued here — one
            // chunk may carry an invoke's end and the next one's start — and it
            // belongs to THAT call's index, not the one about to open.
            self.flush_args(out);
            if tag == INVOKE_OPEN {
                self.open_call();
            } else {
                self.state = State::AfterBlock;
            }
            return i + tag.len();
        }
        let emittable = rest.len() - hold_len(rest, &TAGS);
        self.structural(out, &rest[..emittable]);
        emittable
    }

    /// Start reading a new invoke. The index continues leg 1's numbering.
    fn open_call(&mut self) {
        self.state = State::InvokeName;
        self.name.clear();
        self.index = self.next_index;
        self.next_index = self.index.saturating_add(1);
        self.opened = false;
        self.params = false;
    }

    fn scan_invoke_name(&mut self, rest: &str, out: &mut Vec<ForwardDelta>) -> usize {
        if let Some(i) = rest.find(TAG_END) {
            self.name.push_str(&rest[..i]);
            let name = std::mem::take(&mut self.name);
            self.flush_args(out);
            out.push(ForwardDelta::ToolCall {
                index: self.index,
                id: Some(Self::new_id()),
                name: Some(name),
                arguments: String::new(),
            });
            self.state = State::Invoke;
            return i + TAG_END.len();
        }
        // A name cannot contain `"`, so only a trailing quote is ambiguous.
        let emittable = rest.len() - hold_len(rest, &[TAG_END]);
        self.name.push_str(&rest[..emittable]);
        emittable
    }

    fn scan_invoke(&mut self, rest: &str, out: &mut Vec<ForwardDelta>) -> usize {
        const TAGS: [&str; 2] = [PARAMETER_OPEN, INVOKE_CLOSE];
        if let Some((i, tag)) = find_first(rest, &TAGS) {
            self.structural(out, &rest[..i]);
            let mut frag = String::new();
            if !self.opened {
                frag.push('{');
                self.opened = true;
            }
            if tag == PARAMETER_OPEN {
                if self.params {
                    frag.push_str(", ");
                }
                frag.push('"');
                self.state = State::ParamName;
            } else {
                frag.push('}');
                self.state = State::Block;
            }
            self.push_args(&frag);
            return i + tag.len();
        }
        let emittable = rest.len() - hold_len(rest, &TAGS);
        self.structural(out, &rest[..emittable]);
        emittable
    }

    fn scan_param_name(&mut self, rest: &str) -> usize {
        if let Some(i) = rest.find(NAME_END) {
            let key = escape(&rest[..i]);
            self.push_args(&key);
            // The key's own closing quote is owed now; the `: ` that follows it
            // waits until the `string="…"` attribute says whether the value
            // opens a JSON string.
            self.push_args("\"");
            self.state = State::ParamAttr;
            return i + NAME_END.len();
        }
        let escaped = escape(rest);
        self.push_args(&escaped);
        rest.len()
    }

    fn scan_param_attr(&mut self, rest: &str) -> usize {
        let Some(i) = rest.find(TAG_END) else {
            return 0;
        };
        // ` string="true` / ` string="false` — an unrecognised flag is treated
        // as a string, which keeps the arguments valid JSON either way.
        let flag = rest[..i].rsplit('"').next().unwrap_or("");
        self.is_string = !flag.eq_ignore_ascii_case("false");
        self.push_args(if self.is_string { ": \"" } else { ": " });
        self.state = State::ParamValue;
        i + TAG_END.len()
    }

    fn scan_param_value(&mut self, rest: &str) -> usize {
        if let Some(i) = rest.find(PARAMETER_CLOSE) {
            self.push_value(&rest[..i]);
            if self.is_string {
                self.push_args("\"");
            }
            self.params = true;
            self.state = State::Invoke;
            return i + PARAMETER_CLOSE.len();
        }
        let emittable = rest.len() - hold_len(rest, &[PARAMETER_CLOSE]);
        self.push_value(&rest[..emittable]);
        emittable
    }

    /// A `string="true"` value is JSON-escaped; a `string="false"` one already
    /// IS JSON and goes through verbatim, which is what makes a non-scalar
    /// argument re-serialize to the bytes the model wrote.
    fn push_value(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.is_string {
            let escaped = escape(text);
            self.push_args(&escaped);
        } else {
            self.push_args(text);
        }
    }
}

impl ForwardParser for Dsv4Forward {
    fn feed(&mut self, raw: &str) -> Vec<ForwardDelta> {
        let mut out = Vec::new();
        let mut buf = std::mem::take(&mut self.hold);
        buf.push_str(raw);

        let mut pos = 0usize;
        while pos < buf.len() {
            let consumed = self.advance(&buf[pos..], &mut out);
            if consumed == 0 {
                break;
            }
            pos += consumed;
        }
        self.hold = buf[pos..].to_string();
        self.flush_args(&mut out);
        out
    }

    fn finish(&mut self) -> Vec<ForwardDelta> {
        let mut out = Vec::new();
        let hold = std::mem::take(&mut self.hold);
        match self.state {
            State::Reasoning if !hold.is_empty() => self.emit(&mut out, ForwardDelta::Reasoning(hold)),
            State::Content | State::AfterBlock if !hold.is_empty() => self.emit(&mut out, ForwardDelta::Content(hold)),
            State::Block | State::Invoke => self.structural(&mut out, &hold),
            State::ParamName => {
                let escaped = escape(&hold);
                self.push_args(&escaped);
            }
            State::ParamValue => self.push_value(&hold),
            // A half-written invoke open tag or `string="…"` attribute is
            // structure the client was never shown and the accumulator never
            // recorded; it dies with the leg rather than leaking as text.
            State::InvokeName | State::ParamAttr => {}
            _ => {}
        }
        self.flush_args(&mut out);
        out
    }
}

/// The earliest occurrence of any of `tags`, preferring the longest tag on a tie.
fn find_first(rest: &str, tags: &[&'static str]) -> Option<(usize, &'static str)> {
    tags.iter()
        .filter_map(|tag| rest.find(tag).map(|i| (i, *tag)))
        .min_by_key(|(i, tag)| (*i, std::cmp::Reverse(tag.len())))
}

/// How many trailing bytes of `rest` must be withheld because they could still
/// grow into one of `tags`.
///
/// Called only when no tag matches outright, so the suffix in question is always
/// a PROPER prefix. The scan starts at most `longest tag - 1` bytes from the end
/// and skips non-boundary offsets, so a multi-byte character (every DSML tag
/// contains `｜`, U+FF5C) is never split.
fn hold_len(rest: &str, tags: &[&str]) -> usize {
    let longest = tags.iter().map(|t| t.len()).max().unwrap_or(0);
    let start = rest.len().saturating_sub(longest.saturating_sub(1));
    for i in start..rest.len() {
        if !rest.is_char_boundary(i) {
            continue;
        }
        let tail = &rest[i..];
        if tags.iter().any(|tag| tag.starts_with(tail)) {
            return rest.len() - i;
        }
    }
    0
}

/// The body of a JSON string literal — `serde_json`'s encoding with the
/// surrounding quotes removed, so fragments concatenate. Non-ASCII is not
/// escaped, matching what the reconstructor re-dumps.
fn escape(text: &str) -> String {
    let mut encoded = serde_json::to_string(text).unwrap_or_else(|_| format!("\"{text}\""));
    encoded.pop();
    if !encoded.is_empty() {
        encoded.remove(0);
    }
    encoded
}
