import { useCallback, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useUser } from "../../api/control-layer/hooks";
import { dwctlApi } from "../../api/control-layer/client";
import { queryKeys } from "../../api/control-layer/keys";
import { OrganizationContext } from "./context";

export function OrganizationProvider({ children }: { children: ReactNode }) {
  const queryClient = useQueryClient();
  const { data: currentUser } = useUser("current", { include: "organizations" });

  // Derive active organization from the server response (source of truth)
  const activeOrganizationId = currentUser?.active_organization_id ?? null;

  const activeOrganization =
    currentUser?.organizations?.find(
      (org) => org.id === activeOrganizationId,
    ) ?? null;

  const setActiveOrganization = useCallback(
    async (orgId: string | null) => {
      // Update the server-side cookie
      await dwctlApi.organizations.setActive(orgId);

      // Re-fetch current user to get updated active_organization_id
      await queryClient.invalidateQueries({
        queryKey: queryKeys.users.byId("current", "organizations"),
      });

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
