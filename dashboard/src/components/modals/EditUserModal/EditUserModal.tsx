import React, { useState } from "react";
import { User, Info } from "lucide-react";
import { useUpdateUser, useConfig } from "../../../api/control-layer";
import type { Role } from "../../../api/control-layer/types";
import { SchedulingTolerations } from "../../features/scheduling/SchedulingTolerations";
import {
  AVAILABLE_ROLES,
  SUBSET_ROLES,
  getRoleDisplayName,
} from "../../../utils/roles";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
import { AlertBox } from "../../ui/alert-box";
import { HoverCard, HoverCardContent, HoverCardTrigger } from "../../ui";
import { Switch } from "../../ui/switch";

interface EditUserModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess: () => void;
  userId: string;
  currentUser: {
    name: string;
    email: string;
    username: string;
    avatar?: string;
    roles: Role[];
    zero_data_retention: boolean;
    self_hosted_only?: boolean;
    toleration_preset?: string | null;
  };
  /** Whether the current user may toggle zero data retention (admins only). */
  canEditZdr?: boolean;
  supportOnly?: boolean;
  /** Whether the current user may change the serving account settings (platform managers only). */
  canEditServing?: boolean;
}

const inputClass =
  "w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-blue-500 focus:border-transparent";

function SectionHeading({ children }: { children: React.ReactNode }) {
  return (
    <h3 className="text-xs font-semibold uppercase tracking-wide text-gray-500">
      {children}
    </h3>
  );
}

export const EditUserModal: React.FC<EditUserModalProps> = ({
  isOpen,
  onClose,
  onSuccess,
  userId,
  currentUser,
  canEditZdr = false,
  supportOnly = false,
  canEditServing = false,
}) => {
  const [formData, setFormData] = useState({
    display_name: currentUser.name,
    avatar_url: currentUser.avatar || "",
    roles: currentUser.roles,
    zero_data_retention: currentUser.zero_data_retention,
    self_hosted_only: currentUser.self_hosted_only ?? false,
    // null/absent both mean "Default" (no preset pinned).
    toleration_preset: currentUser.toleration_preset ?? null,
  });
  const [error, setError] = useState<string | null>(null);

  const updateUserMutation = useUpdateUser();
  const { data: config } = useConfig();

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);

    try {
      await updateUserMutation.mutateAsync({
        id: userId,
        data: supportOnly ? { zero_data_retention: true } : {
          display_name: formData.display_name.trim() || undefined,
          avatar_url: formData.avatar_url.trim() || undefined,
          roles: formData.roles,
          // ZDR is admin-only server-side; only send it when the viewer can
          // edit it, otherwise dwctl rejects the whole request with a 403.
          ...(canEditZdr
            ? { zero_data_retention: formData.zero_data_retention }
            : {}),
          // Serving settings are platform-manager only server-side.
          ...(canEditServing
            ? {
                self_hosted_only: formData.self_hosted_only,
                // Scheduling tolerations are platform-manager only too, and
                // share the serving-settings gate server-side.
                toleration_preset: formData.toleration_preset,
              }
            : {}),
        },
      });

      onSuccess();
      onClose();
    } catch (err) {
      console.error("Failed to update user:", err);
      setError(err instanceof Error ? err.message : "Failed to update user");
    }
  };

  const handleRoleChange = (role: Role) => {
    if (role === "StandardUser") return; // Cannot change StandardUser
    setFormData((prev) => ({
      ...prev,
      roles: prev.roles.includes(role)
        ? prev.roles.filter((r) => r !== role)
        : [...prev.roles, role],
    }));
  };

  const getRoleDescription = (role: Role): string => {
    const descriptions: Record<Role, string> = {
      StandardUser:
        "Standard Users can access models, create API keys, use the playground, and manage their profile.",
      PlatformManager:
        "Platform Managers can control access to models, create new users, change permissions for existing users, manage inference endpoints, and configure system settings.",
      RequestViewer:
        "Request Viewers can view a full log of all requests that have transited the gateway.",
      BillingManager:
        "Billing Managers can view and manage cost information and user credit balances.",
      BatchAPIUser:
        "Batch API Users can upload, view, and delete their own files for use with the Batch API.",
      ConnectionsUser:
        "Connections Users can create and manage external data source connections, sync files, and view resulting batches.",
    };
    return descriptions[role];
  };

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      {/* Header and footer stay put; only the form body scrolls, so the dialog
          never grows past the viewport however many settings it shows. */}
      <DialogContent className="sm:max-w-2xl max-h-[90vh] flex flex-col gap-0 p-0 overflow-hidden">
        <DialogHeader className="px-6 pt-6 pb-4 border-b">
          <div className="flex items-center gap-3">
            <div className="w-10 h-10 bg-blue-100 rounded-full flex items-center justify-center shrink-0">
              <User className="w-5 h-5 text-blue-600" />
            </div>
            <div className="min-w-0">
              <DialogTitle>Edit User</DialogTitle>
              <DialogDescription className="truncate">
                <span>{currentUser.username}</span>
                <span aria-hidden="true"> · </span>
                <span>{currentUser.email}</span>
              </DialogDescription>
            </div>
          </div>
        </DialogHeader>

        <form
          id="edit-user-form"
          onSubmit={handleSubmit}
          className="flex-1 min-h-0 overflow-y-auto px-6 py-4 space-y-6"
          data-testid="edit-user-form-body"
        >
          <AlertBox variant="error">{error}</AlertBox>

          <section className="space-y-3" hidden={supportOnly}>
            <SectionHeading>Profile</SectionHeading>
            <div className="grid gap-4 sm:grid-cols-2">
              <div>
                <label
                  htmlFor="display_name"
                  className="block text-sm font-medium text-gray-700 mb-1"
                >
                  Display Name
                </label>
                <input
                  type="text"
                  id="display_name"
                  value={formData.display_name}
                  onChange={(e) =>
                    setFormData((prev) => ({
                      ...prev,
                      display_name: e.target.value,
                    }))
                  }
                  className={inputClass}
                  placeholder="Enter display name"
                />
              </div>
              <div>
                <label
                  htmlFor="avatar_url"
                  className="block text-sm font-medium text-gray-700 mb-1"
                >
                  Avatar URL
                </label>
                <input
                  type="url"
                  id="avatar_url"
                  value={formData.avatar_url}
                  onChange={(e) =>
                    setFormData((prev) => ({
                      ...prev,
                      avatar_url: e.target.value,
                    }))
                  }
                  className={inputClass}
                  placeholder="https://example.com/avatar.jpg"
                />
              </div>
            </div>
            <p className="text-xs text-gray-500">
              Email and username cannot be changed.
            </p>
          </section>

          <section className="space-y-3" hidden={supportOnly}>
            <SectionHeading>Roles</SectionHeading>
            <div className="grid gap-x-6 gap-y-2 sm:grid-cols-2">
              {AVAILABLE_ROLES.map((role) => {
                const isPlatformManagerSelected =
                  formData.roles.includes("PlatformManager");
                const isSubsetRole = SUBSET_ROLES.includes(role);
                const isDisabled =
                  role === "StandardUser" ||
                  (isPlatformManagerSelected && isSubsetRole);

                return (
                  <label key={role} className="flex items-center">
                    <input
                      type="checkbox"
                      value={role}
                      checked={
                        role === "StandardUser" ||
                        formData.roles.includes(role) ||
                        (isPlatformManagerSelected && isSubsetRole)
                      }
                      onChange={() => handleRoleChange(role)}
                      disabled={isDisabled}
                      className={`border-gray-300 text-blue-600 focus:ring-blue-500 rounded ${
                        isDisabled ? "opacity-50 cursor-not-allowed" : ""
                      }`}
                    />
                    <div className="ml-2 text-sm flex-1 flex items-center gap-1">
                      <span
                        className={
                          isDisabled ? "text-gray-500" : "text-gray-700"
                        }
                      >
                        {getRoleDisplayName(role)}
                        {isPlatformManagerSelected && isSubsetRole && (
                          <span className="text-gray-400 text-xs ml-1">
                            (included in Platform Manager)
                          </span>
                        )}
                      </span>
                      <HoverCard openDelay={150} closeDelay={200}>
                        <HoverCardTrigger asChild>
                          <Info className="w-3 h-3 text-gray-400 cursor-pointer" />
                        </HoverCardTrigger>
                        <HoverCardContent side="top" align="end">
                          <p className="text-sm">{getRoleDescription(role)}</p>
                        </HoverCardContent>
                      </HoverCard>
                    </div>
                  </label>
                );
              })}
            </div>
          </section>

          {canEditZdr && (
            <section className="flex items-start justify-between gap-4 border-t pt-4">
              <div className="flex items-center gap-1">
                <label
                  htmlFor="zero_data_retention"
                  className="text-sm font-medium text-gray-700"
                >
                  Zero Data Retention
                </label>
                <HoverCard openDelay={150} closeDelay={200}>
                  <HoverCardTrigger asChild>
                    <Info className="w-3 h-3 text-gray-400 cursor-pointer" />
                  </HoverCardTrigger>
                  <HoverCardContent side="top" align="end">
                    <p className="text-sm">
                      When enabled, this account is flagged for zero data
                      retention. Applies to every API key owned by the account.
                    </p>
                  </HoverCardContent>
                </HoverCard>
              </div>
              <Switch
                id="zero_data_retention"
                disabled={supportOnly && currentUser.zero_data_retention}
                checked={formData.zero_data_retention}
                onCheckedChange={(checked) =>
                  setFormData((prev) => ({
                    ...prev,
                    zero_data_retention: checked,
                  }))
                }
                aria-label="Toggle zero data retention"
              />
            </section>
          )}

          {canEditServing && (
            <section className="space-y-4 border-t pt-4">
              <SectionHeading>Serving (platform managers only)</SectionHeading>
              <div className="flex items-center justify-between gap-4">
                <div className="flex items-center gap-1">
                  <label
                    htmlFor="self_hosted_only"
                    className="text-sm text-gray-700"
                  >
                    No external-provider fallback (gateway)
                  </label>
                  <HoverCard openDelay={150} closeDelay={200}>
                    <HoverCardTrigger asChild>
                      <Info className="w-3 h-3 text-gray-400 cursor-pointer" />
                    </HoverCardTrigger>
                    <HoverCardContent side="top" align="end">
                      <p className="text-sm">
                        When on, this account's requests will never fail over to
                        a third-party (external) provider: the gateway may only
                        route them to self-hosted endpoints. It does not change
                        where the inference backend places a request — that is
                        the scheduling-tolerations setting below.
                      </p>
                    </HoverCardContent>
                  </HoverCard>
                </div>
                <Switch
                  id="self_hosted_only"
                  checked={formData.self_hosted_only}
                  onCheckedChange={(checked) =>
                    setFormData((prev) => ({
                      ...prev,
                      self_hosted_only: checked,
                    }))
                  }
                  aria-label="Toggle no external-provider fallback"
                />
              </div>
              <SchedulingTolerations
                preset={formData.toleration_preset}
                presets={config?.scheduling_toleration_presets}
                canEdit={canEditServing}
                onChange={(next) =>
                  setFormData((prev) => ({
                    ...prev,
                    toleration_preset: next,
                  }))
                }
              />
            </section>
          )}
        </form>

        <DialogFooter className="px-6 py-4 border-t">
          <Button
            type="button"
            variant="outline"
            onClick={onClose}
            disabled={updateUserMutation.isPending}
          >
            Cancel
          </Button>
          <Button
            type="submit"
            form="edit-user-form"
            disabled={updateUserMutation.isPending || (supportOnly && (!formData.zero_data_retention || currentUser.zero_data_retention))}
          >
            {updateUserMutation.isPending ? (
              <>
                <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-white"></div>
                Saving...
              </>
            ) : (
              "Save Changes"
            )}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
