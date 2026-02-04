//! Database models for users.

use crate::api::models::users::{Role, UserCreate, UserUpdate};
use crate::types::UserId;
use chrono::{DateTime, Utc};

/// Database request for creating a new user
#[derive(Debug, Clone)]
pub struct UserCreateDBRequest {
    pub username: String,
    pub email: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub is_admin: bool,
    pub roles: Vec<Role>,
    pub auth_source: String,
    pub password_hash: Option<String>,
    pub external_user_id: Option<String>,
}

impl From<UserCreate> for UserCreateDBRequest {
    fn from(api: UserCreate) -> Self {
        Self {
            username: api.username,
            email: api.email,
            display_name: api.display_name,
            avatar_url: api.avatar_url,
            is_admin: false, // API users cannot create admins
            roles: api.roles,
            auth_source: "proxy-header".to_string(), // Default auth source
            password_hash: None,                     // No password for SSO proxy users
            external_user_id: None,                  // Not set via API
        }
    }
}

/// Database request for updating a user
#[derive(Debug, Clone)]
pub struct UserUpdateDBRequest {
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub roles: Option<Vec<Role>>,
    pub password_hash: Option<String>,
}

impl UserUpdateDBRequest {
    pub fn new(update: UserUpdate) -> Self {
        Self {
            display_name: update.display_name,
            avatar_url: update.avatar_url,
            roles: update.roles,
            password_hash: None, // Regular updates don't include password changes
        }
    }
}

/// Database response for a user
#[derive(Debug, Clone)]
pub struct UserDBResponse {
    pub id: UserId,
    pub username: String,
    pub email: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub auth_source: String,
    pub is_admin: bool,
    pub roles: Vec<Role>,
    pub password_hash: Option<String>,
    pub external_user_id: Option<String>,
    pub payment_provider_id: Option<String>,
    pub batch_notifications_enabled: bool,
}
