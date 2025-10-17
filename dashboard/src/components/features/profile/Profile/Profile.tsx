import React, { useState, useEffect } from "react";
import {
  User,
  Save,
  Loader2,
  Calendar,
  Shield,
  AtSign,
  Info,
} from "lucide-react";
import { useUser, useUpdateUser } from "../../../../api/dwctl/hooks";
import {
  UserAvatar,
  Tooltip,
  TooltipContent,
  TooltipTrigger,
  HoverCard,
  HoverCardContent,
  HoverCardTrigger,
} from "../../../ui";
import { AVAILABLE_ROLES, getRoleDisplayName } from "../../../../utils/roles";
import type { Role } from "../../../../api/dwctl/types";

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
  const [error, setError] = useState("");
  const [success, setSuccess] = useState("");

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

      // Refetch user data to get the updated information
      await refetchUser();
      setSuccess("Profile updated successfully!");
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
      <div className="p-8">
        <div className="max-w-4xl mx-auto">
          <div className="animate-pulse">
            <div className="h-8 bg-gray-200 rounded w-48 mb-6"></div>
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
    <div className="p-8">
      <div className="max-w-4xl mx-auto">
        <div className="mb-6">
          <h1 className="text-2xl font-bold text-gray-900">Profile Settings</h1>
          <p className="text-gray-600 mt-1">
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

        <div className="grid grid-cols-1 lg:grid-cols-3 gap-6">
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
                <h3 className="text-lg font-medium text-gray-900">
                  {displayName ||
                    currentUser?.display_name ||
                    currentUser?.username ||
                    "Unknown User"}
                </h3>
                <p className="text-sm text-gray-500">{currentUser?.email}</p>
              </div>
            </div>

            {/* Account Details */}
            <div className="bg-white rounded-lg shadow-sm border border-gray-200 p-6 mt-6">
              <h4 className="text-lg font-medium text-gray-900 mb-4">
                Account Details
              </h4>
              <div className="space-y-3">
                <div className="flex items-center text-sm">
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
                </div>
                <div className="flex items-center text-sm">
                  <AtSign className="w-4 h-4 text-gray-400 mr-2 flex-shrink-0" />
                  <span className="text-gray-600 w-20 flex-shrink-0">
                    Email:
                  </span>
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
                {currentUser?.created_at && (
                  <div className="flex items-center text-sm">
                    <Calendar className="w-4 h-4 text-gray-400 mr-2 flex-shrink-0" />
                    <span className="text-gray-600 w-20 flex-shrink-0">
                      Joined:
                    </span>
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
                  <Shield className="w-4 h-4 text-gray-400 mr-2 flex-shrink-0" />
                  <span className="text-gray-600 w-20 flex-shrink-0">
                    Type:
                  </span>
                  <span className="text-gray-900">
                    {currentUser?.is_admin ? "Admin" : "User"}
                  </span>
                </div>
                {currentUser?.auth_source && (
                  <div className="flex items-center text-sm">
                    <AtSign className="w-4 h-4 text-gray-400 mr-2 flex-shrink-0" />
                    <span className="text-gray-600 w-20 flex-shrink-0">
                      Auth:
                    </span>
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <span className="text-gray-900 capitalize truncate">
                          {currentUser.auth_source}
                        </span>
                      </TooltipTrigger>
                      <TooltipContent>
                        <p>{currentUser.auth_source}</p>
                      </TooltipContent>
                    </Tooltip>
                  </div>
                )}
              </div>
            </div>
          </div>

          {/* Editable Profile Information */}
          <div className="lg:col-span-2">
            <div className="bg-white rounded-lg shadow-sm border border-gray-200 p-6">
              <h4 className="text-lg font-medium text-gray-900 mb-6">
                Edit Profile
              </h4>

              <div className="space-y-6">
                <div className="grid grid-cols-1 md:grid-cols-2 gap-6">
                  <div>
                    <label className="block text-sm font-medium text-gray-700 mb-2">
                      Username
                    </label>
                    <input
                      type="text"
                      value={currentUser?.username || ""}
                      disabled
                      className="w-full px-3 py-2 border border-gray-300 rounded-lg bg-gray-50 text-gray-500"
                    />
                    <p className="text-xs text-gray-500 mt-1">
                      Username cannot be changed
                    </p>
                  </div>

                  <div>
                    <label className="block text-sm font-medium text-gray-700 mb-2">
                      Email Address
                    </label>
                    <input
                      type="email"
                      value={currentUser?.email || ""}
                      disabled
                      className="w-full px-3 py-2 border border-gray-300 rounded-lg bg-gray-50 text-gray-500"
                    />
                    <p className="text-xs text-gray-500 mt-1">
                      Email cannot be changed
                    </p>
                  </div>
                </div>

                <div>
                  <label
                    htmlFor="displayName"
                    className="block text-sm font-medium text-gray-700 mb-2"
                  >
                    Display Name
                  </label>
                  <input
                    id="displayName"
                    type="text"
                    value={displayName}
                    onChange={(e) => setDisplayName(e.target.value)}
                    className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-doubleword-primary focus:border-transparent"
                    placeholder="Enter your display name"
                  />
                  <p className="text-xs text-gray-500 mt-1">
                    This is how your name will appear to other users
                  </p>
                </div>

                <div>
                  <label
                    htmlFor="avatarUrl"
                    className="block text-sm font-medium text-gray-700 mb-2"
                  >
                    Avatar URL
                  </label>
                  <input
                    id="avatarUrl"
                    type="url"
                    value={avatarUrl}
                    onChange={(e) => setAvatarUrl(e.target.value)}
                    className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-doubleword-primary focus:border-transparent"
                    placeholder="https://example.com/avatar.jpg"
                  />
                  <p className="text-xs text-gray-500 mt-1">
                    Enter a URL to your profile picture
                  </p>
                </div>

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
                      {AVAILABLE_ROLES.map((role) => (
                        <label key={role} className="flex items-center">
                          <input
                            type="checkbox"
                            value={role}
                            checked={
                              role === "StandardUser" || roles.includes(role)
                            }
                            onChange={() =>
                              role !== "StandardUser" && handleRoleChange(role)
                            }
                            disabled={role === "StandardUser"}
                            className={`border-gray-300 text-doubleword-primary focus:ring-doubleword-primary rounded ${
                              role === "StandardUser"
                                ? "opacity-50 cursor-not-allowed"
                                : ""
                            }`}
                          />
                          <div className="ml-2 text-sm flex-1 flex items-center gap-1">
                            <span
                              className={
                                role === "StandardUser"
                                  ? "text-gray-500"
                                  : "text-gray-700"
                              }
                            >
                              {getRoleDisplayName(role)}
                              {role === "StandardUser" && " (always enabled)"}
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
                      ))}
                    </div>
                    <p className="text-xs text-gray-500 mt-1"></p>
                  </div>
                ) : (
                  <div>
                    <label className="block text-sm font-medium text-gray-700 mb-2">
                      Roles
                    </label>
                    <input
                      type="text"
                      value={
                        currentUser?.roles
                          ?.map(getRoleDisplayName)
                          .join(", ") || "User"
                      }
                      disabled
                      className="w-full px-3 py-2 border border-gray-300 rounded-lg bg-gray-50 text-gray-500"
                    />
                    <p className="text-xs text-gray-500 mt-1">
                      Roles are managed by administrators
                    </p>
                  </div>
                )}

                <div className="flex justify-end items-center pt-6 border-t border-gray-200">
                  <button
                    onClick={handleSave}
                    disabled={updateUserMutation.isPending}
                    className="flex items-center gap-2 px-6 py-2 text-sm font-medium text-white bg-doubleword-primary rounded-lg hover:bg-doubleword-primary/90 disabled:opacity-50"
                  >
                    {updateUserMutation.isPending ? (
                      <Loader2 className="w-4 h-4 animate-spin" />
                    ) : (
                      <Save className="w-4 h-4" />
                    )}
                    Save Changes
                  </button>
                </div>
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
};
