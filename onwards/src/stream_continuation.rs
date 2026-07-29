use axum::{
    body::Body,
    http::{
        HeaderMap, Method,
        header::{CONTENT_ENCODING, CONTENT_TYPE},
    },
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::client::HttpClient;
use crate::handlers::{UpstreamRequestMetadata, build_upstream_request};
use crate::load_balancer::{ProviderPool, SelectionState};
use crate::sse::{
    CheckedSseStream, ParsedSseEvent, SseFramingError, SseStreamError, framing_error_in_chain,
    parse_sse_event,
};
use crate::target::ConcurrencyGuard;
use crate::target::StreamContinuationConfig;

const CONTINUATION_MAX_SSE_EVENT_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn event_buffer_size() -> usize {
    CONTINUATION_MAX_SSE_EVENT_BYTES
}

/// State retained while forwarding an eligible text-generation stream.
pub struct StreamContinuation {
    request: Value,
    protocol: ContinuationProtocol,
    generated_text: String,
    max_buffered_bytes: usize,
    continuable: bool,
    terminal: bool,
}

enum ContinuationProtocol {
    Completion(TextStreamIdentity),
    Chat(TextStreamIdentity),
    Responses(ResponsesStreamState),
}

struct TextStreamIdentity {
    id: Option<Value>,
    model: Option<Value>,
    created: Option<Value>,
    identity_established: bool,
}

struct ResponsesStreamState {
    response_id: String,
    model: String,
    item_id: String,
    response_identity_established: bool,
    item_identity_established: bool,
    last_sequence: Option<u64>,
    emit_event_field: Option<bool>,
    continuation_prefix_len: Option<usize>,
}

pub(crate) fn is_event_stream(headers: &HeaderMap) -> bool {
    let mut content_types = headers.get_all(CONTENT_TYPE).iter();
    let (Some(content_type), None) = (content_types.next(), content_types.next()) else {
        return false;
    };
    content_type
        .to_str()
        .ok()
        .and_then(|value| {
            value
                .parse::<mime::Mime>()
                .ok()
                .map(|media_type| (value, media_type))
        })
        .is_some_and(|(raw_value, media_type)| {
            media_type.type_() == mime::TEXT
                && media_type
                    .subtype()
                    .as_str()
                    .eq_ignore_ascii_case("event-stream")
                && (!raw_value.contains(';') || media_type.params().next().is_some())
                && media_type
                    .params()
                    .all(|(_, value)| !value.as_str().is_empty())
        })
}

pub(crate) fn has_identity_content_encoding(headers: &HeaderMap) -> bool {
    let mut encodings = headers.get_all(CONTENT_ENCODING).iter();
    match (encodings.next(), encodings.next()) {
        (None, None) => true,
        (Some(value), None) => value
            .to_str()
            .is_ok_and(|encoding| encoding.trim().eq_ignore_ascii_case("identity")),
        _ => false,
    }
}

/// The forwarded SSE event and whether the stream reached a terminal state.
pub struct EventObservation {
    pub event: Bytes,
    pub forward: bool,
    pub terminal: bool,
    pub done: bool,
    pub safe: bool,
    pub accepted: bool,
}

/// Errors raised while building a continuation request or rewriting an event.
#[derive(Debug)]
pub enum ContinuationError {
    NotContinuable,
    Serialize(serde_json::Error),
}

impl fmt::Display for ContinuationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotContinuable => write!(f, "generation stream cannot be continued"),
            Self::Serialize(_) => write!(f, "failed to serialize stream continuation data"),
        }
    }
}

impl std::error::Error for ContinuationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotContinuable => None,
            Self::Serialize(error) => Some(error),
        }
    }
}

impl StreamContinuation {
    /// Creates protocol state when a request can be safely resumed from text output.
    pub fn from_request(
        path: &str,
        method: &Method,
        body: &[u8],
        config: &StreamContinuationConfig,
    ) -> Option<Self> {
        Self::from_request_with_resolved_model(path, path, method, body, config, None)
    }

    pub(crate) fn from_request_with_resolved_model(
        eligible_path: &str,
        protocol_path: &str,
        method: &Method,
        body: &[u8],
        config: &StreamContinuationConfig,
        resolved_model: Option<&str>,
    ) -> Option<Self> {
        if method != Method::POST || !config.enabled_for_path(eligible_path) {
            return None;
        }

        let request: Value = serde_json::from_slice(body).ok()?;
        let request_object = request.as_object()?;
        let fallback_model = resolved_model
            .map(|model| Value::String(model.to_owned()))
            .or_else(|| request.get("model").cloned())
            .unwrap_or_else(|| Value::String("unknown".to_string()));
        let fallback_created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let protocol = match protocol_path {
            "/v1/completions" if eligible_completion_request(request_object) => {
                ContinuationProtocol::Completion(TextStreamIdentity::new(
                    "cmpl",
                    fallback_model,
                    fallback_created,
                ))
            }
            "/v1/chat/completions" if eligible_chat_request(request_object) => {
                ContinuationProtocol::Chat(TextStreamIdentity::new(
                    "chatcmpl",
                    fallback_model,
                    fallback_created,
                ))
            }
            "/v1/responses" if eligible_responses_request(request_object) => {
                ContinuationProtocol::Responses(ResponsesStreamState::new(
                    fallback_model.as_str().unwrap_or("unknown").to_string(),
                ))
            }
            _ => return None,
        };
        Some(Self {
            request,
            protocol,
            generated_text: String::new(),
            max_buffered_bytes: config.max_buffered_bytes,
            continuable: true,
            terminal: false,
        })
    }

    /// Records a complete SSE event and optionally normalizes its stream identity.
    pub fn observe_event(
        &mut self,
        event: &[u8],
        rewrite_identity: bool,
    ) -> Result<EventObservation, ContinuationError> {
        let (data, event_type) = match parse_sse_event(event) {
            ParsedSseEvent::Comment => {
                return Ok(EventObservation {
                    event: Bytes::copy_from_slice(event),
                    forward: true,
                    terminal: self.terminal,
                    done: false,
                    safe: true,
                    accepted: false,
                });
            }
            ParsedSseEvent::Data { data, event_type } => (data, event_type),
            ParsedSseEvent::Invalid => {
                return Ok(self.reject_event(event));
            }
        };

        let Ok(data) = std::str::from_utf8(&data) else {
            return Ok(self.reject_event(event));
        };
        if data == "[DONE]" {
            if event_type.is_some() {
                return Ok(self.reject_event(event));
            }
            self.terminal = true;
            return Ok(EventObservation {
                event: Bytes::copy_from_slice(event),
                forward: true,
                terminal: true,
                done: true,
                safe: true,
                accepted: true,
            });
        }

        let Ok(mut value) = serde_json::from_str::<Value>(data) else {
            return Ok(self.reject_event(event));
        };
        let outcome = match &mut self.protocol {
            ContinuationProtocol::Completion(identity) => observe_completion_event(
                identity,
                &mut value,
                event_type.as_deref(),
                rewrite_identity,
            ),
            ContinuationProtocol::Chat(identity) => observe_chat_event(
                identity,
                &mut value,
                event_type.as_deref(),
                rewrite_identity,
            ),
            ContinuationProtocol::Responses(state) => observe_responses_event(
                state,
                &mut value,
                event_type.as_deref(),
                rewrite_identity,
                &self.generated_text,
            ),
        };
        let ProtocolObservation {
            text,
            terminal,
            accepted,
            forward,
            event,
        } = match outcome {
            Some(outcome) => outcome,
            None => return Ok(self.reject_event(event)),
        };

        if let Some(text) = text.as_deref() {
            append_generated_text(
                &mut self.generated_text,
                &mut self.continuable,
                self.max_buffered_bytes,
                text,
            );
        }
        self.terminal |= terminal;

        Ok(EventObservation {
            event,
            forward,
            terminal: self.terminal,
            done: false,
            safe: true,
            accepted,
        })
    }

    /// Returns whether an interrupted response may issue another generation request.
    pub fn is_continuable(&self) -> bool {
        self.continuable && !self.terminal
    }

    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    fn begin_continuation_attempt(&mut self) {
        if let ContinuationProtocol::Responses(state) = &mut self.protocol {
            state.continuation_prefix_len = Some(self.generated_text.len());
        }
    }

    /// Builds the next protocol request with the emitted text supplied as a prefix.
    pub fn request_body(&self, model_override: Option<&str>) -> Result<Bytes, ContinuationError> {
        if !self.is_continuable() {
            return Err(ContinuationError::NotContinuable);
        }

        let mut request = self.request.clone();
        let request = request
            .as_object_mut()
            .expect("eligible continuation requests are JSON objects");
        match self.protocol {
            ContinuationProtocol::Completion(_) => {
                let prompt = request
                    .get("prompt")
                    .and_then(Value::as_str)
                    .expect("eligible completion prompt is a string");
                request.insert(
                    "prompt".to_owned(),
                    Value::String(format!("{prompt}{}", self.generated_text)),
                );
            }
            ContinuationProtocol::Chat(_) => {
                request
                    .get_mut("messages")
                    .and_then(Value::as_array_mut)
                    .expect("eligible chat messages are an array")
                    .push(serde_json::json!({
                        "role": "assistant",
                        "content": self.generated_text,
                    }));
            }
            ContinuationProtocol::Responses(_) => {
                let original_input = request
                    .remove("input")
                    .expect("eligible Responses input is present");
                let mut input = match original_input {
                    Value::String(text) => vec![serde_json::json!({
                        "type": "message",
                        "role": "user",
                        "content": text,
                    })],
                    Value::Array(items) => items,
                    _ => unreachable!("eligible Responses input is text or an array"),
                };
                input.push(serde_json::json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": self.generated_text,
                    }],
                }));
                request.insert("input".to_owned(), Value::Array(input));
            }
        }
        if let Some(model) = model_override {
            request.insert("model".to_owned(), Value::String(model.to_owned()));
        }

        serde_json::to_vec(&request)
            .map(Bytes::from)
            .map_err(ContinuationError::Serialize)
    }

    fn reject_event(&mut self, event: &[u8]) -> EventObservation {
        self.continuable = false;
        EventObservation {
            event: Bytes::copy_from_slice(event),
            forward: true,
            terminal: self.terminal,
            done: false,
            safe: false,
            accepted: false,
        }
    }
}

struct ProtocolObservation {
    text: Option<String>,
    terminal: bool,
    accepted: bool,
    forward: bool,
    event: Bytes,
}

impl TextStreamIdentity {
    fn new(prefix: &str, model: Value, created: u64) -> Self {
        Self {
            id: Some(Value::String(format!("{prefix}-{}", uuid::Uuid::new_v4()))),
            model: Some(model),
            created: Some(Value::from(created)),
            identity_established: false,
        }
    }

    fn establish(&mut self, value: &Value) {
        self.id = value.get("id").cloned().or_else(|| self.id.take());
        self.model = value.get("model").cloned().or_else(|| self.model.take());
        self.created = value
            .get("created")
            .cloned()
            .or_else(|| self.created.take());
        self.identity_established = true;
    }

    fn rewrite(&self, value: &mut Value) {
        let Some(object) = value.as_object_mut() else {
            return;
        };
        if let Some(id) = &self.id {
            object.insert("id".to_owned(), id.clone());
        }
        if let Some(model) = &self.model {
            object.insert("model".to_owned(), model.clone());
        }
        if let Some(created) = &self.created {
            object.insert("created".to_owned(), created.clone());
        }
    }
}

impl ResponsesStreamState {
    fn new(model: String) -> Self {
        Self {
            response_id: format!("resp_{}", uuid::Uuid::new_v4()),
            model,
            item_id: format!("msg_{}", uuid::Uuid::new_v4()),
            response_identity_established: false,
            item_identity_established: false,
            last_sequence: None,
            emit_event_field: None,
            continuation_prefix_len: None,
        }
    }
}

fn eligible_completion_request(request: &serde_json::Map<String, Value>) -> bool {
    !has_unsupported_controls(
        request,
        &[
            "tools",
            "tool_choice",
            "functions",
            "function_call",
            "response_format",
            "json_schema",
            "grammar",
        ],
    ) && request.get("prompt").is_some_and(Value::is_string)
        && request.get("stream") == Some(&Value::Bool(true))
        && supports_single_choice(request.get("n"))
        && matches!(request.get("echo"), None | Some(Value::Bool(false)))
        && matches!(request.get("logprobs"), None | Some(Value::Null))
}

fn eligible_chat_request(request: &serde_json::Map<String, Value>) -> bool {
    !has_unsupported_controls(
        request,
        &[
            "tools",
            "tool_choice",
            "functions",
            "function_call",
            "response_format",
            "json_schema",
            "grammar",
            "modalities",
            "audio",
            "parallel_tool_calls",
            "reasoning",
            "reasoning_effort",
            "include_reasoning",
            "prediction",
        ],
    ) && request.get("messages").is_some_and(eligible_chat_messages)
        && request.get("stream") == Some(&Value::Bool(true))
        && supports_single_choice(request.get("n"))
        && matches!(request.get("logprobs"), None | Some(Value::Bool(false)))
        && !request.contains_key("top_logprobs")
}

fn eligible_responses_request(request: &serde_json::Map<String, Value>) -> bool {
    let input_supported = request.get("input").is_some_and(eligible_responses_input);
    let plain_text = match request.get("text") {
        None => true,
        Some(Value::Object(text)) => match text.get("format") {
            None => true,
            Some(Value::Object(format)) => {
                format.get("type").and_then(Value::as_str) == Some("text")
            }
            Some(_) => false,
        },
        Some(_) => false,
    };
    input_supported
        && plain_text
        && request.get("stream") == Some(&Value::Bool(true))
        && matches!(request.get("background"), None | Some(Value::Bool(false)))
        && matches!(request.get("store"), None | Some(Value::Bool(false)))
        && !has_unsupported_controls(
            request,
            &[
                "tools",
                "tool_choice",
                "max_tool_calls",
                "parallel_tool_calls",
                "reasoning",
                "previous_response_id",
                "conversation",
                "include",
                "json_schema",
                "grammar",
                "top_logprobs",
                "logprobs",
            ],
        )
}

fn eligible_chat_messages(value: &Value) -> bool {
    const ALLOWED_FIELDS: &[&str] = &["role", "content", "name"];
    value.as_array().is_some_and(|messages| {
        !messages.is_empty()
            && messages.iter().all(|message| {
                let Some(message) = message.as_object() else {
                    return false;
                };
                message
                    .keys()
                    .all(|key| ALLOWED_FIELDS.contains(&key.as_str()))
                    && matches!(
                        message.get("role").and_then(Value::as_str),
                        Some("system" | "developer" | "user" | "assistant")
                    )
                    && message.get("content").is_some_and(eligible_chat_content)
            })
    })
}

fn eligible_chat_content(value: &Value) -> bool {
    match value {
        Value::String(_) => true,
        Value::Array(parts) => {
            !parts.is_empty()
                && parts
                    .iter()
                    .all(|part| match part.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            keys_allowed(part, &["type", "text"])
                                && part.get("text").is_some_and(Value::is_string)
                        }
                        Some("image_url") => {
                            keys_allowed(part, &["type", "image_url"])
                                && part.get("image_url").is_some_and(Value::is_object)
                        }
                        _ => false,
                    })
        }
        _ => false,
    }
}

fn eligible_responses_input(value: &Value) -> bool {
    match value {
        Value::String(_) => true,
        Value::Array(items) => {
            !items.is_empty()
                && items.iter().all(|item| {
                    let Some(item) = item.as_object() else {
                        return false;
                    };
                    item.keys().all(|key| {
                        ["type", "id", "role", "content", "status"].contains(&key.as_str())
                    }) && item
                        .get("type")
                        .is_none_or(|item_type| item_type.as_str() == Some("message"))
                        && matches!(
                            item.get("role").and_then(Value::as_str),
                            Some("system" | "developer" | "user" | "assistant")
                        )
                        && item
                            .get("content")
                            .is_some_and(eligible_responses_message_content)
                })
        }
        _ => false,
    }
}

fn eligible_responses_message_content(value: &Value) -> bool {
    match value {
        Value::String(_) => true,
        Value::Array(parts) => {
            !parts.is_empty()
                && parts
                    .iter()
                    .all(|part| match part.get("type").and_then(Value::as_str) {
                        Some("input_text") => {
                            keys_allowed(part, &["type", "text"])
                                && part.get("text").is_some_and(Value::is_string)
                        }
                        Some("output_text") => {
                            keys_allowed(part, &["type", "text", "annotations", "logprobs"])
                                && part.get("text").is_some_and(Value::is_string)
                        }
                        Some("input_image") => keys_allowed(part, &["type", "image_url", "detail"]),
                        Some("input_file") => keys_allowed(part, &["type", "file_id", "filename"]),
                        _ => false,
                    })
        }
        _ => false,
    }
}

fn has_unsupported_controls(request: &serde_json::Map<String, Value>, controls: &[&str]) -> bool {
    controls.iter().any(|key| request.contains_key(*key))
        || request.keys().any(|key| key.starts_with("guided_"))
}

fn supports_single_choice(n: Option<&Value>) -> bool {
    match n {
        None => true,
        Some(Value::Number(n)) => n.as_u64() == Some(1),
        Some(_) => false,
    }
}

fn append_generated_text(
    generated_text: &mut String,
    continuable: &mut bool,
    max_buffered_bytes: usize,
    text: &str,
) {
    if !*continuable {
        return;
    }
    if generated_text.len().saturating_add(text.len()) > max_buffered_bytes {
        *continuable = false;
        metrics::counter!("onwards_stream_continuation_buffer_exhausted_total").increment(1);
        return;
    }
    generated_text.push_str(text);
}

fn observe_completion_event(
    identity: &mut TextStreamIdentity,
    value: &mut Value,
    event_type: Option<&[u8]>,
    rewrite_identity: bool,
) -> Option<ProtocolObservation> {
    if event_type.is_some() {
        return None;
    }
    let (text, terminal) = match completion_event(value) {
        CompletionEvent::Unsafe => return None,
        CompletionEvent::Recognized { text, terminal } => (text.map(str::to_owned), terminal),
    };
    if !rewrite_identity && !identity.identity_established {
        identity.establish(value);
    }
    identity.rewrite(value);
    Some(ProtocolObservation {
        accepted: text.is_some() || terminal,
        text,
        terminal,
        forward: true,
        event: Bytes::from(serialize_sse(value).ok()?),
    })
}

fn observe_chat_event(
    identity: &mut TextStreamIdentity,
    value: &mut Value,
    event_type: Option<&[u8]>,
    rewrite_identity: bool,
) -> Option<ProtocolObservation> {
    if event_type.is_some() {
        return None;
    }
    let event = chat_event(value)?;
    if !rewrite_identity && !identity.identity_established {
        identity.establish(value);
    }
    identity.rewrite(value);

    let (text, terminal, role_only) = match event {
        ChatEvent::Usage => (None, false, false),
        ChatEvent::Choice {
            text,
            terminal,
            role_present,
        } => {
            if rewrite_identity && role_present {
                value
                    .get_mut("choices")?
                    .get_mut(0)?
                    .get_mut("delta")?
                    .as_object_mut()?
                    .remove("role");
            }
            let role_only = role_present && text.is_none() && !terminal;
            (text, terminal, role_only)
        }
    };
    let forward = !(rewrite_identity && role_only);
    Some(ProtocolObservation {
        accepted: text.is_some() || terminal,
        text,
        terminal,
        forward,
        event: if forward {
            Bytes::from(serialize_sse(value).ok()?)
        } else {
            Bytes::new()
        },
    })
}

fn observe_responses_event(
    state: &mut ResponsesStreamState,
    value: &mut Value,
    event_field: Option<&[u8]>,
    rewrite_identity: bool,
    generated_text: &str,
) -> Option<ProtocolObservation> {
    if rewrite_identity && state.continuation_prefix_len.is_none() {
        state.continuation_prefix_len = Some(generated_text.len());
    }
    let event_type = value.get("type")?.as_str()?.to_owned();
    let incoming_sequence = value.get("sequence_number")?.as_u64()?;
    if let Some(event_field) = event_field
        && std::str::from_utf8(event_field).ok()? != event_type
    {
        return None;
    }
    let has_event_field = event_field.is_some();
    if !rewrite_identity {
        match state.emit_event_field {
            None => state.emit_event_field = Some(has_event_field),
            Some(expected) if expected != has_event_field => return None,
            Some(_) => {}
        }
        establish_responses_identity(state, value);
    }

    let (text, terminal, forward) = match event_type.as_str() {
        "response.created" => {
            if !keys_allowed(value, &["type", "sequence_number", "response"])
                || !rewrite_response_snapshot(
                    value.get_mut("response")?,
                    state,
                    generated_text,
                    false,
                )
            {
                return None;
            }
            (None, false, !rewrite_identity)
        }
        "response.in_progress" => {
            if !keys_allowed(value, &["type", "sequence_number", "response"])
                || !rewrite_response_snapshot(
                    value.get_mut("response")?,
                    state,
                    generated_text,
                    false,
                )
            {
                return None;
            }
            (None, false, !rewrite_identity)
        }
        "response.output_item.added" => {
            if !keys_allowed(value, &["type", "sequence_number", "output_index", "item"])
                || value.get("output_index").and_then(Value::as_u64) != Some(0)
                || !rewrite_response_item(value.get_mut("item")?, state, generated_text, false)
            {
                return None;
            }
            (None, false, !rewrite_identity)
        }
        "response.content_part.added" => {
            if !keys_allowed(
                value,
                &[
                    "type",
                    "sequence_number",
                    "item_id",
                    "output_index",
                    "content_index",
                    "part",
                ],
            ) || !valid_responses_indexes(value)
                || !rewrite_item_id(value, state)
                || !rewrite_output_text_part(value.get_mut("part")?, state, generated_text, false)
            {
                return None;
            }
            (None, false, !rewrite_identity)
        }
        "response.output_text.delta" => {
            if !keys_allowed(
                value,
                &[
                    "type",
                    "sequence_number",
                    "item_id",
                    "output_index",
                    "content_index",
                    "delta",
                    "logprobs",
                ],
            ) || !valid_responses_indexes(value)
                || !rewrite_item_id(value, state)
                || !empty_logprobs(value.get("logprobs"))
            {
                return None;
            }
            let text = value.get("delta")?.as_str()?.to_owned();
            (Some(text), false, true)
        }
        "response.output_text.done" => {
            if !keys_allowed(
                value,
                &[
                    "type",
                    "sequence_number",
                    "item_id",
                    "output_index",
                    "content_index",
                    "text",
                    "logprobs",
                ],
            ) || !valid_responses_indexes(value)
                || !rewrite_item_id(value, state)
                || !value.get("text").is_some_and(Value::is_string)
                || !empty_logprobs(value.get("logprobs"))
            {
                return None;
            }
            if !snapshot_text_matches(value.get("text")?.as_str()?, state, generated_text) {
                return None;
            }
            value
                .as_object_mut()?
                .insert("text".to_owned(), Value::String(generated_text.to_owned()));
            (None, false, true)
        }
        "response.content_part.done" => {
            if !keys_allowed(
                value,
                &[
                    "type",
                    "sequence_number",
                    "item_id",
                    "output_index",
                    "content_index",
                    "part",
                ],
            ) || !valid_responses_indexes(value)
                || !rewrite_item_id(value, state)
                || !rewrite_output_text_part(value.get_mut("part")?, state, generated_text, true)
            {
                return None;
            }
            (None, false, true)
        }
        "response.output_item.done" => {
            if !keys_allowed(value, &["type", "sequence_number", "output_index", "item"])
                || value.get("output_index").and_then(Value::as_u64) != Some(0)
                || !rewrite_response_item(value.get_mut("item")?, state, generated_text, true)
            {
                return None;
            }
            (None, false, true)
        }
        "response.completed" | "response.incomplete" => {
            if !keys_allowed(value, &["type", "sequence_number", "response"])
                || !rewrite_response_snapshot(
                    value.get_mut("response")?,
                    state,
                    generated_text,
                    true,
                )
            {
                return None;
            }
            (None, true, true)
        }
        "response.failed" | "response.cancelled" | "response.canceled" => {
            if !keys_allowed(value, &["type", "sequence_number", "response"])
                || !rewrite_response_snapshot(
                    value.get_mut("response")?,
                    state,
                    generated_text,
                    false,
                )
            {
                return None;
            }
            (None, true, true)
        }
        _ => return None,
    };

    if !forward {
        return Some(ProtocolObservation {
            text: None,
            terminal,
            accepted: false,
            forward: false,
            event: Bytes::new(),
        });
    }
    if rewrite_identity {
        let sequence = state.last_sequence.map_or(0, |last| last.saturating_add(1));
        value
            .as_object_mut()?
            .insert("sequence_number".to_owned(), Value::from(sequence));
        state.last_sequence = Some(sequence);
    } else {
        if state
            .last_sequence
            .is_some_and(|last| incoming_sequence <= last)
        {
            return None;
        }
        state.last_sequence = Some(incoming_sequence);
    }

    Some(ProtocolObservation {
        accepted: text.is_some() || terminal,
        text,
        terminal,
        forward: true,
        event: Bytes::from(
            serialize_response_sse(
                &event_type,
                value,
                state.emit_event_field.unwrap_or(has_event_field),
            )
            .ok()?,
        ),
    })
}

fn keys_allowed(value: &Value, allowed: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|object| object.keys().all(|key| allowed.contains(&key.as_str())))
}

fn empty_logprobs(value: Option<&Value>) -> bool {
    matches!(value, None | Some(Value::Null))
        || value.and_then(Value::as_array).is_some_and(Vec::is_empty)
}

fn establish_responses_identity(state: &mut ResponsesStreamState, value: &Value) {
    if !state.response_identity_established
        && let Some(response) = value.get("response").and_then(Value::as_object)
    {
        if let Some(id) = response.get("id").and_then(Value::as_str) {
            state.response_id = id.to_owned();
        }
        if let Some(model) = response.get("model").and_then(Value::as_str) {
            state.model = model.to_owned();
        }
        state.response_identity_established = true;
    }
    if !state.item_identity_established {
        let item = value.get("item").and_then(Value::as_object);
        let response_item = value
            .get("response")
            .and_then(|response| response.get("output"))
            .and_then(Value::as_array)
            .and_then(|output| output.first())
            .and_then(Value::as_object);
        let item_id = value
            .get("item_id")
            .and_then(Value::as_str)
            .or_else(|| item.and_then(|item| item.get("id")).and_then(Value::as_str))
            .or_else(|| {
                response_item
                    .and_then(|item| item.get("id"))
                    .and_then(Value::as_str)
            });
        if let Some(item_id) = item_id {
            state.item_id = item_id.to_owned();
        }
        if value.get("item_id").is_some() || item.is_some() || response_item.is_some() {
            state.item_identity_established = true;
        }
    }
}

fn valid_responses_indexes(value: &Value) -> bool {
    value.get("output_index").and_then(Value::as_u64) == Some(0)
        && value.get("content_index").and_then(Value::as_u64) == Some(0)
}

fn rewrite_item_id(value: &mut Value, state: &ResponsesStreamState) -> bool {
    let Some(object) = value.as_object_mut() else {
        return false;
    };
    if !object.get("item_id").is_some_and(Value::is_string) {
        return false;
    }
    object.insert("item_id".to_owned(), Value::String(state.item_id.clone()));
    true
}

fn rewrite_response_snapshot(
    response: &mut Value,
    state: &ResponsesStreamState,
    generated_text: &str,
    require_output: bool,
) -> bool {
    let Some(object) = response.as_object_mut() else {
        return false;
    };
    object.insert("id".to_owned(), Value::String(state.response_id.clone()));
    object.insert("model".to_owned(), Value::String(state.model.clone()));
    let Some(output) = object.get_mut("output") else {
        return !require_output;
    };
    let Some(output) = output.as_array_mut() else {
        return false;
    };
    if output.is_empty() {
        return !require_output;
    }
    output.len() == 1
        && rewrite_response_item(&mut output[0], state, generated_text, require_output)
}

fn rewrite_response_item(
    item: &mut Value,
    state: &ResponsesStreamState,
    generated_text: &str,
    require_content: bool,
) -> bool {
    let Some(object) = item.as_object_mut() else {
        return false;
    };
    if object.get("type").and_then(Value::as_str) != Some("message")
        || object.get("role").and_then(Value::as_str) != Some("assistant")
    {
        return false;
    }
    object.insert("id".to_owned(), Value::String(state.item_id.clone()));
    let Some(content) = object.get_mut("content") else {
        return !require_content;
    };
    let Some(content) = content.as_array_mut() else {
        return false;
    };
    if content.is_empty() {
        return !require_content;
    }
    content.len() == 1
        && rewrite_output_text_part(&mut content[0], state, generated_text, require_content)
}

fn rewrite_output_text_part(
    part: &mut Value,
    state: &ResponsesStreamState,
    generated_text: &str,
    require_text: bool,
) -> bool {
    let Some(object) = part.as_object_mut() else {
        return false;
    };
    if object.get("type").and_then(Value::as_str) != Some("output_text") {
        return false;
    }
    if require_text {
        let Some(text) = object.get("text").and_then(Value::as_str) else {
            return false;
        };
        if !snapshot_text_matches(text, state, generated_text) {
            return false;
        }
        object.insert("text".to_owned(), Value::String(generated_text.to_owned()));
    }
    true
}

fn snapshot_text_matches(
    snapshot_text: &str,
    state: &ResponsesStreamState,
    generated_text: &str,
) -> bool {
    snapshot_text == generated_text
        || state
            .continuation_prefix_len
            .and_then(|prefix_len| generated_text.get(prefix_len..))
            .is_some_and(|attempt_text| snapshot_text == attempt_text)
}

fn serialize_response_sse(
    event_type: &str,
    value: &Value,
    emit_event_field: bool,
) -> Result<Vec<u8>, ContinuationError> {
    let data = serde_json::to_vec(value).map_err(ContinuationError::Serialize)?;
    let mut event = Vec::with_capacity(data.len() + event_type.len() + 24);
    if emit_event_field {
        event.extend_from_slice(b"event: ");
        event.extend_from_slice(event_type.as_bytes());
        event.push(b'\n');
    }
    event.extend_from_slice(b"data: ");
    event.extend_from_slice(&data);
    event.extend_from_slice(b"\n\n");
    Ok(event)
}

enum ChatEvent {
    Usage,
    Choice {
        text: Option<String>,
        terminal: bool,
        role_present: bool,
    },
}

fn chat_event(value: &Value) -> Option<ChatEvent> {
    const TOP_LEVEL_FIELDS: &[&str] = &[
        "id",
        "object",
        "created",
        "model",
        "choices",
        "usage",
        "system_fingerprint",
        "service_tier",
    ];
    let object = value.as_object()?;
    if object
        .keys()
        .any(|key| !TOP_LEVEL_FIELDS.contains(&key.as_str()))
        || object
            .get("object")
            .is_some_and(|object| object.as_str() != Some("chat.completion.chunk"))
    {
        return None;
    }
    let choices = object.get("choices")?.as_array()?;
    if choices.is_empty() {
        return object
            .get("usage")
            .filter(|usage| usage.is_object())
            .map(|_| ChatEvent::Usage);
    }
    if choices.len() != 1 {
        return None;
    }
    let choice = choices[0].as_object()?;
    const CHOICE_FIELDS: &[&str] = &["index", "delta", "finish_reason", "logprobs"];
    if choice
        .keys()
        .any(|key| !CHOICE_FIELDS.contains(&key.as_str()))
        || choice.get("index").and_then(Value::as_u64) != Some(0)
        || !matches!(choice.get("logprobs"), None | Some(Value::Null))
    {
        return None;
    }
    let delta = choice.get("delta")?.as_object()?;
    if delta
        .keys()
        .any(|key| !matches!(key.as_str(), "role" | "content"))
    {
        return None;
    }
    let role_present = match delta.get("role") {
        None => false,
        Some(Value::String(role)) if role == "assistant" => true,
        Some(_) => return None,
    };
    let text = match delta.get("content") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return None,
    };
    let terminal = match choice.get("finish_reason") {
        None | Some(Value::Null) => false,
        Some(Value::String(_)) => true,
        Some(_) => return None,
    };
    Some(ChatEvent::Choice {
        text,
        terminal,
        role_present,
    })
}

enum StreamInterruption {
    Done,
    Eof,
    Body(std::io::Error),
    Framing(SseFramingError),
    IdleTimeout,
}

#[derive(Default)]
struct TerminalMetricState {
    recorded: bool,
}

impl TerminalMetricState {
    fn observe_finish_reason(&mut self) -> Option<&'static str> {
        self.record("finish_reason")
    }

    fn observe_done(&mut self) -> Option<&'static str> {
        self.record("done")
    }

    fn record(&mut self, reason: &'static str) -> Option<&'static str> {
        if self.recorded {
            None
        } else {
            self.recorded = true;
            Some(reason)
        }
    }
}

impl StreamInterruption {
    fn into_error(self) -> Option<std::io::Error> {
        match self {
            Self::Done | Self::Eof => None,
            Self::Body(error) => Some(error),
            Self::Framing(error) => Some(std::io::Error::other(error)),
            Self::IdleTimeout => Some(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "upstream generation stream idle timeout",
            )),
        }
    }

    fn reason(&self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Eof => "eof",
            Self::Body(_) => "body_error",
            Self::Framing(_) => "framing_error",
            Self::IdleTimeout => "idle_timeout",
        }
    }

    fn exhausted_reason(&self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Eof => "exhausted_eof",
            Self::Body(_) => "exhausted_body_error",
            Self::Framing(_) => "framing_error",
            Self::IdleTimeout => "exhausted_idle_timeout",
        }
    }
}

/// Combines an initial generation body with independently selected continuation bodies.
pub(crate) fn wrap_generation_stream<T>(
    initial_body: Body,
    initial_guard: ConcurrencyGuard,
    mut continuation: StreamContinuation,
    config: StreamContinuationConfig,
    pool: ProviderPool,
    http_client: T,
    request_metadata: UpstreamRequestMetadata,
) -> Body
where
    T: HttpClient + Send + 'static,
{
    let fallback = pool.fallback().cloned().unwrap_or_default();
    let mut selection = SelectionState::new(config.max_attempts, fallback.with_replacement);
    let max_event_bytes = event_buffer_size();
    let stream = async_stream::stream! {
        use tracing::Instrument;

        let mut current_body = initial_body;
        let mut current_guard = Some(initial_guard);
        let mut rewrite_identity = false;
        let mut awaiting_resumption_event = false;
        let mut terminal_metric = TerminalMetricState::default();
        let mut continuation_attempt = 0_u32;
        let mut total_backoff_ms = 0_u64;

        loop {
            let mut events = CheckedSseStream::with_max_buffer_size(
                current_body.into_data_stream(),
                max_event_bytes,
            );
            let interruption = loop {
                let next = if let Some(timeout_ms) = config.idle_timeout_ms {
                    match tokio::time::timeout(Duration::from_millis(timeout_ms), events.next()).await {
                        Ok(next) => next,
                        Err(_) => break StreamInterruption::IdleTimeout,
                    }
                } else {
                    events.next().await
                };

                match next {
                    Some(Ok(event)) => {
                        let observation = match continuation.observe_event(&event, rewrite_identity) {
                            Ok(observation) => observation,
                            Err(error) => {
                                yield Err::<Bytes, std::io::Error>(std::io::Error::other(error));
                                return;
                            }
                        };
                        if rewrite_identity && !observation.safe {
                            metrics::counter!(
                                "onwards_stream_continuation_failures_total",
                                "reason" => "unsafe_continuation_event"
                            )
                            .increment(1);
                            yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                                "unsafe generation event from continuation provider",
                            ));
                            return;
                        }
                        if !rewrite_identity && !observation.safe {
                            metrics::counter!(
                                "onwards_stream_continuation_failures_total",
                                "reason" => "unsafe_initial_event"
                            )
                            .increment(1);
                        }
                        if rewrite_identity && awaiting_resumption_event && observation.accepted {
                            awaiting_resumption_event = false;
                            metrics::counter!("onwards_stream_continuation_resumptions_total")
                                .increment(1);
                        }
                        if observation.terminal
                            && !observation.done
                            && let Some(reason) = terminal_metric.observe_finish_reason()
                        {
                            metrics::counter!(
                                "onwards_stream_continuation_terminal_total",
                                "reason" => reason
                            )
                            .increment(1);
                        }
                        let done = observation.done;
                        if observation.forward {
                            yield Ok::<Bytes, std::io::Error>(observation.event);
                        }
                        if done {
                            break StreamInterruption::Done;
                        }
                    }
                    Some(Err(SseStreamError::Source(error))) => {
                        if let Some(framing) = framing_error_in_chain(&error) {
                            break StreamInterruption::Framing(framing);
                        }
                        break StreamInterruption::Body(std::io::Error::other(error));
                    }
                    Some(Err(SseStreamError::Framing(error))) => {
                        break StreamInterruption::Framing(error);
                    }
                    None => break StreamInterruption::Eof,
                }
            };

            drop(events);
            drop(current_guard.take());

            tracing::debug!(
                reason = interruption.reason(),
                attempt = continuation_attempt,
                "Generation stream ended"
            );

            if matches!(interruption, StreamInterruption::Done) {
                if let Some(reason) = terminal_metric.observe_done() {
                    metrics::counter!(
                        "onwards_stream_continuation_terminal_total",
                        "reason" => reason
                    )
                    .increment(1);
                }
                break;
            }

            if continuation.is_terminal() {
                break;
            }

            if matches!(interruption, StreamInterruption::Framing(_)) {
                metrics::counter!(
                    "onwards_stream_continuation_failures_total",
                    "reason" => "framing_error"
                )
                .increment(1);
                if let Some(error) = interruption.into_error() {
                    yield Err::<Bytes, std::io::Error>(error);
                }
                break;
            }

            let exhausted_reason = interruption.exhausted_reason();
            let final_error = interruption.into_error();
            if !continuation.is_continuable() {
                if let Some(error) = final_error {
                    yield Err::<Bytes, std::io::Error>(error);
                }
                break;
            }

            let mut next_body = None;
            let mut next_guard = None;
            loop {
                if continuation_attempt as usize >= config.max_attempts {
                    metrics::counter!(
                        "onwards_stream_continuation_failures_total",
                        "reason" => "max_attempts"
                    )
                    .increment(1);
                    break;
                }
                let retry_index = continuation_attempt.saturating_add(1);
                if let Some(backoff) = fallback.backoff.as_ref() {
                    let delay = backoff.delay(retry_index);
                    let delay_ms = delay.as_millis() as u64;
                    let next_total = total_backoff_ms.saturating_add(delay_ms);
                    if fallback
                        .max_total_backoff_ms
                        .is_some_and(|max_total| next_total > max_total)
                    {
                        metrics::counter!(
                            "onwards_stream_continuation_failures_total",
                            "reason" => "backoff_budget"
                        )
                        .increment(1);
                        break;
                    }
                    total_backoff_ms = next_total;
                    tokio::time::sleep(delay).await;
                }

                let Some((_index, target, guard)) = pool.select_next(&mut selection) else {
                    metrics::counter!(
                        "onwards_stream_continuation_failures_total",
                        "reason" => "selection_exhausted"
                    )
                    .increment(1);
                    break;
                };
                continuation_attempt = continuation_attempt.saturating_add(1);
                metrics::counter!("onwards_stream_continuation_attempts_total").increment(1);
                let target = target.clone();
                let attempt_span = tracing::info_span!(
                    "onwards.stream_continuation_attempt",
                    attempt = continuation_attempt,
                    outcome = tracing::field::Empty,
                    http.response.status_code = tracing::field::Empty,
                );
                let attempt_request_metadata = request_metadata.for_child_span(&attempt_span);

                if target
                    .limiter
                    .as_ref()
                    .is_some_and(|limiter| limiter.check().is_err())
                {
                    attempt_span.record("outcome", "rate_limit");
                    drop(guard);
                    metrics::counter!(
                        "onwards_stream_continuation_failures_total",
                        "reason" => "rate_limit"
                    )
                    .increment(1);
                    if pool.should_fallback_on_rate_limit() {
                        continue;
                    }
                    break;
                }

                let request_body = match continuation.request_body(target.onwards_model.as_deref()) {
                    Ok(body) => body,
                    Err(error) => {
                        drop(guard);
                        metrics::counter!(
                            "onwards_stream_continuation_failures_total",
                            "reason" => "request_body"
                        )
                        .increment(1);
                        tracing::error!(error = %error, "Failed to build stream continuation request");
                        break;
                    }
                };
                let (request, upstream_uri) = match build_upstream_request(
                    &target,
                    &attempt_request_metadata,
                    request_body,
                ) {
                    Ok(request) => request,
                    Err(_error) => {
                        drop(guard);
                        metrics::counter!(
                            "onwards_stream_continuation_failures_total",
                            "reason" => "request_build"
                        )
                        .increment(1);
                        tracing::error!(
                            "Failed to build stream continuation upstream request"
                        );
                        break;
                    }
                };

                tracing::debug!(
                    attempt = continuation_attempt,
                    upstream = %upstream_uri,
                    "Requesting generation stream continuation"
                );
                let request_future = http_client
                    .request(request)
                    .instrument(attempt_span.clone());
                let response = if let Some(timeout_secs) = target.request_timeout_secs {
                    match tokio::time::timeout(
                        Duration::from_secs(timeout_secs),
                        request_future,
                    )
                    .await
                    {
                        Ok(response) => response,
                        Err(_) => {
                            attempt_span.record("outcome", "header_timeout");
                            drop(guard);
                            metrics::counter!(
                                "onwards_stream_continuation_failures_total",
                                "reason" => "header_timeout"
                            )
                            .increment(1);
                            continue;
                        }
                    }
                } else {
                    request_future.await
                };

                let response = match response {
                    Ok(response) => response,
                    Err(error) => {
                        attempt_span.record("outcome", "network_error");
                        drop(guard);
                        metrics::counter!(
                            "onwards_stream_continuation_failures_total",
                            "reason" => "network_error"
                        )
                        .increment(1);
                        tracing::warn!(
                            upstream = %target.url,
                            error = %error,
                            "Stream continuation request failed"
                        );
                        continue;
                    }
                };
                let status = response.status().as_u16();
                attempt_span.record("http.response.status_code", status);
                if !(200..300).contains(&status) {
                    attempt_span.record("outcome", "status");
                    drop(response);
                    drop(guard);
                    metrics::counter!(
                        "onwards_stream_continuation_failures_total",
                        "reason" => "status"
                    )
                    .increment(1);
                    if pool.should_fallback_on_status(status) {
                        continue;
                    }
                    break;
                }
                if !is_event_stream(response.headers()) {
                    attempt_span.record("outcome", "content_type");
                    drop(response);
                    drop(guard);
                    metrics::counter!(
                        "onwards_stream_continuation_failures_total",
                        "reason" => "content_type"
                    )
                    .increment(1);
                    continue;
                }
                if !has_identity_content_encoding(response.headers()) {
                    attempt_span.record("outcome", "content_encoding");
                    drop(response);
                    drop(guard);
                    metrics::counter!(
                        "onwards_stream_continuation_failures_total",
                        "reason" => "content_encoding"
                    )
                    .increment(1);
                    continue;
                }

                next_body = Some(response.into_body());
                next_guard = Some(guard);
                attempt_span.record("outcome", "headers_accepted");
                break;
            }

            match (next_body, next_guard) {
                (Some(body), Some(guard)) => {
                    continuation.begin_continuation_attempt();
                    current_body = body;
                    current_guard = Some(guard);
                    rewrite_identity = true;
                    awaiting_resumption_event = true;
                }
                _ => {
                    metrics::counter!(
                        "onwards_stream_continuation_failures_total",
                        "reason" => exhausted_reason
                    )
                    .increment(1);
                    if let Some(error) = final_error {
                        yield Err::<Bytes, std::io::Error>(error);
                    }
                    break;
                }
            }
        }
    };

    Body::from_stream(stream)
}

enum CompletionEvent<'a> {
    Recognized {
        text: Option<&'a str>,
        terminal: bool,
    },
    Unsafe,
}

fn completion_event(completion: &Value) -> CompletionEvent<'_> {
    let Some(object) = completion.as_object() else {
        return CompletionEvent::Unsafe;
    };
    const SUPPORTED_TOP_LEVEL_FIELDS: &[&str] = &[
        "id",
        "object",
        "created",
        "model",
        "choices",
        "usage",
        "system_fingerprint",
        "error",
    ];
    if object
        .keys()
        .any(|key| !SUPPORTED_TOP_LEVEL_FIELDS.contains(&key.as_str()))
    {
        return CompletionEvent::Unsafe;
    }
    if object.contains_key("error") {
        return CompletionEvent::Unsafe;
    }
    if object
        .get("object")
        .is_some_and(|value| value.as_str() != Some("text_completion"))
    {
        return CompletionEvent::Unsafe;
    }

    let Some(choices_value) = object.get("choices") else {
        return if choice_less_completion_metadata(completion) {
            CompletionEvent::Recognized {
                text: None,
                terminal: false,
            }
        } else {
            CompletionEvent::Unsafe
        };
    };
    let Some(choices) = choices_value.as_array() else {
        return CompletionEvent::Unsafe;
    };

    if choices.is_empty() && choice_less_completion_metadata(completion) {
        return CompletionEvent::Recognized {
            text: None,
            terminal: false,
        };
    }

    if choices.len() != 1 {
        return CompletionEvent::Unsafe;
    }

    let Some(choice) = choices.first().and_then(Value::as_object) else {
        return CompletionEvent::Unsafe;
    };
    const SUPPORTED_CHOICE_FIELDS: &[&str] = &["index", "text", "finish_reason", "logprobs"];
    if choice
        .keys()
        .any(|key| !SUPPORTED_CHOICE_FIELDS.contains(&key.as_str()))
        || choice.get("index").and_then(Value::as_u64) != Some(0)
        || !matches!(choice.get("logprobs"), None | Some(Value::Null))
    {
        return CompletionEvent::Unsafe;
    }
    let Some(text) = choice.get("text").and_then(Value::as_str) else {
        return CompletionEvent::Unsafe;
    };
    let terminal = match choice.get("finish_reason") {
        None | Some(Value::Null) => false,
        Some(Value::String(_)) => true,
        Some(_) => return CompletionEvent::Unsafe,
    };

    CompletionEvent::Recognized {
        text: Some(text),
        terminal,
    }
}

fn choice_less_completion_metadata(completion: &Value) -> bool {
    let Some(completion) = completion.as_object() else {
        return false;
    };
    completion.get("object").and_then(Value::as_str) == Some("text_completion")
        && completion.contains_key("id")
        && completion.contains_key("model")
        && completion.contains_key("created")
}

fn serialize_sse(completion: &Value) -> Result<Vec<u8>, ContinuationError> {
    let data = serde_json::to_vec(completion).map_err(ContinuationError::Serialize)?;
    let mut event = Vec::with_capacity(b"data: \n\n".len() + data.len());
    event.extend_from_slice(b"data: ");
    event.extend_from_slice(&data);
    event.extend_from_slice(b"\n\n");
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::StreamContinuationConfig;
    use axum::http::Method;
    use serde_json::Value;

    fn eligible_state(max_buffered_bytes: usize) -> StreamContinuation {
        let config = StreamContinuationConfig {
            enabled: true,
            endpoints: vec!["/v1/completions".to_string()],
            max_attempts: 1,
            max_buffered_bytes,
            idle_timeout_ms: None,
        };
        StreamContinuation::from_request(
            "/v1/completions",
            &Method::POST,
            br#"{"model":"requested-m","prompt":"Say hello: ","stream":true}"#,
            &config,
        )
        .unwrap()
    }

    #[test]
    fn accepts_only_supported_completion_requests() {
        let config = StreamContinuationConfig {
            enabled: true,
            endpoints: vec!["/v1/completions".to_string()],
            max_attempts: 1,
            max_buffered_bytes: 1024,
            idle_timeout_ms: None,
        };
        let eligible = br#"{"prompt":"hello","stream":true}"#;

        assert!(
            StreamContinuation::from_request("/v1/completions", &Method::POST, eligible, &config,)
                .is_some()
        );
        for (path, method, body) in [
            ("/v1/chat/completions", Method::POST, eligible.as_slice()),
            ("/v1/completions", Method::GET, eligible.as_slice()),
            (
                "/v1/completions",
                Method::POST,
                br#"{"prompt":["hello"],"stream":true}"#.as_slice(),
            ),
            (
                "/v1/completions",
                Method::POST,
                br#"{"prompt":"hello","stream":false}"#.as_slice(),
            ),
            (
                "/v1/completions",
                Method::POST,
                br#"{"prompt":"hello","stream":true,"n":2}"#.as_slice(),
            ),
            (
                "/v1/completions",
                Method::POST,
                br#"{"prompt":"hello","stream":true,"echo":true}"#.as_slice(),
            ),
            (
                "/v1/completions",
                Method::POST,
                br#"{"prompt":"hello","stream":true,"logprobs":1}"#.as_slice(),
            ),
        ] {
            assert!(StreamContinuation::from_request(path, &method, body, &config).is_none());
        }

        assert!(
            StreamContinuation::from_request(
                "/v1/completions",
                &Method::POST,
                br#"{"prompt":"hello","stream":true,"logprobs":null}"#,
                &config,
            )
            .is_some()
        );
    }

    #[test]
    fn advanced_generation_controls_are_ineligible() {
        let config = StreamContinuationConfig {
            enabled: true,
            endpoints: vec!["/v1/completions".to_string()],
            max_attempts: 1,
            max_buffered_bytes: 1024,
            idle_timeout_ms: None,
        };

        for key in [
            "tools",
            "tool_choice",
            "functions",
            "function_call",
            "response_format",
            "json_schema",
            "grammar",
            "guided_json",
            "guided_regex",
            "guided_choice",
            "guided_grammar",
            "guided_options_request",
        ] {
            let mut request = serde_json::json!({
                "prompt": "hello",
                "stream": true
            });
            request[key] = serde_json::json!({"enabled": true});
            let body = serde_json::to_vec(&request).unwrap();

            assert!(
                StreamContinuation::from_request("/v1/completions", &Method::POST, &body, &config,)
                    .is_none(),
                "{key} must disable continuation"
            );
        }
    }

    #[test]
    fn interrupted_completion_builds_prefix_request() {
        let mut state = eligible_state(1024);
        state
            .observe_event(
                br#"data: {"id":"cmpl-a","created":1,"model":"m","choices":[{"index":0,"text":"hello","finish_reason":null}]}

"#,
                false,
            )
            .unwrap();
        let body: Value =
            serde_json::from_slice(&state.request_body(Some("upstream-m")).unwrap()).unwrap();
        assert_eq!(body["prompt"], "Say hello: hello");
        assert_eq!(body["model"], "upstream-m");
    }

    #[test]
    fn terminal_events_prevent_continuation() {
        let mut state = eligible_state(1024);
        assert!(
            state
                .observe_event(b"data: [DONE]\n\n", false)
                .unwrap()
                .terminal
        );
        assert!(!state.is_continuable());

        let mut state = eligible_state(1024);
        assert!(
            state
                .observe_event(
                    br#"data: {"choices":[{"index":0,"text":"","finish_reason":"stop"}]}

"#,
                    false,
                )
                .unwrap()
                .terminal
        );
        assert!(!state.is_continuable());
    }

    #[test]
    fn multiline_continuation_event_reuses_original_identity() {
        let mut state = eligible_state(1024);
        state
            .observe_event(
                br#"data: {"id":"cmpl-first","created":1,"model":"first","choices":[{"index":0,"text":"one","finish_reason":null}]}

"#,
                false,
            )
            .unwrap();

        let observation = state
            .observe_event(
                b"data: {\"id\":\"cmpl-next\",\"created\":2,\ndata: \"model\":\"next\",\"choices\":[{\"index\":0,\"text\":\"two\",\"finish_reason\":null}]}\n\n",

                true,
            )
            .unwrap();

        let rewritten = String::from_utf8(observation.event.to_vec()).unwrap();
        assert_eq!(
            rewritten,
            "data: {\"choices\":[{\"finish_reason\":null,\"index\":0,\"text\":\"two\"}],\"created\":1,\"id\":\"cmpl-first\",\"model\":\"first\"}\n\n"
        );
    }

    #[test]
    fn comments_are_safe_but_malformed_events_disable_continuation() {
        let mut state = eligible_state(1024);
        let comment = b": keepalive\n\n";
        let observation = state.observe_event(comment, false).unwrap();
        assert_eq!(observation.event.as_ref(), comment);
        assert!(state.is_continuable());

        let malformed = b"data: {not-json}\n\n";
        let observation = state.observe_event(malformed, false).unwrap();
        assert_eq!(observation.event.as_ref(), malformed);
        assert!(!state.is_continuable());
    }

    #[test]
    fn multiple_choices_disable_continuation_without_appending_a_prefix() {
        let mut state = eligible_state(1024);
        let event = br#"data: {"choices":[{"index":0,"text":"one","finish_reason":null},{"index":1,"text":"two","finish_reason":"stop"}]}

"#;
        let observation = state.observe_event(event, true).unwrap();

        assert_eq!(observation.event.as_ref(), event);
        assert!(!observation.terminal);
        assert!(!state.is_continuable());
        assert!(state.generated_text.is_empty());
    }

    #[test]
    fn buffer_exhaustion_disables_continuation_without_retaining_new_text() {
        let mut state = eligible_state(5);
        state
            .observe_event(
                br#"data: {"choices":[{"index":0,"text":"hello","finish_reason":null}]}

"#,
                false,
            )
            .unwrap();
        state
            .observe_event(
                br#"data: {"choices":[{"index":0,"text":"!","finish_reason":null}]}

"#,
                false,
            )
            .unwrap();

        assert!(!state.is_continuable());
        assert!(state.request_body(None).is_err());
    }

    #[test]
    fn initial_recognized_event_receives_stable_fallback_identity() {
        let mut state = eligible_state(1024);
        let event = br#"data: {"choices":[{"index":0,"text":"hello","finish_reason":null}]}

"#;
        let observation = state.observe_event(event, false).unwrap();
        let rewritten: Value =
            serde_json::from_slice(&observation.event[6..observation.event.len() - 2]).unwrap();
        assert!(rewritten["id"].as_str().unwrap().starts_with("cmpl-"));
        assert_eq!(rewritten["model"], "requested-m");
        assert!(rewritten["created"].is_u64());
        assert_eq!(rewritten["choices"][0]["text"], "hello");
        assert!(!observation.terminal);
    }

    #[test]
    fn unrecognized_choice_events_pass_through_and_disable_continuation() {
        for event in [
            br#"data: {"id":"chat-next","choices":[{"index":0,"delta":{"content":"two"},"finish_reason":"stop"}]}

"#
            .as_slice(),
            br#"data: {"id":"empty-choice","choices":[{}]}

"#
            .as_slice(),
        ] {
            let mut state = eligible_state(1024);
            state
                .observe_event(
                br#"data: {"id":"cmpl-first","created":1,"model":"first","choices":[{"index":0,"text":"one","finish_reason":null}]}

"#,
                    false,
                )
                .unwrap();
            let observation = state.observe_event(event, true).unwrap();
            assert_eq!(observation.event.as_ref(), event);
            assert!(!observation.terminal, "unsafe finish reasons are not trusted");
            assert!(!state.is_continuable());
        }
    }

    #[test]
    fn unrecognized_json_without_choices_passes_through_and_disables_continuation() {
        let mut state = eligible_state(1024);
        let event = br#"data: {"id":"other","type":"notification","payload":{"value":1}}

"#;

        let observation = state.observe_event(event, true).unwrap();
        assert_eq!(observation.event.as_ref(), event);
        assert!(!observation.terminal);
        assert!(!state.is_continuable());
    }

    #[test]
    fn hybrid_sse_fields_disable_continuation_without_changing_initial_bytes() {
        for field in [
            "event: completion",
            "id: provider-event",
            "retry: 1000",
            "provider-field: unsafe",
        ] {
            let mut state = eligible_state(1024);
            let event = format!(
                "{field}\ndata: {{\"choices\":[{{\"index\":0,\"text\":\"x\",\"finish_reason\":null}}]}}\n\n"
            );

            let observation = state.observe_event(event.as_bytes(), false).unwrap();

            assert_eq!(observation.event.as_ref(), event.as_bytes());
            assert!(!observation.safe, "unsupported SSE field: {field}");
            assert!(!state.is_continuable(), "unsupported SSE field: {field}");
        }
    }

    #[test]
    fn unsafe_top_level_completion_keys_disable_continuation() {
        for key in ["payload", "tool_calls", "unknown_provider_state"] {
            let mut state = eligible_state(1024);
            let mut value = serde_json::json!({
                "choices": [{"index": 0, "text": "x", "finish_reason": null}]
            });
            value[key] = serde_json::json!({"unsafe": true});
            let event = format!("data: {value}\n\n");

            let observation = state.observe_event(event.as_bytes(), false).unwrap();

            assert!(!observation.safe, "unsafe key: {key}");
            assert!(!state.is_continuable(), "unsafe key: {key}");
        }
    }

    #[test]
    fn safe_completion_metadata_remains_continuable() {
        let mut state = eligible_state(1024);
        let event = b"data: {\"id\":\"cmpl-a\",\"object\":\"text_completion\",\"created\":1,\"model\":\"m\",\"system_fingerprint\":\"fp\",\"choices\":[{\"index\":0,\"text\":\"x\",\"finish_reason\":null}],\"usage\":{\"completion_tokens\":1}}\n\n";

        let observation = state.observe_event(event, false).unwrap();

        assert!(observation.safe);
        assert!(state.is_continuable());
    }

    #[test]
    fn terminal_metric_reason_is_emitted_only_once() {
        let mut state = TerminalMetricState::default();

        assert_eq!(state.observe_finish_reason(), Some("finish_reason"));
        assert_eq!(state.observe_done(), None);
        assert_eq!(state.observe_finish_reason(), None);
    }

    #[test]
    fn unsafe_completion_shapes_fail_closed_without_changing_initial_bytes() {
        for event in [
            b"data: {not-json}\n\n".as_slice(),
            br#"data: {"error":{"message":"failed"}}

"#
            .as_slice(),
            br#"data: {"choices":[{"index":0,"text":"x","finish_reason":null}],"error":{"message":"failed"}}

"#
            .as_slice(),
            br#"data: {"choices":[{"index":0,"delta":{"content":"x"},"finish_reason":null}]}

"#
            .as_slice(),
            br#"data: {"choices":[{"index":0,"message":{"content":"x"},"finish_reason":null}]}

"#
            .as_slice(),
            br#"data: {"choices":[{"index":0,"text":"x","finish_reason":null,"tool_calls":[]}]}

"#
            .as_slice(),
            br#"data: {"choices":[{"text":"x","finish_reason":null}]}

"#
            .as_slice(),
            br#"data: {"choices":[{"index":1,"text":"x","finish_reason":null}]}

"#
            .as_slice(),
            br#"data: {"type":"notification","payload":{"value":1}}

"#
            .as_slice(),
        ] {
            let mut state = eligible_state(1024);
            let observation = state.observe_event(event, false).unwrap();

            assert_eq!(observation.event.as_ref(), event);
            assert!(!state.is_continuable(), "unsafe event: {event:?}");
        }
    }

    #[test]
    fn done_without_post_colon_space_and_bom_are_recognized() {
        let mut state = eligible_state(1024);
        let event = b"\xef\xbb\xbfdata:[DONE]\r\n\r\n";
        let observation = state.observe_event(event, false).unwrap();

        assert_eq!(observation.event.as_ref(), event);
        assert!(observation.done);
        assert!(observation.terminal);
        assert!(!state.is_continuable());
    }

    #[test]
    fn observation_flags_distinguish_comments_content_and_unsafe_events() {
        let mut state = eligible_state(1024);
        let comment = state.observe_event(b": ping\n\n", false).unwrap();
        assert!(comment.safe);
        assert!(!comment.accepted);

        let content = state
            .observe_event(
                b"data:{\"choices\":[{\"index\":0,\"text\":\"x\",\"finish_reason\":null}]}\n\n",
                false,
            )
            .unwrap();
        assert!(content.safe);
        assert!(content.accepted);

        let unsafe_event = state.observe_event(b"data:{not-json}\n\n", false).unwrap();
        assert!(!unsafe_event.safe);
        assert!(!unsafe_event.accepted);
    }

    #[test]
    fn missing_initial_identity_is_stable_across_continuations() {
        let mut state = eligible_state(1024);
        let initial = state
            .observe_event(
                br#"data: {"id":"cmpl-first","choices":[{"index":0,"text":"one","finish_reason":null}]}

"#,
                false,
            )
            .unwrap();
        let initial: Value =
            serde_json::from_slice(&initial.event[6..initial.event.len() - 2]).unwrap();

        for (model, created) in [("provider-a", 2), ("provider-b", 3)] {
            let event = format!(
                "data: {{\"id\":\"cmpl-next\",\"created\":{created},\"model\":\"{model}\",\"choices\":[{{\"index\":0,\"text\":\"two\",\"finish_reason\":null}}]}}\n\n"
            );
            let observation = state.observe_event(event.as_bytes(), true).unwrap();
            let rewritten: Value =
                serde_json::from_slice(&observation.event[6..observation.event.len() - 2]).unwrap();
            assert_eq!(rewritten["id"], "cmpl-first");
            assert_eq!(rewritten["model"], initial["model"]);
            assert_eq!(rewritten["created"], initial["created"]);
        }
    }

    #[test]
    fn response_representation_checks_are_exact_and_case_insensitive() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            "Text/Event-Stream; charset=utf-8".parse().unwrap(),
        );
        assert!(is_event_stream(&headers));
        assert!(has_identity_content_encoding(&headers));

        headers.insert(CONTENT_ENCODING, "IDENTITY".parse().unwrap());
        assert!(has_identity_content_encoding(&headers));

        headers.insert(
            "content-type",
            "application/x-text/event-streamish".parse().unwrap(),
        );
        headers.insert(CONTENT_ENCODING, "gzip".parse().unwrap());
        assert!(!is_event_stream(&headers));
        assert!(!has_identity_content_encoding(&headers));
    }

    #[test]
    fn event_stream_media_type_requires_one_fully_valid_mime_value() {
        for valid in [
            "text/event-stream",
            "Text/Event-Stream; charset=utf-8",
            "TEXT/EVENT-STREAM; Charset=\"utf-8\"",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, valid.parse().unwrap());
            assert!(is_event_stream(&headers), "valid MIME rejected: {valid}");
        }

        for invalid in [
            "text/event-stream;",
            "text/event-stream; charset",
            "text/event-stream; charset=",
            "text/event-stream, application/json",
            "text/event-stream; charset=utf-8, application/json",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, invalid.parse().unwrap());
            assert!(
                !is_event_stream(&headers),
                "invalid MIME accepted: {invalid}"
            );
        }

        let mut headers = HeaderMap::new();
        headers.append(CONTENT_TYPE, "text/event-stream".parse().unwrap());
        headers.append(CONTENT_TYPE, "text/event-stream".parse().unwrap());
        assert!(!is_event_stream(&headers));
    }

    #[test]
    fn unambiguous_choice_less_completion_metadata_captures_initial_identity() {
        let mut state = eligible_state(1024);
        state
            .observe_event(
                br#"data: {"id":"cmpl-first","object":"text_completion","created":1,"model":"first"}

"#,
                false,
            )
            .unwrap();

        let observation = state
            .observe_event(
                br#"data: {"id":"cmpl-next","created":2,"model":"next","choices":[{"index":0,"text":"two","finish_reason":null}]}

"#,
                true,
            )
            .unwrap();
        let rewritten: Value =
            serde_json::from_slice(&observation.event[6..observation.event.len() - 2]).unwrap();
        assert_eq!(rewritten["id"], "cmpl-first");
        assert_eq!(rewritten["model"], "first");
        assert_eq!(rewritten["created"], 1);
    }

    #[test]
    fn multibyte_text_at_the_buffer_boundary_is_retained_but_overflow_is_not() {
        let event =
            "data: {\"choices\":[{\"index\":0,\"text\":\"éé\",\"finish_reason\":null}]}\n\n";

        let mut exact_boundary = eligible_state(4);
        exact_boundary
            .observe_event(event.as_bytes(), false)
            .unwrap();
        let body: Value =
            serde_json::from_slice(&exact_boundary.request_body(None).unwrap()).unwrap();
        assert_eq!(body["prompt"], "Say hello: éé");

        let mut overflow = eligible_state(3);
        overflow.observe_event(event.as_bytes(), false).unwrap();
        assert!(!overflow.is_continuable());
        assert!(overflow.generated_text.is_empty());
    }

    #[test]
    fn chat_continuation_appends_the_emitted_assistant_prefix() {
        let config = StreamContinuationConfig {
            enabled: true,
            endpoints: vec!["/v1/chat/completions".to_string()],
            max_attempts: 1,
            max_buffered_bytes: 1024,
            idle_timeout_ms: None,
        };
        let mut state = StreamContinuation::from_request(
            "/v1/chat/completions",
            &Method::POST,
            br#"{"model":"chat-model","messages":[{"role":"user","content":"Hello"}],"stream":true}"#,
            &config,
        )
        .expect("text-only chat streams should be continuable");

        state
            .observe_event(
                br#"data: {"id":"chatcmpl-first","object":"chat.completion.chunk","created":1,"model":"chat-model","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}

"#,
                false,
            )
            .unwrap();
        state
            .observe_event(
                br#"data: {"id":"chatcmpl-first","object":"chat.completion.chunk","created":1,"model":"chat-model","choices":[{"index":0,"delta":{"content":"Partial answer"},"finish_reason":null}]}

"#,
                false,
            )
            .unwrap();

        let body: Value = serde_json::from_slice(&state.request_body(None).unwrap()).unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(body["messages"][1]["content"], "Partial answer");
    }

    #[test]
    fn responses_continuation_appends_an_assistant_output_item() {
        let config = StreamContinuationConfig {
            enabled: true,
            endpoints: vec!["/v1/responses".to_string()],
            max_attempts: 1,
            max_buffered_bytes: 1024,
            idle_timeout_ms: None,
        };
        let mut state = StreamContinuation::from_request(
            "/v1/responses",
            &Method::POST,
            br#"{"model":"responses-model","input":"Hello","stream":true}"#,
            &config,
        )
        .expect("text-only Responses streams should be continuable");

        state
            .observe_event(
                b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"item_id\":\"msg_first\",\"output_index\":0,\"content_index\":0,\"delta\":\"Partial answer\"}\n\n",
                false,
            )
            .unwrap();

        let body: Value = serde_json::from_slice(&state.request_body(None).unwrap()).unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"], "Hello");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[1]["content"][0]["text"], "Partial answer");
    }

    #[test]
    fn chat_continuation_suppresses_repeated_role_and_reuses_identity() {
        let config = StreamContinuationConfig {
            enabled: true,
            endpoints: vec!["/v1/chat/completions".to_string()],
            max_attempts: 1,
            max_buffered_bytes: 1024,
            idle_timeout_ms: None,
        };
        let mut state = StreamContinuation::from_request(
            "/v1/chat/completions",
            &Method::POST,
            br#"{"model":"requested","messages":[{"role":"user","content":"Hello"}],"stream":true}"#,
            &config,
        )
        .unwrap();
        state
            .observe_event(
                b"data: {\"id\":\"chat-first\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"first\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
                false,
            )
            .unwrap();
        state
            .observe_event(
                b"data: {\"id\":\"chat-first\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"first\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
                false,
            )
            .unwrap();

        let repeated_role = state
            .observe_event(
                b"data: {\"id\":\"chat-second\",\"object\":\"chat.completion.chunk\",\"created\":2,\"model\":\"second\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
                true,
            )
            .unwrap();
        assert!(!repeated_role.forward);

        let continued = state
            .observe_event(
                b"data: {\"id\":\"chat-second\",\"object\":\"chat.completion.chunk\",\"created\":2,\"model\":\"second\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
                true,
            )
            .unwrap();
        let continued: Value =
            serde_json::from_slice(&continued.event[6..continued.event.len() - 2]).unwrap();
        assert_eq!(continued["id"], "chat-first");
        assert_eq!(continued["model"], "first");
        assert_eq!(continued["created"], 1);
        assert_eq!(continued["choices"][0]["delta"]["content"], " world");
    }

    #[test]
    fn chat_tool_or_reasoning_events_disable_continuation() {
        for delta in [
            serde_json::json!({"tool_calls": []}),
            serde_json::json!({"reasoning_content": "thinking"}),
        ] {
            let config = StreamContinuationConfig {
                enabled: true,
                endpoints: vec!["/v1/chat/completions".to_string()],
                max_attempts: 1,
                max_buffered_bytes: 1024,
                idle_timeout_ms: None,
            };
            let mut state = StreamContinuation::from_request(
                "/v1/chat/completions",
                &Method::POST,
                br#"{"messages":[{"role":"user","content":"Hello"}],"stream":true}"#,
                &config,
            )
            .unwrap();
            let event = format!(
                "data: {}\n\n",
                serde_json::json!({
                    "id": "chat-first",
                    "object": "chat.completion.chunk",
                    "created": 1,
                    "model": "first",
                    "choices": [{"index": 0, "delta": delta, "finish_reason": null}]
                })
            );

            let observation = state.observe_event(event.as_bytes(), false).unwrap();
            assert!(!observation.safe);
            assert!(!state.is_continuable());
        }
    }

    #[test]
    fn completion_chat_and_responses_logprobs_events_disable_continuation() {
        let mut completion = eligible_state(1024);
        let completion_event = b"data: {\"id\":\"cmpl-first\",\"object\":\"text_completion\",\"created\":1,\"model\":\"first\",\"choices\":[{\"index\":0,\"text\":\"Hello\",\"finish_reason\":null,\"logprobs\":{\"tokens\":[\"Hello\"]}}]}\n\n";
        assert!(
            !completion
                .observe_event(completion_event, false)
                .unwrap()
                .safe
        );
        assert!(!completion.is_continuable());

        let chat_config = StreamContinuationConfig {
            enabled: true,
            endpoints: vec!["/v1/chat/completions".to_string()],
            max_attempts: 1,
            max_buffered_bytes: 1024,
            idle_timeout_ms: None,
        };
        let mut chat = StreamContinuation::from_request(
            "/v1/chat/completions",
            &Method::POST,
            br#"{"messages":[{"role":"user","content":"Hello"}],"stream":true}"#,
            &chat_config,
        )
        .unwrap();
        let chat_event = b"data: {\"id\":\"chat-first\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"first\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null,\"logprobs\":{\"content\":[]}}]}\n\n";
        assert!(!chat.observe_event(chat_event, false).unwrap().safe);
        assert!(!chat.is_continuable());

        let responses_config = StreamContinuationConfig {
            enabled: true,
            endpoints: vec!["/v1/responses".to_string()],
            max_attempts: 1,
            max_buffered_bytes: 1024,
            idle_timeout_ms: None,
        };
        let mut responses = StreamContinuation::from_request(
            "/v1/responses",
            &Method::POST,
            br#"{"input":"Hello","stream":true}"#,
            &responses_config,
        )
        .unwrap();
        let responses_event = b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"sequence_number\":0,\"item_id\":\"msg_first\",\"output_index\":0,\"content_index\":0,\"delta\":\"Hello\",\"logprobs\":[{}]}\n\n";
        assert!(
            !responses
                .observe_event(responses_event, false)
                .unwrap()
                .safe
        );
        assert!(!responses.is_continuable());
    }

    #[test]
    fn chat_and_responses_reject_non_text_continuation_context() {
        let config = StreamContinuationConfig {
            enabled: true,
            endpoints: vec![
                "/v1/chat/completions".to_string(),
                "/v1/responses".to_string(),
            ],
            max_attempts: 1,
            max_buffered_bytes: 1024,
            idle_timeout_ms: None,
        };

        for request in [
            serde_json::json!({
                "messages": [{"role": "assistant", "content": null, "tool_calls": []}],
                "stream": true
            }),
            serde_json::json!({
                "messages": [{
                    "role": "user",
                    "content": [{"type": "tool_call", "name": "lookup"}]
                }],
                "stream": true
            }),
            serde_json::json!({
                "messages": [{"role": "user", "content": "Hello"}],
                "stream": true,
                "parallel_tool_calls": true
            }),
            serde_json::json!({
                "messages": [{"role": "user", "content": "Hello"}],
                "stream": true,
                "reasoning_effort": "high"
            }),
        ] {
            assert!(
                StreamContinuation::from_request(
                    "/v1/chat/completions",
                    &Method::POST,
                    &serde_json::to_vec(&request).unwrap(),
                    &config,
                )
                .is_none(),
                "unsafe Chat request was eligible: {request}"
            );
        }

        for request in [
            serde_json::json!({
                "input": [{"type": "function_call", "name": "lookup", "arguments": "{}"}],
                "stream": true
            }),
            serde_json::json!({
                "input": [{"type": "reasoning", "summary": []}],
                "stream": true
            }),
            serde_json::json!({
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "reasoning_text", "text": "thinking"}]
                }],
                "stream": true
            }),
            serde_json::json!({
                "input": [{
                    "type": "message",
                    "role": "assistant",
                    "content": "Hello",
                    "tool_calls": []
                }],
                "stream": true
            }),
            serde_json::json!({"input": [], "stream": true}),
            serde_json::json!({"input": "Hello", "stream": true, "text": "invalid"}),
            serde_json::json!({
                "input": "Hello",
                "stream": true,
                "text": {"format": {"type": "json_schema"}}
            }),
        ] {
            assert!(
                StreamContinuation::from_request(
                    "/v1/responses",
                    &Method::POST,
                    &serde_json::to_vec(&request).unwrap(),
                    &config,
                )
                .is_none(),
                "unsafe Responses request was eligible: {request}"
            );
        }
    }

    #[test]
    fn chat_and_responses_accept_supported_message_context() {
        let config = StreamContinuationConfig {
            enabled: true,
            endpoints: vec![
                "/v1/chat/completions".to_string(),
                "/v1/responses".to_string(),
            ],
            max_attempts: 1,
            max_buffered_bytes: 1024,
            idle_timeout_ms: None,
        };
        let chat = serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "Describe this"},
                    {"type": "image_url", "image_url": {"url": "https://example.com/image.png"}}
                ]
            }],
            "stream": true
        });
        let responses = serde_json::json!({
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Hello"}]
                },
                {
                    "type": "message",
                    "id": "msg_prior",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": "Hi",
                        "annotations": [],
                        "logprobs": []
                    }]
                }
            ],
            "stream": true
        });

        assert!(
            StreamContinuation::from_request(
                "/v1/chat/completions",
                &Method::POST,
                &serde_json::to_vec(&chat).unwrap(),
                &config,
            )
            .is_some()
        );
        assert!(
            StreamContinuation::from_request(
                "/v1/responses",
                &Method::POST,
                &serde_json::to_vec(&responses).unwrap(),
                &config,
            )
            .is_some()
        );
    }

    #[test]
    fn responses_continuation_stitches_lifecycle_identity_and_sequence() {
        let config = StreamContinuationConfig {
            enabled: true,
            endpoints: vec!["/v1/responses".to_string()],
            max_attempts: 1,
            max_buffered_bytes: 1024,
            idle_timeout_ms: None,
        };
        let mut state = StreamContinuation::from_request(
            "/v1/responses",
            &Method::POST,
            br#"{"model":"requested","input":"Hello","stream":true}"#,
            &config,
        )
        .unwrap();
        for event in [
            "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_first\",\"model\":\"first\",\"output\":[]}}\n\n",
            "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_first\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n",
            "event: response.content_part.added\ndata: {\"type\":\"response.content_part.added\",\"sequence_number\":2,\"item_id\":\"msg_first\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"\"}}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"item_id\":\"msg_first\",\"output_index\":0,\"content_index\":0,\"delta\":\"Hello\"}\n\n",
        ] {
            assert!(
                state
                    .observe_event(event.as_bytes(), false)
                    .unwrap()
                    .forward
            );
        }

        for event in [
            "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_second\",\"model\":\"second\",\"output\":[]}}\n\n",
            "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_second\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n",
            "event: response.content_part.added\ndata: {\"type\":\"response.content_part.added\",\"sequence_number\":2,\"item_id\":\"msg_second\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"\"}}\n\n",
        ] {
            assert!(!state.observe_event(event.as_bytes(), true).unwrap().forward);
        }

        let delta = state
            .observe_event(
                b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"item_id\":\"msg_second\",\"output_index\":0,\"content_index\":0,\"delta\":\" world\"}\n\n",
                true,
            )
            .unwrap();
        let delta = response_event_json(&delta.event);
        assert_eq!(delta["sequence_number"], 4);
        assert_eq!(delta["item_id"], "msg_first");

        let completed = state
            .observe_event(
                b"event: response.completed\ndata: {\"type\":\"response.completed\",\"sequence_number\":4,\"response\":{\"id\":\"resp_second\",\"model\":\"second\",\"output\":[{\"id\":\"msg_second\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\" world\"}]}]}}\n\n",
                true,
            )
            .unwrap();
        let completed_json = response_event_json(&completed.event);
        assert!(completed.terminal);
        assert_eq!(completed_json["sequence_number"], 5);
        assert_eq!(completed_json["response"]["id"], "resp_first");
        assert_eq!(completed_json["response"]["model"], "first");
        assert_eq!(completed_json["response"]["output"][0]["id"], "msg_first");
        assert_eq!(
            completed_json["response"]["output"][0]["content"][0]["text"],
            "Hello world"
        );
    }

    #[test]
    fn responses_fallback_identity_is_frozen_when_initial_events_omit_ids() {
        let config = StreamContinuationConfig {
            enabled: true,
            endpoints: vec!["/v1/responses".to_string()],
            max_attempts: 1,
            max_buffered_bytes: 1024,
            idle_timeout_ms: None,
        };
        let mut state = StreamContinuation::from_request(
            "/v1/responses",
            &Method::POST,
            br#"{"model":"requested","input":"Hello","stream":true}"#,
            &config,
        )
        .unwrap();

        let created = state
            .observe_event(
                b"event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"model\":\"first\",\"output\":[]}}\n\n",
                false,
            )
            .unwrap();
        let response_id = response_event_json(&created.event)["response"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let in_progress = state
            .observe_event(
                b"event: response.in_progress\ndata: {\"type\":\"response.in_progress\",\"sequence_number\":1,\"response\":{\"id\":\"resp_late\",\"model\":\"late\",\"output\":[]}}\n\n",
                false,
            )
            .unwrap();
        assert_eq!(
            response_event_json(&in_progress.event)["response"]["id"],
            response_id
        );

        let item = state
            .observe_event(
                b"event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"sequence_number\":2,\"output_index\":0,\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                false,
            )
            .unwrap();
        let item_id = response_event_json(&item.event)["item"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let delta = state
            .observe_event(
                b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"item_id\":\"msg_late\",\"output_index\":0,\"content_index\":0,\"delta\":\"Hello\"}\n\n",
                false,
            )
            .unwrap();
        assert_eq!(response_event_json(&delta.event)["item_id"], item_id);
    }

    #[test]
    fn responses_retry_setup_is_suppressed_after_a_sparse_initial_stream() {
        let config = StreamContinuationConfig {
            enabled: true,
            endpoints: vec!["/v1/responses".to_string()],
            max_attempts: 1,
            max_buffered_bytes: 1024,
            idle_timeout_ms: None,
        };
        let mut state = StreamContinuation::from_request(
            "/v1/responses",
            &Method::POST,
            br#"{"model":"requested","input":"Hello","stream":true}"#,
            &config,
        )
        .unwrap();
        state
            .observe_event(
                b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"sequence_number\":0,\"item_id\":\"msg_first\",\"output_index\":0,\"content_index\":0,\"delta\":\"Hello\"}\n\n",
                false,
            )
            .unwrap();

        let created = state
            .observe_event(
                b"event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_second\",\"model\":\"second\",\"output\":[]}}\n\n",
                true,
            )
            .unwrap();

        assert!(!created.forward);
    }

    #[test]
    fn responses_done_only_text_is_preserved_and_disables_continuation() {
        let config = StreamContinuationConfig {
            enabled: true,
            endpoints: vec!["/v1/responses".to_string()],
            max_attempts: 1,
            max_buffered_bytes: 1024,
            idle_timeout_ms: None,
        };
        let mut state = StreamContinuation::from_request(
            "/v1/responses",
            &Method::POST,
            br#"{"model":"requested","input":"Hello","stream":true}"#,
            &config,
        )
        .unwrap();
        let event = b"event: response.output_text.done\ndata: {\"type\":\"response.output_text.done\",\"sequence_number\":0,\"item_id\":\"msg_first\",\"output_index\":0,\"content_index\":0,\"text\":\"Hello\"}\n\n";

        let observation = state.observe_event(event, false).unwrap();

        assert!(!observation.safe);
        assert_eq!(observation.event.as_ref(), event);
        assert!(!state.is_continuable());
    }

    fn response_event_json(event: &[u8]) -> Value {
        let data = event
            .split(|byte| *byte == b'\n')
            .find_map(|line| line.strip_prefix(b"data: "))
            .unwrap();
        serde_json::from_slice(data).unwrap()
    }
}
