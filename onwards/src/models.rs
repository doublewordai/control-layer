//! Data models for OpenAI-compatible API endpoints
//!
//! This module defines the request and response structures used by the proxy's
//! API endpoints, particularly the `/v1/models` endpoint which lists available
//! targets as OpenAI-compatible models.
use serde::{Deserialize, Serialize};

/// Requests to the /v1/{*} endpoints get forwarded onto OpenAI compatible targets.
/// The target is chosen based on the model specified in the request body.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ExtractedModel<'a> {
    #[serde(borrow)]
    pub(crate) model: &'a str,
}

/// The returned models from the /v1/models endpoint.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct Model {
    /// The model identifier, which can be referenced in the API endpoints.
    pub(crate) id: String,
    /// The Unix timestamp (in seconds) when the model was created.
    pub(crate) created: u32,
    /// The object type, which is always "model".
    pub(crate) object: String,
    /// The organization that owns the model.
    pub(crate) owned_by: String,
}

/// The response from the /v1/models endpoint, which is a list of models.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct ListModelResponse {
    /// The object type, which is always "list".
    pub object: String,
    /// A list of model objects.
    pub data: Vec<Model>,
}

impl ListModelResponse {
    /// Creates a new ListModelResponse from a list of model names.
    pub(crate) fn from_model_names(model_names: &[String]) -> Self {
        let data = model_names
            .iter()
            .map(|name| Model {
                id: name.clone(),
                created: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as u32,
                object: "model".into(),
                owned_by: "None".into(),
            })
            .collect::<Vec<_>>();
        ListModelResponse {
            object: "list".into(),
            data,
        }
    }
}
