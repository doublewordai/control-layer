import { useState, useCallback, useEffect, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useUser } from "../../api/control-layer/hooks";
import { dwctlApi } from "../../api/control-layer/client";
import { queryKeys } from "../../api/control-layer/keys";
import { OrganizationContext } from "./context";

const STORAGE_KEY = "activeOrganizationId";

export function OrganizationProvider({ children }: { children: ReactNode }) {
  const [activeOrganizationId, setActiveOrgId] = useState<string | null>(
    () => localStorage.getItem(STORAGE_KEY),
  );
  const queryClient = useQueryClient();
  const { data: currentUser } = useUser("current", { include: "organizations" });

  // Validate stored org ID against current user's memberships
  useEffect(() => {
    if (!currentUser?.organizations || !activeOrganizationId) return;
    const isMember = currentUser.organizations.some(
      (org) => org.id === activeOrganizationId,
    );
    if (!isMember) {
      localStorage.removeItem(STORAGE_KEY);
      setActiveOrgId(null);
    }
  }, [currentUser?.organizations, activeOrganizationId]);

  const activeOrganization =
    currentUser?.organizations?.find(
      (org) => org.id === activeOrganizationId,
    ) ?? null;

  const setActiveOrganization = useCallback(
    async (orgId: string | null) => {
      // Validate membership with the backend
      await dwctlApi.organizations.setActive(orgId);

      if (orgId) {
        localStorage.setItem(STORAGE_KEY, orgId);
      } else {
        localStorage.removeItem(STORAGE_KEY);
      }
      setActiveOrgId(orgId);

      // Invalidate queries that are scoped to user/org context
      queryClient.invalidateQueries({ queryKey: queryKeys.models.all });
      queryClient.invalidateQueries({ queryKey: queryKeys.apiKeys.all });
      queryClient.invalidateQueries({ queryKey: queryKeys.batches.all });
      queryClient.invalidateQueries({ queryKey: queryKeys.files.all });
      queryClient.invalidateQueries({ queryKey: queryKeys.usage.all });
      queryClient.invalidateQueries({ queryKey: queryKeys.webhooks.all });
    },
    [queryClient],
  );

  return (
    <OrganizationContext.Provider
      value={{
        activeOrganizationId,
        activeOrganization,
        isOrgContext: activeOrganizationId !== null,
        setActiveOrganization,
      }}
    >
      {children}
    </OrganizationContext.Provider>
  );
}
