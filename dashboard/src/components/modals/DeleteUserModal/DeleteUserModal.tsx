import React, { useState } from "react";
import { AlertTriangle } from "lucide-react";
import { useDeleteUser } from "../../../api/waycast";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "../../ui/dialog";
import { Button } from "../../ui/button";

interface DeleteUserModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess: () => void;
  userId: string;
  userName: string;
  userEmail: string;
}

export const DeleteUserModal: React.FC<DeleteUserModalProps> = ({
  isOpen,
  onClose,
  onSuccess,
  userId,
  userName,
  userEmail,
}) => {
  const [error, setError] = useState<string | null>(null);

  const deleteUserMutation = useDeleteUser();

  const handleDelete = async () => {
    setError(null);

    try {
      await deleteUserMutation.mutateAsync(userId);
      console.log("User deleted successfully:", { userId, userName });
      onSuccess();
      onClose();
    } catch (err) {
      console.error("Failed to delete user:", err);
      setError(err instanceof Error ? err.message : "Failed to delete user");
    }
  };

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent
        className="sm:max-w-md"
        aria-labelledby="delete-user-title"
      >
        <DialogHeader>
          <div className="flex items-center gap-3">
            <div className="w-10 h-10 bg-red-100 rounded-full flex items-center justify-center">
              <AlertTriangle className="w-5 h-5 text-red-600" />
            </div>
            <div>
              <DialogTitle id="delete-user-title">Delete User</DialogTitle>
              <DialogDescription>
                This action cannot be undone
              </DialogDescription>
            </div>
          </div>
        </DialogHeader>

        <div className="space-y-4">
          {error && (
            <div className="p-3 bg-red-50 border border-red-200 rounded-lg">
              <p className="text-sm text-red-600">{error}</p>
            </div>
          )}

          <p className="text-gray-700">
            Are you sure you want to delete the user <strong>{userName}</strong>
            ?
          </p>

          <div className="bg-gray-50 rounded-lg p-3">
            <p className="text-sm text-gray-600">
              <strong>Email:</strong> {userEmail}
            </p>
            <p className="text-sm text-gray-600 mt-1">
              <strong>ID:</strong> {userId}
            </p>
          </div>

          <div className="p-3 bg-yellow-50 border border-yellow-200 rounded-lg">
            <p className="text-sm text-yellow-800">
              <strong>Warning:</strong> This will permanently delete the user
              account and remove them from all groups. This action cannot be
              undone.
            </p>
          </div>
        </div>

        <DialogFooter>
          <Button
            onClick={onClose}
            disabled={deleteUserMutation.isPending}
            variant="outline"
          >
            Cancel
          </Button>
          <Button
            onClick={handleDelete}
            disabled={deleteUserMutation.isPending}
            variant="destructive"
            className="gap-2"
          >
            {deleteUserMutation.isPending ? (
              <>
                <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-white"></div>
                Deleting...
              </>
            ) : (
              <>
                <AlertTriangle className="w-4 h-4" />
                Delete User
              </>
            )}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
