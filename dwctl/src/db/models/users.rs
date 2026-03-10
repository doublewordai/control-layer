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
    pub batch_notifications_enabled: Option<bool>,
    /// Low balance notification threshold.
    /// `None` = don't change, `Some(None)` = disable, `Some(Some(val))` = set threshold.
    pub low_balance_threshold: Option<Option<f32>>,
    /// Auto top-up amount.
    /// `None` = don't change, `Some(None)` = disable, `Some(Some(val))` = set amount.
    pub auto_topup_amount: Option<Option<f32>>,
    /// Auto top-up threshold (balance level that triggers auto top-up).
    /// `None` = don't change, `Some(None)` = disable, `Some(Some(val))` = set threshold.
    pub auto_topup_threshold: Option<Option<f32>>,
    /// Monthly auto top-up spending limit.
    /// `None` = don't change, `Some(None)` = disable limit, `Some(Some(val))` = set limit.
    pub auto_topup_monthly_limit: Option<Option<f32>>,
    /// When true, sets `last_login` to NOW().
    pub acknowledge_login: Option<bool>,
}

impl UserUpdateDBRequest {
    pub fn new(update: UserUpdate) -> Self {
        Self {
            display_name: update.display_name,
            avatar_url: update.avatar_url,
            roles: update.roles,
            password_hash: None, // Regular updates don't include password changes
            batch_notifications_enabled: update.batch_notifications_enabled,
            low_balance_threshold: update.low_balance_threshold,
            auto_topup_amount: update.auto_topup_amount,
            auto_topup_threshold: update.auto_topup_threshold,
            auto_topup_monthly_limit: update.auto_topup_monthly_limit,
            acknowledge_login: update.acknowledge_login,
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
    pub last_login: Option<DateTime<Utc>>,
    pub auth_source: String,
    pub is_admin: bool,
    pub roles: Vec<Role>,
    pub password_hash: Option<String>,
    pub external_user_id: Option<String>,
    pub payment_provider_id: Option<String>,
    pub batch_notifications_enabled: bool,
    pub first_batch_email_sent: bool,
    pub low_balance_notification_sent: bool,
    /// Low balance notification threshold. NULL means notifications are disabled.
    pub low_balance_threshold: Option<f32>,
    /// Auto top-up amount. NULL means auto top-up is disabled.
    pub auto_topup_amount: Option<f32>,
    /// Auto top-up threshold. When balance drops below this, auto top-up triggers.
    pub auto_topup_threshold: Option<f32>,
    /// Monthly auto top-up spending limit. NULL means no limit.
    pub auto_topup_monthly_limit: Option<f32>,
    /// User type: 'individual' or 'organization'
    pub user_type: String,
}
