use crate::types::{InferenceEndpointId, UserId};
use chrono::{DateTime, Utc};
use url::Url;

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
    pub created_by: UserId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
