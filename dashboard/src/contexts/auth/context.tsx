import { useEffect, useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { clayApi } from "../../api/clay/client";
import { queryKeys } from "../../api/clay/keys";
import { AuthContext } from "./auth-context";
import type {
  AuthContextValue,
  AuthState,
  LoginCredentials,
  RegisterCredentials,
} from "./types";

interface AuthProviderProps {
  children: ReactNode;
}

export function AuthProvider({ children }: AuthProviderProps) {
  const [authState, setAuthState] = useState<AuthState>({
    user: null,
    isAuthenticated: false,
    isLoading: true,
    authMethod: null,
  });

  const queryClient = useQueryClient();

  // Check authentication status on mount
  useEffect(() => {
    checkAuthStatus();
  }, []);

  const checkAuthStatus = async () => {
    try {
      setAuthState((prev) => ({ ...prev, isLoading: true }));

      // Try to get current user (works for both proxy and native auth)
      const user = await clayApi.users.get("current");

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
    const response = await clayApi.auth.login(credentials);

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
    const response = await clayApi.auth.register(credentials);

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
      await clayApi.auth.logout();
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
