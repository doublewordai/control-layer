//! API request/response models for user groups.

use super::pagination::Pagination;
use crate::api::models::deployments::DeployedModelResponse;
use crate::api::models::users::UserResponse;
use crate::db::models::groups::GroupDBResponse;
use crate::types::{GroupId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Query parameters for listing groups
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct ListGroupsQuery {
    /// Pagination parameters
    #[serde(flatten)]
    #[param(inline)]
    pub pagination: Pagination,

    /// Include related data (comma-separated: "users", "models")
    pub include: Option<String>,

    /// Search query to filter groups by name or description (case-insensitive substring match)
    pub search: Option<String>,
}

/// Request body for creating a new group.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GroupCreate {
    /// Display name for the group (must be unique)
    #[schema(example = "Engineering Team")]
    pub name: String,
    /// Optional description of the group's purpose
    #[schema(example = "Backend and frontend engineers")]
    pub description: Option<String>,
}

/// Request body for updating an existing group. All fields are optional;
/// only provided fields will be updated.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GroupUpdate {
    /// New display name (null to keep unchanged)
    #[schema(example = "Engineering Team - Updated")]
    pub name: Option<String>,
    /// New description (null to keep unchanged)
    #[schema(example = "Updated description")]
    pub description: Option<String>,
}

/// Full group details returned by the API.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GroupResponse {
    /// Unique identifier for the group
    #[schema(value_type = String, format = "uuid")]
    pub id: GroupId,
    /// Display name for the group
    pub name: String,
    /// Description of the group's purpose
    pub description: Option<String>,
    /// User ID of who created the group (may be hidden based on permissions)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "uuid")]
    pub created_by: Option<UserId>,
    /// When the group was created
    pub created_at: DateTime<Utc>,
    /// When the group was last modified
    pub updated_at: DateTime<Utc>,
    /// Users in this group (only included if `include=users` is specified)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub users: Option<Vec<UserResponse>>,
    /// Models accessible by this group (only included if `include=models` is specified)
    /// Note: no_recursion is important! utoipa will panic at runtime, because it overflows the
    /// stack trying to follow the relationship.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(no_recursion)]
    pub models: Option<Vec<DeployedModelResponse>>,
    /// Origin of the group (e.g., "local", "oidc")
    pub source: String,
}

impl From<GroupDBResponse> for GroupResponse {
    fn from(db: GroupDBResponse) -> Self {
        Self {
            id: db.id,
            name: db.name,
            description: db.description,
            created_by: Some(db.created_by),
            created_at: db.created_at,
            updated_at: db.updated_at,
            source: db.source,
            users: None, // By default, relationships are not included
            models: None,
        }
    }
}

impl GroupResponse {
    /// Create a response with both users and models included
    pub fn with_relationships(mut self, users: Option<Vec<UserResponse>>, models: Option<Vec<DeployedModelResponse>>) -> Self {
        if let Some(users) = users {
            self.users = Some(users);
        }
        if let Some(models) = models {
            self.models = Some(models);
        }
        self
    }

    /// Mask created_by field (sets to None for users without permission)
    pub fn mask_created_by(mut self) -> Self {
        self.created_by = None;
        self
    }
}
