//! Database models for inference endpoints.

use crate::reasoning::ReasoningTranslationConfig;
use crate::types::{InferenceEndpointId, UserId};
use chrono::{DateTime, Utc};
use url::Url;

/// What kind of server an endpoint is. Only `dynamo` receives the
/// serving-class envelope; `external` members are skipped for organisations
/// that must never fall over to a third party. Set explicitly when an
/// endpoint is created so a new provider is classified from day one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    /// Self-hosted, behind the dynamo frontend.
    Dynamo,
    /// Self-hosted, not behind dynamo.
    Hosted,
    /// A third-party provider.
    #[default]
    External,
}

impl EndpointKind {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Dynamo => "dynamo",
            Self::Hosted => "hosted",
            Self::External => "external",
        }
    }

    pub fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "dynamo" => Some(Self::Dynamo),
            "hosted" => Some(Self::Hosted),
            "external" => Some(Self::External),
            _ => None,
        }
    }
}

/// Database request for creating a new inference endpoint
#[derive(Debug, Clone)]
pub struct InferenceEndpointCreateDBRequest {
    pub created_by: UserId,
    pub name: String,
    pub description: Option<String>,
    pub url: Url,
    pub api_key: Option<String>,
    pub model_filter: Option<Vec<String>>,
    pub auth_header_name: Option<String>,
    pub auth_header_prefix: Option<String>,
    pub reasoning_translation: Option<ReasoningTranslationConfig>,
    /// The endpoint's serving stack understands the scheduling `priority`
    /// request field (dynamo frontend). See migration 136.
    pub accepts_scheduling_priority: bool,
    /// What kind of server this is. See migration 147.
    pub kind: EndpointKind,
}

/// Database request for updating an inference endpoint
#[derive(Debug, Clone)]
pub struct InferenceEndpointUpdateDBRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub url: Option<Url>,
    pub api_key: Option<Option<String>>,
    pub model_filter: Option<Option<Vec<String>>>,
    pub auth_header_name: Option<String>,
    pub auth_header_prefix: Option<String>,
    /// None leaves the value unchanged; Some(None) clears it.
    pub reasoning_translation: Option<Option<ReasoningTranslationConfig>>,
    /// None leaves the value unchanged.
    pub accepts_scheduling_priority: Option<bool>,
    /// None leaves the value unchanged.
    pub kind: Option<EndpointKind>,
}

/// Database response for an inference endpoint
#[derive(Debug, Clone)]
pub struct InferenceEndpointDBResponse {
    pub id: InferenceEndpointId,
    pub name: String,
    pub description: Option<String>,
    pub url: Url,
    pub api_key: Option<String>,
    pub model_filter: Option<Vec<String>>,
    pub auth_header_name: String,
    pub auth_header_prefix: String,
    pub reasoning_translation: Option<ReasoningTranslationConfig>,
    pub accepts_scheduling_priority: bool,
    pub kind: EndpointKind,
    pub created_by: UserId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
