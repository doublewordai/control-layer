//! API request/response models for users.

use super::pagination::Pagination;
use crate::api::models::groups::GroupResponse;
use crate::db::models::users::UserDBResponse;
use crate::types::UserId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// User role determining access permissions and capabilities.
///
/// Roles are additive - a user can have multiple roles, and their effective
/// permissions are the union of all role permissions.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type, PartialEq, ToSchema)]
#[sqlx(type_name = "user_role", rename_all = "UPPERCASE")]
pub enum Role {
    /// Full administrative access: manage users, groups, deployments, and endpoints
    PlatformManager,
    /// Read-only access to API request logs and analytics
    RequestViewer,
    /// Basic user access: can make API requests through assigned model deployments
    StandardUser,
    /// Access to billing information and credit management
    BillingManager,
    /// Access to batch processing API endpoints
    BatchAPIUser,
}

/// Request body for creating a new user.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserCreate {
    /// Unique username for login (must be unique across the system)
    #[schema(example = "jsmith")]
    pub username: String,
    /// User's email address (must be unique across the system)
    #[schema(example = "john.smith@example.com")]
    pub email: String,
    /// Human-readable display name shown in the UI
    #[schema(example = "John Smith")]
    pub display_name: Option<String>,
    /// URL to the user's avatar image
    #[schema(example = "https://example.com/avatars/jsmith.png")]
    pub avatar_url: Option<String>,
    /// Roles to assign to this user (determines permissions)
    pub roles: Vec<Role>,
}

/// Request body for updating an existing user. All fields are optional;
/// only provided fields will be updated.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserUpdate {
    /// New display name (null to keep unchanged)
    #[schema(example = "John Smith Jr.")]
    pub display_name: Option<String>,
    /// New avatar URL (null to keep unchanged)
    #[schema(example = "https://example.com/avatars/jsmith-new.png")]
    pub avatar_url: Option<String>,
    /// New set of roles (replaces all existing roles; null to keep unchanged)
    pub roles: Option<Vec<Role>>,
}

/// Full user details returned by the API.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserResponse {
    /// Unique identifier for the user
    #[schema(value_type = String, format = "uuid")]
    pub id: UserId,
    /// Unique username for login
    pub username: String,
    /// User's email address
    pub email: String,
    /// Human-readable display name
    pub display_name: Option<String>,
    /// URL to the user's avatar image
    pub avatar_url: Option<String>,
    /// Whether this user has legacy admin privileges (deprecated, use roles instead)
    pub is_admin: bool,
    /// Roles assigned to this user
    pub roles: Vec<Role>,
    /// When the user account was created
    pub created_at: DateTime<Utc>,
    /// When the user account was last modified
    pub updated_at: DateTime<Utc>,
    /// When the user last logged in (null if never logged in)
    pub last_login: Option<DateTime<Utc>>,
    /// Authentication source (e.g., "local", "google", "oidc")
    pub auth_source: String,
    /// ID from external authentication provider (if using SSO)
    pub external_user_id: Option<String>,
    /// Groups this user belongs to (only included if `include=groups` is specified)
    /// Note: no_recursion is important! utoipa will panic at runtime, because it overflows the
    /// stack trying to follow the relationship.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(no_recursion)]
    pub groups: Option<Vec<GroupResponse>>,
    /// User's credit balance (only included if `include=billing` is specified)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credit_balance: Option<f64>,
    /// ID in external payment provider (e.g., Stripe customer ID)
    pub payment_provider_id: Option<String>,
}

/// Query parameters for listing users
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct ListUsersQuery {
    /// Pagination parameters
    #[serde(flatten)]
    #[param(inline)]
    pub pagination: Pagination,

    /// Include related data (comma-separated: "groups", "billing")
    pub include: Option<String>,

    /// Search query to filter users by display_name, username, or email (case-insensitive substring match)
    pub search: Option<String>,
}

/// The currently authenticated user's information.
/// This is a subset of UserResponse containing only the fields relevant
/// to the current session.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CurrentUser {
    /// Unique identifier for the user
    #[schema(value_type = String, format = "uuid")]
    pub id: UserId,
    /// Unique username for login
    pub username: String,
    /// User's email address
    pub email: String,
    /// Whether this user has legacy admin privileges
    pub is_admin: bool,
    /// Roles assigned to this user
    pub roles: Vec<Role>,
    /// Human-readable display name
    pub display_name: Option<String>,
    /// URL to the user's avatar image
    pub avatar_url: Option<String>,
    /// ID in external payment provider
    pub payment_provider_id: Option<String>,
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
            payment_provider_id: None, // UserResponse doesn't include payment_provider_id
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
            external_user_id: db.external_user_id,
            last_login: None,     // UserDBResponse doesn't have last_login
            groups: None,         // By default, relationships are not included
            credit_balance: None, // By default, credit balances are not included
            payment_provider_id: db.payment_provider_id,
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

/// Query parameters for retrieving a single user.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GetUserQuery {
    /// Include related data (comma-separated: "groups", "billing")
    #[schema(example = "groups,billing")]
    pub include: Option<String>,
}
