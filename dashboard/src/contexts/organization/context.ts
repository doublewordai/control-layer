import { createContext } from "react";
import type { OrganizationSummary } from "../../api/control-layer/types";

export interface OrganizationContextValue {
  /** Currently active organization ID, or null for personal context */
  activeOrganizationId: string | null;
  /** The active organization summary (resolved from current user's memberships) */
  activeOrganization: OrganizationSummary | null;
  /** Whether the user is in an organization context */
  isOrgContext: boolean;
  /** Switch to an organization context (validates membership) or null for personal */
  setActiveOrganization: (orgId: string | null) => Promise<void>;
}

export const OrganizationContext =
  createContext<OrganizationContextValue | null>(null);
