use crate::api::models::groups::GroupResponse;
use crate::db::models::users::UserDBResponse;
use crate::types::UserId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

// Role enum for different job functions
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type, PartialEq, ToSchema)]
#[sqlx(type_name = "user_role", rename_all = "UPPERCASE")]
pub enum Role {
    PlatformManager,
    RequestViewer,
    StandardUser,
    BillingManager,
}

// User request models
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserCreate {
    pub username: String,
    pub email: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub roles: Vec<Role>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserUpdate {
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub roles: Option<Vec<Role>>,
}

// User response models
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserResponse {
    #[schema(value_type = String, format = "uuid")]
    pub id: UserId,
    pub username: String,
    pub email: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub is_admin: bool,
    pub roles: Vec<Role>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_login: Option<DateTime<Utc>>,
    pub auth_source: String,
    /// Groups this user belongs to (only included if requested)
    /// Note: no_recursion is important! utoipa will panic at runtime, because it overflows the
    /// stack trying to follow the relationship.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(no_recursion)]
    pub groups: Option<Vec<GroupResponse>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credit_balance: Option<f64>,
}

/// Query parameters for listing users
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct ListUsersQuery {
    /// Number of items to skip
    #[param(default = 0, minimum = 0)]
    pub skip: Option<i64>,

    /// Maximum number of items to return
    #[param(default = 100, minimum = 1, maximum = 1000)]
    pub limit: Option<i64>,

    /// Include related data (comma-separated: "groups")
    pub include: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CurrentUser {
    #[schema(value_type = String, format = "uuid")]
    pub id: UserId,
    pub username: String,
    pub email: String,
    pub is_admin: bool,
    pub roles: Vec<Role>,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
}

impl CurrentUser {
    #[cfg(test)]
    pub fn is_admin(&self) -> bool {
        self.is_admin
    }
}

impl From<UserResponse> for CurrentUser {
    fn from(response: UserResponse) -> Self {
        Self {
            id: response.id,
            username: response.username,
            email: response.email,
            is_admin: response.is_admin,
            roles: response.roles,
            display_name: response.display_name,
            avatar_url: response.avatar_url,
        }
    }
}

impl From<UserDBResponse> for UserResponse {
    fn from(db: UserDBResponse) -> Self {
        Self {
            id: db.id,
            username: db.username,
            email: db.email,
            display_name: db.display_name,
            avatar_url: db.avatar_url,
            is_admin: db.is_admin,
            roles: db.roles,
            created_at: db.created_at,
            updated_at: db.updated_at,
            auth_source: db.auth_source,
            last_login: None,     // UserDBResponse doesn't have last_login
            groups: None,         // By default, relationships are not included
            credit_balance: None, // By default, credit balances are not included
        }
    }
}

impl UserResponse {
    /// Create a response with groups included
    pub fn with_groups(mut self, groups: Vec<GroupResponse>) -> Self {
        self.groups = Some(groups);
        self
    }

    /// Create a response with credit balance included
    pub fn with_credit_balance(mut self, balance: f64) -> Self {
        self.credit_balance = Some(balance);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GetUserQuery {
    pub include: Option<String>,
}
