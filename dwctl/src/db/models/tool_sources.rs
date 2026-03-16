//! Database models for tool sources.

use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

/// Database response for a tool source record.
#[derive(Debug, Clone)]
pub struct ToolSourceDBResponse {
    pub id: Uuid,
    pub kind: String,
    pub name: String,
    pub description: Option<String>,
    pub parameters: Option<Value>,
    pub url: String,
    /// Raw api_key value — handlers must not expose this directly.
    pub api_key: Option<String>,
    pub timeout_secs: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Database request for creating a tool source.
#[derive(Debug, Clone)]
pub struct ToolSourceCreateDBRequest {
    pub kind: String,
    pub name: String,
    pub description: Option<String>,
    pub parameters: Option<Value>,
    pub url: String,
    pub api_key: Option<String>,
    pub timeout_secs: i32,
}

/// Database request for updating a tool source.
#[derive(Debug, Clone)]
pub struct ToolSourceUpdateDBRequest {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub parameters: Option<Option<Value>>,
    pub url: Option<String>,
    pub api_key: Option<Option<String>>,
    pub timeout_secs: Option<i32>,
}
