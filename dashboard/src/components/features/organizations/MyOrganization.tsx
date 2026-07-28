import { useOrganization, useUpdateOrganization } from "@/api/control-layer/hooks";
import type { KeyManagementMode } from "@/api/control-layer/types";
import { useOrganizationContext } from "@/contexts";
import { MemberManagement } from "./MemberManagement";
import { NotificationSettings } from "../notifications/NotificationSettings";
import { Badge } from "@/components/ui/badge";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { Building, ShieldCheck } from "lucide-react";
import { toast } from "sonner";

const KEY_MANAGEMENT_OPTIONS: {
  value: KeyManagementMode;
  label: string;
  description: string;
}[] = [
  {
    value: "open",
    label: "Self-serve (open)",
    description: "Members create and manage their own API keys.",
  },
  {
    value: "managed",
    label: "Admin-managed",
    description:
      "Only owners and admins create keys and issue them to members; members can view and copy their issued keys but not change them.",
  },
];

interface KeyManagementSettingsProps {
  organizationId: string;
  mode: KeyManagementMode;
  readOnly: boolean;
}

function KeyManagementSettings({
  organizationId,
  mode,
  readOnly,
}: KeyManagementSettingsProps) {
  const updateOrgMutation = useUpdateOrganization();

  const handleModeChange = async (value: string) => {
    const newMode = value as KeyManagementMode;
    if (newMode === mode) return;
    try {
      await updateOrgMutation.mutateAsync({
        id: organizationId,
        data: { key_management_mode: newMode },
      });
      toast.success("API key management setting updated");
    } catch (error) {
      toast.error(
        error instanceof Error
          ? error.message
          : "Failed to update API key management setting",
      );
    }
  };

  return (
    <div className="bg-white rounded-lg shadow-sm border border-gray-200 p-6">
      <h4 className="text-lg font-medium text-gray-900 mb-1">
        API key management
      </h4>
      <p className="text-xs text-gray-500 mb-4">
        {readOnly
          ? "Controls who can create and manage the organization's API keys. Contact an owner or admin to make changes."
          : "Controls who can create and manage the organization's API keys."}
      </p>
      <RadioGroup
        value={mode}
        onValueChange={handleModeChange}
        disabled={readOnly || updateOrgMutation.isPending}
        aria-label="API key management mode"
      >
        {KEY_MANAGEMENT_OPTIONS.map((option) => (
          <div
            key={option.value}
            className={`flex items-start gap-3 rounded-lg border p-3 ${
              mode === option.value
                ? "border-doubleword-accent-blue bg-blue-50/50"
                : "border-gray-200"
            }`}
          >
            <RadioGroupItem
              value={option.value}
              id={`key-management-${option.value}`}
              className="mt-0.5"
            />
            <div>
              <Label
                htmlFor={`key-management-${option.value}`}
                className={`text-sm font-medium text-gray-900 ${
                  readOnly ? "" : "cursor-pointer"
                }`}
              >
                {option.label}
              </Label>
              <p className="text-xs text-gray-500 mt-0.5">
                {option.description}
              </p>
            </div>
          </div>
        ))}
      </RadioGroup>
      <p className="text-xs text-gray-500 mt-3">
        Changing this setting does not affect existing keys — only who can
        create and edit keys from now on.
      </p>
    </div>
  );
}

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
          {org?.zero_data_retention && (
            <Badge
              variant="secondary"
              title="Request and response payloads are not retained for this organization"
            >
              <ShieldCheck />
              Zero data retention
            </Badge>
          )}
        </div>
      </div>

      <MemberManagement
        organizationId={activeOrganizationId}
        readOnly={!canManage}
      />

      <KeyManagementSettings
        organizationId={activeOrganizationId}
        mode={org?.key_management_mode ?? "open"}
        readOnly={!canManage}
      />

      <NotificationSettings
        userId={activeOrganizationId}
        isOrganization
        readOnly={!canManage}
      />
    </div>
  );
}
