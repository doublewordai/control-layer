import { useEffect, useState, type ReactNode } from "react";
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
import {useSettings} from "@/contexts";

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

  // Check authentication status on mount, but wait for MSW in demo mode
  useEffect(() => {
    // If in demo mode, wait for MSW to be ready before checking auth
    if (isDemoMode && !isMswReady) {
      return;
    }

    checkAuthStatus();
  }, [isDemoMode, isMswReady]);

  const checkAuthStatus = async () => {
    try {
      setAuthState((prev) => ({ ...prev, isLoading: true }));

      // Try to get current user (works for both proxy and native auth)
      const user = await dwctlApi.users.get("current");

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
  };

  const login = async (credentials: LoginCredentials) => {
    const response = await dwctlApi.auth.login(credentials);

    setAuthState({
      user: response.user,
      isAuthenticated: true,
      isLoading: false,
      authMethod: "native",
    });

    // Invalidate user queries to refresh data
    queryClient.invalidateQueries({ queryKey: queryKeys.users.all });
  };

  const register = async (credentials: RegisterCredentials) => {
    const response = await dwctlApi.auth.register(credentials);

    setAuthState({
      user: response.user,
      isAuthenticated: true,
      isLoading: false,
      authMethod: "native",
    });

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
