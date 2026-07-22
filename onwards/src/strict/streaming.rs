//! Streaming event synthesis for Open Responses
//!
//! This module transforms `chat.completion.chunk` SSE events from upstream providers
//! into Open Responses semantic streaming events.
//!
//! ## Event Sequence
//!
//! For each output item, the following events are generated in order:
//!
//! 1. `response.output_item.added` - Item created
//! 2. `response.content_part.added` - Content part started
//! 3. `response.output_text.delta` - Text deltas (repeating)
//! 4. `response.content_part.done` - Content part finished
//! 5. `response.output_item.done` - Item finished
//!
//! At the end of the response:
//! 6. `response.completed` - Full response complete

use super::schemas::chat_completions::{ChatCompletionChunk, ChunkChoice};
use super::schemas::responses::{
    ContentPart, FunctionCallItem, Item, ItemStatus, MessageContent, MessageItem, ReasoningContent,
    ReasoningItem, ResponseStatus, ResponseUsage, ResponsesRequest, ResponsesResponse,
    SummaryContent, TextConfig, TextFormat, TruncationStrategy,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// State machine for tracking streaming response state
#[derive(Debug, Clone)]
pub struct StreamingState {
    /// Unique response ID
    response_id: String,
    /// Model being used
    model: String,
    /// Creation timestamp
    created_at: u64,
    /// Current iteration's output items being built (message items and function call items)
    items: Vec<StreamingItem>,
    /// Items from previous tool-loop iterations (already emitted to client)
    completed_items: Vec<StreamingItem>,
    /// Offset added to item indices so IDs and output_index values are unique across iterations
    item_index_offset: usize,
    /// Sequence number for events
    sequence_number: u32,
    /// Whether we've seen the first chunk
    started: bool,
    /// Accumulated usage (from final chunk if include_usage is set)
    usage: Option<ResponseUsage>,
    /// Original request parameters (for echoing back in response.completed)
    request: ResponsesRequest,
    /// Maps choice_index → items-vec index for message items
    msg_item_for_choice: HashMap<usize, usize>,
    /// Maps (choice_index, tc_index) → items-vec index for function call items
    fn_call_item_for: HashMap<(usize, usize), usize>,
    /// Maps choice_index → items-vec index for reasoning items
    reasoning_item_for_choice: HashMap<usize, usize>,
}

/// The inner content of a streaming output item
#[derive(Debug, Clone)]
enum StreamingItemKind {
    Message {
        role: String,
        content_text: String,
        content_part_started: bool,
        /// Tracks the current content part index (always 0 today; reserved for future multi-part)
        content_index: u32,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    Reasoning {
        summary_text: String,
        summary_part_started: bool,
    },
}

/// A streaming item being built incrementally
#[derive(Debug, Clone)]
struct StreamingItem {
    id: String,
    status: ItemStatus,
    kind: StreamingItemKind,
}

/// Generate a unique response ID using timestamp and random component
///
/// Format: resp_{timestamp_hex}_{random_hex}
/// This avoids ID collisions across server restarts and concurrent requests.
fn generate_response_id() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    // Use rng for better randomness distribution under high load
    use rand::Rng;
    let random: u64 = rand::rng().random();

    format!("resp_{:016x}_{:08x}", timestamp, random)
}

impl StreamingState {
    /// Create a new streaming state for a response
    pub fn new(request: &ResponsesRequest) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Self {
            response_id: generate_response_id(),
            model: request.model.clone(),
            created_at: timestamp,
            items: Vec::new(),
            completed_items: Vec::new(),
            item_index_offset: 0,
            sequence_number: 0,
            started: false,
            usage: None,
            request: request.clone(),
            msg_item_for_choice: HashMap::new(),
            fn_call_item_for: HashMap::new(),
            reasoning_item_for_choice: HashMap::new(),
        }
    }

    /// Override the generated response ID with a caller-supplied value.
    pub fn set_response_id(&mut self, id: String) {
        self.response_id = id;
    }

    /// Process a chat completion chunk and return semantic events
    pub fn process_chunk(&mut self, chunk: &ChatCompletionChunk) -> Vec<StreamingEvent> {
        let mut events = Vec::new();

        // Emit response.created on first chunk
        if !self.started {
            self.started = true;
            events.push(self.create_response_created_event());
        }

        // Process each choice in the chunk
        for choice in &chunk.choices {
            let choice_events = self.process_choice(choice);
            events.extend(choice_events);
        }

        // Store usage if present (typically in final chunk)
        if let Some(ref usage) = chunk.usage {
            self.usage = Some(super::chat_usage_to_response_usage(usage));
        }

        // Check for finish_reason to emit done events
        for choice in &chunk.choices {
            if choice.finish_reason.is_some() {
                let done_events = self.finalize_for_choice(choice.index as usize);
                events.extend(done_events);
            }
        }

        events
    }

    /// Finalize the response and emit completion event
    pub fn finalize(&mut self) -> Vec<StreamingEvent> {
        let mut events = Vec::new();

        // Finalize any remaining items
        for i in 0..self.items.len() {
            if self.items[i].status == ItemStatus::InProgress {
                let done_events = self.finalize_item(i);
                events.extend(done_events);
            }
        }

        // Emit response.completed
        events.push(self.create_response_completed_event());

        events
    }

    /// Extract tool calls accumulated during the current iteration.
    ///
    /// Returns `(call_id, function_name, arguments)` tuples.
    pub fn extract_tool_calls(&self) -> Vec<(String, String, String)> {
        self.items
            .iter()
            .filter_map(|item| {
                if let StreamingItemKind::FunctionCall {
                    call_id,
                    name,
                    arguments,
                } = &item.kind
                {
                    if !call_id.is_empty() {
                        Some((call_id.clone(), name.clone(), arguments.clone()))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect()
    }

    /// Prepare the state machine for the next tool-loop iteration.
    ///
    /// Moves the current iteration's items into `completed_items` so that
    /// `build_final_response` can include all items across iterations, and
    /// advances the index offset so new items get unique IDs and output indices.
    pub fn prepare_next_iteration(&mut self) {
        self.item_index_offset += self.items.len();
        self.completed_items.append(&mut self.items);
        self.msg_item_for_choice.clear();
        self.fn_call_item_for.clear();
        self.reasoning_item_for_choice.clear();
    }

    /// Process a single choice from a chunk
    fn process_choice(&mut self, choice: &ChunkChoice) -> Vec<StreamingEvent> {
        let mut events = Vec::new();
        let choice_index = choice.index as usize;
        let delta = &choice.delta;

        // --- Reasoning item ---
        let reasoning_merged = super::merge_reasoning_text(
            delta.reasoning.as_ref(),
            delta.reasoning_content.as_ref(),
            delta.reasoning_details.as_ref(),
        );
        let reasoning_delta: Option<&str> = if reasoning_merged.is_empty() {
            None
        } else {
            Some(reasoning_merged.as_str())
        };

        if let Some(reasoning_text) = reasoning_delta {
            let r_idx = if let Some(&idx) = self.reasoning_item_for_choice.get(&choice_index) {
                idx
            } else {
                let idx = self.items.len();
                let item = StreamingItem {
                    id: format!("item_{}", self.item_index_offset + idx),
                    status: ItemStatus::InProgress,
                    kind: StreamingItemKind::Reasoning {
                        summary_text: String::new(),
                        summary_part_started: false,
                    },
                };
                self.items.push(item);
                self.reasoning_item_for_choice.insert(choice_index, idx);
                events.push(self.create_item_added_event(idx));
                idx
            };

            let mut emit_summary_part_added = false;
            if let StreamingItemKind::Reasoning {
                summary_text,
                summary_part_started,
            } = &mut self.items[r_idx].kind
            {
                if !*summary_part_started {
                    *summary_part_started = true;
                    emit_summary_part_added = true;
                }
                summary_text.push_str(reasoning_text);
            }

            if emit_summary_part_added {
                events.push(self.create_reasoning_summary_part_added_event(r_idx));
            }
            events.push(self.create_reasoning_summary_text_delta_event(r_idx, reasoning_text));
        }

        // --- Message item (text content / role) ---
        let mut emit_content_part_added = false;
        let mut emit_text_delta: Option<String> = None;

        let has_content = delta
            .content
            .as_ref()
            .map(|s| !s.is_empty())
            .unwrap_or(false);

        if has_content {
            // Find or create the message item for this choice
            let msg_idx = if let Some(&idx) = self.msg_item_for_choice.get(&choice_index) {
                idx
            } else {
                let idx = self.items.len();
                let item = StreamingItem {
                    id: format!("item_{}", self.item_index_offset + idx),
                    status: ItemStatus::InProgress,
                    kind: StreamingItemKind::Message {
                        role: String::new(),
                        content_text: String::new(),
                        content_part_started: false,
                        content_index: 0,
                    },
                };
                self.items.push(item);
                self.msg_item_for_choice.insert(choice_index, idx);
                events.push(self.create_item_added_event(idx));
                idx
            };

            if let StreamingItemKind::Message {
                role,
                content_text,
                content_part_started,
                ..
            } = &mut self.items[msg_idx].kind
            {
                if let Some(ref r) = delta.role {
                    *role = r.clone();
                }
                if let Some(ref content) = delta.content
                    && !content.is_empty()
                {
                    if !*content_part_started {
                        *content_part_started = true;
                        emit_content_part_added = true;
                    }
                    content_text.push_str(content);
                    emit_text_delta = Some(content.clone());
                }
            }

            if emit_content_part_added {
                events.push(self.create_content_part_added_event(msg_idx));
            }
            if let Some(ref text) = emit_text_delta {
                events.push(self.create_text_delta_event(msg_idx, text));
            }
        }

        // --- Function call items ---
        if let Some(ref tool_calls) = delta.tool_calls {
            for tc in tool_calls {
                let tc_index = tc.index as usize;
                let key = (choice_index, tc_index);

                // Find or create the function call item
                let fc_idx = if let Some(&idx) = self.fn_call_item_for.get(&key) {
                    idx
                } else {
                    let idx = self.items.len();
                    let item = StreamingItem {
                        id: format!("item_{}", self.item_index_offset + idx),
                        status: ItemStatus::InProgress,
                        kind: StreamingItemKind::FunctionCall {
                            call_id: String::new(),
                            name: String::new(),
                            arguments: String::new(),
                        },
                    };
                    self.items.push(item);
                    self.fn_call_item_for.insert(key, idx);
                    events.push(self.create_item_added_event(idx));
                    idx
                };

                // Collect mutations and events to emit
                let mut emit_args_delta: Option<String> = None;

                if let StreamingItemKind::FunctionCall {
                    call_id,
                    name,
                    arguments,
                } = &mut self.items[fc_idx].kind
                {
                    if let Some(ref id) = tc.id {
                        *call_id = id.clone();
                    }
                    if let Some(ref func) = tc.function {
                        if let Some(ref n) = func.name {
                            *name = n.clone();
                        }
                        if let Some(ref args) = func.arguments
                            && !args.is_empty()
                        {
                            arguments.push_str(args);
                            emit_args_delta = Some(args.clone());
                        }
                    }
                }

                if let Some(delta) = emit_args_delta {
                    events.push(self.create_fn_call_arguments_delta_event(fc_idx, &delta));
                }
            }
        }

        events
    }

    /// Finalize all items associated with a choice and emit done events
    fn finalize_for_choice(&mut self, choice_index: usize) -> Vec<StreamingEvent> {
        let mut indices: Vec<usize> = Vec::new();
        if let Some(&idx) = self.reasoning_item_for_choice.get(&choice_index) {
            indices.push(idx);
        }
        if let Some(&idx) = self.msg_item_for_choice.get(&choice_index) {
            indices.push(idx);
        }
        // Collect fn call indices for this choice
        let fc_indices: Vec<usize> = self
            .fn_call_item_for
            .iter()
            .filter(|((ci, _), _)| *ci == choice_index)
            .map(|(_, &idx)| idx)
            .collect();
        indices.extend(fc_indices);
        indices.sort_unstable();

        let mut events = Vec::new();
        for idx in indices {
            events.extend(self.finalize_item(idx));
        }
        events
    }

    /// Finalize a single item by index and emit done events
    fn finalize_item(&mut self, index: usize) -> Vec<StreamingEvent> {
        if index >= self.items.len() || self.items[index].status != ItemStatus::InProgress {
            return vec![];
        }
        self.items[index].status = ItemStatus::Completed;

        match &self.items[index].kind {
            StreamingItemKind::Message {
                content_part_started,
                ..
            } => {
                let had_content = *content_part_started;
                let mut events = Vec::new();
                if had_content {
                    events.push(self.create_content_part_done_event(index));
                }
                events.push(self.create_item_done_event(index));
                events
            }
            StreamingItemKind::FunctionCall { .. } => {
                vec![
                    self.create_fn_call_arguments_done_event(index),
                    self.create_item_done_event(index),
                ]
            }
            StreamingItemKind::Reasoning {
                summary_part_started,
                ..
            } => {
                let had_part = *summary_part_started;
                let mut events = Vec::new();
                if had_part {
                    events.push(self.create_reasoning_summary_text_done_event(index));
                    events.push(self.create_reasoning_summary_part_done_event(index));
                }
                events.push(self.create_item_done_event(index));
                events
            }
        }
    }

    fn next_sequence(&mut self) -> u32 {
        let seq = self.sequence_number;
        self.sequence_number += 1;
        seq
    }

    fn create_response_created_event(&mut self) -> StreamingEvent {
        StreamingEvent {
            event_type: "response.created".to_string(),
            data: StreamingEventData::ResponseCreated {
                response: self.build_response_snapshot(ResponseStatus::InProgress, vec![], None),
            },
            sequence_number: self.next_sequence(),
        }
    }

    fn create_item_added_event(&mut self, index: usize) -> StreamingEvent {
        let item = &self.items[index];
        let output_index = (self.item_index_offset + index) as u32;
        let output_item = match &item.kind {
            StreamingItemKind::Message { role, .. } => Item::Message(MessageItem {
                id: Some(item.id.clone()),
                role: if role.is_empty() {
                    "assistant".to_string()
                } else {
                    role.clone()
                },
                content: MessageContent::Parts(vec![]),
                status: Some(ItemStatus::InProgress),
            }),
            StreamingItemKind::FunctionCall { call_id, name, .. } => {
                Item::FunctionCall(FunctionCallItem {
                    id: Some(item.id.clone()),
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments: String::new(),
                    status: Some(ItemStatus::InProgress),
                })
            }
            StreamingItemKind::Reasoning { .. } => Item::Reasoning(ReasoningItem {
                id: Some(item.id.clone()),
                content: None,
                encrypted_content: None,
                summary: Some(vec![]),
                status: Some(ItemStatus::InProgress),
            }),
        };
        StreamingEvent {
            event_type: "response.output_item.added".to_string(),
            data: StreamingEventData::OutputItemAdded {
                output_index,
                item: output_item,
            },
            sequence_number: self.next_sequence(),
        }
    }

    fn create_content_part_added_event(&mut self, index: usize) -> StreamingEvent {
        let content_index = match &self.items[index].kind {
            StreamingItemKind::Message { content_index, .. } => *content_index,
            StreamingItemKind::FunctionCall { .. } | StreamingItemKind::Reasoning { .. } => 0,
        };
        StreamingEvent {
            event_type: "response.content_part.added".to_string(),
            data: StreamingEventData::ContentPartAdded {
                item_id: self.items[index].id.clone(),
                output_index: (self.item_index_offset + index) as u32,
                content_index,
                part: ContentPart::OutputText {
                    text: String::new(),
                    annotations: vec![],
                    logprobs: vec![],
                },
            },
            sequence_number: self.next_sequence(),
        }
    }

    fn create_text_delta_event(&mut self, index: usize, delta: &str) -> StreamingEvent {
        let content_index = match &self.items[index].kind {
            StreamingItemKind::Message { content_index, .. } => *content_index,
            StreamingItemKind::FunctionCall { .. } | StreamingItemKind::Reasoning { .. } => 0,
        };
        StreamingEvent {
            event_type: "response.output_text.delta".to_string(),
            data: StreamingEventData::OutputTextDelta {
                item_id: self.items[index].id.clone(),
                output_index: (self.item_index_offset + index) as u32,
                content_index,
                delta: delta.to_string(),
                logprobs: vec![],
            },
            sequence_number: self.next_sequence(),
        }
    }

    fn create_content_part_done_event(&mut self, index: usize) -> StreamingEvent {
        let (content_text, content_index) = match &self.items[index].kind {
            StreamingItemKind::Message {
                content_text,
                content_index,
                ..
            } => (content_text.clone(), *content_index),
            StreamingItemKind::FunctionCall { .. } | StreamingItemKind::Reasoning { .. } => {
                (String::new(), 0)
            }
        };
        StreamingEvent {
            event_type: "response.content_part.done".to_string(),
            data: StreamingEventData::ContentPartDone {
                item_id: self.items[index].id.clone(),
                output_index: (self.item_index_offset + index) as u32,
                content_index,
                part: ContentPart::OutputText {
                    text: content_text,
                    annotations: vec![],
                    logprobs: vec![],
                },
            },
            sequence_number: self.next_sequence(),
        }
    }

    fn create_item_done_event(&mut self, index: usize) -> StreamingEvent {
        let item = &self.items[index];
        let output_index = (self.item_index_offset + index) as u32;
        let output_item = match &item.kind {
            StreamingItemKind::Message {
                role, content_text, ..
            } => Item::Message(MessageItem {
                id: Some(item.id.clone()),
                role: if role.is_empty() {
                    "assistant".to_string()
                } else {
                    role.clone()
                },
                content: MessageContent::Parts(vec![ContentPart::OutputText {
                    text: content_text.clone(),
                    annotations: vec![],
                    logprobs: vec![],
                }]),
                status: Some(ItemStatus::Completed),
            }),
            StreamingItemKind::FunctionCall {
                call_id,
                name,
                arguments,
            } => Item::FunctionCall(FunctionCallItem {
                id: Some(item.id.clone()),
                call_id: call_id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
                status: Some(ItemStatus::Completed),
            }),
            StreamingItemKind::Reasoning { summary_text, .. } => Item::Reasoning(ReasoningItem {
                id: Some(item.id.clone()),
                content: Some(vec![ReasoningContent::Text {
                    text: summary_text.clone(),
                }]),
                encrypted_content: None,
                summary: Some(vec![SummaryContent::Text {
                    text: summary_text.clone(),
                }]),
                status: Some(ItemStatus::Completed),
            }),
        };
        StreamingEvent {
            event_type: "response.output_item.done".to_string(),
            data: StreamingEventData::OutputItemDone {
                output_index,
                item: output_item,
            },
            sequence_number: self.next_sequence(),
        }
    }

    fn create_reasoning_summary_part_added_event(&mut self, index: usize) -> StreamingEvent {
        StreamingEvent {
            event_type: "response.reasoning_summary_part.added".to_string(),
            data: StreamingEventData::ReasoningSummaryPartAdded {
                item_id: self.items[index].id.clone(),
                output_index: (self.item_index_offset + index) as u32,
                summary_index: 0,
                part: SummaryContent::Text {
                    text: String::new(),
                },
            },
            sequence_number: self.next_sequence(),
        }
    }

    fn create_reasoning_summary_text_delta_event(
        &mut self,
        index: usize,
        delta: &str,
    ) -> StreamingEvent {
        StreamingEvent {
            event_type: "response.reasoning_summary_text.delta".to_string(),
            data: StreamingEventData::ReasoningSummaryTextDelta {
                item_id: self.items[index].id.clone(),
                output_index: (self.item_index_offset + index) as u32,
                summary_index: 0,
                delta: delta.to_string(),
            },
            sequence_number: self.next_sequence(),
        }
    }

    fn create_reasoning_summary_text_done_event(&mut self, index: usize) -> StreamingEvent {
        let text = match &self.items[index].kind {
            StreamingItemKind::Reasoning { summary_text, .. } => summary_text.clone(),
            _ => String::new(),
        };
        StreamingEvent {
            event_type: "response.reasoning_summary_text.done".to_string(),
            data: StreamingEventData::ReasoningSummaryTextDone {
                item_id: self.items[index].id.clone(),
                output_index: (self.item_index_offset + index) as u32,
                summary_index: 0,
                text,
            },
            sequence_number: self.next_sequence(),
        }
    }

    fn create_reasoning_summary_part_done_event(&mut self, index: usize) -> StreamingEvent {
        let text = match &self.items[index].kind {
            StreamingItemKind::Reasoning { summary_text, .. } => summary_text.clone(),
            _ => String::new(),
        };
        StreamingEvent {
            event_type: "response.reasoning_summary_part.done".to_string(),
            data: StreamingEventData::ReasoningSummaryPartDone {
                item_id: self.items[index].id.clone(),
                output_index: (self.item_index_offset + index) as u32,
                summary_index: 0,
                part: SummaryContent::Text { text },
            },
            sequence_number: self.next_sequence(),
        }
    }

    fn create_fn_call_arguments_delta_event(
        &mut self,
        index: usize,
        delta: &str,
    ) -> StreamingEvent {
        StreamingEvent {
            event_type: "response.function_call_arguments.delta".to_string(),
            data: StreamingEventData::FunctionCallArgumentsDelta {
                item_id: self.items[index].id.clone(),
                output_index: (self.item_index_offset + index) as u32,
                delta: delta.to_string(),
            },
            sequence_number: self.next_sequence(),
        }
    }

    fn create_fn_call_arguments_done_event(&mut self, index: usize) -> StreamingEvent {
        let arguments = match &self.items[index].kind {
            StreamingItemKind::FunctionCall { arguments, .. } => arguments.clone(),
            StreamingItemKind::Message { .. } | StreamingItemKind::Reasoning { .. } => {
                String::new()
            }
        };
        StreamingEvent {
            event_type: "response.function_call_arguments.done".to_string(),
            data: StreamingEventData::FunctionCallArgumentsDone {
                item_id: self.items[index].id.clone(),
                output_index: (self.item_index_offset + index) as u32,
                arguments,
            },
            sequence_number: self.next_sequence(),
        }
    }

    fn create_response_completed_event(&mut self) -> StreamingEvent {
        StreamingEvent {
            event_type: "response.completed".to_string(),
            data: StreamingEventData::ResponseCompleted {
                response: self.build_final_response(),
            },
            sequence_number: self.next_sequence(),
        }
    }

    /// Build a response snapshot with the given status, output, and usage
    fn build_response_snapshot(
        &self,
        status: ResponseStatus,
        output: Vec<Item>,
        usage: Option<ResponseUsage>,
    ) -> ResponsesResponse {
        let req = &self.request;

        let tool_choice = req
            .tool_choice
            .as_ref()
            .and_then(|tc| serde_json::to_value(tc).ok())
            .unwrap_or(serde_json::Value::String("auto".to_string()));

        let completed_at = if status == ResponseStatus::Completed {
            Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            )
        } else {
            None
        };

        ResponsesResponse {
            id: self.response_id.clone(),
            object: "response".to_string(),
            created_at: self.created_at,
            completed_at,
            status,
            incomplete_details: None,
            model: self.model.clone(),
            previous_response_id: req.previous_response_id.clone(),
            instructions: req.instructions.clone(),
            output,
            error: None,
            tools: req.tools.clone().unwrap_or_default(),
            tool_choice,
            truncation: req
                .truncation
                .clone()
                .unwrap_or(TruncationStrategy::Disabled),
            parallel_tool_calls: req.parallel_tool_calls.unwrap_or(true),
            text: req.text.clone().unwrap_or(TextConfig {
                format: Some(TextFormat::Text),
            }),
            top_p: req.top_p.unwrap_or(1.0),
            presence_penalty: 0.0,
            frequency_penalty: 0.0,
            top_logprobs: 0,
            temperature: req.temperature.unwrap_or(1.0),
            reasoning: serde_json::to_value(&req.reasoning).unwrap_or(serde_json::Value::Null),
            usage,
            max_output_tokens: req.max_output_tokens,
            max_tool_calls: None,
            store: req.store.unwrap_or(false),
            background: false,
            service_tier: "default".to_string(),
            metadata: req.metadata.clone(),
            safety_identifier: None,
            prompt_cache_key: None,
        }
    }

    /// Build the final response object
    fn build_final_response(&self) -> ResponsesResponse {
        let output: Vec<Item> = self
            .completed_items
            .iter()
            .chain(self.items.iter())
            .map(|item| match &item.kind {
                StreamingItemKind::Message {
                    role, content_text, ..
                } => Item::Message(MessageItem {
                    id: Some(item.id.clone()),
                    role: if role.is_empty() {
                        "assistant".to_string()
                    } else {
                        role.clone()
                    },
                    content: MessageContent::Parts(vec![ContentPart::OutputText {
                        text: content_text.clone(),
                        annotations: vec![],
                        logprobs: vec![],
                    }]),
                    status: Some(item.status),
                }),
                StreamingItemKind::FunctionCall {
                    call_id,
                    name,
                    arguments,
                } => Item::FunctionCall(FunctionCallItem {
                    id: Some(item.id.clone()),
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                    status: Some(item.status),
                }),
                StreamingItemKind::Reasoning { summary_text, .. } => {
                    Item::Reasoning(ReasoningItem {
                        id: Some(item.id.clone()),
                        content: Some(vec![ReasoningContent::Text {
                            text: summary_text.clone(),
                        }]),
                        encrypted_content: None,
                        summary: Some(vec![SummaryContent::Text {
                            text: summary_text.clone(),
                        }]),
                        status: Some(item.status),
                    })
                }
            })
            .collect();

        self.build_response_snapshot(ResponseStatus::Completed, output, self.usage.clone())
    }
}

/// A semantic streaming event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingEvent {
    /// Event type (e.g., "response.output_text.delta")
    #[serde(rename = "type")]
    pub event_type: String,
    /// Event data
    #[serde(flatten)]
    pub data: StreamingEventData,
    /// Sequence number for ordering
    pub sequence_number: u32,
}

impl StreamingEvent {
    /// Format as SSE event
    pub fn to_sse(&self) -> String {
        let json = serde_json::to_string(&self).unwrap_or_default();
        format!("event: {}\ndata: {}\n\n", self.event_type, json)
    }
}

/// Event data variants
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StreamingEventData {
    ResponseCreated {
        response: ResponsesResponse,
    },
    OutputItemAdded {
        output_index: u32,
        item: Item,
    },
    ContentPartAdded {
        item_id: String,
        output_index: u32,
        content_index: u32,
        part: ContentPart,
    },
    OutputTextDelta {
        item_id: String,
        output_index: u32,
        content_index: u32,
        delta: String,
        logprobs: Vec<serde_json::Value>,
    },
    ContentPartDone {
        item_id: String,
        output_index: u32,
        content_index: u32,
        part: ContentPart,
    },
    OutputItemDone {
        output_index: u32,
        item: Item,
    },
    FunctionCallArgumentsDelta {
        item_id: String,
        output_index: u32,
        delta: String,
    },
    FunctionCallArgumentsDone {
        item_id: String,
        output_index: u32,
        arguments: String,
    },
    ReasoningSummaryPartAdded {
        item_id: String,
        output_index: u32,
        summary_index: u32,
        part: SummaryContent,
    },
    ReasoningSummaryTextDelta {
        item_id: String,
        output_index: u32,
        summary_index: u32,
        delta: String,
    },
    ReasoningSummaryTextDone {
        item_id: String,
        output_index: u32,
        summary_index: u32,
        text: String,
    },
    ReasoningSummaryPartDone {
        item_id: String,
        output_index: u32,
        summary_index: u32,
        part: SummaryContent,
    },
    ResponseCompleted {
        response: ResponsesResponse,
    },
}

/// Parse a chat.completion.chunk from SSE data line
pub fn parse_chat_chunk(data: &str) -> Option<ChatCompletionChunk> {
    if data.trim() == "[DONE]" {
        return None;
    }
    serde_json::from_str(data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strict::schemas::chat_completions::{
        ChunkChoice, ChunkDelta, ChunkFunctionCall, ChunkToolCall,
    };
    use crate::strict::schemas::responses::Input;

    fn test_request(model: &str) -> ResponsesRequest {
        ResponsesRequest {
            model: model.to_string(),
            input: Input::Text(String::new()),
            instructions: None,
            previous_response_id: None,
            store: None,
            metadata: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            stop: None,
            stream: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            truncation: None,
            user: None,
            reasoning: None,
            text: None,
            background: None,
            service_tier: None,
            extra: None,
        }
    }

    fn create_test_chunk(
        id: &str,
        content: Option<&str>,
        role: Option<&str>,
        finish_reason: Option<&str>,
    ) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: id.to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: role.map(String::from),
                    content: content.map(String::from),
                    tool_calls: None,
                    reasoning: None,
                    reasoning_content: None,
                    reasoning_details: None,
                },
                finish_reason: finish_reason.map(String::from),
                logprobs: None,
            }],
            usage: None,
            system_fingerprint: None,
            service_tier: None,
        }
    }

    #[test]
    fn test_streaming_state_initial_events() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        // Role-only delta should only emit response.created (message item
        // is deferred until content arrives to avoid empty items)
        let chunk = create_test_chunk("chunk_1", None, Some("assistant"), None);
        let events = state.process_chunk(&chunk);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "response.created");

        // Content delta creates the message item
        let chunk2 = create_test_chunk("chunk_2", Some("Hi"), None, None);
        let events2 = state.process_chunk(&chunk2);
        assert_eq!(events2[0].event_type, "response.output_item.added");
    }

    #[test]
    fn test_streaming_state_content_events() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        // First chunk with role only — deferred, no message item yet
        let chunk1 = create_test_chunk("chunk_1", None, Some("assistant"), None);
        state.process_chunk(&chunk1);

        // Content chunk creates message item + content
        let chunk2 = create_test_chunk("chunk_2", Some("Hello"), None, None);
        let events = state.process_chunk(&chunk2);

        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event_type, "response.output_item.added");
        assert_eq!(events[1].event_type, "response.content_part.added");
        assert_eq!(events[2].event_type, "response.output_text.delta");
    }

    #[test]
    fn test_streaming_state_completion_events() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        // First chunk
        let chunk1 = create_test_chunk("chunk_1", Some("Hi"), Some("assistant"), None);
        state.process_chunk(&chunk1);

        // Final chunk with finish_reason
        let chunk2 = create_test_chunk("chunk_2", None, None, Some("stop"));
        let events = state.process_chunk(&chunk2);

        // Should emit content_part.done and output_item.done
        assert!(
            events
                .iter()
                .any(|e| e.event_type == "response.content_part.done")
        );
        assert!(
            events
                .iter()
                .any(|e| e.event_type == "response.output_item.done")
        );
    }

    #[test]
    fn test_streaming_state_preserves_usage_token_details() {
        use super::super::schemas::chat_completions::Usage;

        let mut state = StreamingState::new(&test_request("gpt-4"));

        // Content delta (no usage yet).
        let chunk1 = create_test_chunk("chunk_1", Some("Hi"), Some("assistant"), None);
        state.process_chunk(&chunk1);

        // Final chunk with usage carrying the detail fields providers send for
        // prompt caching and thinking models.
        let mut chunk2 = create_test_chunk("chunk_2", None, None, Some("stop"));
        chunk2.usage = Some(Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            prompt_tokens_details: Some(serde_json::json!({ "cached_tokens": 42 })),
            completion_tokens_details: Some(serde_json::json!({ "reasoning_tokens": 30 })),
        });
        state.process_chunk(&chunk2);

        let usage = state.usage.as_ref().expect("usage should be captured");
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.total_tokens, 150);
        assert_eq!(
            usage.input_tokens_details.cached_tokens, 42,
            "cached_tokens should flow through to streaming state"
        );
        assert_eq!(
            usage.output_tokens_details.reasoning_tokens, 30,
            "reasoning_tokens should flow through to streaming state"
        );
    }

    #[test]
    fn test_streaming_state_finalize() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        let chunk = create_test_chunk("chunk_1", Some("Hello"), Some("assistant"), Some("stop"));
        state.process_chunk(&chunk);

        let events = state.finalize();

        // Should emit response.completed
        assert!(events.iter().any(|e| e.event_type == "response.completed"));
    }

    #[test]
    fn test_event_sse_format() {
        let event = StreamingEvent {
            event_type: "response.output_text.delta".to_string(),
            data: StreamingEventData::OutputTextDelta {
                item_id: "item_0".to_string(),
                output_index: 0,
                content_index: 0,
                delta: "Hello".to_string(),
                logprobs: vec![],
            },
            sequence_number: 1,
        };

        let sse = event.to_sse();
        assert!(sse.starts_with("event: response.output_text.delta\n"));
        assert!(sse.contains("data:"));
    }

    #[test]
    fn test_parse_chat_chunk() {
        let json = r#"{"id":"chatcmpl-123","object":"chat.completion.chunk","created":1234567890,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#;

        let chunk = parse_chat_chunk(json);
        assert!(chunk.is_some());

        let chunk = chunk.unwrap();
        assert_eq!(chunk.id, "chatcmpl-123");
    }

    #[test]
    fn test_parse_done_marker() {
        let chunk = parse_chat_chunk("[DONE]");
        assert!(chunk.is_none());
    }

    #[test]
    fn test_sequence_numbers_increase() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        let chunk1 = create_test_chunk("chunk_1", Some("Hello"), Some("assistant"), None);
        let events1 = state.process_chunk(&chunk1);

        let chunk2 = create_test_chunk("chunk_2", Some(" world"), None, None);
        let events2 = state.process_chunk(&chunk2);

        // Sequence numbers should be monotonically increasing
        let all_events: Vec<_> = events1.into_iter().chain(events2).collect();
        for i in 1..all_events.len() {
            assert!(all_events[i].sequence_number > all_events[i - 1].sequence_number);
        }
    }

    /// Helper: create a chunk with tool call deltas
    fn create_tool_call_chunk(
        id: &str,
        role: Option<&str>,
        tool_call_id: Option<&str>,
        tool_name: Option<&str>,
        tool_args: Option<&str>,
        finish_reason: Option<&str>,
    ) -> ChatCompletionChunk {
        let tool_calls = if tool_call_id.is_some() || tool_name.is_some() || tool_args.is_some() {
            Some(vec![ChunkToolCall {
                index: 0,
                id: tool_call_id.map(String::from),
                call_type: tool_call_id.map(|_| "function".to_string()),
                function: if tool_name.is_some() || tool_args.is_some() {
                    Some(ChunkFunctionCall {
                        name: tool_name.map(String::from),
                        arguments: tool_args.map(String::from),
                    })
                } else {
                    None
                },
            }])
        } else {
            None
        };

        ChatCompletionChunk {
            id: id.to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: role.map(String::from),
                    content: None,
                    tool_calls,
                    reasoning: None,
                    reasoning_content: None,
                    reasoning_details: None,
                },
                finish_reason: finish_reason.map(String::from),
                logprobs: None,
            }],
            usage: None,
            system_fingerprint: None,
            service_tier: None,
        }
    }

    #[test]
    fn test_extract_tool_calls() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        // Stream a tool call: role, then tool call start, then arguments, then finish
        state.process_chunk(&create_tool_call_chunk(
            "c1",
            Some("assistant"),
            None,
            None,
            None,
            None,
        ));
        state.process_chunk(&create_tool_call_chunk(
            "c2",
            None,
            Some("call_abc"),
            Some("get_weather"),
            Some(r#"{"loc"#),
            None,
        ));
        state.process_chunk(&create_tool_call_chunk(
            "c3",
            None,
            None,
            None,
            Some(r#"ation":"Paris"}"#),
            None,
        ));
        state.process_chunk(&create_tool_call_chunk(
            "c4",
            None,
            None,
            None,
            None,
            Some("tool_calls"),
        ));

        let tool_calls = state.extract_tool_calls();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].0, "call_abc");
        assert_eq!(tool_calls[0].1, "get_weather");
        assert_eq!(tool_calls[0].2, r#"{"location":"Paris"}"#);
    }

    #[test]
    fn test_extract_tool_calls_empty_when_no_tools() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        // Plain text stream with no tool calls
        state.process_chunk(&create_test_chunk(
            "c1",
            Some("Hi"),
            Some("assistant"),
            None,
        ));
        state.process_chunk(&create_test_chunk("c2", None, None, Some("stop")));

        let tool_calls = state.extract_tool_calls();
        assert!(tool_calls.is_empty());
    }

    #[test]
    fn test_prepare_next_iteration() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        // First iteration: one item
        state.process_chunk(&create_test_chunk(
            "c1",
            Some("Hi"),
            Some("assistant"),
            Some("stop"),
        ));

        assert_eq!(state.items.len(), 1);
        assert_eq!(state.completed_items.len(), 0);
        assert_eq!(state.item_index_offset, 0);

        state.prepare_next_iteration();

        assert_eq!(state.items.len(), 0);
        assert_eq!(state.completed_items.len(), 1);
        assert_eq!(state.item_index_offset, 1);
    }

    #[test]
    fn test_multi_iteration_item_ids_are_unique() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        // First iteration
        state.process_chunk(&create_test_chunk(
            "c1",
            Some("call result"),
            Some("assistant"),
            Some("tool_calls"),
        ));

        // Collect first iteration item IDs
        let first_item_ids: Vec<String> = state.items.iter().map(|i| i.id.clone()).collect();

        state.prepare_next_iteration();

        // Second iteration — choice.index is 0 again
        state.process_chunk(&create_test_chunk(
            "c2",
            Some("Final answer"),
            Some("assistant"),
            Some("stop"),
        ));

        let second_item_ids: Vec<String> = state.items.iter().map(|i| i.id.clone()).collect();

        // IDs must not overlap
        for id in &first_item_ids {
            assert!(
                !second_item_ids.contains(id),
                "ID collision across iterations: {id}"
            );
        }

        // Specifically: first iteration gets item_0, second gets item_1
        assert_eq!(first_item_ids, vec!["item_0"]);
        assert_eq!(second_item_ids, vec!["item_1"]);
    }

    #[test]
    fn test_multi_iteration_output_indices() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        // First iteration
        let events1 = state.process_chunk(&create_test_chunk(
            "c1",
            Some("Hi"),
            Some("assistant"),
            None,
        ));

        // Find the output_item.added event and check its index
        let added_event_1 = events1
            .iter()
            .find(|e| e.event_type == "response.output_item.added")
            .unwrap();
        if let StreamingEventData::OutputItemAdded { output_index, .. } = &added_event_1.data {
            assert_eq!(*output_index, 0);
        } else {
            panic!("Expected OutputItemAdded");
        }

        state.process_chunk(&create_test_chunk("c2", None, None, Some("stop")));
        state.prepare_next_iteration();

        // Second iteration — should get output_index 1
        let events2 = state.process_chunk(&create_test_chunk(
            "c3",
            Some("World"),
            Some("assistant"),
            None,
        ));

        let added_event_2 = events2
            .iter()
            .find(|e| e.event_type == "response.output_item.added")
            .unwrap();
        if let StreamingEventData::OutputItemAdded { output_index, .. } = &added_event_2.data {
            assert_eq!(*output_index, 1);
        } else {
            panic!("Expected OutputItemAdded");
        }
    }

    #[test]
    fn test_multi_iteration_final_response_includes_all_items() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        // First iteration
        state.process_chunk(&create_test_chunk(
            "c1",
            Some("thinking..."),
            Some("assistant"),
            Some("tool_calls"),
        ));
        state.prepare_next_iteration();

        // Second iteration
        state.process_chunk(&create_test_chunk(
            "c2",
            Some("The answer is 42."),
            Some("assistant"),
            Some("stop"),
        ));

        let final_events = state.finalize();
        let completed = final_events
            .iter()
            .find(|e| e.event_type == "response.completed")
            .expect("Should have response.completed event");

        if let StreamingEventData::ResponseCompleted { response } = &completed.data {
            // Should have items from both iterations
            assert_eq!(
                response.output.len(),
                2,
                "Final response should contain items from both iterations"
            );
        } else {
            panic!("Expected ResponseCompleted");
        }
    }

    /// Helper: create a chunk with reasoning deltas
    fn create_reasoning_chunk(
        id: &str,
        reasoning: Option<&str>,
        reasoning_content: Option<&str>,
        content: Option<&str>,
        role: Option<&str>,
        finish_reason: Option<&str>,
    ) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: id.to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "deepseek-r1".to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: role.map(String::from),
                    content: content.map(String::from),
                    tool_calls: None,
                    reasoning: reasoning.map(String::from),
                    reasoning_content: reasoning_content.map(String::from),
                    reasoning_details: None,
                },
                finish_reason: finish_reason.map(String::from),
                logprobs: None,
            }],
            usage: None,
            system_fingerprint: None,
            service_tier: None,
        }
    }

    #[test]
    fn test_reasoning_only_stream_produces_reasoning_item() {
        let mut state = StreamingState::new(&test_request("deepseek-r1"));

        // Stream reasoning deltas
        let events1 = state.process_chunk(&create_reasoning_chunk(
            "c1",
            Some("Let me think"),
            None,
            None,
            Some("assistant"),
            None,
        ));

        // Should get: response.created, output_item.added, reasoning_summary_part.added, reasoning_summary_text.delta
        assert!(events1.iter().any(|e| e.event_type == "response.created"));
        assert!(
            events1
                .iter()
                .any(|e| e.event_type == "response.output_item.added")
        );
        assert!(
            events1
                .iter()
                .any(|e| e.event_type == "response.reasoning_summary_part.added")
        );
        assert!(
            events1
                .iter()
                .any(|e| e.event_type == "response.reasoning_summary_text.delta")
        );

        // More reasoning
        let events2 = state.process_chunk(&create_reasoning_chunk(
            "c2",
            Some(" step by step"),
            None,
            None,
            None,
            None,
        ));
        assert_eq!(events2.len(), 1);
        assert_eq!(
            events2[0].event_type,
            "response.reasoning_summary_text.delta"
        );

        // Finish
        state.process_chunk(&create_reasoning_chunk(
            "c3",
            None,
            None,
            None,
            None,
            Some("stop"),
        ));

        let final_events = state.finalize();
        let completed = final_events
            .iter()
            .find(|e| e.event_type == "response.completed")
            .unwrap();
        if let StreamingEventData::ResponseCompleted { response } = &completed.data {
            assert_eq!(response.output.len(), 1);
            assert!(matches!(response.output[0], Item::Reasoning(_)));
            if let Item::Reasoning(ref r) = response.output[0] {
                let summary = r.summary.as_ref().unwrap();
                assert_eq!(summary.len(), 1);
                let SummaryContent::Text { text } = &summary[0];
                assert_eq!(text, "Let me think step by step");
            }
        } else {
            panic!("Expected ResponseCompleted");
        }
    }

    #[test]
    fn test_reasoning_plus_content_stream() {
        let mut state = StreamingState::new(&test_request("deepseek-r1"));

        // Reasoning phase (using reasoning_content field for vLLM)
        state.process_chunk(&create_reasoning_chunk(
            "c1",
            None,
            Some("thinking..."),
            None,
            Some("assistant"),
            None,
        ));

        // Content phase
        state.process_chunk(&create_reasoning_chunk(
            "c2",
            None,
            None,
            Some("The answer is 42."),
            None,
            None,
        ));

        // Finish
        state.process_chunk(&create_reasoning_chunk(
            "c3",
            None,
            None,
            None,
            None,
            Some("stop"),
        ));

        let final_events = state.finalize();
        let completed = final_events
            .iter()
            .find(|e| e.event_type == "response.completed")
            .unwrap();
        if let StreamingEventData::ResponseCompleted { response } = &completed.data {
            // Reasoning item should come before message item
            assert_eq!(response.output.len(), 2);
            assert!(
                matches!(response.output[0], Item::Reasoning(_)),
                "First item should be Reasoning"
            );
            assert!(
                matches!(response.output[1], Item::Message(_)),
                "Second item should be Message"
            );
        } else {
            panic!("Expected ResponseCompleted");
        }
    }

    #[test]
    fn test_reasoning_stream_event_sequence() {
        let mut state = StreamingState::new(&test_request("deepseek-r1"));

        // First reasoning chunk
        let events = state.process_chunk(&create_reasoning_chunk(
            "c1",
            Some("step 1"),
            None,
            None,
            Some("assistant"),
            None,
        ));

        let event_types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(
            event_types,
            vec![
                "response.created",
                "response.output_item.added",
                "response.reasoning_summary_part.added",
                "response.reasoning_summary_text.delta",
            ]
        );

        // Finish
        let done_events = state.process_chunk(&create_reasoning_chunk(
            "c2",
            None,
            None,
            None,
            None,
            Some("stop"),
        ));

        let done_types: Vec<&str> = done_events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(
            done_types,
            vec![
                "response.reasoning_summary_text.done",
                "response.reasoning_summary_part.done",
                "response.output_item.done",
            ]
        );
    }
}
