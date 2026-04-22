import { Navigate, useSearchParams } from "react-router-dom";
import { useAuth } from "../../contexts/auth";

interface AuthGuardProps {
  children: React.ReactNode;
  requireAuth?: boolean;
}

export function AuthGuard({ children, requireAuth = false }: AuthGuardProps) {
  const { isAuthenticated, isLoading } = useAuth();
  const [searchParams] = useSearchParams();

  if (isLoading) {
    return (
      <div className="flex items-center justify-center h-screen">
        <div className="animate-spin rounded-full h-12 w-12 border-b-2 border-doubleword-accent-blue"></div>
      </div>
    );
  }

  // If auth is required but user is not authenticated, redirect to login
  if (requireAuth && !isAuthenticated) {
    return <Navigate to={`/login${window.location.search}`} replace />;
  }

  // If auth is not required (login/register pages) but user is authenticated, redirect
  if (!requireAuth && isAuthenticated) {
    const redirect = searchParams.get("redirect");
    const target = redirect || "/";
    // Preserve non-redirect query params (e.g. utm_source) through the redirect
    const preserved = new URLSearchParams(searchParams);
    preserved.delete("redirect");
    const qs = preserved.toString();
    const separator = target.includes("?") ? "&" : "?";
    return <Navigate to={qs ? `${target}${separator}${qs}` : target} replace />;
  }

  return <>{children}</>;
}
