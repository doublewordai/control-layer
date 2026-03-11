import { useCallback, useEffect, useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { dwctlApi } from "../../api/control-layer/client";
import { queryKeys } from "../../api/control-layer/keys";
import { AuthContext } from "./auth-context";
import type {
  AuthContextValue,
  AuthState,
  LoginCredentials,
  RegisterCredentials,
} from "./types";
import { useSettings } from "@/contexts";

interface AuthProviderProps {
  children: ReactNode;
}

export function AuthProvider({ children }: AuthProviderProps) {
  const { isFeatureEnabled, isMswReady } = useSettings();
  const isDemoMode = isFeatureEnabled("demo");

  const [authState, setAuthState] = useState<AuthState>({
    user: null,
    isAuthenticated: false,
    isLoading: true,
    authMethod: null,
  });

  const queryClient = useQueryClient();

  const checkAuthStatus = useCallback(async () => {
    try {
      setAuthState((prev) => ({ ...prev, isLoading: true }));

      // Fetch directly to avoid 401 errors entering the React Query cache,
      // which would trigger the global onError handler and redirect to /login.
      // On success, populate the cache manually for downstream consumers.
      // Include organizations so OrganizationProvider can read from the same cache entry.
      const user = await dwctlApi.users.get("current", { include: "organizations" });
      queryClient.setQueryData(
        queryKeys.users.byId("current", "organizations"),
        user,
      );

      // Redirect first-time users to onboarding if configured (server sets
      // onboarding_redirect_url only when last_login is null). Org invite
      // redirect params take priority.
      if (user.onboarding_redirect_url) {
        const urlRedirect = new URLSearchParams(window.location.search).get("redirect");
        if (!urlRedirect) {
          window.location.href = user.onboarding_redirect_url;
          return;
        }
      }

      // Determine auth method based on response headers or user data
      const authMethod = user.auth_source === "native" ? "native" : "proxy";

      setAuthState({
        user,
        isAuthenticated: true,
        isLoading: false,
        authMethod,
      });
    } catch {
      // User not authenticated
      setAuthState({
        user: null,
        isAuthenticated: false,
        isLoading: false,
        authMethod: null,
      });
    }
  }, [queryClient]);

  // Check authentication status on mount, but wait for MSW in demo mode
  useEffect(() => {
    // If in demo mode, wait for MSW to be ready before checking auth
    if (isDemoMode && !isMswReady) {
      return;
    }

    checkAuthStatus();
  }, [isDemoMode, isMswReady, checkAuthStatus]);

  const login = async (credentials: LoginCredentials) => {
    await dwctlApi.auth.login(credentials);

    // Re-fetch current user to pick up onboarding redirect and full user data
    await checkAuthStatus();

    // Invalidate user queries to refresh data
    queryClient.invalidateQueries({ queryKey: queryKeys.users.all });
  };

  const register = async (credentials: RegisterCredentials) => {
    await dwctlApi.auth.register(credentials);

    // Re-fetch current user to pick up onboarding redirect and full user data
    await checkAuthStatus();

    // Invalidate user queries to refresh data
    queryClient.invalidateQueries({ queryKey: queryKeys.users.all });
  };

  const logout = async () => {
    try {
      await dwctlApi.auth.logout();
      // Always redirect to root after successful logout
      window.location.href = "/";
    } catch {
      // POST failed. Assume that its because of proxy auth and redirect to logout endpoint
      window.location.href = "/authentication/logout";
    }
  };

  const refreshUser = async () => {
    await checkAuthStatus();
  };

  const value: AuthContextValue = {
    ...authState,
    login,
    register,
    logout,
    refreshUser,
  };

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}
