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
    /// Number of items to skip
    #[param(default = 0, minimum = 0)]
    pub skip: Option<i64>,

    /// Maximum number of items to return
    #[param(default = 100, minimum = 1, maximum = 1000)]
    pub limit: Option<i64>,

    /// Include related data (comma-separated: "users", "models")
    pub include: Option<String>,
}

// Request models
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GroupCreate {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GroupUpdate {
    pub name: Option<String>,
    pub description: Option<String>,
}

// Response model
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GroupResponse {
    #[schema(value_type = String, format = "uuid")]
    pub id: GroupId,
    pub name: String,
    pub description: Option<String>,
    #[schema(value_type = String, format = "uuid")]
    pub created_by: UserId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Users in this group (only included if requested)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub users: Option<Vec<UserResponse>>,
    /// Models accessible by this group (only included if requested)  
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Note: no_recursion is important! utoipa will panic at runtime, because it overflows the
    /// stack trying to follow the relationship.
    #[schema(no_recursion)]
    pub models: Option<Vec<DeployedModelResponse>>,
}

impl From<GroupDBResponse> for GroupResponse {
    fn from(db: GroupDBResponse) -> Self {
        Self {
            id: db.id,
            name: db.name,
            description: db.description,
            created_by: db.created_by,
            created_at: db.created_at,
            updated_at: db.updated_at,
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
}
