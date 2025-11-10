import { Navigate } from "react-router-dom";
import type { ReactNode } from "react";
import { useAuth } from "../../contexts/auth";
import { useAuthorization } from "../../utils/authorization";
import { useSettings } from "../../contexts";
import type { FeatureFlags } from "../../contexts/settings/types";

interface ProtectedRouteProps {
  children: ReactNode;
  path: string;
  requiredFeatureFlag?: keyof FeatureFlags;
}

export function ProtectedRoute({ children, path, requiredFeatureFlag }: ProtectedRouteProps) {
  const { isAuthenticated, isLoading: authLoading } = useAuth();
  const {
    canAccessRoute,
    getFirstAccessibleRoute,
    isLoading: authorizationLoading,
  } = useAuthorization();
  const { isFeatureEnabled } = useSettings();

  // Show loading state while checking authentication or permissions
  if (authLoading || authorizationLoading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="animate-spin rounded-full h-12 w-12 border-b-2 border-doubleword-accent-blue"></div>
      </div>
    );
  }

  // If not authenticated, redirect to login page (will work for both native and proxy auth)
  if (!isAuthenticated) {
    return <Navigate to="/login" replace />;
  }

  // Check if required feature flag is enabled
  if (requiredFeatureFlag && !isFeatureEnabled(requiredFeatureFlag)) {
    const fallbackRoute = getFirstAccessibleRoute();
    return <Navigate to={fallbackRoute} replace />;
  }

  // Check if user can access this route
  if (!canAccessRoute(path)) {
    // Redirect to the first accessible route
    const fallbackRoute = getFirstAccessibleRoute();
    return <Navigate to={fallbackRoute} replace />;
  }

  return <>{children}</>;
}
