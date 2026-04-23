import { Navigate, useLocation, useSearchParams } from "react-router-dom";
import { useAuth } from "../../contexts/auth";
import { mergePreservedParams } from "../../utils/url";

interface AuthGuardProps {
  children: React.ReactNode;
  requireAuth?: boolean;
}

export function AuthGuard({ children, requireAuth = false }: AuthGuardProps) {
  const { isAuthenticated, isLoading } = useAuth();
  const [searchParams] = useSearchParams();
  const { search } = useLocation();

  if (isLoading) {
    return (
      <div className="flex items-center justify-center h-screen">
        <div className="animate-spin rounded-full h-12 w-12 border-b-2 border-doubleword-accent-blue"></div>
      </div>
    );
  }

  // If auth is required but user is not authenticated, redirect to login
  if (requireAuth && !isAuthenticated) {
    return <Navigate to={`/login${search}`} replace />;
  }

  // If auth is not required (login/register pages) but user is authenticated, redirect
  if (!requireAuth && isAuthenticated) {
    const redirect = searchParams.get("redirect");
    const target = redirect || "/";
    return <Navigate to={mergePreservedParams(target, searchParams)} replace />;
  }

  return <>{children}</>;
}
