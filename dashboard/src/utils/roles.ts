import type { Role } from "../api/control-layer/types";

// Available user roles - must match Role type
export const AVAILABLE_ROLES: Role[] = [
  "PlatformManager",
  "RequestViewer",
  "StandardUser",
];

// Roles available for editing in user management forms (excludes StandardUser)
export const EDITABLE_ROLES: Role[] = ["PlatformManager", "RequestViewer"];

/**
 * Format role for display (PLATFORMMANAGER -> Platform Manager, etc.)
 */
export const formatRoleForDisplay = (role: Role): string => {
  const displayNames: Record<Role, string> = {
    PlatformManager: "Platform Manager",
    RequestViewer: "Request Viewer",
    StandardUser: "Standard User",
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
