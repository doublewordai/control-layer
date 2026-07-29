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

/// Database request for updating an organization.
#[derive(Debug, Clone)]
pub struct OrganizationUpdateDBRequest {
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    /// Direct write to the organization's contact email.
    ///
    /// **Security invariant:** user-facing PATCH (`update_organization`) must
    /// always pass `None` here. The contact email is rendered into Stripe
    /// receipts, invitation emails and audit notifications, so a silent
    /// change could redirect security-sensitive mail to an attacker. The
    /// only caller permitted to set `Some(_)` is the email-verification
    /// flow (`confirm_email_change`), which proves possession of the new
    /// mailbox via a hashed token before applying the change.
    pub email: Option<String>,
    pub batch_notifications_enabled: Option<bool>,
    /// `None` = don't change, `Some(None)` = disable, `Some(Some(val))` = set threshold.
    pub low_balance_threshold: Option<Option<f32>>,
    /// Account-wide zero-data-retention flag. `None` = don't change.
    pub zero_data_retention: Option<bool>,
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

/// Database response for a pending organization email change.
///
/// Carries both token-side confirmation timestamps. The change is applied
/// to `users.email` only when both `old_email_confirmed_at` and
/// `new_email_confirmed_at` are non-null; until then the row sits as a
/// pending request that supersedes future PATCHes via the UNIQUE
/// `organization_id` constraint.
#[derive(Debug, Clone)]
pub struct PendingOrgEmailChangeDBResponse {
    pub id: uuid::Uuid,
    pub organization_id: UserId,
    pub new_email: String,
    pub requested_by: UserId,
    pub new_email_confirmed_at: Option<DateTime<Utc>>,
    pub old_email_confirmed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl PendingOrgEmailChangeDBResponse {
    /// Returns true when both mailboxes have clicked their verification link.
    pub fn is_fully_confirmed(&self) -> bool {
        self.new_email_confirmed_at.is_some() && self.old_email_confirmed_at.is_some()
    }
}
