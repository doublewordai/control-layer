import type { OrgMemberRole, Role } from "../api/control-layer/types";

// Available user roles - must match Role type
export const AVAILABLE_ROLES: Role[] = [
  "PlatformManager",
  "RequestViewer",
  "StandardUser",
  "BillingManager",
  "BatchAPIUser",
  "ConnectionsUser",
];

// Roles whose permissions are a subset of PlatformManager.
// Used to auto-select/disable these checkboxes when PM is selected.
export const SUBSET_ROLES: Role[] = [
  "BatchAPIUser",
  "BillingManager",
  "ConnectionsUser",
];

// Roles available for editing in user management forms (excludes StandardUser)
export const EDITABLE_ROLES: Role[] = [
  "PlatformManager",
  "RequestViewer",
  "BillingManager",
  "BatchAPIUser",
  "ConnectionsUser",
];

/**
 * Format role for display (PLATFORMMANAGER -> Platform Manager, etc.)
 */
export const formatRoleForDisplay = (role: Role): string => {
  const displayNames: Record<Role, string> = {
    PlatformManager: "Platform Manager",
    RequestViewer: "Request Viewer",
    StandardUser: "Standard User",
    BillingManager: "Billing Manager",
    BatchAPIUser: "Batch API User",
    ConnectionsUser: "Connections User",
  };
  return displayNames[role] || role;
};

/**
 * Check if user has admin privileges (using is_admin flag)
 * @deprecated Use user.is_admin directly instead
 */
export const isAdmin = (isAdminFlag: boolean): boolean => {
  return isAdminFlag;
};

/**
 * Get display-friendly role labels
 */
export const getRoleDisplayName = (role: Role): string => {
  return formatRoleForDisplay(role);
};

// Organization membership roles - must match OrgMemberRole type.
// Ordered from least to most privileged.
export const ORG_MEMBER_ROLES: OrgMemberRole[] = ["member", "admin", "owner"];

/**
 * Format an organization membership role for display
 * (member -> Member, admin -> Admin, owner -> Owner).
 */
export const formatOrgRoleForDisplay = (role: OrgMemberRole): string => {
  const displayNames: Record<OrgMemberRole, string> = {
    owner: "Owner",
    admin: "Admin",
    member: "Member",
  };
  return displayNames[role] || role;
};

/**
 * Human-readable description of what an organization membership role can do.
 * Org roles are independent of platform (system) roles: they only govern
 * access within the organization context.
 */
export const getOrgRoleDescription = (role: OrgMemberRole): string => {
  const descriptions: Record<OrgMemberRole, string> = {
    member:
      "Members can use the organization: create organization API keys, view usage and credit transactions, and read members and webhooks. They cannot manage members, change organization settings, or add funds.",
    admin:
      "Admins can do everything Members can, plus manage the organization: invite and remove members, change member roles, edit organization settings and webhooks, and add funds. They cannot promote others to Owner.",
    owner:
      "Owners have full control of the organization. They can do everything Admins can, plus assign the Owner role to other members. An organization must always keep at least one Owner.",
  };
  return descriptions[role];
};
