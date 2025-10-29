use crate::db::models::inference_endpoints::InferenceEndpointDBResponse;
use crate::types::{InferenceEndpointId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::{IntoParams, ToSchema};

/// A model from an OpenAI-compatible API
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OpenAIModel {
    pub id: String,
    pub object: String,
    pub created: Option<i64>, // openAI always returns this, but google never does
    pub owned_by: String,
}

/// Response from the /v1/models endpoint of an OpenAI-compatible API
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OpenAIModelsResponse {
    pub object: String,
    pub data: Vec<OpenAIModel>,
}

/// A model from the Anthropic API
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AnthropicModel {
    pub created_at: String,
    pub display_name: String,
    pub id: String,
    pub r#type: String,
}

/// Response from the /v1/models endpoint of the anthropic API
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AnthropicModelsResponse {
    pub data: Vec<AnthropicModel>,
    pub first_id: String,
    pub has_more: bool,
    pub last_id: String,
}

impl From<AnthropicModelsResponse> for OpenAIModelsResponse {
    fn from(anthropic: AnthropicModelsResponse) -> Self {
        let data = anthropic
            .data
            .into_iter()
            .map(|model| OpenAIModel {
                id: model.id,
                object: "model".to_string(),
                created: Some(0),
                owned_by: "anthropic".to_string(),
            })
            .collect();
        Self {
            object: "list".to_string(),
            data,
        }
    }
}

/// Query parameters for listing inference endpoints
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct ListEndpointsQuery {
    /// Number of items to skip
    #[param(default = 0, minimum = 0)]
    pub skip: Option<i64>,

    /// Maximum number of items to return
    #[param(default = 100, minimum = 1, maximum = 1000)]
    pub limit: Option<i64>,
}

// Request models
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InferenceEndpointCreate {
    pub name: String,
    pub description: Option<String>,
    pub url: String,
    pub api_key: Option<String>,
    /// List of model names to import from the endpoint (if None, imports all)
    pub model_filter: Option<Vec<String>>,
    /// Optional mapping of model names to custom aliases
    /// Key: model_name, Value: desired_alias
    /// If a model is not in this map, its alias will default to the model_name
    #[serde(default)]
    pub alias_mapping: Option<HashMap<String, String>>,
    /// Whether to automatically synchronize models after creation (defaults to true)
    #[serde(default = "default_sync")]
    pub sync: bool,
}

fn default_sync() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InferenceEndpointUpdate {
    pub name: Option<String>,
    pub description: Option<String>,
    pub url: Option<String>,
    pub api_key: Option<Option<String>>,
    pub model_filter: Option<Option<Vec<String>>>,
    #[serde(default)]
    pub alias_mapping: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InferenceEndpointValidate {
    New {
        url: String,
        api_key: Option<String>,
    },
    Existing {
        #[schema(value_type = String, format = "uuid")]
        endpoint_id: InferenceEndpointId,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InferenceEndpointValidateResponse {
    pub status: String, // "success" | "error"
    pub models: Option<OpenAIModelsResponse>,
    pub error: Option<String>,
}

// Response model
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InferenceEndpointResponse {
    #[schema(value_type = String, format = "uuid")]
    pub id: InferenceEndpointId,
    pub name: String,
    pub description: Option<String>,
    pub url: String,
    pub model_filter: Option<Vec<String>>,
    pub requires_api_key: bool,
    #[schema(value_type = String, format = "uuid")]
    pub created_by: UserId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<InferenceEndpointDBResponse> for InferenceEndpointResponse {
    fn from(db: InferenceEndpointDBResponse) -> Self {
        Self {
            id: db.id,
            name: db.name,
            description: db.description,
            url: db.url.to_string(),
            model_filter: db.model_filter,
            requires_api_key: db.api_key.is_some() && !db.api_key.as_ref().unwrap().is_empty(),
            created_by: db.created_by,
            created_at: db.created_at,
            updated_at: db.updated_at,
        }
    }
}
