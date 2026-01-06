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
  LockIcon,
} from "lucide-react";
import { useUser, useUpdateUser } from "../../../../api/control-layer/hooks";
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
import { AVAILABLE_ROLES, getRoleDisplayName } from "../../../../utils/roles";
import type { Role } from "../../../../api/control-layer/types";
import { dwctlApi } from "../../../../api/control-layer/client";
import { ApiError } from "../../../../api/control-layer/errors";

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

  const handleRoleChange = (role: Role) => {
    setRoles((prev) =>
      prev.includes(role) ? prev.filter((r) => r !== role) : [...prev, role],
    );
  };

  const getAuthProviderDisplay = (currentUser: User): string => {
    const { authSource, externalUserId } = currentUser;

    if (authSource === "native" || authSource === "system") {
      return authSource;
    }

    // attempt to parse common providers from external ID
    if (externalUserId) {
      const lowerExternalId = externalUserId.toLowerCase();
      if (lowerExternalId.startsWith("github")) {
        return "GitHub";
      }
      if (lowerExternalId.startsWith("google")) {
        return "Google";
      }
      return "Oauth";
    }

    return "email";
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
                <h3 className="text-lg font-medium text-gray-900 truncate px-2">
                  {displayName ||
                    currentUser?.display_name ||
                    currentUser?.username ||
                    "Unknown User"}
                </h3>
                <p className="text-sm text-gray-500 truncate px-2">
                  {currentUser?.email}
                </p>
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
                {currentUser?.auth_source && (
                  <div className="flex items-center text-sm">
                    <LockIcon className="w-4 h-4 text-gray-400 mr-2 shrink-0" />
                    <span className="text-gray-600 w-20 shrink-0">Auth:</span>
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <span className="text-gray-900 truncate">
                          {getAuthProviderDisplay(currentUser)}
                        </span>
                      </TooltipTrigger>
                      <TooltipContent>
                        <p>{getAuthProviderDisplay(currentUser)}</p>
                      </TooltipContent>
                    </Tooltip>
                  </div>
                )}
              </div>
            </div>
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

                <div className="flex justify-end items-center">
                  <Button
                    onClick={handleSave}
                    disabled={updateUserMutation.isPending}
                  >
                    {updateUserMutation.isPending ? (
                      <Loader2 className="w-4 h-4 animate-spin" />
                    ) : (
                      <Save className="w-4 h-4" />
                    )}
                    Save Changes
                  </Button>
                </div>
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
};
