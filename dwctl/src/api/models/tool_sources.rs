//! API request/response models for tool sources.

use crate::db::models::tool_sources::ToolSourceDBResponse;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;
use uuid::Uuid;

/// Request body for creating a new tool source.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ToolSourceCreate {
    /// Tool kind — currently always "http"
    #[serde(default = "default_kind")]
    pub kind: String,
    /// Unique display name for this tool
    #[schema(example = "Weather API")]
    pub name: String,
    /// Human-readable description shown to the model
    #[schema(example = "Fetch current weather for a given location")]
    pub description: Option<String>,
    /// JSON Schema describing the tool's input parameters
    pub parameters: Option<Value>,
    /// HTTP endpoint that will be called with the tool arguments as JSON body
    #[schema(example = "https://api.example.com/tools/weather")]
    pub url: String,
    /// Optional Bearer token sent in the Authorization header when calling the tool endpoint
    pub api_key: Option<String>,
    /// Request timeout in seconds (default: 30)
    #[serde(default = "default_timeout")]
    pub timeout_secs: i32,
}

fn default_kind() -> String {
    "http".to_string()
}

fn default_timeout() -> i32 {
    30
}

/// Request body for updating a tool source. All fields are optional.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ToolSourceUpdate {
    /// New display name
    pub name: Option<String>,
    /// New description — pass `null` to clear
    #[serde(default, with = "::serde_with::rust::double_option")]
    pub description: Option<Option<String>>,
    /// New JSON Schema — pass `null` to clear
    #[serde(default, with = "::serde_with::rust::double_option")]
    pub parameters: Option<Option<Value>>,
    /// New tool endpoint URL
    pub url: Option<String>,
    /// New API key — pass `null` to clear
    #[serde(default, with = "::serde_with::rust::double_option")]
    pub api_key: Option<Option<String>>,
    /// New timeout in seconds
    pub timeout_secs: Option<i32>,
}

/// Tool source details returned by the API.
///
/// The `api_key` field is never included; use `has_api_key` to know whether
/// one is configured.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ToolSourceResponse {
    /// Unique identifier
    #[schema(value_type = String, format = "uuid")]
    pub id: Uuid,
    /// Tool kind (currently always "http")
    pub kind: String,
    /// Display name
    pub name: String,
    /// Human-readable description
    pub description: Option<String>,
    /// JSON Schema for tool inputs
    pub parameters: Option<Value>,
    /// HTTP endpoint URL
    pub url: String,
    /// Whether an API key is configured (key itself is never returned)
    pub has_api_key: bool,
    /// Timeout in seconds
    pub timeout_secs: i32,
    /// Creation timestamp
    pub created_at: DateTime<Utc>,
    /// Last update timestamp
    pub updated_at: DateTime<Utc>,
}

impl From<ToolSourceDBResponse> for ToolSourceResponse {
    fn from(db: ToolSourceDBResponse) -> Self {
        Self {
            id: db.id,
            kind: db.kind,
            name: db.name,
            description: db.description,
            parameters: db.parameters,
            url: db.url,
            has_api_key: db.api_key.is_some(),
            timeout_secs: db.timeout_secs,
            created_at: db.created_at,
            updated_at: db.updated_at,
        }
    }
}
