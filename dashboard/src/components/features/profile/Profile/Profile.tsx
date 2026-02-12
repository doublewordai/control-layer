import React, { useState, useEffect } from "react";
import {
  Save,
  Loader2,
  Calendar,
  Shield,
  AtSign,
  Info,
  Eye,
  EyeOff,
  Lock,
  Plus,
  Pencil,
  Trash2,
  RotateCw,
  TestTube,
  AlertTriangle,
  Copy,
  Check,
  Globe,
  Mail,
} from "lucide-react";
import {
  useUser,
  useUpdateUser,
  useWebhooks,
  useCreateWebhook,
  useUpdateWebhook,
  useDeleteWebhook,
  useRotateWebhookSecret,
  useTestWebhook,
} from "../../../../api/control-layer/hooks";
import {
  UserAvatar,
  Tooltip,
  TooltipContent,
  TooltipTrigger,
  HoverCard,
  HoverCardContent,
  HoverCardTrigger,
} from "../../../ui";
import { Input } from "../../../ui/input";
import { Button } from "../../../ui/button";
import { Switch } from "../../../ui/switch";
import { Label } from "../../../ui/label";
import { Badge } from "../../../ui/badge";
import { Checkbox } from "../../../ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from "../../../ui/dialog";
import { AVAILABLE_ROLES, getRoleDisplayName } from "../../../../utils/roles";
import type {
  Role,
  Webhook,
  WebhookTestResponse,
} from "../../../../api/control-layer/types";
import { dwctlApi } from "../../../../api/control-layer/client";
import { ApiError } from "../../../../api/control-layer/errors";

const EVENT_TYPE_OPTIONS = [
  {
    value: "batch.completed",
    label: "Batch completed",
    example: `{
  "type": "batch.completed",
  "batch_id": "batch_abc123",
  "status": "completed",
  "total_requests": 150,
  "completed": 150,
  "failed": 0
}`,
  },
  {
    value: "batch.failed",
    label: "Batch failed",
    example: `{
  "type": "batch.failed",
  "batch_id": "batch_abc123",
  "status": "failed",
  "total_requests": 150,
  "completed": 80,
  "failed": 70
}`,
  },
];

export const Profile: React.FC = () => {
  const {
    data: currentUser,
    isLoading: loading,
    error: userError,
    refetch: refetchUser,
  } = useUser("current");
  const updateUserMutation = useUpdateUser();
  const [displayName, setDisplayName] = useState("");
  const [avatarUrl, setAvatarUrl] = useState("");
  const [roles, setRoles] = useState<Role[]>([]);
  const [currentPassword, setCurrentPassword] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [confirmPassword, setConfirmPassword] = useState("");
  const [showCurrentPassword, setShowCurrentPassword] = useState(false);
  const [showNewPassword, setShowNewPassword] = useState(false);
  const [showConfirmPassword, setShowConfirmPassword] = useState(false);
  const [error, setError] = useState("");
  const [success, setSuccess] = useState("");

  // Webhook state
  const { data: webhooks } = useWebhooks();
  const createWebhookMutation = useCreateWebhook();
  const updateWebhookMutation = useUpdateWebhook();
  const deleteWebhookMutation = useDeleteWebhook();
  const rotateSecretMutation = useRotateWebhookSecret();
  const testWebhookMutation = useTestWebhook();

  const [webhookDialogOpen, setWebhookDialogOpen] = useState(false);
  const [editingWebhook, setEditingWebhook] = useState<Webhook | null>(null);
  const [webhookUrl, setWebhookUrl] = useState("");
  const [webhookDescription, setWebhookDescription] = useState("");
  const [webhookEventTypes, setWebhookEventTypes] = useState<string[]>([]);
  const [webhookSecret, setWebhookSecret] = useState<string | null>(null);
  const [webhookError, setWebhookError] = useState("");
  const [deletingWebhookId, setDeletingWebhookId] = useState<string | null>(
    null,
  );
  const [testResults, setTestResults] = useState<
    Record<string, WebhookTestResponse>
  >({});
  const [copiedSecret, setCopiedSecret] = useState(false);
  const [emailNotifSaved, setEmailNotifSaved] = useState(false);

  const handleRoleChange = (role: Role) => {
    setRoles((prev) =>
      prev.includes(role) ? prev.filter((r) => r !== role) : [...prev, role],
    );
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

  useEffect(() => {
    if (currentUser) {
      setDisplayName(currentUser.display_name || "");
      setAvatarUrl(currentUser.avatar_url || "");
      setRoles(currentUser.roles || []);
    }
    if (userError) {
      setError("Failed to load profile information");
    }
  }, [currentUser, userError]);

  const handleSave = async () => {
    if (!currentUser) return;

    setError("");
    setSuccess("");

    try {
      // Validate password fields if any are filled
      const isChangingPassword =
        currentPassword || newPassword || confirmPassword;

      if (isChangingPassword) {
        // Validate all password fields are filled
        if (!currentPassword || !newPassword || !confirmPassword) {
          setError("All password fields are required to change your password");
          return;
        }

        // Validate passwords match
        if (newPassword !== confirmPassword) {
          setError("New passwords do not match");
          return;
        }

        // Validate password length
        if (newPassword.length < 8) {
          setError("New password must be at least 8 characters long");
          return;
        }

        // Validate passwords are different
        if (currentPassword === newPassword) {
          setError("New password must be different from current password");
          return;
        }
      }

      // Update profile information
      const updateData = {
        display_name: displayName.trim() || undefined,
        avatar_url: avatarUrl.trim() || undefined,
        roles: currentUser.is_admin
          ? ([...new Set([...roles, "StandardUser"])] as Role[])
          : undefined,
      };

      await updateUserMutation.mutateAsync({
        id: currentUser.id,
        data: updateData,
      });

      // Change password if requested
      if (isChangingPassword) {
        try {
          await dwctlApi.auth.changePassword({
            current_password: currentPassword,
            new_password: newPassword,
          });

          // Clear password fields on success
          setCurrentPassword("");
          setNewPassword("");
          setConfirmPassword("");
          setSuccess("Profile and password updated successfully!");
        } catch (passwordErr) {
          if (passwordErr instanceof ApiError) {
            try {
              const errorData = JSON.parse(passwordErr.message);
              setError(errorData.message || "Failed to change password");
            } catch {
              setError(passwordErr.message || "Failed to change password");
            }
          } else {
            setError("Failed to change password. Please try again.");
          }
          console.error("Password change error:", passwordErr);
          return;
        }
      } else {
        setSuccess("Profile updated successfully!");
      }

      // Refetch user data to get the updated information
      await refetchUser();
    } catch (err) {
      setError("Failed to update profile. Please try again.");
      console.error("Failed to update profile:", err);
    }
  };

  const handleEmailNotificationsToggle = async (enabled: boolean) => {
    if (!currentUser) return;
    try {
      await updateUserMutation.mutateAsync({
        id: currentUser.id,
        data: { batch_notifications_enabled: enabled },
      });
      await refetchUser();
      setEmailNotifSaved(true);
      setTimeout(() => setEmailNotifSaved(false), 2000);
    } catch {
      // Revert will happen via refetchUser
    }
  };

  // Webhook handlers
  const openCreateWebhookDialog = () => {
    setEditingWebhook(null);
    setWebhookUrl("");
    setWebhookDescription("");
    setWebhookEventTypes(EVENT_TYPE_OPTIONS.map((o) => o.value));
    setWebhookSecret(null);
    setWebhookError("");
    setWebhookDialogOpen(true);
  };

  const openEditWebhookDialog = (webhook: Webhook) => {
    setEditingWebhook(webhook);
    setWebhookUrl(webhook.url);
    setWebhookDescription(webhook.description || "");
    setWebhookEventTypes(
      webhook.event_types && webhook.event_types.length > 0
        ? webhook.event_types
        : EVENT_TYPE_OPTIONS.map((o) => o.value),
    );
    setWebhookSecret(null);
    setWebhookError("");
    setWebhookDialogOpen(true);
  };

  const handleWebhookEventTypeToggle = (eventType: string) => {
    setWebhookEventTypes((prev) =>
      prev.includes(eventType)
        ? prev.filter((t) => t !== eventType)
        : [...prev, eventType],
    );
  };

  const handleWebhookSave = async () => {
    setWebhookError("");

    if (!webhookUrl.trim()) {
      setWebhookError("URL is required");
      return;
    }

    try {
      new URL(webhookUrl);
    } catch {
      setWebhookError("Please enter a valid URL");
      return;
    }

    if (
      !webhookUrl.startsWith("https://") &&
      !webhookUrl.startsWith("http://localhost")
    ) {
      setWebhookError("URL must use HTTPS (except for localhost)");
      return;
    }

    try {
      if (editingWebhook) {
        await updateWebhookMutation.mutateAsync({
          webhookId: editingWebhook.id,
          data: {
            url: webhookUrl,
            description: webhookDescription.trim() || null,
            event_types: webhookEventTypes,
          },
        });
        setWebhookDialogOpen(false);
      } else {
        const result = await createWebhookMutation.mutateAsync({
          data: {
            url: webhookUrl,
            description: webhookDescription.trim() || undefined,
            event_types: webhookEventTypes,
          },
        });
        setWebhookSecret(result.secret);
      }
    } catch {
      setWebhookError(
        editingWebhook
          ? "Failed to update webhook"
          : "Failed to create webhook",
      );
    }
  };

  const handleWebhookDelete = async (webhookId: string) => {
    try {
      await deleteWebhookMutation.mutateAsync({ webhookId });
      setDeletingWebhookId(null);
    } catch {
      // Error handled by mutation
    }
  };

  const handleWebhookToggle = async (webhook: Webhook) => {
    try {
      await updateWebhookMutation.mutateAsync({
        webhookId: webhook.id,
        data: { enabled: !webhook.enabled },
      });
    } catch {
      // Error handled by mutation
    }
  };

  const handleRotateSecret = async (webhookId: string) => {
    try {
      const result = await rotateSecretMutation.mutateAsync({ webhookId });
      setWebhookSecret(result.secret);
    } catch {
      // Error handled by mutation
    }
  };

  const handleTestWebhook = async (webhookId: string) => {
    try {
      const result = await testWebhookMutation.mutateAsync({ webhookId });
      setTestResults((prev) => ({ ...prev, [webhookId]: result }));
      // Clear test result after 5 seconds
      setTimeout(() => {
        setTestResults((prev) => {
          const next = { ...prev };
          delete next[webhookId];
          return next;
        });
      }, 5000);
    } catch {
      setTestResults((prev) => ({
        ...prev,
        [webhookId]: {
          success: false,
          error: "Failed to send test",
          duration_ms: 0,
        },
      }));
      setTimeout(() => {
        setTestResults((prev) => {
          const next = { ...prev };
          delete next[webhookId];
          return next;
        });
      }, 5000);
    }
  };

  const handleCopySecret = async (secret: string) => {
    await navigator.clipboard.writeText(secret);
    setCopiedSecret(true);
    setTimeout(() => setCopiedSecret(false), 2000);
  };

  const formatDate = (dateString: string) => {
    return new Date(dateString).toLocaleDateString("en-US", {
      year: "numeric",
      month: "long",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
    });
  };

  if (loading) {
    return (
      <div className="p-6">
        <div className="max-w-5xl mx-auto">
          <div className="animate-pulse">
            <div className="h-8 bg-gray-200 rounded w-48 mb-8"></div>
            <div className="bg-white rounded-lg shadow-sm border border-gray-200 p-6">
              <div className="space-y-4">
                <div className="h-20 bg-gray-200 rounded-full w-20 mx-auto"></div>
                <div className="h-4 bg-gray-200 rounded w-32 mx-auto"></div>
                <div className="h-4 bg-gray-200 rounded w-48 mx-auto"></div>
              </div>
            </div>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="p-6">
      <div className="max-w-5xl mx-auto">
        <div className="mb-8">
          <h1 className="text-3xl font-bold text-gray-900 mb-2">
            Profile Settings
          </h1>
          <p className="text-gray-600">
            Manage your account information and preferences
          </p>
        </div>

        {error && (
          <div className="mb-6 bg-red-50 border border-red-200 text-red-700 px-4 py-3 rounded-lg">
            {error}
          </div>
        )}

        {success && (
          <div className="mb-6 bg-green-50 border border-green-200 text-green-700 px-4 py-3 rounded-lg">
            {success}
          </div>
        )}

        <div className="grid grid-cols-1 lg:grid-cols-4 gap-6">
          {/* Profile Picture and Basic Info */}
          <div className="lg:col-span-1">
            <div className="bg-white rounded-lg shadow-sm border border-gray-200 p-6">
              <div className="text-center">
                {currentUser && (
                  <UserAvatar
                    user={currentUser}
                    size="lg"
                    className="w-24 h-24 mx-auto mb-4"
                  />
                )}
                <Tooltip>
                  <TooltipTrigger asChild>
                    <h3 className="text-lg font-medium text-gray-900 truncate px-2 cursor-default">
                      {displayName ||
                        currentUser?.display_name ||
                        currentUser?.username ||
                        "Unknown User"}
                    </h3>
                  </TooltipTrigger>
                  <TooltipContent>
                    <p>Display name: {currentUser?.display_name || "Not set"}</p>
                  </TooltipContent>
                </Tooltip>
                <Tooltip>
                  <TooltipTrigger asChild>
                    <p className="text-sm text-gray-500 truncate px-2 cursor-default">
                      {currentUser?.username}
                    </p>
                  </TooltipTrigger>
                  <TooltipContent>
                    <p>Username: {currentUser?.username}</p>
                  </TooltipContent>
                </Tooltip>
              </div>
            </div>

            {/* Account Details */}
            <div className="bg-white rounded-lg shadow-sm border border-gray-200 p-6 mt-6">
              <h4 className="text-lg font-medium text-gray-900 mb-4">
                Account Details
              </h4>
              <div className="space-y-3">
                <div className="flex items-center text-sm">
                  <AtSign className="w-4 h-4 text-gray-400 mr-2 shrink-0" />
                  <span className="text-gray-600 w-20 shrink-0">Email:</span>
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <span className="text-gray-900 truncate">
                        {currentUser?.email}
                      </span>
                    </TooltipTrigger>
                    <TooltipContent>
                      <p>{currentUser?.email || ""}</p>
                    </TooltipContent>
                  </Tooltip>
                </div>
                {/*<div className="flex items-center text-sm">
                  <User className="w-4 h-4 text-gray-400 mr-2 flex-shrink-0" />
                  <span className="text-gray-600 w-20 flex-shrink-0">
                    Username:
                  </span>
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <span className="text-gray-900 truncate">
                        {currentUser?.username}
                      </span>
                    </TooltipTrigger>
                    <TooltipContent>
                      <p>{currentUser?.username || ""}</p>
                    </TooltipContent>
                  </Tooltip>
                </div>*/}
                {currentUser?.created_at && (
                  <div className="flex items-center text-sm">
                    <Calendar className="w-4 h-4 text-gray-400 mr-2 shrink-0" />
                    <span className="text-gray-600 w-20 shrink-0">Joined:</span>
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <span className="text-gray-900 truncate">
                          {formatDate(currentUser.created_at)}
                        </span>
                      </TooltipTrigger>
                      <TooltipContent>
                        <p>{formatDate(currentUser.created_at)}</p>
                      </TooltipContent>
                    </Tooltip>
                  </div>
                )}
                <div className="flex items-center text-sm">
                  <Shield className="w-4 h-4 text-gray-400 mr-2 shrink-0" />
                  <span className="text-gray-600 w-20 shrink-0">Type:</span>
                  <span className="text-gray-900">
                    {currentUser?.is_admin ? "Admin" : "User"}
                  </span>
                </div>
              </div>
            </div>

            {/* Save Button */}
            <Button
              onClick={handleSave}
              disabled={updateUserMutation.isPending}
              className="w-full mt-6"
            >
              {updateUserMutation.isPending ? (
                <Loader2 className="w-4 h-4 animate-spin" />
              ) : (
                <Save className="w-4 h-4" />
              )}
              Save Changes
            </Button>
          </div>

          {/* Editable Profile Information */}
          <div className="lg:col-span-3">
            <div className="bg-white rounded-lg shadow-sm border border-gray-200 p-6">
              <h4 className="text-lg font-medium text-gray-900 mb-3">
                Edit Profile
              </h4>

              <div className="space-y-3">
                <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
                  <div>
                    <label
                      htmlFor="displayName"
                      className="block text-sm font-medium text-gray-700 mb-1.5"
                    >
                      Display Name
                    </label>
                    <Input
                      id="displayName"
                      type="text"
                      value={displayName}
                      onChange={(e) => setDisplayName(e.target.value)}
                      placeholder="Enter your display name"
                    />
                    <p className="text-xs text-gray-500 mt-1">
                      This is how your name will appear to other users
                    </p>
                  </div>

                  <div>
                    <label
                      htmlFor="avatarUrl"
                      className="block text-sm font-medium text-gray-700 mb-1.5"
                    >
                      Avatar URL
                    </label>
                    <Input
                      id="avatarUrl"
                      type="url"
                      autoComplete="off"
                      value={avatarUrl}
                      onChange={(e) => setAvatarUrl(e.target.value)}
                      placeholder="https://example.com/avatar.jpg"
                    />
                    <p className="text-xs text-gray-500 mt-1">
                      Enter a URL to your profile picture
                    </p>
                  </div>
                </div>

                {/* Password Change Fields - Only for password-based auth */}
                {(currentUser?.auth_source === "native" ||
                  currentUser?.auth_source === "system") && (
                  <>
                    <div className="pt-3 border-t border-gray-200">
                      <h5 className="text-base font-medium text-gray-900 mb-2">
                        Change Password
                      </h5>
                    </div>

                    <div>
                      <label
                        htmlFor="currentPassword"
                        className="block text-sm font-medium text-gray-700 mb-1.5"
                      >
                        Current Password
                      </label>
                      <div className="relative">
                        <div className="absolute inset-y-0 left-0 pl-3 flex items-center pointer-events-none">
                          <Lock className="h-4 w-4 text-gray-400" />
                        </div>
                        <Input
                          id="currentPassword"
                          type={showCurrentPassword ? "text" : "password"}
                          value={currentPassword}
                          autoComplete="new-password"
                          onChange={(e) => setCurrentPassword(e.target.value)}
                          className="pl-10 pr-10"
                          placeholder="Enter your current password"
                        />
                        <button
                          type="button"
                          onClick={() =>
                            setShowCurrentPassword(!showCurrentPassword)
                          }
                          className="absolute inset-y-0 right-0 pr-3 flex items-center"
                          tabIndex={-1}
                        >
                          {showCurrentPassword ? (
                            <EyeOff className="h-4 w-4 text-gray-400 hover:text-gray-600" />
                          ) : (
                            <Eye className="h-4 w-4 text-gray-400 hover:text-gray-600" />
                          )}
                        </button>
                      </div>
                    </div>

                    <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
                      <div>
                        <label
                          htmlFor="newPassword"
                          className="block text-sm font-medium text-gray-700 mb-1.5"
                        >
                          New Password
                        </label>
                        <div className="relative">
                          <div className="absolute inset-y-0 left-0 pl-3 flex items-center pointer-events-none">
                            <Lock className="h-4 w-4 text-gray-400" />
                          </div>
                          <Input
                            id="newPassword"
                            type={showNewPassword ? "text" : "password"}
                            value={newPassword}
                            onChange={(e) => setNewPassword(e.target.value)}
                            className="pl-10 pr-10"
                            placeholder="Enter new password"
                          />
                          <button
                            type="button"
                            onClick={() => setShowNewPassword(!showNewPassword)}
                            className="absolute inset-y-0 right-0 pr-3 flex items-center"
                            tabIndex={-1}
                          >
                            {showNewPassword ? (
                              <EyeOff className="h-4 w-4 text-gray-400 hover:text-gray-600" />
                            ) : (
                              <Eye className="h-4 w-4 text-gray-400 hover:text-gray-600" />
                            )}
                          </button>
                        </div>
                        <p className="text-xs text-gray-500 mt-1">
                          At least 8 characters
                        </p>
                      </div>

                      <div>
                        <label
                          htmlFor="confirmPassword"
                          className="block text-sm font-medium text-gray-700 mb-1.5"
                        >
                          Confirm New Password
                        </label>
                        <div className="relative">
                          <div className="absolute inset-y-0 left-0 pl-3 flex items-center pointer-events-none">
                            <Lock className="h-4 w-4 text-gray-400" />
                          </div>
                          <Input
                            id="confirmPassword"
                            type={showConfirmPassword ? "text" : "password"}
                            value={confirmPassword}
                            onChange={(e) => setConfirmPassword(e.target.value)}
                            className="pl-10 pr-10"
                            placeholder="Confirm new password"
                          />
                          <button
                            type="button"
                            onClick={() =>
                              setShowConfirmPassword(!showConfirmPassword)
                            }
                            className="absolute inset-y-0 right-0 pr-3 flex items-center"
                            tabIndex={-1}
                          >
                            {showConfirmPassword ? (
                              <EyeOff className="h-4 w-4 text-gray-400 hover:text-gray-600" />
                            ) : (
                              <Eye className="h-4 w-4 text-gray-400 hover:text-gray-600" />
                            )}
                          </button>
                        </div>
                      </div>
                    </div>
                  </>
                )}

                {currentUser?.is_admin ? (
                  <div>
                    <div className="flex items-center gap-2 mb-2">
                      <label className="text-sm font-medium text-gray-700">
                        Roles
                      </label>
                      <HoverCard openDelay={150} closeDelay={200}>
                        <HoverCardTrigger asChild>
                          <Info className="w-3 h-3 text-gray-400 cursor-pointer" />
                        </HoverCardTrigger>
                        <HoverCardContent side="top" align="start">
                          <p className="text-sm">
                            As an admin, you can change your roles to experience
                            the system with different permissions. Return to
                            this page anytime to modify your roles and regain
                            access to restricted areas.
                          </p>
                        </HoverCardContent>
                      </HoverCard>
                    </div>
                    <div className="space-y-2 border border-gray-300 rounded-lg p-3">
                      {AVAILABLE_ROLES.map((role) => {
                        const isPlatformManagerSelected =
                          roles.includes("PlatformManager");
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
                                roles.includes(role) ||
                                (isPlatformManagerSelected && isSubsetRole)
                              }
                              onChange={() =>
                                role !== "StandardUser" &&
                                handleRoleChange(role)
                              }
                              disabled={isDisabled}
                              className={`border-gray-300 text-doubleword-primary focus:ring-doubleword-primary rounded ${
                                isDisabled
                                  ? "opacity-50 cursor-not-allowed"
                                  : ""
                              }`}
                            />
                            <div className="ml-2 text-sm flex-1 flex items-center gap-1">
                              <span
                                className={
                                  isDisabled ? "text-gray-500" : "text-gray-700"
                                }
                              >
                                {getRoleDisplayName(role)}
                                {role === "StandardUser" && " (always enabled)"}
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
                    <p className="text-xs text-gray-500 mt-1"></p>
                  </div>
                ) : (
                  <div>
                    <label className="block text-sm font-medium text-gray-700 mb-1.5">
                      Roles
                    </label>
                    <Input
                      type="text"
                      value={
                        currentUser?.roles
                          ?.map(getRoleDisplayName)
                          .join(", ") || "User"
                      }
                      disabled
                    />
                    <p className="text-xs text-gray-500 mt-1">
                      Roles are managed by administrators
                    </p>
                  </div>
                )}

              </div>
            </div>

            {/* Notifications */}
            <div className="bg-white rounded-lg shadow-sm border border-gray-200 p-6 mt-6">
              <h4 className="text-lg font-medium text-gray-900 mb-4">
                Notifications
              </h4>

              {/* Email Notifications Toggle */}
              <div className="flex items-center justify-between pb-4 border-b border-gray-200">
                <div className="flex items-center gap-3">
                  <div className="p-2 bg-gray-100 rounded-lg">
                    <Mail className="w-4 h-4 text-gray-600" />
                  </div>
                  <div>
                    <Label
                      htmlFor="emailNotifications"
                      className="text-sm font-medium text-gray-900"
                    >
                      Email Notifications
                    </Label>
                    <p className="text-xs text-gray-500 mt-0.5">
                      Receive email when a batch completes or fails
                    </p>
                  </div>
                </div>
                <div className="flex items-center gap-2 shrink-0">
                  {emailNotifSaved && (
                    <span className="text-xs text-green-600 flex items-center gap-1">
                      <Check className="w-3 h-3" />
                      Saved
                    </span>
                  )}
                  <Switch
                    id="emailNotifications"
                    checked={currentUser?.batch_notifications_enabled ?? false}
                    onCheckedChange={handleEmailNotificationsToggle}
                    disabled={updateUserMutation.isPending}
                    aria-label="Email notifications"
                  />
                </div>
              </div>

              {/* Webhooks Section */}
              <div className="pt-4">
                <div className="flex items-center justify-between mb-3">
                  <div className="flex items-center gap-3">
                    <div className="p-2 bg-gray-100 rounded-lg">
                      <Globe className="w-4 h-4 text-gray-600" />
                    </div>
                    <div>
                      <h5 className="text-sm font-medium text-gray-900">
                        Webhooks
                      </h5>
                      <p className="text-xs text-gray-500 mt-0.5">
                        Receive HTTP callbacks when events occur
                      </p>
                    </div>
                  </div>
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={openCreateWebhookDialog}
                    aria-label="Add webhook"
                  >
                    <Plus className="w-3.5 h-3.5" />
                    Add Webhook
                  </Button>
                </div>

                {/* Webhook List */}
                {webhooks && webhooks.length > 0 ? (
                  <div className="space-y-2">
                    {webhooks.map((webhook) => (
                      <div
                        key={webhook.id}
                        className="border border-gray-200 rounded-lg p-3"
                      >
                        <div className="flex items-start justify-between gap-3">
                          <div className="min-w-0 flex-1">
                            <div className="flex items-center gap-2">
                              <code className="text-sm text-gray-900 truncate block">
                                {webhook.url}
                              </code>
                              {!webhook.enabled && (
                                <Badge variant="secondary">Disabled</Badge>
                              )}
                              {webhook.consecutive_failures > 0 && (
                                <Tooltip>
                                  <TooltipTrigger asChild>
                                    <span className="inline-flex items-center gap-1">
                                      <AlertTriangle className="w-3.5 h-3.5 text-amber-500" />
                                      <span className="text-xs text-amber-600">
                                        {webhook.consecutive_failures} failure
                                        {webhook.consecutive_failures !== 1
                                          ? "s"
                                          : ""}
                                      </span>
                                    </span>
                                  </TooltipTrigger>
                                  <TooltipContent>
                                    <p>
                                      {webhook.consecutive_failures} consecutive
                                      delivery failure
                                      {webhook.consecutive_failures !== 1
                                        ? "s"
                                        : ""}
                                      {webhook.disabled_at &&
                                        ". Auto-disabled due to failures."}
                                    </p>
                                  </TooltipContent>
                                </Tooltip>
                              )}
                            </div>
                            {webhook.description && (
                              <p className="text-xs text-gray-500 mt-0.5">
                                {webhook.description}
                              </p>
                            )}
                            <div className="flex items-center gap-1.5 mt-1.5">
                              {webhook.event_types &&
                              webhook.event_types.length > 0 ? (
                                webhook.event_types.map((type) => (
                                  <Badge
                                    key={type}
                                    variant="outline"
                                    className="text-[10px] px-1.5 py-0"
                                  >
                                    {type}
                                  </Badge>
                                ))
                              ) : (
                                <span className="text-[10px] text-gray-400">
                                  All events
                                </span>
                              )}
                            </div>
                            {/* Test result inline */}
                            {testResults[webhook.id] && (
                              <div
                                className={`text-xs mt-1.5 ${testResults[webhook.id].success ? "text-green-600" : "text-red-600"}`}
                              >
                                {testResults[webhook.id].success
                                  ? `Test succeeded (${testResults[webhook.id].status_code}, ${testResults[webhook.id].duration_ms}ms)`
                                  : `Test failed: ${testResults[webhook.id].error || "Unknown error"}`}
                              </div>
                            )}
                          </div>
                          <div className="flex items-center gap-1 shrink-0">
                            <Switch
                              checked={webhook.enabled}
                              onCheckedChange={() =>
                                handleWebhookToggle(webhook)
                              }
                              aria-label={`Toggle webhook ${webhook.url}`}
                            />
                            <Tooltip>
                              <TooltipTrigger asChild>
                                <Button
                                  variant="ghost"
                                  size="icon"
                                  className="h-7 w-7"
                                  onClick={() => handleTestWebhook(webhook.id)}
                                  disabled={
                                    testWebhookMutation.isPending &&
                                    testWebhookMutation.variables
                                      ?.webhookId === webhook.id
                                  }
                                  aria-label={`Test webhook ${webhook.url}`}
                                >
                                  {testWebhookMutation.isPending &&
                                  testWebhookMutation.variables?.webhookId ===
                                    webhook.id ? (
                                    <Loader2 className="w-3.5 h-3.5 animate-spin" />
                                  ) : (
                                    <TestTube className="w-3.5 h-3.5" />
                                  )}
                                </Button>
                              </TooltipTrigger>
                              <TooltipContent>Test webhook</TooltipContent>
                            </Tooltip>
                            <Tooltip>
                              <TooltipTrigger asChild>
                                <Button
                                  variant="ghost"
                                  size="icon"
                                  className="h-7 w-7"
                                  onClick={() =>
                                    openEditWebhookDialog(webhook)
                                  }
                                  aria-label={`Edit webhook ${webhook.url}`}
                                >
                                  <Pencil className="w-3.5 h-3.5" />
                                </Button>
                              </TooltipTrigger>
                              <TooltipContent>Edit webhook</TooltipContent>
                            </Tooltip>
                            <Tooltip>
                              <TooltipTrigger asChild>
                                <Button
                                  variant="ghost"
                                  size="icon"
                                  className="h-7 w-7 text-red-500 hover:text-red-700 hover:bg-red-50"
                                  onClick={() =>
                                    setDeletingWebhookId(webhook.id)
                                  }
                                  aria-label={`Delete webhook ${webhook.url}`}
                                >
                                  <Trash2 className="w-3.5 h-3.5" />
                                </Button>
                              </TooltipTrigger>
                              <TooltipContent>Delete webhook</TooltipContent>
                            </Tooltip>
                          </div>
                        </div>
                      </div>
                    ))}
                  </div>
                ) : (
                  <div className="text-sm text-gray-500 py-4 text-center border border-dashed border-gray-200 rounded-lg">
                    No webhooks configured. Add one to receive HTTP
                    notifications.
                  </div>
                )}
              </div>
            </div>
          </div>
        </div>
      </div>

      {/* Webhook Create/Edit Dialog */}
      <Dialog
        open={webhookDialogOpen}
        onOpenChange={(open) => {
          if (!open) {
            setWebhookDialogOpen(false);
            setWebhookSecret(null);
          }
        }}
      >
        <DialogContent>
          {webhookSecret ? (
            <>
              <DialogHeader>
                <DialogTitle>
                  {editingWebhook ? "Secret Rotated" : "Webhook Created"}
                </DialogTitle>
                <DialogDescription>
                  Copy this secret now. It will not be shown again.
                </DialogDescription>
              </DialogHeader>
              <div className="space-y-3">
                <div className="flex items-center gap-2 p-3 bg-gray-50 rounded-lg border border-gray-200">
                  <code className="text-sm flex-1 break-all font-mono">
                    {webhookSecret}
                  </code>
                  <Button
                    variant="ghost"
                    size="icon"
                    className="shrink-0 h-8 w-8"
                    onClick={() => handleCopySecret(webhookSecret)}
                    aria-label="Copy secret"
                  >
                    {copiedSecret ? (
                      <Check className="w-4 h-4 text-green-600" />
                    ) : (
                      <Copy className="w-4 h-4" />
                    )}
                  </Button>
                </div>
                <p className="text-xs text-amber-600 flex items-center gap-1">
                  <AlertTriangle className="w-3 h-3 shrink-0" />
                  Use this secret to verify webhook signatures. Store it
                  securely.
                </p>
              </div>
              <DialogFooter>
                <Button onClick={() => setWebhookDialogOpen(false)}>
                  Done
                </Button>
              </DialogFooter>
            </>
          ) : (
            <>
              <DialogHeader>
                <DialogTitle>
                  {editingWebhook ? "Edit Webhook" : "Add Webhook"}
                </DialogTitle>
                <DialogDescription>
                  {editingWebhook
                    ? "Update your webhook configuration."
                    : "Configure a URL to receive HTTP POST notifications."}
                </DialogDescription>
              </DialogHeader>
              <div className="space-y-4">
                {webhookError && (
                  <div className="text-sm text-red-600 bg-red-50 border border-red-200 p-2 rounded">
                    {webhookError}
                  </div>
                )}
                <div>
                  <Label htmlFor="webhookUrl" className="text-sm font-medium">
                    Endpoint URL
                  </Label>
                  <p className="text-xs text-gray-500 mt-0.5 mb-1">
                    We'll send a POST request with a JSON body to this URL when
                    events occur.
                  </p>
                  <Input
                    id="webhookUrl"
                    type="url"
                    value={webhookUrl}
                    onChange={(e) => setWebhookUrl(e.target.value)}
                    placeholder="https://example.com/webhooks/doubleword"
                  />
                </div>
                <div>
                  <Label
                    htmlFor="webhookDescription"
                    className="text-sm font-medium"
                  >
                    Description (optional)
                  </Label>
                  <Input
                    id="webhookDescription"
                    type="text"
                    value={webhookDescription}
                    onChange={(e) => setWebhookDescription(e.target.value)}
                    placeholder="My notification webhook"
                    className="mt-1"
                  />
                </div>
                <div>
                  <Label className="text-sm font-medium">
                    Event Types
                  </Label>
                  <p className="text-xs text-gray-500 mt-0.5 mb-2">
                    Select at least one event to listen for.
                  </p>
                  <div className="space-y-2">
                    {EVENT_TYPE_OPTIONS.map((option) => (
                      <label
                        key={option.value}
                        className="flex items-center gap-2 cursor-pointer"
                      >
                        <Checkbox
                          checked={webhookEventTypes.includes(option.value)}
                          onCheckedChange={() =>
                            handleWebhookEventTypeToggle(option.value)
                          }
                        />
                        <span className="text-sm text-gray-700">
                          {option.label}
                        </span>
                        <HoverCard openDelay={200} closeDelay={100}>
                          <HoverCardTrigger asChild>
                            <code className="text-xs text-gray-400 ml-auto cursor-help border-b border-dashed border-gray-300">
                              {option.value}
                            </code>
                          </HoverCardTrigger>
                          <HoverCardContent side="left" align="start" className="w-72">
                            <p className="text-xs font-medium text-gray-700 mb-1">
                              Example payload
                            </p>
                            <pre className="text-[11px] text-gray-500 bg-gray-50 rounded p-2 overflow-x-auto whitespace-pre">
                              {option.example}
                            </pre>
                          </HoverCardContent>
                        </HoverCard>
                      </label>
                    ))}
                  </div>
                </div>
                {editingWebhook && (
                  <div className="pt-2 border-t border-gray-200">
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() => handleRotateSecret(editingWebhook.id)}
                      disabled={rotateSecretMutation.isPending}
                    >
                      {rotateSecretMutation.isPending ? (
                        <Loader2 className="w-3.5 h-3.5 animate-spin" />
                      ) : (
                        <RotateCw className="w-3.5 h-3.5" />
                      )}
                      Rotate Secret
                    </Button>
                    <p className="text-xs text-gray-500 mt-1">
                      Generate a new signing secret. The old secret will stop
                      working immediately.
                    </p>
                  </div>
                )}
              </div>
              <DialogFooter>
                <Button
                  variant="outline"
                  onClick={() => setWebhookDialogOpen(false)}
                >
                  Cancel
                </Button>
                <Button
                  onClick={handleWebhookSave}
                  disabled={
                    createWebhookMutation.isPending ||
                    updateWebhookMutation.isPending ||
                    webhookEventTypes.length === 0
                  }
                >
                  {createWebhookMutation.isPending ||
                  updateWebhookMutation.isPending ? (
                    <Loader2 className="w-4 h-4 animate-spin" />
                  ) : null}
                  {editingWebhook ? "Save Changes" : "Create Webhook"}
                </Button>
              </DialogFooter>
            </>
          )}
        </DialogContent>
      </Dialog>

      {/* Delete Confirmation Dialog */}
      <Dialog
        open={!!deletingWebhookId}
        onOpenChange={(open) => !open && setDeletingWebhookId(null)}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Delete Webhook</DialogTitle>
            <DialogDescription>
              Are you sure you want to delete this webhook? This action cannot be
              undone.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setDeletingWebhookId(null)}
            >
              Cancel
            </Button>
            <Button
              variant="destructive"
              onClick={() =>
                deletingWebhookId && handleWebhookDelete(deletingWebhookId)
              }
              disabled={deleteWebhookMutation.isPending}
            >
              {deleteWebhookMutation.isPending ? (
                <Loader2 className="w-4 h-4 animate-spin" />
              ) : null}
              Delete
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
};
