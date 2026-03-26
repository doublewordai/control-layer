//! API request/response models for organizations.

use crate::api::models::pagination::Pagination;
use crate::api::models::users::UserResponse;

use crate::types::UserId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_with::rust::double_option;
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
    /// User ID to assign as owner (platform managers only; defaults to current user)
    #[schema(value_type = Option<String>, format = "uuid")]
    pub owner_id: Option<UserId>,
}

/// Request body for updating an organization
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OrganizationUpdate {
    /// New display name
    pub display_name: Option<String>,
    /// New contact email
    pub email: Option<String>,
    /// Whether batch completion/failure email notifications are enabled
    pub batch_notifications_enabled: Option<bool>,
    /// Low balance notification threshold in dollars
    /// (e.g. 2.0 means notify when balance drops below $2), set to null to disable.
    /// Omit entirely to leave unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    pub low_balance_threshold: Option<Option<f32>>,
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
    /// Membership row ID
    #[schema(value_type = String, format = "uuid")]
    pub id: uuid::Uuid,
    /// The member's user details (None for pending invites where user hasn't signed up)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<UserResponse>,
    /// Role in the organization: 'owner', 'admin', or 'member'
    pub role: String,
    /// Membership status: 'active' or 'pending'
    pub status: String,
    /// When the membership was created
    pub created_at: DateTime<Utc>,
    /// Email address for pending invites
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invite_email: Option<String>,
}

/// Request body for inviting a member by email
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InviteMemberRequest {
    /// Email address to invite
    #[schema(example = "newuser@example.com")]
    pub email: String,
    /// Role to assign: 'owner', 'admin', or 'member' (defaults to 'member')
    pub role: Option<String>,
}

/// Response after creating an invite
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InviteMemberResponse {
    /// Membership row ID
    #[schema(value_type = String, format = "uuid")]
    pub id: uuid::Uuid,
    /// Invited email address
    pub email: String,
    /// Assigned role
    pub role: String,
    /// Invite status (always 'pending')
    pub status: String,
    /// When the invite was created
    pub created_at: DateTime<Utc>,
    /// When the invite expires
    pub expires_at: DateTime<Utc>,
}

/// Details about a pending invite (returned when looking up by token)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InviteDetailsResponse {
    /// Name of the organization
    pub org_name: String,
    /// Role being offered
    pub role: String,
    /// Display name of the person who sent the invite
    pub inviter_name: Option<String>,
    /// When the invite expires
    pub expires_at: DateTime<Utc>,
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
