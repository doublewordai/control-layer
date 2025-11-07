import { BrowserRouter, Routes, Route, Navigate } from "react-router-dom";
import { useEffect } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { ReactQueryDevtools } from "@tanstack/react-query-devtools";
import { Toaster } from "./components/ui/sonner";
import {
  ApiKeys,
  CostManagement,
  Endpoints,
  Models,
  ModelInfo,
  Playground,
  Profile,
  Requests,
  Settings,
  UsersGroups,
} from "./components/features";
import { AppLayout } from "./components/layout";
import {
  ProtectedRoute,
  LoginForm,
  RegisterForm,
  AuthGuard,
  PasswordResetRequestForm,
  PasswordResetForm,
} from "./components/auth";
import { SettingsProvider, useSettings } from "./contexts";
import { AuthProvider, useAuth } from "./contexts/auth";
import { useAuthorization } from "./utils";
import { useRegistrationInfo } from "./api/control-layer/hooks";

// Create a client
const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 5 * 60 * 1000, // 5 minutes
      gcTime: 10 * 60 * 1000, // 10 minutes (was cacheTime in v4)
      refetchOnWindowFocus: false,
      retry: (failureCount, error) => {
        // Don't retry on 401/403/404 (auth/not found errors) - fail fast
        if (error instanceof Error) {
          // Check if it's our ApiError with status property
          if (
            "status" in error &&
            (error.status === 401 ||
              error.status === 403 ||
              error.status === 404)
          ) {
            return false;
          }
          // Also check error message for "401", "403", or "404" as fallback
          if (
            error.message.includes("401") ||
            error.message.includes("403") ||
            error.message.includes("404")
          ) {
            return false;
          }
        }
        // Default retry behavior for other errors (3 retries)
        return failureCount < 3;
      },
    },
  },
});

function RootRedirect() {
  const { isAuthenticated, isLoading } = useAuth();
  const { getFirstAccessibleRoute, isLoading: authorizationLoading } =
    useAuthorization();

  // Show loading if either auth or authorization is loading
  if (isLoading || authorizationLoading) {
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

  // If authenticated, redirect to first accessible route
  return <Navigate to={getFirstAccessibleRoute()} replace />;
}

function AppRoutes() {
  const { isFeatureEnabled, isMswReady, setMswReady } = useSettings();
  const { data: registrationInfo } = useRegistrationInfo();

  // Initialize MSW based on demo mode
  useEffect(() => {
    async function enableMocking() {
      if (!isFeatureEnabled("demo")) {
        return;
      }

      console.log("Demo mode is enabled, setting up MSW...");
      try {
        const { worker } = await import("./utils/msw");

        const registration = await worker.start({
          onUnhandledRequest: "bypass",
          serviceWorker: {
            url: "/mockServiceWorker.js",
          },
        });

        console.log("MSW started successfully", registration);
        setMswReady(true);
      } catch (error) {
        console.error("Failed to start MSW:", error);
        setMswReady(true); // Set ready even on failure to prevent indefinite blocking
      }
    }

    enableMocking();
  }, [isFeatureEnabled, setMswReady]);

  // Show loading screen while MSW initializes in demo mode
  if (isFeatureEnabled("demo") && !isMswReady) {
    return (
      <div className="min-h-screen bg-doubleword-background-secondary flex items-center justify-center">
        <div className="text-center">
          <div className="animate-spin rounded-full h-12 w-12 border-b-2 border-doubleword-accent-blue mx-auto mb-4"></div>
          <p className="text-doubleword-neutral-600">
            Initializing demo mode...
          </p>
        </div>
      </div>
    );
  }

  return (
    <BrowserRouter>
      <Routes>
        {/* Public auth routes */}
        <Route
          path="/login"
          element={
            <AuthGuard requireAuth={false}>
              <LoginForm />
            </AuthGuard>
          }
        />
        {/* Only show register route if registration is enabled */}
        {registrationInfo?.enabled && (
          <Route
            path="/register"
            element={
              <AuthGuard requireAuth={false}>
                <RegisterForm />
              </AuthGuard>
            }
          />
        )}

        {/* Password reset routes */}
        <Route
          path="/forgot-password"
          element={
            <AuthGuard requireAuth={false}>
              <PasswordResetRequestForm />
            </AuthGuard>
          }
        />
        <Route
          path="/reset-password"
          element={
            <AuthGuard requireAuth={false}>
              <PasswordResetForm />
            </AuthGuard>
          }
        />

        {/* Protected app routes with layout */}
        <Route path="/" element={<RootRedirect />} />
        <Route
          path="/analytics"
          element={
            <AppLayout>
              <ProtectedRoute path="/analytics">
                <Requests />
              </ProtectedRoute>
            </AppLayout>
          }
        />
        <Route
          path="/endpoints"
          element={
            <AppLayout>
              <ProtectedRoute path="/endpoints">
                <Endpoints />
              </ProtectedRoute>
            </AppLayout>
          }
        />
        <Route
          path="/models"
          element={
            <AppLayout>
              <ProtectedRoute path="/models">
                <Models />
              </ProtectedRoute>
            </AppLayout>
          }
        />
        <Route
          path="/models/:modelId"
          element={
            <AppLayout>
              <ProtectedRoute path="/models/:modelId">
                <ModelInfo />
              </ProtectedRoute>
            </AppLayout>
          }
        />
        <Route
          path="/playground"
          element={
            <AppLayout>
              <ProtectedRoute path="/playground">
                <Playground />
              </ProtectedRoute>
            </AppLayout>
          }
        />
        <Route
          path="/users-groups"
          element={
            <AppLayout>
              <ProtectedRoute path="/users-groups">
                <UsersGroups />
              </ProtectedRoute>
            </AppLayout>
          }
        />
        <Route
          path="/settings"
          element={
            <AppLayout>
              <ProtectedRoute path="/settings">
                <Settings />
              </ProtectedRoute>
            </AppLayout>
          }
        />
        <Route
          path="/profile"
          element={
            <AppLayout>
              <ProtectedRoute path="/profile">
                <Profile />
              </ProtectedRoute>
            </AppLayout>
          }
        />
        <Route
          path="/api-keys"
          element={
            <AppLayout>
              <ProtectedRoute path="/api-keys">
                <ApiKeys />
              </ProtectedRoute>
            </AppLayout>
          }
        />
        <Route
          path="/cost-management"
          element={
            <AppLayout>
              <ProtectedRoute path="/cost-management" requiredFeatureFlag="use_billing">
                <CostManagement />
              </ProtectedRoute>
            </AppLayout>
          }
        />
      </Routes>
    </BrowserRouter>
  );
}

function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <SettingsProvider>
        <AuthProvider>
          <AppRoutes />
        </AuthProvider>
      </SettingsProvider>
      <ReactQueryDevtools initialIsOpen={false} />
      <Toaster />
    </QueryClientProvider>
  );
}

export default App;
