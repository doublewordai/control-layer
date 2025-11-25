import React, { useEffect, useMemo, useState } from "react";
import { Users, Plus, Trash2 } from "lucide-react";
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
import {
  useUsers,
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

  // Fetch all users data
  // Only fetch when modal is open to avoid 403 errors for users without permission
  const { data: usersResponse, isLoading: loading } = useUsers({
    include: "groups",
    enabled: isOpen,
  });

  const users = useMemo(() => usersResponse?.data || [], [usersResponse]);

  const addUserToGroupMutation = useAddUserToGroup();
  const removeUserFromGroupMutation = useRemoveUserFromGroup();

  // Clear error when modal opens
  useEffect(() => {
    if (isOpen) {
      setError(null);
    }
  }, [isOpen, group.name, users]);

  // Check if user is in this group (use current users data, not static group prop)
  const isUserInGroup = (userId: string): boolean => {
    const user = users.find((u) => u.id === userId);
    return user?.groups?.some((g) => g.id === group.id) || false;
  };

  // Helper function to check if a specific user is being updated
  const isUserUpdating = (userId: string) => {
    return (
      (addUserToGroupMutation.isPending ||
        removeUserFromGroupMutation.isPending) &&
      (addUserToGroupMutation.variables?.userId === userId ||
        removeUserFromGroupMutation.variables?.userId === userId)
    );
  };

  const handleAddUserToGroup = async (userId: string) => {
    setError(null);
    try {
      await addUserToGroupMutation.mutateAsync({ groupId: group.id, userId });
    } catch (err) {
      console.error("Failed to add user to group:", err);
      setError(
        err instanceof Error ? err.message : "Failed to add user to group",
      );
    }
  };

  const handleRemoveUserFromGroup = async (userId: string) => {
    setError(null);
    try {
      await removeUserFromGroupMutation.mutateAsync({
        groupId: group.id,
        userId,
      });
    } catch (err) {
      console.error("Failed to remove user from group:", err);
      setError(
        err instanceof Error ? err.message : "Failed to remove user from group",
      );
    }
  };

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-2xl max-h-[80vh] overflow-hidden">
        <DialogHeader>
          <DialogTitle>Manage Group Members</DialogTitle>
          <DialogDescription>Group: {group.name}</DialogDescription>
        </DialogHeader>

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
                <h3 className="text-lg font-medium text-gray-900 mb-2">
                  Available Users
                </h3>
                <p className="text-sm text-gray-600">
                  Manage which users belong to this group. Group members will
                  have access to models assigned to this group.
                </p>
              </div>

              {users.length === 0 ? (
                <div className="text-center py-8">
                  <Users className="w-12 h-12 text-gray-400 mx-auto mb-3" />
                  <p className="text-gray-500">No users available</p>
                  <p className="text-sm text-gray-400">
                    Create users first to manage group membership
                  </p>
                </div>
              ) : (
                users.map((user) => (
                  <div
                    key={user.id}
                    className="flex items-center justify-between p-4 border border-gray-200 rounded-lg hover:bg-gray-50 transition-colors"
                  >
                    <div className="flex items-center gap-3">
                      <UserAvatar user={user} size="md" />
                      <div>
                        <h4 className="font-medium text-gray-900">
                          {user.display_name || user.username}
                        </h4>
                        <p className="text-sm text-gray-500">{user.email}</p>
                      </div>
                    </div>

                    <div className="flex items-center gap-2">
                      {isUserInGroup(user.id) ? (
                        <>
                          <span className="text-xs px-2 py-1 bg-green-100 text-green-700 rounded-full">
                            Member
                          </span>
                          <button
                            onClick={() => handleRemoveUserFromGroup(user.id)}
                            disabled={isUserUpdating(user.id)}
                            className="p-2 text-red-600 hover:bg-red-50 rounded-lg transition-colors disabled:opacity-50"
                            title="Remove from group"
                          >
                            {isUserUpdating(user.id) ? (
                              <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-red-600"></div>
                            ) : (
                              <Trash2 className="w-4 h-4" />
                            )}
                          </button>
                        </>
                      ) : (
                        <button
                          onClick={() => handleAddUserToGroup(user.id)}
                          disabled={isUserUpdating(user.id)}
                          className="flex items-center gap-2 px-3 py-2 text-blue-600 hover:bg-blue-50 rounded-lg transition-colors disabled:opacity-50"
                        >
                          {isUserUpdating(user.id) ? (
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
