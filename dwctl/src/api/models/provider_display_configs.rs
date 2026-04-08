use crate::db::models::provider_display_configs::{KnownProviderDBResponse, ProviderDisplayConfigDBResponse};
use crate::types::UserId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct ListProviderDisplayConfigsQuery {}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateProviderDisplayConfig {
    pub provider_key: String,
    pub display_name: Option<String>,
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpdateProviderDisplayConfig {
    pub display_name: Option<String>,
    #[serde(default)]
    pub icon: Option<Option<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ProviderDisplayConfigResponse {
    pub provider_key: String,
    pub display_name: String,
    pub icon: Option<String>,
    pub model_count: i64,
    pub configured: bool,
    #[schema(value_type = Option<String>, format = "uuid")]
    pub created_by: Option<UserId>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl ProviderDisplayConfigResponse {
    pub fn from_parts(config: Option<ProviderDisplayConfigDBResponse>, known: Option<KnownProviderDBResponse>) -> Self {
        match (config, known) {
            (Some(config), Some(known)) => Self {
                provider_key: config.provider_key,
                display_name: config.display_name,
                icon: config.icon,
                model_count: known.model_count,
                configured: true,
                created_by: Some(config.created_by),
                created_at: Some(config.created_at),
                updated_at: Some(config.updated_at),
            },
            (Some(config), None) => Self {
                provider_key: config.provider_key,
                display_name: config.display_name,
                icon: config.icon,
                model_count: 0,
                configured: true,
                created_by: Some(config.created_by),
                created_at: Some(config.created_at),
                updated_at: Some(config.updated_at),
            },
            (None, Some(known)) => Self {
                provider_key: known.provider_key,
                display_name: known.display_name,
                icon: None,
                model_count: known.model_count,
                configured: false,
                created_by: None,
                created_at: None,
                updated_at: None,
            },
            (None, None) => unreachable!("provider display config response requires at least one source"),
        }
    }
}
