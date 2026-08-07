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
    /// Organization domain or unique identifier (becomes username, used for domain-based auto-join)
    #[schema(example = "acme.com")]
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
    /// Account-wide zero-data-retention flag. Admin-only: only callers with
    /// UpdateAll on organizations may set this. Omit to leave unchanged.
    pub zero_data_retention: Option<bool>,
    /// Admit signups from this workspace's claimed email domain as members
    /// automatically. Owner-only, like zero data retention: it decides who
    /// gets into the workspace without anyone reviewing them, which is not a
    /// call an admin should be able to make unilaterally. Omit to leave
    /// unchanged.
    pub auto_join_enabled: Option<bool>,
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
    /// A pending email change awaiting verification. Present when the
    /// caller has requested a new contact email but it has not yet been
    /// confirmed via the link sent to the new address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_email_change: Option<PendingEmailChangeResponse>,
    /// Whether a signup whose email domain matches this workspace's claimed
    /// domain is admitted as a member on the spot, instead of being offered
    /// the choice to request access. Off unless an owner or admin turns it on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_join_enabled: Option<bool>,
}

impl OrganizationResponse {
    pub fn from_user(user: UserResponse) -> Self {
        Self {
            user,
            member_count: None,
            pending_email_change: None,
            auto_join_enabled: None,
        }
    }

    pub fn with_member_count(mut self, count: i64) -> Self {
        self.member_count = Some(count);
        self
    }

    pub fn with_pending_email_change(mut self, info: PendingEmailChangeResponse) -> Self {
        self.pending_email_change = Some(info);
        self
    }

    pub fn with_auto_join_enabled(mut self, enabled: bool) -> Self {
        self.auto_join_enabled = Some(enabled);
        self
    }
}

/// Public projection of a pending organization email change.
///
/// Deliberately omits the row id, requested-by user, and token-related
/// fields stored in [`PendingOrgEmailChangeDBResponse`] so that no
/// token-adjacent metadata leaks out of the API layer.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PendingEmailChangeResponse {
    /// The address the contact email will become if the verification link is clicked.
    pub new_email: String,
    /// When the verification token expires.
    pub expires_at: DateTime<Utc>,
}

impl From<crate::db::models::organizations::PendingOrgEmailChangeDBResponse> for PendingEmailChangeResponse {
    fn from(p: crate::db::models::organizations::PendingOrgEmailChangeDBResponse) -> Self {
        Self {
            new_email: p.new_email,
            expires_at: p.expires_at,
        }
    }
}

/// Optional body for approving a join request.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, Default)]
pub struct ApproveJoinRequestRequest {
    /// Role to grant on approval. Defaults to 'member'.
    #[serde(default)]
    pub role: Option<String>,
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
    /// Membership status: 'active' for current members, 'pending' for an
    /// invite the organization sent, or 'requested' for someone asking to
    /// join and awaiting a decision.
    pub status: String,
    /// When the membership was created
    pub created_at: DateTime<Utc>,
    /// Effective key-creation capability: owners/admins always true; members
    /// true only when their membership carries the additive 'manage_keys'
    /// org role. Governs whether the member can create/edit their own API
    /// keys (without it they hold issued keys: view + rotate only).
    pub can_manage_keys: bool,
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
    /// For base role 'member': may they create and manage their own API keys?
    /// Defaults to FALSE — new members hold issued keys only until an org
    /// admin opts them in. Ignored for owner/admin (implicitly true).
    pub can_manage_keys: Option<bool>,
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
    /// For base role 'member': may they create and manage their own API keys?
    /// Defaults to FALSE — new members hold issued keys only until an org
    /// admin opts them in. Ignored for owner/admin (implicitly true).
    pub can_manage_keys: Option<bool>,
}

/// Request body for updating a member's role
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpdateMemberRoleRequest {
    /// New role: 'owner', 'admin', or 'member'
    pub role: String,
    /// Grant/revoke the 'manage_keys' org role alongside the base role.
    /// Absent = leave unchanged. Only meaningful for base role 'member'
    /// (owners/admins hold the capability implicitly).
    pub can_manage_keys: Option<bool>,
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
    /// Whether the organization is flagged for zero data retention. Account-wide
    /// flag on the org; surfaced here so a member can see their org's ZDR status.
    pub zero_data_retention: bool,
    /// Effective key-creation capability in this org: owners/admins always
    /// true; members true only with the additive 'manage_keys' org role.
    /// Drives the dashboard's create/edit affordances and the batch key
    /// selection requirement.
    pub can_manage_keys: bool,
    /// Whether the organization has proven a payment method - a completed
    /// purchase or a setup-mode card verification. Surfaced alongside the
    /// other org-wide flags so a member can see their organization's status
    /// without a second request per organization.
    ///
    /// Distinct from the same field on the *user*: paying as an org admin
    /// verifies the organization, not you, so a client showing "is this
    /// workspace able to pay" must read it from here when acting as an org.
    pub verified: bool,
}

/// An organization the caller has asked to join and is waiting on.
///
/// Signup files these automatically when the caller's email domain matches a
/// claimed organization, so the requester is typically unaware one exists.
/// Onboarding reads this to say so, rather than presenting the create-workspace
/// screen as if their company had no workspace.
///
/// Deliberately thinner than [`OrganizationSummary`]: the caller is not a
/// member, so this carries only what identifies the organization they are
/// queued for. Nothing about its membership, billing or configuration is
/// readable from here.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PendingJoinRequestResponse {
    /// The join request itself, not the organization. A `user_organizations`
    /// row id — deliberately `Uuid` rather than `UserId`, which it is not and
    /// which would invite a caller to pass it where a user is expected. Same
    /// convention as `OrganizationMemberResponse::id` above.
    #[schema(value_type = String, format = "uuid")]
    pub id: uuid::Uuid,
    #[schema(value_type = String, format = "uuid")]
    pub organization_id: UserId,
    /// The organization's `username`. Carries the `{domain}~{suffix}` form for
    /// organizations created since domains became claimable; clients that want
    /// to show a company name should strip at the separator.
    pub organization_name: String,
    /// When the request was filed — the moment the user asked, not the moment
    /// they signed up: nothing is filed on their behalf.
    pub requested_at: chrono::DateTime<chrono::Utc>,
}

/// An outstanding invitation addressed to the caller's own email.
///
/// The same invite the emailed link opens, reached from onboarding instead.
/// Invite tokens are stored only as hashes, so the link cannot be handed back
/// to a user who never opened the mail — this is how they find it anyway.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PendingInvitationResponse {
    /// The invitation's id. Accept or decline it at
    /// `/organizations/{organization_id}/invites/{id}/accept|decline`.
    ///
    /// A `user_organizations` row id, so `Uuid` rather than `UserId` — same
    /// reason as [`PendingJoinRequestResponse::id`].
    #[schema(value_type = String, format = "uuid")]
    pub id: uuid::Uuid,
    #[schema(value_type = String, format = "uuid")]
    pub organization_id: UserId,
    /// Display name where the organization has one, `username` otherwise —
    /// the same resolution the token-based invite screen uses, so both
    /// entry points name the organization identically.
    pub organization_name: String,
    /// The role being offered: 'owner', 'admin' or 'member'.
    pub role: String,
    /// Who sent it, where that is known and they still exist.
    pub inviter_name: Option<String>,
    /// When the invitation lapses. Expired invitations are never returned.
    pub expires_at: Option<DateTime<Utc>>,
}

/// An existing workspace that has claimed the caller's own email domain.
///
/// Deliberately anonymous. It names the **domain** and nothing else: not the
/// workspace's display name, not its size, not who owns it. The caller has
/// proven only that they receive mail at that domain, which is enough to be
/// told "your company already has a workspace here" and not enough to be told
/// anything about it. Naming it would turn every signup into a lookup of what
/// a given company runs.
///
/// For the same reason there is no endpoint that takes a domain as a
/// parameter: this one derives it from the caller's authenticated address, so
/// probing for a company's workspace means first controlling an address there.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DomainMatchResponse {
    #[schema(value_type = String, format = "uuid")]
    pub organization_id: UserId,
    /// The claimed domain, e.g. `acme.com` — extracted from the caller's own
    /// email, not from the organization's `{domain}~{suffix}` username, so
    /// clients never have to split on the separator to display it.
    #[schema(example = "acme.com")]
    pub domain: String,
    /// Whether this workspace admits the caller without review.
    ///
    /// Normally false by the time anyone reads this: when it is on, signup has
    /// already made them a member and there is nothing to report. It can still
    /// be true if the owner turned auto-join on *after* they signed up, and
    /// that case is the reason it is carried rather than assumed — a client
    /// seeing `true` should complete the join rather than ask the user to
    /// request access to a workspace that would have let them straight in.
    pub auto_join_enabled: bool,
}

/// What `POST /users/{id}/join-requests` actually did.
///
/// The caller asks to act on their domain match; the *organization's* setting
/// decides whether that is a membership or a queue position, so the outcome
/// cannot be known before the call and is reported rather than assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DomainJoinOutcome {
    /// Auto-join was on: the caller is now an active member.
    Joined,
    /// Auto-join was off: an owner or admin has to approve.
    Requested,
    /// The caller was already an active member. Nothing changed.
    AlreadyMember,
}

/// Result of acting on a domain match.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DomainJoinResponse {
    pub outcome: DomainJoinOutcome,
    #[schema(value_type = String, format = "uuid")]
    pub organization_id: UserId,
    /// The queued request, when `outcome` is `requested`. Absent otherwise.
    pub join_request: Option<PendingJoinRequestResponse>,
}

/// Everything onboarding needs to decide which screen to show, in one read.
///
/// One endpoint rather than three because the decision is a single branch and
/// the client cannot render anything until it has all of it — three calls
/// would mean either a flash of the wrong screen or three waterfalled
/// spinners. The precedence is the caller's to apply and is documented on the
/// handler.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OnboardingContextResponse {
    /// Someone invited this exact address. Takes precedence over everything
    /// else: it is the only signal that a human deliberately chose to admit
    /// this person.
    pub invitation: Option<PendingInvitationResponse>,
    /// A workspace has claimed the caller's email domain, and has not opted
    /// into admitting its domain automatically. Absent when auto-join was on,
    /// because signup will already have made them a member.
    pub domain_match: Option<DomainMatchResponse>,
    /// The caller has already asked to join, and is waiting. Present on a
    /// return visit to onboarding after clicking "Request access".
    pub join_request: Option<PendingJoinRequestResponse>,
}
