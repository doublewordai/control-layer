import { useMemo } from "react";
import type { Role } from "../api/control-layer/types";
import { useUser } from "../api/control-layer/hooks";

export type PagePermission =
  | "models"
  | "endpoints"
  | "playground"
  | "analytics"
  | "cost-management"
  | "users-groups"
  | "api-keys"
  | "settings"
  | "profile"
  | "manage-groups";

// Define which roles can access which pages
const ROLE_PERMISSIONS: Record<Role, PagePermission[]> = {
  PlatformManager: [
    "models",
    "endpoints",
    "playground",
    "analytics",
    "cost-management",
    "users-groups",
    "api-keys",
    "settings",
    "profile",
    "manage-groups",
  ],
  StandardUser: ["models", "api-keys", "playground", "cost-management", "profile"],
  RequestViewer: ["models", "endpoints", "analytics", "cost-management", "profile", "settings"],
};

// Map route paths to permissions
export const ROUTE_PERMISSIONS: Record<string, PagePermission> = {
  "/models": "models",
  "/endpoints": "endpoints",
  "/playground": "playground",
  "/analytics": "analytics",
  "/cost-management": "cost-management",
  "/users-groups": "users-groups",
  "/api-keys": "api-keys",
  "/settings": "settings",
  "/profile": "profile",
};

/**
 * Check if user has permission to access a specific page
 */
export function hasPermission(
  userRoles: Role[],
  permission: PagePermission,
): boolean {
  return userRoles.some((role) => ROLE_PERMISSIONS[role]?.includes(permission));
}

/**
 * Check if user can access a specific route path
 */
export function canAccessRoute(userRoles: Role[], path: string): boolean {
  const permission = ROUTE_PERMISSIONS[path];
  if (!permission) {
    return true;
  }
  return hasPermission(userRoles, permission);
}

/**
 * Get the first accessible route for a user (fallback when current route is restricted)
 */
export function getFirstAccessibleRoute(userRoles: Role[]): string {
  // Priority order for fallback routes
  const fallbackOrder: string[] = [
    "/models",
    "/playground",
    "/api-keys",
    "/profile",
  ];

  for (const route of fallbackOrder) {
    if (canAccessRoute(userRoles, route)) {
      return route;
    }
  }

  // Should never happen, but fallback to profile
  return "/profile";
}

/**
 * Hook to check user permissions
 */
export function useAuthorization() {
  const { data: currentUser, isLoading } = useUser("current");

  const permissions = useMemo(() => {
    if (!currentUser?.roles) {
      return {
        isLoading,
        userRoles: [] as Role[],
        isAdmin: false,
        hasPermission: () => false,
        canAccessRoute: () => false,
        getFirstAccessibleRoute: (): string => "/profile",
      };
    }

    const userRoles = currentUser.roles;

    return {
      isLoading,
      userRoles,
      hasPermission: (permission: PagePermission) =>
        hasPermission(userRoles, permission),
      canAccessRoute: (path: string) => canAccessRoute(userRoles, path),
      getFirstAccessibleRoute: (): string => getFirstAccessibleRoute(userRoles),
    };
  }, [currentUser, isLoading]);

  return permissions;
}
