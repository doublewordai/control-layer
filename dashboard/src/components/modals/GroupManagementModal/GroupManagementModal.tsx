import React, { useEffect, useMemo, useState } from "react";
import { Users, Plus, Trash2, Search, Building } from "lucide-react";
import { UserAvatar } from "../../ui";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
import { Input } from "../../ui/input";
import { TablePagination } from "../../ui/table-pagination";
import { useServerPagination } from "../../../hooks/useServerPagination";
import { useDebounce } from "../../../hooks/useDebounce";
import {
  useUsers,
  useOrganizations,
  useAddUserToGroup,
  useRemoveUserFromGroup,
  type Group as BackendGroup,
} from "../../../api/control-layer";
import { AlertBox } from "@/components/ui/alert-box";

interface GroupManagementModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess: () => void;
  group: BackendGroup;
}

export const GroupManagementModal: React.FC<GroupManagementModalProps> = ({
  isOpen,
  onClose,
  group,
}) => {
  const [error, setError] = useState<string | null>(null);
  const [viewMode, setViewMode] = useState<"users" | "orgs">("users");
  const [searchQuery, setSearchQuery] = useState("");
  const debouncedSearch = useDebounce(searchQuery, 300);

  // Pagination hook
  const pagination = useServerPagination({
    paramPrefix: "groupModal",
    defaultPageSize: 20,
  });

  // Fetch users (for users mode — includes groups for membership check)
  const { data: usersResponse, isLoading: usersLoading } = useUsers({
    include: "groups",
    enabled: isOpen && viewMode === "users",
    search: debouncedSearch || undefined,
    ...pagination.queryParams,
  });

  // Fetch organizations (for orgs mode)
  const { data: orgsResponse, isLoading: orgsLoading } = useOrganizations({
    enabled: isOpen && viewMode === "orgs",
    search: debouncedSearch || undefined,
    include: "member_count",
    ...pagination.queryParams,
  });

  const users = useMemo(() => usersResponse?.data || [], [usersResponse]);
  const orgs = useMemo(() => orgsResponse?.data || [], [orgsResponse]);

  const items = viewMode === "users" ? users : orgs;
  const totalCount = viewMode === "users"
    ? usersResponse?.total_count ?? 0
    : orgsResponse?.total_count ?? 0;
  const loading = viewMode === "users" ? usersLoading : orgsLoading;

  // Build set of member IDs from group.users (populated by parent via include=users)
  const memberIds = useMemo(() => {
    const ids = new Set<string>();
    if (group.users) {
      for (const user of group.users) {
        ids.add(user.id);
      }
    }
    return ids;
  }, [group.users]);

  const addUserToGroupMutation = useAddUserToGroup();
  const removeUserFromGroupMutation = useRemoveUserFromGroup();

  // Clear error when modal opens
  useEffect(() => {
    if (isOpen) {
      setError(null);
    }
  }, [isOpen, group.name]);

  // Reset pagination and search when switching view mode
  const handleViewModeChange = (mode: "users" | "orgs") => {
    setViewMode(mode);
    setSearchQuery("");
    pagination.handleReset();
  };

  // Clean up when modal closes
  const handleClose = () => {
    pagination.handleClear();
    setSearchQuery("");
    setViewMode("users");
    onClose();
  };

  // Check if entity is in this group
  const isInGroup = (id: string): boolean => {
    if (viewMode === "users") {
      // For users, check via the user's groups (more up-to-date after mutations)
      const user = users.find((u) => u.id === id);
      return user?.groups?.some((g) => g.id === group.id) || false;
    }
    // For orgs, check against the group's member list
    return memberIds.has(id);
  };

  const isUpdating = (id: string) => {
    return (
      (addUserToGroupMutation.isPending ||
        removeUserFromGroupMutation.isPending) &&
      (addUserToGroupMutation.variables?.userId === id ||
        removeUserFromGroupMutation.variables?.userId === id)
    );
  };

  const handleAdd = async (id: string) => {
    setError(null);
    try {
      await addUserToGroupMutation.mutateAsync({ groupId: group.id, userId: id });
    } catch (err) {
      console.error("Failed to add to group:", err);
      setError(
        err instanceof Error ? err.message : "Failed to add to group",
      );
    }
  };

  const handleRemove = async (id: string) => {
    setError(null);
    try {
      await removeUserFromGroupMutation.mutateAsync({
        groupId: group.id,
        userId: id,
      });
    } catch (err) {
      console.error("Failed to remove from group:", err);
      setError(
        err instanceof Error ? err.message : "Failed to remove from group",
      );
    }
  };

  const entityLabel = viewMode === "users" ? "users" : "organizations";

  return (
    <Dialog open={isOpen} onOpenChange={handleClose}>
      <DialogContent className="sm:max-w-2xl max-h-[80vh] overflow-hidden">
        <DialogHeader>
          <DialogTitle>Manage Group Members</DialogTitle>
          <DialogDescription>Group: {group.name}</DialogDescription>
        </DialogHeader>

        {/* Search and Toggle */}
        <div className="flex items-center gap-3">
          <div className="relative flex-1">
            <Search className="absolute left-2.5 top-2.5 h-4 w-4 text-muted-foreground" />
            <Input
              placeholder={`Search ${entityLabel}...`}
              value={searchQuery}
              onChange={(e) => {
                setSearchQuery(e.target.value);
                pagination.handleReset();
              }}
              className="pl-8"
              aria-label={`Search ${entityLabel}`}
            />
          </div>
          <div className="flex rounded-lg border border-gray-200 p-0.5">
            <button
              onClick={() => handleViewModeChange("users")}
              className={`flex items-center gap-1.5 px-3 py-1.5 text-sm rounded-md transition-colors ${
                viewMode === "users"
                  ? "bg-gray-900 text-white"
                  : "text-gray-600 hover:text-gray-900"
              }`}
            >
              <Users className="w-3.5 h-3.5" />
              Users
            </button>
            <button
              onClick={() => handleViewModeChange("orgs")}
              className={`flex items-center gap-1.5 px-3 py-1.5 text-sm rounded-md transition-colors ${
                viewMode === "orgs"
                  ? "bg-gray-900 text-white"
                  : "text-gray-600 hover:text-gray-900"
              }`}
            >
              <Building className="w-3.5 h-3.5" />
              Orgs
            </button>
          </div>
        </div>

        <div className="overflow-y-auto max-h-[60vh]">
          <AlertBox variant="error" className="mb-4">
            {error}
          </AlertBox>

          {loading ? (
            <div className="flex items-center justify-center py-8">
              <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-blue-500"></div>
            </div>
          ) : (
            <div className="space-y-3">
              <div className="mb-4">
                <p className="text-sm text-gray-600">
                  Manage which {entityLabel} belong to this group.
                  Group members will have access to models assigned to this group.
                </p>
              </div>

              {items.length === 0 ? (
                <div className="text-center py-8">
                  {viewMode === "users" ? (
                    <Users className="w-12 h-12 text-gray-400 mx-auto mb-3" />
                  ) : (
                    <Building className="w-12 h-12 text-gray-400 mx-auto mb-3" />
                  )}
                  <p className="text-gray-500">
                    {searchQuery
                      ? `No ${entityLabel} match your search`
                      : `No ${entityLabel} available`}
                  </p>
                </div>
              ) : (
                items.map((item) => (
                  <div
                    key={item.id}
                    className="flex items-center justify-between p-4 border border-gray-200 rounded-lg hover:bg-gray-50 transition-colors"
                  >
                    <div className="flex items-center gap-3">
                      {viewMode === "users" ? (
                        <UserAvatar user={item} size="md" />
                      ) : (
                        <div className="w-8 h-8 bg-gray-100 rounded-full flex items-center justify-center">
                          <Building className="w-4 h-4 text-gray-500" />
                        </div>
                      )}
                      <div>
                        <h4 className="font-medium text-gray-900">
                          {item.display_name || item.username}
                        </h4>
                        <p className="text-sm text-gray-500">{item.email}</p>
                      </div>
                    </div>

                    <div className="flex items-center gap-2">
                      {isInGroup(item.id) ? (
                        <>
                          <span className="text-xs px-2 py-1 bg-green-100 text-green-700 rounded-full">
                            Member
                          </span>
                          <button
                            onClick={() => handleRemove(item.id)}
                            disabled={isUpdating(item.id)}
                            className="p-2 text-red-600 hover:bg-red-50 rounded-lg transition-colors disabled:opacity-50"
                            title="Remove from group"
                          >
                            {isUpdating(item.id) ? (
                              <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-red-600"></div>
                            ) : (
                              <Trash2 className="w-4 h-4" />
                            )}
                          </button>
                        </>
                      ) : (
                        <button
                          onClick={() => handleAdd(item.id)}
                          disabled={isUpdating(item.id)}
                          className="flex items-center gap-2 px-3 py-2 text-blue-600 hover:bg-blue-50 rounded-lg transition-colors disabled:opacity-50"
                        >
                          {isUpdating(item.id) ? (
                            <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-blue-600"></div>
                          ) : (
                            <Plus className="w-4 h-4" />
                          )}
                          <span className="text-sm">Add to Group</span>
                        </button>
                      )}
                    </div>
                  </div>
                ))
              )}
            </div>
          )}

          {/* Pagination */}
          {totalCount > 0 && (
            <TablePagination
              itemName={viewMode === "users" ? "user" : "organization"}
              itemsPerPage={pagination.pageSize}
              currentPage={pagination.page}
              onPageChange={pagination.handlePageChange}
              totalItems={totalCount}
            />
          )}
        </div>

        <DialogFooter>
          <Button onClick={handleClose} variant="outline">
            Done
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
