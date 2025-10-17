import React, { useEffect, useState } from "react";
import { Users, Plus, Trash2 } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
import {
  useGroups,
  useAddUserToGroup,
  useRemoveUserFromGroup,
  type Group as BackendGroup,
} from "../../../api/dwctl";
import type { DisplayUser } from "../../../types/display";

interface UserGroupsModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess: () => void;
  user: DisplayUser;
}

export const UserGroupsModal: React.FC<UserGroupsModalProps> = ({
  isOpen,
  onClose,
  user,
}) => {
  const [error, setError] = useState<string | null>(null);

  // Fetch all groups data (includes users for membership checking)
  // Only fetch when modal is open to avoid 403 errors for users without permission
  const { data: groupsData, isLoading: loading } = useGroups({
    include: "users",
    enabled: isOpen,
  });
  const addUserToGroupMutation = useAddUserToGroup();
  const removeUserFromGroupMutation = useRemoveUserFromGroup();

  // Clear error when modal opens
  useEffect(() => {
    if (isOpen) {
      setError(null);
    }
  }, [isOpen, user.name, groupsData]);

  // Check user group access. Use the groups data (since this is dynamic - i.e.
  // changes when memberships changes) and not the passed in 'user` prop, since
  // that's static & its groups won't change.
  const isUserInGroup = (groupId: string): boolean => {
    const group = groupsData?.find((g) => g.id === groupId);
    const result = group?.users?.some((u) => u.id === user.id) || false;
    return result;
  };

  // Helper function to check if a specific group is being updated
  const isGroupUpdating = (groupId: string) => {
    return (
      (addUserToGroupMutation.isPending &&
        addUserToGroupMutation.variables?.groupId === groupId &&
        addUserToGroupMutation.variables?.userId === user.id) ||
      (removeUserFromGroupMutation.isPending &&
        removeUserFromGroupMutation.variables?.groupId === groupId &&
        removeUserFromGroupMutation.variables?.userId === user.id)
    );
  };

  const handleAddUserToGroup = async (groupId: string) => {
    setError(null);
    try {
      await addUserToGroupMutation.mutateAsync({ groupId, userId: user.id });
    } catch (err) {
      setError(
        err instanceof Error ? err.message : "Failed to add user to group",
      );
    }
  };

  const handleRemoveUserFromGroup = async (groupId: string) => {
    setError(null);
    try {
      await removeUserFromGroupMutation.mutateAsync({
        groupId,
        userId: user.id,
      });
    } catch (err) {
      setError(
        err instanceof Error ? err.message : "Failed to remove user from group",
      );
    }
  };

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-2xl max-h-[80vh] overflow-hidden">
        <DialogHeader>
          <DialogTitle>Manage User Groups</DialogTitle>
          <DialogDescription>User: {user.name}</DialogDescription>
        </DialogHeader>

        <div className="overflow-y-auto max-h-[60vh]">
          {error && (
            <div className="mb-4 p-3 bg-red-50 border border-red-200 rounded-lg">
              <p className="text-sm text-red-600">{error}</p>
            </div>
          )}

          {loading ? (
            <div className="flex items-center justify-center py-8">
              <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-blue-500"></div>
            </div>
          ) : (
            <div className="space-y-3">
              <div className="mb-4">
                <h3 className="text-lg font-medium text-gray-900 mb-2">
                  Available Groups
                </h3>
                <p className="text-sm text-gray-600">
                  Manage which groups this user belongs to. Group membership
                  determines access to models and resources.
                </p>
              </div>

              {!groupsData || groupsData.length === 0 ? (
                <div className="text-center py-8">
                  <Users className="w-12 h-12 text-gray-400 mx-auto mb-3" />
                  <p className="text-gray-500">No groups available</p>
                  <p className="text-sm text-gray-400 mb-4">
                    Create groups first to manage user membership
                  </p>
                  <button
                    onClick={() => {
                      onClose();
                      window.location.hash = "#/users-groups";
                    }}
                    className="px-4 py-2 bg-blue-600 text-white rounded-lg hover:bg-blue-700 transition-colors"
                  >
                    Create Groups
                  </button>
                </div>
              ) : (
                groupsData.map((group: BackendGroup) => (
                  <div
                    key={group.id}
                    className="flex items-center justify-between p-4 border border-gray-200 rounded-lg hover:bg-gray-50 transition-colors"
                  >
                    <div className="flex items-center gap-3">
                      <div className="w-10 h-10 bg-blue-500 rounded-lg flex items-center justify-center">
                        <Users className="w-5 h-5 text-white" />
                      </div>
                      <div>
                        <h4 className="font-medium text-gray-900">
                          {group.name}
                        </h4>
                        <p className="text-sm text-gray-500">
                          {group.description} • {group.users?.length || 0}{" "}
                          members
                        </p>
                      </div>
                    </div>

                    <div className="flex items-center gap-2">
                      {isUserInGroup(group.id) ? (
                        <>
                          <span className="text-xs px-2 py-1 bg-green-100 text-green-700 rounded-full">
                            Member
                          </span>
                          {/* Don't show remove button for "everyone" group (nil UUID) */}
                          {group.id !==
                            "00000000-0000-0000-0000-000000000000" && (
                            <button
                              onClick={() =>
                                handleRemoveUserFromGroup(group.id)
                              }
                              disabled={isGroupUpdating(group.id)}
                              className="p-2 text-red-600 hover:bg-red-50 rounded-lg transition-colors disabled:opacity-50"
                              title="Remove from group"
                            >
                              {isGroupUpdating(group.id) ? (
                                <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-red-600"></div>
                              ) : (
                                <Trash2 className="w-4 h-4" />
                              )}
                            </button>
                          )}
                        </>
                      ) : (
                        <button
                          onClick={() => handleAddUserToGroup(group.id)}
                          disabled={isGroupUpdating(group.id)}
                          className="flex items-center gap-2 px-3 py-2 text-blue-600 hover:bg-blue-50 rounded-lg transition-colors disabled:opacity-50"
                        >
                          {isGroupUpdating(group.id) ? (
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
        </div>

        <DialogFooter>
          <Button onClick={onClose} variant="outline">
            Done
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
