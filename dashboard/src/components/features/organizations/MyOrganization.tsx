import { useOrganization } from "@/api/control-layer/hooks";
import { useOrganizationContext } from "@/contexts";
import { MemberManagement } from "./MemberManagement";
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
        <h1 className="text-3xl font-bold text-doubleword-neutral-900">
          {org?.display_name || org?.username || activeOrganization.name}
        </h1>
        {org?.email && (
          <p className="text-doubleword-neutral-600 mt-1">{org.email}</p>
        )}
        {org?.display_name && org?.username && (
          <p className="text-sm text-muted-foreground">
            Slug: {org.username}
          </p>
        )}
      </div>

      <div className="grid grid-cols-3 gap-4">
        <div className="border rounded-lg p-4">
          <p className="text-sm text-muted-foreground">Members</p>
          <p className="text-2xl font-bold">{org?.member_count ?? "—"}</p>
        </div>
        <div className="border rounded-lg p-4">
          <p className="text-sm text-muted-foreground">Balance</p>
          <p className="text-2xl font-bold">
            {org?.credit_balance !== undefined
              ? `$${org.credit_balance.toFixed(2)}`
              : "—"}
          </p>
        </div>
        <div className="border rounded-lg p-4">
          <p className="text-sm text-muted-foreground">Created</p>
          <p className="text-2xl font-bold">
            {org ? new Date(org.created_at).toLocaleDateString() : "—"}
          </p>
        </div>
      </div>

      <MemberManagement
        organizationId={activeOrganizationId}
        readOnly={!canManage}
      />
    </div>
  );
}
