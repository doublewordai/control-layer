//! Database models for organizations.

use crate::types::UserId;
use chrono::{DateTime, Utc};

/// Database request for creating a new organization
#[derive(Debug, Clone)]
pub struct OrganizationCreateDBRequest {
    pub name: String,
    pub email: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub created_by: UserId,
}

/// Database request for updating an organization
#[derive(Debug, Clone)]
pub struct OrganizationUpdateDBRequest {
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub email: Option<String>,
}

/// Database response for an organization membership
#[derive(Debug, Clone)]
pub struct OrganizationMemberDBResponse {
    pub id: UserId,
    pub user_id: Option<UserId>,
    pub organization_id: UserId,
    pub role: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub invite_email: Option<String>,
    pub invited_by: Option<UserId>,
    pub expires_at: Option<DateTime<Utc>>,
}
