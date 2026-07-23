//! `previous_response_id` hydration for the OpenAI Responses API.
//!
//! This is the Responses control plane's request-side stateful step: given a
//! request that references a prior turn, read that turn from the store and inline
//! its output items ahead of the current input, producing a self-contained
//! request. It runs in the inference middleware (outer to the edge translation),
//! so the pure translator downstream converts it as if there were no prior turns.
//! Ported from onwards' `OpenResponsesAdapter::to_chat_request` prev-response block.

use serde_json::Value;

use crate::inference::response_store::ResponseStore;

use super::types::{Input, Item, MessageContent, MessageItem, ResponsesRequest, ResponsesResponse};

/// Failure modes for hydration.
pub enum HydrationError {
    /// `previous_response_id` referenced a response that isn't in the store (400).
    NotFound(String),
    /// Store read or (de)serialisation failure (5xx).
    Internal(String),
}

/// If `request_value` carries a `previous_response_id`, inline the prior
/// response's output items ahead of the current input (mutating `request_value`
/// in place). A request without a `previous_response_id` is left untouched - call
/// sites should gate on its presence to skip the typed round-trip on the common
/// path.
pub async fn hydrate_previous_response(store: &dyn ResponseStore, request_value: &mut Value) -> Result<(), HydrationError> {
    let mut req: ResponsesRequest =
        serde_json::from_value(request_value.clone()).map_err(|e| HydrationError::Internal(format!("parsing Responses request: {e}")))?;

    let Some(prev_id) = req.previous_response_id.clone() else {
        return Ok(());
    };

    let context = store
        .get_context(&prev_id)
        .await
        .map_err(|e| HydrationError::Internal(format!("reading previous response {prev_id}: {e}")))?
        .ok_or_else(|| HydrationError::NotFound(prev_id.clone()))?;

    let prior: ResponsesResponse =
        serde_json::from_value(context).map_err(|e| HydrationError::Internal(format!("deserialising previous response: {e}")))?;

    let current_items = match std::mem::replace(&mut req.input, Input::Items(Vec::new())) {
        Input::Text(text) => vec![Item::Message(MessageItem {
            id: None,
            role: "user".to_string(),
            content: MessageContent::Text(text),
            status: None,
        })],
        Input::Items(items) => items,
    };

    let mut items = prior.output;
    items.extend(current_items);
    req.input = Input::Items(items);

    *request_value = serde_json::to_value(&req).map_err(|e| HydrationError::Internal(format!("re-serialising hydrated request: {e}")))?;
    Ok(())
}
