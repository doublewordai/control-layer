use crate::types::UserId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderDisplayConfigCreateDBRequest {
    pub provider_key: String,
    pub display_name: String,
    pub icon: Option<String>,
    pub sort_order: i32,
    pub created_by: UserId,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderDisplayConfigUpdateDBRequest {
    pub display_name: Option<String>,
    pub icon: Option<Option<String>>,
    pub sort_order: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderDisplayConfigDBResponse {
    pub provider_key: String,
    pub display_name: String,
    pub icon: Option<String>,
    pub sort_order: i32,
    pub created_by: UserId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownProviderDBResponse {
    pub provider_key: String,
    pub display_name: String,
    pub model_count: i64,
}
