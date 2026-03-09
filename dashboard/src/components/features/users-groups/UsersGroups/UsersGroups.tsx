import React, { useState, useRef, useEffect, useMemo } from "react";
import { useSearchParams, useNavigate } from "react-router-dom";
import { Users, UserPlus, Search, X, Trash2, Plus } from "lucide-react";
import { useDebounce } from "@/hooks/useDebounce";
import {
  useUsers,
  useGroups,
  useDeleteUser,
  useDeleteGroup,
  useOrganizations,
  useDeleteOrganization,
  type Group as BackendGroup,
} from "../../../../api/control-layer";
import type { Organization } from "../../../../api/control-layer/types";
import {
  CreateUserModal,
  CreateGroupModal,
  EditUserModal,
  EditGroupModal,
  UserGroupsModal,
  DeleteUserModal,
  GroupManagementModal,
  DeleteGroupModal,
} from "../../../modals";
import { CreateOrganizationModal } from "../../organizations/CreateOrganizationModal";
import { EditOrganizationModal } from "../../organizations/EditOrganizationModal";
import { GroupActionsDropdown } from "../";
import { UserAvatar, Button } from "../../../ui";
import { DataTable } from "../../../ui/data-table";
import { createUserColumns } from "./columns";
import { createOrganizationColumns } from "../../organizations/columns";
import { Input } from "../../../ui/input";
import { toast } from "sonner";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from "../../../ui/dialog";
import type { DisplayUser, DisplayGroup } from "../../../../types/display";
import { TablePagination } from "@/components/ui/table-pagination";
import { useServerPagination } from "@/hooks/useServerPagination";
import { useAuthorization } from "../../../../utils";


const UsersGroups: React.FC = () => {
  const [searchParams] = useSearchParams();
  const navigate = useNavigate();

  // Use pagination hooks with prefixes for multi-table support
  const usersPagination = useServerPagination({
    paramPrefix: "users",
    defaultPageSize: 10,
  });

  const groupsPagination = useServerPagination({
    paramPrefix: "groups",
    defaultPageSize: 9,
  });

  const orgsPagination = useServerPagination({
    paramPrefix: "orgs",
    defaultPageSize: 10,
  });

  // Get tab from URL or default to "users"
  type TabType = "users" | "orgs" | "groups";
  const tabFromUrl = searchParams.get("tab");
  const [activeTab, setActiveTab] = useState<TabType>(() => {
    if (tabFromUrl === "groups" || tabFromUrl === "orgs") return tabFromUrl;
    return "users";
  });

  // Update activeTab when URL changes
  useEffect(() => {
    const tabFromUrl = searchParams.get("tab");
    if (tabFromUrl === "groups" || tabFromUrl === "users" || tabFromUrl === "orgs") {
      setActiveTab(tabFromUrl);
    }
  }, [searchParams]);

  // Handle tab change
  const handleTabChange = (tab: TabType) => {
    setActiveTab(tab);
    const newParams = new URLSearchParams(searchParams);
    newParams.set("tab", tab);
    navigate(`/users-groups?${newParams.toString()}`, { replace: true });
  };

  // Search state for users (server-side) - must be declared before useUsers hook
  const [userSearchQuery, setUserSearchQuery] = useState("");
  const debouncedUserSearch = useDebounce(userSearchQuery, 300);

  // Search state for groups (server-side) - must be declared before useGroups hook
  const [groupSearchQuery, setGroupSearchQuery] = useState("");
  const debouncedGroupSearch = useDebounce(groupSearchQuery, 300);

  // Search state for organizations (server-side)
  const [orgSearchQuery, setOrgSearchQuery] = useState("");
  const debouncedOrgSearch = useDebounce(orgSearchQuery, 300);

  // Data from the API: uses the tanstack query hooks to fetch both users and groups TODO: (this is a bit redundant right now, but we can optimize later)
  const {
    data: usersData,
    isLoading: usersLoading,
    error: usersError,
  } = useUsers({
    include: "groups",
    search: debouncedUserSearch || undefined,
    ...usersPagination.queryParams,
  });

  const {
    data: groupsData,
    isLoading: groupsLoading,
    error: groupsError,
  } = useGroups({
    include: "users",
    search: debouncedGroupSearch || undefined,
    ...groupsPagination.queryParams,
  });

  const {
    data: orgsData,
    isLoading: orgsLoading,
    error: _orgsError,
  } = useOrganizations({
    search: debouncedOrgSearch || undefined,
    include: "member_count",
    ...orgsPagination.queryParams,
  });

  const loading = usersLoading || groupsLoading;
  const error = usersError || groupsError;

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

  // Org modals
  const [showCreateOrgModal, setShowCreateOrgModal] = useState(false);
  const [editingOrg, setEditingOrg] = useState<Organization | null>(null);
  const [deletingOrg, setDeletingOrg] = useState<Organization | null>(null);

  // 'active' means the 3 dots have been clicked on a user or group, vs. selected in the table.
  const [activeUser, setActiveUser] = useState<DisplayUser | null>(null);
  const [activeGroup, setActiveGroup] = useState<DisplayGroup | null>(null);

  // Bulk operations
  const deleteUserMutation = useDeleteUser();
  const deleteGroupMutation = useDeleteGroup();
  const deleteOrgMutation = useDeleteOrganization();

  // Authorization
  const { hasPermission } = useAuthorization();
  const isPlatformManager = hasPermission("users-groups");

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
    if (selectedGroups.size === groups.length) {
      setSelectedGroups(new Set());
    } else {
      setSelectedGroups(new Set(groups.map((g) => g.id)));
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
    ? usersData.data.map((user) => ({
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
    ? groupsData.data.map((group: BackendGroup) => ({
        ...group, // Keep all backend fields
        memberCount: group.users ? group.users.length : 0,
        memberIds: group.users ? group.users.map((user) => user.id) : [],
      }))
    : [];

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
      navigate(`/cost-management?user=${user.id}`);
    },
    groups: groups,
    showTransactions: true,
  });

  // Column configuration for organizations DataTable
  const orgColumns = useMemo(
    () =>
      createOrganizationColumns({
        onView: (org) => navigate(`/organizations/${org.id}`),
        onEdit: (org) => setEditingOrg(org),
        onDelete: (org) => setDeletingOrg(org),
        canDelete: isPlatformManager,
      }),
    [navigate, isPlatformManager],
  );

  const handleDeleteOrg = async () => {
    if (!deletingOrg) return;
    try {
      await deleteOrgMutation.mutateAsync(deletingOrg.id);
      toast.success("Organization deleted");
      setDeletingOrg(null);
    } catch (error) {
      toast.error(
        error instanceof Error
          ? error.message
          : "Failed to delete organization",
      );
    }
  };

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
          User Access
        </h1>
        <p className="text-doubleword-neutral-600 mt-2">
          Manage users, organizations, and access groups
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
            Users
          </button>
          <button
            id="orgs-tab"
            role="tab"
            aria-label="Organizations"
            aria-selected={activeTab === "orgs"}
            aria-controls="orgs-panel"
            onClick={() => handleTabChange("orgs")}
            className={`pb-3 px-1 border-b-2 transition-colors ${
              activeTab === "orgs"
                ? "border-doubleword-primary text-doubleword-primary font-medium"
                : "border-transparent text-doubleword-neutral-500 hover:text-doubleword-neutral-700"
            }`}
          >
            Organizations
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
            Groups
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
                value={groupSearchQuery}
                onChange={(e) => setGroupSearchQuery(e.target.value)}
                className="pl-8"
                aria-label="Search groups"
              />
            </div>
            {selectedGroups.size > 0 && (
              <div className="text-sm text-muted-foreground">
                {selectedGroups.size} of {groups.length} group(s) selected
              </div>
            )}
          </div>
          <div className="flex items-center gap-2">
            <Button onClick={() => setShowCreateGroupModal(true)} size="sm">
              <Users className="w-4 h-4" />
              Add Group
            </Button>
            {groups.length > 0 && (
              <Button
                variant="outline"
                onClick={handleSelectAllGroups}
                size="sm"
              >
                {selectedGroups.size === groups.length
                  ? "Deselect All"
                  : "Select All"}
              </Button>
            )}
          </div>
        </div>
      )}

      {/* Bulk action bar for groups */}
      {activeTab === "groups" && selectedGroups.size > 0 && (
        <div className="bg-muted border rounded-lg p-3 mb-4 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <span className="text-sm font-medium text-foreground">
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
            externalSearch={{
              value: userSearchQuery,
              onChange: (value) => {
                setUserSearchQuery(value);
                usersPagination.handleReset();
              },
            }}
            paginationMode="server"
            serverPagination={{
              page: usersPagination.page,
              pageSize: usersPagination.pageSize,
              totalItems: usersData?.total_count || 0,
              onPageChange: usersPagination.handlePageChange,
              onPageSizeChange: usersPagination.handlePageSizeChange,
            }}
            showPageSizeSelector={true}
            onSelectionChange={setSelectedUsers}
            headerActions={
              <div className="flex items-center gap-2">
                <Button onClick={() => setShowCreateUserModal(true)} size="sm">
                  <UserPlus className="w-4 h-4" />
                  Add User
                </Button>
              </div>
            }
            actionBar={
              <div className="bg-muted border rounded-lg p-3 mb-4 flex items-center justify-between">
                <div className="flex items-center gap-2">
                  <span className="text-sm font-medium text-foreground">
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
        id="orgs-panel"
        aria-labelledby="orgs-tab"
        hidden={activeTab !== "orgs"}
      >
        {activeTab === "orgs" && (
          <DataTable
            columns={orgColumns}
            data={orgsData?.data ?? []}
            isLoading={orgsLoading}
            searchPlaceholder="Search organizations..."
            externalSearch={{
              value: orgSearchQuery,
              onChange: (value) => {
                setOrgSearchQuery(value);
                orgsPagination.handleReset();
              },
            }}
            paginationMode="server"
            serverPagination={{
              page: orgsPagination.page,
              pageSize: orgsPagination.pageSize,
              totalItems: orgsData?.total_count ?? 0,
              onPageChange: orgsPagination.handlePageChange,
              onPageSizeChange: orgsPagination.handlePageSizeChange,
            }}
            headerActions={
              <Button onClick={() => setShowCreateOrgModal(true)} size="sm">
                <Plus className="w-4 h-4" />
                Create Organization
              </Button>
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
            {groups.map((group) => {
              const isSelected = selectedGroups.has(group.id);
              return (
                <div
                  key={group.id}
                  className={`bg-white dark:bg-doubleword-background-dark rounded-lg border p-6 transition-colors cursor-pointer ${
                    isSelected
                      ? "border-doubleword-primary bg-doubleword-primary/5"
                      : "border-doubleword-neutral-200 dark:border-doubleword-neutral-700 hover:border-doubleword-neutral-300 dark:hover:border-doubleword-neutral-600"
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
                        className="w-10 h-10 bg-doubleword-neutral-100 dark:bg-doubleword-neutral-800 rounded-lg flex items-center justify-center"
                      >
                        <Users className="w-5 h-5 text-doubleword-neutral-500" />
                      </div>
                      <div className="min-w-0 flex-1">
                        <h3 className="font-semibold text-doubleword-neutral-900 truncate break-all">
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
                  <p className="text-sm text-doubleword-neutral-600 mb-4 wrap-break-word">
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
        <TablePagination
          itemName="group"
          itemsPerPage={groupsPagination.pageSize}
          currentPage={groupsPagination.page}
          onPageChange={groupsPagination.handlePageChange}
          totalItems={groupsData?.total_count || 0}
        />
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
      {/* Organization Modals */}
      <CreateOrganizationModal
        isOpen={showCreateOrgModal}
        onClose={() => setShowCreateOrgModal(false)}
        isPlatformManager={isPlatformManager}
      />
      <EditOrganizationModal
        isOpen={!!editingOrg}
        onClose={() => setEditingOrg(null)}
        organization={editingOrg}
      />
      <Dialog
        open={!!deletingOrg}
        onOpenChange={(open) => !open && setDeletingOrg(null)}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Delete Organization</DialogTitle>
            <DialogDescription>
              Are you sure you want to delete{" "}
              <strong>
                {deletingOrg?.display_name || deletingOrg?.username}
              </strong>
              ? This action cannot be undone.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setDeletingOrg(null)}>
              Cancel
            </Button>
            <Button
              variant="destructive"
              onClick={handleDeleteOrg}
              disabled={deleteOrgMutation.isPending}
            >
              {deleteOrgMutation.isPending ? "Deleting..." : "Delete"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Bulk Delete Confirmation Modal */}
      <Dialog open={showBulkDeleteModal} onOpenChange={setShowBulkDeleteModal}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>Delete Users</DialogTitle>
            <DialogDescription>This action cannot be undone</DialogDescription>
          </DialogHeader>
          <div className="flex items-center gap-3 pb-4">
            <div className="w-10 h-10 bg-red-100 rounded-full flex items-center justify-center">
              <Trash2 className="w-5 h-5 text-red-600" />
            </div>
          </div>

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
            <DialogTitle>Delete Groups</DialogTitle>
            <DialogDescription>This action cannot be undone</DialogDescription>
          </DialogHeader>
          <div className="flex items-center gap-3 pb-4">
            <div className="w-10 h-10 bg-red-100 rounded-full flex items-center justify-center">
              <Trash2 className="w-5 h-5 text-red-600" />
            </div>
          </div>

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
