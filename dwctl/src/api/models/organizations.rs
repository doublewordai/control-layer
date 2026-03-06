//! API request/response models for organizations.

use crate::api::models::pagination::Pagination;
use crate::api::models::users::UserResponse;

use crate::types::UserId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Request body for creating a new organization
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OrganizationCreate {
    /// Unique slug/handle for the organization (becomes username)
    #[schema(example = "acme-corp")]
    pub name: String,
    /// Organization contact email (for billing, notifications)
    #[schema(example = "admin@acme.com")]
    pub email: String,
    /// Human-readable display name
    #[schema(example = "Acme Corporation")]
    pub display_name: Option<String>,
}

/// Request body for updating an organization
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OrganizationUpdate {
    /// New display name
    pub display_name: Option<String>,
    /// New contact email
    pub email: Option<String>,
}

/// Full organization details returned by the API.
/// Organizations are users with user_type = 'organization', so this wraps UserResponse
/// with additional org-specific fields.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OrganizationResponse {
    /// The organization's user record
    #[serde(flatten)]
    pub user: UserResponse,
    /// Number of members in the organization
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member_count: Option<i64>,
}

impl OrganizationResponse {
    pub fn from_user(user: UserResponse) -> Self {
        Self { user, member_count: None }
    }

    pub fn with_member_count(mut self, count: i64) -> Self {
        self.member_count = Some(count);
        self
    }
}

/// Organization member details
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OrganizationMemberResponse {
    /// The member's user details
    pub user: UserResponse,
    /// Role in the organization: 'owner', 'admin', or 'member'
    pub role: String,
    /// When the membership was created
    pub created_at: DateTime<Utc>,
}

/// Request body for adding a member to an organization
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AddMemberRequest {
    /// User ID to add as a member
    #[schema(value_type = String, format = "uuid")]
    pub user_id: UserId,
    /// Role to assign: 'owner', 'admin', or 'member' (defaults to 'member')
    pub role: Option<String>,
}

/// Request body for updating a member's role
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpdateMemberRoleRequest {
    /// New role: 'owner', 'admin', or 'member'
    pub role: String,
}

/// Query parameters for listing organizations
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct ListOrganizationsQuery {
    /// Pagination parameters
    #[serde(flatten)]
    #[param(inline)]
    pub pagination: Pagination,

    /// Search query to filter by name, display_name, or email
    pub search: Option<String>,

    /// Include related data (comma-separated: "member_count")
    pub include: Option<String>,
}

/// Request body for setting/clearing active organization context
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SetActiveOrganizationRequest {
    /// Organization ID to activate, or null to clear
    #[schema(value_type = Option<String>, format = "uuid")]
    pub organization_id: Option<UserId>,
}

/// Response for the set active organization endpoint
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SetActiveOrganizationResponse {
    /// The active organization ID, or null if cleared
    #[schema(value_type = Option<String>, format = "uuid")]
    pub active_organization_id: Option<UserId>,
}

/// Summary of an organization for inclusion in user responses
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OrganizationSummary {
    #[schema(value_type = String, format = "uuid")]
    pub id: UserId,
    pub name: String,
    pub role: String,
}
