import React, { useState } from "react";
import { Info } from "lucide-react";
import { useCreateUser } from "../../../api/control-layer";
import type { Role } from "../../../api/control-layer/types";
import { AVAILABLE_ROLES, getRoleDisplayName } from "../../../utils/roles";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
  DialogDescription,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
import { AlertBox } from "../../ui/alert-box";
import { HoverCard, HoverCardContent, HoverCardTrigger } from "../../ui";

interface CreateUserModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess: () => void;
}

export const CreateUserModal: React.FC<CreateUserModalProps> = ({
  isOpen,
  onClose,
  onSuccess,
}) => {
  const [formData, setFormData] = useState({
    username: "",
    email: "",
    display_name: "",
    avatar_url: "",
    roles: ["StandardUser"] as Role[],
  });
  const [error, setError] = useState<string | null>(null);

  const createUserMutation = useCreateUser();

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!formData.username.trim() || !formData.email.trim()) {
      setError("Username and email are required");
      return;
    }

    // Basic email validation
    const emailRegex = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;
    if (!emailRegex.test(formData.email)) {
      setError("Please enter a valid email address");
      return;
    }

    setError(null);

    try {
      await createUserMutation.mutateAsync({
        username: formData.username.trim(),
        email: formData.email.trim(),
        display_name: formData.display_name.trim() || undefined,
        avatar_url: formData.avatar_url.trim() || undefined,
        roles: formData.roles,
      });

      // Reset form
      setFormData({
        username: "",
        email: "",
        display_name: "",
        avatar_url: "",
        roles: ["StandardUser"],
      });
      onSuccess();
      onClose();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to create user");
    }
  };

  const handleClose = () => {
    if (!createUserMutation.isPending) {
      setFormData({
        username: "",
        email: "",
        display_name: "",
        avatar_url: "",
        roles: ["StandardUser"],
      });
      setError(null);
      onClose();
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
    <Dialog open={isOpen} onOpenChange={(open) => !open && handleClose()}>
      <DialogContent className="sm:max-w-md max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>Create New User</DialogTitle>
          <DialogDescription>
            Create a new user with email, metadata, and role.
          </DialogDescription>
        </DialogHeader>

        <AlertBox variant="error" className="mb-4">
          {error}
        </AlertBox>

        <form
          id="create-user-form"
          onSubmit={handleSubmit}
          className="space-y-4"
        >
          <div className="mb-4">
            <label
              htmlFor="username"
              className="block text-sm font-medium text-gray-700 mb-2"
            >
              Username *
            </label>
            <input
              type="text"
              id="username"
              value={formData.username}
              onChange={(e) =>
                setFormData({ ...formData, username: e.target.value })
              }
              className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-blue-500 focus:border-transparent"
              placeholder="Enter username"
              disabled={createUserMutation.isPending}
              required
            />
          </div>

          <div className="mb-4">
            <label
              htmlFor="email"
              className="block text-sm font-medium text-gray-700 mb-2"
            >
              Email *
            </label>
            <input
              type="email"
              id="email"
              value={formData.email}
              onChange={(e) =>
                setFormData({ ...formData, email: e.target.value })
              }
              className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-blue-500 focus:border-transparent"
              placeholder="Enter email address"
              disabled={createUserMutation.isPending}
              required
            />
          </div>

          <div className="mb-4">
            <label
              htmlFor="display_name"
              className="block text-sm font-medium text-gray-700 mb-2"
            >
              Display Name
            </label>
            <input
              type="text"
              id="display_name"
              value={formData.display_name}
              onChange={(e) =>
                setFormData({ ...formData, display_name: e.target.value })
              }
              className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-blue-500 focus:border-transparent"
              placeholder="Enter display name (optional)"
              disabled={createUserMutation.isPending}
            />
          </div>

          <div className="mb-4">
            <label
              htmlFor="avatar_url"
              className="block text-sm font-medium text-gray-700 mb-2"
            >
              Avatar URL
            </label>
            <input
              type="url"
              id="avatar_url"
              value={formData.avatar_url}
              onChange={(e) =>
                setFormData({ ...formData, avatar_url: e.target.value })
              }
              className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-blue-500 focus:border-transparent"
              placeholder="Enter avatar URL (optional)"
              disabled={createUserMutation.isPending}
            />
          </div>

          <div className="mb-6">
            <label className="block text-sm font-medium text-gray-700 mb-2">
              Roles *
            </label>
            <div className="space-y-2">
              {AVAILABLE_ROLES.map((role) => {
                const isPlatformManagerSelected =
                  formData.roles.includes("PlatformManager");
                const isSubsetRole =
                  role === "BatchAPIUser" || role === "BillingManager";
                const isDisabled =
                  role === "StandardUser" ||
                  createUserMutation.isPending ||
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
          </div>
        </form>

        <DialogFooter>
          <Button
            type="button"
            variant="outline"
            onClick={handleClose}
            disabled={createUserMutation.isPending}
          >
            Cancel
          </Button>
          <Button
            type="submit"
            form="create-user-form"
            disabled={
              createUserMutation.isPending ||
              !formData.username.trim() ||
              !formData.email.trim() ||
              formData.roles.length === 0
            }
          >
            {createUserMutation.isPending ? "Creating..." : "Create User"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
