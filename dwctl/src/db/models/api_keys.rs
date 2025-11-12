//! Database models for API keys.

use crate::api::models::api_keys::ApiKeyCreate;
use crate::types::{ApiKeyId, DeploymentId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// API key purpose - determines which endpoints the key can access
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, sqlx::Type)]
#[sqlx(type_name = "VARCHAR", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum ApiKeyPurpose {
    /// Platform/management API access (/admin/api/*)
    Platform,
    /// Inference API access (/ai/*)
    Inference,
}

/// Database request for creating a new API key
#[derive(Debug, Clone)]
pub struct ApiKeyCreateDBRequest {
    pub user_id: UserId,
    pub name: String,
    pub description: Option<String>,
    pub purpose: ApiKeyPurpose,
    pub requests_per_second: Option<f32>,
    pub burst_size: Option<i32>,
}

impl ApiKeyCreateDBRequest {
    pub fn new(user_id: UserId, create: ApiKeyCreate) -> Self {
        Self {
            user_id,
            name: create.name,
            description: create.description,
            purpose: create.purpose,
            requests_per_second: create.requests_per_second,
            burst_size: create.burst_size,
        }
    }
}

/// Database request for updating an API key
#[derive(Debug, Clone)]
pub struct ApiKeyUpdateDBRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub requests_per_second: Option<Option<f32>>,
    pub burst_size: Option<Option<i32>>,
}

/// Database response for an API key
#[derive(Debug, Clone)]
pub struct ApiKeyDBResponse {
    pub id: ApiKeyId,
    pub name: String,
    pub description: Option<String>,
    pub secret: String,
    pub purpose: ApiKeyPurpose,
    pub user_id: UserId,
    pub created_at: DateTime<Utc>,
    pub last_used: Option<DateTime<Utc>>,
    pub model_access: Vec<DeploymentId>,
    pub requests_per_second: Option<f32>,
    pub burst_size: Option<i32>,
}
