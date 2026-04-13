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
  const { user, isAuthenticated } = useAuth();

  useEffect(() => {
    if (!isAuthenticated || !user) {
      identifyUser(null);
      return;
    }
    identifyUser(user.id, {
      roles: user.roles,
      auth_source: user.auth_source,
    });
  }, [isAuthenticated, user]);

  return null;
}
