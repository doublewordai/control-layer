//! API request/response models for users.

use super::pagination::Pagination;
use crate::api::models::groups::GroupResponse;
use crate::db::models::users::UserDBResponse;
use crate::types::{ApiKeyId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_with::rust::double_option;
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
    /// Access to spare-capacity background inference
    BackgroundInferenceUser,
    /// Access to external data source connections and sync operations
    ConnectionsUser,
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
    /// Whether to receive email notifications when batches complete (null to keep unchanged)
    pub batch_notifications_enabled: Option<bool>,
    /// Low balance notification threshold in dollars. Set to a number to enable
    /// (e.g. 2.0 means notify when balance drops below $2), set to null to disable.
    /// Omit entirely to leave unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    pub low_balance_threshold: Option<Option<f32>>,
    /// Auto top-up amount in dollars. Set to a number to enable automatic credit
    /// replenishment, set to null to disable. Omit entirely to leave unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    pub auto_topup_amount: Option<Option<f32>>,
    /// Auto top-up threshold in dollars. When balance drops below this amount,
    /// auto top-up is triggered. Set to null to disable. Omit entirely to leave unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    pub auto_topup_threshold: Option<Option<f32>>,
    /// Monthly auto top-up spending limit in dollars. Set to a number to cap monthly
    /// auto top-up charges, set to null to remove the limit. Omit entirely to leave unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    pub auto_topup_monthly_limit: Option<Option<f32>>,
    /// Account-wide zero-data-retention flag. Users may set this on their own
    /// account; callers with UpdateAll on users may set it for any account.
    /// Omit to leave unchanged.
    pub zero_data_retention: Option<bool>,
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
    /// Whether this billing target has ever completed a purchase (only included
    /// if `include=billing` is specified).
    ///
    /// This is the ledger truth behind the first-payment-match promotion: the
    /// match is granted exactly when no earlier `purchase` exists for the
    /// target. Clients gating on "has this account already had its first
    /// top-up?" must read this rather than `has_payment_provider_id`, which is
    /// true for accounts that only verified a card or merely attempted to turn
    /// on auto top-up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_purchased: Option<bool>,
    /// Indicates whether this user has an associated payment provider customer record.
    ///
    /// Note: This field replaces the previous `payment_provider_id` response field to avoid
    /// exposing the underlying payment provider customer ID. API consumers that previously
    /// relied on `payment_provider_id` should instead use this boolean flag and store or
    /// manage any provider-specific identifiers on their own side.
    pub has_payment_provider_id: bool,
    /// Whether the user receives email notifications when batches complete
    pub batch_notifications_enabled: bool,
    /// Low balance notification threshold in dollars. Null means notifications are disabled.
    pub low_balance_threshold: Option<f32>,
    /// Auto top-up amount in dollars. Null means auto top-up is disabled.
    pub auto_topup_amount: Option<f32>,
    /// Auto top-up threshold in dollars. When balance drops below this, auto top-up triggers.
    pub auto_topup_threshold: Option<f32>,
    /// Whether the user has a payment method set up for auto top-up.
    pub has_auto_topup_payment_method: bool,
    /// Monthly auto top-up spending limit in dollars. Null means no limit.
    pub auto_topup_monthly_limit: Option<f32>,
    /// User type: 'individual' or 'organization'
    pub user_type: String,
    /// Whether this account has proven a real payment method — either a
    /// completed purchase or a setup-mode card verification
    /// (`POST /payments/setup`). Distinct from `has_payment_provider_id`,
    /// which only says a customer record exists at the provider and is true
    /// even for a customer created just to collect a billing address.
    ///
    /// Clients gating a capability on "this account can pay" (the onboarding
    /// ZDR step, for one) should read this, not `has_payment_provider_id`.
    pub verified: bool,
    /// Account-wide zero-data-retention flag.
    pub zero_data_retention: bool,
    /// Whether this account is billed by emailed invoice rather than an
    /// immediate card charge. Enabled by Doubleword after approval, not
    /// self-service - it extends credit on payment terms.
    pub invoicing_enabled: bool,
    /// Organizations this user belongs to (only included if `include=organizations` is specified)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organizations: Option<Vec<super::organizations::OrganizationSummary>>,
    /// Active organization ID from the session cookie (only present for /users/current)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "uuid")]
    pub active_organization_id: Option<UserId>,
    /// Onboarding redirect URL (only present for /users/current when last_login is null and onboarding_url is configured)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub onboarding_redirect_url: Option<String>,
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
    /// Organizations the user belongs to
    pub organizations: Vec<UserOrganizationContext>,
    /// Active organization ID (from X-Organization-Id header)
    #[schema(value_type = Option<String>, format = "uuid")]
    pub active_organization: Option<UserId>,
    /// The API key that authenticated this request, when the request was
    /// authenticated by API key (None for session/JWT/proxy-header auth).
    /// Internal-only: lets handlers resolve per-key state — e.g. batch/file
    /// creation picking a capped key's cap-scope child as the execution key.
    /// Never serialized to clients.
    #[serde(skip)]
    pub api_key_id: Option<ApiKeyId>,
}

/// Context about a user's organization membership
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserOrganizationContext {
    #[schema(value_type = String, format = "uuid")]
    pub id: UserId,
    pub name: String,
    pub role: String,
    /// Effective key-creation capability in this org: owners/admins always
    /// true; members true only with the additive 'manage_keys' org role.
    pub can_manage_keys: bool,
}

impl CurrentUser {
    #[cfg(test)]
    pub fn is_admin(&self) -> bool {
        self.is_admin
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
            last_login: db.last_login,
            groups: None,         // By default, relationships are not included
            credit_balance: None, // By default, credit balances are not included
            has_purchased: None,  // Ditto: only populated for include=billing
            has_payment_provider_id: db.payment_provider_id.as_ref().is_some_and(|s| !s.is_empty()),
            batch_notifications_enabled: db.batch_notifications_enabled,
            low_balance_threshold: db.low_balance_threshold,
            auto_topup_amount: db.auto_topup_amount,
            auto_topup_threshold: db.auto_topup_threshold,
            has_auto_topup_payment_method: db.payment_provider_id.as_ref().is_some_and(|s| !s.is_empty()),
            auto_topup_monthly_limit: db.auto_topup_monthly_limit,
            user_type: db.user_type,
            verified: db.verified,
            zero_data_retention: db.zero_data_retention,
            invoicing_enabled: db.invoicing_enabled,
            organizations: None,
            active_organization_id: None,
            onboarding_redirect_url: None,
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

    /// Create a response with the first-purchase flag included
    pub fn with_has_purchased(mut self, has_purchased: bool) -> Self {
        self.has_purchased = Some(has_purchased);
        self
    }

    /// Create a response with organizations included
    pub fn with_organizations(mut self, organizations: Vec<super::organizations::OrganizationSummary>) -> Self {
        self.organizations = Some(organizations);
        self
    }

    /// Set the active organization ID (from session cookie)
    pub fn with_active_organization(mut self, id: Option<UserId>) -> Self {
        self.active_organization_id = id;
        self
    }

    /// Set the onboarding redirect URL (for first-time users)
    pub fn with_onboarding_redirect_url(mut self, url: String) -> Self {
        self.onboarding_redirect_url = Some(url);
        self
    }
}

impl From<UserDBResponse> for CurrentUser {
    fn from(db: UserDBResponse) -> Self {
        Self {
            id: db.id,
            username: db.username,
            email: db.email,
            is_admin: db.is_admin,
            roles: db.roles,
            display_name: db.display_name,
            avatar_url: db.avatar_url,
            payment_provider_id: db.payment_provider_id,
            organizations: vec![],
            active_organization: None,
            api_key_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GetUserQuery {
    /// Include related data (comma-separated: "groups", "billing")
    #[schema(example = "groups,billing")]
    pub include: Option<String>,
}
