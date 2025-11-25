import React, { useState } from "react";
import { User, Info } from "lucide-react";
import { useUpdateUser } from "../../../api/control-layer";
import type { Role } from "../../../api/control-layer/types";
import { AVAILABLE_ROLES, getRoleDisplayName } from "../../../utils/roles";
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
  };
}

export const EditUserModal: React.FC<EditUserModalProps> = ({
  isOpen,
  onClose,
  onSuccess,
  userId,
  currentUser,
}) => {
  const [formData, setFormData] = useState({
    display_name: currentUser.name,
    avatar_url: currentUser.avatar || "",
    roles: currentUser.roles,
  });
  const [error, setError] = useState<string | null>(null);

  const updateUserMutation = useUpdateUser();

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);

    try {
      await updateUserMutation.mutateAsync({
        id: userId,
        data: {
          display_name: formData.display_name.trim() || undefined,
          avatar_url: formData.avatar_url.trim() || undefined,
          roles: formData.roles,
        },
      });

      console.log("User updated successfully:", { userId, formData });
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
    };
    return descriptions[role];
  };

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <div className="flex items-center gap-3">
            <div className="w-10 h-10 bg-blue-100 rounded-full flex items-center justify-center">
              <User className="w-5 h-5 text-blue-600" />
            </div>
            <div>
              <DialogTitle>Edit User</DialogTitle>
              <DialogDescription>{currentUser.username}</DialogDescription>
            </div>
          </div>
        </DialogHeader>

        <AlertBox variant="error" className="mb-4">
          {error}
        </AlertBox>

        <form id="edit-user-form" onSubmit={handleSubmit} className="space-y-4">
          <div className="space-y-4">
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
                className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-blue-500 focus:border-transparent"
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
                className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-blue-500 focus:border-transparent"
                placeholder="https://example.com/avatar.jpg"
              />
            </div>

            <div>
              <label className="block text-sm font-medium text-gray-700 mb-2">
                Roles
              </label>
              <div className="space-y-2">
                {AVAILABLE_ROLES.map((role) => {
                  const isPlatformManagerSelected =
                    formData.roles.includes("PlatformManager");
                  const isSubsetRole =
                    role === "BatchAPIUser" || role === "BillingManager";
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
                            <p className="text-sm">
                              {getRoleDescription(role)}
                            </p>
                          </HoverCardContent>
                        </HoverCard>
                      </div>
                    </label>
                  );
                })}
              </div>
            </div>

            <div className="bg-gray-50 rounded-lg p-3">
              <p className="text-sm text-gray-600">
                <strong>Email:</strong> {currentUser.email} (cannot be changed)
              </p>
              <p className="text-sm text-gray-600 mt-1">
                <strong>Username:</strong> {currentUser.username} (cannot be
                changed)
              </p>
            </div>
          </div>
        </form>

        <DialogFooter>
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
            disabled={updateUserMutation.isPending}
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
