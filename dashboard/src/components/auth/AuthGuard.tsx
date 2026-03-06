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
    return <Navigate to="/login" replace />;
  }

  // If auth is not required (login/register pages) but user is authenticated, redirect
  if (!requireAuth && isAuthenticated) {
    const redirect = searchParams.get("redirect");
    return <Navigate to={redirect || "/"} replace />;
  }

  return <>{children}</>;
}
