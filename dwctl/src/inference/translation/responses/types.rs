//! Open Responses API schemas.
//!
//! Copied from onwards' `strict::schemas::responses` as part of moving Responses
//! ownership into dwctl (onwards' copy retires in COR-536). These schemas match
//! the Open Responses specification. See: https://www.openresponses.org/specification

// Copied wholesale; dwctl doesn't yet exercise every type/helper (e.g. the
// response-field normaliser), so allow dead code rather than trimming the
// faithful copy.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::util::{ensure_field, scrub_request_id_fields_from_extra};

pub(crate) fn generated_response_id() -> String {
    format!("resp_{}", Uuid::new_v4())
}

fn response_status_for_event_type(event_type: &str) -> &'static str {
    match event_type {
        "response.created" | "response.in_progress" => "in_progress",
        "response.completed" => "completed",
        "response.failed" => "failed",
        "response.incomplete" => "incomplete",
        "response.cancelled" | "response.canceled" => "cancelled",
        _ => "completed",
    }
}

fn backfill_responses_response_fields(object: &mut serde_json::Map<String, Value>, fallback_model: &str, fallback_response_id: &str) {
    ensure_field(object, "id", || Value::String(fallback_response_id.to_string()));
    ensure_field(object, "object", || Value::String("response".to_string()));
    ensure_field(object, "created_at", || Value::from(0));
    ensure_field(object, "completed_at", || Value::Null);
    ensure_field(object, "status", || Value::String("completed".to_string()));
    ensure_field(object, "incomplete_details", || Value::Null);
    ensure_field(object, "model", || Value::String(fallback_model.to_string()));
    ensure_field(object, "previous_response_id", || Value::Null);
    ensure_field(object, "instructions", || Value::Null);
    ensure_field(object, "output", || Value::Array(Vec::new()));
    ensure_field(object, "error", || Value::Null);
    ensure_field(object, "tools", || Value::Array(Vec::new()));
    ensure_field(object, "tool_choice", || Value::String("auto".to_string()));
    ensure_field(object, "truncation", || Value::String("disabled".to_string()));
    ensure_field(object, "parallel_tool_calls", || Value::Bool(true));
    ensure_field(object, "text", || {
        serde_json::json!({
            "format": {
                "type": "text"
            }
        })
    });
    ensure_field(object, "top_p", || Value::from(1.0));
    ensure_field(object, "presence_penalty", || Value::from(0.0));
    ensure_field(object, "frequency_penalty", || Value::from(0.0));
    ensure_field(object, "top_logprobs", || Value::from(0));
    ensure_field(object, "temperature", || Value::from(1.0));
    ensure_field(object, "reasoning", || Value::Null);
    ensure_field(object, "usage", || Value::Null);
    ensure_field(object, "max_output_tokens", || Value::Null);
    ensure_field(object, "max_tool_calls", || Value::Null);
    ensure_field(object, "store", || Value::Bool(false));
    ensure_field(object, "background", || Value::Bool(false));
    ensure_field(object, "service_tier", || Value::String("default".to_string()));
    ensure_field(object, "metadata", || Value::Null);
    ensure_field(object, "safety_identifier", || Value::Null);
    ensure_field(object, "prompt_cache_key", || Value::Null);
}

/// Fill in omitted non-critical fields so provider Responses payloads can still
/// round-trip through the strict schema without discarding successful generations.
///
/// This is intentionally separate from serde defaults on `ResponsesResponse`.
/// The struct is used as a strict schema in multiple contexts, and broad serde
/// defaults would silently relax every deserialize path, including internal
/// loads from the response store. We only want that leniency when sanitizing
/// third-party provider payloads for client-facing strict mode.
pub(crate) fn normalize_responses_response_value(value: &mut Value, fallback_model: &str) {
    let Some(object) = value.as_object_mut() else {
        return;
    };

    // Only coerce payloads that already look like Responses API success bodies.
    if !object.contains_key("output") {
        return;
    }

    let fallback_response_id = generated_response_id();
    backfill_responses_response_fields(object, fallback_model, &fallback_response_id);
}

/// Normalize a Responses streaming event, backfilling a missing nested response
/// snapshot with schema-valid defaults when the provider omits bookkeeping fields.
///
/// Streaming needs a dedicated normalizer because some defaults depend on the
/// SSE event type itself, notably `status`.
pub(crate) fn normalize_responses_streaming_event_value(value: &mut Value, fallback_model: &str, fallback_response_id: &str) {
    let Some(object) = value.as_object_mut() else {
        return;
    };

    let event_type = object.get("type").and_then(Value::as_str).unwrap_or_default().to_string();

    if let Some(response) = object.get_mut("response") {
        let missing_status = response
            .as_object()
            .map(|response_object| !response_object.contains_key("status"))
            .unwrap_or(false);

        if let Some(response_object) = response.as_object_mut() {
            if missing_status {
                response_object.insert(
                    "status".to_string(),
                    Value::String(response_status_for_event_type(&event_type).to_string()),
                );
            }

            // Streaming snapshots like response.created may legitimately omit output.
            backfill_responses_response_fields(response_object, fallback_model, fallback_response_id);
        }
    }
}

/// Request body for POST /v1/responses
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesRequest {
    /// The model to use for completion
    pub model: String,

    /// The input to generate a response for - can be a string or array of items (required per Open Responses spec)
    pub input: Input,

    /// Instructions for the model (system prompt)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,

    /// Reference to a previous response for context continuation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,

    /// Whether to store this response for future reference
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,

    /// Metadata key-value pairs
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,

    /// Sampling temperature (0-2)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    /// Nucleus sampling parameter
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,

    /// Maximum output tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,

    /// Stop sequences
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopSequence>,

    /// Whether to stream the response
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,

    /// Tools available to the model
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,

    /// How to choose which tool to use
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,

    /// Parallel tool calls setting
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,

    /// Context truncation strategy
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationStrategy>,

    /// User identifier for abuse tracking
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,

    /// Reasoning configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,

    /// Text generation configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextConfig>,

    /// Whether to run the response in background mode.
    /// When true, the response is returned immediately with status `queued`
    /// and the client polls `GET /v1/responses/{id}` for completion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,

    /// Processing tier: "auto", "default", "priority", "flex", "scale".
    /// Controls routing and pricing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,

    /// Additional fields not explicitly modeled
    #[serde(flatten)]
    pub extra: Option<serde_json::Value>,
}

impl ResponsesRequest {
    /// Remove caller-controlled request identifier fields before proxying.
    pub(crate) fn scrub_request_id_fields(&mut self) {
        scrub_request_id_fields_from_extra(&mut self.extra);
    }
}

/// Input to the model - either a string or array of items
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Input {
    Text(String),
    Items(Vec<Item>),
}

/// An item in the input/output
#[derive(Debug, Clone)]
pub enum Item {
    /// A message item
    Message(MessageItem),

    /// A function call item
    FunctionCall(FunctionCallItem),

    /// A function call output item
    FunctionCallOutput(FunctionCallOutputItem),

    /// A reasoning item
    Reasoning(ReasoningItem),

    /// Unknown item type (for forward compatibility)
    /// Preserves the raw JSON to avoid data loss
    Unknown(serde_json::Value),
}

// Custom deserialization for Item to preserve unknown types
impl<'de> serde::Deserialize<'de> for Item {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // First deserialize to a generic Value
        let value = serde_json::Value::deserialize(deserializer)?;

        // The item `type` is the discriminator (message / function_call /
        // function_call_output / reasoning). Per the Open Responses spec a
        // bare message may omit it: the `EasyInputMessage` shape defaults
        // `type` to "message", so `{"role":"user","content":"hi"}` is valid
        // input. Mirror that default rather than requiring the discriminator
        // — the same default dwctl's own input translator already applies.
        //
        // The default applies only when `type` is absent. A `type` that is
        // present but not a string (null, number, object) is malformed input,
        // not an omitted field, so surface a deserialization error rather than
        // silently misclassifying the item as a message.
        let item_type = match value.get("type") {
            None => "message",
            Some(v) => v
                .as_str()
                .ok_or_else(|| serde::de::Error::custom("Responses input item `type` must be a string"))?,
        };

        // Match against known types
        match item_type {
            "message" => {
                let item: MessageItem = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(Item::Message(item))
            }
            "function_call" => {
                let item: FunctionCallItem = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(Item::FunctionCall(item))
            }
            "function_call_output" => {
                let item: FunctionCallOutputItem = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(Item::FunctionCallOutput(item))
            }
            "reasoning" => {
                let item: ReasoningItem = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(Item::Reasoning(item))
            }
            // Unknown type - preserve the entire raw value
            _ => Ok(Item::Unknown(value)),
        }
    }
}

// Custom serialization for Item to handle unknown types
impl serde::Serialize for Item {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Item::Message(msg) => {
                // Serialize with type tag
                let mut map = serde_json::Map::new();
                map.insert("type".to_string(), serde_json::Value::String("message".to_string()));
                let value = serde_json::to_value(msg).map_err(serde::ser::Error::custom)?;
                if let serde_json::Value::Object(obj) = value {
                    map.extend(obj);
                }
                serde_json::Value::Object(map).serialize(serializer)
            }
            Item::FunctionCall(fc) => {
                let mut map = serde_json::Map::new();
                map.insert("type".to_string(), serde_json::Value::String("function_call".to_string()));
                let value = serde_json::to_value(fc).map_err(serde::ser::Error::custom)?;
                if let serde_json::Value::Object(obj) = value {
                    map.extend(obj);
                }
                serde_json::Value::Object(map).serialize(serializer)
            }
            Item::FunctionCallOutput(fco) => {
                let mut map = serde_json::Map::new();
                map.insert("type".to_string(), serde_json::Value::String("function_call_output".to_string()));
                let value = serde_json::to_value(fco).map_err(serde::ser::Error::custom)?;
                if let serde_json::Value::Object(obj) = value {
                    map.extend(obj);
                }
                serde_json::Value::Object(map).serialize(serializer)
            }
            Item::Reasoning(r) => {
                let mut map = serde_json::Map::new();
                map.insert("type".to_string(), serde_json::Value::String("reasoning".to_string()));
                let value = serde_json::to_value(r).map_err(serde::ser::Error::custom)?;
                if let serde_json::Value::Object(obj) = value {
                    map.extend(obj);
                }
                serde_json::Value::Object(map).serialize(serializer)
            }
            // Unknown type - serialize the raw value as-is
            Item::Unknown(value) => value.serialize(serializer),
        }
    }
}

/// A message item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageItem {
    /// Unique identifier (required in responses, optional in requests)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// The role of the message author
    pub role: String,

    /// The message content
    pub content: MessageContent,

    /// Current status of this item (required in responses, optional in requests)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
}

/// Message content - array of content parts
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

/// A content part in a message
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    /// Text content
    #[serde(rename = "input_text")]
    InputText { text: String },

    /// Output text content
    #[serde(rename = "output_text")]
    OutputText {
        text: String,
        #[serde(default)]
        annotations: Vec<Annotation>,
        #[serde(default)]
        logprobs: Vec<serde_json::Value>,
    },

    /// Input image content
    #[serde(rename = "input_image")]
    InputImage {
        #[serde(skip_serializing_if = "Option::is_none")]
        image_url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },

    /// Input file content
    #[serde(rename = "input_file")]
    InputFile {
        #[serde(skip_serializing_if = "Option::is_none")]
        file_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
    },

    /// Refusal content
    #[serde(rename = "refusal")]
    Refusal { refusal: String },
}

/// Annotation on output text
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Annotation {
    #[serde(rename = "type")]
    pub annotation_type: String,
    #[serde(flatten)]
    pub data: serde_json::Value,
}

/// Function call item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCallItem {
    /// Unique identifier (required in responses, optional in requests)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// The ID to correlate with output
    pub call_id: String,

    /// Function name
    pub name: String,

    /// Function arguments as JSON string
    pub arguments: String,

    /// Current status of this item (required in responses, optional in requests)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
}

/// Function call output item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCallOutputItem {
    /// Unique identifier
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// The call_id this output corresponds to
    pub call_id: String,

    /// The function output as a string
    pub output: String,
}

/// Reasoning item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningItem {
    /// Unique identifier
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Plain text reasoning content (if not encrypted)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ReasoningContent>>,

    /// Encrypted reasoning content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<String>,

    /// Summary of the reasoning
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<Vec<SummaryContent>>,

    /// Current status of this item
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
}

/// Reasoning content part
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ReasoningContent {
    #[serde(rename = "reasoning_text")]
    Text { text: String },
}

/// Summary content part
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SummaryContent {
    #[serde(rename = "summary_text")]
    Text { text: String },
}

/// Item status
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    /// Model is currently emitting tokens
    InProgress,
    /// Token budget exhausted (terminal)
    Incomplete,
    /// Fully sampled (terminal)
    Completed,
}

/// Stop sequence
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StopSequence {
    Single(String),
    Multiple(Vec<String>),
}

/// Default value for strict field in function tools
fn default_strict() -> bool {
    true
}

/// Tool definition
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Tool {
    #[serde(rename = "function")]
    Function {
        /// Function name (required per Open Responses spec)
        name: String,
        /// Function description (required per Open Responses spec)
        description: String,
        /// Function parameters as JSON Schema (required per Open Responses spec)
        parameters: serde_json::Value,
        /// Whether to enforce strict parameter validation (defaults to true)
        #[serde(default = "default_strict")]
        strict: bool,
    },

    /// Code interpreter tool
    #[serde(rename = "code_interpreter")]
    CodeInterpreter {
        #[serde(skip_serializing_if = "Option::is_none")]
        container: Option<serde_json::Value>,
    },

    /// File search tool
    #[serde(rename = "file_search")]
    FileSearch {
        #[serde(skip_serializing_if = "Option::is_none")]
        vector_store_ids: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_num_results: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        filters: Option<serde_json::Value>,
    },

    /// Web search tool
    #[serde(rename = "web_search_preview")]
    WebSearch {
        #[serde(skip_serializing_if = "Option::is_none")]
        user_location: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        search_context_size: Option<String>,
    },

    /// MCP tool
    #[serde(rename = "mcp")]
    Mcp {
        server_label: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        server_url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        headers: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        require_approval: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        allowed_tools: Option<Vec<String>>,
    },

    /// Computer use tool
    #[serde(rename = "computer_use_preview")]
    ComputerUse {
        environment: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        display_width: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        display_height: Option<u32>,
    },

    /// Server-hosted tool — the client opts in by name; the server resolves
    /// the full schema (description, parameters, URL) from its tool registry.
    ///
    /// ```json
    /// {"type": "hosted_tool", "name": "web_search"}
    /// ```
    #[serde(rename = "hosted_tool")]
    HostedTool {
        /// Name of the server-side tool to enable for this request.
        name: String,
    },
}

/// Tool choice specification
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolChoice {
    Mode(String), // "none", "auto", "required"
    Specific {
        #[serde(rename = "type")]
        tool_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

/// Truncation strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncationStrategy {
    Auto,
    Disabled,
}

/// Reasoning configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Text generation configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<TextFormat>,
}

/// Text format configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TextFormat {
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "json_object")]
    JsonObject,
    #[serde(rename = "json_schema")]
    JsonSchema {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        schema: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },
}

/// Response from POST /v1/responses
///
/// All fields are required by the Open Responses specification.
/// Nullable fields must serialize as `null`, not be omitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesResponse {
    /// Unique identifier for this response
    pub id: String,

    /// Object type, always "response"
    pub object: String,

    /// Unix timestamp of creation
    pub created_at: u64,

    /// Unix timestamp of completion (null if not yet completed)
    pub completed_at: Option<u64>,

    /// Response status
    pub status: ResponseStatus,

    /// Details about why response is incomplete (null if complete)
    pub incomplete_details: Option<IncompleteDetails>,

    /// The model used
    pub model: String,

    /// Reference to previous response (null if none)
    pub previous_response_id: Option<String>,

    /// System instructions (null if none)
    pub instructions: Option<String>,

    /// Output items
    pub output: Vec<Item>,

    /// Error information (null if no error)
    pub error: Option<ResponseError>,

    /// Tools available during generation
    pub tools: Vec<Tool>,

    /// How the model chose which tool to use
    pub tool_choice: serde_json::Value,

    /// Context truncation strategy
    pub truncation: TruncationStrategy,

    /// Whether parallel tool calls were allowed
    pub parallel_tool_calls: bool,

    /// Text generation configuration
    pub text: TextConfig,

    /// Nucleus sampling parameter
    pub top_p: f32,

    /// Presence penalty
    pub presence_penalty: f32,

    /// Frequency penalty
    pub frequency_penalty: f32,

    /// Number of top logprobs returned
    pub top_logprobs: u32,

    /// Sampling temperature
    pub temperature: f32,

    /// Reasoning configuration (null if not used)
    /// Uses serde_json::Value to preserve null fields like `effort: null` and `summary: null`
    /// that would otherwise be dropped by skip_serializing_if on the inner ReasoningConfig struct.
    pub reasoning: serde_json::Value,

    /// Token usage (null if not available)
    pub usage: Option<ResponseUsage>,

    /// Maximum output tokens (null if not set)
    pub max_output_tokens: Option<u32>,

    /// Maximum tool calls (null if not set)
    pub max_tool_calls: Option<u32>,

    /// Whether this response was stored
    pub store: bool,

    /// Whether this request ran in background
    pub background: bool,

    /// Service tier used
    pub service_tier: String,

    /// Metadata
    pub metadata: Option<serde_json::Value>,

    /// Safety identifier (null if not set)
    pub safety_identifier: Option<String>,

    /// Prompt cache key (null if not set)
    pub prompt_cache_key: Option<String>,
}

/// A streaming event from POST /v1/responses with stream: true
///
/// All events share `type` and `sequence_number`. Response-level events
/// (response.created, response.in_progress, response.completed, etc.) also
/// carry a full `ResponsesResponse` under `response`. All other event-specific
/// fields (item, delta, output_index, content_index, …) pass through via
/// `#[serde(flatten)]`.
///
/// This is the Responses API analogue of `ChatCompletionChunk` for chat
/// completions streaming.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesStreamingEvent {
    /// The event type, e.g. "response.created", "response.output_text.delta"
    #[serde(rename = "type")]
    pub event_type: String,

    /// Monotonically increasing sequence number for ordering events
    pub sequence_number: u64,

    /// Full response snapshot — present only on response-level events
    /// (response.created, response.in_progress, response.completed,
    ///  response.failed, response.incomplete)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponsesResponse>,

    /// Event-specific fields (item, delta, output_index, content_index, …)
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Response status
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    /// Response is queued for processing (background mode)
    Queued,
    /// Response is being generated
    InProgress,
    /// Response completed successfully
    Completed,
    /// Response is incomplete (e.g., token limit)
    Incomplete,
    /// Response failed
    Failed,
    /// Requires tool action from client
    RequiresAction,
    /// Response was cancelled
    Cancelled,
}

/// Response error
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseError {
    #[serde(rename = "type")]
    pub error_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
}

/// Details about why response is incomplete
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncompleteDetails {
    pub reason: String,
}

/// Token usage for responses
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
    pub input_tokens_details: InputTokensDetails,
    pub output_tokens_details: OutputTokensDetails,
}

/// Details about input tokens
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputTokensDetails {
    pub cached_tokens: u32,
}

/// Details about output tokens
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputTokensDetails {
    pub reasoning_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_simple_request() {
        let json = r#"{
            "model": "gpt-4o",
            "input": "Hello, how are you?"
        }"#;

        let request: ResponsesRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.model, "gpt-4o");
        assert!(matches!(request.input, Input::Text(_)));
    }

    #[test]
    fn test_deserialize_request_with_items() {
        let json = r#"{
            "model": "gpt-4o",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": "What's the weather?"
                }
            ]
        }"#;

        let request: ResponsesRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(request.input, Input::Items(_)));
    }

    #[test]
    fn test_message_item_defaults_type_to_message() {
        // Open Responses `EasyInputMessage`: a bare message may omit the
        // item `type` discriminator, which defaults to "message". A client
        // sending only role + content must parse as a message, not be
        // rejected for a missing `type` field.
        let json = r#"{
            "role": "user",
            "content": "What's the weather?"
        }"#;

        let item: Item = serde_json::from_str(json).unwrap();
        let Item::Message(msg) = item else {
            panic!("expected Item::Message, got {item:?}");
        };
        assert_eq!(msg.role, "user");
        assert!(matches!(msg.content, MessageContent::Text(_)));
    }

    #[test]
    fn test_non_string_item_type_is_rejected() {
        // A `type` that is present but not a string is malformed input, not
        // an omitted discriminator. It must error rather than defaulting to
        // a message (which would misclassify the item).
        for raw in [
            r#"{"type": 123, "role": "user", "content": "hi"}"#,
            r#"{"type": null, "role": "user", "content": "hi"}"#,
            r#"{"type": {"x": 1}, "role": "user", "content": "hi"}"#,
        ] {
            let err = serde_json::from_str::<Item>(raw).unwrap_err();
            assert!(
                err.to_string().contains("`type` must be a string"),
                "expected type-must-be-string error for {raw}, got: {err}"
            );
        }
    }

    #[test]
    fn test_request_with_typeless_message_items_parses() {
        // The same default applies inside a full request's input array:
        // omitting `type` on each message must still yield message items.
        let json = r#"{
            "model": "gpt-4o",
            "input": [
                {"role": "system", "content": "Be terse."},
                {"role": "user", "content": "Hi"}
            ]
        }"#;

        let request: ResponsesRequest = serde_json::from_str(json).unwrap();
        let Input::Items(items) = request.input else {
            panic!("expected Input::Items");
        };
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| matches!(i, Item::Message(_))));
    }

    #[test]
    fn test_deserialize_with_previous_response_id() {
        let json = r#"{
            "model": "gpt-4o",
            "previous_response_id": "resp_abc123",
            "input": "What about tomorrow?"
        }"#;

        let request: ResponsesRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.previous_response_id, Some("resp_abc123".to_string()));
    }

    #[test]
    fn test_deserialize_function_call_output_item() {
        let json = r#"{
            "type": "function_call_output",
            "call_id": "call_123",
            "output": "{\"temperature\": 72}"
        }"#;

        let item: Item = serde_json::from_str(json).unwrap();
        assert!(matches!(item, Item::FunctionCallOutput(_)));
    }

    #[test]
    fn test_serialize_response() {
        let response = ResponsesResponse {
            id: "resp_123".to_string(),
            object: "response".to_string(),
            created_at: 1234567890,
            completed_at: Some(1234567891),
            model: "gpt-4o".to_string(),
            status: ResponseStatus::Completed,
            incomplete_details: None,
            previous_response_id: None,
            instructions: None,
            output: vec![Item::Message(MessageItem {
                id: Some("item_0".to_string()),
                role: "assistant".to_string(),
                content: MessageContent::Text("Hello!".to_string()),
                status: Some(ItemStatus::Completed),
            })],
            error: None,
            tools: vec![],
            tool_choice: serde_json::Value::String("auto".to_string()),
            truncation: TruncationStrategy::Disabled,
            parallel_tool_calls: true,
            text: TextConfig {
                format: Some(TextFormat::Text),
            },
            top_p: 1.0,
            presence_penalty: 0.0,
            frequency_penalty: 0.0,
            top_logprobs: 0,
            temperature: 1.0,
            reasoning: serde_json::Value::Null,
            usage: Some(ResponseUsage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                input_tokens_details: InputTokensDetails { cached_tokens: 0 },
                output_tokens_details: OutputTokensDetails { reasoning_tokens: 0 },
            }),
            max_output_tokens: None,
            max_tool_calls: None,
            store: false,
            background: false,
            service_tier: "default".to_string(),
            metadata: None,
            safety_identifier: None,
            prompt_cache_key: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("resp_123"));
        assert!(json.contains("completed"));
        // Verify nullable fields are present as null
        assert!(json.contains("\"previous_response_id\":null"));
        assert!(json.contains("\"reasoning\":null"));
    }

    #[test]
    fn test_item_status_serialization() {
        assert_eq!(serde_json::to_string(&ItemStatus::InProgress).unwrap(), "\"in_progress\"");
        assert_eq!(serde_json::to_string(&ItemStatus::Completed).unwrap(), "\"completed\"");
    }

    #[test]
    fn test_valid_function_tool_with_all_required_fields() {
        let json = r#"{
            "model": "gpt-4o",
            "input": "test",
            "tools": [{
                "type": "function",
                "name": "add",
                "description": "Add two numbers",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "a": {"type": "number"},
                        "b": {"type": "number"}
                    },
                    "required": ["a", "b"]
                }
            }]
        }"#;

        let request: ResponsesRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.model, "gpt-4o");
        assert!(request.tools.is_some());
        let tools = request.tools.unwrap();
        assert_eq!(tools.len(), 1);

        match &tools[0] {
            Tool::Function {
                name,
                description,
                parameters,
                strict,
            } => {
                assert_eq!(name, "add");
                assert_eq!(description, "Add two numbers");
                assert!(parameters.is_object());
                assert!(*strict); // defaults to true
            }
            _ => panic!("Expected Function tool"),
        }
    }

    #[test]
    fn test_nested_openai_format_tool_is_rejected() {
        // This is the format that previously passed validation but failed downstream
        let json = r#"{
            "model": "gpt-4o",
            "input": "test",
            "tools": [{
                "type": "function",
                "function": {
                    "name": "add",
                    "description": "Add two numbers",
                    "parameters": {"type": "object", "properties": {}}
                }
            }]
        }"#;

        let result: Result<ResponsesRequest, _> = serde_json::from_str(json);
        assert!(result.is_err(), "Nested OpenAI format should be rejected");

        let err_msg = result.unwrap_err().to_string();
        // Should fail because "name" field is missing at the top level
        assert!(
            err_msg.contains("missing field") || err_msg.contains("name"),
            "Error should mention missing required field, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_function_tool_missing_name_is_rejected() {
        let json = r#"{
            "model": "gpt-4o",
            "input": "test",
            "tools": [{
                "type": "function",
                "description": "Some function",
                "parameters": {"type": "object"}
            }]
        }"#;

        let result: Result<ResponsesRequest, _> = serde_json::from_str(json);
        assert!(result.is_err(), "Tool without name should be rejected");

        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("name"),
            "Error should mention missing name field, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_function_tool_missing_description_is_rejected() {
        let json = r#"{
            "model": "gpt-4o",
            "input": "test",
            "tools": [{
                "type": "function",
                "name": "add",
                "parameters": {"type": "object"}
            }]
        }"#;

        let result: Result<ResponsesRequest, _> = serde_json::from_str(json);
        assert!(result.is_err(), "Tool without description should be rejected");

        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("description"),
            "Error should mention missing description field, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_function_tool_missing_parameters_is_rejected() {
        let json = r#"{
            "model": "gpt-4o",
            "input": "test",
            "tools": [{
                "type": "function",
                "name": "add",
                "description": "Add numbers"
            }]
        }"#;

        let result: Result<ResponsesRequest, _> = serde_json::from_str(json);
        assert!(result.is_err(), "Tool without parameters should be rejected");

        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("parameters"),
            "Error should mention missing parameters field, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_request_missing_input_is_rejected() {
        let json = r#"{
            "model": "gpt-4o"
        }"#;

        let result: Result<ResponsesRequest, _> = serde_json::from_str(json);
        assert!(result.is_err(), "Request without input should be rejected");

        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("input"),
            "Error should mention missing input field, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_function_tool_with_empty_parameters_is_valid() {
        // No-argument functions should still work with empty parameter object
        let json = r#"{
            "model": "gpt-4o",
            "input": "test",
            "tools": [{
                "type": "function",
                "name": "get_time",
                "description": "Get current time",
                "parameters": {"type": "object", "properties": {}}
            }]
        }"#;

        let request: ResponsesRequest = serde_json::from_str(json).unwrap();
        assert!(request.tools.is_some());
    }

    #[test]
    fn test_streaming_event_parses_response_created() {
        // response.created carries a full ResponsesResponse under "response"
        let json = r#"{
            "type": "response.created",
            "sequence_number": 0,
            "response": {
                "id": "resp_abc",
                "object": "response",
                "created_at": 1234567890,
                "completed_at": null,
                "status": "in_progress",
                "incomplete_details": null,
                "model": "gpt-4o-mini-2024-07-18",
                "previous_response_id": null,
                "instructions": null,
                "output": [],
                "error": null,
                "tools": [],
                "tool_choice": "auto",
                "truncation": "disabled",
                "parallel_tool_calls": true,
                "text": {"format": {"type": "text"}},
                "top_p": 1.0,
                "presence_penalty": 0.0,
                "frequency_penalty": 0.0,
                "top_logprobs": 0,
                "temperature": 1.0,
                "reasoning": {"effort": null, "summary": null},
                "usage": null,
                "max_output_tokens": null,
                "max_tool_calls": null,
                "store": true,
                "background": false,
                "service_tier": "auto",
                "metadata": {},
                "safety_identifier": null,
                "prompt_cache_key": null
            }
        }"#;

        let event: ResponsesStreamingEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type, "response.created");
        assert_eq!(event.sequence_number, 0);
        let response = event.response.as_ref().unwrap();
        assert_eq!(response.model, "gpt-4o-mini-2024-07-18");
        // reasoning null fields must survive the roundtrip
        assert_eq!(response.reasoning, serde_json::json!({"effort": null, "summary": null}));
    }

    #[test]
    fn test_streaming_event_model_rewrite_roundtrips() {
        // Verify that rewriting response.model and re-serializing produces valid JSON
        let json = r#"{
            "type": "response.completed",
            "sequence_number": 5,
            "response": {
                "id": "resp_xyz",
                "object": "response",
                "created_at": 1234567890,
                "completed_at": 1234567891,
                "status": "completed",
                "incomplete_details": null,
                "model": "gpt-4o-mini-2024-07-18",
                "previous_response_id": null,
                "instructions": null,
                "output": [],
                "error": null,
                "tools": [],
                "tool_choice": "auto",
                "truncation": "disabled",
                "parallel_tool_calls": true,
                "text": {"format": {"type": "text"}},
                "top_p": 1.0,
                "presence_penalty": 0.0,
                "frequency_penalty": 0.0,
                "top_logprobs": 0,
                "temperature": 1.0,
                "reasoning": {"effort": null, "summary": null},
                "usage": null,
                "max_output_tokens": null,
                "max_tool_calls": null,
                "store": true,
                "background": false,
                "service_tier": "default",
                "metadata": {},
                "safety_identifier": null,
                "prompt_cache_key": null
            }
        }"#;

        let mut event: ResponsesStreamingEvent = serde_json::from_str(json).unwrap();
        event.response.as_mut().unwrap().model = "gpt-4o-mini".to_string();

        let out = serde_json::to_string(&event).unwrap();
        let reparsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(reparsed["type"], "response.completed");
        assert_eq!(reparsed["sequence_number"], 5);
        assert_eq!(reparsed["response"]["model"], "gpt-4o-mini");
        // reasoning null fields must survive roundtrip through the typed struct
        assert_eq!(reparsed["response"]["reasoning"]["effort"], serde_json::Value::Null);
        assert_eq!(reparsed["response"]["reasoning"]["summary"], serde_json::Value::Null);
    }

    #[test]
    fn test_streaming_event_parses_delta_without_response() {
        // response.output_text.delta has no "response" field — only event-specific fields
        let json = r#"{
            "type": "response.output_text.delta",
            "sequence_number": 3,
            "item_id": "msg_abc",
            "output_index": 0,
            "content_index": 0,
            "delta": "Hello"
        }"#;

        let event: ResponsesStreamingEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type, "response.output_text.delta");
        assert_eq!(event.sequence_number, 3);
        assert!(event.response.is_none());
        // event-specific fields pass through via flatten
        assert_eq!(event.extra["delta"], "Hello");
        assert_eq!(event.extra["item_id"], "msg_abc");
    }

    #[test]
    fn test_streaming_event_rejects_missing_required_fields() {
        // Events missing type or sequence_number should fail to parse
        let no_type = r#"{"sequence_number": 0, "delta": "hi"}"#;
        assert!(serde_json::from_str::<ResponsesStreamingEvent>(no_type).is_err());

        let no_seq = r#"{"type": "response.output_text.delta", "delta": "hi"}"#;
        assert!(serde_json::from_str::<ResponsesStreamingEvent>(no_seq).is_err());
    }
}
