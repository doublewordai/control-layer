import React, { useState } from "react";
import { AlertTriangle } from "lucide-react";
import { useDeleteGroup } from "../../../api/control-layer";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from "../../ui/dialog";
import { Button } from "../../ui/button";

interface DeleteGroupModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess: () => void;
  groupId: string;
  groupName: string;
  memberCount: number;
}

export const DeleteGroupModal: React.FC<DeleteGroupModalProps> = ({
  isOpen,
  onClose,
  onSuccess,
  groupId,
  groupName,
  memberCount,
}) => {
  const [error, setError] = useState<string | null>(null);

  const deleteGroupMutation = useDeleteGroup();

  const handleDelete = async () => {
    setError(null);

    try {
      await deleteGroupMutation.mutateAsync(groupId);
      console.log("Group deleted successfully:", { groupId, groupName });
      onSuccess();
      onClose();
    } catch (err) {
      console.error("Failed to delete group:", err);
      setError(err instanceof Error ? err.message : "Failed to delete group");
    }
  };

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <div className="flex items-center gap-3 mb-2">
            <div className="w-10 h-10 bg-red-100 rounded-full flex items-center justify-center">
              <AlertTriangle className="w-5 h-5 text-red-600" />
            </div>
            <div>
              <DialogTitle>Delete Group</DialogTitle>
              <DialogDescription>
                This action cannot be undone
              </DialogDescription>
            </div>
          </div>
        </DialogHeader>

        <div>
          {error && (
            <div className="mb-4 p-3 bg-red-50 border border-red-200 rounded-lg">
              <p className="text-sm text-red-600">{error}</p>
            </div>
          )}

          <div className="mb-6">
            <p className="text-gray-700 mb-4">
              Are you sure you want to delete the group{" "}
              <strong>{groupName}</strong>?
            </p>
            <div className="bg-gray-50 rounded-lg p-3">
              <p className="text-sm text-gray-600">
                <strong>Group:</strong> {groupName}
              </p>
              <p className="text-sm text-gray-600 mt-1">
                <strong>Members:</strong> {memberCount}
              </p>
              <p className="text-sm text-gray-600 mt-1">
                <strong>ID:</strong> {groupId}
              </p>
            </div>
            <div className="mt-4 p-3 bg-yellow-50 border border-yellow-200 rounded-lg">
              <p className="text-sm text-yellow-800">
                <strong>Warning:</strong> This will permanently delete the group
                and remove all members from it. Any model access granted to this
                group will also be revoked. This action cannot be undone.
              </p>
            </div>
          </div>
        </div>

        <DialogFooter>
          <Button
            variant="outline"
            onClick={onClose}
            disabled={deleteGroupMutation.isPending}
          >
            Cancel
          </Button>
          <Button
            variant="destructive"
            onClick={handleDelete}
            disabled={deleteGroupMutation.isPending}
          >
            {deleteGroupMutation.isPending ? (
              <>
                <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-white"></div>
                Deleting...
              </>
            ) : (
              <>
                <AlertTriangle className="w-4 h-4" />
                Delete Group
              </>
            )}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
