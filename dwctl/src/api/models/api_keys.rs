//! API request/response models for API keys.

use crate::db::models::api_keys::{ApiKeyDBResponse, ApiKeyPurpose};
use crate::types::{ApiKeyId, DeploymentId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

// API Key request models.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ApiKeyCreate {
    pub name: String,
    pub description: Option<String>,
    /// Purpose of the API key: 'platform' for /admin/api/* access, 'inference' for /ai/* access
    pub purpose: ApiKeyPurpose,
    /// Per-API-key rate limit: requests per second (null = no limit)
    pub requests_per_second: Option<f32>,
    /// Per-API-key rate limit: maximum burst size (null = no limit)
    pub burst_size: Option<i32>,
}

// API Key update.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ApiKeyUpdate {
    pub name: Option<String>,
    pub description: Option<String>,
    /// Per-API-key rate limit: requests per second (null = no limit, Some(None) = remove limit)
    pub requests_per_second: Option<Option<f32>>,
    /// Per-API-key rate limit: maximum burst size (null = no limit, Some(None) = remove limit)
    pub burst_size: Option<Option<i32>>,
}

// API Key response models
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ApiKeyResponse {
    #[schema(value_type = String, format = "uuid")]
    pub id: ApiKeyId,
    pub name: String,
    pub description: Option<String>,
    pub key: String,
    pub purpose: ApiKeyPurpose,
    #[schema(value_type = String, format = "uuid")]
    pub user_id: UserId,
    pub created_at: DateTime<Utc>,
    pub last_used: Option<DateTime<Utc>>,
    #[schema(value_type = Vec<String>)]
    pub model_access: Vec<DeploymentId>,
    /// Per-API-key rate limit: requests per second (null = no limit)
    pub requests_per_second: Option<f32>,
    /// Per-API-key rate limit: maximum burst size (null = no limit)
    pub burst_size: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ApiKeyInfoResponse {
    #[schema(value_type = String, format = "uuid")]
    pub id: ApiKeyId,
    pub name: String,
    pub description: Option<String>,
    pub purpose: ApiKeyPurpose,
    #[schema(value_type = String, format = "uuid")]
    pub user_id: UserId,
    pub created_at: DateTime<Utc>,
    pub last_used: Option<DateTime<Utc>>,
    #[schema(value_type = Vec<String>)]
    pub model_access: Vec<DeploymentId>,
    /// Per-API-key rate limit: requests per second (null = no limit)
    pub requests_per_second: Option<f32>,
    /// Per-API-key rate limit: maximum burst size (null = no limit)
    pub burst_size: Option<i32>,
}

#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct ListApiKeysQuery {
    // Number of items to skip
    #[param(default = 0, minimum = 0)]
    pub skip: Option<i64>,

    // Maximum number of items to return
    #[param(default = 100, minimum = 1, maximum = 1000)]
    pub limit: Option<i64>,
}

impl From<ApiKeyDBResponse> for ApiKeyResponse {
    fn from(db: ApiKeyDBResponse) -> Self {
        Self {
            id: db.id,
            name: db.name,
            description: db.description,
            key: db.secret,
            purpose: db.purpose,
            user_id: db.user_id,
            created_at: db.created_at,
            last_used: db.last_used,
            model_access: db.model_access,
            requests_per_second: db.requests_per_second,
            burst_size: db.burst_size,
        }
    }
}

impl From<ApiKeyDBResponse> for ApiKeyInfoResponse {
    fn from(db: ApiKeyDBResponse) -> Self {
        Self {
            id: db.id,
            name: db.name,
            description: db.description,
            purpose: db.purpose,
            user_id: db.user_id,
            created_at: db.created_at,
            last_used: db.last_used,
            model_access: db.model_access,
            requests_per_second: db.requests_per_second,
            burst_size: db.burst_size,
        }
    }
}
