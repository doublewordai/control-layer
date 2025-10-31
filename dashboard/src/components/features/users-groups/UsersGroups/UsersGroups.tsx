import React, { useState, useRef, useEffect } from "react";
import { useSearchParams, useNavigate } from "react-router-dom";
import { Users, UserPlus, Search, X, Trash2 } from "lucide-react";
import {
  useUsers,
  useGroups,
  useDeleteUser,
  useDeleteGroup,
  type Group as BackendGroup,
} from "../../../../api/control-layer";
import { useSettings } from "../../../../contexts";
import {
  CreateUserModal,
  CreateGroupModal,
  EditUserModal,
  EditGroupModal,
  UserGroupsModal,
  DeleteUserModal,
  GroupManagementModal,
  DeleteGroupModal,
  UserTransactionsModal,
} from "../../../modals";
import { GroupActionsDropdown } from "../";
import { UserAvatar, Button } from "../../../ui";
import { DataTable } from "../../../ui/data-table";
import { createUserColumns } from "./columns";
import { Input } from "../../../ui/input";
import { toast } from "sonner";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from "../../../ui/dialog";
import type { DisplayUser, DisplayGroup } from "../../../../types/display";

// Predefined color classes that Tailwind will include
const GROUP_COLOR_CLASSES = [
  "bg-blue-500",
  "bg-purple-500",
  "bg-green-500",
  "bg-yellow-500",
  "bg-red-500",
  "bg-indigo-500",
  "bg-teal-500",
  "bg-orange-500",
  "bg-pink-500",
  "bg-cyan-500",
];

// Function to get a consistent color for a group
const getGroupColor = (_groupId: string, index: number): string => {
  // Use index to assign colors consistently
  return GROUP_COLOR_CLASSES[index % GROUP_COLOR_CLASSES.length];
};

const UsersGroups: React.FC = () => {
  const [searchParams] = useSearchParams();
  const navigate = useNavigate();
  const { isFeatureEnabled } = useSettings();

  // Get tab from URL or default to "users"
  const tabFromUrl = searchParams.get("tab");
  const [activeTab, setActiveTab] = useState<"users" | "groups">(() => {
    return tabFromUrl === "groups" ? "groups" : "users";
  });

  // Update activeTab when URL changes
  useEffect(() => {
    const tabFromUrl = searchParams.get("tab");
    if (tabFromUrl === "groups" || tabFromUrl === "users") {
      setActiveTab(tabFromUrl);
    }
  }, [searchParams]);

  // Handle tab change
  const handleTabChange = (tab: "users" | "groups") => {
    setActiveTab(tab);
    const newParams = new URLSearchParams(searchParams);
    newParams.set("tab", tab);
    navigate(`/users-groups?${newParams.toString()}`, { replace: true });
  };
  // Data from the API: uses the tanstack query hooks to fetch both users and groups TODO: (this is a bit redundant right now, but we can optimize later)
  const {
    data: usersData,
    isLoading: usersLoading,
    error: usersError,
  } = useUsers({ include: "groups" });
  const {
    data: groupsData,
    isLoading: groupsLoading,
    error: groupsError,
  } = useGroups({ include: "users" });
  const loading = usersLoading || groupsLoading;
  const error = usersError || groupsError;

  // Searching for groups
  const [searchQuery, setSearchQuery] = useState("");

  // Selected users and groups for bulk operations
  const [selectedUsers, setSelectedUsers] = useState<DisplayUser[]>([]);
  const [selectedGroups, setSelectedGroups] = useState<Set<string>>(new Set());
  const tableRef = useRef<any>(null);

  // Modals
  const [showCreateUserModal, setShowCreateUserModal] = useState(false);
  const [showCreateGroupModal, setShowCreateGroupModal] = useState(false);
  const [showUserGroupsModal, setShowUserGroupsModal] = useState(false);
  const [showDeleteUserModal, setShowDeleteUserModal] = useState(false);
  const [showGroupManagementModal, setShowGroupManagementModal] =
    useState(false);
  const [showDeleteGroupModal, setShowDeleteGroupModal] = useState(false);
  const [showEditUserModal, setShowEditUserModal] = useState(false);
  const [showEditGroupModal, setShowEditGroupModal] = useState(false);
  const [showBulkDeleteModal, setShowBulkDeleteModal] = useState(false);
  const [showBulkDeleteGroupsModal, setShowBulkDeleteGroupsModal] =
    useState(false);
  const [showUserTransactionsModal, setShowUserTransactionsModal] =
    useState(false);

  // 'active' means the 3 dots have been clicked on a user or group, vs. selected in the table.
  const [activeUser, setActiveUser] = useState<DisplayUser | null>(null);
  const [activeGroup, setActiveGroup] = useState<DisplayGroup | null>(null);

  // Bulk operations
  const deleteUserMutation = useDeleteUser();
  const deleteGroupMutation = useDeleteGroup();

  const handleSelectGroup = (groupId: string) => {
    setSelectedGroups((prev) => {
      const newSet = new Set(prev);
      if (newSet.has(groupId)) {
        newSet.delete(groupId);
      } else {
        newSet.add(groupId);
      }
      return newSet;
    });
  };

  const handleSelectAllGroups = () => {
    if (selectedGroups.size === filteredGroups.length) {
      setSelectedGroups(new Set());
    } else {
      setSelectedGroups(new Set(filteredGroups.map((g) => g.id)));
    }
  };

  const handleBulkDeleteGroups = async () => {
    try {
      // Delete groups one by one
      for (const groupId of selectedGroups) {
        await deleteGroupMutation.mutateAsync(groupId);
      }
      setSelectedGroups(new Set());
      setShowBulkDeleteGroupsModal(false);
      toast.success(
        `Successfully deleted ${selectedGroups.size} group${selectedGroups.size !== 1 ? "s" : ""}`,
      );
    } catch (error) {
      console.error("Error deleting groups:", error);
      toast.error("Failed to delete some groups. Please try again.");
    }
  };

  const handleBulkDelete = async () => {
    try {
      // Delete users one by one
      for (const user of selectedUsers) {
        await deleteUserMutation.mutateAsync(user.id);
      }
      setSelectedUsers([]); // Clear selection after successful deletion
      setShowBulkDeleteModal(false);
      // Clear table selection if ref is available
      if (tableRef.current?.resetRowSelection) {
        tableRef.current.resetRowSelection();
      }
    } catch (error) {
      console.error("Error deleting users:", error);
      // Keep modal open to show error
    }
  };

  // Transform API data
  const users: DisplayUser[] = usersData
    ? usersData.map((user) => ({
        ...user,
        name: user.display_name || user.username,
        avatar: user.avatar_url || "",
        isAdmin: user.is_admin ?? false,
        groupNames: user.groups
          ? user.groups.map((group: BackendGroup) => group.name)
          : [],
      }))
    : [];

  const groups: DisplayGroup[] = groupsData
    ? groupsData.map((group: BackendGroup) => ({
        ...group, // Keep all backend fields
        memberCount: group.users ? group.users.length : 0,
        memberIds: group.users ? group.users.map((user) => user.id) : [],
      }))
    : [];

  const filteredGroups = groups.filter(
    (group) =>
      group.name.toLowerCase().includes(searchQuery.toLowerCase()) ||
      group.description?.toLowerCase().includes(searchQuery.toLowerCase()),
  );

  // Column configuration for users DataTable
  const userColumns = createUserColumns({
    onEdit: (user) => {
      setActiveUser(user);
      setShowEditUserModal(true);
    },
    onDelete: (user) => {
      setActiveUser(user);
      setShowDeleteUserModal(true);
    },
    onManageGroups: (user) => {
      setActiveUser(user);
      setShowUserGroupsModal(true);
    },
    onViewTransactions: (user) => {
      setActiveUser(user);
      setShowUserTransactionsModal(true);
    },
    groups: groups,
    showTransactions: isFeatureEnabled("use_billing"),
  });

  if (loading) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <div
            className="animate-spin rounded-full h-12 w-12 border-b-2 border-doubleword-accent-blue mx-auto mb-4"
            role="progressbar"
            aria-label="Loading"
          ></div>
          <p className="text-doubleword-neutral-600">
            Loading users and groups...
          </p>
        </div>
      </div>
    );
  }

  if (error) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <div className="text-red-500 mb-4">
            <X className="h-12 w-12 mx-auto" />
          </div>
          <p className="text-red-600 font-semibold">
            Error:{" "}
            {error instanceof Error ? error.message : "Failed to load data"}
          </p>
        </div>
      </div>
    );
  }

  return (
    <div className="p-6">
      {/* Header */}
      <div className="mb-8">
        <h1 className="text-3xl font-bold text-doubleword-neutral-900">
          Users & Groups
        </h1>
        <p className="text-doubleword-neutral-600 mt-2">
          Manage user access and group permissions
        </p>
      </div>

      {/* Tabs */}
      <div className="border-b border-doubleword-neutral-200 mb-6">
        <nav
          className="flex gap-8"
          role="tablist"
          aria-label="User and group management"
        >
          <button
            id="users-tab"
            role="tab"
            aria-label="Users"
            aria-selected={activeTab === "users"}
            aria-controls="users-panel"
            onClick={() => handleTabChange("users")}
            className={`pb-3 px-1 border-b-2 transition-colors ${
              activeTab === "users"
                ? "border-doubleword-primary text-doubleword-primary font-medium"
                : "border-transparent text-doubleword-neutral-500 hover:text-doubleword-neutral-700"
            }`}
          >
            Users ({users.length})
          </button>
          <button
            id="groups-tab"
            role="tab"
            aria-label="Groups"
            aria-selected={activeTab === "groups"}
            aria-controls="groups-panel"
            onClick={() => handleTabChange("groups")}
            className={`pb-3 px-1 border-b-2 transition-colors ${
              activeTab === "groups"
                ? "border-doubleword-primary text-doubleword-primary font-medium"
                : "border-transparent text-doubleword-neutral-500 hover:text-doubleword-neutral-700"
            }`}
          >
            Groups ({groups.length})
          </button>
        </nav>
      </div>

      {/* Search and Actions for Groups Tab */}
      {activeTab === "groups" && (
        <div className="flex items-center justify-between mb-6">
          <div className="flex items-center gap-4 flex-1">
            <div className="relative w-full md:w-96">
              <Search className="absolute left-2 top-2.5 h-4 w-4 text-muted-foreground" />
              <Input
                type="text"
                placeholder="Search groups..."
                value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
                className="pl-8"
                aria-label="Search groups"
              />
            </div>
            {selectedGroups.size > 0 && (
              <div className="text-sm text-muted-foreground">
                {selectedGroups.size} of {filteredGroups.length} group(s)
                selected
              </div>
            )}
          </div>
          <div className="flex items-center gap-2">
            <Button onClick={() => setShowCreateGroupModal(true)} size="sm">
              <Users className="w-4 h-4" />
              Add Group
            </Button>
            {filteredGroups.length > 0 && (
              <Button
                variant="outline"
                onClick={handleSelectAllGroups}
                size="sm"
              >
                {selectedGroups.size === filteredGroups.length
                  ? "Deselect All"
                  : "Select All"}
              </Button>
            )}
          </div>
        </div>
      )}

      {/* Bulk action bar for groups */}
      {activeTab === "groups" && selectedGroups.size > 0 && (
        <div className="bg-blue-50 border border-blue-200 rounded-lg p-3 mb-4 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <span className="text-sm font-medium text-blue-900">
              {selectedGroups.size} group{selectedGroups.size !== 1 ? "s" : ""}{" "}
              selected
            </span>
          </div>
          <div className="flex items-center gap-2">
            <button
              onClick={() => setShowBulkDeleteGroupsModal(true)}
              className="flex items-center gap-1 px-3 py-1.5 bg-red-600 text-white text-sm rounded-md hover:bg-red-700 transition-colors"
            >
              <Trash2 className="w-4 h-4" />
              Delete Selected
            </button>
          </div>
        </div>
      )}

      <div
        role="tabpanel"
        id="users-panel"
        aria-labelledby="users-tab"
        hidden={activeTab !== "users"}
      >
        {activeTab === "users" && (
          /* Users DataTable */
          <DataTable
            columns={userColumns}
            data={users}
            searchPlaceholder="Search users..."
            searchColumn="name"
            showPagination={users.length > 10}
            onSelectionChange={setSelectedUsers}
            headerActions={
              <Button onClick={() => setShowCreateUserModal(true)} size="sm">
                <UserPlus className="w-4 h-4" />
                Add User
              </Button>
            }
            actionBar={
              <div className="bg-blue-50 border border-blue-200 rounded-lg p-3 mb-4 flex items-center justify-between">
                <div className="flex items-center gap-2">
                  <span className="text-sm font-medium text-blue-900">
                    {selectedUsers.length} user
                    {selectedUsers.length !== 1 ? "s" : ""} selected
                  </span>
                </div>
                <div className="flex items-center gap-2">
                  <button
                    onClick={() => setShowBulkDeleteModal(true)}
                    className="flex items-center gap-1 px-3 py-1.5 bg-red-600 text-white text-sm rounded-md hover:bg-red-700 transition-colors"
                  >
                    <Trash2 className="w-4 h-4" />
                    Delete Selected
                  </button>
                </div>
              </div>
            }
          />
        )}
      </div>
      <div
        role="tabpanel"
        id="groups-panel"
        aria-labelledby="groups-tab"
        hidden={activeTab !== "groups"}
      >
        {activeTab === "groups" && (
          /* Groups Grid */
          <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-6">
            {filteredGroups.map((group, index) => {
              const colorClass = getGroupColor(group.id, index);
              const isSelected = selectedGroups.has(group.id);
              return (
                <div
                  key={group.id}
                  className={`bg-white rounded-xl shadow-sm border-2 p-6 hover:shadow-md transition-all cursor-pointer ${
                    isSelected
                      ? "border-blue-500 bg-blue-50 shadow-md"
                      : "border-transparent hover:border-gray-200"
                  }`}
                  onClick={(e) => {
                    // Only select if not clicking on the dropdown or its children
                    if (!(e.target as HTMLElement).closest("[data-dropdown]")) {
                      handleSelectGroup(group.id);
                    }
                  }}
                >
                  <div className="flex items-start justify-between mb-4">
                    <div className="flex items-center gap-3">
                      <div
                        className={`w-10 h-10 ${colorClass} rounded-lg flex items-center justify-center`}
                      >
                        <Users className="w-5 h-5 text-white" />
                      </div>
                      <div>
                        <h3 className="font-semibold text-doubleword-neutral-900">
                          {group.name}
                        </h3>
                        <p className="text-sm text-doubleword-neutral-500">
                          {group.memberCount} members
                        </p>
                      </div>
                    </div>
                    <div data-dropdown>
                      <GroupActionsDropdown
                        groupId={group.id}
                        onEditGroup={() => {
                          setActiveGroup(group);
                          setShowEditGroupModal(true);
                        }}
                        onManageGroup={() => {
                          setActiveGroup(group);
                          setShowGroupManagementModal(true);
                        }}
                        onDeleteGroup={() => {
                          setActiveGroup(group);
                          setShowDeleteGroupModal(true);
                        }}
                      />
                    </div>
                  </div>
                  <p className="text-sm text-doubleword-neutral-600 mb-4">
                    {group.description}
                  </p>
                  <div className="flex items-center justify-start pt-4 border-t border-doubleword-neutral-100">
                    <div className="flex -space-x-2">
                      {group.memberIds.slice(0, 4).map((memberId) => {
                        const member = users.find((u) => u.id === memberId);
                        return member ? (
                          <div
                            key={memberId}
                            className="border-2 border-white rounded-full"
                          >
                            <UserAvatar user={member} size="md" />
                          </div>
                        ) : null;
                      })}
                      {group.memberCount > 4 && (
                        <div className="w-8 h-8 bg-doubleword-neutral-200 rounded-full border-2 border-white flex items-center justify-center">
                          <span className="text-xs text-doubleword-neutral-600">
                            +{group.memberCount - 4}
                          </span>
                        </div>
                      )}
                    </div>
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </div>

      {/* Modals */}
      <CreateUserModal
        isOpen={showCreateUserModal}
        onClose={() => setShowCreateUserModal(false)}
        onSuccess={() => {
          /* TanStack Query will auto-update */
        }}
      />
      <CreateGroupModal
        isOpen={showCreateGroupModal}
        onClose={() => setShowCreateGroupModal(false)}
        onSuccess={() => {
          /* TanStack Query will auto-update */
        }}
      />
      {activeUser && (
        <UserGroupsModal
          isOpen={showUserGroupsModal}
          onClose={() => {
            setShowUserGroupsModal(false);
            setActiveUser(null);
            // Refresh data to update group memberships
            // TanStack Query will auto-update
          }}
          onSuccess={() => {
            // Don't refresh here to avoid modal jumping - just update when modal closes
          }}
          user={activeUser}
        />
      )}
      {activeUser && (
        <DeleteUserModal
          isOpen={showDeleteUserModal}
          onClose={() => {
            setShowDeleteUserModal(false);
            setActiveUser(null);
          }}
          onSuccess={() => {
            // Refresh data to update user list after deletion
            // TanStack Query will auto-update
          }}
          userId={activeUser.id}
          userName={activeUser.name}
          userEmail={activeUser.email}
        />
      )}
      {activeGroup && (
        <GroupManagementModal
          isOpen={showGroupManagementModal}
          onClose={() => {
            setShowGroupManagementModal(false);
            setActiveGroup(null);
            // Refresh data to update group memberships
            // TanStack Query will auto-update
          }}
          onSuccess={() => {
            // Don't refresh here to avoid modal jumping - just update when modal closes
          }}
          group={activeGroup}
        />
      )}
      {activeGroup && (
        <DeleteGroupModal
          isOpen={showDeleteGroupModal}
          onClose={() => {
            setShowDeleteGroupModal(false);
            setActiveGroup(null);
          }}
          onSuccess={() => {
            // Refresh data to update group list after deletion
            // TanStack Query will auto-update
          }}
          groupId={activeGroup.id}
          groupName={activeGroup.name}
          memberCount={activeGroup.memberCount}
        />
      )}
      {activeUser && (
        <EditUserModal
          isOpen={showEditUserModal}
          onClose={() => {
            setShowEditUserModal(false);
            setActiveUser(null);
          }}
          onSuccess={() => {
            // Refresh data to update user list after editing
            // TanStack Query will auto-update
          }}
          userId={activeUser.id}
          currentUser={{
            name: activeUser.name,
            email: activeUser.email,
            username: activeUser.username,
            avatar: activeUser.avatar,
            roles: activeUser.roles,
          }}
        />
      )}
      {activeGroup && (
        <EditGroupModal
          isOpen={showEditGroupModal}
          onClose={() => {
            setShowEditGroupModal(false);
            setActiveGroup(null);
          }}
          onSuccess={() => {
            // Refresh data to update group list after editing
            // TanStack Query will auto-update
          }}
          groupId={activeGroup.id}
          currentGroup={{
            name: activeGroup.name,
            description: activeGroup.description || "",
          }}
        />
      )}
      {activeUser && (
        <UserTransactionsModal
          isOpen={showUserTransactionsModal}
          onClose={() => {
            setShowUserTransactionsModal(false);
            setActiveUser(null);
          }}
          user={activeUser}
        />
      )}

      {/* Bulk Delete Confirmation Modal */}
      <Dialog open={showBulkDeleteModal} onOpenChange={setShowBulkDeleteModal}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <div className="flex items-center gap-3">
              <div className="w-10 h-10 bg-red-100 rounded-full flex items-center justify-center">
                <Trash2 className="w-5 h-5 text-red-600" />
              </div>
              <div>
                <DialogTitle>Delete Users</DialogTitle>
                <p className="text-sm text-gray-600">
                  This action cannot be undone
                </p>
              </div>
            </div>
          </DialogHeader>

          <div className="space-y-4">
            <p className="text-gray-700">
              Are you sure you want to delete{" "}
              <strong>{selectedUsers.length}</strong> user
              {selectedUsers.length !== 1 ? "s" : ""}?
            </p>

            <div className="bg-gray-50 rounded-lg p-3 max-h-32 overflow-y-auto">
              <p className="text-sm font-medium text-gray-600 mb-2">
                Users to be deleted:
              </p>
              <ul className="text-sm text-gray-700 space-y-1">
                {selectedUsers.map((user) => (
                  <li key={user.id} className="flex justify-between">
                    <span>{user.name}</span>
                    <span className="text-gray-500">{user.email}</span>
                  </li>
                ))}
              </ul>
            </div>

            <div className="p-3 bg-yellow-50 border border-yellow-200 rounded-lg">
              <p className="text-sm text-yellow-800">
                <strong>Warning:</strong> This will permanently delete{" "}
                {selectedUsers.length > 1
                  ? "these user accounts"
                  : "this user account"}{" "}
                and remove {selectedUsers.length > 1 ? "them" : "them"} from all
                groups. This action cannot be undone.
              </p>
            </div>
          </div>

          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => setShowBulkDeleteModal(false)}
              disabled={deleteUserMutation.isPending}
            >
              Cancel
            </Button>
            <Button
              type="button"
              variant="destructive"
              onClick={handleBulkDelete}
              disabled={deleteUserMutation.isPending}
            >
              {deleteUserMutation.isPending ? (
                <>
                  <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-white"></div>
                  Deleting...
                </>
              ) : (
                <>
                  <Trash2 className="w-4 h-4" />
                  Delete {selectedUsers.length} User
                  {selectedUsers.length !== 1 ? "s" : ""}
                </>
              )}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Bulk Delete Groups Confirmation Modal */}
      <Dialog
        open={showBulkDeleteGroupsModal}
        onOpenChange={setShowBulkDeleteGroupsModal}
      >
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <div className="flex items-center gap-3">
              <div className="w-10 h-10 bg-red-100 rounded-full flex items-center justify-center">
                <Trash2 className="w-5 h-5 text-red-600" />
              </div>
              <div>
                <DialogTitle>Delete Groups</DialogTitle>
                <p className="text-sm text-gray-600">
                  This action cannot be undone
                </p>
              </div>
            </div>
          </DialogHeader>

          <div className="space-y-4">
            <p className="text-gray-700">
              Are you sure you want to delete{" "}
              <strong>{selectedGroups.size}</strong> group
              {selectedGroups.size !== 1 ? "s" : ""}?
            </p>

            <div className="bg-gray-50 rounded-lg p-3 max-h-32 overflow-y-auto">
              <p className="text-sm font-medium text-gray-600 mb-2">
                Groups to be deleted:
              </p>
              <ul className="text-sm text-gray-700 space-y-1">
                {Array.from(selectedGroups).map((groupId) => {
                  const group = groups.find((g) => g.id === groupId);
                  return group ? (
                    <li key={group.id} className="flex justify-between">
                      <span>{group.name}</span>
                      <span className="text-gray-500">
                        {group.memberCount} member
                        {group.memberCount !== 1 ? "s" : ""}
                      </span>
                    </li>
                  ) : null;
                })}
              </ul>
            </div>

            <div className="p-3 bg-yellow-50 border border-yellow-200 rounded-lg">
              <p className="text-sm text-yellow-800">
                <strong>Warning:</strong> This will permanently delete{" "}
                {selectedGroups.size > 1 ? "these groups" : "this group"} and
                remove all associated permissions. This action cannot be undone.
              </p>
            </div>
          </div>

          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => setShowBulkDeleteGroupsModal(false)}
              disabled={deleteGroupMutation.isPending}
            >
              Cancel
            </Button>
            <Button
              type="button"
              variant="destructive"
              onClick={handleBulkDeleteGroups}
              disabled={deleteGroupMutation.isPending}
            >
              {deleteGroupMutation.isPending ? (
                <>
                  <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-white mr-2"></div>
                  Deleting...
                </>
              ) : (
                <>
                  <Trash2 className="w-4 h-4 mr-2" />
                  Delete {selectedGroups.size} Group
                  {selectedGroups.size !== 1 ? "s" : ""}
                </>
              )}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
};

export default UsersGroups;
