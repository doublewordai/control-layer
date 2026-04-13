import { useEffect } from "react";
import { useAuth } from "../contexts/auth";
import { identifyUser } from "./telemetry";

/**
 * Keeps the telemetry facade in sync with the authenticated user. Renders
 * nothing. Must be placed inside `<AuthProvider>`.
 *
 * Only an opaque user id and non-PII traits are forwarded — email and other
 * personally identifiable fields are intentionally excluded so operators can
 * choose their own identity strategy in the bootstrap layer.
 */
export function TelemetryIdentity() {
  const { user, isAuthenticated, isLoading } = useAuth();

  useEffect(() => {
    // Don't emit identity events while the auth check is still in flight —
    // otherwise every page load fires a spurious "clear identity" that can
    // mis-attribute startup errors as anonymous.
    if (isLoading) {
      return;
    }
    if (!isAuthenticated || !user) {
      identifyUser(null);
      return;
    }
    identifyUser(user.id, {
      roles: user.roles,
      auth_source: user.auth_source,
    });
  }, [isAuthenticated, isLoading, user]);

  return null;
}
