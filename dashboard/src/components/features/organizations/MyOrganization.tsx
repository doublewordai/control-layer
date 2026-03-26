import { useOrganization } from "@/api/control-layer/hooks";
import { useOrganizationContext } from "@/contexts";
import { MemberManagement } from "./MemberManagement";
import { NotificationSettings } from "../notifications/NotificationSettings";
import { Building } from "lucide-react";

export function MyOrganization() {
  const { activeOrganizationId, activeOrganization } =
    useOrganizationContext();
  const { data: org, isLoading } = useOrganization(activeOrganizationId ?? "");

  if (!activeOrganizationId || !activeOrganization) {
    return (
      <div className="p-6">
        <div className="flex flex-col items-center justify-center h-64 text-center">
          <Building className="h-12 w-12 text-muted-foreground mb-4" />
          <p className="text-muted-foreground">
            Select an organization from the sidebar menu to view its details.
          </p>
        </div>
      </div>
    );
  }

  if (isLoading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="animate-spin rounded-full h-12 w-12 border-b-2 border-doubleword-accent-blue" />
      </div>
    );
  }

  const canManage =
    activeOrganization.role === "owner" || activeOrganization.role === "admin";

  return (
    <div className="p-6 space-y-6">
      <div>
        <h1 className="text-2xl font-bold tracking-tight">
          {org?.display_name || org?.username || activeOrganization.name}
        </h1>
        <div className="flex items-center gap-3 mt-1 text-sm text-muted-foreground flex-wrap">
          {org?.email && <span>{org.email}</span>}
          {org?.email && <span>·</span>}
          <span>{org?.member_count ?? 0} members</span>
          {org?.credit_balance !== undefined && (
            <>
              <span>·</span>
              <span className="font-mono tabular-nums">${org.credit_balance.toFixed(2)}</span>
            </>
          )}
          {org && (
            <>
              <span>·</span>
              <span>Created {new Date(org.created_at).toLocaleDateString()}</span>
            </>
          )}
        </div>
      </div>

      <MemberManagement
        organizationId={activeOrganizationId}
        readOnly={!canManage}
      />

      {canManage && (
        <NotificationSettings userId={activeOrganizationId} isOrganization />
      )}
    </div>
  );
}
